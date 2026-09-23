/**
 * t_api.rs - Local Import API
 *
 * An opt-in HTTP server on 127.0.0.1 that lets local tools, such as a browser
 * extension, list albums and import images and videos into a folder of the
 * current library. It is off by default and is enabled in Settings > Advanced.
 *
 * Routes (all require `Authorization: Bearer <token>`):
 *   GET  /v1/status   app version and current library
 *   GET  /v1/albums   albums of the current library and their known folders
 *   POST /v1/import   import one image or video from a URL or base64 data
 *
 * Web pages cannot use the API: a request carrying a browser `Origin` must
 * come from an extension, and the `Host` header must name the loopback
 * address (guards against DNS rebinding). Imports address folders by id, so
 * callers can only write into folders that belong to an album.
 */
use crate::t_cmds;
use crate::t_config::{self, LocalApiConfig};
use crate::t_sqlite::{AFile, AFolder, Album};
use crate::t_utils;
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::{Bytes, Incoming};
use hyper::header::{self, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Mutex, RwLock};
use std::time::Duration;
use tauri::{AppHandle, Emitter};

/// Largest accepted request body. Base64 inflates data by a third, so this
/// admits the same 200 MB files as drag-and-drop import.
const MAX_BODY_BYTES: usize = 280 * 1024 * 1024;

const EXTENSION_ORIGINS: [&str; 3] = [
    "chrome-extension://",
    "moz-extension://",
    "safari-web-extension://",
];

type ApiResponse = Response<Full<Bytes>>;

struct ServerState {
    running: Option<(u16, tauri::async_runtime::JoinHandle<()>)>,
    error: Option<String>,
}

static SERVER: Mutex<ServerState> = Mutex::new(ServerState {
    running: None,
    error: None,
});
static TOKEN: RwLock<String> = RwLock::new(String::new());

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalApiStatus {
    pub enabled: bool,
    pub port: u16,
    pub token: String,
    pub running: bool,
    pub error: Option<String>,
}

/// Start the server at launch if the user enabled it.
pub fn init(app_handle: &AppHandle) {
    match t_config::load_app_config() {
        Ok(config) => apply(app_handle, &config.local_api),
        Err(e) => eprintln!("Failed to load local API config: {}", e),
    }
}

#[tauri::command]
pub fn get_local_api_status() -> Result<LocalApiStatus, String> {
    Ok(status(&t_config::load_app_config()?.local_api))
}

#[tauri::command]
pub fn set_local_api_config(
    app_handle: AppHandle,
    enabled: bool,
    port: u16,
) -> Result<LocalApiStatus, String> {
    if port == 0 {
        return Err("Invalid port".to_string());
    }
    let mut config = t_config::load_app_config()?;
    config.local_api.enabled = enabled;
    config.local_api.port = port;
    if enabled && config.local_api.token.is_empty() {
        config.local_api.token = generate_token();
    }
    t_config::save_app_config(&config)?;
    apply(&app_handle, &config.local_api);
    Ok(status(&config.local_api))
}

#[tauri::command]
pub fn regenerate_local_api_token(app_handle: AppHandle) -> Result<LocalApiStatus, String> {
    let mut config = t_config::load_app_config()?;
    config.local_api.token = generate_token();
    t_config::save_app_config(&config)?;
    apply(&app_handle, &config.local_api);
    Ok(status(&config.local_api))
}

fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn status(config: &LocalApiConfig) -> LocalApiStatus {
    let (running, error) = SERVER
        .lock()
        .map(|server| (server.running.is_some(), server.error.clone()))
        .unwrap_or((false, None));
    LocalApiStatus {
        enabled: config.enabled,
        port: config.port,
        token: config.token.clone(),
        running,
        error,
    }
}

/// Bring the server in line with `config`: update the token, then start,
/// stop or move the listener as needed.
fn apply(app_handle: &AppHandle, config: &LocalApiConfig) {
    if let Ok(mut token) = TOKEN.write() {
        *token = config.token.clone();
    }
    let Ok(mut server) = SERVER.lock() else {
        return;
    };
    let running_port = server.running.as_ref().map(|(port, _)| *port);
    if config.enabled && running_port == Some(config.port) {
        return;
    }
    if let Some((_, task)) = server.running.take() {
        task.abort();
    }
    server.error = None;
    if config.enabled {
        match start(app_handle, config.port) {
            Ok(task) => server.running = Some((config.port, task)),
            Err(e) => {
                eprintln!("{}", e);
                server.error = Some(e);
            }
        }
    }
}

fn start(
    app_handle: &AppHandle,
    port: u16,
) -> Result<tauri::async_runtime::JoinHandle<()>, String> {
    // Bind synchronously so a port conflict is reported to Settings.
    let listener = std::net::TcpListener::bind(("127.0.0.1", port))
        .and_then(|listener| listener.set_nonblocking(true).map(|_| listener))
        .map_err(|e| format!("Failed to listen on 127.0.0.1:{}: {}", port, e))?;
    let app_handle = app_handle.clone();

    Ok(tauri::async_runtime::spawn(async move {
        let listener = match tokio::net::TcpListener::from_std(listener) {
            Ok(listener) => listener,
            Err(e) => {
                eprintln!("Failed to start local API server: {}", e);
                return;
            }
        };
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            };
            let app_handle = app_handle.clone();
            tauri::async_runtime::spawn(async move {
                let service = service_fn(move |req| handle_request(req, app_handle.clone(), port));
                let _ = http1::Builder::new()
                    .timer(TokioTimer::new())
                    .header_read_timeout(Duration::from_secs(10))
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    }))
}

async fn handle_request(
    req: Request<Incoming>,
    app_handle: AppHandle,
    port: u16,
) -> Result<ApiResponse, Infallible> {
    let origin = req.headers().get(header::ORIGIN).cloned();
    let origin_allowed = origin.as_ref().is_none_or(|origin| {
        origin.to_str().is_ok_and(|origin| {
            EXTENSION_ORIGINS
                .iter()
                .any(|scheme| origin.starts_with(scheme))
        })
    });
    if !origin_allowed || !is_loopback_host(&req, port) {
        return Ok(error_response(StatusCode::FORBIDDEN, "Forbidden"));
    }

    let mut response = route(req, &app_handle).await;
    if let Some(origin) = origin {
        let headers = response.headers_mut();
        headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
        headers.insert(header::VARY, HeaderValue::from_static("Origin"));
    }
    Ok(response)
}

async fn route(req: Request<Incoming>, app_handle: &AppHandle) -> ApiResponse {
    if req.method() == Method::OPTIONS {
        let mut response = Response::new(Full::new(Bytes::new()));
        *response.status_mut() = StatusCode::NO_CONTENT;
        let headers = response.headers_mut();
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_METHODS,
            HeaderValue::from_static("GET, POST, OPTIONS"),
        );
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            HeaderValue::from_static("Authorization, Content-Type"),
        );
        headers.insert(
            header::ACCESS_CONTROL_MAX_AGE,
            HeaderValue::from_static("600"),
        );
        return response;
    }

    if !is_authorized(&req) {
        return error_response(StatusCode::UNAUTHORIZED, "Missing or invalid token");
    }

    match (req.method(), req.uri().path()) {
        (&Method::GET, "/v1/status") => get_status(app_handle),
        (&Method::GET, "/v1/albums") => get_albums().await,
        (&Method::POST, "/v1/import") => post_import(req, app_handle).await,
        _ => error_response(StatusCode::NOT_FOUND, "Not found"),
    }
}

fn is_loopback_host(req: &Request<Incoming>, port: u16) -> bool {
    let Some(host) = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let name = host.strip_suffix(&format!(":{}", port)).unwrap_or(host);
    name == "127.0.0.1" || name.eq_ignore_ascii_case("localhost")
}

fn is_authorized(req: &Request<Incoming>) -> bool {
    let Some(provided) = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    else {
        return false;
    };
    let Ok(token) = TOKEN.read() else {
        return false;
    };
    // Compare in constant time so the token cannot be guessed byte by byte.
    !token.is_empty()
        && provided.len() == token.len()
        && provided
            .bytes()
            .zip(token.bytes())
            .fold(0u8, |diff, (a, b)| diff | (a ^ b))
            == 0
}

#[derive(Serialize)]
struct LibrarySummary {
    id: String,
    name: String,
}

fn current_library() -> Result<LibrarySummary, String> {
    let config = t_config::load_app_config()?;
    let library = config
        .libraries
        .into_iter()
        .find(|library| library.id == config.current_library_id)
        .ok_or_else(|| "Current library not found".to_string())?;
    Ok(LibrarySummary {
        id: library.id,
        name: library.name,
    })
}

fn get_status(app_handle: &AppHandle) -> ApiResponse {
    match current_library() {
        Ok(library) => json_response(
            StatusCode::OK,
            &json!({
                "app": "Lap",
                "version": app_handle.package_info().version.to_string(),
                "library": library,
            }),
        ),
        Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, &e),
    }
}

#[derive(Serialize)]
struct AlbumSummary {
    id: i64,
    name: String,
    path: String,
    folders: Vec<FolderSummary>,
}

#[derive(Serialize)]
struct FolderSummary {
    id: i64,
    name: String,
    path: String,
    is_favorite: bool,
}

async fn get_albums() -> ApiResponse {
    let result = tauri::async_runtime::spawn_blocking(|| {
        let library = current_library()?;
        let albums = list_albums()?;
        Ok::<_, String>(json!({ "library": library, "albums": albums }))
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|result| result);

    match result {
        Ok(body) => json_response(StatusCode::OK, &body),
        Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, &e),
    }
}

/// Albums in sidebar order, each with the subfolders Lap has indexed or
/// visited. The album root itself is addressed by `album_id` on import.
fn list_albums() -> Result<Vec<AlbumSummary>, String> {
    let mut folders_by_album: HashMap<i64, Vec<FolderSummary>> = HashMap::new();
    for folder in AFolder::get_all()? {
        let Some(id) = folder.id else { continue };
        folders_by_album
            .entry(folder.album_id)
            .or_default()
            .push(FolderSummary {
                id,
                name: folder.name,
                path: folder.path,
                is_favorite: folder.is_favorite.unwrap_or(false),
            });
    }

    Ok(Album::get_all_albums()?
        .into_iter()
        .filter_map(|album| {
            let id = album.id?;
            let mut folders = folders_by_album.remove(&id).unwrap_or_default();
            folders.retain(|folder| folder.path != album.path);
            folders.sort_by(|a, b| a.path.cmp(&b.path));
            Some(AlbumSummary {
                id,
                name: album.name,
                path: album.path,
                folders,
            })
        })
        .collect())
}

#[derive(Deserialize)]
struct ImportRequest {
    /// Destination folder; takes precedence over `album_id`.
    folder_id: Option<i64>,
    /// Destination album; the file goes into the album's root folder.
    album_id: Option<i64>,
    /// Image or video to download (http or https).
    url: Option<String>,
    /// Referer sent with the download, for hosts that block hotlinking.
    referer: Option<String>,
    /// Base64-encoded file bytes, as an alternative to `url`.
    data: Option<String>,
    /// MIME type of `data`, e.g. "image/jpeg" or "video/mp4".
    content_type: Option<String>,
    /// File name for `data` (URL imports are named from the response);
    /// required when `content_type` is absent.
    name: Option<String>,
}

#[derive(Serialize)]
struct ImportResponse {
    album_id: i64,
    folder_id: i64,
    file: AFile,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct FilesImportedEvent {
    album_id: i64,
    folder_id: i64,
}

async fn post_import(req: Request<Incoming>, app_handle: &AppHandle) -> ApiResponse {
    let body = match Limited::new(req.into_body(), MAX_BODY_BYTES)
        .collect()
        .await
    {
        Ok(body) => body.to_bytes(),
        Err(_) => {
            return error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Request body is too large or incomplete",
            );
        }
    };
    let request: ImportRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, &format!("Invalid JSON: {}", e)),
    };
    drop(body);

    let (album_id, folder_id) = (request.album_id, request.folder_id);
    if album_id.is_none() && folder_id.is_none() {
        return error_response(StatusCode::BAD_REQUEST, "Provide folder_id or album_id");
    }
    let destination =
        tauri::async_runtime::spawn_blocking(move || resolve_destination(album_id, folder_id))
            .await
            .map_err(|e| e.to_string())
            .and_then(|result| result);
    let (album_id, folder_id, folder_path) = match destination {
        Ok(Some(destination)) => destination,
        Ok(None) => return error_response(StatusCode::NOT_FOUND, "Folder not found"),
        Err(e) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };
    if !t_utils::directory_accessible(&folder_path) {
        return error_response(StatusCode::CONFLICT, "Folder is not accessible");
    }

    let result = match (request.url, request.data) {
        (Some(url), None) => {
            let is_http =
                reqwest::Url::parse(&url).is_ok_and(|url| matches!(url.scheme(), "http" | "https"));
            if !is_http {
                return error_response(StatusCode::BAD_REQUEST, "url must be an http(s) URL");
            }
            t_cmds::import_url_inner(&url, request.referer.as_deref(), folder_id, folder_path).await
        }
        (None, Some(data)) => {
            let bytes = match STANDARD.decode(data.trim()) {
                Ok(bytes) if !bytes.is_empty() => bytes,
                _ => {
                    return error_response(
                        StatusCode::BAD_REQUEST,
                        "data must be non-empty base64",
                    );
                }
            };
            match (request.content_type, request.name) {
                (Some(content_type), name) => {
                    let mime = content_type
                        .split(';')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .to_ascii_lowercase();
                    if let Some(ext) = t_utils::video_mime_to_ext(&mime) {
                        if !t_utils::is_video_header(&bytes[..bytes.len().min(16)]) {
                            return error_response(
                                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                                "data is not a video",
                            );
                        }
                        let name = t_utils::video_file_name(name.as_deref(), ext);
                        t_cmds::import_file_bytes(bytes, name, folder_id, folder_path).await
                    } else if t_utils::image_mime_to_ext(&mime).is_some() {
                        t_cmds::import_image_bytes(bytes, mime, name, folder_id, folder_path).await
                    } else {
                        return error_response(
                            StatusCode::UNSUPPORTED_MEDIA_TYPE,
                            &format!("Unsupported format: {}", mime),
                        );
                    }
                }
                (None, Some(name)) => {
                    t_cmds::import_file_bytes(bytes, name, folder_id, folder_path).await
                }
                (None, None) => {
                    return error_response(
                        StatusCode::BAD_REQUEST,
                        "data requires content_type or name",
                    );
                }
            }
        }
        _ => return error_response(StatusCode::BAD_REQUEST, "Provide either url or data"),
    };

    match result {
        Ok(Some(file)) => {
            let _ = app_handle.emit(
                "local-api-files-imported",
                FilesImportedEvent {
                    album_id,
                    folder_id,
                },
            );
            json_response(
                StatusCode::CREATED,
                &ImportResponse {
                    album_id,
                    folder_id,
                    file,
                },
            )
        }
        Ok(None) => error_response(StatusCode::UNPROCESSABLE_ENTITY, "Nothing was imported"),
        Err(e) => error_response(StatusCode::UNPROCESSABLE_ENTITY, &e),
    }
}

/// Resolve an import destination to (album id, folder id, folder path), or
/// `None` when the album or folder does not exist in the current library.
fn resolve_destination(
    album_id: Option<i64>,
    folder_id: Option<i64>,
) -> Result<Option<(i64, i64, String)>, String> {
    if let Some(folder_id) = folder_id {
        return Ok(
            AFolder::get_by_id(folder_id)?.map(|folder| (folder.album_id, folder_id, folder.path))
        );
    }
    let Some(album_id) = album_id else {
        return Ok(None);
    };
    let Some(album) = Album::get_all_albums()?
        .into_iter()
        .find(|album| album.id == Some(album_id))
    else {
        return Ok(None);
    };
    let folder = AFolder::add_to_db(album_id, &album.path)?;
    Ok(folder
        .id
        .map(|folder_id| (album_id, folder_id, folder.path)))
}

fn json_response(status: StatusCode, body: &impl Serialize) -> ApiResponse {
    let (status, body) = match serde_json::to_vec(body) {
        Ok(body) => (status, body),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": e.to_string() }).to_string().into_bytes(),
        ),
    };
    let mut response = Response::new(Full::new(Bytes::from(body)));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

fn error_response(status: StatusCode, message: &str) -> ApiResponse {
    json_response(status, &json!({ "error": message }))
}

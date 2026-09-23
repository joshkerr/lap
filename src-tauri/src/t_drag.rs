/**
 * t_drag.rs - Drag files out of Lap into other apps
 *
 * The grid's drag is drawn inside the web page, so it cannot leave the window.
 * When the pointer does leave it, the frontend calls `start_drag_out` and the
 * drag continues as a native OS drag carrying the files, which Finder,
 * Explorer, editors, chat apps and browser upload fields accept.
 */
use crate::t_sqlite::AThumb;
use image::{ImageFormat, Rgba, RgbaImage};
use std::io::Cursor;
use std::path::PathBuf;
use tauri::{AppHandle, Emitter, Window};

/// Longest side of the image shown under the cursor during the drag.
const PREVIEW_SIZE: u32 = 128;

#[derive(Clone, serde::Serialize)]
struct DragOutFinished {
    dropped: bool,
}

/// Start a native drag of `file_paths` from `window`. The mouse button must
/// still be held. Emits `drag-out-finished` when the drag ends.
#[tauri::command]
pub async fn start_drag_out(
    app_handle: AppHandle,
    window: Window,
    file_paths: Vec<String>,
    preview_file_id: Option<i64>,
) -> Result<(), String> {
    let paths: Vec<PathBuf> = file_paths
        .into_iter()
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .collect();
    if paths.is_empty() {
        return Err("No files to drag".to_string());
    }

    let preview = tauri::async_runtime::spawn_blocking(move || preview_png(preview_file_id))
        .await
        .map_err(|e| e.to_string())?;

    let (tx, rx) = tokio::sync::oneshot::channel();
    let emitter = app_handle.clone();
    app_handle
        .run_on_main_thread(move || {
            #[cfg(target_os = "linux")]
            let handle = window.gtk_window().map_err(|e| e.to_string());
            #[cfg(not(target_os = "linux"))]
            let handle: Result<Window, String> = Ok(window);

            let result = handle.and_then(|handle| {
                drag::start_drag(
                    &handle,
                    drag::DragItem::Files(paths),
                    drag::Image::Raw(preview),
                    move |result, _| {
                        let dropped = matches!(result, drag::DragResult::Dropped);
                        let _ = emitter.emit("drag-out-finished", DragOutFinished { dropped });
                    },
                    // Copy, never move: a drop elsewhere must not take files
                    // out of the library.
                    drag::Options {
                        mode: drag::DragMode::Copy,
                        ..Default::default()
                    },
                )
                .map_err(|e| e.to_string())
            });
            let _ = tx.send(result);
        })
        .map_err(|e| e.to_string())?;

    rx.await.map_err(|e| e.to_string())?
}

/// The image shown under the cursor, from the file's thumbnail.
fn preview_png(file_id: Option<i64>) -> Vec<u8> {
    let thumb_data = file_id
        .and_then(|id| AThumb::fetch(id).ok().flatten())
        .and_then(|thumb| thumb.thumb_data);
    render_preview(thumb_data.as_deref())
}

/// A PNG of `thumb_data` scaled down, or a plain tile when there is none or
/// it cannot be read. Always returns a valid image, because the macOS drag
/// code aborts on one it cannot decode.
fn render_preview(thumb_data: Option<&[u8]>) -> Vec<u8> {
    let thumbnail = thumb_data
        .and_then(|data| image::load_from_memory(data).ok())
        .map(|image| image.thumbnail(PREVIEW_SIZE, PREVIEW_SIZE).into_rgba8());
    let image = thumbnail.unwrap_or_else(|| {
        RgbaImage::from_pixel(
            PREVIEW_SIZE / 2,
            PREVIEW_SIZE / 2,
            Rgba([128, 128, 128, 160]),
        )
    });

    let mut png = Vec::new();
    if image
        .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
        .is_err()
    {
        png.clear();
        let _ = RgbaImage::from_pixel(1, 1, Rgba([0, 0, 0, 0]))
            .write_to(&mut Cursor::new(&mut png), ImageFormat::Png);
    }
    png
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(png: &[u8]) -> image::DynamicImage {
        assert_eq!(image::guess_format(png).ok(), Some(ImageFormat::Png));
        image::load_from_memory(png).expect("preview must decode")
    }

    #[test]
    fn scales_a_thumbnail_to_the_preview_size() {
        // Lap's cached thumbnails are JPEG (or PNG).
        let mut jpeg = Vec::new();
        image::RgbImage::from_pixel(512, 256, image::Rgb([200, 30, 30]))
            .write_to(&mut Cursor::new(&mut jpeg), ImageFormat::Jpeg)
            .unwrap();
        let preview = decode(&render_preview(Some(&jpeg)));
        assert_eq!(
            (preview.width(), preview.height()),
            (PREVIEW_SIZE, PREVIEW_SIZE / 2)
        );
    }

    #[test]
    fn falls_back_to_a_tile_without_a_usable_thumbnail() {
        for data in [None, Some(&b"not an image"[..]), Some(&[][..])] {
            let preview = decode(&render_preview(data));
            assert_eq!(preview.width(), PREVIEW_SIZE / 2);
        }
    }
}

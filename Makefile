# Makefile — build and install Lap from source so `updateproj` can manage it
# (it runs `make build && make install` after every pull).
#
# Prereqs (see README "Build from Source"): Rust stable, Node 20+, pnpm,
# tauri-cli (`cargo install tauri-cli --version "^2.0.0" --locked`), and the
# platform system deps. `make deps` fetches everything else the build needs.

SHELL := /bin/bash
.PHONY: deps build install dev clean help

UNAME_S := $(shell uname -s)
BUNDLE_DIR := $(CURDIR)/src-tauri/target/release/bundle

# Updater artifacts need upstream's private signing key, so local builds
# turn them off.
TAURI_CONFIG := {"bundle":{"createUpdaterArtifacts":false}}

ifeq ($(OS),Windows_NT)
  BUNDLES := msi
else ifeq ($(UNAME_S),Darwin)
  BUNDLES := app
else
  BUNDLES := deb
endif

help:
	@echo "make deps     - submodules, AI models, ffmpeg sidecar, frontend packages"
	@echo "make build    - release build + $(BUNDLES) bundle (runs deps first)"
	@echo "make install  - install the bundle for this platform"
	@echo "make dev      - cargo tauri dev (hot reload)"
	@echo "make clean    - remove build output"

# Everything here is idempotent: already-fetched files are skipped.
deps:
	git submodule update --init --recursive
ifeq ($(OS),Windows_NT)
	powershell -NoProfile -ExecutionPolicy Bypass -File scripts/download_models.ps1
	powershell -NoProfile -ExecutionPolicy Bypass -File scripts/download_ffmpeg_sidecar.ps1
else
	./scripts/download_models.sh
	./scripts/download_ffmpeg_sidecar.sh
endif
	pnpm --dir src-vite install --frozen-lockfile

build: deps
	cargo tauri build --bundles $(BUNDLES) --config '$(TAURI_CONFIG)'

install:
ifeq ($(OS),Windows_NT)
	@msi=$$(ls -t $(BUNDLE_DIR)/msi/Lap_*.msi | head -1); \
	echo "Installing $$msi"; \
	msiexec //i "$$(cygpath -w "$$msi")" //passive
else ifeq ($(UNAME_S),Darwin)
	@rm -rf /Applications/Lap.app; \
	ditto "$(BUNDLE_DIR)/macos/Lap.app" /Applications/Lap.app; \
	touch /Applications/Lap.app; \
	/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -f /Applications/Lap.app; \
	killall Dock 2>/dev/null || true; \
	echo "Installed to /Applications/Lap.app"
else
	@deb=$$(ls -t $(BUNDLE_DIR)/deb/Lap_*.deb | head -1); \
	echo "Installing $$deb"; \
	if command -v pkexec >/dev/null 2>&1 && [ -n "$$DISPLAY$$WAYLAND_DISPLAY" ]; then \
		pkexec apt-get install -y --reinstall "$$deb"; \
	else \
		sudo apt-get install -y --reinstall "$$deb"; \
	fi
endif

dev:
	cargo tauri dev

clean:
	rm -rf src-tauri/target src-vite/dist

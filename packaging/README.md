# Packaging & Distribution Guide

## Linux

### AppImage, `.deb`, and `.rpm` (Tauri Bundler)
```sh
cd src-tauri
cargo tauri build --bundles deb,rpm,appimage
```
Output bundles are written to `src-tauri/target/release/bundle/`.

### Arch Linux (AUR `PKGBUILD`)
```sh
cd packaging/aur
makepkg -si
```
Note: update `pkgver` and `sha256sums` when cutting a tagged release.

### Flatpak
```sh
flatpak-builder --user --install --force-clean build-dir packaging/flatpak/io.github.micheta.rex-music.yml
```

## Windows
Requires Rust (`x86_64-pc-windows-msvc`), WebView2, and WiX Toolset v3/v4 (for `.msi`):
```powershell
cd src-tauri
cargo tauri build --bundles msi,nsis
```
For a portable `.zip`, bundle `src-tauri/target/release/rex-music.exe` alongside `LICENSE` and `README.md`.

## macOS
Requires Xcode Command Line Tools and both Apple Silicon + Intel Rust targets (`aarch64-apple-darwin`, `x86_64-apple-darwin`):
```sh
cd src-tauri
cargo tauri build --target universal-apple-darwin --bundles dmg
```
Set `APPLE_SIGNING_IDENTITY`, `APPLE_ID`, `APPLE_PASSWORD`, and `APPLE_TEAM_ID` for Developer ID signing and notarization.

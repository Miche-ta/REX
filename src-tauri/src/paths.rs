//! Cross-platform standard directories for config, cache, data (playlists), and music.
//!
//! Uses the `dirs` crate so Linux follows XDG Base Directory (`~/.config`, `~/.cache`,
//! `~/.local/share`), Windows uses `%APPDATA%` / `%LOCALAPPDATA%` / `%USERPROFILE%\Music`,
//! and macOS uses `~/Library/Application Support`, `~/Library/Caches`, and `~/Music`.

use std::path::PathBuf;

const APP_DIR: &str = "rex-music";

pub fn config_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join(APP_DIR))
}

pub fn cache_dir() -> Option<PathBuf> {
    dirs::cache_dir().map(|d| d.join(APP_DIR))
}

pub fn data_dir() -> Option<PathBuf> {
    dirs::data_local_dir().map(|d| d.join(APP_DIR))
}

pub fn default_music_dir() -> PathBuf {
    dirs::audio_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join("Music")))
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directories_end_with_app_name() {
        if let Some(p) = config_dir() {
            assert_eq!(p.file_name().and_then(|s| s.to_str()), Some(APP_DIR));
        }
        if let Some(p) = cache_dir() {
            assert_eq!(p.file_name().and_then(|s| s.to_str()), Some(APP_DIR));
        }
        if let Some(p) = data_dir() {
            assert_eq!(p.file_name().and_then(|s| s.to_str()), Some(APP_DIR));
        }
    }
}

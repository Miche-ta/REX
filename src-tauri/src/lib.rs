mod audio_engine;
mod audio_probe;
mod library;
mod mpris;
pub mod logger;

use std::sync::Mutex;
use tauri::State;

use audio_engine::{AudioEngineHandle, PlaybackStatus};
use audio_probe::{get_audio_devices, AudioDevice};
use library::{scan_directory, TrackInfo};
use logger::{clear_logs, get_logs, LogEntry};

struct AppState {
    engine: AudioEngineHandle,
    cached_library: Mutex<Vec<TrackInfo>>,
}

#[tauri::command(async)]
fn list_audio_devices() -> Vec<AudioDevice> {
    get_audio_devices()
}

#[tauri::command(async)]
fn play_track(state: State<'_, AppState>, file_path: String, device_id: String) {
    state.engine.play(file_path, device_id);
}

#[tauri::command(async)]
fn queue_next(state: State<'_, AppState>, file_path: Option<String>) {
    state.engine.set_next(file_path);
}

#[tauri::command(async)]
fn set_volume(device_id: String, percent: u32, gain_db: Option<f64>) -> bool {
    audio_engine::set_hw_volume(&device_id, percent, gain_db.unwrap_or(0.0))
}

#[tauri::command(async)]
fn get_volume(device_id: String) -> Option<u32> {
    audio_engine::get_hw_volume(&device_id)
}

#[tauri::command(async)]
fn get_cover(file_path: String) -> Option<String> {
    library::get_cover(&file_path)
}

#[tauri::command(async)]
fn mpris_update(title: String, artist: String, album: String, length_secs: f64, file_path: Option<String>,
                can_next: bool, can_prev: bool, status: String) {
    let art = file_path.as_deref().and_then(cover_file_url).unwrap_or_default();
    mpris::update(title, artist, album, length_secs, art, can_next, can_prev, &status);
}

/// Desktop widgets want a file URL for the cover, so write it to the cache dir (only the current cover is kept).
fn cover_file_url(track: &str) -> Option<String> {
    use std::hash::{Hash, Hasher};
    let (bytes, mime) = library::cover_bytes(track)?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    let ext = if mime.contains("png") { "png" } else { "jpg" };
    let base = std::env::var("XDG_CACHE_HOME").ok()
        .or_else(|| std::env::var("HOME").ok().map(|h| format!("{h}/.cache")))?;
    let dir = std::path::Path::new(&base).join("rex-music");
    std::fs::create_dir_all(&dir).ok()?;
    let name = format!("cover-{:x}.{ext}", h.finish());
    let target = dir.join(&name);
    if !target.exists() {
        for e in std::fs::read_dir(&dir).ok()?.flatten() {
            if e.file_name().to_string_lossy().starts_with("cover-") { let _ = std::fs::remove_file(e.path()); }
        }
        std::fs::write(&target, bytes).ok()?;
    }
    Some(format!("file://{}", target.display()))
}

// ---- Playlists: plain .m3u files in ~/.local/share/rex-music/playlists ----
fn playlist_dir() -> Option<std::path::PathBuf> {
    let base = std::env::var("XDG_DATA_HOME").ok()
        .or_else(|| std::env::var("HOME").ok().map(|h| format!("{h}/.local/share")))?;
    let dir = std::path::Path::new(&base).join("rex-music").join("playlists");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

fn playlist_path(name: &str) -> Result<std::path::PathBuf, String> {
    let ok = !name.trim().is_empty() && name.chars().all(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.')) && !name.starts_with('.');
    if !ok { return Err("Use letters, numbers, spaces, - and _ in the name".into()); }
    Ok(playlist_dir().ok_or("No data directory")?.join(format!("{}.m3u", name.trim())))
}

#[tauri::command(async)]
fn list_playlists() -> Vec<String> {
    let Some(dir) = playlist_dir() else { return vec![] };
    let mut names: Vec<String> = std::fs::read_dir(dir).into_iter().flatten().flatten()
        .filter_map(|e| e.path().file_stem().map(|s| s.to_string_lossy().into_owned())
            .filter(|_| e.path().extension().map_or(false, |x| x == "m3u")))
        .collect();
    names.sort();
    names
}

#[tauri::command(async)]
fn save_playlist(name: String, paths: Vec<String>) -> Result<(), String> {
    let path = playlist_path(&name)?;
    let mut text = String::from("#EXTM3U\n");
    for p in paths { text.push_str(&p); text.push('\n'); }
    std::fs::write(path, text).map_err(|e| e.to_string())
}

/// Append one track to a playlist, creating the playlist if it does not exist.
#[tauri::command(async)]
fn add_to_playlist(name: String, path: String) -> Result<(), String> {
    use std::io::Write;
    let file = playlist_path(&name)?;
    let is_new = !file.exists();
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&file).map_err(|e| e.to_string())?;
    if is_new { writeln!(f, "#EXTM3U").map_err(|e| e.to_string())?; }
    writeln!(f, "{path}").map_err(|e| e.to_string())
}

#[tauri::command(async)]
fn rename_playlist(old: String, new: String) -> Result<(), String> {
    let to = playlist_path(&new)?;
    if to.exists() { return Err("A playlist with that name already exists".into()); }
    std::fs::rename(playlist_path(&old)?, to).map_err(|e| e.to_string())
}

#[tauri::command(async)]
fn delete_playlist(name: String) -> Result<(), String> {
    std::fs::remove_file(playlist_path(&name)?).map_err(|e| e.to_string())
}

/// Absolute file paths of a playlist; relative entries resolve against the playlist's folder.
#[tauri::command(async)]
fn load_playlist(name: String) -> Result<Vec<String>, String> {
    let path = playlist_path(&name)?;
    let dir = path.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    Ok(text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| if l.starts_with('/') { l.to_string() } else { dir.join(l).display().to_string() })
        .collect())
}

#[tauri::command(async)]
fn get_lyrics(file_path: String) -> Option<String> {
    library::get_lyrics(&file_path)
}

#[tauri::command(async)]
fn pause_track(state: State<'_, AppState>) {
    state.engine.pause();
}

#[tauri::command(async)]
fn resume_track(state: State<'_, AppState>) {
    state.engine.resume();
}

#[tauri::command(async)]
fn stop_track(state: State<'_, AppState>) {
    state.engine.stop();
}

#[tauri::command(async)]
fn seek_track(state: State<'_, AppState>, position_seconds: f64) {
    state.engine.seek(position_seconds);
}

#[tauri::command(async)]
fn get_playback_status(state: State<'_, AppState>) -> PlaybackStatus {
    state.engine.get_status()
}

#[tauri::command(async)]
fn scan_music_directory(state: State<'_, AppState>, dir_path: Option<String>) -> Vec<TrackInfo> {
    let target_dir = dir_path.unwrap_or_else(|| {
        std::env::var("HOME")
            .map(|h| format!("{}/Music", h))
            .unwrap_or_else(|_| ".".into())
    });

    let tracks = scan_directory(&target_dir);
    if let Ok(mut cache) = state.cached_library.lock() {
        *cache = tracks.clone();
    }
    tracks
}

#[tauri::command(async)]
fn get_default_music_dir() -> String {
    std::env::var("HOME")
        .map(|h| format!("{}/Music", h))
        .unwrap_or_else(|_| ".".into())
}

#[tauri::command(async)]
fn get_engine_logs() -> Vec<LogEntry> {
    get_logs()
}

#[tauri::command(async)]
fn clear_engine_logs() {
    clear_logs();
}

#[tauri::command(async)]
fn get_kernel_hw_params() -> String {
    let mut out = String::new();
    for i in 0..5 {
        let pcm_path = format!("/proc/asound/card{}/pcm0p/sub0/hw_params", i);
        if let Ok(c) = std::fs::read_to_string(&pcm_path) {
            let card_name_path = format!("/proc/asound/card{}/id", i);
            let name = std::fs::read_to_string(&card_name_path).unwrap_or_else(|_| format!("Card {}", i));
            out.push_str(&format!("=== Card {} [{}]: /proc/asound/card{}/pcm0p/sub0/hw_params ===\n{}\n\n", i, name.trim(), i, c.trim()));
        }
    }
    if out.is_empty() {
        "No active ALSA playback streams found in /proc/asound/ (devices are closed or suspended)".into()
    } else {
        out
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let engine = AudioEngineHandle::new();
    let app_state = AppState {
        engine,
        cached_library: Mutex::new(Vec::new()),
    };

    tauri::Builder::default()
        .setup(|app| {
            mpris::start_mpris_server(app.handle().clone());
            Ok(())
        })
        .manage(app_state)
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            list_audio_devices,
            play_track,
            get_cover,
            get_lyrics,
            mpris_update,
            queue_next,
            list_playlists,
            save_playlist,
            add_to_playlist,
            delete_playlist,
            rename_playlist,
            load_playlist,
            set_volume,
            get_volume,
            pause_track,
            resume_track,
            stop_track,
            seek_track,
            get_playback_status,
            scan_music_directory,
            get_default_music_dir,
            get_engine_logs,
            clear_engine_logs,
            get_kernel_hw_params,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|_, event| {
            if let tauri::RunEvent::Exit = event {
                audio_engine::release_device_to_pipewire();
            }
        });
}

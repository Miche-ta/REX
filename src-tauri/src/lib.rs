mod audio_engine;
mod audio_probe;
mod library;
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

#[tauri::command]
fn list_audio_devices() -> Vec<AudioDevice> {
    get_audio_devices()
}

#[tauri::command]
fn play_track(state: State<'_, AppState>, file_path: String, device_id: String) {
    state.engine.play(file_path, device_id);
}

#[tauri::command]
fn set_volume(device_id: String, percent: u32) -> bool {
    audio_engine::set_hw_volume(&device_id, percent)
}

#[tauri::command]
fn get_volume(device_id: String) -> Option<u32> {
    audio_engine::get_hw_volume(&device_id)
}

#[tauri::command]
fn get_cover(file_path: String) -> Option<String> {
    library::get_cover(&file_path)
}

#[tauri::command]
fn pause_track(state: State<'_, AppState>) {
    state.engine.pause();
}

#[tauri::command]
fn resume_track(state: State<'_, AppState>) {
    state.engine.resume();
}

#[tauri::command]
fn stop_track(state: State<'_, AppState>) {
    state.engine.stop();
}

#[tauri::command]
fn seek_track(state: State<'_, AppState>, position_seconds: f64) {
    state.engine.seek(position_seconds);
}

#[tauri::command]
fn get_playback_status(state: State<'_, AppState>) -> PlaybackStatus {
    state.engine.get_status()
}

#[tauri::command]
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

#[tauri::command]
fn get_default_music_dir() -> String {
    std::env::var("HOME")
        .map(|h| format!("{}/Music", h))
        .unwrap_or_else(|_| ".".into())
}

#[tauri::command]
fn get_engine_logs() -> Vec<LogEntry> {
    get_logs()
}

#[tauri::command]
fn clear_engine_logs() {
    clear_logs();
}

#[tauri::command]
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
        .manage(app_state)
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            list_audio_devices,
            play_track,
            get_cover,
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
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

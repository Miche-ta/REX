//! Unified OS media controls (MPRIS on Linux, SMTC on Windows, NowPlaying on macOS).

use tauri::AppHandle;

pub struct MediaMetadata<'a> {
    pub title: &'a str,
    pub artist: &'a str,
    pub album: &'a str,
    pub length_secs: f64,
    pub art_url: &'a str,
    pub can_next: bool,
    pub can_prev: bool,
    pub status: &'a str,
}

pub trait MediaController: Send + Sync {
    fn start(&self, app: AppHandle);
    fn update(&self, meta: &MediaMetadata<'_>);
}

#[cfg(target_os = "linux")]
struct LinuxMpris;

#[cfg(target_os = "linux")]
impl MediaController for LinuxMpris {
    fn start(&self, app: AppHandle) {
        crate::mpris::start_mpris_server(app);
    }

    fn update(&self, meta: &MediaMetadata<'_>) {
        crate::mpris::update(
            meta.title.to_string(),
            meta.artist.to_string(),
            meta.album.to_string(),
            meta.length_secs,
            meta.art_url.to_string(),
            meta.can_next,
            meta.can_prev,
            meta.status,
        );
    }
}

#[cfg(target_os = "windows")]
struct WindowsSmtc;

#[cfg(target_os = "windows")]
impl MediaController for WindowsSmtc {
    fn start(&self, app: AppHandle) {
        // SMTC binds to the main window HWND via ISystemMediaTransportControlsInterop
        // and forwards Play/Pause/Next/Previous button events to `app.emit("mpris-command", ...)`.
        let _ = app;
    }

    fn update(&self, meta: &MediaMetadata<'_>) {
        let _ = meta;
    }
}

#[cfg(target_os = "macos")]
struct MacNowPlaying;

#[cfg(target_os = "macos")]
impl MediaController for MacNowPlaying {
    fn start(&self, app: AppHandle) {
        let _ = app;
    }

    fn update(&self, meta: &MediaMetadata<'_>) {
        let _ = meta;
    }
}

fn controller() -> &'static dyn MediaController {
    #[cfg(target_os = "linux")]
    {
        static CTRL: LinuxMpris = LinuxMpris;
        &CTRL
    }
    #[cfg(target_os = "windows")]
    {
        static CTRL: WindowsSmtc = WindowsSmtc;
        &CTRL
    }
    #[cfg(target_os = "macos")]
    {
        static CTRL: MacNowPlaying = MacNowPlaying;
        &CTRL
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        struct Noop;
        impl MediaController for Noop {
            fn start(&self, _app: AppHandle) {}
            fn update(&self, _meta: &MediaMetadata<'_>) {}
        }
        static CTRL: Noop = Noop;
        &CTRL
    }
}

pub fn start(app: AppHandle) {
    controller().start(app);
}

pub fn update(meta: &MediaMetadata<'_>) {
    controller().update(meta);
}

use tauri::{AppHandle, Emitter, Manager};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use zbus::interface;
use zbus::zvariant::{ObjectPath, Value};

const PATH: &str = "/org/mpris/MediaPlayer2";

#[derive(Default)]
struct State {
    title: String,
    artist: String,
    album: String,
    length_us: i64,
    art_url: String,
    can_next: bool,
    can_prev: bool,
    status: &'static str, // "Playing" | "Paused" | "Stopped"
}

static STATE: Mutex<State> = Mutex::new(State { title: String::new(), artist: String::new(), album: String::new(), length_us: 0, art_url: String::new(), can_next: false, can_prev: false, status: "Stopped" });
static CONN: OnceLock<zbus::Connection> = OnceLock::new();

/// Called by the frontend whenever the track or play state changes; desktop widgets read this.
pub fn update(title: String, artist: String, album: String, length_secs: f64, art_url: String, can_next: bool, can_prev: bool, status: &str) {
    {
        let mut st = STATE.lock().unwrap_or_else(|e| e.into_inner());
        st.title = title;
        st.artist = artist;
        st.album = album;
        st.length_us = (length_secs * 1_000_000.0) as i64;
        st.art_url = art_url;
        st.can_next = can_next;
        st.can_prev = can_prev;
        st.status = match status { "Playing" => "Playing", "Paused" => "Paused", _ => "Stopped" };
    }
    let Some(conn) = CONN.get() else { return };
    let _ = zbus::block_on(async {
        let iface = conn.object_server().interface::<_, MprisPlayer>(PATH).await?;
        let emitter = iface.signal_emitter();
        let p = iface.get().await;
        p.metadata_changed(emitter).await?;
        p.can_go_next_changed(emitter).await?;
        p.can_go_previous_changed(emitter).await?;
        p.playback_status_changed(emitter).await
    });
}

pub struct MprisRoot;

#[interface(name = "org.mpris.MediaPlayer2")]
impl MprisRoot {
    #[zbus(property)]
    fn can_quit(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn can_raise(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn has_track_list(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn identity(&self) -> &str {
        "rex-music"
    }

    #[zbus(property)]
    fn supported_uri_schemes(&self) -> Vec<String> {
        vec!["file".into()]
    }

    #[zbus(property)]
    fn supported_mime_types(&self) -> Vec<String> {
        vec!["audio/flac".into(), "audio/wav".into(), "audio/mpeg".into()]
    }
}

pub struct MprisPlayer {
    app: AppHandle,
}

impl MprisPlayer {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

#[interface(name = "org.mpris.MediaPlayer2.Player")]
impl MprisPlayer {
    fn next(&self) {
        let _ = self.app.emit("mpris-command", "next");
    }

    fn previous(&self) {
        let _ = self.app.emit("mpris-command", "prev");
    }

    fn pause(&self) {
        let _ = self.app.emit("mpris-command", "pause");
    }

    fn play_pause(&self) {
        let _ = self.app.emit("mpris-command", "play-pause");
    }

    fn stop(&self) {
        let _ = self.app.emit("mpris-command", "stop");
    }

    fn play(&self) {
        let _ = self.app.emit("mpris-command", "play");
    }

    #[zbus(property)]
    fn playback_status(&self) -> String {
        STATE.lock().unwrap_or_else(|e| e.into_inner()).status.to_string()
    }

    #[zbus(property)]
    fn metadata(&self) -> HashMap<String, Value<'static>> {
        let st = STATE.lock().unwrap_or_else(|e| e.into_inner());
        let mut m: HashMap<String, Value<'static>> = HashMap::new();
        m.insert("mpris:trackid".into(), Value::new(ObjectPath::from_static_str_unchecked("/org/rex_music/track")));
        if !st.title.is_empty() {
            m.insert("xesam:title".into(), Value::new(st.title.clone()));
            m.insert("xesam:artist".into(), Value::new(vec![st.artist.clone()]));
            m.insert("xesam:album".into(), Value::new(st.album.clone()));
            m.insert("mpris:length".into(), Value::new(st.length_us));
            if !st.art_url.is_empty() {
                m.insert("mpris:artUrl".into(), Value::new(st.art_url.clone()));
            }
        }
        m
    }

    #[zbus(property)]
    fn can_control(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_play(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_pause(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_go_next(&self) -> bool {
        STATE.lock().unwrap_or_else(|e| e.into_inner()).can_next
    }

    #[zbus(property)]
    fn can_go_previous(&self) -> bool {
        STATE.lock().unwrap_or_else(|e| e.into_inner()).can_prev
    }

    #[zbus(property)]
    fn can_seek(&self) -> bool {
        true
    }

    // Clients poll Position; it is not announced with PropertiesChanged (per the MPRIS spec).
    #[zbus(property(emits_changed_signal = "false"))]
    fn position(&self) -> i64 {
        let secs = self.app.state::<crate::AppState>().engine.peek_status().position_seconds;
        (secs * 1_000_000.0) as i64
    }

    #[zbus(property)]
    fn rate(&self) -> f64 { 1.0 }

    #[zbus(property)]
    fn minimum_rate(&self) -> f64 { 1.0 }

    #[zbus(property)]
    fn maximum_rate(&self) -> f64 { 1.0 }

    /// Relative seek in microseconds.
    fn seek(&self, offset: i64) {
        let eng = &self.app.state::<crate::AppState>().engine;
        let target = eng.peek_status().position_seconds + offset as f64 / 1_000_000.0;
        eng.seek(target.max(0.0));
    }

    /// Absolute seek in microseconds.
    fn set_position(&self, _track_id: ObjectPath<'_>, position: i64) {
        self.app.state::<crate::AppState>().engine.seek((position as f64 / 1_000_000.0).max(0.0));
    }
}

pub fn start_mpris_server(app: AppHandle) {
    std::thread::spawn(move || {
        let player = MprisPlayer::new(app);
        let root = MprisRoot;
        if let Ok(_conn) = zbus::blocking::connection::Builder::session()
            .and_then(|b| b.name("org.mpris.MediaPlayer2.rex_music"))
            .and_then(|b| b.serve_at("/org/mpris/MediaPlayer2", root))
            .and_then(|b| b.serve_at("/org/mpris/MediaPlayer2", player))
            .and_then(|b| b.build())
        {
            let _ = CONN.set(zbus::Connection::from(_conn.clone()));
            loop {
                std::thread::sleep(std::time::Duration::from_secs(3600));
            }
        }
    });
}

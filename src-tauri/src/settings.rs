//! One config file (~/.config/rex-music/config.json): audio buffer settings read by the engine, plus
//! the UI's small settings (view, device, shuffle...). Written atomically; unknown or missing
//! fields fall back to defaults, so old files keep working.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

pub const VERSION: u32 = 1;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub version: u32,
    pub buffer_ms: u32,
    pub period_ms: u32,
    pub reserve_dbus: bool, // free the card via ReserveDevice1 instead of pactl
    pub ui: BTreeMap<String, String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self { version: VERSION, buffer_ms: 500, period_ms: 100, reserve_dbus: false, ui: BTreeMap::new() }
    }
}

impl Settings {
    /// Keep audio settings in a range the hardware can reasonably take.
    pub fn clamp(&mut self) {
        self.buffer_ms = self.buffer_ms.clamp(50, 4000);
        self.period_ms = self.period_ms.clamp(5, (self.buffer_ms / 2).max(5));
    }
}

static CURRENT: Mutex<Option<Settings>> = Mutex::new(None);

fn path() -> Option<PathBuf> {
    let base = std::env::var("XDG_CONFIG_HOME").ok()
        .or_else(|| std::env::var("HOME").ok().map(|h| format!("{h}/.config")))?;
    Some(PathBuf::from(base).join("rex-music").join("config.json"))
}

fn parse(text: &str) -> Settings {
    let mut s: Settings = serde_json::from_str(text).unwrap_or_default();
    s.version = VERSION;
    s.clamp();
    s
}

fn load() -> Settings {
    path().and_then(|p| std::fs::read_to_string(p).ok()).map(|t| parse(&t)).unwrap_or_default()
}

fn save(s: &Settings) {
    let Some(p) = path() else { return };
    let Some(dir) = p.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() { return; }
    let tmp = p.with_extension("json.tmp");
    // write then rename: a power cut leaves either the old file or the new one, never half of one
    if let Ok(text) = serde_json::to_string_pretty(s) {
        if std::fs::write(&tmp, text).is_ok() { let _ = std::fs::rename(&tmp, &p); }
    }
}

pub fn get() -> Settings {
    CURRENT.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(load).clone()
}

pub fn update(f: impl FnOnce(&mut Settings)) -> Settings {
    let mut g = CURRENT.lock().unwrap_or_else(|e| e.into_inner());
    let s = g.get_or_insert_with(load);
    f(s);
    s.clamp();
    save(s);
    s.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_or_corrupt_file_gives_defaults() {
        assert_eq!(parse(""), Settings::default());
        assert_eq!(parse("{ not json"), Settings::default());
    }

    #[test]
    fn missing_fields_fall_back_and_known_ones_load() {
        let s = parse(r#"{"buffer_ms": 1000, "ui": {"view": "library"}}"#);
        assert_eq!((s.buffer_ms, s.period_ms), (1000, 100));
        assert_eq!(s.ui.get("view").map(String::as_str), Some("library"));
    }

    #[test]
    fn clamps_silly_values() {
        let mut s = Settings { buffer_ms: 10, period_ms: 9999, ..Settings::default() };
        s.clamp();
        assert_eq!((s.buffer_ms, s.period_ms), (50, 25));
        let mut s = Settings { buffer_ms: 999_999, period_ms: 0, ..Settings::default() };
        s.clamp();
        assert_eq!((s.buffer_ms, s.period_ms), (4000, 5));
    }

    #[test]
    fn roundtrips_through_json() {
        let mut s = Settings::default();
        s.ui.insert("shuffle".into(), "1".into());
        assert_eq!(parse(&serde_json::to_string(&s).unwrap()), s);
    }
}

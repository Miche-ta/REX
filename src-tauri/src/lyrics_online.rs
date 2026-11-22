//! Online lyrics from LRCLIB (https://lrclib.net). Strictly opt-in: nothing here runs unless the user
//! presses "Fetch online" or turns on auto-fetch. It sends title, artist, album and length to lrclib.net.
//! Results are cached under ~/.cache/rex-music/lyrics (never in the music folders), so each song is asked once.
use serde::Deserialize;
use std::path::PathBuf;
use std::time::Duration;

const API: &str = "https://lrclib.net/api";

#[derive(Deserialize)]
struct Entry {
    #[serde(rename = "syncedLyrics")]
    synced: Option<String>,
    #[serde(rename = "plainLyrics")]
    plain: Option<String>,
    duration: Option<f64>,
}

fn best_text(e: &Entry) -> Option<String> {
    [e.synced.as_deref(), e.plain.as_deref()].into_iter().flatten()
        .map(str::trim).find(|s| !s.is_empty()).map(str::to_string)
}

/// Body of /api/get: one entry. Prefer time-synced lyrics over plain text.
pub fn parse_get(body: &str) -> Option<String> {
    best_text(&serde_json::from_str::<Entry>(body).ok()?)
}

/// Body of /api/search: a list. Keep entries whose length is within 3 s of the track (when known),
/// prefer one that has synced lyrics.
pub fn parse_search(body: &str, duration_secs: u32) -> Option<String> {
    let list: Vec<Entry> = serde_json::from_str(body).ok()?;
    let close = |e: &&Entry| duration_secs == 0 || e.duration.map_or(true, |d| (d - duration_secs as f64).abs() <= 3.0);
    let pool: Vec<&Entry> = list.iter().filter(close).collect();
    pool.iter().find(|e| e.synced.as_deref().map_or(false, |s| !s.trim().is_empty())).or(pool.first())
        .and_then(|e| best_text(e))
}

pub fn cache_key(title: &str, artist: &str, album: &str, duration_secs: u32) -> String {
    let id = format!("{}\0{}\0{}\0{}", artist.to_lowercase(), title.to_lowercase(), album.to_lowercase(), duration_secs);
    format!("{:016x}", crate::library::fnv1a(&id))
}

fn cache_dir() -> Option<PathBuf> {
    let base = std::env::var("XDG_CACHE_HOME").ok()
        .or_else(|| std::env::var("HOME").ok().map(|h| format!("{h}/.cache")))?;
    Some(PathBuf::from(base).join("rex-music").join("lyrics"))
}

pub enum Cached {
    Hit(String),
    NotFound, // asked before, the service had nothing
    Unknown,
}

pub fn cached(key: &str) -> Cached {
    let Some(dir) = cache_dir() else { return Cached::Unknown };
    if let Ok(t) = std::fs::read_to_string(dir.join(format!("{key}.lrc"))) { return Cached::Hit(t); }
    if dir.join(format!("{key}.none")).exists() { return Cached::NotFound; }
    Cached::Unknown
}

fn store(key: &str, text: Option<&str>) {
    let Some(dir) = cache_dir() else { return };
    if std::fs::create_dir_all(&dir).is_err() { return; }
    let _ = match text {
        Some(t) => std::fs::write(dir.join(format!("{key}.lrc")), t),
        None => std::fs::write(dir.join(format!("{key}.none")), b""),
    };
}

fn agent() -> ureq::Agent {
    // generic client name only: no contact details or user data in the request
    ureq::AgentBuilder::new().timeout(Duration::from_secs(8)).user_agent("rex-music").build()
}

/// Ask lrclib.net. `force` ignores the cache (including "not found"). Network errors are not cached.
pub fn fetch(title: &str, artist: &str, album: &str, duration_secs: u32, force: bool) -> Result<Option<String>, String> {
    let key = cache_key(title, artist, album, duration_secs);
    if !force {
        match cached(&key) {
            Cached::Hit(t) => return Ok(Some(t)),
            Cached::NotFound => return Ok(None),
            Cached::Unknown => {}
        }
    }
    // tags that were filled in by the scanner, not real metadata: nothing to look up
    if title.trim().is_empty() || artist.trim().is_empty() || artist == "Unknown Artist" {
        return Ok(None);
    }
    let net = |e: ureq::Error| format!("Could not reach lrclib.net: {e}");

    let mut found: Option<String> = None;
    let mut exact = agent().get(&format!("{API}/get")).query("artist_name", artist).query("track_name", title);
    if album != "Unknown Album" && !album.is_empty() { exact = exact.query("album_name", album); }
    if duration_secs > 0 { exact = exact.query("duration", &duration_secs.to_string()); }
    match exact.call() {
        Ok(r) => found = parse_get(&r.into_string().map_err(|e| e.to_string())?),
        Err(ureq::Error::Status(404, _)) => {}
        Err(e) => return Err(net(e)),
    }
    if found.is_none() { // fuzzier second try
        match agent().get(&format!("{API}/search")).query("artist_name", artist).query("track_name", title).call() {
            Ok(r) => found = parse_search(&r.into_string().map_err(|e| e.to_string())?, duration_secs),
            Err(ureq::Error::Status(404, _)) => {}
            Err(e) => return Err(net(e)),
        }
    }
    store(&key, found.as_deref());
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_prefers_synced_then_plain() {
        let both = r#"{"syncedLyrics":"[00:01.00] hi","plainLyrics":"hi"}"#;
        assert_eq!(parse_get(both).as_deref(), Some("[00:01.00] hi"));
        assert_eq!(parse_get(r#"{"syncedLyrics":null,"plainLyrics":"hi"}"#).as_deref(), Some("hi"));
        assert_eq!(parse_get(r#"{"syncedLyrics":"  ","plainLyrics":null}"#), None);
        assert_eq!(parse_get("not json"), None);
    }

    #[test]
    fn search_filters_by_length_and_prefers_synced() {
        let body = r#"[
            {"duration": 300.0, "syncedLyrics": "[00:01.00] wrong song", "plainLyrics": null},
            {"duration": 201.0, "syncedLyrics": null, "plainLyrics": "plain right"},
            {"duration": 199.5, "syncedLyrics": "[00:02.00] synced right", "plainLyrics": null}
        ]"#;
        assert_eq!(parse_search(body, 200).as_deref(), Some("[00:02.00] synced right"));
        assert_eq!(parse_search(body, 300).as_deref(), Some("[00:01.00] wrong song"));
        assert_eq!(parse_search(body, 10), None); // nothing within 3 s
        assert_eq!(parse_search("[]", 200), None);
    }

    #[test]
    fn unknown_length_accepts_any_entry() {
        let body = r#"[{"duration": 123.0, "syncedLyrics": null, "plainLyrics": "x"}]"#;
        assert_eq!(parse_search(body, 0).as_deref(), Some("x"));
    }

    #[test]
    fn cache_key_ignores_case_but_not_length() {
        assert_eq!(cache_key("Song", "ARTIST", "Album", 200), cache_key("song", "artist", "album", 200));
        assert_ne!(cache_key("Song", "Artist", "Album", 200), cache_key("Song", "Artist", "Album", 201));
    }

    #[test]
    fn placeholder_tags_are_never_looked_up() {
        // returns before any network call
        assert_eq!(fetch("Song", "Unknown Artist", "Album", 200, true), Ok(None));
        assert_eq!(fetch("", "Artist", "Album", 200, true), Ok(None));
    }
}

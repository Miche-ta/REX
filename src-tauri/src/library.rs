use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use serde::{Deserialize, Serialize};

use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, StandardTag};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackInfo {
    pub id: String,
    pub file_path: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration_seconds: f64,
    pub sample_rate: u32,
    pub bit_depth: u32,
    pub channels: u32,
    pub format: String,
}

// ---- on-disk library cache: a rescan only reads files whose size or modified time changed ----
#[derive(Serialize, Deserialize, Clone)]
struct CacheEntry {
    mtime: u64,
    size: u64,
    info: TrackInfo,
}

#[derive(Serialize, Deserialize, Default)]
struct DiskCache {
    version: u32,
    entries: HashMap<String, CacheEntry>,
}

const CACHE_VERSION: u32 = 1;

/// FNV-1a: a hash that stays the same across builds (std's DefaultHasher does not promise that).
pub(crate) fn fnv1a(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3))
}

fn cache_path(dir: &str) -> Option<PathBuf> {
    let base = std::env::var("XDG_CACHE_HOME").ok()
        .or_else(|| std::env::var("HOME").ok().map(|h| format!("{h}/.cache")))?;
    Some(PathBuf::from(base).join("rex-music").join(format!("library-{:016x}.json", fnv1a(dir))))
}

fn load_cache(dir: &str) -> HashMap<String, CacheEntry> {
    let cache: Option<DiskCache> = cache_path(dir)
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok());
    match cache {
        Some(c) if c.version == CACHE_VERSION => c.entries,
        _ => HashMap::new(),
    }
}

fn save_cache(dir: &str, entries: HashMap<String, CacheEntry>) {
    let Some(p) = cache_path(dir) else { return };
    let Some(parent) = p.parent() else { return };
    if fs::create_dir_all(parent).is_err() { return; }
    let tmp = p.with_extension("json.tmp");
    if let Ok(text) = serde_json::to_string(&DiskCache { version: CACHE_VERSION, entries }) {
        if fs::write(&tmp, text).is_ok() { let _ = fs::rename(&tmp, &p); } // atomic swap
    }
}

fn is_fresh(e: &CacheEntry, mtime: u64, size: u64) -> bool {
    e.mtime == mtime && e.size == size
}

fn sort_tracks(tracks: &mut [TrackInfo]) {
    tracks.sort_by(|a, b| a.artist.cmp(&b.artist).then(a.album.cmp(&b.album)).then(a.title.cmp(&b.title)));
}

/// What the last scan of this folder found, without touching the music files: instant at startup.
pub fn cached_tracks(dir_path: &str) -> Vec<TrackInfo> {
    let mut tracks: Vec<TrackInfo> = load_cache(dir_path).into_values().map(|e| e.info).collect();
    sort_tracks(&mut tracks);
    tracks
}

pub fn scan_directory(dir_path: &str) -> Vec<TrackInfo> {
    let mut tracks = Vec::new();
    let root = Path::new(dir_path);
    if !root.exists() || !root.is_dir() {
        return tracks;
    }

    let old = load_cache(dir_path);
    let mut fresh: HashMap<String, CacheEntry> = HashMap::new();
    let mut dirs_to_visit = vec![root.to_path_buf()];

    while let Some(current_dir) = dirs_to_visit.pop() {
        if let Ok(entries) = fs::read_dir(&current_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    dirs_to_visit.push(path);
                } else if path.is_file() {
                    if let Some(ext) = path.extension().and_then(|e| e.to_str()).map(|s| s.to_lowercase()) {
                        if matches!(ext.as_str(), "flac" | "wav" | "ogg" | "mp3" | "m4a" | "aac" | "alac") {
                            let key = path.to_string_lossy().to_string();
                            let (mtime, size) = fs::metadata(&path).ok().map(|m| (
                                m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs()),
                                m.len(),
                            )).unwrap_or((0, 0));
                            let info = match old.get(&key) {
                                Some(e) if is_fresh(e, mtime, size) => Some(e.info.clone()),
                                _ => extract_track_info(&path, &ext),
                            };
                            if let Some(info) = info {
                                fresh.insert(key, CacheEntry { mtime, size, info: info.clone() });
                                tracks.push(info);
                            }
                        }
                    }
                }
            }
        }
    }

    save_cache(dir_path, fresh); // files that vanished simply are not carried over
    sort_tracks(&mut tracks);
    tracks
}

fn extract_track_info(path: &Path, ext: &str) -> Option<TrackInfo> {
    let file = fs::File::open(path).ok()?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    hint.with_extension(ext);

    let fmt_opts = FormatOptions::default();
    let meta_opts = MetadataOptions::default();

    let mut format = symphonia::default::get_probe()
        .probe(&hint, mss, fmt_opts, meta_opts)
        .ok()?;

    let track = format.default_track(TrackType::Audio)?.clone();
    let codec_params = track.codec_params.as_ref()?;
    let audio_params = codec_params.audio()?;

    let sample_rate = audio_params.sample_rate.unwrap_or(44100);
    let channels = audio_params.channels.as_ref().map(|c| c.count() as u32).unwrap_or(2);
    let bit_depth = audio_params.bits_per_sample.unwrap_or(16);
    let duration_seconds = track
        .num_frames
        .map(|f| f as f64 / sample_rate as f64)
        .unwrap_or(0.0);

    let mut title = path.file_stem().and_then(|s| s.to_str()).unwrap_or("Unknown").to_string();
    let mut artist = "Unknown Artist".to_string();
    let mut album = "Unknown Album".to_string();

    if let Some(meta) = format.metadata().current() {
        for tag in &meta.media.tags {
            if let Some(ref std_tag) = tag.std {
                match std_tag {
                    StandardTag::TrackTitle(t) => title = t.to_string(),
                    StandardTag::Artist(a) => artist = a.to_string(),
                    StandardTag::Album(al) => album = al.to_string(),
                    _ => {}
                }
            }
        }
    }

    Some(TrackInfo {
        id: path.to_string_lossy().to_string(),
        file_path: path.to_string_lossy().to_string(),
        title,
        artist,
        album,
        duration_seconds,
        sample_rate,
        bit_depth,
        channels,
        format: ext.to_uppercase(),
    })
}

/// Cover art as a data URL: embedded picture first, then cover/folder/front image beside the file.
/// Cover image bytes and mime type: embedded picture first, else cover/folder/front/album image beside the file.
pub fn cover_bytes(file_path: &str) -> Option<(Vec<u8>, String)> {
    let path = Path::new(file_path);
    let mime_of = |p: &Path| match p.extension().and_then(|e| e.to_str()).map(|e| e.to_lowercase()).as_deref() {
        Some("png") => "image/png",
        _ => "image/jpeg",
    };

    let file = fs::File::open(path).ok()?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) { hint.with_extension(ext); }
    if let Ok(mut format) = symphonia::default::get_probe().probe(&hint, mss, FormatOptions::default(), MetadataOptions::default()) {
        if let Some(rev) = format.metadata().current() {
            if let Some(v) = rev.media.visuals.first() {
                let mime = v.media_type.clone().unwrap_or_else(|| "image/jpeg".into());
                return Some((v.data.to_vec(), mime));
            }
        }
    }

    let dir = path.parent()?;
    for name in ["cover", "folder", "front", "album"] {
        for ext in ["jpg", "jpeg", "png"] {
            let img = dir.join(format!("{name}.{ext}"));
            if let Ok(bytes) = fs::read(&img) {
                return Some((bytes, mime_of(&img).to_string()));
            }
        }
    }
    None
}

pub fn get_cover(file_path: &str) -> Option<String> {
    use base64::{engine::general_purpose::STANDARD, Engine};
    let (bytes, mime) = cover_bytes(file_path)?;
    Some(format!("data:{};base64,{}", mime, STANDARD.encode(bytes)))
}

pub fn get_lyrics(file_path: &str) -> Option<String> {
    let path = Path::new(file_path);
    // 1. Same directory, same file stem with .lrc extension
    let lrc_path = path.with_extension("lrc");
    if let Ok(content) = fs::read_to_string(&lrc_path) {
        return Some(content);
    }
    // 2. Also check lyrics.lrc in the parent folder
    if let Some(dir) = path.parent() {
        let folder_lrc = dir.join("lyrics.lrc");
        if let Ok(content) = fs::read_to_string(&folder_lrc) {
            return Some(content);
        }
    }
    // 3. Check embedded metadata tags
    let file = fs::File::open(path).ok()?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) { hint.with_extension(ext); }
    if let Ok(mut format) = symphonia::default::get_probe().probe(&hint, mss, FormatOptions::default(), MetadataOptions::default()) {
        if let Some(rev) = format.metadata().current() {
            for tag in &rev.media.tags {
                let key = tag.raw.key.to_lowercase();
                if key == "lyrics" || key == "unsyncedlyrics" || key == "unsynced lyrics" || key.starts_with("uslt") {
                    return Some(tag.raw.value.to_string());
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(p: &str) -> TrackInfo {
        TrackInfo { id: p.into(), file_path: p.into(), title: "t".into(), artist: "a".into(), album: "b".into(),
            duration_seconds: 1.0, sample_rate: 44100, bit_depth: 16, channels: 2, format: "FLAC".into() }
    }

    #[test]
    fn unchanged_file_is_fresh_changed_file_is_not() {
        let e = CacheEntry { mtime: 100, size: 5000, info: info("/m/a.flac") };
        assert!(is_fresh(&e, 100, 5000));
        assert!(!is_fresh(&e, 101, 5000)); // edited
        assert!(!is_fresh(&e, 100, 5001)); // replaced
    }

    #[test]
    fn folder_hash_is_stable_and_distinct() {
        assert_eq!(fnv1a("/home/x/Music"), fnv1a("/home/x/Music"));
        assert_ne!(fnv1a("/home/x/Music"), fnv1a("/home/x/Music2"));
        assert_eq!(fnv1a(""), 0xcbf2_9ce4_8422_2325); // FNV-1a offset basis
    }

    #[test]
    fn cache_roundtrips_through_json() {
        let mut entries = HashMap::new();
        entries.insert("/m/a.flac".to_string(), CacheEntry { mtime: 1, size: 2, info: info("/m/a.flac") });
        let text = serde_json::to_string(&DiskCache { version: CACHE_VERSION, entries }).unwrap();
        let back: DiskCache = serde_json::from_str(&text).unwrap();
        assert_eq!(back.entries["/m/a.flac"].info.file_path, "/m/a.flac");
    }

    #[test]
    fn tracks_sort_by_artist_album_title() {
        let mut v = vec![info("/m/z"), info("/m/y")];
        v[0].artist = "Zed".into();
        v[1].artist = "Abe".into();
        sort_tracks(&mut v);
        assert_eq!(v[0].artist, "Abe");
    }
}

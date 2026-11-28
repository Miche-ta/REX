use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use rusqlite::Connection;
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
    pub disc_no: u32,  // 0 = no tag
    pub track_no: u32, // 0 = no tag
}

// ---- library database (SQLite): a rescan only reads files whose size or modified time changed ----
// One file, library.db in the cache dir. `tracks` holds every scanned folder's list (CUE virtual tracks
// included, so they show at startup too); `tracks_fts` is the full-text index the search box queries.
#[derive(Clone)]
struct CacheEntry {
    mtime: u64,
    size: u64,
    info: TrackInfo,
}

const SCHEMA_VERSION: i32 = 2; // 2: disc_no, track_no

/// FNV-1a: a hash that stays the same across builds (std's DefaultHasher does not promise that).
pub(crate) fn fnv1a(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3))
}

fn db_path() -> Option<PathBuf> {
    crate::paths::cache_dir().map(|d| d.join("library.db"))
}

fn open_db(path: &Path) -> rusqlite::Result<Connection> {
    if let Some(dir) = path.parent() { let _ = fs::create_dir_all(dir); }
    let conn = Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.pragma_update(None, "journal_mode", "WAL")?; // searches can read while a scan writes
    let version: i32 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version != SCHEMA_VERSION {
        // it is only a cache: an old layout is dropped and the next scan refills it
        conn.execute_batch(&format!("
            DROP TABLE IF EXISTS tracks;
            DROP TABLE IF EXISTS tracks_fts;
            CREATE TABLE tracks (
                root TEXT NOT NULL, file_path TEXT NOT NULL, mtime INTEGER NOT NULL, size INTEGER NOT NULL,
                title TEXT NOT NULL, artist TEXT NOT NULL, album TEXT NOT NULL, duration REAL NOT NULL,
                sample_rate INTEGER NOT NULL, bit_depth INTEGER NOT NULL, channels INTEGER NOT NULL, format TEXT NOT NULL,
                disc_no INTEGER NOT NULL, track_no INTEGER NOT NULL,
                PRIMARY KEY (root, file_path));
            CREATE VIRTUAL TABLE tracks_fts USING fts5(
                root UNINDEXED, file_path UNINDEXED, title, artist, album,
                tokenize = 'unicode61 remove_diacritics 2');
            PRAGMA user_version = {SCHEMA_VERSION};"))?;
        // the JSON caches this database replaced (library-<hash>.json)
        for e in path.parent().and_then(|d| fs::read_dir(d).ok()).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with("library-") && name.ends_with(".json") { let _ = fs::remove_file(e.path()); }
        }
    }
    Ok(conn)
}

fn load_entries(conn: &Connection, root: &str) -> rusqlite::Result<Vec<CacheEntry>> {
    let mut st = conn.prepare("SELECT file_path, mtime, size, title, artist, album, duration, sample_rate, bit_depth,
        channels, format, disc_no, track_no FROM tracks WHERE root = ?1")?;
    let rows = st.query_map([root], |r| {
        let file_path: String = r.get(0)?;
        Ok(CacheEntry {
            mtime: r.get::<_, i64>(1)? as u64, // SQLite integers are i64; mtimes and sizes fit
            size: r.get::<_, i64>(2)? as u64,
            info: TrackInfo {
                id: file_path.clone(), file_path, title: r.get(3)?, artist: r.get(4)?, album: r.get(5)?,
                duration_seconds: r.get(6)?, sample_rate: r.get(7)?, bit_depth: r.get(8)?, channels: r.get(9)?,
                format: r.get(10)?, disc_no: r.get(11)?, track_no: r.get(12)?,
            },
        })
    })?;
    rows.collect()
}

/// Replace one folder's tracks in one transaction: a crash mid-write leaves the previous scan intact.
fn store_entries(conn: &mut Connection, root: &str, entries: &[CacheEntry]) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM tracks WHERE root = ?1", [root])?;
    tx.execute("DELETE FROM tracks_fts WHERE root = ?1", [root])?;
    {
        let mut ins = tx.prepare("INSERT OR REPLACE INTO tracks VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)")?;
        let mut fts = tx.prepare("INSERT INTO tracks_fts VALUES (?1, ?2, ?3, ?4, ?5)")?;
        for e in entries {
            let t = &e.info;
            ins.execute(rusqlite::params![root, t.file_path, e.mtime as i64, e.size as i64, t.title, t.artist, t.album,
                t.duration_seconds, t.sample_rate, t.bit_depth, t.channels, t.format, t.disc_no, t.track_no])?;
            fts.execute(rusqlite::params![root, t.file_path, t.title, t.artist, t.album])?;
        }
    }
    tx.commit()
}

/// Every word must match the start of a word in title, artist or album, in any order, ignoring accents.
/// Returns the matching file paths.
fn search_db(conn: &Connection, root: &str, query: &str) -> rusqlite::Result<Vec<String>> {
    let fts_query = query.split_whitespace()
        .map(|w| format!("\"{}\"*", w.replace('"', "\"\"")))
        .collect::<Vec<_>>().join(" ");
    if fts_query.is_empty() { return Ok(Vec::new()); }
    let mut st = conn.prepare("SELECT file_path FROM tracks_fts WHERE tracks_fts MATCH ?1 AND root = ?2")?;
    let rows = st.query_map([fts_query.as_str(), root], |r| r.get(0))?;
    rows.collect()
}

fn with_db<T>(f: impl FnOnce(&mut Connection) -> rusqlite::Result<T>) -> Option<T> {
    let mut conn = open_db(&db_path()?).ok()?;
    f(&mut conn).ok()
}

fn load_cache(root: &str) -> HashMap<String, CacheEntry> {
    with_db(|c| load_entries(c, root)).unwrap_or_default()
        .into_iter().map(|e| (e.info.file_path.clone(), e)).collect()
}

pub fn search(root: &str, query: &str) -> Option<Vec<String>> {
    with_db(|c| search_db(c, root, query))
}

fn is_fresh(e: &CacheEntry, mtime: u64, size: u64) -> bool {
    e.mtime == mtime && e.size == size
}

fn sort_tracks(tracks: &mut [TrackInfo]) {
    tracks.sort_by(|a, b| a.artist.cmp(&b.artist).then(a.album.cmp(&b.album))
        .then(a.disc_no.cmp(&b.disc_no)).then(a.track_no.cmp(&b.track_no)).then(a.title.cmp(&b.title)));
}

/// What the last scan of this folder found, without touching the music files: instant at startup.
pub fn cached_tracks(dir_path: &str) -> Vec<TrackInfo> {
    let mut tracks: Vec<TrackInfo> = load_cache(dir_path).into_values().map(|e| e.info).collect();
    sort_tracks(&mut tracks);
    tracks
}

/// `progress(done, total)` runs after every file (total = 0 while still listing folders);
/// returning false cancels the scan, which then returns None and leaves the cache untouched.
pub fn scan_directory(dir_path: &str, progress: &mut dyn FnMut(usize, usize) -> bool) -> Option<Vec<TrackInfo>> {
    let mut tracks = Vec::new();
    let root = Path::new(dir_path);
    if !root.exists() || !root.is_dir() {
        return Some(tracks);
    }

    let old = load_cache(dir_path);
    let mut fresh: Vec<CacheEntry> = Vec::new();
    let mut dirs_to_visit = vec![root.to_path_buf()];
    let mut cue_files: Vec<PathBuf> = Vec::new();
    let mut audio_files: Vec<(PathBuf, String)> = Vec::new();

    while let Some(current_dir) = dirs_to_visit.pop() {
        if !progress(0, 0) { return None; }
        if let Ok(entries) = fs::read_dir(&current_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    dirs_to_visit.push(path);
                } else if path.is_file() {
                    if let Some(ext) = path.extension().and_then(|e| e.to_str()).map(|s| s.to_lowercase()) {
                        if ext == "cue" {
                            cue_files.push(path);
                        } else if matches!(ext.as_str(), "flac" | "wav" | "ogg" | "mp3" | "m4a" | "aac" | "alac" | "dsf" | "dff") {
                            audio_files.push((path, ext));
                        }
                    }
                }
            }
        }
    }

    let total = cue_files.len() + audio_files.len();
    let mut done = 0;

    // Parse CUE sheets first so we can suppress duplicate monolithic album files.
    let mut cue_referenced: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    for cue_path in &cue_files {
        done += 1;
        if !progress(done, total) { return None; }
        if let Some(cue_tracks) = crate::cue::parse_cue_file(cue_path) {
            let mut probed: HashMap<PathBuf, Option<TrackInfo>> = HashMap::new();
            for ct in cue_tracks {
                if !ct.file_path.exists() { continue; }
                cue_referenced.insert(ct.file_path.clone());
                let base_opt = probed.entry(ct.file_path.clone()).or_insert_with(|| {
                    let ext = ct.file_path.extension().and_then(|e| e.to_str()).unwrap_or("flac").to_lowercase();
                    extract_track_info(&ct.file_path, &ext)
                });
                let Some(base) = base_opt else { continue };
                let dur = ct.end_seconds
                    .map(|e| (e - ct.start_seconds).max(0.0))
                    .unwrap_or_else(|| (base.duration_seconds - ct.start_seconds).max(0.0));
                let vp = crate::cue::format_virtual_path(&ct.file_path, ct.start_seconds, ct.end_seconds);
                let info = TrackInfo {
                    id: vp.clone(),
                    file_path: vp,
                    title: ct.title,
                    artist: ct.artist,
                    album: ct.album,
                    duration_seconds: dur,
                    sample_rate: base.sample_rate,
                    bit_depth: base.bit_depth,
                    channels: base.channels,
                    format: format!("{} (CUE)", base.format),
                    disc_no: 0,
                    track_no: ct.track_num,
                };
                fresh.push(CacheEntry { mtime: 0, size: 0, info: info.clone() }); // CUE tracks are re-parsed every scan
                tracks.push(info);
            }
        }
    }

    for (path, ext) in audio_files {
        done += 1;
        if !progress(done, total) { return None; }
        if cue_referenced.contains(&path) {
            continue;
        }
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
            fresh.push(CacheEntry { mtime, size, info: info.clone() });
            tracks.push(info);
        }
    }

    with_db(|c| store_entries(c, dir_path, &fresh)); // files that vanished simply are not carried over
    sort_tracks(&mut tracks);
    Some(tracks)
}

fn extract_track_info(path: &Path, ext: &str) -> Option<TrackInfo> {
    if crate::dsd::is_dsd_path(path) {
        let dsd = crate::dsd::probe_dsd(path)?;
        let title = path.file_stem().and_then(|s| s.to_str()).unwrap_or("Unknown").to_string();
        return Some(TrackInfo {
            id: path.to_string_lossy().to_string(),
            file_path: path.to_string_lossy().to_string(),
            title,
            artist: "Unknown Artist".to_string(),
            album: path.parent().and_then(|p| p.file_name()).and_then(|s| s.to_str()).unwrap_or("DSD Audio").to_string(),
            duration_seconds: dsd.duration_seconds,
            sample_rate: dsd.dsd_rate,
            bit_depth: 1,
            channels: dsd.channels,
            format: dsd.dsd_label,
            disc_no: 0,
            track_no: 0,
        });
    }
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
    let (mut disc_no, mut track_no) = (0u32, 0u32);

    if let Some(meta) = format.metadata().current() {
        for tag in &meta.media.tags {
            if let Some(ref std_tag) = tag.std {
                match std_tag {
                    StandardTag::TrackTitle(t) => title = t.to_string(),
                    StandardTag::Artist(a) => artist = a.to_string(),
                    StandardTag::Album(al) => album = al.to_string(),
                    StandardTag::DiscNumber(n) => disc_no = *n as u32,
                    StandardTag::TrackNumber(n) => track_no = *n as u32,
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
        disc_no,
        track_no,
    })
}

/// Cover image bytes and mime type: embedded picture first, else cover/folder/front/album image beside the file.
pub fn cover_bytes(file_path: &str) -> Option<(Vec<u8>, String)> {
    let (real_path, _) = crate::cue::parse_virtual_path(file_path);
    let path = Path::new(real_path);
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

pub fn get_lyrics(file_path: &str) -> Option<String> {
    let (real_path, _) = crate::cue::parse_virtual_path(file_path);
    let path = Path::new(real_path);
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
            duration_seconds: 1.0, sample_rate: 44100, bit_depth: 16, channels: 2, format: "FLAC".into(),
            disc_no: 0, track_no: 0 }
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

    fn temp_db(name: &str) -> (PathBuf, Connection) {
        let p = std::env::temp_dir().join(format!("rex-{name}-{}.db", std::process::id()));
        let _ = fs::remove_file(&p);
        let c = open_db(&p).unwrap();
        (p, c)
    }

    #[test]
    fn db_roundtrips_and_replaces_one_folder_only() {
        let (p, mut c) = temp_db("roundtrip");
        let e = |path: &str| CacheEntry { mtime: 1, size: 2, info: info(path) };
        store_entries(&mut c, "/m", &[e("/m/a.flac"), e("/m/b.flac")]).unwrap();
        store_entries(&mut c, "/n", &[e("/n/c.flac")]).unwrap();
        store_entries(&mut c, "/m", &[e("/m/a.flac")]).unwrap(); // rescan: b.flac vanished
        let m = load_entries(&c, "/m").unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!((m[0].info.file_path.as_str(), m[0].mtime, m[0].size), ("/m/a.flac", 1, 2));
        assert_eq!(load_entries(&c, "/n").unwrap().len(), 1, "other folder untouched");
        drop(c);
        assert_eq!(load_entries(&open_db(&p).unwrap(), "/m").unwrap().len(), 1, "survives reopening");
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn search_matches_word_prefixes_in_any_order_ignoring_accents() {
        let (p, mut c) = temp_db("search");
        let mut a = info("/m/a.flac");
        (a.title, a.artist, a.album) = ("Jóga".into(), "Björk".into(), "Homogenic".into());
        let mut b = info("/m/b.flac");
        (b.title, b.artist, b.album) = ("Come Together".into(), "The Beatles".into(), "Abbey Road".into());
        let e = |t: TrackInfo| CacheEntry { mtime: 0, size: 0, info: t };
        store_entries(&mut c, "/m", &[e(a), e(b)]).unwrap();
        let q = |s: &str| search_db(&c, "/m", s).unwrap();
        assert_eq!(q("bjork"), vec!["/m/a.flac"], "accents ignored");
        assert_eq!(q("beat abbey"), vec!["/m/b.flac"], "prefixes, any order, across columns");
        assert!(q("beatles homogenic").is_empty(), "every word must match");
        assert!(q("\"; DROP").is_empty(), "quotes cannot break the query");
        assert!(search_db(&c, "/other", "bjork").unwrap().is_empty(), "scoped to the folder");
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn tracks_sort_by_artist_album_title() {
        let mut v = vec![info("/m/z"), info("/m/y")];
        v[0].artist = "Zed".into();
        v[1].artist = "Abe".into();
        sort_tracks(&mut v);
        assert_eq!(v[0].artist, "Abe");
    }

    #[test]
    fn album_tracks_sort_by_disc_then_track_number_not_title() {
        let mut v: Vec<TrackInfo> = [(2, 1, "A"), (1, 2, "B"), (1, 10, "C"), (1, 1, "Z")].iter().map(|&(d, n, t)| {
            let mut x = info(t);
            (x.disc_no, x.track_no, x.title) = (d, n, t.into());
            x
        }).collect();
        sort_tracks(&mut v);
        assert_eq!(v.iter().map(|t| t.title.as_str()).collect::<String>(), "ZBCA");
    }

    #[test]
    fn scan_reports_progress_and_can_be_cancelled() {
        let dir = std::env::temp_dir().join(format!("rex-scan-test-{}", std::process::id()));
        fs::create_dir_all(dir.join("sub")).unwrap();
        for f in ["a.flac", "b.mp3", "sub/c.wav", "notes.txt"] { fs::write(dir.join(f), b"not audio").unwrap(); }
        let d = dir.to_str().unwrap();

        let mut seen = Vec::new();
        let full = scan_directory(d, &mut |done, total| { seen.push((done, total)); true });
        assert!(full.is_some());
        assert_eq!(seen.last(), Some(&(3, 3)), "3 audio files, the .txt is skipped");

        let mut calls = 0;
        assert!(scan_directory(d, &mut |done, _| { calls += 1; done < 2 }).is_none(), "cancel returns None");
        assert!(calls < seen.len(), "stops at the cancel, not after the whole folder");
        let _ = fs::remove_dir_all(&dir);
    }
}

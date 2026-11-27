//! CUE sheet parser and virtual-track path helpers for single-file album rips.
//!
//! Virtual track paths use the form `<audio_file>#cue:<start_sec>-<end_sec>` (or
//! `<audio_file>#cue:<start_sec>-` when it runs to the end of the file).

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub struct CueTrack {
    pub track_num: u32,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub file_path: PathBuf,
    pub start_seconds: f64,
    pub end_seconds: Option<f64>,
}

/// Split a possibly-virtual CUE track path into `(real_file_path, Option<(start_sec, end_sec)>)`.
pub fn parse_virtual_path(path: &str) -> (&str, Option<(f64, Option<f64>)>) {
    let Some((base, frag)) = path.split_once("#cue:") else {
        return (path, None);
    };
    let Some((s, e)) = frag.split_once('-') else {
        return (path, None);
    };
    let Ok(start) = s.parse::<f64>() else {
        return (path, None);
    };
    let end = if e.is_empty() { None } else { e.parse::<f64>().ok() };
    (base, Some((start, end)))
}

/// Format a virtual CUE track path.
pub fn format_virtual_path(file_path: &Path, start: f64, end: Option<f64>) -> String {
    match end {
        Some(e) => format!("{}#cue:{start:.6}-{e:.6}", file_path.to_string_lossy()),
        None => format!("{}#cue:{start:.6}-", file_path.to_string_lossy()),
    }
}

/// Parse CUE timestamp `MM:SS:FF` (75 frames per second) into seconds.
pub fn parse_cue_time(ts: &str) -> Option<f64> {
    let mut parts = ts.trim().split(':');
    let mins: f64 = parts.next()?.parse().ok()?;
    let secs: f64 = parts.next()?.parse().ok()?;
    let frames: f64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(mins * 60.0 + secs + frames / 75.0)
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

fn parse_quoted_arg(rest: &str) -> String {
    let trimmed = rest.trim();
    if let Some(after_open) = trimmed.strip_prefix('"') {
        if let Some(end) = after_open.rfind('"') {
            return after_open[..end].to_string();
        }
    }
    unquote(trimmed)
}

/// Parse a `.cue` file located at `cue_path`, resolving referenced audio files relative to `cue_path`'s directory.
pub fn parse_cue_file(cue_path: &Path) -> Option<Vec<CueTrack>> {
    let content = std::fs::read_to_string(cue_path).ok()?;
    let dir = cue_path.parent()?;
    Some(parse_cue_str(&content, dir))
}

pub fn parse_cue_str(content: &str, base_dir: &Path) -> Vec<CueTrack> {
    #[derive(Clone)]
    struct RawTrack {
        num: u32,
        title: Option<String>,
        artist: Option<String>,
        file: PathBuf,
        index01: Option<f64>,
    }

    let mut album_title = base_dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("Unknown Album")
        .to_string();
    let mut album_artist = "Unknown Artist".to_string();
    let mut current_file: Option<PathBuf> = None;
    let mut raw_tracks: Vec<RawTrack> = Vec::new();
    let mut current_track: Option<RawTrack> = None;

    for raw_line in content.lines() {
        let line = raw_line.trim().trim_start_matches('\u{feff}');
        if line.is_empty() || line.starts_with("REM ") {
            continue;
        }
        let (kw, rest) = match line.split_once(char::is_whitespace) {
            Some((k, r)) => (k.to_ascii_uppercase(), r.trim()),
            None => continue,
        };
        match kw.as_str() {
            "TITLE" => {
                let val = unquote(rest);
                if let Some(ref mut t) = current_track {
                    t.title = Some(val);
                } else {
                    album_title = val;
                }
            }
            "PERFORMER" => {
                let val = unquote(rest);
                if let Some(ref mut t) = current_track {
                    t.artist = Some(val);
                } else {
                    album_artist = val;
                }
            }
            "FILE" => {
                if let Some(t) = current_track.take() {
                    raw_tracks.push(t);
                }
                let fname = parse_quoted_arg(rest);
                current_file = Some(base_dir.join(fname));
            }
            "TRACK" => {
                if let Some(t) = current_track.take() {
                    raw_tracks.push(t);
                }
                let num = rest
                    .split_whitespace()
                    .next()
                    .and_then(|n| n.parse::<u32>().ok())
                    .unwrap_or((raw_tracks.len() + 1) as u32);
                if let Some(ref file) = current_file {
                    current_track = Some(RawTrack {
                        num,
                        title: None,
                        artist: None,
                        file: file.clone(),
                        index01: None,
                    });
                }
            }
            "INDEX" => {
                let mut parts = rest.split_whitespace();
                let idx_num = parts.next();
                let ts = parts.next();
                if idx_num == Some("01") {
                    if let (Some(t), Some(ts)) = (current_track.as_mut(), ts) {
                        t.index01 = parse_cue_time(ts);
                    }
                }
            }
            _ => {}
        }
    }
    if let Some(t) = current_track.take() {
        raw_tracks.push(t);
    }

    let valid: Vec<RawTrack> = raw_tracks.into_iter().filter(|t| t.index01.is_some()).collect();
    let mut out = Vec::with_capacity(valid.len());
    for i in 0..valid.len() {
        let cur = &valid[i];
        let start = cur.index01.unwrap_or(0.0);
        let end = if i + 1 < valid.len() && valid[i + 1].file == cur.file {
            valid[i + 1].index01
        } else {
            None
        };
        out.push(CueTrack {
            track_num: cur.num,
            title: cur.title.clone().unwrap_or_else(|| format!("Track {:02}", cur.num)),
            artist: cur.artist.clone().unwrap_or_else(|| album_artist.clone()),
            album: album_title.clone(),
            file_path: cur.file.clone(),
            start_seconds: start,
            end_seconds: end,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cue_timestamps_with_75fps_frames() {
        assert!((parse_cue_time("00:00:00").unwrap() - 0.0).abs() < 1e-6);
        assert!((parse_cue_time("03:45:37").unwrap() - (225.0 + 37.0 / 75.0)).abs() < 1e-6);
        assert!(parse_cue_time("bad").is_none());
    }

    #[test]
    fn parses_single_file_album_cue_with_accurate_boundaries() {
        let cue = r#"
REM GENRE Jazz
PERFORMER "Miles Davis"
TITLE "Kind of Blue"
FILE "Kind of Blue.flac" WAVE
  TRACK 01 AUDIO
    TITLE "So What"
    INDEX 01 00:00:00
  TRACK 02 AUDIO
    TITLE "Freddie Freeloader"
    PERFORMER "Miles Davis Quartet"
    INDEX 00 09:20:00
    INDEX 01 09:22:37
  TRACK 03 AUDIO
    TITLE "Blue in Green"
    INDEX 01 19:08:00
"#;
        let tracks = parse_cue_str(cue, Path::new("/music/kob"));
        assert_eq!(tracks.len(), 3);
        assert_eq!(tracks[0].title, "So What");
        assert_eq!(tracks[0].artist, "Miles Davis");
        assert_eq!(tracks[0].album, "Kind of Blue");
        assert_eq!(tracks[0].start_seconds, 0.0);
        assert!((tracks[0].end_seconds.unwrap() - (9.0 * 60.0 + 22.0 + 37.0 / 75.0)).abs() < 1e-6);

        assert_eq!(tracks[1].artist, "Miles Davis Quartet");
        assert!((tracks[1].start_seconds - tracks[0].end_seconds.unwrap()).abs() < 1e-6);
        assert!((tracks[1].end_seconds.unwrap() - (19.0 * 60.0 + 8.0)).abs() < 1e-6);

        assert!(tracks[2].end_seconds.is_none());
    }

    #[test]
    fn virtual_path_roundtrips_cleanly() {
        let vp = format_virtual_path(Path::new("/music/album.flac"), 12.5, Some(240.25));
        let (real, span) = parse_virtual_path(&vp);
        assert_eq!(real, "/music/album.flac");
        let (s, e) = span.unwrap();
        assert!((s - 12.5).abs() < 1e-5);
        assert!((e.unwrap() - 240.25).abs() < 1e-5);

        assert_eq!(parse_virtual_path("/music/plain.flac"), ("/music/plain.flac", None));
    }
}

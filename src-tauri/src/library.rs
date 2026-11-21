use std::fs;
use std::path::Path;
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

pub fn scan_directory(dir_path: &str) -> Vec<TrackInfo> {
    let mut tracks = Vec::new();
    let root = Path::new(dir_path);
    if !root.exists() || !root.is_dir() {
        return tracks;
    }

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
                            if let Some(track) = extract_track_info(&path, &ext) {
                                tracks.push(track);
                            }
                        }
                    }
                }
            }
        }
    }

    tracks.sort_by(|a, b| a.artist.cmp(&b.artist).then(a.album.cmp(&b.album)).then(a.title.cmp(&b.title)));
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
pub fn get_cover(file_path: &str) -> Option<String> {
    use base64::{engine::general_purpose::STANDARD, Engine};
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
                return Some(format!("data:{};base64,{}", mime, STANDARD.encode(&v.data)));
            }
        }
    }

    let dir = path.parent()?;
    for name in ["cover", "folder", "front", "album"] {
        for ext in ["jpg", "jpeg", "png"] {
            let img = dir.join(format!("{name}.{ext}"));
            if let Ok(bytes) = fs::read(&img) {
                return Some(format!("data:{};base64,{}", mime_of(&img), STANDARD.encode(bytes)));
            }
        }
    }
    None
}

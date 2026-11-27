//! DSD container decoding (.dsf and .dff) and DoP v1.1 (DSD over PCM) 24-bit packing.
//!
//! DoP v1.1 framing:
//! - DSD64  (2,822,400 Hz)  -> 176,400 Hz 24-bit PCM
//! - DSD128 (5,644,800 Hz)  -> 352,800 Hz 24-bit PCM
//! - DSD256 (11,289,600 Hz) -> 705,600 Hz 24-bit PCM
//! - Alternating 8-bit marker `0x05` / `0xFA` in the top byte of each 24-bit sample,
//!   followed by two MSB-first DSD bytes: `((marker) << 24) | ((b0) << 16) | ((b1) << 8)`.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DsdKind {
    Dsf,
    Dff,
}

#[derive(Debug, Clone)]
pub struct DsdInfo {
    pub kind: DsdKind,
    pub dsd_rate: u32,
    pub dop_rate: u32,
    pub channels: u32,
    pub lsb_first: bool,
    pub data_offset: u64,
    pub data_bytes: u64,
    pub block_size: usize, // DSF block size per channel (usually 4096); 1 for byte-interleaved DFF
    pub duration_seconds: f64,
    pub dsd_label: String, // e.g. "DSD64", "DSD128"
}

pub fn is_dsd_path(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()).map(|s| s.to_ascii_lowercase()).as_deref(),
        Some("dsf" | "dff")
    )
}

pub fn dsd_label(dsd_rate: u32) -> String {
    let mult = dsd_rate / 44_100;
    if mult >= 64 {
        format!("DSD{mult}")
    } else {
        format!("DSD {}Hz", dsd_rate)
    }
}

fn read_u32_be(r: &mut impl Read) -> Option<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b).ok()?;
    Some(u32::from_be_bytes(b))
}

fn read_u64_be(r: &mut impl Read) -> Option<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b).ok()?;
    Some(u64::from_be_bytes(b))
}

fn read_u16_be(r: &mut impl Read) -> Option<u16> {
    let mut b = [0u8; 2];
    r.read_exact(&mut b).ok()?;
    Some(u16::from_be_bytes(b))
}

/// Probe a `.dsf` or `.dff` file header without decoding the audio stream.
pub fn probe_dsd(path: &Path) -> Option<DsdInfo> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    let mut f = File::open(path).ok()?;
    match ext.as_str() {
        "dsf" => probe_dsf(&mut f),
        "dff" => probe_dff(&mut f),
        _ => None,
    }
}

fn probe_dsf(f: &mut File) -> Option<DsdInfo> {
    let mut hdr = [0u8; 28];
    f.read_exact(&mut hdr).ok()?;
    if &hdr[0..4] != b"DSD " {
        return None;
    }
    let dsd_chunk_size = u64::from_le_bytes(hdr[4..12].try_into().ok()?);
    if dsd_chunk_size < 28 {
        return None;
    }
    f.seek(SeekFrom::Start(dsd_chunk_size)).ok()?;

    let mut fmt_hdr = [0u8; 52];
    f.read_exact(&mut fmt_hdr).ok()?;
    if &fmt_hdr[0..4] != b"fmt " {
        return None;
    }
    let fmt_size = u64::from_le_bytes(fmt_hdr[4..12].try_into().ok()?);
    let format_id = u32::from_le_bytes(fmt_hdr[16..20].try_into().ok()?);
    if format_id != 0 {
        return None; // only raw uncompressed DSD
    }
    let channels = u32::from_le_bytes(fmt_hdr[24..28].try_into().ok()?);
    let dsd_rate = u32::from_le_bytes(fmt_hdr[28..32].try_into().ok()?);
    let bits_per_sample = u32::from_le_bytes(fmt_hdr[32..36].try_into().ok()?);
    let sample_count = u64::from_le_bytes(fmt_hdr[36..44].try_into().ok()?);
    let block_size = u32::from_le_bytes(fmt_hdr[44..48].try_into().ok()?) as usize;

    if channels == 0 || dsd_rate < 44_100 || block_size == 0 {
        return None;
    }

    let data_hdr_pos = dsd_chunk_size + fmt_size;
    f.seek(SeekFrom::Start(data_hdr_pos)).ok()?;
    let mut data_hdr = [0u8; 12];
    f.read_exact(&mut data_hdr).ok()?;
    if &data_hdr[0..4] != b"data" {
        return None;
    }
    let data_chunk_size = u64::from_le_bytes(data_hdr[4..12].try_into().ok()?);
    let data_offset = data_hdr_pos + 12;
    let data_bytes = data_chunk_size.saturating_sub(12);
    let duration_seconds = sample_count as f64 / dsd_rate as f64;

    Some(DsdInfo {
        kind: DsdKind::Dsf,
        dsd_rate,
        dop_rate: dsd_rate / 16,
        channels,
        lsb_first: bits_per_sample == 1,
        data_offset,
        data_bytes,
        block_size,
        duration_seconds,
        dsd_label: dsd_label(dsd_rate),
    })
}

fn probe_dff(f: &mut File) -> Option<DsdInfo> {
    let mut id = [0u8; 4];
    f.read_exact(&mut id).ok()?;
    if &id != b"FRM8" {
        return None;
    }
    let form_size = read_u64_be(f)?;
    let mut form_type = [0u8; 4];
    f.read_exact(&mut form_type).ok()?;
    if &form_type != b"DSD " {
        return None;
    }

    let end_pos = 12 + form_size;
    let mut dsd_rate = 0u32;
    let mut channels = 0u32;
    let mut data_offset = 0u64;
    let mut data_bytes = 0u64;

    while f.stream_position().ok()? + 12 <= end_pos {
        let mut cid = [0u8; 4];
        if f.read_exact(&mut cid).is_err() {
            break;
        }
        let csize = read_u64_be(f)?;
        let payload_pos = f.stream_position().ok()?;

        match &cid {
            b"PROP" => {
                let mut prop_type = [0u8; 4];
                f.read_exact(&mut prop_type).ok()?;
                if &prop_type == b"SND " {
                    let prop_end = payload_pos + csize;
                    while f.stream_position().ok()? + 12 <= prop_end {
                        let mut sub_id = [0u8; 4];
                        if f.read_exact(&mut sub_id).is_err() {
                            break;
                        }
                        let sub_size = read_u64_be(f)?;
                        let sub_pos = f.stream_position().ok()?;
                        match &sub_id {
                            b"FS  " => dsd_rate = read_u32_be(f)?,
                            b"CHNL" => channels = read_u16_be(f)? as u32,
                            _ => {}
                        }
                        let next = sub_pos + sub_size + (sub_size & 1);
                        f.seek(SeekFrom::Start(next)).ok()?;
                    }
                }
            }
            b"DSD " => {
                data_offset = payload_pos;
                data_bytes = csize;
                break;
            }
            _ => {}
        }

        let next = payload_pos + csize + (csize & 1);
        f.seek(SeekFrom::Start(next)).ok()?;
    }

    if channels == 0 || dsd_rate < 44_100 || data_bytes == 0 {
        return None;
    }
    let total_bits_per_ch = (data_bytes / channels as u64) * 8;
    let duration_seconds = total_bits_per_ch as f64 / dsd_rate as f64;

    Some(DsdInfo {
        kind: DsdKind::Dff,
        dsd_rate,
        dop_rate: dsd_rate / 16,
        channels,
        lsb_first: false,
        data_offset,
        data_bytes,
        block_size: 1,
        duration_seconds,
        dsd_label: dsd_label(dsd_rate),
    })
}

/// Streaming reader that decodes DSF/DFF blocks directly into left-aligned 24-bit DoP v1.1 `i32` frames.
pub struct DsdReader {
    file: File,
    pub info: DsdInfo,
    byte_pos: u64,
    marker_toggle: bool,
    scratch: Vec<u8>,
}

impl DsdReader {
    pub fn open(path: &Path) -> Option<Self> {
        let info = probe_dsd(path)?;
        let mut file = File::open(path).ok()?;
        file.seek(SeekFrom::Start(info.data_offset)).ok()?;
        let chunk_bytes = match info.kind {
            DsdKind::Dsf => info.block_size * info.channels as usize,
            DsdKind::Dff => 4096 * info.channels as usize,
        };
        Some(Self {
            file,
            info,
            byte_pos: 0,
            marker_toggle: false,
            scratch: vec![0u8; chunk_bytes],
        })
    }

    pub fn seek_seconds(&mut self, secs: f64) {
        let dop_frames = (secs.max(0.0) * self.info.dop_rate as f64) as u64;
        let bytes_per_dop_frame = 2 * self.info.channels as u64;
        let target = match self.info.kind {
            DsdKind::Dsf => {
                let cluster = (self.info.block_size * self.info.channels as usize) as u64;
                ((dop_frames * bytes_per_dop_frame) / cluster) * cluster
            }
            DsdKind::Dff => dop_frames * bytes_per_dop_frame,
        };
        let clamped = target.min(self.info.data_bytes);
        if self.file.seek(SeekFrom::Start(self.info.data_offset + clamped)).is_ok() {
            self.byte_pos = clamped;
        }
    }

    /// Read the next chunk of DoP-packed 24-bit left-aligned `i32` samples.
    /// Returns `Some((interleaved_i32, dop_frames))` or `None` on EOF.
    pub fn next_dop_chunk(&mut self, out: &mut Vec<i32>) -> Option<usize> {
        out.clear();
        let rem = self.info.data_bytes.saturating_sub(self.byte_pos);
        if rem == 0 {
            return None;
        }
        let ch = self.info.channels as usize;
        match self.info.kind {
            DsdKind::Dsf => {
                let cluster = self.info.block_size * ch;
                if (rem as usize) < cluster {
                    return None;
                }
                self.file.read_exact(&mut self.scratch[..cluster]).ok()?;
                self.byte_pos += cluster as u64;
                let dop_frames = self.info.block_size / 2;
                out.reserve(dop_frames * ch);
                for f in 0..dop_frames {
                    let marker: i32 = if self.marker_toggle { 0xFA } else { 0x05 };
                    self.marker_toggle = !self.marker_toggle;
                    for c in 0..ch {
                        let base = c * self.info.block_size + f * 2;
                        let mut b0 = self.scratch[base];
                        let mut b1 = self.scratch[base + 1];
                        if self.info.lsb_first {
                            b0 = b0.reverse_bits();
                            b1 = b1.reverse_bits();
                        }
                        let sample = (marker << 24) | ((b0 as i32) << 16) | ((b1 as i32) << 8);
                        out.push(sample);
                    }
                }
                Some(dop_frames)
            }
            DsdKind::Dff => {
                let frame_bytes = ch * 2;
                let max_bytes = (rem as usize).min(self.scratch.len());
                let to_read = (max_bytes / frame_bytes) * frame_bytes;
                if to_read == 0 {
                    return None;
                }
                self.file.read_exact(&mut self.scratch[..to_read]).ok()?;
                self.byte_pos += to_read as u64;
                let dop_frames = to_read / frame_bytes;
                out.reserve(dop_frames * ch);
                for f in 0..dop_frames {
                    let marker: i32 = if self.marker_toggle { 0xFA } else { 0x05 };
                    self.marker_toggle = !self.marker_toggle;
                    let base = f * frame_bytes;
                    for c in 0..ch {
                        let b0 = self.scratch[base + c];
                        let b1 = self.scratch[base + ch + c];
                        let sample = (marker << 24) | ((b0 as i32) << 16) | ((b1 as i32) << 8);
                        out.push(sample);
                    }
                }
                Some(dop_frames)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dop_packing_alternates_0x05_and_0xfa_markers() {
        let mut dir = std::env::temp_dir();
        dir.push(format!("rex-dsf-test-{}.dsf", std::process::id()));

        // Build a minimal valid DSF file (2 channels, block_size = 4, DSD64 = 2_822_400 Hz)
        let mut buf = Vec::new();
        buf.extend_from_slice(b"DSD ");
        buf.extend_from_slice(&28u64.to_le_bytes());
        buf.extend_from_slice(&92u64.to_le_bytes());
        buf.extend_from_slice(&0u64.to_le_bytes());

        buf.extend_from_slice(b"fmt ");
        buf.extend_from_slice(&52u64.to_le_bytes());
        buf.extend_from_slice(&1u32.to_le_bytes()); // version
        buf.extend_from_slice(&0u32.to_le_bytes()); // format id (raw)
        buf.extend_from_slice(&2u32.to_le_bytes()); // channel type
        buf.extend_from_slice(&2u32.to_le_bytes()); // channels
        buf.extend_from_slice(&2_822_400u32.to_le_bytes()); // dsd rate
        buf.extend_from_slice(&1u32.to_le_bytes()); // lsb first
        buf.extend_from_slice(&32u64.to_le_bytes()); // sample count
        buf.extend_from_slice(&4u32.to_le_bytes()); // block size per channel
        buf.extend_from_slice(&0u32.to_le_bytes()); // reserved

        buf.extend_from_slice(b"data");
        buf.extend_from_slice(&20u64.to_le_bytes()); // 12 + 8 bytes
        // Ch 0 block (4 bytes): 0x80 reversed = 0x01, 0x40 reversed = 0x02, ...
        buf.extend_from_slice(&[0x80, 0x40, 0x00, 0xFF]);
        // Ch 1 block (4 bytes)
        buf.extend_from_slice(&[0xC0, 0x20, 0xAA, 0x55]);

        std::fs::write(&dir, &buf).unwrap();
        let mut reader = DsdReader::open(&dir).expect("valid DSF");
        assert_eq!(reader.info.dop_rate, 176_400);
        assert_eq!(reader.info.dsd_label, "DSD64");

        let mut out = Vec::new();
        let frames = reader.next_dop_chunk(&mut out).unwrap();
        assert_eq!(frames, 2);
        assert_eq!(out.len(), 4);

        // Frame 0 uses marker 0x05; Ch0 b0=0x01, b1=0x02
        assert_eq!(out[0] as u32, 0x05_01_02_00);
        // Frame 1 uses marker 0xFA
        assert_eq!((out[2] as u32) >> 24, 0xFA);

        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn dsd_labels_match_standard_multipliers() {
        assert_eq!(dsd_label(2_822_400), "DSD64");
        assert_eq!(dsd_label(5_644_800), "DSD128");
        assert_eq!(dsd_label(11_289_600), "DSD256");
    }
}

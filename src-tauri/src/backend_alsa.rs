//! Linux ALSA direct hardware (`hw:` / `plughw:` / `default`) backend, hardware mixer control,
//! and PipeWire / WirePlumber device hand-off.

use std::fs;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use alsa::card::Iter as CardIter;
use alsa::device_name::HintIter;
use alsa::pcm::{Access, HwParams, IO, PCM};
use alsa::{Direction, ValueOr};

use crate::audio_backend::{AudioBackend, OpenInfo, OpenReq, ProbeResult, WriteErr};
use crate::audio_probe::AudioDevice;
use crate::diagnostics;
use crate::logger::log_msg;
use crate::pcm_format::{pick_format, OutFmt};

pub struct AlsaBackend {
    pcm: Option<PCM>,
    info: OpenInfo,
}

impl AlsaBackend {
    /// Open `hw:`-style devices directly and negotiate the widest-enough format, rate and buffer.
    pub fn open(req: &OpenReq) -> Result<Self, ()> {
        claim_device_from_pipewire(&req.device_id);

        // EBUSY can linger briefly after the previous owner lets go, so retry. Never fall back to another
        // device: "default" may be a microphone or the resampling mixer.
        let mut opened = None;
        for attempt in 0..10 {
            match PCM::new(&req.device_id, Direction::Playback, false) {
                Ok(p) => {
                    diagnostics::REOPENED.fetch_add(1, Relaxed);
                    opened = Some(p);
                    break;
                }
                Err(err) if err.errno() == 16 && attempt < 9 => {
                    thread::sleep(Duration::from_millis(100));
                }
                Err(err) => {
                    log_msg(
                        "ERROR",
                        format!(
                            "Failed to open '{}': {:?}. (EBUSY = another process, e.g. WirePlumber/PipeWire, holds the card)",
                            req.device_id, err
                        ),
                    );
                    break;
                }
            }
        }
        let Some(pcm) = opened else {
            log_msg("ERROR", "Playback aborted: no fallback device used.");
            return Err(());
        };
        log_msg("ALSA", format!("✓ Successfully claimed PCM: '{}'", req.device_id));

        let (out_fmt, lossless, hw_rate, buffer, period) = {
            let hwp = HwParams::any(&pcm)
                .map_err(|e| log_msg("ERROR", format!("No hardware parameters: {e:?}")))?;
            let _ = hwp.set_access(Access::RWInterleaved);
            let _ = hwp.set_rate_resample(false); // never let ALSA resample behind our back
            let (fmt, lossless) = pick_format(req.bits, |f| hwp.test_format(f.alsa()).is_ok());
            let _ = hwp.set_format(fmt.alsa());
            let _ = hwp.set_channels(req.channels);
            let _ = hwp.set_rate_near(req.sample_rate, ValueOr::Nearest);
            let cfg = crate::settings::get();
            let _ = hwp.set_buffer_time_near(cfg.buffer_ms * 1000, ValueOr::Nearest);
            let _ = hwp.set_period_time_near(cfg.period_ms * 1000, ValueOr::Nearest);
            pcm.hw_params(&hwp)
                .map_err(|e| log_msg("ERROR", format!("Failed to apply HwParams: {e:?}")))?;
            let cur = pcm.hw_params_current().map_err(|_| ())?;
            (
                fmt,
                lossless,
                cur.get_rate().unwrap_or(req.sample_rate),
                cur.get_buffer_size().unwrap_or(0).max(0) as u64,
                cur.get_period_size().unwrap_or(0).max(0) as u64,
            )
        };
        let fmt = out_fmt.label(req.bits, lossless);
        log_msg("ALSA", format!("HwParams applied: {hw_rate} Hz · {fmt}"));
        set_start_threshold(&pcm);

        let info = OpenInfo {
            device_id: req.device_id.clone(),
            hw_rate,
            channels: req.channels,
            wide: req.bits >= 24,
            out_fmt,
            lossless,
            fmt,
            buffer,
            period,
        };
        Ok(Self {
            pcm: Some(pcm),
            info,
        })
    }
}

impl AudioBackend for AlsaBackend {
    fn info(&self) -> &OpenInfo {
        &self.info
    }

    fn write_i16(&mut self, data: &[i16], frames: usize) -> Result<(), WriteErr> {
        let Some(pcm) = &self.pcm else { return Err(WriteErr::Fatal) };
        let ch = self.info.channels as usize;
        match pcm.io_i16() {
            Ok(io) => write_all(pcm, &io, data, frames, ch),
            Err(e) => {
                log_msg("ERROR", format!("Output format mismatch: {e}"));
                Err(WriteErr::Fatal)
            }
        }
    }

    fn write_i32(&mut self, data: &[i32], frames: usize) -> Result<(), WriteErr> {
        let Some(pcm) = &self.pcm else { return Err(WriteErr::Fatal) };
        let ch = self.info.channels as usize;
        let io = match self.info.out_fmt {
            OutFmt::S24Low => pcm.io_i32_s24(),
            _ => pcm.io_i32(),
        };
        match io {
            Ok(io) => write_all(pcm, &io, data, frames, ch),
            Err(e) => {
                log_msg("ERROR", format!("Output format mismatch: {e}"));
                Err(WriteErr::Fatal)
            }
        }
    }

    fn write_bytes(&mut self, data: &[u8], frames: usize, bytes_per_frame: usize) -> Result<(), WriteErr> {
        let Some(pcm) = &self.pcm else { return Err(WriteErr::Fatal) };
        write_all(pcm, &pcm.io_bytes(), data, frames, bytes_per_frame)
    }

    fn delay_frames(&self) -> u64 {
        self.pcm
            .as_ref()
            .map_or(0, |p| p.delay().unwrap_or(0).max(0) as u64)
    }

    fn pause(&mut self, paused: bool) {
        if let Some(pcm) = &self.pcm {
            let _ = pcm.pause(paused);
        }
    }

    fn drain(&mut self) {
        if let Some(pcm) = &self.pcm {
            let _ = pcm.drain();
        }
    }

    fn flush_and_prepare(&mut self) {
        if let Some(pcm) = &self.pcm {
            let _ = pcm.drop();
            let _ = pcm.prepare();
            set_start_threshold(pcm);
        }
    }

    fn close(&mut self) {
        if let Some(pcm) = self.pcm.take() {
            let _ = pcm.drop();
            drop(pcm);
        }
        release_device_to_pipewire();
    }
}

impl Drop for AlsaBackend {
    fn drop(&mut self) {
        self.close();
    }
}

pub fn probe_device(device_id: &str) -> ProbeResult {
    match PCM::new(device_id, Direction::Playback, false) {
        Ok(pcm) => {
            drop(pcm);
            ProbeResult::Ready
        }
        Err(e) if e.errno() == 16 => ProbeResult::Busy,
        Err(_) => ProbeResult::Missing,
    }
}

/// Start playing only once the buffer is full, so the stream never starts half-fed.
fn set_start_threshold(pcm: &PCM) {
    if let (Ok(swp), Ok(hw)) = (pcm.sw_params_current(), pcm.hw_params_current()) {
        if let Ok(buf) = hw.get_buffer_size() {
            let _ = swp.set_start_threshold(buf);
            let _ = pcm.sw_params(&swp);
        }
    }
}

fn is_disconnect_errno(errno: i32) -> bool {
    // ENXIO (6), EIO (5), ENODEV (19), ESTRPIPE (86)
    matches!(errno, 5 | 6 | 19 | 86)
}

/// Write `num_frames` frames, retrying after recoverable errors (xrun) with the same data.
fn write_all<S: Copy>(
    pcm: &PCM,
    io: &IO<'_, S>,
    data: &[S],
    num_frames: usize,
    items_per_frame: usize,
) -> Result<(), WriteErr> {
    let mut written = 0;
    let mut retries = 0;
    while written < num_frames {
        match io.writei(&data[written * items_per_frame..]) {
            Ok(w) => written += w,
            Err(e) => {
                let errno = e.errno();
                let msg = e.to_string();
                if is_disconnect_errno(errno) {
                    diagnostics::DISCONNECTS.fetch_add(1, Relaxed);
                    log_msg("WARN", format!("Audio device disconnected (errno {errno}): {msg}"));
                    return Err(WriteErr::Disconnected);
                }
                if errno == 32 {
                    diagnostics::XRUNS.fetch_add(1, Relaxed); // EPIPE
                }
                retries += 1;
                if retries > 5 || pcm.try_recover(e, true).is_err() {
                    diagnostics::WRITE_FAILS.fetch_add(1, Relaxed);
                    log_msg("ERROR", format!("ALSA write failed: {msg}"));
                    return Err(WriteErr::Fatal);
                }
                diagnostics::RECOVERED.fetch_add(1, Relaxed);
            }
        }
    }
    Ok(())
}

// ---- Device enumeration ------------------------------------------------------------------

pub fn list_devices() -> Vec<AudioDevice> {
    let mut devices = Vec::new();

    for card_res in CardIter::new() {
        if let Ok(card) = card_res {
            let card_idx = card.get_index();
            let card_name = card.get_name().unwrap_or_else(|_| format!("Card {}", card_idx));
            let card_long = card.get_longname().unwrap_or_default();

            let stream_path = format!("/proc/asound/card{}/stream0", card_idx);
            let has_usb_stream = fs::metadata(&stream_path).is_ok();
            let is_usb_dac = has_usb_stream
                || card_long.to_lowercase().contains("usb")
                || card_name.to_lowercase().contains("usb");

            let mut hw_rates = Vec::new();
            if let Ok(contents) = fs::read_to_string(&stream_path) {
                for line in contents.lines() {
                    if line.trim().starts_with("Rates:") {
                        let rates_str = line.trim().trim_start_matches("Rates:").trim();
                        for r_part in rates_str.split(',') {
                            if let Ok(r) = r_part.trim().parse::<u32>() {
                                if !hw_rates.contains(&r) {
                                    hw_rates.push(r);
                                }
                            }
                        }
                    }
                }
            }
            hw_rates.sort();

            if hw_rates.is_empty() {
                if is_usb_dac {
                    hw_rates = vec![44100, 48000, 88200, 96000, 176400, 192000];
                } else {
                    hw_rates = vec![48000];
                }
            }

            let min_rate = *hw_rates.first().unwrap_or(&44100);
            let max_rate = *hw_rates.last().unwrap_or(&192000);

            let (disp_name, disp_desc) = if is_usb_dac {
                (
                    format!("⚡ {} (Bit-Perfect USB DAC hw:{},0)", card_name, card_idx),
                    format!("{} · External USB DAC with native clock switching", card_long),
                )
            } else {
                (
                    format!("{} (Internal Audio hw:{},0)", card_name, card_idx),
                    format!("{} · Motherboard audio (fixed 48kHz, in-engine resampled if needed)", card_long),
                )
            };

            let hw_id = format!("hw:{},0", card_idx);
            devices.push(AudioDevice {
                id: hw_id,
                name: disp_name,
                description: disp_desc,
                is_hardware: true,
                is_usb_dac,
                min_rate,
                max_rate,
                supported_rates: hw_rates.clone(),
                supports_16bit: true,
                supports_24bit: true,
                supports_32bit: true,
            });

            let plughw_id = format!("plughw:{},0", card_idx);
            devices.push(AudioDevice {
                id: plughw_id,
                name: format!("{} (ALSA Plug plughw:{},0)", card_name, card_idx),
                description: format!("{} · Hardware with automatic format negotiation", card_long),
                is_hardware: true,
                is_usb_dac,
                min_rate,
                max_rate,
                supported_rates: hw_rates,
                supports_16bit: true,
                supports_24bit: true,
                supports_32bit: true,
            });
        }
    }

    devices.push(AudioDevice {
        id: "default".into(),
        name: "System Default (PipeWire / Shared Audio)".into(),
        description: "Shared desktop audio server (auto-rate negotiation & system mixing)".into(),
        is_hardware: false,
        is_usb_dac: false,
        min_rate: 44100,
        max_rate: 192000,
        supported_rates: vec![44100, 48000, 96000, 192000],
        supports_16bit: true,
        supports_24bit: true,
        supports_32bit: true,
    });

    if let Ok(hints) = HintIter::new_str(None, "pcm") {
        for hint in hints {
            if hint.direction == Some(Direction::Capture) {
                continue;
            }
            if let Some(name) = hint.name {
                if name.starts_with("front:CARD=") || name.starts_with("iec958:CARD=") {
                    let desc = hint.desc.unwrap_or_default().replace('\n', " - ");
                    devices.push(AudioDevice {
                        id: name.clone(),
                        name: format!("ALSA Front: {}", name),
                        description: desc,
                        is_hardware: true,
                        is_usb_dac: false,
                        min_rate: 44100,
                        max_rate: 192000,
                        supported_rates: vec![44100, 48000, 96000, 192000],
                        supports_16bit: true,
                        supports_24bit: true,
                        supports_32bit: true,
                    });
                }
            }
        }
    }

    devices
}

// ---- Hardware mixer volume ---------------------------------------------------------------

static MIXER_PICK: Mutex<Option<String>> = Mutex::new(None);

pub fn set_mixer_pick(name: Option<String>) {
    *MIXER_PICK.lock().unwrap_or_else(|e| e.into_inner()) = name.filter(|n| !n.is_empty());
}

fn open_mixer(device_id: &str) -> Option<alsa::mixer::Mixer> {
    let card = device_id.split(',').next()?.replace("plughw:", "hw:");
    if !card.starts_with("hw:") {
        return None;
    }
    alsa::mixer::Mixer::new(&card, false).ok()
}

pub fn list_volume_controls(device_id: &str) -> Vec<String> {
    let Some(mixer) = open_mixer(device_id) else { return vec![] };
    mixer
        .iter()
        .filter_map(|e| alsa::mixer::Selem::new(e))
        .filter(|s| s.has_playback_volume())
        .filter_map(|s| s.get_id().get_name().ok().map(str::to_string))
        .collect()
}

fn with_volume_ctl<T>(
    device_id: &str,
    f: impl FnOnce(&alsa::mixer::Selem, i64, i64) -> Option<T>,
) -> Option<T> {
    use alsa::mixer::SelemId;
    let mixer = open_mixer(device_id)?;
    let ids: Vec<SelemId> = mixer
        .iter()
        .filter_map(|e| alsa::mixer::Selem::new(e))
        .map(|s| s.get_id())
        .collect();
    let has_vol = |i: &SelemId| mixer.find_selem(i).map_or(false, |s| s.has_playback_volume());
    let wanted = MIXER_PICK.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let pick = wanted
        .as_deref()
        .and_then(|w| ids.iter().find(|i| i.get_name().ok() == Some(w)).filter(|i| has_vol(i)))
        .or_else(|| {
            ["PCM", "Master", "Headphone", "Speaker"]
                .iter()
                .find_map(|n| ids.iter().find(|i| i.get_name().ok() == Some(*n)))
        })
        .or_else(|| ids.iter().find(|i| has_vol(i)))?;
    let sel = mixer.find_selem(pick)?;
    if !sel.has_playback_volume() {
        return None;
    }
    let (lo, hi) = sel.get_playback_volume_range();
    f(&sel, lo, hi)
}

pub fn apply_hw_volume(device_id: &str, percent: u32, gain_db: f64) -> bool {
    with_volume_ctl(device_id, |sel, lo, hi| {
        let v = lo + (hi - lo) * percent.min(100) as i64 / 100;
        if gain_db < 0.0 {
            if let Ok(db) = sel.ask_playback_vol_db(v) {
                let target = alsa::mixer::MilliBel(db.0 + (gain_db * 100.0) as i64);
                return sel.set_playback_db_all(target, alsa::Round::Floor).ok();
            }
        }
        sel.set_playback_volume_all(v).ok()
    })
    .is_some()
}

pub fn get_hw_volume(device_id: &str) -> Option<u32> {
    with_volume_ctl(device_id, |sel, lo, hi| {
        let v = sel.get_playback_volume(alsa::mixer::SelemChannelId::FrontLeft).ok()?;
        Some(if hi > lo { ((v - lo) * 100 / (hi - lo)) as u32 } else { 0 })
    })
}

// ---- PipeWire hand-off -------------------------------------------------------------------

static SUSPENDED_SINKS: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn pactl(args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("pactl")
        .args(args)
        .env("LC_ALL", "C")
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn card_of(device_id: &str) -> Option<String> {
    let rest = device_id.strip_prefix("plughw:").or_else(|| device_id.strip_prefix("hw:"))?;
    Some(rest.split(',').next()?.to_string())
}

pub fn claim_device_from_pipewire(device_id: &str) {
    let Some(card) = card_of(device_id) else { return };
    if crate::settings::get().reserve_dbus {
        if crate::reserve::claim(&card) {
            log_msg("INFO", format!("Reserved card {card} over D-Bus (ReserveDevice1)"));
            return;
        }
        log_msg("WARN", format!("D-Bus reservation of card {card} failed; falling back to pactl"));
    }
    let Some(listing) = pactl(&["list", "sinks"]) else { return };
    let needle = format!("alsa.card = \"{card}\"");
    let mut name: Option<String> = None;
    for line in listing.lines().map(str::trim) {
        if let Some(n) = line.strip_prefix("Name: ") {
            name = Some(n.to_string());
        } else if line == needle {
            if let Some(n) = name.take() {
                if pactl(&["suspend-sink", &n, "1"]).is_some() {
                    log_msg("INFO", format!("Suspended PipeWire sink to free the DAC: {n}"));
                    let mut held = SUSPENDED_SINKS.lock().unwrap_or_else(|e| e.into_inner());
                    if !held.contains(&n) {
                        held.push(n);
                    }
                }
            }
        }
    }
}

pub fn release_device_to_pipewire() {
    crate::reserve::release_all();
    let sinks: Vec<String> = SUSPENDED_SINKS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .drain(..)
        .collect();
    for n in sinks {
        pactl(&["suspend-sink", &n, "0"]);
        log_msg("INFO", format!("Returned sink to PipeWire: {n}"));
    }
}

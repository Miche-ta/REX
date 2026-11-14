use std::fs::File;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use alsa::pcm::{Access, Format, HwParams, PCM};
use alsa::{Direction, ValueOr};
use serde::{Deserialize, Serialize};

use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, SeekMode, SeekTo, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, StandardTag};
use symphonia::core::units::Time;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybackStatus {
    pub is_playing: bool,
    pub is_paused: bool,
    pub position_seconds: f64,
    pub duration_seconds: f64,
    pub file_path: Option<String>,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub sample_rate: u32,
    pub bit_depth: u32,
    pub channels: u32,
    pub active_device_id: String,
    pub active_hardware_rate: u32,
    pub active_hardware_format: String,
    pub is_bit_perfect: bool,
    pub ended: bool, // true only when the track played to its natural end
}

impl Default for PlaybackStatus {
    fn default() -> Self {
        Self {
            is_playing: false,
            is_paused: false,
            position_seconds: 0.0,
            duration_seconds: 0.0,
            file_path: None,
            title: "No Track Selected".into(),
            artist: "Idle".into(),
            album: "rex-music".into(),
            sample_rate: 44100,
            bit_depth: 16,
            channels: 2,
            active_device_id: "default".into(),
            active_hardware_rate: 44100,
            active_hardware_format: "None".into(),
            is_bit_perfect: false,
            ended: false,
        }
    }
}

pub enum EngineCommand {
    Play {
        file_path: String,
        device_id: String,
    },
    Pause,
    Resume,
    Stop,
    Seek(f64),
}

pub struct AudioEngineHandle {
    status: Arc<Mutex<PlaybackStatus>>,
    command_tx: std::sync::mpsc::Sender<EngineCommand>,
}

impl AudioEngineHandle {
    pub fn new() -> Self {
        let status = Arc::new(Mutex::new(PlaybackStatus::default()));
        let (tx, rx) = std::sync::mpsc::channel::<EngineCommand>();

        let thread_status = Arc::clone(&status);

        thread::Builder::new()
            .name("rex-audio-thread".into())
            .spawn(move || {
                run_audio_worker(rx, thread_status);
            })
            .expect("Failed to spawn audio engine thread");

        Self {
            status,
            command_tx: tx,
        }
    }

    pub fn get_status(&self) -> PlaybackStatus {
        self.status.lock().unwrap().clone()
    }

    pub fn play(&self, file_path: String, device_id: String) {
        let _ = self.command_tx.send(EngineCommand::Play { file_path, device_id });
    }

    pub fn pause(&self) {
        let _ = self.command_tx.send(EngineCommand::Pause);
    }

    pub fn resume(&self) {
        let _ = self.command_tx.send(EngineCommand::Resume);
    }

    pub fn stop(&self) {
        let _ = self.command_tx.send(EngineCommand::Stop);
    }

    pub fn seek(&self, seconds: f64) {
        let _ = self.command_tx.send(EngineCommand::Seek(seconds));
    }
}

fn run_audio_worker(
    rx: std::sync::mpsc::Receiver<EngineCommand>,
    status: Arc<Mutex<PlaybackStatus>>,
) {
    while let Ok(cmd) = rx.recv() {
        match cmd {
            EngineCommand::Play { file_path, device_id } => {
                // play_file_loop returns (and drops its PCM) before the next track opens
                let mut next = Some((file_path, device_id));
                while let Some((f, d)) = next.take() {
                    play_file_loop(&rx, &status, &f, &d, &mut next);
                }
            }
            EngineCommand::Stop => {
                let mut st = status.lock().unwrap();
                st.is_playing = false;
                st.is_paused = false;
                st.position_seconds = 0.0;
            }
            _ => {}
        }
    }
}

fn play_file_loop(
    rx: &std::sync::mpsc::Receiver<EngineCommand>,
    status: &Arc<Mutex<PlaybackStatus>>,
    file_path: &str,
    device_id: &str,
    next: &mut Option<(String, String)>,
) {
    let path = Path::new(file_path);
    let src = match File::open(path) {
        Ok(f) => f,
        Err(_) => return,
    };

    let mss = MediaSourceStream::new(Box::new(src), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
        hint.with_extension(ext);
    }

    let fmt_opts = FormatOptions::default();
    let meta_opts = MetadataOptions::default();
    let dec_opts = AudioDecoderOptions::default();

    let mut format_reader = match symphonia::default::get_probe().probe(&hint, mss, fmt_opts, meta_opts) {
        Ok(p) => p,
        Err(_) => return,
    };

    let track = match format_reader.default_track(TrackType::Audio) {
        Some(t) => t.clone(),
        None => return,
    };

    let track_id = track.id;
    let codec_params = match track.codec_params.as_ref() {
        Some(cp) => cp,
        None => return,
    };

    let audio_params = match codec_params.audio() {
        Some(ap) => ap,
        None => return,
    };

    let sample_rate = audio_params.sample_rate.unwrap_or(44100);
    let channels = audio_params.channels.as_ref().map(|c| c.count() as u32).unwrap_or(2);
    let bits_per_sample = audio_params.bits_per_sample.unwrap_or(16);

    let duration_seconds = track
        .num_frames
        .map(|f| f as f64 / sample_rate as f64)
        .unwrap_or(0.0);

    let is_hw = device_id.starts_with("hw:") || device_id.starts_with("plughw:");

    // Read metadata tags
    let mut title = path.file_stem().and_then(|s| s.to_str()).unwrap_or("Unknown Title").to_string();
    let mut artist = "Unknown Artist".to_string();
    let mut album = "Local Audio".to_string();

    if let Some(meta) = format_reader.metadata().current() {
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

    use crate::logger::log_msg;

    log_msg("INFO", format!("Playing: {} ({})", title, path.display()));
    log_msg("INFO", format!("Track Format: {} Hz · {}-bit · {} channels · Duration: {:.2}s", sample_rate, bits_per_sample, channels, duration_seconds));
    log_msg("ALSA", format!("Requesting audio output endpoint: '{}'", device_id));

    // Open target PCM. EBUSY can linger briefly after the previous stream closes, so retry.
    // Never fall back to another device: "default" may be a mic or the resampling mixer.
    let mut opened = None;
    for attempt in 0..10 {
        match PCM::new(device_id, Direction::Playback, false) {
            Ok(p) => { opened = Some(p); break; }
            Err(err) if err.errno() == 16 && attempt < 9 => {
                thread::sleep(Duration::from_millis(100));
            }
            Err(err) => {
                log_msg("ERROR", format!("Failed to open '{}': {:?}. (EBUSY = another process, e.g. WirePlumber/PipeWire, holds the card)", device_id, err));
                break;
            }
        }
    }
    let Some(pcm) = opened else {
        log_msg("ERROR", "Playback aborted: no fallback device used.");
        if let Ok(mut st) = status.lock() { st.is_playing = false; }
        return;
    };
    log_msg("ALSA", format!("✓ Successfully claimed PCM: '{}'", device_id));
    let resolved_device_id = device_id.to_string();

    // Configure Hardware Parameters for True Bit-Perfect Output
    let mut actual_rate = sample_rate;
    let mut actual_format_str = "S16_LE".to_string();
    let mut use_32bit_output = false;

    if let Ok(hwp) = HwParams::any(&pcm) {
        // 1. MUST set access method first
        let _ = hwp.set_access(Access::RWInterleaved);

        // 2. CRITICAL: Disable ALSA rate resampling! Forces bit-perfect matching
        let _ = hwp.set_rate_resample(false);

        // 3. Set Format: if 24-bit or 32-bit, prefer S32_LE or S24_3LE
        let chosen_format = if bits_per_sample >= 24 {
            if hwp.test_format(Format::s32()).is_ok() {
                use_32bit_output = true;
                actual_format_str = "S32_LE (Bit-Exact 24/32)".to_string();
                Format::s32()
            } else if hwp.test_format(Format::s24_3()).is_ok() {
                use_32bit_output = true;
                actual_format_str = "S24_3LE (Bit-Exact 24)".to_string();
                Format::s24_3()
            } else {
                actual_format_str = "S16_LE (Truncated to 16)".to_string();
                Format::s16()
            }
        } else {
            if hwp.test_format(Format::s16()).is_ok() {
                actual_format_str = "S16_LE (Bit-Exact 16)".to_string();
                Format::s16()
            } else if hwp.test_format(Format::s32()).is_ok() {
                use_32bit_output = true;
                actual_format_str = "S32_LE (16-bit Padded to 32)".to_string();
                Format::s32()
            } else {
                Format::s16()
            }
        };

        let _ = hwp.set_format(chosen_format);
        let _ = hwp.set_channels(channels);

        // 4. Request the exact sample rate (e.g. 192000, 96000, 44100)
        let _ = hwp.set_rate_near(sample_rate, ValueOr::Nearest);

        // Apply hardware params
        match pcm.hw_params(&hwp) {
            Ok(_) => {
                if let Ok(curr) = pcm.hw_params_current() {
                    if let Ok(r) = curr.get_rate() {
                        actual_rate = r;
                    }
                }
                log_msg("ALSA", format!("HwParams applied: {} Hz · {}", actual_rate, actual_format_str));
            }
            Err(e) => {
                log_msg("ERROR", format!("Failed to apply HwParams: {:?}", e));
            }
        }
    }

    if let (Ok(swp), Ok(hwp_curr)) = (pcm.sw_params_current(), pcm.hw_params_current()) {
        if let Ok(buf_size) = hwp_curr.get_buffer_size() {
            let _ = swp.set_start_threshold(buf_size);
            let _ = pcm.sw_params(&swp);
            log_msg("ALSA", format!("SwParams start threshold set to buffer size: {}", buf_size));
        }
    }

    let is_really_hw = resolved_device_id.starts_with("hw:") || resolved_device_id.starts_with("plughw:");
    let is_true_bit_perfect = is_really_hw && actual_rate == sample_rate;

    if is_true_bit_perfect {
        log_msg("INFO", format!(">>> ⚡ BIT-PERFECT VERIFIED: Direct HW '{}' @ {} Hz (0 resampling)", resolved_device_id, actual_rate));
    } else {
        log_msg("WARN", format!(">>> ⚠️ NON-BIT-PERFECT: Active output '{}' running @ {} Hz (Source: {} Hz)", resolved_device_id, actual_rate, sample_rate));
    }

    // Update status
    {
        let mut st = status.lock().unwrap();
        st.is_playing = true;
        st.ended = false;
        st.is_paused = false;
        st.position_seconds = 0.0;
        st.duration_seconds = duration_seconds;
        st.file_path = Some(file_path.to_string());
        st.title = title;
        st.artist = artist;
        st.album = album;
        st.sample_rate = sample_rate;
        st.bit_depth = bits_per_sample;
        st.channels = channels;
        st.active_device_id = resolved_device_id;
        st.active_hardware_rate = actual_rate;
        st.active_hardware_format = actual_format_str;
        st.is_bit_perfect = is_true_bit_perfect;
    }

    let mut decoder = match symphonia::default::get_codecs().make_audio_decoder(audio_params, &dec_opts) {
        Ok(d) => d,
        Err(_) => return,
    };

    let mut current_frame: u64 = 0;
    let mut sample_buf_16: Vec<i16> = Vec::new();
    let mut sample_buf_32: Vec<i32> = Vec::new();

    let mut reached_eof = false;
    loop {
        // Handle incoming commands
        if let Ok(cmd) = rx.try_recv() {
            match cmd {
                EngineCommand::Stop => {
                    let _ = pcm.drop();
                    break;
                }
                EngineCommand::Play { file_path: new_file, device_id: new_dev } => {
                    *next = Some((new_file, new_dev));
                    let _ = pcm.drop(); // discard buffer, release card immediately
                    return;
                }
                EngineCommand::Pause => {
                    if let Ok(mut st) = status.lock() {
                        st.is_paused = true;
                    }
                    let _ = pcm.pause(true);
                    loop {
                        thread::sleep(Duration::from_millis(50));
                        if let Ok(resume_cmd) = rx.recv() {
                            match resume_cmd {
                                EngineCommand::Resume => {
                                    if let Ok(mut st) = status.lock() {
                                        st.is_paused = false;
                                    }
                                    let _ = pcm.pause(false);
                                    break;
                                }
                                EngineCommand::Stop => return,
                                EngineCommand::Play { file_path: f, device_id: d } => {
                                    *next = Some((f, d));
                                    let _ = pcm.drop();
                                    return;
                                }
                                _ => {}
                            }
                        }
                    }
                }
                EngineCommand::Seek(pos) => {
                    let time = Time::try_from_secs_f64(pos).unwrap_or(Time::ZERO);
                    let _ = format_reader.seek(
                        SeekMode::Accurate,
                        SeekTo::Time {
                            time,
                            track_id: Some(track_id),
                        },
                    );
                    current_frame = (pos * sample_rate as f64) as u64;
                    // flush audio already queued in the DAC buffer so the jump is immediate
                    let _ = pcm.drop();
                    let _ = pcm.prepare();
                    decoder.reset();
                    if let Ok(mut st) = status.lock() { st.position_seconds = pos; }
                }
                EngineCommand::Resume => {}
            }
        }

        // Fetch packet from audio stream
        let packet = match format_reader.next_packet() {
            Ok(Some(pkt)) => pkt,
            Ok(None) => { reached_eof = true; break; } // Reached EOF
            Err(SymphoniaError::ResetRequired) => break,
            Err(_) => break,
        };

        if packet.track_id != track_id {
            continue;
        }

        match decoder.decode(&packet) {
            Ok(audio_buf) => {
                let num_samples = audio_buf.samples_interleaved();
                let num_frames = num_samples / (channels as usize);

                if use_32bit_output {
                    // Bit-Exact 24-bit / 32-bit streaming directly to DAC
                    sample_buf_32.resize(num_samples, 0i32);
                    audio_buf.copy_to_slice_interleaved(&mut sample_buf_32);

                    if let Ok(io) = pcm.io_i32() {
                        let mut frames_written = 0;
                        while frames_written < num_frames {
                            let offset = frames_written * channels as usize;
                            let slice = &sample_buf_32[offset..];
                            match io.writei(slice) {
                                Ok(w) => frames_written += w,
                                Err(_) => {
                                    let _ = pcm.try_recover(alsa::Error::new("pcm_write", -32), false);
                                    break;
                                }
                            }
                        }
                    }
                } else {
                    // Standard 16-bit integer streaming
                    sample_buf_16.resize(num_samples, 0i16);
                    audio_buf.copy_to_slice_interleaved(&mut sample_buf_16);

                    if let Ok(io) = pcm.io_i16() {
                        let mut frames_written = 0;
                        while frames_written < num_frames {
                            let offset = frames_written * channels as usize;
                            let slice = &sample_buf_16[offset..];
                            match io.writei(slice) {
                                Ok(w) => frames_written += w,
                                Err(_) => {
                                    let _ = pcm.try_recover(alsa::Error::new("pcm_write", -32), false);
                                    break;
                                }
                            }
                        }
                    }
                }

                current_frame += num_frames as u64;

                // Report what is actually audible: frames written minus frames still queued in the DAC buffer
                let queued = pcm.delay().unwrap_or(0).max(0) as u64;
                if let Ok(mut st) = status.lock() {
                    st.position_seconds = current_frame.saturating_sub(queued) as f64 / sample_rate as f64;
                }
            }
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(_) => break,
        }
    }

    if reached_eof { let _ = pcm.drain(); } else { let _ = pcm.drop(); }
    if let Ok(mut st) = status.lock() {
        st.ended = reached_eof;
        st.is_playing = false;
        st.is_paused = false;
        st.position_seconds = 0.0;
    }
}

/// Hardware (DAC) volume via the ALSA mixer: leaves the PCM stream untouched, so bit-perfect stays intact.
/// `device_id` like "hw:2,0" -> mixer card "hw:2". Returns the control's volume as 0..=100.
fn with_volume_ctl<T>(device_id: &str, f: impl FnOnce(&alsa::mixer::Selem, i64, i64) -> Option<T>) -> Option<T> {
    use alsa::mixer::{Mixer, SelemId};
    let card = device_id.split(',').next()?.replace("plughw:", "hw:");
    if !card.starts_with("hw:") { return None; }
    let mixer = Mixer::new(&card, false).ok()?;
    // first playback-volume control, preferring the usual names
    let ids: Vec<SelemId> = mixer.iter().filter_map(|e| alsa::mixer::Selem::new(e)).map(|s| s.get_id()).collect();
    let pick = ["PCM", "Master", "Headphone", "Speaker"].iter()
        .find_map(|n| ids.iter().find(|i| i.get_name().ok() == Some(*n)))
        .or_else(|| ids.iter().find(|i| mixer.find_selem(i).map_or(false, |s| s.has_playback_volume())))?;
    let sel = mixer.find_selem(pick)?;
    if !sel.has_playback_volume() { return None; }
    let (lo, hi) = sel.get_playback_volume_range();
    f(&sel, lo, hi)
}

pub fn set_hw_volume(device_id: &str, percent: u32) -> bool {
    with_volume_ctl(device_id, |sel, lo, hi| {
        let v = lo + (hi - lo) * percent.min(100) as i64 / 100;
        sel.set_playback_volume_all(v).ok()
    }).is_some()
}

pub fn get_hw_volume(device_id: &str) -> Option<u32> {
    with_volume_ctl(device_id, |sel, lo, hi| {
        let v = sel.get_playback_volume(alsa::mixer::SelemChannelId::FrontLeft).ok()?;
        Some(if hi > lo { ((v - lo) * 100 / (hi - lo)) as u32 } else { 0 })
    })
}

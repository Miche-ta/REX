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
    pub active_buffer_frames: u64,
    pub active_period_frames: u64,
    pub is_bit_perfect: bool,
    pub ended: bool, // true only when the track played to its natural end
    pub replay_gain_db: f32, // track ReplayGain tag in dB, 0 when absent
    pub track_serial: u64, // bumps every time a track starts (also when the engine advances by itself)
    pub vu_left: f32,
    pub vu_right: f32,
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
            active_buffer_frames: 0,
            active_period_frames: 0,
            is_bit_perfect: false,
            ended: false,
            replay_gain_db: 0.0,
            track_serial: 0,
            vu_left: 0.0,
            vu_right: 0.0,
        }
    }
}

/// An open, configured PCM carried from one track to the next, so the device is never closed
/// between tracks (no gap, no EBUSY race with PipeWire).
struct Kept {
    pcm: PCM,
    device_id: String,
    rate: u32,
    channels: u32,
    wide: bool, // source is 24-bit or more
    use_32bit: bool,
    fmt: String,
    buffer: u64,
    period: u64,
    drain_first: bool, // stream is still running with the previous track's tail in it
}

/// Path of the track that should follow the current one (set by the UI).
type NextHint = Arc<Mutex<Option<String>>>;

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
    next_hint: NextHint,
}

impl AudioEngineHandle {
    pub fn new() -> Self {
        let status = Arc::new(Mutex::new(PlaybackStatus::default()));
        let (tx, rx) = std::sync::mpsc::channel::<EngineCommand>();

        let thread_status = Arc::clone(&status);
        let next_hint: NextHint = Arc::new(Mutex::new(None));
        let thread_hint = Arc::clone(&next_hint);

        thread::Builder::new()
            .name("rex-audio-thread".into())
            .spawn(move || {
                run_audio_worker(rx, thread_status, thread_hint);
            })
            .expect("Failed to spawn audio engine thread");

        Self {
            status,
            command_tx: tx,
            next_hint,
        }
    }

    /// Like get_status but leaves the VU peak alone (for background readers such as MPRIS).
    pub fn peek_status(&self) -> PlaybackStatus {
        self.status.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn get_status(&self) -> PlaybackStatus {
        let mut st = self.status.lock().unwrap_or_else(|e| e.into_inner());
        let out = st.clone();
        st.vu_left = 0.0; // peak-hold restarts after each read
        st.vu_right = 0.0;
        out
    }

    pub fn play(&self, file_path: String, device_id: String) {
        let _ = self.command_tx.send(EngineCommand::Play { file_path, device_id });
    }

    /// Tell the engine what plays after the current track (None = nothing), for gapless hand-off.
    pub fn set_next(&self, file_path: Option<String>) {
        *self.next_hint.lock().unwrap_or_else(|e| e.into_inner()) = file_path;
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
    next_hint_w: NextHint,
) {
    while let Ok(cmd) = rx.recv() {
        match cmd {
            EngineCommand::Play { file_path, device_id } => {
                // play_file_loop returns (and drops its PCM) before the next track opens
                let mut next = Some((file_path, device_id));
                let mut kept: Option<Kept> = None;
                while let Some((f, d)) = next.take() {
                    play_file_loop(&rx, &status, &next_hint_w, &f, &d, &mut next, &mut kept);
                }
                drop(kept);
                release_device_to_pipewire();
                // a hand-off to a file that would not open must not leave the UI "playing"
                if let Ok(mut st) = status.lock() {
                    st.is_playing = false;
                    st.is_paused = false;
                    st.vu_left = 0.0;
                    st.vu_right = 0.0;
                }
            }
            EngineCommand::Stop => {
                let mut st = status.lock().unwrap_or_else(|e| e.into_inner());
                st.is_playing = false;
                st.is_paused = false;
                st.position_seconds = 0.0;
                st.vu_left = 0.0;
                st.vu_right = 0.0;
            }
            _ => {}
        }
    }
}

fn play_file_loop(
    rx: &std::sync::mpsc::Receiver<EngineCommand>,
    status: &Arc<Mutex<PlaybackStatus>>,
    next_hint: &NextHint,
    file_path: &str,
    device_id: &str,
    next: &mut Option<(String, String)>,
    kept: &mut Option<Kept>,
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

    // Read metadata tags
    let mut title = path.file_stem().and_then(|s| s.to_str()).unwrap_or("Unknown Title").to_string();
    let mut artist = "Unknown Artist".to_string();
    let mut album = "Local Audio".to_string();
    let mut replay_gain: f32 = 0.0;

    if let Some(meta) = format_reader.metadata().current() {
        for tag in &meta.media.tags {
            if let Some(ref std_tag) = tag.std {
                match std_tag {
                    StandardTag::TrackTitle(t) => title = t.to_string(),
                    StandardTag::Artist(a) => artist = a.to_string(),
                    StandardTag::Album(al) => album = al.to_string(),
                    StandardTag::ReplayGainTrackGain(g) => {
                        // e.g. "-6.50 dB"
                        replay_gain = g.trim().trim_end_matches("dB").trim().parse().unwrap_or(0.0);
                    }
                    _ => {}
                }
            }
        }
    }

    use crate::logger::log_msg;

    log_msg("INFO", format!("Playing: {} ({})", title, path.display()));
    log_msg("INFO", format!("Track Format: {} Hz · {}-bit · {} channels · Duration: {:.2}s", sample_rate, bits_per_sample, channels, duration_seconds));
    log_msg("ALSA", format!("Requesting audio output endpoint: '{}'", device_id));

    let wide = bits_per_sample >= 24;
    let resolved_device_id = device_id.to_string();
    let mut actual_rate = sample_rate;
    let mut actual_buffer: u64 = 0;
    let mut actual_period: u64 = 0;
    let mut actual_format_str = "S16_LE".to_string();
    let mut use_32bit_output = false;

    // Same device, rate, channels and sample width as the open PCM: keep it (gapless, no EBUSY).
    let reuse = match kept.take() {
        Some(k) if k.device_id == device_id && k.rate == sample_rate && k.channels == channels && k.wide == wide => Some(k),
        Some(k) => {
            if k.drain_first { let _ = k.pcm.drain(); } // let the previous track's tail finish
            None
        }
        None => None,
    };

    let pcm = if let Some(k) = reuse {
        log_msg("ALSA", format!("Gapless: reusing open PCM '{}' @ {} Hz", device_id, k.rate));
        actual_rate = k.rate;
        actual_buffer = k.buffer;
        actual_period = k.period;
        actual_format_str = k.fmt;
        use_32bit_output = k.use_32bit;
        k.pcm
    } else {
        claim_device_from_pipewire(device_id);

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

            // Bounded buffer: 500ms buffer / 100ms period (MPD-like). The default is the device
            // maximum (~2s), which is hard on USB audio and makes seeks lag.
            let _ = hwp.set_buffer_time_near(500_000, ValueOr::Nearest);
            let _ = hwp.set_period_time_near(100_000, ValueOr::Nearest);

            // Apply hardware params
            match pcm.hw_params(&hwp) {
                Ok(_) => {
                    if let Ok(curr) = pcm.hw_params_current() {
                        if let Ok(r) = curr.get_rate() {
                            actual_rate = r;
                        }
                        actual_buffer = curr.get_buffer_size().unwrap_or(0).max(0) as u64;
                        actual_period = curr.get_period_size().unwrap_or(0).max(0) as u64;
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

        pcm
    };

    let is_really_hw = resolved_device_id.starts_with("hw:") || resolved_device_id.starts_with("plughw:");
    let is_true_bit_perfect = is_really_hw && actual_rate == sample_rate;

    if is_true_bit_perfect {
        log_msg("INFO", format!(">>> ⚡ BIT-PERFECT VERIFIED: Direct HW '{}' @ {} Hz (0 resampling)", resolved_device_id, actual_rate));
    } else {
        log_msg("INFO", format!(">>> 🎛️ IN-ENGINE RESAMPLER: Converting {} Hz -> {} Hz on '{}' (1.0x pitch preserved)", sample_rate, actual_rate, resolved_device_id));
    }

    // Update status
    {
        let mut st = status.lock().unwrap_or_else(|e| e.into_inner());
        st.is_playing = true;
        st.ended = false;
        st.track_serial += 1;
        st.replay_gain_db = replay_gain;
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
        st.active_hardware_format = actual_format_str.clone();
        st.active_buffer_frames = actual_buffer;
        st.active_period_frames = actual_period;
        st.is_bit_perfect = is_true_bit_perfect;
    }

    let mut decoder = match symphonia::default::get_codecs().make_audio_decoder(audio_params, &dec_opts) {
        Ok(d) => d,
        Err(_) => return,
    };

    use rubato::{FastFixedIn, PolynomialDegree, Resampler};
    let mut resampler = if actual_rate != sample_rate {
        FastFixedIn::<f32>::new(
            actual_rate as f64 / sample_rate as f64,
            1.0,
            PolynomialDegree::Septic,
            1024,
            channels as usize,
        ).ok()
    } else {
        None
    };
    let mut channel_fifo: Vec<Vec<f32>> = vec![Vec::new(); channels as usize];

    let mut current_frame: u64 = 0;
    let mut sample_buf_f32: Vec<f32> = Vec::new();
    let mut sample_buf_16: Vec<i16> = Vec::new();
    let mut sample_buf_32: Vec<i32> = Vec::new();

    let mut reached_eof = false;
    let mut write_failed = false;

    let write_pcm_i32 = |pcm: &PCM, data: &[i32], num_frames: usize| -> Result<(), ()> {
        if let Ok(io) = pcm.io_i32() {
            let mut frames_written = 0;
            let mut retries = 0;
            while frames_written < num_frames {
                let offset = frames_written * channels as usize;
                let slice = &data[offset..];
                match io.writei(slice) {
                    Ok(w) => frames_written += w,
                    Err(e) => {
                        let msg = e.to_string();
                        retries += 1;
                        if retries > 5 || pcm.try_recover(e, true).is_err() {
                            log_msg("ERROR", format!("ALSA write failed: {msg}"));
                            return Err(());
                        }
                    }
                }
            }
            Ok(())
        } else {
            Err(())
        }
    };

    let write_pcm_i16 = |pcm: &PCM, data: &[i16], num_frames: usize| -> Result<(), ()> {
        if let Ok(io) = pcm.io_i16() {
            let mut frames_written = 0;
            let mut retries = 0;
            while frames_written < num_frames {
                let offset = frames_written * channels as usize;
                let slice = &data[offset..];
                match io.writei(slice) {
                    Ok(w) => frames_written += w,
                    Err(e) => {
                        let msg = e.to_string();
                        retries += 1;
                        if retries > 5 || pcm.try_recover(e, true).is_err() {
                            log_msg("ERROR", format!("ALSA write failed: {msg}"));
                            return Err(());
                        }
                    }
                }
            }
            Ok(())
        } else {
            Err(())
        }
    };

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
                    let _ = pcm.drop(); // discard queued audio, but keep the device open for the next track
                    let _ = pcm.prepare();
                    *kept = Some(Kept { pcm, device_id: device_id.to_string(), rate: actual_rate, channels, wide,
                        use_32bit: use_32bit_output, fmt: actual_format_str.clone(), buffer: actual_buffer,
                        period: actual_period, drain_first: false });
                    return;
                }
                EngineCommand::Pause => {
                    if let Ok(mut st) = status.lock() {
                        st.is_paused = true;
                        st.vu_left = 0.0;
                        st.vu_right = 0.0;
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
                                    let _ = pcm.drop(); // discard queued audio, but keep the device open for the next track
                                    let _ = pcm.prepare();
                                    *kept = Some(Kept { pcm, device_id: device_id.to_string(), rate: actual_rate, channels, wide,
                                        use_32bit: use_32bit_output, fmt: actual_format_str.clone(), buffer: actual_buffer,
                                        period: actual_period, drain_first: false });
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
                    if let Some(ref mut resamp) = resampler {
                        resamp.reset();
                        for ch in channel_fifo.iter_mut() {
                            ch.clear();
                        }
                    }
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
            // some demuxers report end of stream as an I/O error: still a natural end
            Err(SymphoniaError::IoError(_)) => { reached_eof = true; break; }
            Err(_) => break,
        };

        if packet.track_id != track_id {
            continue;
        }

        match decoder.decode(&packet) {
            Ok(audio_buf) => {
                let num_samples = audio_buf.samples_interleaved();
                let num_frames = num_samples / (channels as usize);

                if let Some(ref mut resamp) = resampler {
                    // In-Engine Resampling Path (maintains 1.0x speed and original pitch)
                    sample_buf_f32.resize(num_samples, 0.0f32);
                    audio_buf.copy_to_slice_interleaved(&mut sample_buf_f32);
                    let ch_count = channels as usize;
                    for (i, &s) in sample_buf_f32.iter().enumerate() {
                        channel_fifo[i % ch_count].push(s);
                    }

                    while channel_fifo[0].len() >= resamp.input_frames_next() {
                        let needed = resamp.input_frames_next();
                        let in_chunk: Vec<Vec<f32>> = (0..ch_count)
                            .map(|c| channel_fifo[c].drain(0..needed).collect())
                            .collect();
                        if let Ok(out_channels) = resamp.process(&in_chunk, None) {
                            let out_frames = out_channels[0].len();
                            if use_32bit_output {
                                sample_buf_32.clear();
                                sample_buf_32.reserve(out_frames * ch_count);
                                for f in 0..out_frames {
                                    for c in 0..ch_count {
                                        let s = out_channels[c][f];
                                        sample_buf_32.push((s * 2147483647.0).clamp(-2147483648.0, 2147483647.0) as i32);
                                    }
                                }
                                if write_pcm_i32(&pcm, &sample_buf_32, out_frames).is_err() {
                                    write_failed = true;
                                    break;
                                }
                            } else {
                                sample_buf_16.clear();
                                sample_buf_16.reserve(out_frames * ch_count);
                                for f in 0..out_frames {
                                    for c in 0..ch_count {
                                        let s = out_channels[c][f];
                                        sample_buf_16.push((s * 32767.0).clamp(-32768.0, 32767.0) as i16);
                                    }
                                }
                                if write_pcm_i16(&pcm, &sample_buf_16, out_frames).is_err() {
                                    write_failed = true;
                                    break;
                                }
                            }
                        }
                    }
                } else {
                    // True Bit-Perfect Pass-Through Path (0 alterations, bit-exact)
                    if use_32bit_output {
                        sample_buf_32.resize(num_samples, 0i32);
                        audio_buf.copy_to_slice_interleaved(&mut sample_buf_32);
                        if write_pcm_i32(&pcm, &sample_buf_32, num_frames).is_err() {
                            write_failed = true;
                        }
                    } else {
                        sample_buf_16.resize(num_samples, 0i16);
                        audio_buf.copy_to_slice_interleaved(&mut sample_buf_16);
                        if write_pcm_i16(&pcm, &sample_buf_16, num_frames).is_err() {
                            write_failed = true;
                        }
                    }
                }

                if write_failed { break; }

                current_frame += num_frames as u64;

                let (vu_l, vu_r) = if use_32bit_output {
                    let mut max_l: u32 = 0;
                    let mut max_r: u32 = 0;
                    for chunk in sample_buf_32.chunks_exact(channels as usize) {
                        let l = chunk[0].unsigned_abs();
                        if l > max_l { max_l = l; }
                        if channels > 1 {
                            let r = chunk[1].unsigned_abs();
                            if r > max_r { max_r = r; }
                        } else {
                            max_r = max_l;
                        }
                    }
                    (
                        (max_l as f32 / 2147483648.0).clamp(0.0, 1.0),
                        (max_r as f32 / 2147483648.0).clamp(0.0, 1.0),
                    )
                } else {
                    let mut max_l: u16 = 0;
                    let mut max_r: u16 = 0;
                    for chunk in sample_buf_16.chunks_exact(channels as usize) {
                        let l = chunk[0].unsigned_abs();
                        if l > max_l { max_l = l; }
                        if channels > 1 {
                            let r = chunk[1].unsigned_abs();
                            if r > max_r { max_r = r; }
                        } else {
                            max_r = max_l;
                        }
                    }
                    (
                        (max_l as f32 / 32768.0).clamp(0.0, 1.0),
                        (max_r as f32 / 32768.0).clamp(0.0, 1.0),
                    )
                };

                // Report what is actually audible: frames written minus frames still queued in the DAC buffer
                let queued = pcm.delay().unwrap_or(0).max(0) as u64;
                if let Ok(mut st) = status.lock() {
                    let queued_src = (queued as f64 * sample_rate as f64 / actual_rate as f64) as u64;
                    st.position_seconds = current_frame.saturating_sub(queued_src) as f64 / sample_rate as f64;
                    // peak since the UI last looked, so short transients are not missed between polls
                    st.vu_left = st.vu_left.max(vu_l);
                    st.vu_right = st.vu_right.max(vu_r);
                }
            }
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(_) => break,
        }
    }

    if reached_eof && !write_failed {
        if let Some(ref mut resamp) = resampler {
            if !channel_fifo[0].is_empty() {
                if let Ok(out_channels) = resamp.process_partial(Some(&channel_fifo), None) {
                    let out_frames = out_channels[0].len();
                    let ch_count = channels as usize;
                    if use_32bit_output {
                        sample_buf_32.clear();
                        sample_buf_32.reserve(out_frames * ch_count);
                        for f in 0..out_frames {
                            for c in 0..ch_count {
                                let s = out_channels[c][f];
                                sample_buf_32.push((s * 2147483647.0).clamp(-2147483648.0, 2147483647.0) as i32);
                            }
                        }
                        let _ = write_pcm_i32(&pcm, &sample_buf_32, out_frames);
                    } else {
                        sample_buf_16.clear();
                        sample_buf_16.reserve(out_frames * ch_count);
                        for f in 0..out_frames {
                            for c in 0..ch_count {
                                let s = out_channels[c][f];
                                sample_buf_16.push((s * 32767.0).clamp(-32768.0, 32767.0) as i16);
                            }
                        }
                        let _ = write_pcm_i16(&pcm, &sample_buf_16, out_frames);
                    }
                }
            }
        }
        // Gapless: if the UI named a next track, leave the stream running and hand the open PCM over.
        // ponytail: the title flips up to one buffer (500 ms) before the last notes are audible.
        let next_path = next_hint.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(np) = next_path {
            *next = Some((np, device_id.to_string()));
            *kept = Some(Kept { pcm, device_id: device_id.to_string(), rate: actual_rate, channels, wide,
                use_32bit: use_32bit_output, fmt: actual_format_str.clone(), buffer: actual_buffer,
                period: actual_period, drain_first: true });
            return; // still "playing": the next track sets the status
        }
        let _ = pcm.drain();
    } else {
        let _ = pcm.drop();
    }

    if let Ok(mut st) = status.lock() {
        st.ended = reached_eof;
        st.is_playing = false;
        st.is_paused = false;
        st.position_seconds = 0.0;
        st.vu_left = 0.0;
        st.vu_right = 0.0;
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

/// `gain_db` (ReplayGain) only ever lowers the volume from the slider position, and only on mixers that report dB.
pub fn set_hw_volume(device_id: &str, percent: u32, gain_db: f64) -> bool {
    with_volume_ctl(device_id, |sel, lo, hi| {
        let v = lo + (hi - lo) * percent.min(100) as i64 / 100;
        if gain_db < 0.0 {
            if let Ok(db) = sel.ask_playback_vol_db(v) {
                let target = alsa::mixer::MilliBel(db.0 + (gain_db * 100.0) as i64);
                return sel.set_playback_db_all(target, alsa::Round::Floor).ok();
            }
        }
        sel.set_playback_volume_all(v).ok()
    }).is_some()
}

pub fn get_hw_volume(device_id: &str) -> Option<u32> {
    with_volume_ctl(device_id, |sel, lo, hi| {
        let v = sel.get_playback_volume(alsa::mixer::SelemChannelId::FrontLeft).ok()?;
        Some(if hi > lo { ((v - lo) * 100 / (hi - lo)) as u32 } else { 0 })
    })
}

// ---- PipeWire hand-off -------------------------------------------------------------------
// PipeWire/WirePlumber keeps the DAC open, so a direct hw: open gets EBUSY. Suspending the
// card's sink (what `pactl suspend-sink` does) makes PipeWire release it; we resume it afterwards.
static SUSPENDED_SINKS: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn pactl(args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("pactl").args(args).env("LC_ALL", "C").output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn card_of(device_id: &str) -> Option<String> {
    let rest = device_id.strip_prefix("plughw:").or_else(|| device_id.strip_prefix("hw:"))?;
    Some(rest.split(',').next()?.to_string())
}

fn claim_device_from_pipewire(device_id: &str) {
    use crate::logger::log_msg;
    let Some(card) = card_of(device_id) else { return };
    let Some(listing) = pactl(&["list", "sinks"]) else { return }; // no pactl: nothing to do
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
                    if !held.contains(&n) { held.push(n); }
                }
            }
        }
    }
}

/// Give suspended sinks back to PipeWire (end of playback, or app exit).
pub fn release_device_to_pipewire() {
    let sinks: Vec<String> = SUSPENDED_SINKS.lock().unwrap_or_else(|e| e.into_inner()).drain(..).collect();
    for n in sinks {
        pactl(&["suspend-sink", &n, "0"]);
        crate::logger::log_msg("INFO", format!("Returned sink to PipeWire: {n}"));
    }
}

use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

use serde::{Deserialize, Serialize};
use tauri::Emitter;

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
    pub decoder_serial: u64, // bumps each time the decoder starts a track (it can be ahead of what is audible)
    pub decoding_path: Option<String>, // the file the decoder is on (can be a track ahead of what is audible)
    pub track_serial: u64, // bumps every time a track starts (also when the engine advances by itself)
    pub vu_left: f32,
    pub vu_right: f32,
    pub device_disconnected: bool, // true when USB DAC was unplugged mid-playback and awaiting reconnect
    pub signal_mode: String, // e.g. "Bit-Perfect Direct", "DoP (DSD64)", "Software Resampled"
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
            decoder_serial: 0,
            decoding_path: None,
            track_serial: 0,
            vu_left: 0.0,
            vu_right: 0.0,
            device_disconnected: false,
            signal_mode: "Idle".into(),
        }
    }
}

/// Set once at startup so the engine thread can tell the UI that something changed.
pub static APP: OnceLock<tauri::AppHandle> = OnceLock::new();

pub(crate) fn announce() {
    if let Some(app) = APP.get() {
        let _ = app.emit("player-event", ());
    }
}

/// Path of the track that should follow the current one (set by the UI).
pub(crate) type NextHint = Arc<Mutex<Option<String>>>;

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
    vis_queue: Arc<crate::visualizer::VisQueue>,
}

impl AudioEngineHandle {
    pub fn new() -> Self {
        let status = Arc::new(Mutex::new(PlaybackStatus::default()));
        let (tx, rx) = std::sync::mpsc::channel::<EngineCommand>();

        let thread_status = Arc::clone(&status);
        let next_hint: NextHint = Arc::new(Mutex::new(None));
        let thread_hint = Arc::clone(&next_hint);
        let vis_queue = crate::visualizer::VisQueue::new();
        let thread_vis = Arc::clone(&vis_queue);

        thread::Builder::new()
            .name("rex-audio-thread".into())
            .spawn(move || {
                run_audio_worker(rx, thread_status, thread_hint, thread_vis);
            })
            .expect("Failed to spawn audio engine thread");

        Self {
            status,
            command_tx: tx,
            next_hint,
            vis_queue,
        }
    }

    pub fn get_visualizer_frame(&self) -> crate::visualizer::VisFrame {
        let st = self.peek_status();
        if !st.is_playing || st.is_paused {
            return crate::visualizer::VisFrame::default();
        }
        self.vis_queue.sample_at(st.position_seconds)
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
    vis_queue: Arc<crate::visualizer::VisQueue>,
) {
    crate::pipeline::run(rx, status, next_hint_w, vis_queue);
}

pub fn set_mixer_pick(name: Option<String>) {
    crate::audio_backend::set_mixer_pick(name);
}

/// Names of the mixer controls on this card that have a playback volume.
pub fn list_volume_controls(device_id: &str) -> Vec<String> {
    crate::audio_backend::list_volume_controls(device_id)
}

/// `gain_db` (ReplayGain) only ever lowers the volume from the slider position, and only on mixers that report dB.
pub fn set_hw_volume(device_id: &str, percent: u32, gain_db: f64) -> bool {
    crate::audio_backend::set_hw_volume(device_id, percent, gain_db)
}

/// Force the DAC hardware mixer to 100% (0 dBFS unity gain) when enabling Bit-Perfect 0 dBFS Lock.
pub fn lock_hw_volume_0db(device_id: &str) -> bool {
    crate::audio_backend::lock_hw_volume_0db(device_id)
}

pub fn get_hw_volume(device_id: &str) -> Option<u32> {
    crate::audio_backend::get_hw_volume(device_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    use alsa::pcm::PCM;
    #[cfg(target_os = "linux")]
    use alsa::Direction;
    use std::sync::atomic::Ordering::Relaxed;
    use std::time::Duration;

    /// A short stereo sine as a canonical PCM WAV file (16- or 24-bit).
    fn wav_bytes(rate: u32, bits: u16, frames: u32) -> Vec<u8> {
        let ch = 2u16;
        let bytes = bits / 8;
        let data_len = frames * ch as u32 * bytes as u32;
        let mut w = Vec::new();
        w.extend_from_slice(b"RIFF");
        w.extend_from_slice(&(36 + data_len).to_le_bytes());
        w.extend_from_slice(b"WAVEfmt ");
        w.extend_from_slice(&16u32.to_le_bytes());
        w.extend_from_slice(&1u16.to_le_bytes()); // PCM
        w.extend_from_slice(&ch.to_le_bytes());
        w.extend_from_slice(&rate.to_le_bytes());
        w.extend_from_slice(&(rate * ch as u32 * bytes as u32).to_le_bytes());
        w.extend_from_slice(&(ch * bytes).to_le_bytes());
        w.extend_from_slice(&bits.to_le_bytes());
        w.extend_from_slice(b"data");
        w.extend_from_slice(&data_len.to_le_bytes());
        for n in 0..frames {
            let v = (n as f64 * 440.0 * std::f64::consts::TAU / rate as f64).sin() * 0.3;
            for _ in 0..ch {
                let s = (v * if bits == 16 { 32767.0 } else { 8_388_607.0 }) as i32;
                w.extend_from_slice(&s.to_le_bytes()[..bytes as usize]);
            }
        }
        w
    }

    #[test]
    fn wav_generator_writes_a_consistent_header() {
        let w = wav_bytes(44100, 24, 100);
        assert_eq!(&w[0..4], b"RIFF");
        assert_eq!(w.len(), 44 + 100 * 2 * 3);
        assert_eq!(u32::from_le_bytes(w[40..44].try_into().unwrap()), 600);
    }

    fn wait_for(engine: &AudioEngineHandle, secs: u64, mut ok: impl FnMut(&PlaybackStatus) -> bool) -> bool {
        for _ in 0..secs * 20 {
            if ok(&engine.peek_status()) { return true; }
            thread::sleep(Duration::from_millis(50));
        }
        false
    }

    /// Plays synthetic files through the real engine on a fake sound card. Not part of the normal run:
    ///   sudo modprobe snd-dummy && cargo test dummy_card -- --ignored --nocapture
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "needs a fake sound card: sudo modprobe snd-dummy"]
    fn dummy_card_gapless_and_rate_change() {
        let dev = "hw:Dummy,0";
        if PCM::new(dev, Direction::Playback, false).is_err() {
            eprintln!("skipped: no {dev} (run: sudo modprobe snd-dummy)");
            return;
        }
        let dir = std::env::temp_dir().join(format!("rex-dummy-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mk = |name: &str, rate, bits| {
            let p = dir.join(name);
            std::fs::write(&p, wav_bytes(rate, bits, rate)).unwrap(); // one second each
            p.to_string_lossy().to_string()
        };
        let (a, b, c) = (mk("a.wav", 44100, 16), mk("b.wav", 44100, 16), mk("c.wav", 48000, 24));

        let engine = AudioEngineHandle::new();
        let (reused0, reopened0) = (crate::diagnostics::REUSED.load(Relaxed), crate::diagnostics::REOPENED.load(Relaxed));

        // A then B: same rate and width, so the open card must carry over (gapless)
        engine.set_next(Some(b.clone()));
        engine.play(a, dev.into());
        // the decoder runs ahead of the speakers, so the UI names the track after B as soon as the decoder is on B
        assert!(wait_for(&engine, 8, |s| s.decoding_path.as_deref() == Some(b.as_str())), "decoder never reached B");
        engine.set_next(Some(c.clone()));
        assert!(wait_for(&engine, 8, |s| s.track_serial >= 2 && s.file_path.as_deref() == Some(b.as_str())), "B did not start by itself");
        assert_eq!(crate::diagnostics::REUSED.load(Relaxed), reused0 + 1, "same-format hand-off should reuse the PCM");

        // B then C: rate and width change, so the card is reopened
        assert!(wait_for(&engine, 8, |s| s.track_serial >= 3 && s.file_path.as_deref() == Some(c.as_str())), "C did not start by itself");
        assert!(crate::diagnostics::REOPENED.load(Relaxed) > reopened0 + 1, "format change should reopen the PCM");

        // C is last: it must end cleanly
        assert!(wait_for(&engine, 8, |s| s.ended && !s.is_playing), "last track did not end");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Pause, resume, seek, skip and stop through the real engine on the fake card.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "needs a fake sound card: sudo modprobe snd-dummy"]
    fn dummy_card_controls() {
        let dev = "hw:Dummy,0";
        if PCM::new(dev, Direction::Playback, false).is_err() {
            eprintln!("skipped: no {dev} (run: sudo modprobe snd-dummy)");
            return;
        }
        let dir = std::env::temp_dir().join(format!("rex-dummy-ctl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mk = |name: &str, secs: u32| {
            let p = dir.join(name);
            std::fs::write(&p, wav_bytes(44100, 16, 44100 * secs)).unwrap();
            p.to_string_lossy().to_string()
        };
        let (a, b) = (mk("a.wav", 6), mk("b.wav", 6));
        let engine = AudioEngineHandle::new();

        engine.play(a.clone(), dev.into());
        assert!(wait_for(&engine, 5, |s| s.is_playing && s.position_seconds > 0.4), "playback did not start");

        // pause: the clock must stand still, and move again after resume
        engine.pause();
        assert!(wait_for(&engine, 3, |s| s.is_paused), "pause not reported");
        thread::sleep(Duration::from_millis(400)); // let the card settle (pause lands between chunks)
        let frozen = engine.peek_status().position_seconds;
        thread::sleep(Duration::from_millis(600));
        assert!((engine.peek_status().position_seconds - frozen).abs() < 0.05, "clock moved while paused");
        engine.resume();
        assert!(wait_for(&engine, 3, |s| !s.is_paused && s.position_seconds > frozen + 0.3), "clock did not move after resume");

        // seek: the clock jumps to the new place and keeps going from there
        engine.seek(4.0);
        assert!(wait_for(&engine, 3, |s| s.position_seconds >= 3.9 && s.position_seconds < 5.5), "seek did not land near 4.0 s");
        assert_eq!(engine.peek_status().track_serial, 1, "a seek must not look like a new track");

        // skip to another file while playing: the new file shows up, the old one is gone
        engine.play(b.clone(), dev.into());
        assert!(wait_for(&engine, 5, |s| s.file_path.as_deref() == Some(b.as_str()) && s.position_seconds < 2.0), "skip did not switch tracks");
        assert_eq!(engine.peek_status().track_serial, 2);

        // stop
        engine.stop();
        assert!(wait_for(&engine, 3, |s| !s.is_playing), "stop did not stop");
        assert!(!engine.peek_status().ended, "a stop is not a natural end");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

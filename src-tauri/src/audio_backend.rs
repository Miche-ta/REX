//! Platform-agnostic audio output backend trait and dispatch.
//!
//! - Linux: `backend_alsa::AlsaBackend` (direct ALSA `hw:` / `plughw:` / `default` + PipeWire handoff)
//! - Windows: `backend_wasapi::WasapiBackend` (WASAPI Exclusive Mode)
//! - macOS: `backend_coreaudio::CoreAudioBackend` (CoreAudio HAL Hog/Exclusive Mode)

use crate::audio_probe::AudioDevice;
use crate::pcm_format::{can_reuse, OutFmt};

/// What the decoder needs the audio device to look like for the next track.
#[derive(Clone, Debug)]
pub struct OpenReq {
    pub device_id: String,
    pub sample_rate: u32,
    pub channels: u32,
    pub bits: u32,
}

/// What the audio device actually opened with.
#[derive(Clone, Debug)]
pub struct OpenInfo {
    pub device_id: String,
    pub hw_rate: u32,
    pub channels: u32,
    pub wide: bool, // opened for a source of 24 bits or more
    pub out_fmt: OutFmt,
    pub lossless: bool, // output format is at least as wide as the source
    pub fmt: String,
    pub buffer: u64,
    pub period: u64,
}

impl OpenInfo {
    /// Can the open card carry a track with these parameters untouched?
    pub fn fits(&self, r: &OpenReq) -> bool {
        can_reuse(
            (self.device_id.as_str(), self.hw_rate, self.channels, self.wide),
            (r.device_id.as_str(), r.sample_rate, r.channels, r.bits >= 24),
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteErr {
    Disconnected,
    Fatal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeResult {
    Ready,
    Busy,
    Missing,
}

pub trait AudioBackend: Send {
    fn info(&self) -> &OpenInfo;
    fn write_i16(&mut self, data: &[i16], frames: usize) -> Result<(), WriteErr>;
    fn write_i32(&mut self, data: &[i32], frames: usize) -> Result<(), WriteErr>;
    fn write_bytes(&mut self, data: &[u8], frames: usize, bytes_per_frame: usize) -> Result<(), WriteErr>;
    fn delay_frames(&self) -> u64;
    fn pause(&mut self, paused: bool);
    fn drain(&mut self);
    fn flush_and_prepare(&mut self);
    fn close(&mut self);
}

pub fn open_backend(req: &OpenReq) -> Result<Box<dyn AudioBackend>, ()> {
    #[cfg(target_os = "linux")]
    {
        crate::backend_alsa::AlsaBackend::open(req).map(|b| Box::new(b) as Box<dyn AudioBackend>)
    }
    #[cfg(target_os = "windows")]
    {
        crate::backend_wasapi::WasapiBackend::open(req).map(|b| Box::new(b) as Box<dyn AudioBackend>)
    }
    #[cfg(target_os = "macos")]
    {
        crate::backend_coreaudio::CoreAudioBackend::open(req).map(|b| Box::new(b) as Box<dyn AudioBackend>)
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        let _ = req;
        Err(())
    }
}

pub fn probe_device(device_id: &str) -> ProbeResult {
    #[cfg(target_os = "linux")]
    {
        crate::backend_alsa::probe_device(device_id)
    }
    #[cfg(target_os = "windows")]
    {
        crate::backend_wasapi::probe_device(device_id)
    }
    #[cfg(target_os = "macos")]
    {
        crate::backend_coreaudio::probe_device(device_id)
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        let _ = device_id;
        ProbeResult::Missing
    }
}

pub fn release_all() {
    #[cfg(target_os = "linux")]
    crate::backend_alsa::release_device_to_pipewire();
}

pub fn list_devices() -> Vec<AudioDevice> {
    #[cfg(target_os = "linux")]
    {
        crate::backend_alsa::list_devices()
    }
    #[cfg(target_os = "windows")]
    {
        crate::backend_wasapi::list_devices()
    }
    #[cfg(target_os = "macos")]
    {
        crate::backend_coreaudio::list_devices()
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        Vec::new()
    }
}

pub fn list_volume_controls(device_id: &str) -> Vec<String> {
    #[cfg(target_os = "linux")]
    {
        crate::backend_alsa::list_volume_controls(device_id)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = device_id;
        Vec::new()
    }
}

pub fn set_mixer_pick(name: Option<String>) {
    #[cfg(target_os = "linux")]
    crate::backend_alsa::set_mixer_pick(name);
    #[cfg(not(target_os = "linux"))]
    let _ = name;
}

pub fn set_hw_volume(device_id: &str, percent: u32, gain_db: f64) -> bool {
    if crate::settings::get().bit_perfect_lock {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        crate::backend_alsa::apply_hw_volume(device_id, percent, gain_db)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (device_id, percent, gain_db);
        false
    }
}

pub fn lock_hw_volume_0db(device_id: &str) -> bool {
    #[cfg(target_os = "linux")]
    {
        crate::backend_alsa::apply_hw_volume(device_id, 100, 0.0)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = device_id;
        false
    }
}

pub fn get_hw_volume(device_id: &str) -> Option<u32> {
    #[cfg(target_os = "linux")]
    {
        crate::backend_alsa::get_hw_volume(device_id)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = device_id;
        None
    }
}

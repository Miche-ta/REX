use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioDevice {
    pub id: String,                  // e.g. "hw:2,0", "wasapi:...", "coreaudio:...", "default"
    pub name: String,                // Friendly name or card name
    pub description: String,         // Backend description
    pub is_hardware: bool,           // true if direct hardware / exclusive mode (bit-perfect capable)
    pub is_usb_dac: bool,            // true if external USB DAC with clock switching
    pub min_rate: u32,
    pub max_rate: u32,
    pub supported_rates: Vec<u32>,   // [44100, 48000, 96000, 192000, ...]
    pub supports_16bit: bool,
    pub supports_24bit: bool,
    pub supports_32bit: bool,
}

pub fn get_audio_devices() -> Vec<AudioDevice> {
    crate::audio_backend::list_devices()
}

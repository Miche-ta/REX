use alsa::card::Iter as CardIter;
use alsa::device_name::HintIter;
use alsa::Direction;
use serde::{Deserialize, Serialize};
use std::fs;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioDevice {
    pub id: String,                  // e.g. "hw:2,0", "plughw:2,0", "default"
    pub name: String,                // Friendly name or card name
    pub description: String,         // ALSA description
    pub is_hardware: bool,           // true if direct hardware (bit-perfect capable)
    pub min_rate: u32,
    pub max_rate: u32,
    pub supported_rates: Vec<u32>,   // [44100, 48000, 96000, 192000, ...]
    pub supports_16bit: bool,
    pub supports_24bit: bool,
    pub supports_32bit: bool,
}

pub fn get_audio_devices() -> Vec<AudioDevice> {
    let mut devices = Vec::new();
    let audiophile_rates = [44100, 48000, 88200, 96000, 176400, 192000, 352800, 384000];

    // 1. Direct Physical Soundcard Enumeration (Direct Hardware / Zero Resampling)
    for card_res in CardIter::new() {
        if let Ok(card) = card_res {
            let card_idx = card.get_index();
            let card_name = card.get_name().unwrap_or_else(|_| format!("Card {}", card_idx));
            let card_long = card.get_longname().unwrap_or_default();

            // Ignore HDMI / pure capture cards if possible
            let is_usb_dac = card_long.to_lowercase().contains("usb") || card_name.to_lowercase().contains("audio");

            // Read hardware supported rates directly from kernel /proc/asound/cardX/stream0 (for USB DACs)
            let mut hw_rates = Vec::new();
            let stream_path = format!("/proc/asound/card{}/stream0", card_idx);
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
                hw_rates = vec![44100, 48000, 96000, 192000];
            }

            let min_rate = *hw_rates.first().unwrap_or(&44100);
            let max_rate = *hw_rates.last().unwrap_or(&192000);

            // Add Direct ALSA Hardware (hw:X,0) - Pure Bit-Perfect
            let hw_id = format!("hw:{},0", card_idx);
            devices.push(AudioDevice {
                id: hw_id.clone(),
                name: format!("{} (Direct ALSA hw:{},0)", card_name, card_idx),
                description: format!("{} · Exclusive Direct Hardware (Zero Resampling)", card_long),
                is_hardware: true,
                min_rate,
                max_rate,
                supported_rates: hw_rates.clone(),
                supports_16bit: true,
                supports_24bit: true,
                supports_32bit: true,
            });

            // Add ALSA Plug Hardware (plughw:X,0) - Hardware with auto-format negotiation
            let plughw_id = format!("plughw:{},0", card_idx);
            devices.push(AudioDevice {
                id: plughw_id,
                name: format!("{} (ALSA Plug plughw:{},0)", card_name, card_idx),
                description: format!("{} · Hardware with automatic format negotiation", card_long),
                is_hardware: true,
                min_rate,
                max_rate,
                supported_rates: hw_rates,
                supports_16bit: true,
                supports_24bit: true,
                supports_32bit: true,
            });
        }
    }

    // 2. Add System Default / PipeWire (with dynamic sample-rate switching support)
    devices.push(AudioDevice {
        id: "default".into(),
        name: "PipeWire Media Server (Dynamic Rate Enabled)".into(),
        description: "Shared audio with dynamic sample-rate clock switching".into(),
        is_hardware: false,
        min_rate: 44100,
        max_rate: 192000,
        supported_rates: vec![44100, 48000, 96000, 192000],
        supports_16bit: true,
        supports_24bit: true,
        supports_32bit: true,
    });

    // 3. Scan other hints (e.g. front:CARD=Audio,DEV=0)
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

//! Playback health counters. Plain atomics: the audio thread only does a relaxed add, no locks or allocation.
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};
use std::time::Duration;

pub static XRUNS: AtomicU32 = AtomicU32::new(0); // underruns reported by ALSA (EPIPE)
pub static RECOVERED: AtomicU32 = AtomicU32::new(0); // write errors that try_recover fixed
pub static WRITE_FAILS: AtomicU32 = AtomicU32::new(0); // write errors that stopped playback
pub static REUSED: AtomicU32 = AtomicU32::new(0); // tracks that kept the open PCM (gapless)
pub static REOPENED: AtomicU32 = AtomicU32::new(0); // tracks that had to open the PCM
pub static MAX_DECODE_US: AtomicU64 = AtomicU64::new(0); // slowest single packet decode

pub fn note_decode(d: Duration) {
    MAX_DECODE_US.fetch_max(d.as_micros() as u64, Relaxed);
}

pub fn counters_text() -> String {
    format!(
        "underruns (xrun): {}\nwrite errors recovered: {}\nwrite errors that stopped playback: {}\n\
         tracks that reused the open device: {}\ntracks that reopened the device: {}\nslowest packet decode: {:.2} ms\n",
        XRUNS.load(Relaxed),
        RECOVERED.load(Relaxed),
        WRITE_FAILS.load(Relaxed),
        REUSED.load(Relaxed),
        REOPENED.load(Relaxed),
        MAX_DECODE_US.load(Relaxed) as f64 / 1000.0,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_the_slowest_decode_only() {
        note_decode(Duration::from_micros(300));
        note_decode(Duration::from_micros(1200));
        note_decode(Duration::from_micros(500));
        assert!(MAX_DECODE_US.load(Relaxed) >= 1200);
        assert!(counters_text().contains("slowest packet decode"));
    }
}

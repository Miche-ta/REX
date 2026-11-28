//! Sample-format negotiation and packing for audio hardware output. Pure functions, unit-tested.
#[cfg(target_os = "linux")]
use alsa::pcm::Format;

/// How samples are laid out on the wire to the DAC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutFmt {
    S16,    // 16-bit
    S32,    // 32-bit container, sample left-aligned (what the decoder produces)
    S24Low, // 4-byte container, 24-bit value in the low bits (S24_LE)
    S243,   // 3 bytes per sample (S24_3LE)
}

impl OutFmt {
    #[cfg(target_os = "linux")]
    pub fn alsa(self) -> Format {
        match self {
            OutFmt::S16 => Format::s16(),
            OutFmt::S32 => Format::s32(),
            OutFmt::S24Low => Format::s24(),
            OutFmt::S243 => Format::s24_3(),
        }
    }

    /// Formats fed from the decoder's i32 buffer (everything except plain 16-bit).
    pub fn uses_i32(self) -> bool {
        self != OutFmt::S16
    }

    pub fn label(self, source_bits: u32, lossless: bool) -> String {
        let name = match self {
            OutFmt::S16 => "S16_LE",
            OutFmt::S32 => "S32_LE",
            OutFmt::S24Low => "S24_LE",
            OutFmt::S243 => "S24_3LE",
        };
        let note = if !lossless {
            format!("Truncated to {}", if self == OutFmt::S16 { 16 } else { 24 })
        } else if self != OutFmt::S16 && source_bits <= 16 {
            "16-bit padded".to_string()
        } else {
            format!("Bit-Exact {}", source_bits)
        };
        format!("{name} ({note})")
    }
}

/// Pick the first format wide enough to carry the source untouched. The bool is false when the
/// only option is narrower than the source (the stream is then no longer bit-perfect).
pub fn pick_format(source_bits: u32, supported: impl Fn(OutFmt) -> bool) -> (OutFmt, bool) {
    let wide = source_bits > 16;
    let order: &[OutFmt] = if wide {
        &[OutFmt::S32, OutFmt::S24Low, OutFmt::S243]
    } else {
        &[OutFmt::S16, OutFmt::S32, OutFmt::S24Low, OutFmt::S243]
    };
    for &f in order {
        // the 3-byte packer below writes little-endian
        if f == OutFmt::S243 && !cfg!(target_endian = "little") { continue; }
        if supported(f) { return (f, true); }
    }
    (OutFmt::S16, !wide)
}

/// Rates to try when a device refuses the track's own rate, best first: the rate itself, then lower
/// rates of the same family (192k -> 96k -> 48k: integer ratios resample cleanly), then the other
/// family from the top. Backends without a "nearest rate" call (WASAPI) walk this list.
#[cfg_attr(not(windows), allow(dead_code))] // only WASAPI walks it
pub fn fallback_rates(rate: u32) -> Vec<u32> {
    const RATES: [u32; 10] = [768000, 705600, 384000, 352800, 192000, 176400, 96000, 88200, 48000, 44100];
    let family = |r: u32| r % 44100 == 0;
    let mut out = vec![rate];
    out.extend(RATES.iter().copied().filter(|&r| r < rate && family(r) == family(rate)));
    out.extend(RATES.iter().copied().filter(|&r| r < rate && family(r) != family(rate)));
    out.extend(RATES.iter().copied().filter(|&r| r > rate).rev()); // only above as a last resort
    out.dedup();
    out
}

/// Can the PCM that is already open carry the next track as it is? Same device, rate, channel count
/// and sample width: yes. Anything else needs a reopen so the DAC gets the right parameters.
pub fn can_reuse(open: (&str, u32, u32, bool), next: (&str, u32, u32, bool)) -> bool {
    open == next
}

/// i32 samples (left-aligned) -> S24_3LE bytes: the top three bytes of each sample, little-endian.
pub fn pack_s24_3(src: &[i32], out: &mut Vec<u8>) {
    out.clear();
    out.reserve(src.len() * 3);
    for v in src {
        out.extend_from_slice(&v.to_le_bytes()[1..4]);
    }
}

/// i32 samples (left-aligned) -> S24_LE values (24-bit value in the low bits, sign kept).
pub fn to_s24_low(src: &[i32], out: &mut Vec<i32>) {
    out.clear();
    out.extend(src.iter().map(|v| v >> 8));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_s32_for_24_bit() {
        let (f, ok) = pick_format(24, |_| true);
        assert_eq!((f, ok), (OutFmt::S32, true));
    }

    #[test]
    fn s24_3le_only_dac_gets_three_byte_packing() {
        let (f, ok) = pick_format(24, |f| f == OutFmt::S243 || f == OutFmt::S16);
        assert_eq!((f, ok), (OutFmt::S243, true));
    }

    #[test]
    fn s24_le_only_dac() {
        let (f, ok) = pick_format(24, |f| f == OutFmt::S24Low || f == OutFmt::S16);
        assert_eq!((f, ok), (OutFmt::S24Low, true));
    }

    #[test]
    fn truncating_24_bit_to_16_is_not_lossless() {
        let (f, ok) = pick_format(24, |f| f == OutFmt::S16);
        assert_eq!((f, ok), (OutFmt::S16, false));
    }

    #[test]
    fn sixteen_bit_prefers_native_and_pads_otherwise() {
        assert_eq!(pick_format(16, |_| true), (OutFmt::S16, true));
        assert_eq!(pick_format(16, |f| f == OutFmt::S32), (OutFmt::S32, true));
        assert_eq!(pick_format(16, |f| f == OutFmt::S243), (OutFmt::S243, true));
    }

    #[test]
    fn reuse_needs_every_parameter_to_match() {
        let open = ("hw:1,0", 44100, 2, false);
        assert!(can_reuse(open, ("hw:1,0", 44100, 2, false)));
        assert!(!can_reuse(open, ("hw:1,0", 96000, 2, false))); // rate change
        assert!(!can_reuse(open, ("hw:1,0", 44100, 1, false))); // mono
        assert!(!can_reuse(open, ("hw:1,0", 44100, 2, true))); // 16 -> 24 bit
        assert!(!can_reuse(open, ("hw:2,0", 44100, 2, false))); // other device
    }

    #[test]
    fn packs_s24_3le_little_endian() {
        // left-aligned 24-bit 0x123456 -> i32 0x12345600 -> bytes 56 34 12
        let mut out = Vec::new();
        pack_s24_3(&[0x1234_5600, -0x0100_0000], &mut out);
        assert_eq!(out, vec![0x56, 0x34, 0x12, 0x00, 0x00, 0xFF]);
    }

    #[test]
    fn s24_low_keeps_sign() {
        let mut out = Vec::new();
        to_s24_low(&[0x1234_5600, i32::MIN, i32::MAX & !0xFF], &mut out);
        assert_eq!(out, vec![0x0012_3456, -0x80_0000, 0x7F_FFFF]);
    }

    #[test]
    fn packed_length_is_three_bytes_per_sample() {
        let mut out = Vec::new();
        pack_s24_3(&[0; 8], &mut out);
        assert_eq!(out.len(), 24);
    }

    #[test]
    fn fallback_prefers_same_family_below_then_the_other_family() {
        assert_eq!(&fallback_rates(192000)[..4], &[192000, 96000, 48000, 176400]);
        assert_eq!(&fallback_rates(88200)[..3], &[88200, 44100, 48000]);
        assert_eq!(fallback_rates(44100).last(), Some(&768000));
        let all = fallback_rates(96000);
        assert_eq!(all.len(), 10, "every standard rate once");
    }
}

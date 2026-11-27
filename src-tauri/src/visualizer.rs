//! Latency-compensated audio visualizer (32-band FFT spectrum, goniometer phase scope, RMS & Peak dB).
//!
//! Computed on the decoder thread so the real-time audio writer thread is never slowed down.
//! Frames are tagged with their source position in seconds and matched against the audible
//! DAC clock when queried by the UI, keeping visuals synced with what is heard.

use std::collections::VecDeque;
use std::f32::consts::TAU;
use std::sync::{Arc, Mutex};

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};
use serde::Serialize;

const FFT_SIZE: usize = 1024;
const BANDS: usize = 32;
const SCOPE_POINTS: usize = 64;
const MAX_QUEUED_FRAMES: usize = 120; // ~4 seconds at 30 fps

#[derive(Clone, Debug, Serialize)]
pub struct VisFrame {
    pub bands: Vec<f32>,          // 32 normalized [0.0..1.0] logarithmic frequency bands
    pub scope: Vec<(f32, f32)>,   // 64 normalized (L, R) pairs for goniometer
    pub rms_l: f32,
    pub rms_r: f32,
    pub peak_l_db: f32,
    pub peak_r_db: f32,
    pub correlation: f32,         // -1.0 (out of phase) .. +1.0 (mono/in-phase)
    pub pos_secs: f64,
}

impl Default for VisFrame {
    fn default() -> Self {
        Self {
            bands: vec![0.0; BANDS],
            scope: vec![(0.0, 0.0); SCOPE_POINTS],
            rms_l: 0.0,
            rms_r: 0.0,
            peak_l_db: -60.0,
            peak_r_db: -60.0,
            correlation: 1.0,
            pos_secs: 0.0,
        }
    }
}

pub struct VisQueue {
    frames: Mutex<VecDeque<VisFrame>>,
}

impl VisQueue {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            frames: Mutex::new(VecDeque::with_capacity(MAX_QUEUED_FRAMES)),
        })
    }

    pub fn push(&self, f: VisFrame) {
        let mut g = self.frames.lock().unwrap_or_else(|e| e.into_inner());
        if g.len() >= MAX_QUEUED_FRAMES {
            g.pop_front();
        }
        g.push_back(f);
    }

    pub fn clear(&self) {
        self.frames.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// Return the frame closest to the currently audible DAC position, discarding older ones.
    pub fn sample_at(&self, audible_pos: f64) -> VisFrame {
        let mut g = self.frames.lock().unwrap_or_else(|e| e.into_inner());
        while g.len() > 1 {
            let next_pos = g.get(1).map_or(f64::MAX, |f| f.pos_secs);
            // If the second frame is still <= audible_pos (and not from a seek/new track jump), advance.
            if next_pos <= audible_pos && (audible_pos - next_pos) < 5.0 {
                g.pop_front();
            } else {
                break;
            }
        }
        g.front().cloned().unwrap_or_default()
    }
}

pub struct Analyzer {
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    ring_l: Vec<f32>,
    ring_r: Vec<f32>,
    fft_buf: Vec<Complex<f32>>,
    samples_since_emit: usize,
    emit_interval: usize,
    sample_rate: u32,
}

impl Analyzer {
    pub fn new(sample_rate: u32) -> Self {
        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(FFT_SIZE);
        let window: Vec<f32> = (0..FFT_SIZE)
            .map(|i| 0.5 * (1.0 - (TAU * i as f32 / (FFT_SIZE - 1) as f32).cos()))
            .collect();
        let emit_interval = (sample_rate / 30).max(512) as usize;
        Self {
            fft,
            window,
            ring_l: Vec::with_capacity(FFT_SIZE * 2),
            ring_r: Vec::with_capacity(FFT_SIZE * 2),
            fft_buf: vec![Complex::new(0.0, 0.0); FFT_SIZE],
            samples_since_emit: 0,
            emit_interval,
            sample_rate: sample_rate.max(8000),
        }
    }

    /// Feed interleaved `i32` samples from the decoder and optionally emit a `VisFrame`.
    pub fn feed_i32(&mut self, interleaved: &[i32], channels: usize, pos_secs: f64, queue: &VisQueue) {
        let ch = channels.max(1);
        let frames = interleaved.len() / ch;
        for f in 0..frames {
            let l = interleaved[f * ch] as f32 / 2147483648.0;
            let r = if ch > 1 { interleaved[f * ch + 1] as f32 / 2147483648.0 } else { l };
            self.ring_l.push(l);
            self.ring_r.push(r);
        }
        self.samples_since_emit += frames;
        if self.ring_l.len() > FFT_SIZE * 2 {
            let excess = self.ring_l.len() - FFT_SIZE;
            self.ring_l.drain(0..excess);
            self.ring_r.drain(0..excess);
        }
        if self.samples_since_emit >= self.emit_interval && self.ring_l.len() >= FFT_SIZE {
            self.samples_since_emit = 0;
            let frame = self.compute(pos_secs);
            queue.push(frame);
        }
    }

    /// Feed interleaved `i16` samples from the decoder and optionally emit a `VisFrame`.
    pub fn feed_i16(&mut self, interleaved: &[i16], channels: usize, pos_secs: f64, queue: &VisQueue) {
        let ch = channels.max(1);
        let frames = interleaved.len() / ch;
        for f in 0..frames {
            let l = interleaved[f * ch] as f32 / 32768.0;
            let r = if ch > 1 { interleaved[f * ch + 1] as f32 / 32768.0 } else { l };
            self.ring_l.push(l);
            self.ring_r.push(r);
        }
        self.samples_since_emit += frames;
        if self.ring_l.len() > FFT_SIZE * 2 {
            let excess = self.ring_l.len() - FFT_SIZE;
            self.ring_l.drain(0..excess);
            self.ring_r.drain(0..excess);
        }
        if self.samples_since_emit >= self.emit_interval && self.ring_l.len() >= FFT_SIZE {
            self.samples_since_emit = 0;
            let frame = self.compute(pos_secs);
            queue.push(frame);
        }
    }

    fn compute(&mut self, pos_secs: f64) -> VisFrame {
        let n = self.ring_l.len();
        let l_slice = &self.ring_l[n - FFT_SIZE..n];
        let r_slice = &self.ring_r[n - FFT_SIZE..n];

        let (mut sum_l2, mut sum_r2, mut sum_lr, mut peak_l, mut peak_r) = (0.0f32, 0.0f32, 0.0f32, 0.0f32, 0.0f32);
        for i in 0..FFT_SIZE {
            let l = l_slice[i];
            let r = r_slice[i];
            sum_l2 += l * l;
            sum_r2 += r * r;
            sum_lr += l * r;
            peak_l = peak_l.max(l.abs());
            peak_r = peak_r.max(r.abs());
            let mono = 0.5 * (l + r);
            self.fft_buf[i] = Complex::new(mono * self.window[i], 0.0);
        }

        self.fft.process(&mut self.fft_buf);

        let rms_l = (sum_l2 / FFT_SIZE as f32).sqrt();
        let rms_r = (sum_r2 / FFT_SIZE as f32).sqrt();
        let denom = (sum_l2 * sum_r2).sqrt();
        let correlation = if denom > 1e-12 { (sum_lr / denom).clamp(-1.0, 1.0) } else { 1.0 };
        let to_db = |p: f32| if p > 1e-6 { 20.0 * p.log10() } else { -60.0 };

        // 32 logarithmic bands from 25 Hz to min(20 kHz, Nyquist)
        let nyquist = (self.sample_rate as f32 * 0.5).min(20_000.0);
        let f_min = 25.0f32;
        let ratio = (nyquist / f_min).powf(1.0 / BANDS as f32);
        let bin_hz = self.sample_rate as f32 / FFT_SIZE as f32;
        let half_bins = FFT_SIZE / 2;

        let mut bands = vec![0.0f32; BANDS];
        for (b, slot) in bands.iter_mut().enumerate() {
            let f0 = f_min * ratio.powi(b as i32);
            let f1 = f0 * ratio;
            let bin0 = ((f0 / bin_hz).floor() as usize).clamp(1, half_bins - 1);
            let bin1 = ((f1 / bin_hz).ceil() as usize).clamp(bin0 + 1, half_bins);
            let mut max_mag = 0.0f32;
            for k in bin0..bin1 {
                let mag = self.fft_buf[k].norm() / (FFT_SIZE as f32 * 0.25);
                max_mag = max_mag.max(mag);
            }
            let db = if max_mag > 1e-6 { 20.0 * max_mag.log10() } else { -60.0 };
            *slot = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
        }

        let step = FFT_SIZE / SCOPE_POINTS;
        let scope: Vec<(f32, f32)> = (0..SCOPE_POINTS)
            .map(|i| (l_slice[i * step], r_slice[i * step]))
            .collect();

        VisFrame {
            bands,
            scope,
            rms_l,
            rms_r,
            peak_l_db: to_db(peak_l),
            peak_r_db: to_db(peak_r),
            correlation,
            pos_secs,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analyzer_detects_1khz_sine_peak_in_expected_band() {
        let rate = 48_000u32;
        let mut analyzer = Analyzer::new(rate);
        let q = VisQueue::new();
        let frames = 2048usize;
        let mut samples = Vec::with_capacity(frames * 2);
        for n in 0..frames {
            let v = ((n as f32 * 1000.0 * TAU / rate as f32).sin() * 0.5 * 2_147_483_647.0) as i32;
            samples.push(v);
            samples.push(v);
        }
        analyzer.feed_i32(&samples, 2, 0.5, &q);
        let frame = q.sample_at(0.5);
        assert_eq!(frame.bands.len(), 32);
        assert!(frame.correlation > 0.99, "mono sine should have correlation ~1.0");
        let peak_band = frame.bands.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap().0;
        assert!((15..=20).contains(&peak_band), "1kHz should peak in mid bands, got {peak_band}");
    }

    #[test]
    fn vis_queue_compensates_latency_by_selecting_audible_position() {
        let q = VisQueue::new();
        for i in 0..5 {
            let mut f = VisFrame::default();
            f.pos_secs = i as f64 * 0.1;
            q.push(f);
        }
        let picked = q.sample_at(0.25);
        assert!((picked.pos_secs - 0.2).abs() < 1e-6);
    }
}

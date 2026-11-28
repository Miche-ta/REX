//! Two-thread audio pipeline.
//!
//! * The **decoder** thread reads engine commands, opens and decodes files (resampling when the device
//!   cannot match the file's rate) and queues the result up to ~1 s ahead. At the end of a file it moves
//!   straight on to the next one, so a slow disk or big cover art never stalls the sound card.
//! * The **writer** thread owns the ALSA device. It only writes samples, keeps the clock, the VU peaks and
//!   the "now playing" status, and switches that status when a new track becomes *audible*.
//!
//! One ordered queue connects them. Seeks and skips clear it, so no stale audio is ever played.
use std::collections::VecDeque;
use std::fs::File;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use rubato::{FastFixedIn, PolynomialDegree, Resampler};
use symphonia::core::audio::GenericAudioBufferRef;
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, StandardTag};
use symphonia::core::units::Time;

use crate::audio_backend::{open_backend, probe_device, AudioBackend, OpenInfo, OpenReq, ProbeResult, WriteErr};
use crate::audio_engine::{announce, EngineCommand, NextHint, PlaybackStatus};
use crate::diagnostics;
use crate::logger::log_msg;
use crate::pcm_format::{pack_s24_3, to_s24_low, OutFmt};
use crate::ring_buffer::SpscRing;

/// How long the sound card may sit idle (nothing queued, nothing playing) before it is handed back to PipeWire.
const IDLE_CLOSE: Duration = Duration::from_millis(1500);

// ---------------------------------------------------------------------------------------------
// Messages and the queue between the two threads
// ---------------------------------------------------------------------------------------------

/// Capacity of the lock-free SPSC sample ring buffer (power of two, ~2.7s stereo @ 96 kHz).
const SAMPLE_RING_CAP: usize = 524_288;

/// Everything the UI shows about a track; applied to the status when the track becomes audible.
#[derive(Clone)]
pub struct TrackMeta {
    pub file_path: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub sample_rate: u32,
    pub bit_depth: u32,
    pub channels: u32,
    pub duration_seconds: f64,
    pub replay_gain_db: f32,
    pub device_id: String,
    pub hw_rate: u32,
    pub hw_format: String,
    pub buffer: u64,
    pub period: u64,
    pub bit_perfect: bool,
    pub signal_mode: String,
    pub cue_start_seconds: f64,
}

fn apply_meta(st: &mut PlaybackStatus, m: &TrackMeta, bump_serial: bool) {
    st.is_playing = true;
    st.ended = false;
    if bump_serial { st.track_serial += 1; }
    st.replay_gain_db = m.replay_gain_db;
    st.is_paused = false;
    st.duration_seconds = m.duration_seconds;
    st.file_path = Some(m.file_path.clone());
    st.title = m.title.clone();
    st.artist = m.artist.clone();
    st.album = m.album.clone();
    st.sample_rate = m.sample_rate;
    st.bit_depth = m.bit_depth;
    st.channels = m.channels;
    st.active_device_id = m.device_id.clone();
    st.active_hardware_rate = m.hw_rate;
    st.active_hardware_format = m.hw_format.clone();
    st.active_buffer_frames = m.buffer;
    st.active_period_frames = m.period;
    st.is_bit_perfect = m.bit_perfect;
    st.signal_mode = m.signal_mode.clone();
}

pub enum Samples {
    I16(Vec<i16>),
    I32(Vec<i32>),
}

pub enum Msg {
    /// Make sure the card suits the next track; the writer drains and reopens it if it does not.
    Open { req: OpenReq, reply: Option<Sender<Result<OpenInfo, ()>>> },
    /// A track starts here. `bump` is false when the same track continues (after a seek).
    Track(TrackMeta, bool),
    /// Device-rate frames. `src_start` is the source-file frame index of the first one.
    #[allow(dead_code)]
    Data { samples: Samples, frames: usize, src_start: u64 },
    /// Device-rate frames whose raw samples live in `Pipe::sample_ring` (zero-alloc path).
    RingData { sample_count: usize, frames: usize, is_i16: bool, src_start: u64 },
    /// End of the whole queue: play what is left, then finish.
    End,
    /// Throw away what is buffered in the card (seek, skip); the card stays open.
    Flush,
    /// Throw everything away and give the card back.
    Stop,
}

impl Msg {
    fn is_data(&self) -> bool { matches!(self, Msg::Data { .. } | Msg::RingData { .. }) }
    fn frames(&self) -> usize {
        match self {
            Msg::Data { frames, .. } | Msg::RingData { frames, .. } => *frames,
            _ => 0,
        }
    }
    /// Messages that must get through even while paused.
    fn is_control(&self) -> bool { matches!(self, Msg::Flush | Msg::Stop) }
}

pub enum Begin {
    /// The open card already fits; the writer will confirm it in order.
    Reuse(OpenInfo),
    /// The card must be (re)opened; the answer arrives here.
    Wait(Receiver<Result<OpenInfo, ()>>),
}

struct PipeState {
    q: VecDeque<Msg>,
    frames: usize, // audio frames queued
    open: Option<OpenInfo>,
}

pub struct Pipe {
    st: Mutex<PipeState>,
    cv: Condvar,
    /// Lock-free SPSC ring buffer carrying raw PCM samples from decoder to writer.
    pub sample_ring: Arc<SpscRing<i32>>,
    /// Set by the decoder on Pause/Resume; the writer pauses the card between chunks.
    pub paused: AtomicBool,
    /// Set by the writer when the card failed; the decoder abandons the chain.
    pub failed: AtomicBool,
    /// The writer still has a later track queued behind one that is audible (a seek would hit the wrong track).
    pub boundary_pending: AtomicBool,
}

impl Pipe {
    pub fn new() -> Arc<Pipe> {
        Arc::new(Pipe {
            st: Mutex::new(PipeState { q: VecDeque::new(), frames: 0, open: None }),
            cv: Condvar::new(),
            sample_ring: Arc::new(SpscRing::new(SAMPLE_RING_CAP)),
            paused: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            boundary_pending: AtomicBool::new(false),
        })
    }

    fn lock(&self) -> MutexGuard<'_, PipeState> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Queue a message. Audio waits (up to `wait`) while about a second is already queued; the message
    /// comes back in `Err` if it still did not fit, so the caller can look at commands and try again.
    pub fn push(&self, m: Msg, wait: Duration) -> Result<(), Msg> {
        let mut g = self.lock();
        loop {
            let cap = g.open.as_ref().map_or(48_000, |o| o.hw_rate as usize);
            if !m.is_data() || g.frames < cap { break; }
            let (g2, res) = self.cv.wait_timeout(g, wait).unwrap_or_else(|e| e.into_inner());
            g = g2;
            if res.timed_out() && g.frames >= cap { return Err(m); }
        }
        g.frames += m.frames();
        g.q.push_back(m);
        drop(g);
        self.cv.notify_all();
        Ok(())
    }

    /// Next message for the writer, in order. While paused only Flush/Stop get through.
    pub fn pop(&self, wait: Duration) -> Option<Msg> {
        let mut g = self.lock();
        loop {
            let paused = self.paused.load(Relaxed);
            let ready = g.q.front().map_or(false, |m| !paused || m.is_control());
            if ready {
                let m = g.q.pop_front().expect("front checked");
                g.frames -= m.frames();
                drop(g);
                self.cv.notify_all();
                return Some(m);
            }
            let (g2, res) = self.cv.wait_timeout(g, wait).unwrap_or_else(|e| e.into_inner());
            g = g2;
            if res.timed_out() { return None; }
        }
    }

    /// Drop everything queued, then queue Flush followed by `then`.
    pub fn flush(&self, then: Vec<Msg>) {
        let mut g = self.lock();
        g.q.clear();
        g.frames = 0;
        g.q.push_back(Msg::Flush);
        for m in then { g.frames += m.frames(); g.q.push_back(m); }
        drop(g);
        self.cv.notify_all();
    }

    /// Ask for the card to suit `req`. The Open message is always queued, so the writer never goes idle
    /// between the decision and the audio that follows.
    pub fn begin(&self, req: OpenReq) -> Begin {
        let mut g = self.lock();
        if let Some(info) = g.open.as_ref().filter(|o| o.fits(&req)).cloned() {
            g.q.push_back(Msg::Open { req, reply: None });
            drop(g);
            self.cv.notify_all();
            return Begin::Reuse(info);
        }
        let (tx, rx) = channel();
        g.q.push_back(Msg::Open { req, reply: Some(tx) });
        drop(g);
        self.cv.notify_all();
        Begin::Wait(rx)
    }

    fn set_open(&self, o: Option<OpenInfo>) {
        self.lock().open = o;
    }
}

// ---------------------------------------------------------------------------------------------
// Writing samples through the AudioBackend trait
// ---------------------------------------------------------------------------------------------

/// Write interleaved samples using pre-allocated scratch buffers (zero heap allocation in steady state).
fn write_samples_zero_alloc(
    backend: &mut dyn AudioBackend,
    i32_samples: &[i32],
    frames: usize,
    is_i16: bool,
    scratch_i16: &mut Vec<i16>,
    scratch_i32: &mut Vec<i32>,
    scratch_bytes: &mut Vec<u8>,
) -> Result<(), WriteErr> {
    let info = backend.info().clone();
    let ch = info.channels as usize;
    if is_i16 {
        scratch_i16.clear();
        scratch_i16.extend(i32_samples.iter().map(|&s| s as i16));
        return backend.write_i16(scratch_i16, frames);
    }
    match info.out_fmt {
        OutFmt::S32 => backend.write_i32(i32_samples, frames),
        OutFmt::S24Low => {
            scratch_i32.clear();
            to_s24_low(i32_samples, scratch_i32);
            backend.write_i32(scratch_i32, frames)
        }
        OutFmt::S243 => {
            scratch_bytes.clear();
            pack_s24_3(i32_samples, scratch_bytes);
            backend.write_bytes(scratch_bytes, frames, ch * 3)
        }
        OutFmt::S16 => Err(WriteErr::Fatal),
    }
}

/// Write interleaved samples in whatever layout the card was opened with (legacy Vec path).
fn write_samples(
    backend: &mut dyn AudioBackend,
    samples: &Samples,
    frames: usize,
    scratch_i32: &mut Vec<i32>,
    scratch_bytes: &mut Vec<u8>,
) -> Result<(), WriteErr> {
    let info = backend.info().clone();
    let ch = info.channels as usize;
    match samples {
        Samples::I16(v) => backend.write_i16(v, frames),
        Samples::I32(v) => match info.out_fmt {
            OutFmt::S32 => backend.write_i32(v, frames),
            OutFmt::S24Low => {
                scratch_i32.clear();
                to_s24_low(v, scratch_i32);
                backend.write_i32(scratch_i32, frames)
            }
            OutFmt::S243 => {
                scratch_bytes.clear();
                pack_s24_3(v, scratch_bytes);
                backend.write_bytes(scratch_bytes, frames, ch * 3)
            }
            OutFmt::S16 => Err(WriteErr::Fatal),
        },
    }
}

/// Loudest sample per channel, 0.0 to 1.0 (`i32::MIN` is handled: no overflow).
pub fn peaks_i32(data: &[i32], channels: usize) -> (f32, f32) {
    let (mut l, mut r) = (0u32, 0u32);
    for f in data.chunks_exact(channels.max(1)) {
        l = l.max(f[0].unsigned_abs());
        r = r.max(if channels > 1 { f[1].unsigned_abs() } else { f[0].unsigned_abs() });
    }
    ((l as f32 / 2147483648.0).min(1.0), (r as f32 / 2147483648.0).min(1.0))
}

pub fn peaks_i16(data: &[i16], channels: usize) -> (f32, f32) {
    let (mut l, mut r) = (0u16, 0u16);
    for f in data.chunks_exact(channels.max(1)) {
        l = l.max(f[0].unsigned_abs());
        r = r.max(if channels > 1 { f[1].unsigned_abs() } else { f[0].unsigned_abs() });
    }
    ((l as f32 / 32768.0).min(1.0), (r as f32 / 32768.0).min(1.0))
}

// ---------------------------------------------------------------------------------------------
// Writer thread
// ---------------------------------------------------------------------------------------------

/// Where a track sits in the card's frame count, so the clock can be derived from `delay()`.
struct Seg {
    mark: u64,   // device frames written when this track's audio began
    src0: u64,   // source frame at that point (non-zero after a seek)
    ratio: f64,  // source frames per device frame (differs from 1 only when resampling)
}

struct Entry {
    meta: TrackMeta,
    seg: Option<Seg>,
}

struct Writer {
    pipe: Arc<Pipe>,
    status: Arc<Mutex<PlaybackStatus>>,
    backend: Option<Box<dyn AudioBackend>>,
    pcm_paused: bool,
    written: u64, // device frames written since the card was opened
    active: Option<Entry>, // the track you can hear
    pending: VecDeque<Entry>, // tracks queued behind it
    new_seg: bool, // the next audio chunk starts a track segment
    idle: Instant,
    last_req: Option<OpenReq>,
    reconnect_req: Option<OpenReq>,
    last_reconnect_try: Instant,
    scratch_ring: Vec<i32>,
    scratch_i16: Vec<i16>,
    scratch_i32: Vec<i32>,
    scratch_bytes: Vec<u8>,
}

impl Writer {
    fn new(pipe: Arc<Pipe>, status: Arc<Mutex<PlaybackStatus>>) -> Self {
        Writer {
            pipe,
            status,
            backend: None,
            pcm_paused: false,
            written: 0,
            active: None,
            pending: VecDeque::new(),
            new_seg: false,
            idle: Instant::now(),
            last_req: None,
            reconnect_req: None,
            last_reconnect_try: Instant::now(),
            scratch_ring: Vec::with_capacity(8192),
            scratch_i16: Vec::with_capacity(8192),
            scratch_i32: Vec::with_capacity(8192),
            scratch_bytes: Vec::with_capacity(8192 * 3),
        }
    }

    fn with_status<R>(&self, f: impl FnOnce(&mut PlaybackStatus) -> R) -> R {
        f(&mut self.status.lock().unwrap_or_else(|e| e.into_inner()))
    }

    fn run(&mut self) {
        crate::rt_priority::promote_audio_thread();
        loop {
            self.sync_pause();
            if self.reconnect_req.is_some() && self.last_reconnect_try.elapsed() >= Duration::from_secs(1) {
                self.try_reconnect();
            }
            match self.pipe.pop(Duration::from_millis(20)) {
                Some(m) => { self.idle = Instant::now(); self.handle(m); }
                None => self.maybe_idle_close(),
            }
        }
    }

    fn sync_pause(&mut self) {
        let want = self.pipe.paused.load(Relaxed);
        if let Some(backend) = &mut self.backend {
            if want != self.pcm_paused {
                backend.pause(want);
                self.pcm_paused = want;
            }
        } else {
            self.pcm_paused = false;
        }
    }

    /// Device frames still to be played.
    fn queued(&self) -> u64 {
        self.backend.as_ref().map_or(0, |b| b.delay_frames())
    }

    fn handle(&mut self, m: Msg) {
        match m {
            Msg::Open { req, reply } => self.open(req, reply),
            Msg::Track(meta, bump) => self.track(meta, bump),
            Msg::Data { samples, frames, src_start } => self.data(samples, frames, src_start),
            Msg::RingData { sample_count, frames, is_i16, src_start } => self.ring_data(sample_count, frames, is_i16, src_start),
            Msg::End => self.end(),
            Msg::Flush => self.flush(),
            Msg::Stop => self.stop(),
        }
    }

    fn open(&mut self, req: OpenReq, reply: Option<Sender<Result<OpenInfo, ()>>>) {
        self.reconnect_req = None;
        self.with_status(|st| st.device_disconnected = false);
        self.last_req = Some(req.clone());
        if let Some(backend) = &self.backend {
            let info = backend.info();
            if info.fits(&req) {
                if let Some(r) = reply { let _ = r.send(Ok(info.clone())); }
                return;
            }
        }
        // different rate, width or device: let what is playing finish, then reopen
        if self.backend.is_some() {
            self.unpause();
            if let Some(backend) = &mut self.backend { backend.drain(); }
            self.close_pcm();
        }
        match open_backend(&req) {
            Ok(backend) => {
                let info = backend.info().clone();
                self.pipe.set_open(Some(info.clone()));
                self.backend = Some(backend);
                if let Some(r) = reply { let _ = r.send(Ok(info)); }
            }
            Err(()) => {
                self.pipe.set_open(None);
                self.with_status(|st| st.open_failures += 1);
                announce();
                if let Some(r) = reply { let _ = r.send(Err(())); }
            }
        }
    }

    fn track(&mut self, meta: TrackMeta, bump: bool) {
        // nothing of an earlier track left to hear: show this one now, otherwise when it becomes audible
        if self.active.is_none() || self.queued() == 0 {
            self.activate(Entry { meta, seg: None }, bump);
        } else {
            self.pending.push_back(Entry { meta, seg: None });
            self.pipe.boundary_pending.store(true, Relaxed);
        }
        self.new_seg = true;
    }

    fn activate(&mut self, e: Entry, bump: bool) {
        self.with_status(|st| apply_meta(st, &e.meta, bump));
        announce();
        self.active = Some(e);
        if !bump { self.pending.clear(); }
        if self.pending.is_empty() { self.pipe.boundary_pending.store(false, Relaxed); }
    }

    fn ring_data(&mut self, sample_count: usize, frames: usize, is_i16: bool, src_start: u64) {
        if self.scratch_ring.len() < sample_count {
            self.scratch_ring.resize(sample_count, 0);
        }
        let mut read = 0;
        while read < sample_count {
            let n = self.pipe.sample_ring.pop_slice(&mut self.scratch_ring[read..sample_count]);
            if n == 0 {
                // Should never happen because producer pushes before queueing RingData,
                // unless a flush/reset raced; bail cleanly.
                break;
            }
            read += n;
        }
        if read < sample_count {
            return;
        }
        let Some(backend) = &mut self.backend else { return };
        let info = backend.info().clone();
        let newest = self.pending.back_mut().or(self.active.as_mut());
        if self.new_seg {
            if let Some(e) = newest {
                let ratio = e.meta.sample_rate as f64 / info.hw_rate.max(1) as f64;
                e.seg = Some(Seg { mark: self.written, src0: src_start, ratio });
            }
            self.new_seg = false;
        }
        match write_samples_zero_alloc(
            backend.as_mut(),
            &self.scratch_ring[..sample_count],
            frames,
            is_i16,
            &mut self.scratch_i16,
            &mut self.scratch_i32,
            &mut self.scratch_bytes,
        ) {
            Ok(()) => {}
            Err(WriteErr::Disconnected) => { self.handle_disconnect(); return; }
            Err(WriteErr::Fatal) => { self.fail(); return; }
        }
        self.written += frames as u64;
        let (l, r) = if is_i16 {
            peaks_i16(&self.scratch_i16[..sample_count], info.channels as usize)
        } else {
            peaks_i32(&self.scratch_ring[..sample_count], info.channels as usize)
        };
        self.with_status(|st| { st.vu_left = st.vu_left.max(l); st.vu_right = st.vu_right.max(r); });
        self.update_clock();
    }

    fn data(&mut self, samples: Samples, frames: usize, src_start: u64) {
        let Some(backend) = &mut self.backend else { return }; // card failed or closed: drop stale audio
        let info = backend.info().clone();
        let newest = self.pending.back_mut().or(self.active.as_mut());
        if self.new_seg {
            if let Some(e) = newest {
                let ratio = e.meta.sample_rate as f64 / info.hw_rate.max(1) as f64;
                e.seg = Some(Seg { mark: self.written, src0: src_start, ratio });
            }
            self.new_seg = false;
        }
        match write_samples(backend.as_mut(), &samples, frames, &mut self.scratch_i32, &mut self.scratch_bytes) {
            Ok(()) => {}
            Err(WriteErr::Disconnected) => { self.handle_disconnect(); return; }
            Err(WriteErr::Fatal) => { self.fail(); return; }
        }
        self.written += frames as u64;
        let (l, r) = match &samples {
            Samples::I16(v) => peaks_i16(v, info.channels as usize),
            Samples::I32(v) => peaks_i32(v, info.channels as usize),
        };
        self.with_status(|st| { st.vu_left = st.vu_left.max(l); st.vu_right = st.vu_right.max(r); });
        self.update_clock();
    }

    /// What is audible now = frames written minus frames still queued in the card.
    fn update_clock(&mut self) {
        let audible = self.written.saturating_sub(self.queued());
        while self.pending.front().and_then(|e| e.seg.as_ref()).map_or(false, |s| s.mark <= audible) {
            let e = self.pending.pop_front().expect("front checked");
            self.activate(e, true); // the next track just became audible: now the UI may switch
        }
        if let Some(a) = &self.active {
            if let Some(seg) = &a.seg {
                let src = seg.src0 as f64 + audible.saturating_sub(seg.mark) as f64 * seg.ratio;
                let abs_pos = src / a.meta.sample_rate.max(1) as f64;
                let pos = (abs_pos - a.meta.cue_start_seconds).max(0.0);
                self.with_status(|st| st.position_seconds = pos);
            }
        }
    }

    fn flush(&mut self) {
        self.pipe.sample_ring.consumer_discard_all();
        self.reconnect_req = None;
        self.with_status(|st| st.device_disconnected = false);
        if let Some(backend) = &mut self.backend {
            backend.flush_and_prepare();
        }
        self.active = None;
        self.pending.clear();
        self.new_seg = false;
        self.pipe.boundary_pending.store(false, Relaxed);
    }

    fn end(&mut self) {
        self.unpause();
        if let Some(backend) = &mut self.backend { backend.drain(); }
        self.active = None;
        self.pending.clear();
        self.pipe.boundary_pending.store(false, Relaxed);
        self.with_status(|st| {
            st.ended = true;
            st.is_playing = false;
            st.is_paused = false;
            st.position_seconds = 0.0;
            st.vu_left = 0.0;
            st.vu_right = 0.0;
        });
        announce();
        // the card stays open for a moment: the next song usually needs the same one
    }

    fn stop(&mut self) {
        self.pipe.sample_ring.consumer_discard_all();
        self.reconnect_req = None;
        self.unpause();
        self.close_pcm();
        self.pipe.set_open(None);
        self.reset_status();
        announce();
    }

    fn handle_disconnect(&mut self) {
        self.close_pcm();
        self.pipe.set_open(None);
        self.pipe.paused.store(true, Relaxed);
        self.reconnect_req = self.last_req.clone();
        self.last_reconnect_try = Instant::now();
        self.with_status(|st| {
            st.device_disconnected = true;
            st.is_paused = true;
            st.vu_left = 0.0;
            st.vu_right = 0.0;
        });
        log_msg("WARN", "USB DAC disconnected — paused and waiting for reconnection");
        announce();
    }

    fn try_reconnect(&mut self) {
        self.last_reconnect_try = Instant::now();
        let Some(req) = self.reconnect_req.clone() else { return };
        if matches!(probe_device(&req.device_id), ProbeResult::Missing) {
            return; // still disconnected
        }
        if let Ok(backend) = open_backend(&req) {
            self.pipe.set_open(Some(backend.info().clone()));
            self.backend = Some(backend);
            self.reconnect_req = None;
            self.with_status(|st| st.device_disconnected = false);
            log_msg("INFO", format!("USB DAC '{}' reconnected — ready to resume", req.device_id));
            announce();
        }
    }

    fn fail(&mut self) {
        self.pipe.sample_ring.consumer_discard_all();
        self.reconnect_req = None;
        self.close_pcm();
        self.pipe.set_open(None);
        self.pipe.failed.store(true, Relaxed);
        self.pipe.flush(vec![]);
        self.reset_status();
        announce();
    }

    fn reset_status(&mut self) {
        self.active = None;
        self.pending.clear();
        self.new_seg = false;
        self.pipe.boundary_pending.store(false, Relaxed);
        self.with_status(|st| {
            st.is_playing = false;
            st.is_paused = false;
            st.device_disconnected = false;
            st.position_seconds = 0.0;
            st.vu_left = 0.0;
            st.vu_right = 0.0;
        });
    }

    fn unpause(&mut self) {
        if self.pcm_paused {
            if let Some(backend) = &mut self.backend { backend.pause(false); }
            self.pcm_paused = false;
        }
    }

    /// Drop the card and hand it back to PipeWire (on Linux).
    fn close_pcm(&mut self) {
        if let Some(mut backend) = self.backend.take() {
            backend.close();
            drop(backend);
        }
        self.pcm_paused = false;
        self.written = 0;
    }

    fn maybe_idle_close(&mut self) {
        if self.backend.is_none() || self.active.is_some() || self.idle.elapsed() < IDLE_CLOSE { return; }
        let mut g = self.pipe.lock();
        if !g.q.is_empty() { return; } // the decoder just queued something: stay open
        g.open = None; // decided under the same lock the decoder checks, so it cannot reuse a dying card
        drop(g);
        self.close_pcm();
    }
}

// ---------------------------------------------------------------------------------------------
// Decoder thread
// ---------------------------------------------------------------------------------------------

enum Outcome {
    Next(String),            // end of file and the UI named a next track: keep going, no gap
    Ended,                   // end of file, nothing next: End was queued
    Switch(String, String),  // the user started another track
    Stopped,                 // Stop, or a read error: give the card back
    Failed,                  // the card failed (the writer has already reset the status)
    Unplayable,              // the file could not be opened or decoded
}

enum Poll {
    Go,
    Dropped, // a seek made the chunk in hand obsolete
    Out(Outcome),
}

struct Ctx<'a> {
    rx: &'a Receiver<EngineCommand>,
    status: &'a Arc<Mutex<PlaybackStatus>>,
    pipe: &'a Arc<Pipe>,
    vis: &'a Arc<crate::visualizer::VisQueue>,
}

enum Source {
    Symphonia {
        reader: Box<dyn FormatReader>,
        decoder: Box<dyn AudioDecoder>,
        track_id: u32,
        resampler: Option<FastFixedIn<f32>>,
        fifo: Vec<Vec<f32>>,
    },
    Dsd(crate::dsd::DsdReader),
}

struct Track {
    source: Source,
    meta: TrackMeta,
    info: OpenInfo,
    sample_rate: u32, // device-side / DoP rate for DSD; source PCM rate for PCM
    channels: usize,
    src_frame: u64, // source frames decoded so far
    cue_start: f64,
    cue_end_frame: Option<u64>,
    analyzer: crate::visualizer::Analyzer,
}

/// Interleaved decoder output -> device-rate chunks (resampled when the card runs at another rate).
fn convert(buf: &GenericAudioBufferRef<'_>, info: &OpenInfo, resampler: &mut Option<FastFixedIn<f32>>, fifo: &mut [Vec<f32>], channels: usize) -> Vec<(Samples, usize)> {
    let n = buf.samples_interleaved();
    let mut out = Vec::new();
    let Some(rs) = resampler.as_mut() else {
        // bit-perfect pass-through: not a single sample is altered
        if info.out_fmt.uses_i32() {
            let mut v = vec![0i32; n];
            buf.copy_to_slice_interleaved(&mut v);
            out.push((Samples::I32(v), n / channels));
        } else {
            let mut v = vec![0i16; n];
            buf.copy_to_slice_interleaved(&mut v);
            out.push((Samples::I16(v), n / channels));
        }
        return out;
    };
    let mut f = vec![0.0f32; n];
    buf.copy_to_slice_interleaved(&mut f);
    for (i, &s) in f.iter().enumerate() { fifo[i % channels].push(s); }
    while fifo[0].len() >= rs.input_frames_next() {
        let needed = rs.input_frames_next();
        let chunk: Vec<Vec<f32>> = (0..channels).map(|c| fifo[c].drain(0..needed).collect()).collect();
        if let Ok(o) = rs.process(&chunk, None) { out.push(floats_to_samples(&o, info, channels)); }
    }
    out
}

fn floats_to_samples(o: &[Vec<f32>], info: &OpenInfo, channels: usize) -> (Samples, usize) {
    let frames = o[0].len();
    if info.out_fmt.uses_i32() {
        let mut v = Vec::with_capacity(frames * channels);
        for i in 0..frames { for c in 0..channels { v.push((o[c][i] * 2147483647.0).clamp(-2147483648.0, 2147483647.0) as i32); } }
        (Samples::I32(v), frames)
    } else {
        let mut v = Vec::with_capacity(frames * channels);
        for i in 0..frames { for c in 0..channels { v.push((o[c][i] * 32767.0).clamp(-32768.0, 32767.0) as i16); } }
        (Samples::I16(v), frames)
    }
}

impl Track {
    fn request_open(c: &Ctx, req: OpenReq) -> Result<OpenInfo, Outcome> {
        match c.pipe.begin(req) {
            Begin::Reuse(info) => {
                diagnostics::REUSED.fetch_add(1, Relaxed);
                log_msg("ALSA", format!("Gapless: reusing open PCM '{}' @ {} Hz", info.device_id, info.hw_rate));
                Ok(info)
            }
            Begin::Wait(rx) => {
                let started = Instant::now();
                loop {
                    match rx.recv_timeout(Duration::from_millis(100)) {
                        Ok(Ok(info)) => return Ok(info),
                        Ok(Err(())) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Err(Outcome::Failed),
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                            while let Ok(cmd) = c.rx.try_recv() {
                                match cmd {
                                    EngineCommand::Stop => return Err(Outcome::Stopped),
                                    EngineCommand::Play { file_path, device_id } => return Err(Outcome::Switch(file_path, device_id)),
                                    _ => {}
                                }
                            }
                            if started.elapsed() > Duration::from_secs(20) { return Err(Outcome::Failed); }
                        }
                    }
                }
            }
        }
    }

    /// Probe the file, ask the writer for a suitable card, announce the track.
    fn open(c: &Ctx, file_path: &str, device_id: &str) -> Result<Track, Outcome> {
        let (real_path_str, cue_span) = crate::cue::parse_virtual_path(file_path);
        let path = Path::new(real_path_str);
        let cue_start = cue_span.map(|(s, _)| s).unwrap_or(0.0);
        let cue_end = cue_span.and_then(|(_, e)| e);

        if crate::dsd::is_dsd_path(path) {
            let mut dsd = crate::dsd::DsdReader::open(path).ok_or(Outcome::Unplayable)?;
            let dsd_rate = dsd.info.dsd_rate;
            let dop_rate = dsd.info.dop_rate;
            let channels = dsd.info.channels;
            let title = path.file_stem().and_then(|s| s.to_str()).unwrap_or("Unknown Title").to_string();
            let album = path.parent().and_then(|p| p.file_name()).and_then(|s| s.to_str()).unwrap_or("DSD Audio").to_string();
            let duration = cue_end.map(|e| (e - cue_start).max(0.0)).unwrap_or_else(|| (dsd.info.duration_seconds - cue_start).max(0.0));
            let label = dsd.info.dsd_label.clone();

            log_msg("INFO", format!("Playing {}: {} ({})", label, title, path.display()));
            log_msg("INFO", format!("DSD Format: {dsd_rate} Hz (DoP {dop_rate} Hz · 24-bit) · {channels} channels · Duration: {duration:.2}s"));
            log_msg("ALSA", format!("Requesting DoP output endpoint: '{device_id}'"));

            let req = OpenReq { device_id: device_id.to_string(), sample_rate: dop_rate, channels, bits: 24 };
            let info = Self::request_open(c, req)?;

            // DoP MUST NEVER be resampled or truncated, otherwise markers are destroyed into white noise.
            if info.hw_rate != dop_rate || !info.lossless {
                log_msg("ERROR", format!("DAC '{}' opened at {} Hz ({}), cannot carry {} DoP @ {} Hz untouched", device_id, info.hw_rate, info.fmt, label, dop_rate));
                return Err(Outcome::Unplayable);
            }
            let signal_mode = format!("DoP ({label})");
            log_msg("INFO", format!(">>> ⚡ {signal_mode} VERIFIED: Direct HW '{device_id}' @ {dop_rate} Hz"));

            if cue_start > 0.0 {
                dsd.seek_seconds(cue_start);
            }
            let src_frame = (cue_start * dop_rate as f64) as u64;
            let cue_end_frame = cue_end.map(|e| (e * dop_rate as f64) as u64);

            let meta = TrackMeta {
                file_path: file_path.to_string(), title, artist: "DSD Audio".into(), album,
                sample_rate: dop_rate, bit_depth: 24, channels,
                duration_seconds: duration, replay_gain_db: 0.0, device_id: device_id.to_string(),
                hw_rate: info.hw_rate, hw_format: info.fmt.clone(), buffer: info.buffer, period: info.period,
                bit_perfect: true, signal_mode, cue_start_seconds: cue_start,
            };
            {
                let mut st = c.status.lock().unwrap_or_else(|e| e.into_inner());
                st.decoding_path = Some(file_path.to_string());
                st.decoder_serial += 1;
            }
            announce();

            let mut t = Track {
                source: Source::Dsd(dsd), meta: meta.clone(), info, sample_rate: dop_rate,
                channels: channels as usize, src_frame, cue_start, cue_end_frame,
                analyzer: crate::visualizer::Analyzer::new(dop_rate),
            };
            return match t.send(c, Msg::Track(meta, true)) {
                Poll::Go | Poll::Dropped => Ok(t),
                Poll::Out(o) => Err(o),
            };
        }

        let src = File::open(path).map_err(|_| Outcome::Unplayable)?;
        let mss = MediaSourceStream::new(Box::new(src), Default::default());
        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|s| s.to_str()) { hint.with_extension(ext); }
        let mut reader = symphonia::default::get_probe()
            .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
            .map_err(|_| Outcome::Unplayable)?;
        let track = reader.default_track(TrackType::Audio).ok_or(Outcome::Unplayable)?.clone();
        let params = track.codec_params.as_ref().ok_or(Outcome::Unplayable)?;
        let audio = params.audio().ok_or(Outcome::Unplayable)?;

        let sample_rate = audio.sample_rate.unwrap_or(44100);
        let channels = audio.channels.as_ref().map(|c| c.count() as u32).unwrap_or(2);
        let bits = audio.bits_per_sample.unwrap_or(16);
        let total_duration = track.num_frames.map(|f| f as f64 / sample_rate as f64).unwrap_or(0.0);
        let duration = cue_end
            .map(|e| (e - cue_start).max(0.0))
            .unwrap_or_else(|| (total_duration - cue_start).max(0.0));

        let mut title = path.file_stem().and_then(|s| s.to_str()).unwrap_or("Unknown Title").to_string();
        let (mut artist, mut album, mut gain) = ("Unknown Artist".to_string(), "Local Audio".to_string(), 0.0f32);
        if let Some(m) = reader.metadata().current() {
            for tag in &m.media.tags {
                match &tag.std {
                    Some(StandardTag::TrackTitle(t)) => title = t.to_string(),
                    Some(StandardTag::Artist(a)) => artist = a.to_string(),
                    Some(StandardTag::Album(a)) => album = a.to_string(),
                    Some(StandardTag::ReplayGainTrackGain(g)) => gain = g.trim().trim_end_matches("dB").trim().parse().unwrap_or(0.0), // "-6.50 dB"
                    _ => {}
                }
            }
        }
        log_msg("INFO", format!("Playing: {} ({})", title, path.display()));
        log_msg("INFO", format!("Track Format: {sample_rate} Hz · {bits}-bit · {channels} channels · Duration: {duration:.2}s"));
        log_msg("ALSA", format!("Requesting audio output endpoint: '{device_id}'"));

        // Ask the writer for a card. If the open one fits it carries on without a gap.
        let req = OpenReq { device_id: device_id.to_string(), sample_rate, channels, bits };
        let info = Self::request_open(c, req)?;

        // direct to the device: ALSA hw:, WASAPI exclusive (any id but "default"), CoreAudio hog mode
        let is_hw = ["hw:", "plughw:", "wasapi:", "coreaudio:"].iter().any(|p| device_id.starts_with(p));
        let bit_perfect = is_hw && info.hw_rate == sample_rate && info.lossless;
        let bp_lock = crate::settings::get().bit_perfect_lock;
        let signal_mode = if bit_perfect && bp_lock {
            "Bit-Perfect + 0 dBFS Lock".to_string()
        } else if bit_perfect {
            "Bit-Perfect Direct".to_string()
        } else if info.hw_rate != sample_rate {
            "Software Resampled".to_string()
        } else if !is_hw {
            "Shared Mixer".to_string()
        } else {
            "Bit-Depth Truncated".to_string()
        };

        if !info.lossless {
            log_msg("WARN", format!("Output format {} is narrower than the {bits}-bit source: not bit-perfect", info.fmt));
        }
        if bit_perfect {
            log_msg("INFO", format!(">>> ⚡ BIT-PERFECT VERIFIED: Direct HW '{}' @ {} Hz (0 resampling)", device_id, info.hw_rate));
        } else if info.hw_rate != sample_rate {
            log_msg("INFO", format!(">>> 🎛️ IN-ENGINE RESAMPLER: Converting {} Hz -> {} Hz on '{}' (1.0x pitch preserved)", sample_rate, info.hw_rate, device_id));
        } else if !is_hw {
            log_msg("INFO", format!(">>> Shared device '{}' @ {} Hz: the system mixer may process the audio", device_id, info.hw_rate));
        }

        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(audio, &AudioDecoderOptions::default())
            .map_err(|_| Outcome::Unplayable)?;
        let resampler = if info.hw_rate != sample_rate {
            FastFixedIn::<f32>::new(info.hw_rate as f64 / sample_rate as f64, 1.0, PolynomialDegree::Septic, 1024, channels as usize).ok()
        } else {
            None
        };

        if cue_start > 0.0 {
            let time = Time::try_from_secs_f64(cue_start).unwrap_or(Time::ZERO);
            let _ = reader.seek(SeekMode::Accurate, SeekTo::Time { time, track_id: Some(track.id) });
        }
        let src_frame = (cue_start * sample_rate as f64) as u64;
        let cue_end_frame = cue_end.map(|e| (e * sample_rate as f64) as u64);

        let meta = TrackMeta {
            file_path: file_path.to_string(), title, artist, album, sample_rate, bit_depth: bits, channels,
            duration_seconds: duration, replay_gain_db: gain, device_id: device_id.to_string(), hw_rate: info.hw_rate,
            hw_format: info.fmt.clone(), buffer: info.buffer, period: info.period, bit_perfect, signal_mode,
            cue_start_seconds: cue_start,
        };
        // tell the UI what the decoder is on, so it can name the track after this one in good time
        {
            let mut st = c.status.lock().unwrap_or_else(|e| e.into_inner());
            st.decoding_path = Some(file_path.to_string());
            st.decoder_serial += 1;
        }
        announce();

        let hw_rate = info.hw_rate;
        let mut t = Track {
            source: Source::Symphonia {
                reader, decoder, track_id: track.id, resampler, fifo: vec![Vec::new(); channels as usize],
            },
            meta: meta.clone(), info, sample_rate, channels: channels as usize, src_frame, cue_start, cue_end_frame,
            analyzer: crate::visualizer::Analyzer::new(hw_rate),
        };
        match t.send(c, Msg::Track(meta, true)) {
            Poll::Go | Poll::Dropped => Ok(t),
            Poll::Out(o) => Err(o),
        }
    }

    /// Commands that arrive while decoding.
    fn poll(&mut self, c: &Ctx) -> Poll {
        while let Ok(cmd) = c.rx.try_recv() {
            match cmd {
                EngineCommand::Stop => return Poll::Out(Outcome::Stopped),
                EngineCommand::Play { file_path, device_id } => return Poll::Out(Outcome::Switch(file_path, device_id)),
                EngineCommand::Seek(pos) => {
                    // a later track is queued behind the one the UI is showing: a seek would hit the wrong one
                    if c.pipe.boundary_pending.load(Relaxed) { continue; }
                    self.seek(c, pos);
                    return Poll::Dropped;
                }
                EngineCommand::Pause => {
                    if let Some(o) = self.pause(c) { return Poll::Out(o); }
                }
                EngineCommand::Resume => {
                    if c.pipe.paused.swap(false, Relaxed) {
                        c.status.lock().unwrap_or_else(|e| e.into_inner()).is_paused = false;
                        announce();
                    }
                }
            }
        }
        Poll::Go
    }

    fn pause(&mut self, c: &Ctx) -> Option<Outcome> {
        {
            let mut st = c.status.lock().unwrap_or_else(|e| e.into_inner());
            st.is_paused = true;
            st.vu_left = 0.0;
            st.vu_right = 0.0;
        }
        c.pipe.paused.store(true, Relaxed);
        announce();
        loop {
            match c.rx.recv() {
                Ok(EngineCommand::Resume) => {
                    c.pipe.paused.store(false, Relaxed);
                    c.status.lock().unwrap_or_else(|e| e.into_inner()).is_paused = false;
                    announce();
                    return None;
                }
                Ok(EngineCommand::Stop) | Err(_) => return Some(Outcome::Stopped),
                Ok(EngineCommand::Play { file_path, device_id }) => return Some(Outcome::Switch(file_path, device_id)),
                Ok(_) => {} // seeks while paused are ignored
            }
        }
    }

    fn seek(&mut self, c: &Ctx, pos: f64) {
        let abs_pos = self.cue_start + pos.max(0.0);
        match &mut self.source {
            Source::Symphonia { reader, decoder, track_id, resampler, fifo } => {
                let time = Time::try_from_secs_f64(abs_pos).unwrap_or(Time::ZERO);
                let _ = reader.seek(SeekMode::Accurate, SeekTo::Time { time, track_id: Some(*track_id) });
                decoder.reset();
                if let Some(rs) = resampler.as_mut() {
                    rs.reset();
                    for ch in fifo.iter_mut() { ch.clear(); }
                }
            }
            Source::Dsd(dsd) => {
                dsd.seek_seconds(abs_pos);
            }
        }
        self.src_frame = (abs_pos * self.sample_rate as f64) as u64;
        // drop what is queued and buffered in the card, then carry on with this same track
        c.vis.clear();
        c.pipe.flush(vec![Msg::Track(self.meta.clone(), false)]);
        c.status.lock().unwrap_or_else(|e| e.into_inner()).position_seconds = pos;
    }

    /// Queue a message; while the queue is full keep an eye on commands.
    fn send(&mut self, c: &Ctx, mut m: Msg) -> Poll {
        loop {
            match c.pipe.push(m, Duration::from_millis(20)) {
                Ok(()) => return Poll::Go,
                Err(back) => {
                    m = back;
                    if c.pipe.failed.load(Relaxed) { return Poll::Out(Outcome::Failed); }
                    match self.poll(c) {
                        Poll::Go => {}
                        other => return other,
                    }
                }
            }
        }
    }

    fn emit_ring_i32(&mut self, c: &Ctx, slice: &[i32], frames: usize, is_i16: bool, src_start: u64) -> Poll {
        let mut off = 0;
        while off < slice.len() {
            if c.pipe.failed.load(Relaxed) { return Poll::Out(Outcome::Failed); }
            let n = c.pipe.sample_ring.push_slice(&slice[off..]);
            if n > 0 {
                off += n;
            } else {
                match self.poll(c) {
                    Poll::Go => thread::sleep(Duration::from_millis(5)),
                    other => return other,
                }
            }
        }
        self.send(c, Msg::RingData { sample_count: slice.len(), frames, is_i16, src_start })
    }

    fn emit(&mut self, c: &Ctx, chunks: Vec<(Samples, usize)>, src_start: u64) -> Poll {
        let mut i16_buf = [0i32; 1024];
        let pos_secs = (src_start as f64 / self.sample_rate.max(1) as f64 - self.cue_start).max(0.0);
        for (samples, frames) in chunks {
            let res = match &samples {
                Samples::I32(v) => {
                    self.analyzer.feed_i32(v, self.channels, pos_secs, c.vis);
                    self.emit_ring_i32(c, v, frames, false, src_start)
                }
                Samples::I16(v) => {
                    self.analyzer.feed_i16(v, self.channels, pos_secs, c.vis);
                    let mut off = 0;
                    let mut poll_res = Poll::Go;
                    while off < v.len() {
                        if c.pipe.failed.load(Relaxed) { return Poll::Out(Outcome::Failed); }
                        let n = (v.len() - off).min(i16_buf.len());
                        for (dst, &src) in i16_buf[..n].iter_mut().zip(&v[off..off + n]) {
                            *dst = src as i32;
                        }
                        let mut pushed = 0;
                        while pushed < n {
                            let w = c.pipe.sample_ring.push_slice(&i16_buf[pushed..n]);
                            if w > 0 {
                                pushed += w;
                            } else {
                                match self.poll(c) {
                                    Poll::Go => thread::sleep(Duration::from_millis(5)),
                                    other => { poll_res = other; break; }
                                }
                            }
                        }
                        if !matches!(poll_res, Poll::Go) { break; }
                        off += n;
                    }
                    if matches!(poll_res, Poll::Go) {
                        self.send(c, Msg::RingData { sample_count: v.len(), frames, is_i16: true, src_start })
                    } else {
                        poll_res
                    }
                }
            };
            match res {
                Poll::Go => {}
                other => return other,
            }
        }
        Poll::Go
    }
}

fn play_one(c: &Ctx, next_hint: &NextHint, file_path: &str, device_id: &str) -> Outcome {
    let mut t = match Track::open(c, file_path, device_id) {
        Ok(t) => t,
        Err(o) => return o,
    };
    let mut dop_buf = Vec::new();
    loop {
        match t.poll(c) {
            Poll::Go | Poll::Dropped => {}
            Poll::Out(o) => return o,
        }
        if c.pipe.failed.load(Relaxed) { return Outcome::Failed; }
        if let Some(end_frame) = t.cue_end_frame {
            if t.src_frame >= end_frame {
                break;
            }
        }

        match &mut t.source {
            Source::Dsd(dsd) => {
                let t0 = Instant::now();
                let Some(frames) = dsd.next_dop_chunk(&mut dop_buf) else { break };
                diagnostics::note_decode(t0.elapsed());
                let src_start = t.src_frame;
                t.src_frame += frames as u64;
                match t.emit_ring_i32(c, &dop_buf, frames, false, src_start) {
                    Poll::Go | Poll::Dropped => {}
                    Poll::Out(o) => return o,
                }
            }
            Source::Symphonia { reader, decoder, track_id, resampler, fifo } => {
                let packet = match reader.next_packet() {
                    Ok(Some(p)) => p,
                    Ok(None) | Err(SymphoniaError::IoError(_)) => break, // end of file (some demuxers report it as an I/O error)
                    Err(_) => return Outcome::Stopped,
                };
                if packet.track_id != *track_id { continue; }

                let t0 = Instant::now();
                let decoded = decoder.decode(&packet);
                diagnostics::note_decode(t0.elapsed());
                let buf = match decoded {
                    Ok(b) => b,
                    Err(SymphoniaError::DecodeError(_)) => continue,
                    Err(_) => return Outcome::Stopped,
                };
                let src_start = t.src_frame;
                let frames_in = buf.frames() as u64;
                let chunks = convert(&buf, &t.info, resampler, fifo, t.channels);
                t.src_frame += frames_in;
                match t.emit(c, chunks, src_start) {
                    Poll::Go | Poll::Dropped => {}
                    Poll::Out(o) => return o,
                }
            }
        }
    }

    // end of file: push out what the resampler still holds
    if let Source::Symphonia { resampler: Some(rs), fifo, .. } = &mut t.source {
        if !fifo[0].is_empty() {
            if let Ok(o) = rs.process_partial(Some(fifo), None) {
                let chunk = floats_to_samples(&o, &t.info, t.channels);
                let src_start = t.src_frame;
                match t.emit(c, vec![chunk], src_start) {
                    Poll::Go | Poll::Dropped => {}
                    Poll::Out(o) => return o,
                }
            }
        }
    }
    // Gapless: if the UI named a next track, carry straight on while this one is still being heard.
    if let Some(next) = next_hint.lock().unwrap_or_else(|e| e.into_inner()).take() {
        return Outcome::Next(next);
    }
    let _ = c.pipe.push(Msg::End, Duration::from_millis(0));
    Outcome::Ended
}

fn play_chain(c: &Ctx, next_hint: &NextHint, first: (String, String)) {
    c.pipe.failed.store(false, Relaxed);
    c.pipe.paused.store(false, Relaxed);
    c.vis.clear();
    c.pipe.flush(vec![]); // a new Play starts clean: drop whatever the previous one left queued or buffered
    let mut target = Some(first);
    let mut first_track = true;
    while let Some((file, device)) = target.take() {
        match play_one(c, next_hint, &file, &device) {
            Outcome::Next(path) => target = Some((path, device)),
            Outcome::Switch(f, d) => {
                c.pipe.paused.store(false, Relaxed);
                c.pipe.flush(vec![]);
                target = Some((f, d));
            }
            Outcome::Ended | Outcome::Failed => {}
            Outcome::Stopped => {
                c.pipe.paused.store(false, Relaxed);
                c.pipe.flush(vec![Msg::Stop]);
            }
            Outcome::Unplayable => {
                log_msg("ERROR", format!("Cannot play '{file}'"));
                // the first track: nothing is playing, so stop. Later: let the previous one finish.
                let end = if first_track { Msg::Stop } else { Msg::End };
                c.pipe.paused.store(false, Relaxed);
                let _ = c.pipe.push(end, Duration::from_millis(0));
            }
        }
        first_track = false;
    }
}

/// Runs on the engine thread: spawns the writer, then serves commands as the decoder.
pub fn run(rx: Receiver<EngineCommand>, status: Arc<Mutex<PlaybackStatus>>, next_hint: NextHint, vis: Arc<crate::visualizer::VisQueue>) {
    let pipe = Pipe::new();
    {
        let (p, s) = (pipe.clone(), status.clone());
        thread::Builder::new().name("rex-writer".into())
            .spawn(move || Writer::new(p, s).run())
            .expect("failed to spawn the audio writer thread");
    }
    let c = Ctx { rx: &rx, status: &status, pipe: &pipe, vis: &vis };
    while let Ok(cmd) = rx.recv() {
        match cmd {
            EngineCommand::Play { file_path, device_id } => play_chain(&c, &next_hint, (file_path, device_id)),
            EngineCommand::Stop => {
                pipe.paused.store(false, Relaxed);
                vis.clear();
                pipe.flush(vec![Msg::Stop]);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(frames: usize) -> Msg {
        Msg::Data { samples: Samples::I16(vec![0; frames * 2]), frames, src_start: 0 }
    }

    fn kind(m: &Msg) -> &'static str {
        match m { Msg::Open { .. } => "open", Msg::Track(..) => "track", Msg::Data { .. } | Msg::RingData { .. } => "data", Msg::End => "end", Msg::Flush => "flush", Msg::Stop => "stop" }
    }

    #[test]
    fn messages_come_out_in_order() {
        let p = Pipe::new();
        p.push(Msg::Track(dummy_meta(), true), Duration::ZERO).ok().unwrap();
        p.push(data(10), Duration::ZERO).ok().unwrap();
        p.push(Msg::End, Duration::ZERO).ok().unwrap();
        let got: Vec<_> = (0..3).map(|_| kind(&p.pop(Duration::ZERO).unwrap())).collect();
        assert_eq!(got, ["track", "data", "end"]);
        assert!(p.pop(Duration::from_millis(1)).is_none());
    }

    #[test]
    fn a_full_queue_pushes_audio_back_but_never_control_messages() {
        let p = Pipe::new(); // cap is 48000 frames until a card is open
        p.push(data(48_000), Duration::ZERO).ok().unwrap();
        assert!(p.push(data(100), Duration::from_millis(1)).is_err(), "audio must wait while a second is queued");
        assert!(p.push(Msg::End, Duration::ZERO).is_ok(), "control messages never wait");
        p.pop(Duration::ZERO).unwrap(); // the writer takes the audio...
        assert!(p.push(data(100), Duration::ZERO).is_ok(), "...and there is room again");
    }

    #[test]
    fn flush_drops_queued_audio_and_keeps_what_follows() {
        let p = Pipe::new();
        for _ in 0..3 { p.push(data(100), Duration::ZERO).ok().unwrap(); }
        p.flush(vec![Msg::Track(dummy_meta(), false)]);
        let got: Vec<_> = (0..2).map(|_| kind(&p.pop(Duration::ZERO).unwrap())).collect();
        assert_eq!(got, ["flush", "track"]);
        assert!(p.pop(Duration::from_millis(1)).is_none());
        assert!(p.push(data(48_000), Duration::ZERO).is_ok(), "the frame count was reset by the flush");
    }

    #[test]
    fn while_paused_only_flush_and_stop_get_through() {
        let p = Pipe::new();
        p.push(data(10), Duration::ZERO).ok().unwrap();
        p.paused.store(true, Relaxed);
        assert!(p.pop(Duration::from_millis(1)).is_none(), "audio is held back while paused");
        p.flush(vec![]);
        assert_eq!(kind(&p.pop(Duration::from_millis(1)).unwrap()), "flush");
        p.paused.store(false, Relaxed);
    }

    #[test]
    fn begin_reuses_a_fitting_card_and_asks_for_a_reopen_otherwise() {
        let p = Pipe::new();
        let req = |rate, bits| OpenReq { device_id: "hw:1,0".into(), sample_rate: rate, channels: 2, bits };
        assert!(matches!(p.begin(req(44100, 16)), Begin::Wait(_)), "nothing is open yet");
        p.set_open(Some(info(44100, false)));
        assert!(matches!(p.begin(req(44100, 16)), Begin::Reuse(_)));
        assert!(matches!(p.begin(req(96000, 16)), Begin::Wait(_)), "rate change");
        assert!(matches!(p.begin(req(44100, 24)), Begin::Wait(_)), "16 -> 24 bit");
    }

    #[test]
    fn peaks_handle_the_most_negative_sample() {
        assert_eq!(peaks_i32(&[i32::MIN, 0, 0, i32::MAX], 2), (1.0, 1.0));
        assert_eq!(peaks_i16(&[i16::MIN, 0], 2), (1.0, 0.0));
        assert_eq!(peaks_i16(&[16384, 0, -8192, 0], 1), (0.5, 0.5)); // mono: both meters show the one channel
        assert_eq!(peaks_i32(&[], 2), (0.0, 0.0));
    }

    fn info(rate: u32, wide: bool) -> OpenInfo {
        OpenInfo { device_id: "hw:1,0".into(), hw_rate: rate, channels: 2, wide, out_fmt: OutFmt::S32, lossless: true, fmt: "S32_LE".into(), buffer: 0, period: 0 }
    }

    fn dummy_meta() -> TrackMeta {
        TrackMeta { file_path: "x".into(), title: "t".into(), artist: "a".into(), album: "b".into(), sample_rate: 44100, bit_depth: 16, channels: 2,
            duration_seconds: 1.0, replay_gain_db: 0.0, device_id: "hw:1,0".into(), hw_rate: 44100, hw_format: "S16".into(), buffer: 0, period: 0, bit_perfect: true,
            signal_mode: "Bit-Perfect Direct".into(), cue_start_seconds: 0.0 }
    }

    #[test]
    fn applying_meta_bumps_the_serial_only_for_a_new_track() {
        let mut st = PlaybackStatus::default();
        apply_meta(&mut st, &dummy_meta(), true);
        assert_eq!((st.track_serial, st.is_playing, st.ended), (1, true, false));
        apply_meta(&mut st, &dummy_meta(), false); // same track continues after a seek
        assert_eq!(st.track_serial, 1);
    }
}

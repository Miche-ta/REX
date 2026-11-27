//! Lock-free Single-Producer Single-Consumer (SPSC) ring buffer for zero-copy audio streaming.
//!
//! Design:
//! - Power-of-two capacity `N`, indexed via `& (N - 1)`.
//! - Monotonically increasing `AtomicU64` `head` (producer) and `tail` (consumer) on separate
//!   64-byte cache lines to eliminate false sharing.
//! - Producer: `head.store(Release)`, `tail.load(Acquire)`, reads own `head` `Relaxed`.
//! - Consumer: `tail.store(Release)`, `head.load(Acquire)`, reads own `tail` `Relaxed`.
//! - Each side caches the peer's counter locally and reloads with `Acquire` only when the buffer
//!   appears full or empty (optimistic semantics, avoiding cache-line bouncing).
//! - Two-span `(first, second)` slice API handles wraparound without double-`mmap`.
//! - Debug-mode thread-ID assertions enforce single-producer / single-consumer discipline.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[cfg(debug_assertions)]
use std::sync::Mutex;
#[cfg(debug_assertions)]
use std::thread::ThreadId;

#[repr(align(64))]
struct CachePadded<T>(T);

pub struct SpscRing<T> {
    buf: Box<[UnsafeCell<T>]>,
    capacity: usize,
    mask: usize,
    head: CachePadded<AtomicU64>,
    tail: CachePadded<AtomicU64>,
    cached_tail: CachePadded<UnsafeCell<u64>>,
    cached_head: CachePadded<UnsafeCell<u64>>,
    quiescing: AtomicBool,
    #[cfg(debug_assertions)]
    producer_thread: Mutex<Option<ThreadId>>,
    #[cfg(debug_assertions)]
    consumer_thread: Mutex<Option<ThreadId>>,
}

unsafe impl<T: Send> Send for SpscRing<T> {}
unsafe impl<T: Send> Sync for SpscRing<T> {}

#[allow(dead_code)]
impl<T: Copy + Default> SpscRing<T> {
    /// Create a new SPSC ring buffer. `capacity` must be a non-zero power of two.
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0 && capacity.is_power_of_two(), "SpscRing capacity must be a power of two");
        let mut vec = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            vec.push(UnsafeCell::new(T::default()));
        }
        Self {
            buf: vec.into_boxed_slice(),
            capacity,
            mask: capacity - 1,
            head: CachePadded(AtomicU64::new(0)),
            tail: CachePadded(AtomicU64::new(0)),
            cached_tail: CachePadded(UnsafeCell::new(0)),
            cached_head: CachePadded(UnsafeCell::new(0)),
            quiescing: AtomicBool::new(false),
            #[cfg(debug_assertions)]
            producer_thread: Mutex::new(None),
            #[cfg(debug_assertions)]
            consumer_thread: Mutex::new(None),
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    #[cfg(debug_assertions)]
    fn check_producer(&self) {
        let tid = std::thread::current().id();
        let mut guard = self.producer_thread.lock().unwrap_or_else(|e| e.into_inner());
        match *guard {
            Some(prev) => assert_eq!(prev, tid, "SpscRing producer accessed from multiple threads"),
            None => *guard = Some(tid),
        }
    }

    #[cfg(not(debug_assertions))]
    #[inline(always)]
    fn check_producer(&self) {}

    #[cfg(debug_assertions)]
    fn check_consumer(&self) {
        let tid = std::thread::current().id();
        let mut guard = self.consumer_thread.lock().unwrap_or_else(|e| e.into_inner());
        match *guard {
            Some(prev) => assert_eq!(prev, tid, "SpscRing consumer accessed from multiple threads"),
            None => *guard = Some(tid),
        }
    }

    #[cfg(not(debug_assertions))]
    #[inline(always)]
    fn check_consumer(&self) {}

    /// Approximate number of readable items currently in the ring.
    pub fn len(&self) -> usize {
        let h = self.head.0.load(Ordering::Acquire);
        let t = self.tail.0.load(Ordering::Acquire);
        h.wrapping_sub(t) as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Available space for writing (producer side, uses cached `tail` first).
    pub fn available_write(&self) -> usize {
        self.check_producer();
        let h = self.head.0.load(Ordering::Relaxed);
        // SAFETY: only the producer thread accesses `cached_tail`.
        let cached_t = unsafe { *self.cached_tail.0.get() };
        let used = h.wrapping_sub(cached_t) as usize;
        if used >= self.capacity {
            let fresh_t = self.tail.0.load(Ordering::Acquire);
            unsafe { *self.cached_tail.0.get() = fresh_t };
            self.capacity.saturating_sub(h.wrapping_sub(fresh_t) as usize)
        } else {
            self.capacity - used
        }
    }

    /// Available items for reading (consumer side, uses cached `head` first).
    pub fn available_read(&self) -> usize {
        self.check_consumer();
        let t = self.tail.0.load(Ordering::Relaxed);
        // SAFETY: only the consumer thread accesses `cached_head`.
        let cached_h = unsafe { *self.cached_head.0.get() };
        let avail = cached_h.wrapping_sub(t) as usize;
        if avail == 0 {
            let fresh_h = self.head.0.load(Ordering::Acquire);
            unsafe { *self.cached_head.0.get() = fresh_h };
            fresh_h.wrapping_sub(t) as usize
        } else {
            avail
        }
    }

    /// Push as many items from `src` as fit without blocking. Returns the number of items written.
    pub fn push_slice(&self, src: &[T]) -> usize {
        self.check_producer();
        if self.quiescing.load(Ordering::Acquire) || src.is_empty() {
            return 0;
        }
        let mut avail = self.available_write();
        if avail < src.len() {
            let h = self.head.0.load(Ordering::Relaxed);
            let fresh_t = self.tail.0.load(Ordering::Acquire);
            unsafe { *self.cached_tail.0.get() = fresh_t };
            avail = self.capacity.saturating_sub(h.wrapping_sub(fresh_t) as usize);
        }
        let to_write = src.len().min(avail);
        if to_write == 0 {
            return 0;
        }
        let h = self.head.0.load(Ordering::Relaxed);
        let start = (h as usize) & self.mask;
        let first_len = to_write.min(self.capacity - start);
        let second_len = to_write - first_len;

        // SAFETY: `to_write <= avail` guarantees these slots are exclusively owned by producer
        // until `head.store(..., Release)`.
        unsafe {
            let ptr = self.buf.as_ptr() as *mut T;
            std::ptr::copy_nonoverlapping(src.as_ptr(), ptr.add(start), first_len);
            if second_len > 0 {
                std::ptr::copy_nonoverlapping(src.as_ptr().add(first_len), ptr, second_len);
            }
        }
        self.head.0.store(h.wrapping_add(to_write as u64), Ordering::Release);
        to_write
    }

    /// Pop up to `dst.len()` items into `dst` without blocking. Returns the number of items read.
    pub fn pop_slice(&self, dst: &mut [T]) -> usize {
        self.check_consumer();
        if dst.is_empty() {
            return 0;
        }
        let mut avail = self.available_read();
        if avail < dst.len() {
            let t = self.tail.0.load(Ordering::Relaxed);
            let fresh_h = self.head.0.load(Ordering::Acquire);
            unsafe { *self.cached_head.0.get() = fresh_h };
            avail = fresh_h.wrapping_sub(t) as usize;
        }
        let to_read = dst.len().min(avail);
        if to_read == 0 {
            return 0;
        }
        let t = self.tail.0.load(Ordering::Relaxed);
        let start = (t as usize) & self.mask;
        let first_len = to_read.min(self.capacity - start);
        let second_len = to_read - first_len;

        // SAFETY: `to_read <= avail` guarantees producer already `Release`-published these slots.
        unsafe {
            let ptr = self.buf.as_ptr() as *const T;
            std::ptr::copy_nonoverlapping(ptr.add(start), dst.as_mut_ptr(), first_len);
            if second_len > 0 {
                std::ptr::copy_nonoverlapping(ptr, dst.as_mut_ptr().add(first_len), second_len);
            }
        }
        self.tail.0.store(t.wrapping_add(to_read as u64), Ordering::Release);
        to_read
    }

    /// Discard all currently queued items from the consumer side (100% SPSC-safe without quiescing).
    pub fn consumer_discard_all(&self) {
        self.check_consumer();
        let h = self.head.0.load(Ordering::Acquire);
        unsafe { *self.cached_head.0.get() = h };
        self.tail.0.store(h, Ordering::Release);
    }

    /// Signal producer to pause writes so `reset()` can be called once both sides are idle.
    pub fn quiesce(&self) {
        self.quiescing.store(true, Ordering::SeqCst);
    }

    /// Reset counters and clear thread-ID guards. Caller must ensure both sides are quiesced.
    pub fn reset(&self) {
        self.head.0.store(0, Ordering::SeqCst);
        self.tail.0.store(0, Ordering::SeqCst);
        unsafe {
            *self.cached_head.0.get() = 0;
            *self.cached_tail.0.get() = 0;
        }
        #[cfg(debug_assertions)]
        {
            *self.producer_thread.lock().unwrap_or_else(|e| e.into_inner()) = None;
            *self.consumer_thread.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
        self.quiescing.store(false, Ordering::SeqCst);
    }
}

#[cfg(debug_assertions)]
impl<T> Drop for SpscRing<T> {
    fn drop(&mut self) {
        *self.producer_thread.lock().unwrap_or_else(|e| e.into_inner()) = None;
        *self.consumer_thread.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn push_pop_wraparound_preserves_order() {
        let ring = SpscRing::<i32>::new(8);
        assert_eq!(ring.available_write(), 8);
        assert_eq!(ring.available_read(), 0);

        // Write 6, read 4 -> tail at 4, head at 6
        assert_eq!(ring.push_slice(&[1, 2, 3, 4, 5, 6]), 6);
        let mut out = [0i32; 4];
        assert_eq!(ring.pop_slice(&mut out), 4);
        assert_eq!(out, [1, 2, 3, 4]);

        // Write 6 more -> wraps around boundary (indices 6,7,0,1,2,3)
        assert_eq!(ring.push_slice(&[7, 8, 9, 10, 11, 12]), 6);
        assert_eq!(ring.available_write(), 0);
        // Extra push must drop/return 0
        assert_eq!(ring.push_slice(&[99]), 0);

        let mut rest = [0i32; 8];
        assert_eq!(ring.pop_slice(&mut rest), 8);
        assert_eq!(rest, [5, 6, 7, 8, 9, 10, 11, 12]);
        assert!(ring.is_empty());
    }

    #[test]
    fn quiesce_and_reset_clears_state() {
        let ring = SpscRing::<i32>::new(16);
        assert_eq!(ring.push_slice(&[10, 20, 30]), 3);
        ring.quiesce();
        assert_eq!(ring.push_slice(&[40]), 0);
        ring.reset();
        assert!(ring.is_empty());
        assert_eq!(ring.push_slice(&[50, 60]), 2);
        let mut out = [0i32; 2];
        assert_eq!(ring.pop_slice(&mut out), 2);
        assert_eq!(out, [50, 60]);
    }

    #[test]
    fn consumer_discard_all_drops_pending_safely() {
        let ring = SpscRing::<i32>::new(16);
        assert_eq!(ring.push_slice(&[1, 2, 3, 4, 5]), 5);
        ring.consumer_discard_all();
        assert!(ring.is_empty());
        assert_eq!(ring.push_slice(&[0; 16]), 16);
    }

    #[test]
    fn concurrent_spsc_transfers_500k_samples_intact() {
        let ring = Arc::new(SpscRing::<u32>::new(4096));
        let total = 500_000u32;

        let p_ring = Arc::clone(&ring);
        let prod = thread::spawn(move || {
            let mut next = 0u32;
            let mut buf = [0u32; 137];
            while next < total {
                let n = ((total - next) as usize).min(buf.len());
                for (i, slot) in buf[..n].iter_mut().enumerate() {
                    *slot = next + i as u32;
                }
                let mut off = 0;
                while off < n {
                    let w = p_ring.push_slice(&buf[off..n]);
                    if w == 0 {
                        thread::yield_now();
                    } else {
                        off += w;
                    }
                }
                next += n as u32;
            }
        });

        let c_ring = Arc::clone(&ring);
        let cons = thread::spawn(move || {
            let mut expected = 0u32;
            let mut buf = [0u32; 211];
            while expected < total {
                let r = c_ring.pop_slice(&mut buf);
                if r == 0 {
                    thread::yield_now();
                    continue;
                }
                for &val in &buf[..r] {
                    assert_eq!(val, expected, "corrupted or out-of-order sample");
                    expected += 1;
                }
            }
        });

        prod.join().unwrap();
        cons.join().unwrap();
        assert!(ring.is_empty());
    }
}

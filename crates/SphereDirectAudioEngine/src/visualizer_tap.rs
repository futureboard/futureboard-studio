//! Master-bus tap for the audio visualizers (spectrum, stereo image,
//! loudness, oscilloscope, spectrogram).
//!
//! The output callback copies the master bus — after the master fader, before
//! the Control Room, the same signal an export gets — into a preallocated
//! stereo ring while at least one visualizer is listening. With nobody
//! listening it costs one relaxed load per block.
//!
//! Realtime contract: the producer side is atomics only (no allocation, no
//! locking, no logging). The ring is process-wide, like
//! [`crate::analysis_tap`], so a device or project swap never leaves a reader
//! holding a tap the callback no longer writes.
//!
//! One producer (the output callback) and any number of *readers*: a reader
//! never advances shared state — it keeps its own cursor and copies frames out
//! — so several windows can read the same ring. A reader that falls more than
//! a ring's worth behind skips forward and reports the gap.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::input_ring::InputRing;

/// Frames kept clear of the write head when a late reader resynchronises, so
/// the slots it copies are not the ones the callback is about to overwrite.
const OVERWRITE_MARGIN: u64 = 2_048;

pub struct VisualizerTap {
    ring: InputRing,
    listeners: AtomicU32,
    sample_rate: AtomicU32,
}

impl Default for VisualizerTap {
    fn default() -> Self {
        Self {
            ring: InputRing::default(),
            listeners: AtomicU32::new(0),
            sample_rate: AtomicU32::new(0),
        }
    }
}

/// What one [`VisualizerTap::read`] produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TapRead {
    /// Frames copied into the caller's buffers.
    pub frames: usize,
    /// Frames the reader missed because it fell too far behind. The copy
    /// resumes after the gap, so a reader can reset any state that assumes a
    /// continuous signal.
    pub skipped: u64,
    /// The cursor to pass to the next read.
    pub next: u64,
}

impl VisualizerTap {
    /// Whether any visualizer is listening. One relaxed load: the callback
    /// checks it every block.
    #[inline]
    pub fn is_listening(&self) -> bool {
        self.listeners.load(Ordering::Relaxed) > 0
    }

    /// Producer: append the master bus block. Realtime-safe (atomics only).
    #[inline]
    pub fn write_interleaved(&self, data: &[f32], channels: usize, sample_rate: u32) {
        if channels == 0 {
            return;
        }
        if self.sample_rate.load(Ordering::Relaxed) != sample_rate {
            self.sample_rate.store(sample_rate, Ordering::Relaxed);
        }
        for frame in data.chunks_exact(channels) {
            let left = frame[0];
            let right = if channels > 1 { frame[1] } else { left };
            self.ring.write_stereo(left, right);
        }
    }

    /// Register a listener; the callback starts copying on the next block.
    /// Returns the cursor a new reader should start from (the current head,
    /// so it never reads frames from before it opened).
    pub fn listen(&self) -> u64 {
        self.listeners.fetch_add(1, Ordering::AcqRel);
        self.ring.write_head()
    }

    /// Drop a listener. The callback stops copying once the last one goes.
    pub fn unlisten(&self) {
        let _ = self
            .listeners
            .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |count| {
                count.checked_sub(1)
            });
    }

    pub fn listeners(&self) -> u32 {
        self.listeners.load(Ordering::Relaxed)
    }

    /// Sample rate of the frames most recently written; `0` before the first.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate.load(Ordering::Relaxed)
    }

    /// Copy the frames written since `cursor`, oldest first, up to the length
    /// of the shorter buffer. Never blocks and never touches shared state.
    pub fn read(&self, cursor: u64, left: &mut [f32], right: &mut [f32]) -> TapRead {
        let head = self.ring.write_head();
        let capacity = self.ring.capacity_frames();
        let oldest_safe = head.saturating_sub(capacity.saturating_sub(OVERWRITE_MARGIN));
        let (start, skipped) = if cursor < oldest_safe {
            (oldest_safe, oldest_safe - cursor)
        } else {
            (cursor.min(head), 0)
        };
        let wanted = (head - start) as usize;
        let frames = wanted.min(left.len()).min(right.len());
        for i in 0..frames {
            let (l, r) = self.ring.read_frame(start + i as u64);
            left[i] = l;
            right[i] = r;
        }
        TapRead {
            frames,
            skipped,
            next: start + frames as u64,
        }
    }
}

/// The process-wide tap the output callback writes and visualizers read.
pub fn visualizer_tap() -> &'static VisualizerTap {
    static TAP: OnceLock<VisualizerTap> = OnceLock::new();
    TAP.get_or_init(VisualizerTap::default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_arrive_in_order_and_the_cursor_advances() {
        let tap = VisualizerTap::default();
        let cursor = tap.listen();
        tap.write_interleaved(&[0.1, -0.1, 0.2, -0.2, 0.3, -0.3], 2, 48_000);
        let (mut l, mut r) = ([0.0; 8], [0.0; 8]);
        let read = tap.read(cursor, &mut l, &mut r);
        assert_eq!(read.frames, 3);
        assert_eq!(read.skipped, 0);
        assert_eq!(&l[..3], &[0.1, 0.2, 0.3]);
        assert_eq!(&r[..3], &[-0.1, -0.2, -0.3]);
        assert_eq!(tap.read(read.next, &mut l, &mut r).frames, 0);
        assert_eq!(tap.sample_rate(), 48_000);
    }

    #[test]
    fn mono_is_copied_to_both_sides_and_extra_channels_ignored() {
        let tap = VisualizerTap::default();
        let cursor = tap.listen();
        tap.write_interleaved(&[0.5, 0.25], 1, 44_100);
        tap.write_interleaved(&[0.1, 0.2, 0.9, 0.9], 4, 44_100);
        let (mut l, mut r) = ([0.0; 4], [0.0; 4]);
        let read = tap.read(cursor, &mut l, &mut r);
        assert_eq!(read.frames, 3);
        assert_eq!(&l[..3], &[0.5, 0.25, 0.1]);
        assert_eq!(&r[..3], &[0.5, 0.25, 0.2]);
    }

    #[test]
    fn a_reader_that_falls_behind_skips_forward_and_says_so() {
        let tap = VisualizerTap::default();
        let cursor = tap.listen();
        let block = vec![0.0f32; 1_024 * 2];
        for _ in 0..40 {
            tap.write_interleaved(&block, 2, 48_000);
        }
        let (mut l, mut r) = (vec![0.0; 65_536], vec![0.0; 65_536]);
        let read = tap.read(cursor, &mut l, &mut r);
        assert!(read.skipped > 0);
        assert_eq!(read.next, 40 * 1_024);
        assert!(read.frames as u64 <= tap.ring.capacity_frames() - OVERWRITE_MARGIN);
    }

    #[test]
    fn the_listener_count_never_underflows() {
        let tap = VisualizerTap::default();
        assert!(!tap.is_listening());
        tap.listen();
        tap.listen();
        tap.unlisten();
        assert!(tap.is_listening());
        tap.unlisten();
        tap.unlisten();
        assert_eq!(tap.listeners(), 0);
    }
}

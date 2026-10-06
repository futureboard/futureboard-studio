//! A single-producer, single-consumer ring of samples.
//!
//! Carries the interface's input from the capture callback to the playback
//! callback, and each recorded strip from the audio thread to the disk
//! writer. Wait-free on both sides: a full ring drops what does not fit, an
//! empty one hands back nothing, and neither ever blocks the audio thread.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicUsize, Ordering};

pub struct SampleRing {
    buffer: Box<[UnsafeCell<f32>]>,
    mask: usize,
    /// Consumer cursor (monotonic).
    head: AtomicUsize,
    /// Producer cursor (monotonic).
    tail: AtomicUsize,
}

// SAFETY: the producer only writes slots in [tail, head + capacity) and
// publishes them with a release store of `tail`; the consumer only reads
// slots in [head, tail) after an acquire load. No slot is touched by both at
// once.
unsafe impl Sync for SampleRing {}
unsafe impl Send for SampleRing {}

impl SampleRing {
    /// A ring holding at least `min_capacity` samples (rounded up to a power
    /// of two).
    pub fn new(min_capacity: usize) -> Self {
        let capacity = min_capacity.max(2).next_power_of_two();
        Self {
            buffer: (0..capacity).map(|_| UnsafeCell::new(0.0)).collect(),
            mask: capacity - 1,
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
        }
    }

    pub fn capacity(&self) -> usize {
        self.mask + 1
    }

    /// Samples waiting to be read.
    pub fn len(&self) -> usize {
        self.tail
            .load(Ordering::Acquire)
            .wrapping_sub(self.head.load(Ordering::Acquire))
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Producer: append as much of `samples` as fits; returns how many went
    /// in.
    pub fn push(&self, samples: &[f32]) -> usize {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        let free = self.capacity() - tail.wrapping_sub(head);
        let n = samples.len().min(free);
        for (i, sample) in samples[..n].iter().enumerate() {
            // SAFETY: slot is free (see the type's contract).
            unsafe { *self.buffer[(tail.wrapping_add(i)) & self.mask].get() = *sample };
        }
        self.tail.store(tail.wrapping_add(n), Ordering::Release);
        n
    }

    /// Consumer: fill `out` from the front; returns how many were read.
    pub fn pop(&self, out: &mut [f32]) -> usize {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        let n = out.len().min(tail.wrapping_sub(head));
        for (i, slot) in out[..n].iter_mut().enumerate() {
            // SAFETY: slot is published (see the type's contract).
            *slot = unsafe { *self.buffer[(head.wrapping_add(i)) & self.mask].get() };
        }
        self.head.store(head.wrapping_add(n), Ordering::Release);
        n
    }

    /// Consumer: throw away up to `n` of the oldest samples.
    pub fn skip(&self, n: usize) -> usize {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        let n = n.min(tail.wrapping_sub(head));
        self.head.store(head.wrapping_add(n), Ordering::Release);
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pushes_and_pops_in_order_across_the_wrap() {
        let ring = SampleRing::new(8);
        let mut out = [0.0; 8];
        for round in 0..5 {
            let base = round as f32 * 10.0;
            assert_eq!(
                ring.push(&[base, base + 1.0, base + 2.0, base + 3.0, base + 4.0]),
                5
            );
            assert_eq!(ring.pop(&mut out[..5]), 5);
            assert_eq!(
                out[..5],
                [base, base + 1.0, base + 2.0, base + 3.0, base + 4.0]
            );
        }
    }

    #[test]
    fn a_full_ring_drops_the_overflow_and_an_empty_one_reads_nothing() {
        let ring = SampleRing::new(4);
        assert_eq!(ring.push(&[1.0; 6]), 4);
        assert_eq!(ring.skip(1), 1);
        let mut out = [0.0; 6];
        assert_eq!(ring.pop(&mut out), 3);
        assert_eq!(ring.pop(&mut out), 0);
    }
}

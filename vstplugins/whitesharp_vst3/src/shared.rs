//! What the audio thread publishes for the editor: the pitch telemetry, the
//! in/out meters and the stream's rate and latency. Atomics only; the audio
//! side never waits on the editor.

use std::sync::atomic::{fence, AtomicU32, Ordering};

pub const TELEMETRY_SLOTS: usize = 64;

/// A meter reading for one block, linear.
#[derive(Default)]
pub struct BlockMeter {
    in_peak: f32,
    in_square: f32,
    out_peak: f32,
    out_square: f32,
    frames: u32,
}

impl BlockMeter {
    #[inline]
    pub fn input(&mut self, left: f32, right: f32) {
        self.in_peak = self.in_peak.max(left.abs()).max(right.abs());
        self.in_square += 0.5 * (left * left + right * right);
        self.frames += 1;
    }

    #[inline]
    pub fn output(&mut self, left: f32, right: f32) {
        self.out_peak = self.out_peak.max(left.abs()).max(right.abs());
        self.out_square += 0.5 * (left * left + right * right);
    }

    fn rms(square: f32, frames: u32) -> f32 {
        if frames == 0 {
            0.0
        } else {
            (square / frames as f32).sqrt()
        }
    }
}

/// The meters as the editor takes them: peaks since its last read.
#[derive(Debug, Clone, Copy, Default)]
pub struct MeterReading {
    pub in_peak: f32,
    pub in_rms: f32,
    pub out_peak: f32,
    pub out_rms: f32,
}

pub struct Shared {
    /// Even when the slots are whole; odd while the audio thread writes them.
    telemetry_seq: AtomicU32,
    telemetry: [AtomicU32; TELEMETRY_SLOTS],
    // Peaks are the largest since the editor last took them: non-negative
    // floats order the same as their bits, so `fetch_max` on the bits works.
    in_peak: AtomicU32,
    out_peak: AtomicU32,
    in_rms: AtomicU32,
    out_rms: AtomicU32,
    sample_rate: AtomicU32,
    block: AtomicU32,
    latency: AtomicU32,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            telemetry_seq: AtomicU32::new(0),
            telemetry: std::array::from_fn(|_| AtomicU32::new(0)),
            in_peak: AtomicU32::new(0),
            out_peak: AtomicU32::new(0),
            in_rms: AtomicU32::new(0),
            out_rms: AtomicU32::new(0),
            sample_rate: AtomicU32::new(0),
            block: AtomicU32::new(0),
            latency: AtomicU32::new(0),
        }
    }
}

impl Shared {
    pub fn set_stream(&self, sample_rate: f32, max_block: u32) {
        self.sample_rate
            .store(sample_rate.round() as u32, Ordering::Relaxed);
        self.block.store(max_block, Ordering::Relaxed);
    }

    pub fn set_latency(&self, samples: u32) {
        self.latency.store(samples, Ordering::Relaxed);
    }

    /// `(sample_rate, max_block, latency)`.
    pub fn stream(&self) -> (u32, u32, u32) {
        (
            self.sample_rate.load(Ordering::Relaxed),
            self.block.load(Ordering::Relaxed),
            self.latency.load(Ordering::Relaxed),
        )
    }

    /// Audio thread.
    pub fn publish_meter(&self, meter: &BlockMeter) {
        let finite = |value: f32| if value.is_finite() { value } else { 0.0 };
        self.in_peak
            .fetch_max(finite(meter.in_peak).to_bits(), Ordering::Relaxed);
        self.out_peak
            .fetch_max(finite(meter.out_peak).to_bits(), Ordering::Relaxed);
        self.in_rms.store(
            finite(BlockMeter::rms(meter.in_square, meter.frames)).to_bits(),
            Ordering::Relaxed,
        );
        self.out_rms.store(
            finite(BlockMeter::rms(meter.out_square, meter.frames)).to_bits(),
            Ordering::Relaxed,
        );
    }

    /// Editor thread: the peaks since the last call, which it resets.
    pub fn take_meter(&self) -> MeterReading {
        let load = |slot: &AtomicU32| f32::from_bits(slot.load(Ordering::Relaxed));
        MeterReading {
            in_peak: f32::from_bits(self.in_peak.swap(0, Ordering::Relaxed)),
            out_peak: f32::from_bits(self.out_peak.swap(0, Ordering::Relaxed)),
            in_rms: load(&self.in_rms),
            out_rms: load(&self.out_rms),
        }
    }

    /// Audio thread, the only writer.
    pub fn publish_telemetry(&self, slots: &[f32; TELEMETRY_SLOTS]) {
        let seq = self.telemetry_seq.load(Ordering::Relaxed);
        self.telemetry_seq
            .store(seq.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        for (slot, value) in self.telemetry.iter().zip(slots) {
            slot.store(value.to_bits(), Ordering::Relaxed);
        }
        self.telemetry_seq
            .store(seq.wrapping_add(2), Ordering::Release);
    }

    /// Editor thread: the newest whole telemetry block and its publish
    /// number, or `None` if the audio thread kept writing over the read.
    pub fn telemetry(&self) -> Option<(u32, [f32; TELEMETRY_SLOTS])> {
        for _ in 0..4 {
            let before = self.telemetry_seq.load(Ordering::Acquire);
            if before & 1 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let block = std::array::from_fn(|index| {
                f32::from_bits(self.telemetry[index].load(Ordering::Relaxed))
            });
            fence(Ordering::Acquire);
            if self.telemetry_seq.load(Ordering::Relaxed) == before {
                return Some((before / 2, block));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peaks_hold_until_taken() {
        let shared = Shared::default();
        let mut loud = BlockMeter::default();
        loud.input(0.8, -0.2);
        loud.output(0.5, 0.1);
        shared.publish_meter(&loud);
        let mut quiet = BlockMeter::default();
        quiet.input(0.1, 0.1);
        quiet.output(0.1, 0.1);
        shared.publish_meter(&quiet);

        let reading = shared.take_meter();
        assert_eq!(reading.in_peak, 0.8);
        assert_eq!(reading.out_peak, 0.5);
        assert_eq!(shared.take_meter().in_peak, 0.0);
    }

    #[test]
    fn each_publish_is_a_new_whole_block() {
        let shared = Shared::default();
        let first = [1.0; TELEMETRY_SLOTS];
        shared.publish_telemetry(&first);
        let (seq, block) = shared.telemetry().unwrap();
        assert_eq!(block, first);
        shared.publish_telemetry(&[2.0; TELEMETRY_SLOTS]);
        let (next, block) = shared.telemetry().unwrap();
        assert_eq!(next, seq + 1);
        assert_eq!(block, [2.0; TELEMETRY_SLOTS]);
    }
}

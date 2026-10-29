//! The editor's instrument tuner: monophonic pitch of the rig's input.
//!
//! Adapted from WhiteSharp's voice detector
//! (`crates/BuiltinAudioPlugins/crates/whitesharp/src/detect.rs`): the input
//! is low-passed and decimated to about 12 kHz, a YIN estimate (cumulative-
//! mean-normalised difference function, first dip under the threshold) is
//! taken over two periods of the lowest note about 30 times a second, and the
//! period is refined at the full rate with a parabola through the plain
//! difference function, which lands a 1 kHz tone within a cent. The voice
//! tracker's octave-continuity rules are dropped: a tuner shows one held note.
//!
//! A frame quieter than [`GATE`], or whose power lies mostly above the
//! analysis band (pick noise, hiss), reports no pitch. A confident reading is
//! smoothed toward the next one inside a semitone and jumps beyond it, and is
//! held through a few unvoiced estimates so the readout does not flicker
//! between notes.
//!
//! Every buffer is sized at construction; [`Tuner::push`] never allocates.
//! The difference function costs about `(rate / LOWEST_HZ)²` multiply-adds
//! per estimate — ~160 k at the decimated rate — so it only runs while the
//! editor's tuner is on.

use biquad::{Biquad, Coefficients, DirectForm1, ToHertz, Type};

/// Rate the detector works at, roughly.
const ANALYSIS_RATE: f32 = 12_000.0;
/// Highest frequency the decimation filter keeps.
const ANALYSIS_BAND_HZ: f32 = 4_000.0;
/// The range a reading can land in: a 5-string bass's low B (30.9 Hz) to the
/// top of a guitar's neck and past it.
pub const LOWEST_HZ: f32 = 30.0;
pub const HIGHEST_HZ: f32 = 1_600.0;
/// Estimates per second.
const UPDATES_PER_SECOND: f32 = 30.0;
/// Below this RMS the frame is silence.
const GATE: f32 = 0.002;
/// When less of the input's power than this lies under the analysis band,
/// the frame is hiss or pick noise.
const LOW_BAND_SHARE: f32 = 0.3;
/// How long the band powers are averaged over.
const POWER_MS: f32 = 10.0;
/// The YIN dip a frame must reach to count as pitched.
const THRESHOLD: f32 = 0.15;
/// Unvoiced estimates the last reading survives.
const HOLD_HOPS: u32 = 3;
/// Within this many cents of the last reading a new one is smoothed into it.
const SMOOTH_CENTS: f32 = 100.0;
/// How far each estimate moves the smoothed reading toward the new one.
const SMOOTHING: f32 = 0.5;

/// One frame's finding.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(super) struct Reading {
    /// Hz, 0 for no confident pitch.
    pub hz: f32,
    /// 0–1: how periodic the frame is (1 − the YIN dip).
    pub clarity: f32,
}

pub(super) struct Tuner {
    sample_rate: f32,
    decimation: usize,
    filters: [DirectForm1<f32>; 2],
    phase: usize,
    /// Decimated history, a ring.
    ring: Box<[f32]>,
    write: usize,
    /// Contiguous copy of the newest frame, and the difference function.
    frame: Box<[f32]>,
    diff: Box<[f32]>,
    /// Decimated samples between estimates, and the count since the last.
    hop: usize,
    since: usize,
    tau_min: usize,
    tau_max: usize,
    /// Full-rate history of the filtered input, for the refinement.
    full: Box<[f32]>,
    full_write: usize,
    power_all: f32,
    power_low: f32,
    power_step: f32,
    /// What the editor reads.
    reading: Reading,
    /// Unvoiced estimates since the last confident one.
    unvoiced: u32,
}

impl Tuner {
    pub(super) fn new(sample_rate: f32) -> Self {
        let sample_rate = sample_rate.max(1.0);
        let decimation = ((sample_rate / ANALYSIS_RATE).round() as usize).max(1);
        let rate = sample_rate / decimation as f32;
        let longest = (rate / LOWEST_HZ).ceil() as usize + 2;
        let filter = || {
            let c = Coefficients::<f32>::from_params(
                Type::LowPass,
                sample_rate.hz(),
                ANALYSIS_BAND_HZ.min(rate * 0.45).hz(),
                std::f32::consts::FRAC_1_SQRT_2,
            )
            .unwrap_or(Coefficients {
                a1: 0.0,
                a2: 0.0,
                b0: 1.0,
                b1: 0.0,
                b2: 0.0,
            });
            DirectForm1::<f32>::new(c)
        };
        let tau_max = longest - 2;
        Self {
            sample_rate,
            decimation,
            filters: [filter(), filter()],
            phase: 0,
            ring: vec![0.0; (2 * longest + 8).next_power_of_two()].into_boxed_slice(),
            write: 0,
            frame: vec![0.0; 2 * longest].into_boxed_slice(),
            diff: vec![0.0; longest + 2].into_boxed_slice(),
            hop: ((rate / UPDATES_PER_SECOND).round() as usize).max(1),
            since: 0,
            tau_min: ((rate / HIGHEST_HZ).floor() as usize).clamp(2, tau_max - 2),
            tau_max,
            full: vec![0.0; (2 * longest * decimation + 64).next_power_of_two()].into_boxed_slice(),
            full_write: 0,
            power_all: 0.0,
            power_low: 0.0,
            power_step: 1.0 - (-1.0 / (POWER_MS * 0.001 * sample_rate).max(1.0)).exp(),
            reading: Reading::default(),
            unvoiced: 0,
        }
    }

    pub(super) fn sample_rate(&self) -> f32 {
        self.sample_rate
    }

    pub(super) fn reading(&self) -> Reading {
        self.reading
    }

    /// Back to silence: history, filters and the reading.
    pub(super) fn reset(&mut self) {
        for filter in self.filters.iter_mut() {
            filter.reset_state();
        }
        self.ring.fill(0.0);
        self.full.fill(0.0);
        self.write = 0;
        self.full_write = 0;
        self.phase = 0;
        self.since = 0;
        self.power_all = 0.0;
        self.power_low = 0.0;
        self.reading = Reading::default();
        self.unvoiced = 0;
    }

    /// Takes one full-rate sample; an estimate is made when one is due.
    #[inline]
    pub(super) fn push(&mut self, sample: f32) {
        let first = self.filters[0].run(sample);
        let filtered = builtin_dsp_core::flush_denormal(self.filters[1].run(first));
        let k = self.power_step;
        self.power_all = builtin_dsp_core::flush_denormal(
            self.power_all + (sample * sample - self.power_all) * k,
        );
        self.power_low = builtin_dsp_core::flush_denormal(
            self.power_low + (filtered * filtered - self.power_low) * k,
        );
        let full_mask = self.full.len() - 1;
        self.full[self.full_write] = filtered;
        self.full_write = (self.full_write + 1) & full_mask;
        self.phase += 1;
        if self.phase < self.decimation {
            return;
        }
        self.phase = 0;
        let mask = self.ring.len() - 1;
        self.ring[self.write] = filtered;
        self.write = (self.write + 1) & mask;
        self.since += 1;
        if self.since < self.hop {
            return;
        }
        self.since = 0;
        let found = self.estimate();
        self.take(found);
    }

    /// Folds one estimate into the reading.
    fn take(&mut self, (hz, clarity): (Option<f32>, f32)) {
        match hz {
            Some(hz) => {
                self.unvoiced = 0;
                let last = self.reading.hz;
                let hz = if last > 0.0 && (1_200.0 * (hz / last).log2()).abs() < SMOOTH_CENTS {
                    // Geometric: equal steps in cents.
                    last * (hz / last).powf(SMOOTHING)
                } else {
                    hz
                };
                self.reading = Reading { hz, clarity };
            }
            None => {
                self.unvoiced += 1;
                if self.unvoiced > HOLD_HOPS {
                    self.reading = Reading { hz: 0.0, clarity };
                } else {
                    self.reading.clarity = clarity;
                }
            }
        }
    }

    /// The newest frame's pitch, if it has one, and its clarity.
    fn estimate(&mut self) -> (Option<f32>, f32) {
        let (tau_min, tau_max) = (self.tau_min, self.tau_max);
        let width = tau_max;
        let len = width + tau_max;
        let mask = self.ring.len() - 1;
        let start = self.write.wrapping_sub(len);
        let mut energy = 0.0;
        for i in 0..len {
            let x = self.ring[start.wrapping_add(i) & mask];
            self.frame[i] = x;
            energy += x * x;
        }
        if (energy / len as f32).sqrt() < GATE || self.power_low < LOW_BAND_SHARE * self.power_all {
            return (None, 0.0);
        }

        let frame = &self.frame[..len];
        let diff = &mut self.diff[..=tau_max];
        diff[0] = 1.0;
        let mut running = 0.0;
        for tau in 1..=tau_max {
            let mut sum = 0.0;
            for j in 0..width {
                let d = frame[j] - frame[j + tau];
                sum += d * d;
            }
            running += sum;
            diff[tau] = if running > 0.0 {
                sum * tau as f32 / running
            } else {
                1.0
            };
        }

        // The first dip under the threshold, followed to its bottom.
        let low = tau_min.max(1);
        let mut chosen = None;
        let mut tau = low;
        while tau < tau_max {
            if diff[tau] < THRESHOLD {
                while tau + 1 < tau_max && diff[tau + 1] < diff[tau] {
                    tau += 1;
                }
                chosen = Some(tau);
                break;
            }
            tau += 1;
        }
        let deepest = (low..tau_max)
            .min_by(|a, b| diff[*a].total_cmp(&diff[*b]))
            .unwrap_or(tau_min);
        let tau = chosen.unwrap_or(deepest);
        let clarity = (1.0 - diff[tau]).clamp(0.0, 1.0);
        if chosen.is_none() || tau <= 1 || tau + 1 > tau_max {
            return (None, clarity);
        }
        let (a, b, c) = (diff[tau - 1], diff[tau], diff[tau + 1]);
        let denominator = a - 2.0 * b + c;
        let shift = if denominator.abs() > 1.0e-9 {
            (0.5 * (a - c) / denominator).clamp(-0.5, 0.5)
        } else {
            0.0
        };
        let coarse = (tau as f32 + shift) * self.decimation as f32;
        let period = self.refine(coarse);
        let hz = self.sample_rate / period.max(1.0);
        if (LOWEST_HZ * 0.97..=HIGHEST_HZ * 1.03).contains(&hz) {
            (Some(hz), clarity)
        } else {
            (None, clarity)
        }
    }

    /// The period near `coarse` (full-rate samples), found again at the full
    /// rate: the plain difference function over a decimation step either
    /// side, and a parabola through its minimum.
    fn refine(&self, coarse: f32) -> f32 {
        let step = self.decimation as f32;
        if self.decimation == 1 {
            return coarse;
        }
        let low = ((coarse - step).floor() as usize).max(2);
        let high = (coarse + step).ceil() as usize;
        if high + 68 >= self.full.len() {
            return coarse;
        }
        let mask = self.full.len() - 1;
        let width = ((2.0 * coarse) as usize).clamp(64, self.full.len() - high - 4);
        let start = self.full_write.wrapping_sub(width + high);
        let at = |i: usize| self.full[start.wrapping_add(i) & mask];
        let difference = |lag: usize| -> f32 {
            let mut sum = 0.0;
            for j in 0..width {
                let d = at(j) - at(j + lag);
                sum += d * d;
            }
            sum
        };
        let mut best = (low, f32::MAX);
        for lag in low..=high {
            let d = difference(lag);
            if d < best.1 {
                best = (lag, d);
            }
        }
        let (lag, b) = best;
        if lag <= low || lag >= high {
            return coarse;
        }
        let (a, c) = (difference(lag - 1), difference(lag + 1));
        let denominator = a - 2.0 * b + c;
        if denominator.abs() <= f32::EPSILON * b.max(1.0) {
            return lag as f32;
        }
        lag as f32 + (0.5 * (a - c) / denominator).clamp(-0.5, 0.5)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settle(sr: f32, signal: impl Fn(usize) -> f32) -> Reading {
        let mut tuner = Tuner::new(sr);
        for n in 0..(sr as usize * 3 / 4) {
            tuner.push(signal(n));
        }
        tuner.reading()
    }

    #[test]
    fn sines_read_within_two_cents_across_the_range() {
        for &sr in &[44_100.0f32, 48_000.0, 96_000.0] {
            for hz in [41.2f32, 82.4, 110.0, 196.0, 329.6, 440.0, 1_000.0] {
                let reading = settle(sr, |n| {
                    0.3 * (std::f32::consts::TAU * hz * n as f32 / sr).sin()
                });
                assert!(reading.hz > 0.0, "{hz} Hz @ {sr}: no pitch");
                let cents = 1_200.0 * (reading.hz / hz).log2();
                assert!(cents.abs() < 2.0, "{hz} Hz @ {sr}: {cents:+.2} c");
                assert!(reading.clarity > 0.8, "{hz} Hz @ {sr}: {}", reading.clarity);
            }
        }
    }

    #[test]
    fn silence_and_noise_have_no_pitch() {
        assert_eq!(settle(48_000.0, |_| 0.0).hz, 0.0);
        let noise = |n: usize| {
            let mut x = (n as u32).wrapping_mul(2_654_435_761) ^ 0x9e37_79b9;
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x as f32 / u32::MAX as f32 - 0.5) * 0.6
        };
        assert_eq!(settle(48_000.0, noise).hz, 0.0);
    }
}

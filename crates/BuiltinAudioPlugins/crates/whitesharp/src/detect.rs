//! Realtime monophonic pitch detection.
//!
//! The input is low-passed and decimated to about 12 kHz — plenty for a
//! voice's fundamental — and every few milliseconds a YIN estimate is taken
//! over the newest stretch of it: the cumulative-mean-normalised difference
//! function, its first dip under the tracking threshold — then the period is
//! refined at the full rate, where a high voice's period spans enough
//! samples for a parabola through the dip to land within a cent.
//!
//! A voice is followed, not measured afresh each frame: once tracked it
//! stays voiced through shallower dips, and a pick an octave off the tracked
//! note gives way, for a few frames, to a dip near it. A frame whose power
//! lies mostly above the analysis band — a sibilant, a breath — is unvoiced
//! however periodic its low band looks.
//!
//! Every buffer is sized at construction for the widest range any input type
//! reaches, so switching type or analysing never allocates.

use biquad::{Biquad, Coefficients, DirectForm1, ToHertz, Type};

/// Rate the detector works at, roughly.
const ANALYSIS_RATE: f32 = 12_000.0;
/// Highest frequency the decimation filter keeps.
const ANALYSIS_BAND_HZ: f32 = 4_000.0;
/// Time between estimates.
pub const HOP_MS: f32 = 4.0;
/// Below this RMS the frame is silence, whatever its shape.
const GATE: f32 = 0.002;
/// When less of the input's power than this lies under the analysis band,
/// the frame is a sibilant or breath: unvoiced, whatever the low band does.
const LOW_BAND_SHARE: f32 = 0.3;
/// How long the band powers are averaged over.
const POWER_MS: f32 = 10.0;
/// A voice already tracked stays voiced through dips this much shallower
/// than the threshold a new one must reach.
const HOLD_MARGIN: f32 = 0.1;
/// How far from the tracked period, in octaves, a dip is the same note.
const CONTINUITY_OCTAVES: f32 = 0.25;
/// How much shallower than YIN's pick the tracked dip may be.
const CONTINUITY_SLACK: f32 = 0.12;
/// Estimates a note must hold within reach of itself before continuity
/// defends it: a phrase that starts on the wrong octave is not locked in.
const STEADY_HOPS: u32 = 8;
/// Estimates of sound with no period found the tracked note survives.
const LOST_HOPS: u32 = 3;
/// The most estimates in a row continuity overrules YIN's pick: an octave
/// error is a passing thing, a jump that lasts longer is sung.
const CONTINUITY_HOPS: u32 = 6;

/// The decimation and the decimated hop at `sample_rate`.
fn steps(sample_rate: f32) -> (usize, usize) {
    let decimation = ((sample_rate / ANALYSIS_RATE).round() as usize).max(1);
    let rate = sample_rate / decimation as f32;
    (
        decimation,
        ((rate * HOP_MS * 0.001).round() as usize).max(1),
    )
}

/// Seconds between estimates at `sample_rate` — and so between the pitch
/// readings the editor's graph receives.
pub fn hop_seconds(sample_rate: f32) -> f32 {
    let (decimation, hop) = steps(sample_rate);
    (decimation * hop) as f32 / sample_rate.max(1.0)
}

/// The [`Detector::centre_lag`] and [`Detector::hop_samples`] of a detector
/// at `sample_rate` ranged down to `lowest_hz`, without building one.
pub fn timing(sample_rate: f32, lowest_hz: f32) -> (usize, usize) {
    let (decimation, hop) = steps(sample_rate);
    let rate = sample_rate / decimation as f32;
    let tau_max = ((rate / lowest_hz).ceil() as usize).max(4);
    (tau_max * decimation, hop * decimation)
}

/// One estimate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Estimate {
    /// Period in full-rate samples; `None` when unvoiced.
    pub period: Option<f32>,
    /// 0–1: how periodic the frame is (1 − the YIN dip).
    pub clarity: f32,
}

#[derive(Debug, Clone)]
pub struct Detector {
    decimation: usize,
    /// The decimated rate.
    rate: f32,
    filters: [DirectForm1<f32>; 2],
    phase: usize,
    /// Decimated history, a ring.
    ring: Box<[f32]>,
    write: usize,
    /// Contiguous copy of the newest frame, and the difference functions.
    frame: Box<[f32]>,
    diff: Box<[f32]>,
    /// Decimated samples between estimates, and the count since the last.
    hop: usize,
    since: usize,
    /// The search range for the current input type, in decimated samples.
    tau_min: usize,
    tau_max: usize,
    /// Full-rate history of the filtered input, for the refinement.
    full: Box<[f32]>,
    full_write: usize,
    /// The last voiced period, in decimated samples; `None` once unvoiced.
    tracked: Option<f32>,
    /// Estimates in a row where continuity has overruled YIN's pick.
    overruled: u32,
    /// Estimates in a row the tracked period has stayed within reach of
    /// itself.
    steady: u32,
    /// Estimates in a row with sound but no period.
    lost: u32,
    /// Running power of the input, and of its low band.
    power_all: f32,
    power_low: f32,
    power_step: f32,
}

impl Detector {
    /// A detector at `sample_rate` able to reach `lowest_hz`.
    pub fn new(sample_rate: f32, lowest_hz: f32) -> Self {
        let (decimation, hop) = steps(sample_rate);
        let rate = sample_rate / decimation as f32;
        let longest = (rate / lowest_hz).ceil() as usize + 2;
        let filter = || {
            let c = Coefficients::<f32>::from_params(
                Type::LowPass,
                sample_rate.hz(),
                ANALYSIS_BAND_HZ.min(rate * 0.45).hz(),
                std::f32::consts::FRAC_1_SQRT_2,
            )
            .expect("a low-pass under Nyquist is always valid");
            DirectForm1::<f32>::new(c)
        };
        Self {
            decimation,
            rate,
            filters: [filter(), filter()],
            phase: 0,
            ring: vec![0.0; (2 * longest + 8).next_power_of_two()].into_boxed_slice(),
            write: 0,
            frame: vec![0.0; 2 * longest].into_boxed_slice(),
            diff: vec![0.0; longest + 2].into_boxed_slice(),
            hop,
            since: 0,
            tau_min: 2,
            tau_max: longest - 2,
            full: vec![0.0; (2 * longest * decimation + 64).next_power_of_two()].into_boxed_slice(),
            full_write: 0,
            tracked: None,
            overruled: 0,
            steady: 0,
            lost: 0,
            power_all: 0.0,
            power_low: 0.0,
            power_step: 1.0 - (-1.0 / (POWER_MS * 0.001 * sample_rate).max(1.0)).exp(),
        }
    }

    /// Full-rate samples between estimates.
    pub fn hop_samples(&self) -> usize {
        self.hop * self.decimation
    }

    /// How many full-rate samples an estimate's centre lags the newest input.
    pub fn centre_lag(&self) -> usize {
        self.tau_max * self.decimation
    }

    /// Narrows the search to `lowest_hz`–`highest_hz`.
    pub fn set_range(&mut self, lowest_hz: f32, highest_hz: f32) {
        let capacity = self.diff.len() - 2;
        self.tau_max = ((self.rate / lowest_hz).ceil() as usize).clamp(4, capacity);
        self.tau_min = ((self.rate / highest_hz).floor() as usize).clamp(2, self.tau_max - 2);
        self.forget();
    }

    pub fn reset(&mut self) {
        for filter in self.filters.iter_mut() {
            filter.reset_state();
        }
        self.ring.fill(0.0);
        self.write = 0;
        self.full.fill(0.0);
        self.full_write = 0;
        self.phase = 0;
        self.since = 0;
        self.power_all = 0.0;
        self.power_low = 0.0;
        self.forget();
    }

    /// Drops the tracked note: the next voiced frame starts afresh.
    fn forget(&mut self) {
        self.tracked = None;
        self.overruled = 0;
        self.steady = 0;
        self.lost = 0;
    }

    /// Silence or hiss: the note is over.
    fn unvoiced(&mut self, clarity: f32) -> Estimate {
        self.forget();
        Estimate {
            period: None,
            clarity,
        }
    }

    /// Sound with no period found: unvoiced, but the note is kept through a
    /// frame or two of it.
    fn lost(&mut self, clarity: f32) -> Estimate {
        self.lost += 1;
        if self.lost > LOST_HOPS {
            self.forget();
        }
        Estimate {
            period: None,
            clarity,
        }
    }

    /// Takes one full-rate sample. Returns an estimate when one is due.
    /// `threshold` is the YIN dip a frame must reach to count as voiced.
    #[inline]
    pub fn push(&mut self, sample: f32, threshold: f32) -> Option<Estimate> {
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
            return None;
        }
        self.phase = 0;
        let mask = self.ring.len() - 1;
        self.ring[self.write] = filtered;
        self.write = (self.write + 1) & mask;
        self.since += 1;
        if self.since < self.hop {
            return None;
        }
        self.since = 0;
        Some(self.estimate(threshold))
    }

    fn estimate(&mut self, threshold: f32) -> Estimate {
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
            return self.unvoiced(0.0);
        }

        // Difference function, then its cumulative-mean normalisation.
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

        // The first dip under the threshold, followed to its bottom; the
        // deepest point when none dips that far.
        let low = tau_min.max(1);
        let mut chosen = None;
        let mut tau = low;
        while tau < tau_max {
            if diff[tau] < threshold {
                while tau + 1 < tau_max && diff[tau + 1] < diff[tau] {
                    tau += 1;
                }
                chosen = Some(tau);
                break;
            }
            tau += 1;
        }

        // A voice already tracked is followed: the deepest dip near its
        // period counts at a looser threshold, so a rough patch mid-note
        // does not drop it. And once the note is steady, a pick an octave
        // or more off it is more often a harmonic winning a frame (or a
        // frame straddling a change of tone) than a leap: for a few
        // estimates the tracked period stands while it dips nearly as deep.
        if let Some(tracked) = self.tracked {
            let span = 2f32.powf(CONTINUITY_OCTAVES);
            let from = ((tracked / span).floor() as usize).max(low + 1);
            let to = ((tracked * span).ceil() as usize).min(tau_max - 1);
            let near = (from..=to)
                .filter(|&t| diff[t] <= diff[t - 1] && diff[t] <= diff[t + 1])
                .min_by(|a, b| diff[*a].total_cmp(&diff[*b]))
                .filter(|&t| diff[t] < threshold + HOLD_MARGIN);
            chosen = match (chosen, near) {
                (None, near) => {
                    self.overruled = 0;
                    near
                }
                (Some(first), Some(t))
                    if (first as f32 / tracked).log2().abs() > CONTINUITY_OCTAVES
                        && self.steady >= STEADY_HOPS
                        && self.overruled < CONTINUITY_HOPS
                        && diff[t] <= diff[first] + CONTINUITY_SLACK =>
                {
                    self.overruled += 1;
                    Some(t)
                }
                (first, _) => {
                    self.overruled = 0;
                    first
                }
            };
        }

        let deepest = (low..tau_max)
            .min_by(|a, b| diff[*a].total_cmp(&diff[*b]))
            .unwrap_or(tau_min);
        let tau = chosen.unwrap_or(deepest);
        let clarity = (1.0 - diff[tau]).clamp(0.0, 1.0);
        if chosen.is_none() || tau <= 1 || tau + 1 > tau_max {
            return self.lost(clarity);
        }

        let (a, b, c) = (diff[tau - 1], diff[tau], diff[tau + 1]);
        let denominator = a - 2.0 * b + c;
        let shift = if denominator.abs() > 1.0e-9 {
            (0.5 * (a - c) / denominator).clamp(-0.5, 0.5)
        } else {
            0.0
        };
        let found = tau as f32 + shift;
        self.steady = match self.tracked {
            Some(tracked) if (found / tracked).log2().abs() <= CONTINUITY_OCTAVES => {
                self.steady.saturating_add(1)
            }
            _ => 0,
        };
        self.tracked = Some(found);
        self.lost = 0;
        let coarse = (tau as f32 + shift) * self.decimation as f32;
        Estimate {
            period: Some(self.refine(coarse)),
            clarity,
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
        let mask = self.full.len() - 1;
        let width = ((2.0 * coarse) as usize).clamp(64, self.full.len() - high - 4);
        let start = self.full_write.wrapping_sub(width + high);
        let at = |i: usize| self.full[start.wrapping_add(i) & mask];
        let difference = |lag: usize| -> f32 {
            (0..width)
                .map(|j| {
                    let d = at(j) - at(j + lag);
                    d * d
                })
                .sum()
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

    fn detect(sr: f32, hz: f32, lowest: f32, highest: f32) -> Option<f32> {
        let mut detector = Detector::new(sr, 30.0);
        detector.set_range(lowest, highest);
        let mut last = None;
        for n in 0..(sr as usize / 2) {
            let t = n as f32 / sr;
            // A voice-like tone: a fundamental and falling harmonics.
            let x = (1..6)
                .map(|k| (std::f32::consts::TAU * hz * k as f32 * t).sin() / k as f32)
                .sum::<f32>()
                * 0.3;
            if let Some(estimate) = detector.push(x, 0.15) {
                last = Some(estimate);
            }
        }
        last.and_then(|e| e.period).map(|period| sr / period)
    }

    #[test]
    fn finds_the_fundamental_across_the_voice_range() {
        for &sr in &[44_100.0f32, 48_000.0, 96_000.0] {
            for hz in [82.4f32, 110.0, 196.0, 261.6, 440.0, 659.3, 987.8] {
                let found = detect(sr, hz, 60.0, 1_200.0).expect("voiced");
                let cents = 1_200.0 * (found / hz).log2();
                assert!(
                    cents.abs() < 1.5,
                    "{hz} Hz @ {sr}: found {found} ({cents:+.1} c)"
                );
            }
        }
    }

    #[test]
    fn silence_and_noise_are_unvoiced() {
        let mut detector = Detector::new(48_000.0, 30.0);
        detector.set_range(60.0, 1_200.0);
        let mut seed = 1u32;
        let mut voiced = 0;
        for n in 0..48_000 {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = (seed >> 9) as f32 / (1u32 << 23) as f32 - 0.5;
            let x = if n < 24_000 { 0.0 } else { noise * 0.5 };
            if let Some(estimate) = detector.push(x, 0.15) {
                voiced += usize::from(estimate.period.is_some());
            }
        }
        assert!(voiced < 5, "{voiced} noise frames called voiced");
    }

    #[test]
    fn a_passing_octave_error_does_not_jump_the_pitch() {
        // A held 150 Hz note whose odd harmonics drop 22 dB for 36 ms: the
        // waveform nearly repeats at half the period, which a frame-by-frame
        // YIN takes as the octave above.
        let sr = 48_000.0f32;
        let hz = 150.0f32;
        let mut detector = Detector::new(sr, 30.0);
        detector.set_range(60.0, 1_200.0);
        let mut worst = 0.0f32;
        for n in 0..(sr as usize) {
            let t = n as f32 / sr;
            let odd = if (0.5..0.536).contains(&t) { 0.08 } else { 1.0 };
            let x = (1..7)
                .map(|k| {
                    let level = if k % 2 == 1 { odd } else { 1.0 };
                    level * (std::f32::consts::TAU * hz * k as f32 * t).sin() / k as f32
                })
                .sum::<f32>()
                * 0.3;
            if let Some(period) = detector.push(x, 0.15).and_then(|e| e.period) {
                if t > 0.2 {
                    let cents = 1_200.0 * (sr / period / hz).log2();
                    worst = worst.max(cents.abs());
                }
            }
        }
        assert!(worst < 50.0, "jumped {worst:.0} c");
    }

    #[test]
    fn a_sung_octave_leap_is_followed_within_a_few_estimates() {
        let sr = 48_000.0f32;
        let mut detector = Detector::new(sr, 30.0);
        detector.set_range(60.0, 1_200.0);
        let mut phase = 0.0f32;
        let mut late = Vec::new();
        for n in 0..(sr as usize) {
            let t = n as f32 / sr;
            let hz = if t < 0.5 { 150.0 } else { 300.0 };
            phase = (phase + hz / sr).fract();
            let x = (1..6)
                .map(|k| (std::f32::consts::TAU * phase * k as f32).sin() / k as f32)
                .sum::<f32>()
                * 0.3;
            if let Some(estimate) = detector.push(x, 0.15) {
                // A frame's length after the leap, plus the continuity hold.
                if t > 0.5 + 0.034 + 0.025 {
                    late.extend(estimate.period.map(|p| sr / p));
                }
            }
        }
        assert!(!late.is_empty());
        for hz in late {
            assert!((hz / 300.0 - 1.0).abs() < 0.01, "still at {hz} Hz");
        }
    }

    #[test]
    fn a_sibilant_over_a_quiet_tone_is_unvoiced() {
        // Hiss above the analysis band, a tone 30 dB under it below: the
        // low band alone looks perfectly periodic.
        let sr = 48_000.0f32;
        let mut detector = Detector::new(sr, 30.0);
        detector.set_range(60.0, 1_200.0);
        let mut seed = 3u32;
        let (mut previous, mut before) = (0.0f32, 0.0f32);
        let mut voiced = 0;
        for n in 0..(sr as usize / 2) {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let white = (seed >> 9) as f32 / (1u32 << 23) as f32 - 0.5;
            let hiss = white - 2.0 * previous + before;
            (before, previous) = (previous, white);
            let tone = 0.02 * (std::f32::consts::TAU * 220.0 * n as f32 / sr).sin();
            if let Some(estimate) = detector.push(0.3 * hiss + tone, 0.15) {
                voiced += usize::from(estimate.period.is_some());
            }
        }
        assert!(voiced < 5, "{voiced} hiss frames called voiced");
    }
}

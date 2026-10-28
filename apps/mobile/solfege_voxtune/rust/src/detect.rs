//! Realtime monophonic pitch detection (decimated + full-rate refined YIN).
//! Borrowed from `Futureboard-release/crates/BuiltinAudioPlugins/crates/whitesharp/src/detect.rs`.

use biquad::{Biquad, Coefficients, DirectForm1, ToHertz, Type};
use crate::dsp_core::flush_denormal;

const ANALYSIS_RATE: f32 = 12_000.0;
const ANALYSIS_BAND_HZ: f32 = 4_000.0;
pub const HOP_MS: f32 = 4.0;
const GATE: f32 = 0.002;
const LOW_BAND_SHARE: f32 = 0.3;
const POWER_MS: f32 = 10.0;
const HOLD_MARGIN: f32 = 0.1;
const CONTINUITY_OCTAVES: f32 = 0.25;
const CONTINUITY_SLACK: f32 = 0.12;
const STEADY_HOPS: u32 = 8;
const LOST_HOPS: u32 = 3;
const CONTINUITY_HOPS: u32 = 6;

fn steps(sample_rate: f32) -> (usize, usize) {
    let decimation = ((sample_rate / ANALYSIS_RATE).round() as usize).max(1);
    let rate = sample_rate / decimation as f32;
    (
        decimation,
        ((rate * HOP_MS * 0.001).round() as usize).max(1),
    )
}

pub fn hop_seconds(sample_rate: f32) -> f32 {
    let (decimation, hop) = steps(sample_rate);
    (decimation * hop) as f32 / sample_rate.max(1.0)
}

pub fn timing(sample_rate: f32, lowest_hz: f32) -> (usize, usize) {
    let (decimation, hop) = steps(sample_rate);
    let rate = sample_rate / decimation as f32;
    let tau_max = ((rate / lowest_hz).ceil() as usize).max(4);
    (tau_max * decimation, hop * decimation)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Estimate {
    pub period: Option<f32>,
    pub clarity: f32,
}

#[derive(Debug, Clone)]
pub struct Detector {
    decimation: usize,
    rate: f32,
    filters: [DirectForm1<f32>; 2],
    phase: usize,
    ring: Box<[f32]>,
    write: usize,
    frame: Box<[f32]>,
    diff: Box<[f32]>,
    hop: usize,
    since: usize,
    tau_min: usize,
    tau_max: usize,
    full: Box<[f32]>,
    full_write: usize,
    tracked: Option<f32>,
    overruled: u32,
    steady: u32,
    lost: u32,
    power_all: f32,
    power_low: f32,
    power_step: f32,
}

impl Detector {
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

    pub fn hop_samples(&self) -> usize {
        self.hop * self.decimation
    }

    pub fn centre_lag(&self) -> usize {
        self.tau_max * self.decimation
    }

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

    fn forget(&mut self) {
        self.tracked = None;
        self.overruled = 0;
        self.steady = 0;
        self.lost = 0;
    }

    fn unvoiced(&mut self, clarity: f32) -> Estimate {
        self.forget();
        Estimate {
            period: None,
            clarity,
        }
    }

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

    #[inline]
    pub fn push(&mut self, sample: f32, threshold: f32) -> Option<Estimate> {
        let first = self.filters[0].run(sample);
        let filtered = flush_denormal(self.filters[1].run(first));
        let k = self.power_step;
        self.power_all = flush_denormal(
            self.power_all + (sample * sample - self.power_all) * k,
        );
        self.power_low = flush_denormal(
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

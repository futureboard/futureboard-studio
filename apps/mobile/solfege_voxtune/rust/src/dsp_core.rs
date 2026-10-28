//! Shared descriptors, math, delay lines, biquad EQ, and dynamics helpers.
//! Borrowed from `Futureboard-release/crates/BuiltinAudioPlugins/crates/builtin_dsp_core`.

use biquad::{Biquad, Coefficients, DirectForm1, ToHertz, Type};

/// Realtime-safe stereo insert contract.
pub trait StereoEffect: Send {
    fn reset(&mut self);
    fn set_sample_rate(&mut self, sample_rate: f32);
    fn process_stereo(&mut self, left: f32, right: f32) -> (f32, f32);
}

#[inline]
pub fn clamp(value: f32, min: f32, max: f32) -> f32 {
    value.max(min).min(max)
}

#[inline]
pub fn db_to_linear(db: f32) -> f32 {
    10.0f32.powf(db / 20.0)
}

#[inline]
pub fn linear_to_db(linear: f32) -> f32 {
    20.0 * linear.max(1.0e-12).log10()
}

#[inline]
pub fn mix(dry: f32, wet: f32, amount: f32) -> f32 {
    let a = clamp(amount, 0.0, 1.0);
    dry * (1.0 - a) + wet * a
}

/// One-pole coefficient for attack/release times in seconds.
#[inline]
pub fn time_constant(sample_rate: f32, seconds: f32) -> f32 {
    let samples = sample_rate.max(1.0) * seconds.max(1.0e-6);
    (-1.0 / samples).exp()
}

/// Below this magnitude a filter or envelope state is silence: 1e-25 is −500 dBFS.
pub const DENORMAL_FLOOR: f32 = 1.0e-25;

/// Snap a decaying recursive state to zero before it reaches the subnormal range.
#[inline]
pub fn flush_denormal(x: f32) -> f32 {
    if x.abs() < DENORMAL_FLOOR { 0.0 } else { x }
}

/// A value that moves toward its target by a fixed share each sample.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Smoothed {
    pub value: f32,
    pub target: f32,
}

impl Smoothed {
    pub fn at(value: f32) -> Self {
        Self {
            value,
            target: value,
        }
    }

    #[inline]
    pub fn next(&mut self, step: f32) -> f32 {
        self.value += (self.target - self.value) * step;
        self.value
    }

    pub fn settle(&mut self) {
        self.value = self.target;
    }
}

/// Per-sample share for a one-pole smoother with time constant `ms`.
pub fn smoothing_step(ms: f32, sample_rate: f32) -> f32 {
    1.0 - (-1.0 / (ms * 0.001 * sample_rate).max(1.0)).exp()
}

/// A power-of-two ring read behind its write head.
#[derive(Debug, Clone)]
pub struct DelayRing {
    buffer: Box<[f32]>,
    mask: usize,
    write: usize,
}

impl DelayRing {
    pub fn new(capacity: usize) -> Self {
        let len = capacity.max(8).next_power_of_two();
        Self {
            buffer: vec![0.0; len].into_boxed_slice(),
            mask: len - 1,
            write: 0,
        }
    }

    pub fn for_ms(ms: f32, sample_rate: f32) -> Self {
        Self::new((ms * 0.001 * sample_rate).ceil() as usize + 8)
    }

    pub fn len(&self) -> usize {
        self.buffer.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    pub fn clear(&mut self) {
        self.buffer.fill(0.0);
        self.write = 0;
    }

    pub fn max_cubic_delay(&self) -> f32 {
        (self.buffer.len() - 4) as f32
    }

    #[inline]
    pub fn push(&mut self, sample: f32) {
        self.buffer[self.write] = sample;
        self.write = (self.write + 1) & self.mask;
    }

    #[inline]
    pub fn at(&self, delay: usize) -> f32 {
        self.buffer[self.write.wrapping_sub(delay) & self.mask]
    }

    #[inline]
    pub fn read_linear(&self, delay: f32) -> f32 {
        let delay = clamp(delay, 1.0, (self.buffer.len() - 2) as f32);
        let whole = delay as usize;
        let frac = delay - whole as f32;
        let newer = self.at(whole);
        newer + (self.at(whole + 1) - newer) * frac
    }

    #[inline]
    pub fn read_cubic(&self, delay: f32) -> f32 {
        let delay = clamp(delay, 2.0, self.max_cubic_delay());
        let whole = delay as usize;
        let f = delay - whole as f32;
        let (p0, p1, p2, p3) = (
            self.at(whole - 1),
            self.at(whole),
            self.at(whole + 1),
            self.at(whole + 2),
        );
        let (fm1, fm2, fp1) = (f - 1.0, f - 2.0, f + 1.0);
        let c0 = -f * fm1 * fm2 * (1.0 / 6.0);
        let c1 = fp1 * fm1 * fm2 * 0.5;
        let c2 = -fp1 * f * fm2 * 0.5;
        let c3 = fp1 * f * fm1 * (1.0 / 6.0);
        p0 * c0 + p1 * c1 + p2 * c2 + p3 * c3
    }
}

/// Fixed-length Schroeder allpass.
#[derive(Debug, Clone)]
pub struct Allpass {
    buffer: Box<[f32]>,
    pos: usize,
}

impl Allpass {
    pub fn new(length: usize) -> Self {
        Self {
            buffer: vec![0.0; length.max(1)].into_boxed_slice(),
            pos: 0,
        }
    }

    pub fn for_ms(ms: f32, sample_rate: f32) -> Self {
        Self::new(((ms * 0.001 * sample_rate).round() as usize).max(1))
    }

    pub fn clear(&mut self) {
        self.buffer.fill(0.0);
        self.pos = 0;
    }

    #[inline]
    pub fn process(&mut self, input: f32, gain: f32) -> f32 {
        let delayed = self.buffer[self.pos];
        let stored = flush_denormal(input + delayed * gain);
        self.buffer[self.pos] = stored;
        self.pos += 1;
        if self.pos == self.buffer.len() {
            self.pos = 0;
        }
        delayed - stored * gain
    }
}

/// Soft-knee peak compressor (Giannoulis / Reiss style gain computer).
#[derive(Debug, Clone)]
pub struct SoftKneeCompressor {
    sample_rate: f32,
    threshold_db: f32,
    ratio: f32,
    knee_db: f32,
    attack_coeff: f32,
    release_coeff: f32,
    makeup_linear: f32,
    envelope: f32,
    sidechain_hpf: Option<DirectForm1<f32>>,
    sc_cutoff_hz: f32,
}

impl SoftKneeCompressor {
    pub fn new(sample_rate: f32) -> Self {
        let mut dsp = Self {
            sample_rate: sample_rate.max(1.0),
            threshold_db: -18.0,
            ratio: 3.0,
            knee_db: 6.0,
            attack_coeff: 0.0,
            release_coeff: 0.0,
            makeup_linear: 1.0,
            envelope: 0.0,
            sidechain_hpf: None,
            sc_cutoff_hz: 0.0,
        };
        dsp.set_timing(0.008, 0.09);
        dsp
    }

    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        self.rebuild_sidechain();
    }

    pub fn set_timing(&mut self, attack_sec: f32, release_sec: f32) {
        self.attack_coeff = time_constant(self.sample_rate, attack_sec);
        self.release_coeff = time_constant(self.sample_rate, release_sec);
    }

    pub fn set_curve(&mut self, threshold_db: f32, ratio: f32, knee_db: f32, makeup_db: f32) {
        self.threshold_db = threshold_db;
        self.ratio = ratio.max(1.0);
        self.knee_db = knee_db.max(0.0);
        self.makeup_linear = db_to_linear(makeup_db);
    }

    pub fn set_sidechain_hpf(&mut self, cutoff_hz: f32) {
        self.sc_cutoff_hz = cutoff_hz.max(0.0);
        self.rebuild_sidechain();
    }

    pub fn reset(&mut self) {
        self.envelope = 0.0;
        if let Some(filter) = self.sidechain_hpf.as_mut() {
            filter.reset_state();
        }
    }

    pub fn gain_reduction_db(&self) -> f32 {
        let level_db = linear_to_db(self.envelope.max(1.0e-12));
        -self.compute_gr_db(level_db)
    }

    #[inline]
    pub fn process_mono(&mut self, input: f32) -> f32 {
        let detected = match self.sidechain_hpf.as_mut() {
            Some(hpf) => hpf.run(input),
            None => input,
        };
        let level = detected.abs();
        let coeff = if level > self.envelope {
            self.attack_coeff
        } else {
            self.release_coeff
        };
        self.envelope = flush_denormal(coeff * self.envelope + (1.0 - coeff) * level);
        let level_db = linear_to_db(self.envelope.max(1.0e-12));
        let gr_db = self.compute_gr_db(level_db);
        input * db_to_linear(gr_db) * self.makeup_linear
    }

    #[inline]
    pub fn process_stereo_linked(&mut self, left: f32, right: f32) -> (f32, f32) {
        let detector = left.abs().max(right.abs());
        let detected = match self.sidechain_hpf.as_mut() {
            Some(hpf) => hpf.run(detector),
            None => detector,
        };
        let level = detected.abs();
        let coeff = if level > self.envelope {
            self.attack_coeff
        } else {
            self.release_coeff
        };
        self.envelope = flush_denormal(coeff * self.envelope + (1.0 - coeff) * level);
        let level_db = linear_to_db(self.envelope.max(1.0e-12));
        let gr_db = self.compute_gr_db(level_db);
        let gain = db_to_linear(gr_db) * self.makeup_linear;
        (left * gain, right * gain)
    }

    #[inline]
    fn compute_gr_db(&self, level_db: f32) -> f32 {
        let over = level_db - self.threshold_db;
        let half_knee = self.knee_db * 0.5;
        let compressed = if over <= -half_knee {
            level_db
        } else if over >= half_knee {
            self.threshold_db + over / self.ratio
        } else {
            let t = over + half_knee;
            level_db + (1.0 / self.ratio - 1.0) * (t * t) / (2.0 * self.knee_db.max(1.0e-6))
        };
        compressed - level_db
    }

    fn rebuild_sidechain(&mut self) {
        if self.sc_cutoff_hz < 10.0 {
            self.sidechain_hpf = None;
            return;
        }
        let fs = self.sample_rate.hz();
        let f0 = self
            .sc_cutoff_hz
            .min(max_filter_frequency(self.sample_rate))
            .hz();
        let Ok(coeffs) = Coefficients::<f32>::from_params(Type::HighPass, fs, f0, 0.707) else {
            self.sidechain_hpf = None;
            return;
        };
        self.sidechain_hpf = Some(DirectForm1::<f32>::new(coeffs));
    }
}

pub const MAX_FILTER_FREQUENCY_RATIO: f32 = 0.49;

pub fn max_filter_frequency(sample_rate: f32) -> f32 {
    sample_rate.max(1.0) * MAX_FILTER_FREQUENCY_RATIO
}

pub fn make_eq_coefficients(
    kind: &str,
    freq_hz: f32,
    gain_db: f32,
    q: f32,
    sample_rate: f32,
) -> Option<Coefficients<f32>> {
    let fs = sample_rate.max(1.0);
    let f0 = clamp(freq_hz, 10.0, max_filter_frequency(fs));
    let q = clamp(q, 0.1, 12.0);
    let filter_type = match kind {
        "bell" | "peak" | "peaking" => Type::PeakingEQ(gain_db),
        "lowshelf" | "ls" => Type::LowShelf(gain_db),
        "highshelf" | "hs" => Type::HighShelf(gain_db),
        "lowpass" | "lp" => Type::LowPass,
        "highpass" | "hp" => Type::HighPass,
        "notch" => Type::Notch,
        "bandpass" | "bp" => Type::BandPass,
        _ => return None,
    };
    Coefficients::<f32>::from_params(filter_type, fs.hz(), f0.hz(), q).ok()
}

pub fn make_eq_biquad(
    kind: &str,
    freq_hz: f32,
    gain_db: f32,
    q: f32,
    sample_rate: f32,
) -> Option<DirectForm1<f32>> {
    make_eq_coefficients(kind, freq_hz, gain_db, q, sample_rate).map(DirectForm1::<f32>::new)
}

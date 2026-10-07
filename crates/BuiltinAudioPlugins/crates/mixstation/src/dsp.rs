//! Allocation-free signal-processing building blocks for MixStation.

use builtin_dsp_core::{clamp, db_to_linear, flush_denormal, linear_to_db, time_constant};

const SMOOTH_SECONDS: f32 = 0.020;

#[derive(Debug, Clone, Copy)]
pub struct SmoothedGain {
    value: f32,
    target: f32,
    coeff: f32,
}

impl SmoothedGain {
    pub fn new(sample_rate: f32, db: f32) -> Self {
        let value = db_to_linear(db);
        Self {
            value,
            target: value,
            coeff: time_constant(sample_rate.max(1.0), SMOOTH_SECONDS),
        }
    }

    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        self.coeff = time_constant(sample_rate.max(1.0), SMOOTH_SECONDS);
    }

    pub fn set_db(&mut self, db: f32) {
        self.target = db_to_linear(db);
    }

    pub fn snap_db(&mut self, db: f32) {
        self.target = db_to_linear(db);
        self.value = self.target;
    }

    #[inline]
    pub fn next(&mut self) -> f32 {
        self.value = flush_denormal(self.coeff * self.value + (1.0 - self.coeff) * self.target);
        self.value
    }
}

/// Transposed direct-form-II biquad. Coefficients are replaced without
/// clearing history, avoiding a state discontinuity when an EQ control moves.
#[derive(Debug, Clone, Copy)]
pub struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: [f32; 2],
    z2: [f32; 2],
}

impl Biquad {
    pub const fn identity() -> Self {
        Self {
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
            z1: [0.0; 2],
            z2: [0.0; 2],
        }
    }

    /// The section's gain at `frequency`, in dB, at `sample_rate`. Evaluated
    /// in double precision so a curve stays smooth at the bottom octaves.
    pub fn magnitude_db(&self, sample_rate: f32, frequency: f32) -> f32 {
        let w = std::f64::consts::TAU * f64::from(frequency) / f64::from(sample_rate.max(1.0));
        let (cos1, sin1) = (w.cos(), w.sin());
        let (cos2, sin2) = ((2.0 * w).cos(), (2.0 * w).sin());
        let (b0, b1, b2) = (f64::from(self.b0), f64::from(self.b1), f64::from(self.b2));
        let (a1, a2) = (f64::from(self.a1), f64::from(self.a2));
        let num_re = b0 + b1 * cos1 + b2 * cos2;
        let num_im = -(b1 * sin1 + b2 * sin2);
        let den_re = 1.0 + a1 * cos1 + a2 * cos2;
        let den_im = -(a1 * sin1 + a2 * sin2);
        let power =
            (num_re * num_re + num_im * num_im) / (den_re * den_re + den_im * den_im).max(1.0e-30);
        (10.0 * power.max(1.0e-30).log10()) as f32
    }

    fn set_normalized(&mut self, b0: f32, b1: f32, b2: f32, a0: f32, a1: f32, a2: f32) {
        let inverse = 1.0 / a0;
        self.b0 = b0 * inverse;
        self.b1 = b1 * inverse;
        self.b2 = b2 * inverse;
        self.a1 = a1 * inverse;
        self.a2 = a2 * inverse;
    }

    pub fn set_high_pass(&mut self, sample_rate: f32, frequency: f32, q: f32) {
        let (cos, _, alpha) = common(sample_rate, frequency, q);
        self.set_normalized(
            (1.0 + cos) * 0.5,
            -(1.0 + cos),
            (1.0 + cos) * 0.5,
            1.0 + alpha,
            -2.0 * cos,
            1.0 - alpha,
        );
    }

    pub fn set_low_pass(&mut self, sample_rate: f32, frequency: f32, q: f32) {
        let (cos, _, alpha) = common(sample_rate, frequency, q);
        self.set_normalized(
            (1.0 - cos) * 0.5,
            1.0 - cos,
            (1.0 - cos) * 0.5,
            1.0 + alpha,
            -2.0 * cos,
            1.0 - alpha,
        );
    }

    pub fn set_peak(&mut self, sample_rate: f32, frequency: f32, gain_db: f32, q: f32) {
        let a = 10.0f32.powf(gain_db / 40.0);
        let (cos, _, alpha) = common(sample_rate, frequency, q);
        self.set_normalized(
            1.0 + alpha * a,
            -2.0 * cos,
            1.0 - alpha * a,
            1.0 + alpha / a,
            -2.0 * cos,
            1.0 - alpha / a,
        );
    }

    pub fn set_low_shelf(&mut self, sample_rate: f32, frequency: f32, gain_db: f32) {
        let a = 10.0f32.powf(gain_db / 40.0);
        let w = omega(sample_rate, frequency);
        let cos = w.cos();
        let sin = w.sin();
        let root = a.sqrt();
        let alpha = sin * std::f32::consts::FRAC_1_SQRT_2;
        let two = 2.0 * root * alpha;
        self.set_normalized(
            a * ((a + 1.0) - (a - 1.0) * cos + two),
            2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
            a * ((a + 1.0) - (a - 1.0) * cos - two),
            (a + 1.0) + (a - 1.0) * cos + two,
            -2.0 * ((a - 1.0) + (a + 1.0) * cos),
            (a + 1.0) + (a - 1.0) * cos - two,
        );
    }

    pub fn set_high_shelf(&mut self, sample_rate: f32, frequency: f32, gain_db: f32) {
        let a = 10.0f32.powf(gain_db / 40.0);
        let w = omega(sample_rate, frequency);
        let cos = w.cos();
        let sin = w.sin();
        let root = a.sqrt();
        let alpha = sin * std::f32::consts::FRAC_1_SQRT_2;
        let two = 2.0 * root * alpha;
        self.set_normalized(
            a * ((a + 1.0) + (a - 1.0) * cos + two),
            -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
            a * ((a + 1.0) + (a - 1.0) * cos - two),
            (a + 1.0) - (a - 1.0) * cos + two,
            2.0 * ((a - 1.0) - (a + 1.0) * cos),
            (a + 1.0) - (a - 1.0) * cos - two,
        );
    }

    #[inline]
    pub fn process(&mut self, channel: usize, input: f32) -> f32 {
        let output = self.b0 * input + self.z1[channel];
        self.z1[channel] = flush_denormal(self.b1 * input - self.a1 * output + self.z2[channel]);
        self.z2[channel] = flush_denormal(self.b2 * input - self.a2 * output);
        output
    }

    pub fn reset(&mut self) {
        self.z1 = [0.0; 2];
        self.z2 = [0.0; 2];
    }
}

fn omega(sample_rate: f32, frequency: f32) -> f32 {
    2.0 * std::f32::consts::PI * clamp(frequency, 10.0, sample_rate.max(1.0) * 0.49)
        / sample_rate.max(1.0)
}

fn common(sample_rate: f32, frequency: f32, q: f32) -> (f32, f32, f32) {
    let w = omega(sample_rate, frequency);
    let sin = w.sin();
    (w.cos(), sin, sin / (2.0 * q.max(0.1)))
}

/// Pole Qs of a fourth-order Butterworth response built from two biquads.
/// Cascading two identical Q = 1/√2 sections would sag 6 dB at the corner
/// instead of the 3 dB a real 24 dB/oct console filter shows.
const BUTTERWORTH_4_Q: [f32; 2] = [0.541_196_1, 1.306_562_9];

/// Console filter and EQ section.
///
/// The cut filters are 24 dB/oct Butterworth cascades and drop out of the
/// signal path entirely at the ends of their ranges, so a strip with the
/// filters parked open is bit-transparent rather than quietly phase-shifted.
#[derive(Debug, Clone)]
pub struct Filters {
    pub high_pass: [Biquad; 2],
    pub low_pass: [Biquad; 2],
    pub high_pass_active: bool,
    pub low_pass_active: bool,
    pub eq: [Biquad; 4],
}

impl Filters {
    pub fn new() -> Self {
        Self {
            high_pass: [Biquad::identity(); 2],
            low_pass: [Biquad::identity(); 2],
            high_pass_active: false,
            low_pass_active: false,
            eq: [Biquad::identity(); 4],
        }
    }

    /// `open_below` is the frequency at or under which the cut is bypassed.
    pub fn set_high_pass(&mut self, sample_rate: f32, frequency: f32, open_below: f32) {
        let active = frequency > open_below;
        if active && !self.high_pass_active {
            for stage in &mut self.high_pass {
                stage.reset();
            }
        }
        self.high_pass_active = active;
        for (stage, q) in self.high_pass.iter_mut().zip(BUTTERWORTH_4_Q) {
            stage.set_high_pass(sample_rate, frequency, q);
        }
    }

    /// `open_above` is the frequency at or over which the cut is bypassed.
    pub fn set_low_pass(&mut self, sample_rate: f32, frequency: f32, open_above: f32) {
        let active = frequency < open_above;
        if active && !self.low_pass_active {
            for stage in &mut self.low_pass {
                stage.reset();
            }
        }
        self.low_pass_active = active;
        for (stage, q) in self.low_pass.iter_mut().zip(BUTTERWORTH_4_Q) {
            stage.set_low_pass(sample_rate, frequency, q);
        }
    }

    /// The two cuts' combined gain at `frequency`, in dB; a cut parked open
    /// is out of the path and adds nothing.
    pub fn cuts_db(&self, sample_rate: f32, frequency: f32) -> f32 {
        let mut db = 0.0;
        if self.high_pass_active {
            db += self
                .high_pass
                .iter()
                .map(|s| s.magnitude_db(sample_rate, frequency))
                .sum::<f32>();
        }
        if self.low_pass_active {
            db += self
                .low_pass
                .iter()
                .map(|s| s.magnitude_db(sample_rate, frequency))
                .sum::<f32>();
        }
        db
    }

    /// The four EQ bands' combined gain at `frequency`, in dB.
    pub fn eq_db(&self, sample_rate: f32, frequency: f32) -> f32 {
        self.eq
            .iter()
            .map(|s| s.magnitude_db(sample_rate, frequency))
            .sum()
    }

    #[inline]
    pub fn process_cuts(&mut self, channel: usize, mut sample: f32) -> f32 {
        if self.high_pass_active {
            for stage in &mut self.high_pass {
                sample = stage.process(channel, sample);
            }
        }
        if self.low_pass_active {
            for stage in &mut self.low_pass {
                sample = stage.process(channel, sample);
            }
        }
        sample
    }

    pub fn reset(&mut self) {
        for stage in &mut self.high_pass {
            stage.reset();
        }
        for stage in &mut self.low_pass {
            stage.reset();
        }
        for band in &mut self.eq {
            band.reset();
        }
    }
}

/// Console-style proportional Q for the sweepable mid bands.
///
/// Analogue sweep EQs widen as the band is backed off and tighten as it is
/// pushed; a fixed Q makes small moves sound surgical and large moves sound
/// blunt. Spans Q 0.7 at unity to Q 1.9 at the ±18 dB extremes.
pub fn proportional_q(gain_db: f32) -> f32 {
    0.7 + (gain_db.abs() / 18.0).min(1.0) * 1.2
}

/// One-pole DC blocker for the biased saturation stage.
#[derive(Debug, Clone, Copy)]
pub struct DcBlocker {
    coefficient: f32,
    previous_input: [f32; 2],
    previous_output: [f32; 2],
}

impl DcBlocker {
    pub fn new(sample_rate: f32) -> Self {
        let mut blocker = Self {
            coefficient: 0.0,
            previous_input: [0.0; 2],
            previous_output: [0.0; 2],
        };
        blocker.set_sample_rate(sample_rate);
        blocker
    }

    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        self.coefficient = (-2.0 * std::f32::consts::PI * 5.0 / sample_rate.max(1.0)).exp();
    }

    #[inline]
    pub fn process(&mut self, channel: usize, input: f32) -> f32 {
        let output =
            input - self.previous_input[channel] + self.coefficient * self.previous_output[channel];
        self.previous_input[channel] = input;
        self.previous_output[channel] = flush_denormal(output);
        self.previous_output[channel]
    }

    pub fn reset(&mut self) {
        self.previous_input = [0.0; 2];
        self.previous_output = [0.0; 2];
    }
}

/// Shaping amounts below this leave the signal untouched. The curve tends to
/// the identity as the amount approaches zero, so the threshold is only there
/// to avoid dividing by it.
pub const SATURATION_FLOOR: f32 = 1.0e-4;

/// Peak bias at the extremes of the character control, in the post-drive
/// domain. Large enough for audible even harmonics, small enough that the two
/// polarities stay within about 2.5 dB of each other.
const SATURATION_BIAS: f32 = 0.25;

#[inline]
pub fn saturation_active(drive: f32) -> bool {
    drive > SATURATION_FLOOR
}

/// Bias fades in with drive so the curve stays symmetric — and continuous with
/// the bypassed path — as the control leaves zero.
#[inline]
fn saturation_bias(drive: f32, character: f32) -> f32 {
    (character.clamp(0.0, 1.0) - 0.5) * 2.0 * SATURATION_BIAS * (1.0 - (-drive).exp())
}

/// Half-gain average of the curve, used to normalise it.
#[inline]
fn saturation_normalization(drive: f32, bias: f32) -> f32 {
    (0.5 * ((drive + bias).tanh() - (bias - drive).tanh())).max(1.0e-6)
}

/// `ln(cosh(x))`, evaluated without overflowing for large `|x|`.
#[inline]
fn ln_cosh(x: f32) -> f32 {
    let a = x.abs();
    a + (-2.0 * a).exp().ln_1p() - std::f32::consts::LN_2
}

/// Level-matched soft saturation.
///
/// `drive` is the shaping amount, zero for a straight wire and rising to a hard
/// knee; the curve approaches the identity as it approaches zero, so turning
/// the control up from nothing does not step the level. Character shifts the
/// operating point along the curve: centred it is symmetric and produces odd
/// harmonics only, and either extreme biases it so the two polarities bend at
/// different rates and even harmonics appear. Normalising by the average of the
/// two half-gains keeps the overall level matched while letting the halves
/// differ, which is the asymmetry itself. The curve is smooth through zero — a
/// sign-dependent normalisation would kink there and spray harmonics no
/// analogue stage produces. The channel strip follows this with [`DcBlocker`]
/// to remove the offset the bias introduces.
#[inline]
pub fn saturate(sample: f32, drive: f32, character: f32) -> f32 {
    if !saturation_active(drive) {
        return sample;
    }
    let bias = saturation_bias(drive, character);
    let offset = bias.tanh();
    ((sample * drive + bias).tanh() - offset) / saturation_normalization(drive, bias)
}

/// Antiderivative of [`saturate`] with respect to the input sample.
#[inline]
fn saturate_integral(sample: f32, drive: f32, character: f32) -> f32 {
    let bias = saturation_bias(drive, character);
    let offset = bias.tanh();
    (ln_cosh(sample * drive + bias) / drive - offset * sample)
        / saturation_normalization(drive, bias)
}

/// Below this input delta the ADAA difference quotient loses precision and the
/// direct curve is used instead.
const ADAA_EPSILON: f32 = 1.0e-5;

/// First-order antiderivative anti-aliasing around [`saturate`].
///
/// A waveshaper evaluated pointwise folds every harmonic it creates above
/// Nyquist back into the audible band, which is what makes cheap saturation
/// sound gritty rather than warm. Integrating the curve across each sample
/// interval instead of sampling it applies a one-sample averaging window to the
/// generated harmonics, cutting the aliased energy without the buffers, latency
/// or CPU of oversampling.
#[derive(Debug, Clone, Copy)]
pub struct Saturator {
    previous_input: [f32; 2],
    previous_integral: [f32; 2],
    /// The stored integral belongs to a specific drive/character pair, so it is
    /// invalid until one sample has run under the current settings.
    primed: [bool; 2],
}

impl Saturator {
    pub const fn new() -> Self {
        Self {
            previous_input: [0.0; 2],
            previous_integral: [0.0; 2],
            primed: [false; 2],
        }
    }

    #[inline]
    pub fn process(&mut self, channel: usize, input: f32, drive: f32, character: f32) -> f32 {
        if !saturation_active(drive) {
            self.previous_input[channel] = input;
            self.primed[channel] = false;
            return input;
        }
        let previous = self.previous_input[channel];
        let integral = saturate_integral(input, drive, character);
        let delta = input - previous;
        let output = if !self.primed[channel] || delta.abs() < ADAA_EPSILON {
            saturate((input + previous) * 0.5, drive, character)
        } else {
            (integral - self.previous_integral[channel]) / delta
        };
        self.previous_input[channel] = input;
        self.previous_integral[channel] = integral;
        self.primed[channel] = true;
        flush_denormal(output)
    }

    /// Invalidates the stored integral after a drive or character edit.
    pub fn recurve(&mut self) {
        self.primed = [false; 2];
    }

    pub fn reset(&mut self) {
        self.previous_input = [0.0; 2];
        self.previous_integral = [0.0; 2];
        self.primed = [false; 2];
    }
}

impl Default for Saturator {
    fn default() -> Self {
        Self::new()
    }
}

/// Stereo-linked feed-forward compressor with a program-dependent release.
///
/// Two gain stages share the detector: a fast one that follows the control
/// settings, and a slow one whose attack is deliberately too sluggish to catch
/// transients. Whichever is reducing more wins. A short peak therefore only
/// engages the fast stage and recovers at the release time set on the panel,
/// while sustained material accumulates in the slow stage and recovers several
/// times slower — the behaviour that makes real bus compressors sit still on
/// dense material without pumping on drum hits.
#[derive(Debug, Clone, Copy)]
pub struct StripCompressor {
    sample_rate: f32,
    threshold_db: f32,
    ratio: f32,
    knee_db: f32,
    makeup_linear: f32,
    attack_sec: f32,
    release_sec: f32,
    attack_fast: f32,
    attack_slow: f32,
    release_fast: f32,
    release_slow: f32,
    detector: f32,
    detector_release: f32,
    fast_db: f32,
    slow_db: f32,
}

/// Rectifier smoothing ahead of the detector. Long enough to stop the envelope
/// chattering on individual samples, short enough to keep transient timing.
const DETECTOR_SECONDS: f32 = 0.000_5;
/// The slow stage ignores anything shorter than this multiple of the attack.
const SLOW_ATTACK_FACTOR: f32 = 12.0;
/// …and lets go this many times slower than the panel release.
const SLOW_RELEASE_FACTOR: f32 = 4.0;

impl StripCompressor {
    pub fn new(sample_rate: f32) -> Self {
        let mut compressor = Self {
            sample_rate: sample_rate.max(1.0),
            threshold_db: -18.0,
            ratio: 4.0,
            knee_db: 6.0,
            makeup_linear: 1.0,
            attack_sec: 0.01,
            release_sec: 0.1,
            attack_fast: 0.0,
            attack_slow: 0.0,
            release_fast: 0.0,
            release_slow: 0.0,
            detector: 0.0,
            detector_release: 0.0,
            fast_db: 0.0,
            slow_db: 0.0,
        };
        compressor.set_timing(0.01, 0.1);
        compressor
    }

    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        self.set_timing(self.attack_sec, self.release_sec);
    }

    pub fn set_timing(&mut self, attack_sec: f32, release_sec: f32) {
        let attack = attack_sec.max(0.000_02);
        let release = release_sec.max(0.001);
        self.attack_sec = attack;
        self.release_sec = release;
        self.attack_fast = time_constant(self.sample_rate, attack);
        self.attack_slow = time_constant(self.sample_rate, attack * SLOW_ATTACK_FACTOR);
        self.release_fast = time_constant(self.sample_rate, release);
        self.release_slow = time_constant(self.sample_rate, release * SLOW_RELEASE_FACTOR);
        self.detector_release = time_constant(self.sample_rate, DETECTOR_SECONDS);
    }

    pub fn set_curve(&mut self, threshold_db: f32, ratio: f32, knee_db: f32, makeup_db: f32) {
        self.threshold_db = threshold_db;
        self.ratio = ratio.max(1.0);
        self.knee_db = knee_db.max(0.0);
        self.makeup_linear = db_to_linear(makeup_db);
    }

    pub fn reset(&mut self) {
        self.detector = 0.0;
        self.fast_db = 0.0;
        self.slow_db = 0.0;
    }

    /// Current reduction as a positive dB figure, for metering.
    pub fn gain_reduction_db(&self) -> f32 {
        -self.fast_db.min(self.slow_db)
    }

    #[inline]
    pub fn process_stereo_linked(&mut self, left: f32, right: f32) -> (f32, f32) {
        let peak = left.abs().max(right.abs());
        self.detector = if peak > self.detector {
            peak
        } else {
            flush_denormal(
                self.detector_release * self.detector + (1.0 - self.detector_release) * peak,
            )
        };

        let target_db = self.curve_gain_db(linear_to_db(self.detector.max(1.0e-9)));
        self.fast_db = follow(self.fast_db, target_db, self.attack_fast, self.release_fast);
        self.slow_db = follow(self.slow_db, target_db, self.attack_slow, self.release_slow);

        let gain = db_to_linear(self.fast_db.min(self.slow_db)) * self.makeup_linear;
        (left * gain, right * gain)
    }

    /// Static curve, returning reduction in dB (zero or negative).
    #[inline]
    fn curve_gain_db(&self, level_db: f32) -> f32 {
        compressor_curve_db(level_db, self.threshold_db, self.ratio, self.knee_db)
    }
}

/// The strip compressor's static curve: the gain, in dB (zero or negative),
/// at a detector level of `level_db`, through a quadratic knee `knee_db`
/// wide.
#[inline]
pub fn compressor_curve_db(level_db: f32, threshold_db: f32, ratio: f32, knee_db: f32) -> f32 {
    let over = level_db - threshold_db;
    let half_knee = knee_db * 0.5;
    let slope = 1.0 / ratio.max(1.0) - 1.0;
    if over <= -half_knee {
        0.0
    } else if over >= half_knee {
        slope * over
    } else {
        let t = over + half_knee;
        slope * (t * t) / (2.0 * knee_db.max(1.0e-6))
    }
}

/// One-pole follower with separate attack (moving toward more reduction) and
/// release coefficients, working on dB so the timing does not change with the
/// depth of reduction.
#[inline]
fn follow(current: f32, target: f32, attack: f32, release: f32) -> f32 {
    let coeff = if target < current { attack } else { release };
    flush_denormal(coeff * current + (1.0 - coeff) * target)
}

/// Zero-latency brickwall limiter.
///
/// Reduction starts inside a soft knee below the ceiling so the onset is
/// gradual rather than a switch, the ceiling itself is then guaranteed by a
/// hard division, and recovery waits out a hold window before easing back
/// through two cascaded poles. The hold and the second pole are what keep a
/// limiter from re-modulating bass — a single exponential release that starts
/// on the sample after the peak tracks the waveform itself and turns low
/// frequencies into distortion.
#[derive(Debug, Clone, Copy)]
pub struct Limiter {
    ceiling: f32,
    ceiling_db: f32,
    gain: f32,
    smoothed: f32,
    hold_samples: u32,
    hold_remaining: u32,
    release: f32,
    release_smooth: f32,
}

/// Reduction begins this far below the ceiling.
pub const LIMITER_KNEE_DB: f32 = 1.5;
/// Hold window as a fraction of the release control, capped for fast settings.
const LIMITER_HOLD_FRACTION: f32 = 0.25;
const LIMITER_HOLD_MAX_SECONDS: f32 = 0.010;

impl Limiter {
    pub fn new(sample_rate: f32) -> Self {
        let mut limiter = Self {
            ceiling: 1.0,
            ceiling_db: 0.0,
            gain: 1.0,
            smoothed: 1.0,
            hold_samples: 0,
            hold_remaining: 0,
            release: 0.0,
            release_smooth: 0.0,
        };
        limiter.set_release(sample_rate, 0.1);
        limiter
    }

    pub fn set_ceiling_db(&mut self, ceiling_db: f32) {
        self.ceiling_db = ceiling_db;
        self.ceiling = db_to_linear(ceiling_db);
    }

    pub fn set_release(&mut self, sample_rate: f32, release_sec: f32) {
        let sample_rate = sample_rate.max(1.0);
        let release = release_sec.max(0.001);
        self.release = time_constant(sample_rate, release);
        self.release_smooth = time_constant(sample_rate, release * 0.35);
        let hold = (release * LIMITER_HOLD_FRACTION).min(LIMITER_HOLD_MAX_SECONDS);
        self.hold_samples = (hold * sample_rate) as u32;
    }

    pub fn reset(&mut self) {
        self.gain = 1.0;
        self.smoothed = 1.0;
        self.hold_remaining = 0;
    }

    /// Applied gain, linear and never above one, for metering.
    pub const fn gain(&self) -> f32 {
        self.smoothed
    }

    #[inline]
    pub fn process(&mut self, left: f32, right: f32) -> (f32, f32) {
        let peak = left.abs().max(right.abs());
        let knee_gain = db_to_linear(soft_over_db(
            linear_to_db(peak.max(1.0e-9)) - self.ceiling_db,
            LIMITER_KNEE_DB,
        ));
        let hard_gain = if peak > self.ceiling {
            self.ceiling / peak
        } else {
            1.0
        };
        let target = knee_gain.min(hard_gain);

        if target < self.gain {
            self.gain = target;
            self.hold_remaining = self.hold_samples;
        } else if self.hold_remaining > 0 {
            self.hold_remaining -= 1;
        } else {
            self.gain = flush_denormal(self.release * self.gain + (1.0 - self.release) * 1.0);
        }

        // Second pole rounds the release curve; the clamp keeps the ceiling
        // absolute even while the smoother is catching up.
        self.smoothed = (self.release_smooth * self.smoothed
            + (1.0 - self.release_smooth) * self.gain)
            .min(hard_gain);
        (left * self.smoothed, right * self.smoothed)
    }
}

/// Infinite-ratio reduction in dB for an input `over_db` above the ceiling,
/// eased through a quadratic knee.
#[inline]
pub fn soft_over_db(over_db: f32, knee_db: f32) -> f32 {
    let half_knee = knee_db * 0.5;
    if over_db <= -half_knee {
        0.0
    } else if over_db >= half_knee {
        -over_db
    } else {
        let t = over_db + half_knee;
        -(t * t) / (2.0 * knee_db)
    }
}

#[inline]
pub fn stereo_width(left: f32, right: f32, width: f32) -> (f32, f32) {
    let mid = (left + right) * 0.5;
    let side = (left - right) * 0.5 * width;
    (mid + side, mid - side)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_stay_finite_at_extreme_settings() {
        let mut filter = Biquad::identity();
        for frequency in [10.0, 20_000.0, 23_500.0] {
            filter.set_peak(48_000.0, frequency, 18.0, 12.0);
            for n in 0..10_000 {
                let output = filter.process(0, if n & 1 == 0 { 1.0 } else { -1.0 });
                assert!(output.is_finite());
            }
        }
    }

    #[test]
    fn width_preserves_mid_and_scales_side() {
        assert_eq!(stereo_width(0.5, -0.5, 0.0), (0.0, 0.0));
        assert_eq!(stereo_width(0.5, -0.5, 2.0), (1.0, -1.0));
        assert_eq!(stereo_width(0.25, 0.25, 2.0), (0.25, 0.25));
    }

    #[test]
    fn saturation_is_level_matched_and_smooth() {
        for character in [0.0, 0.25, 0.5, 0.75, 1.0] {
            assert!(saturate(0.0, 4.0, character).abs() < 1.0e-6);
            let positive = saturate(1.0, 4.0, character);
            let negative = saturate(-1.0, 4.0, character);
            // The bias bends the halves apart — that asymmetry is the even
            // harmonics — but the pair stays centred on unity, so the drive
            // control does not double as a level control.
            assert!(((positive - negative) * 0.5 - 1.0).abs() < 1.0e-3);
            assert!((0.7..=1.3).contains(&positive), "{positive}");
            assert!((0.7..=1.3).contains(&-negative), "{negative}");
            let mut previous = f32::NEG_INFINITY;
            for step in -100..=100 {
                let value = saturate(step as f32 / 100.0, 4.0, character);
                assert!(value > previous);
                previous = value;
            }
        }
        // Centred character is exactly symmetric and exactly level matched.
        assert!((saturate(1.0, 4.0, 0.5) - 1.0).abs() < 1.0e-6);
        assert!((saturate(-1.0, 4.0, 0.5) + 1.0).abs() < 1.0e-6);
    }

    #[test]
    fn saturation_meets_the_bypassed_path_at_zero_drive() {
        // The control leaving zero must not step the level, so the curve has to
        // approach the identity rather than a normalised tanh.
        for x in [-0.9, -0.3, 0.2, 0.75] {
            assert_eq!(saturate(x, 0.0, 0.7), x);
            assert!((saturate(x, 0.01, 0.7) - x).abs() < 0.01);
        }
    }

    #[test]
    fn centred_character_stays_symmetric() {
        for x in [0.1, 0.4, 0.9] {
            let positive = saturate(x, 3.0, 0.5);
            let negative = saturate(-x, 3.0, 0.5);
            assert!((positive + negative).abs() < 1.0e-6);
        }
    }

    /// Magnitude of one bin, by Goertzel, so the alias test needs no FFT.
    fn bin_magnitude(samples: &[f32], frequency: f32, sample_rate: f32) -> f32 {
        let w = std::f32::consts::TAU * frequency / sample_rate;
        let coeff = 2.0 * w.cos();
        let (mut s1, mut s2) = (0.0f32, 0.0f32);
        for &sample in samples {
            let s0 = sample + coeff * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        (s1 * s1 + s2 * s2 - coeff * s1 * s2).max(0.0).sqrt() / samples.len() as f32
    }

    #[test]
    fn antiderivative_shaping_cuts_saturation_aliasing() {
        const SR: f32 = 48_000.0;
        const TONE: f32 = 11_000.0;
        // The third harmonic lands at 33 kHz and folds back to 15 kHz.
        const ALIAS: f32 = 15_000.0;
        let mut naive = Vec::with_capacity(8_192);
        let mut shaped = Vec::with_capacity(8_192);
        let mut saturator = Saturator::new();
        for n in 0..8_192 {
            let input = (n as f32 * std::f32::consts::TAU * TONE / SR).sin() * 0.9;
            naive.push(saturate(input, 4.0, 0.5));
            shaped.push(saturator.process(0, input, 4.0, 0.5));
        }
        let naive_alias = bin_magnitude(&naive, ALIAS, SR);
        let shaped_alias = bin_magnitude(&shaped, ALIAS, SR);
        assert!(
            shaped_alias < naive_alias * 0.7,
            "alias {shaped_alias} vs {naive_alias}"
        );
        // …while the fundamental survives.
        let naive_tone = bin_magnitude(&naive, TONE, SR);
        let shaped_tone = bin_magnitude(&shaped, TONE, SR);
        assert!(shaped_tone > naive_tone * 0.85);
    }

    fn compressor_gr_after(burst_samples: usize, rest_samples: usize) -> f32 {
        let mut compressor = StripCompressor::new(48_000.0);
        compressor.set_curve(-20.0, 4.0, 6.0, 0.0);
        compressor.set_timing(0.005, 0.100);
        for n in 0..burst_samples {
            let input = (n as f32 * std::f32::consts::TAU * 220.0 / 48_000.0).sin();
            let _ = compressor.process_stereo_linked(input, input);
        }
        for _ in 0..rest_samples {
            let _ = compressor.process_stereo_linked(0.0, 0.0);
        }
        compressor.gain_reduction_db()
    }

    #[test]
    fn compressor_release_is_program_dependent() {
        // 10 ms transient versus 1 s of sustain, both read 50 ms after the
        // signal stops. The sustained pass must still be holding more gain
        // reduction than the transient one.
        let transient = compressor_gr_after(480, 2_400);
        let sustained = compressor_gr_after(48_000, 2_400);
        assert!(
            sustained > transient + 1.0,
            "transient {transient} sustained {sustained}"
        );
        assert!(transient >= 0.0 && sustained < 40.0);
    }

    #[test]
    fn compressor_reaches_the_static_curve() {
        let mut compressor = StripCompressor::new(48_000.0);
        compressor.set_curve(-20.0, 4.0, 6.0, 0.0);
        compressor.set_timing(0.001, 0.010);
        // -6 dBFS peak sits 14 dB over the threshold, well past the knee, so
        // the steady-state reduction is 14 - 14/4 = 10.5 dB.
        let amplitude = db_to_linear(-6.0);
        for n in 0..48_000 {
            let input = (n as f32 * std::f32::consts::TAU * 500.0 / 48_000.0).sin() * amplitude;
            let _ = compressor.process_stereo_linked(input, input);
        }
        let gr = compressor.gain_reduction_db();
        assert!((gr - 10.5).abs() < 1.0, "{gr}");
    }

    #[test]
    fn limiter_holds_the_ceiling_and_recovers() {
        let mut limiter = Limiter::new(48_000.0);
        limiter.set_ceiling_db(-0.3);
        limiter.set_release(48_000.0, 0.050);
        let ceiling = db_to_linear(-0.3);
        for n in 0..24_000 {
            let input = (n as f32 * std::f32::consts::TAU * 100.0 / 48_000.0).sin() * 2.0;
            let (left, right) = limiter.process(input, input);
            assert!(left.abs() <= ceiling + 1.0e-5, "{left}");
            assert!(right.abs() <= ceiling + 1.0e-5);
        }
        // Hold keeps the gain down for a moment after the peaks stop.
        let (held, _) = limiter.process(0.0, 0.0);
        let _ = held;
        assert!(limiter.gain() < 0.95);
        for _ in 0..48_000 {
            let _ = limiter.process(0.0, 0.0);
        }
        assert!(limiter.gain() > 0.999);
    }

    #[test]
    fn limiter_release_moves_monotonically() {
        // A single overshoot must not step back to unity in one sample, and it
        // must not wander back down on the way up either.
        let mut limiter = Limiter::new(48_000.0);
        limiter.set_ceiling_db(-0.3);
        limiter.set_release(48_000.0, 0.100);
        let _ = limiter.process(2.0, 2.0);
        let mut previous = limiter.gain();
        assert!(previous < 0.6);
        for _ in 0..9_600 {
            let _ = limiter.process(0.0, 0.0);
            let gain = limiter.gain();
            assert!(gain >= previous - 1.0e-6, "{gain} after {previous}");
            previous = gain;
        }
        assert!(previous < 1.0);
    }

    #[test]
    fn butterworth_cut_bypasses_at_the_ends_of_its_range() {
        let mut filters = Filters::new();
        filters.set_high_pass(48_000.0, 20.0, 20.0);
        filters.set_low_pass(48_000.0, 20_000.0, 20_000.0);
        assert!(!filters.high_pass_active);
        assert!(!filters.low_pass_active);
        for n in 0..1_000 {
            let input = (n as f32 * 0.01).sin();
            assert_eq!(filters.process_cuts(0, input), input);
        }
        filters.set_high_pass(48_000.0, 120.0, 20.0);
        assert!(filters.high_pass_active);
    }

    #[test]
    fn fourth_order_high_pass_is_three_db_down_at_its_corner() {
        const SR: f32 = 48_000.0;
        const CORNER: f32 = 200.0;
        let mut filters = Filters::new();
        filters.set_high_pass(SR, CORNER, 20.0);
        filters.set_low_pass(SR, 20_000.0, 20_000.0);
        let mut peak = 0.0f32;
        for n in 0..24_000 {
            let input = (n as f32 * std::f32::consts::TAU * CORNER / SR).sin();
            let output = filters.process_cuts(0, input);
            if n > 12_000 {
                peak = peak.max(output.abs());
            }
        }
        let db = 20.0 * peak.log10();
        assert!((db + 3.0).abs() < 0.6, "{db} dB at the corner");
    }

    #[test]
    fn proportional_q_tightens_with_gain() {
        assert!((proportional_q(0.0) - 0.7).abs() < 1.0e-6);
        assert!(proportional_q(18.0) > proportional_q(6.0));
        assert!(proportional_q(-18.0) > proportional_q(0.0));
        assert!(proportional_q(40.0) <= 1.9 + 1.0e-6);
    }

    #[test]
    fn dc_blocker_removes_biased_saturation_offset() {
        let mut blocker = DcBlocker::new(48_000.0);
        let mut sum = 0.0;
        let mut count = 0;
        for n in 0..96_000 {
            let input = (n as f32 * std::f32::consts::TAU * 440.0 / 48_000.0).sin() * 0.8;
            let output = blocker.process(0, saturate(input, 4.0, 1.0));
            if n >= 48_000 {
                sum += output;
                count += 1;
            }
        }
        assert!((sum / count as f32).abs() < 1.0e-4);
    }
}

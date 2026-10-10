//! BurnLimit — modern brickwall limiter (Pro-L / Ozone–inspired).
//!
//! Lookahead peak limiting with style curves and a 4× cubic inter-sample peak
//! detector. The audio path remains allocation-free and bounded.

use builtin_dsp_core::delay::{Smoothed, smoothing_step};
use builtin_dsp_core::{
    ParamDescriptor, PluginCategory, PluginDescriptor, StereoEffect, clamp, db_to_linear,
    linear_to_db, mix, time_constant,
};
use serde::{Deserialize, Serialize};

pub mod ipc;
pub mod presets;
pub mod ui;

pub use ipc::{UI_PARAM_IDS, ui_param_id, ui_param_index};
pub use presets::{FactoryPreset, factory_presets};

pub const PLUGIN_ID: &str = "futureboard.burnlimit";

const CLIP_THRESHOLD: f32 = 1.0;
const RMS_WINDOW_SECONDS: f32 = 0.300;
const PEAK_FALL_SECONDS: f32 = 0.400;

/// Maximum lookahead buffer. Covers the declared 10 ms range through 384 kHz.
const MAX_LOOKAHEAD_SAMPLES: usize = 4_096;
const TRUE_PEAK_PHASES: [f32; 3] = [0.25, 0.5, 0.75];
/// Four-point cubic interpolation needs one future sample. Three samples of
/// latency ensure the complete cubic support is known before its first sample
/// exits the delay line.
const TRUE_PEAK_MIN_DELAY_SAMPLES: usize = 3;
const TRUE_PEAK_GAIN_HOLD_SAMPLES: usize = 4;
const LOOKAHEAD_MASK: usize = MAX_LOOKAHEAD_SAMPLES - 1;
/// Gain / ceiling / mix smoothing, per stage of a [`Glide`], so a dragged
/// knob cannot zipper.
const SMOOTH_MS: f32 = 7.0;
/// Length of the Power crossfade and of a lookahead change's read-position
/// crossfade.
const FADE_MS: f32 = 10.0;

/// Limiter character. Wire order is the persisted contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Style {
    Clean,
    Punch,
    Modern,
    Clip,
}

impl Style {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::Punch => "punch",
            Self::Modern => "modern",
            Self::Clip => "clip",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "clean" => Some(Self::Clean),
            "punch" => Some(Self::Punch),
            "modern" => Some(Self::Modern),
            "clip" => Some(Self::Clip),
            _ => None,
        }
    }

    pub const fn to_wire(self) -> f32 {
        match self {
            Self::Clean => 0.0,
            Self::Punch => 1.0,
            Self::Modern => 2.0,
            Self::Clip => 3.0,
        }
    }

    pub fn from_wire(value: f32) -> Self {
        match value.round() as i32 {
            1 => Self::Punch,
            2 => Self::Modern,
            3 => Self::Clip,
            _ => Self::Clean,
        }
    }

    /// Soft-knee width in dB — Clean is softest, Clip is nearly hard.
    pub fn knee_db(self) -> f32 {
        match self {
            Self::Clean => 4.0,
            Self::Punch => 2.0,
            Self::Modern => 1.5,
            Self::Clip => 0.2,
        }
    }

    /// Attack seconds layered under the user's release / lookahead.
    pub fn attack_sec(self) -> f32 {
        match self {
            Self::Clean => 0.002,
            Self::Punch => 0.0008,
            Self::Modern => 0.0004,
            Self::Clip => 0.00005,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct MeterFrame {
    pub in_peak: f32,
    pub in_rms: f32,
    pub out_peak: f32,
    pub out_rms: f32,
    pub gain_reduction_db: f32,
    pub in_clip: bool,
    pub out_clip: bool,
}

#[derive(Debug, Clone)]
struct Meters {
    in_peak: f32,
    out_peak: f32,
    in_ms: f32,
    out_ms: f32,
    rms_coeff: f32,
    peak_coeff: f32,
    in_clip: bool,
    out_clip: bool,
}

impl Meters {
    fn new(sample_rate: f32) -> Self {
        Self {
            in_peak: 0.0,
            out_peak: 0.0,
            in_ms: 0.0,
            out_ms: 0.0,
            rms_coeff: time_constant(sample_rate, RMS_WINDOW_SECONDS),
            peak_coeff: time_constant(sample_rate, PEAK_FALL_SECONDS),
            in_clip: false,
            out_clip: false,
        }
    }

    fn reset(&mut self) {
        self.in_peak = 0.0;
        self.out_peak = 0.0;
        self.in_ms = 0.0;
        self.out_ms = 0.0;
        self.in_clip = false;
        self.out_clip = false;
    }

    #[inline]
    fn push_stereo(&mut self, in_l: f32, in_r: f32, out_l: f32, out_r: f32) {
        let in_abs = in_l.abs().max(in_r.abs());
        let out_abs = out_l.abs().max(out_r.abs());

        self.in_peak = if in_abs > self.in_peak {
            in_abs
        } else {
            self.in_peak * self.peak_coeff
        };
        self.out_peak = if out_abs > self.out_peak {
            out_abs
        } else {
            self.out_peak * self.peak_coeff
        };

        let input_ms = (in_l * in_l + in_r * in_r) * 0.5;
        let output_ms = (out_l * out_l + out_r * out_r) * 0.5;
        self.in_ms = self.rms_coeff * self.in_ms + (1.0 - self.rms_coeff) * input_ms;
        self.out_ms = self.rms_coeff * self.out_ms + (1.0 - self.rms_coeff) * output_ms;

        if in_abs >= CLIP_THRESHOLD {
            self.in_clip = true;
        }
        if out_abs >= CLIP_THRESHOLD {
            self.out_clip = true;
        }
    }
}

/// Streaming 4× inter-sample peak estimator. Once four samples are present,
/// Catmull-Rom interpolation evaluates the interval between the middle pair.
/// No buffers grow and no work depends on signal content.
#[derive(Debug, Clone, Copy, Default)]
struct TruePeakDetector {
    history: [f32; 4],
    filled: usize,
}

impl TruePeakDetector {
    fn reset(&mut self) {
        *self = Self::default();
    }

    #[inline]
    fn push(&mut self, sample: f32) -> f32 {
        self.history.rotate_left(1);
        self.history[3] = sample;
        self.filled = (self.filled + 1).min(4);
        if self.filled < 4 {
            return sample.abs();
        }

        let [p0, p1, p2, p3] = self.history;
        let mut peak = p1.abs().max(p2.abs());
        for phase in TRUE_PEAK_PHASES {
            // Catmull-Rom cubic through p1..p2. This is a detector only; audio
            // is not resampled, so it adds no coloration to the signal path.
            let phase2 = phase * phase;
            let phase3 = phase2 * phase;
            let interpolated = 0.5
                * ((2.0 * p1)
                    + (-p0 + p2) * phase
                    + (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * phase2
                    + (-p0 + 3.0 * p1 - 3.0 * p2 + p3) * phase3);
            peak = peak.max(interpolated.abs());
        }
        peak
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Params {
    pub power: bool,
    pub style: Style,
    pub gain_db: f32,
    pub ceiling_db: f32,
    pub release_ms: f32,
    pub lookahead_ms: f32,
    pub true_peak: bool,
    pub mix: f32,
    pub stereo_link: bool,
}

pub fn default_params() -> Params {
    Params {
        power: true,
        style: Style::Modern,
        gain_db: 0.0,
        ceiling_db: -0.3,
        release_ms: 200.0,
        lookahead_ms: 2.0,
        true_peak: true,
        mix: 100.0,
        stereo_link: true,
    }
}

/// The gain computer's static curve: decibels taken off a level of
/// `level_db` held against `threshold_db`, with a quadratic knee `knee_db`
/// wide. Positive.
pub fn static_reduction_db(level_db: f32, threshold_db: f32, knee_db: f32) -> f32 {
    let knee = knee_db.max(1.0e-6);
    let knee_start = threshold_db - knee * 0.5;
    if level_db <= knee_start {
        return 0.0;
    }
    if level_db >= threshold_db + knee * 0.5 {
        level_db - threshold_db
    } else {
        let into_knee = level_db - knee_start;
        (into_knee * into_knee) / (2.0 * knee)
    }
}

/// The steady-state output level, in dBFS, of a peak held at `input_db`:
/// the drive, the style's knee against the ceiling, the final sample-peak
/// guard, then the dry blend. Bypassed, the output is the input.
pub fn transfer_db(params: &Params, input_db: f32) -> f32 {
    if !params.power {
        return input_db;
    }
    let driven_db = input_db + params.gain_db;
    let wet_db = (driven_db
        - static_reduction_db(driven_db, params.ceiling_db, params.style.knee_db()))
    .min(params.ceiling_db);
    let amount = clamp(params.mix, 0.0, 100.0) / 100.0;
    linear_to_db(mix(db_to_linear(input_db), db_to_linear(wet_db), amount).max(1.0e-9))
}

pub fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PLUGIN_ID,
        name: "BurnLimit",
        vendor: "Futureboard",
        category: PluginCategory::Effect,
        version: env!("CARGO_PKG_VERSION"),
        params: &[
            ParamDescriptor {
                id: "power",
                name: "Power",
                default_value: 1.0,
                min: 0.0,
                max: 1.0,
                unit: "bool",
            },
            ParamDescriptor {
                id: "style",
                name: "Style",
                default_value: 2.0,
                min: 0.0,
                max: 3.0,
                unit: "enum",
            },
            ParamDescriptor {
                id: "gainDb",
                name: "Gain",
                default_value: 0.0,
                min: -12.0,
                max: 24.0,
                unit: "dB",
            },
            ParamDescriptor {
                id: "ceilingDb",
                name: "Ceiling",
                default_value: -0.3,
                min: -6.0,
                max: 0.0,
                unit: "dB",
            },
            ParamDescriptor {
                id: "releaseMs",
                name: "Release",
                default_value: 200.0,
                min: 20.0,
                max: 2_000.0,
                unit: "ms",
            },
            ParamDescriptor {
                id: "lookaheadMs",
                name: "Lookahead",
                default_value: 2.0,
                min: 0.0,
                max: 10.0,
                unit: "ms",
            },
            ParamDescriptor {
                id: "truePeak",
                name: "True Peak",
                default_value: 1.0,
                min: 0.0,
                max: 1.0,
                unit: "bool",
            },
            ParamDescriptor {
                id: "mix",
                name: "Mix",
                default_value: 100.0,
                min: 0.0,
                max: 100.0,
                unit: "%",
            },
            ParamDescriptor {
                id: "stereoLink",
                name: "Link",
                default_value: 1.0,
                min: 0.0,
                max: 1.0,
                unit: "bool",
            },
        ],
    }
}

/// How close (relative) a smoothed value must come before it lands exactly.
const SETTLE: f32 = 1.0e-6;

/// One sample of `value` toward its target ([`Smoothed::next`]). Lands
/// exactly once the rest is negligible — or once a step no longer moves it in
/// `f32` — so a finished move leaves the arithmetic it started from.
#[inline]
fn glide(value: &mut Smoothed, step: f32) -> f32 {
    if value.value != value.target {
        let before = value.value;
        value.next(step);
        if value.value == before
            || (value.target - value.value).abs() <= SETTLE * (1.0 + value.target.abs())
        {
            value.settle();
        }
    }
    value.value
}

/// Two [`Smoothed`] stages in series. The second starts every move with zero
/// slope, so even a target that jumps bends in without a corner.
#[derive(Debug, Clone, Copy)]
struct Glide {
    inner: Smoothed,
    outer: Smoothed,
}

impl Glide {
    fn at(value: f32) -> Self {
        Self {
            inner: Smoothed::at(value),
            outer: Smoothed::at(value),
        }
    }

    fn set(&mut self, target: f32) {
        self.inner.target = target;
    }

    fn settle(&mut self) {
        self.inner.settle();
        self.outer = Smoothed::at(self.inner.target);
    }

    #[inline]
    fn moving(&self) -> bool {
        self.outer.value != self.inner.target || self.inner.value != self.inner.target
    }

    /// One sample further; the smoothed value.
    #[inline]
    fn next(&mut self, step: f32) -> f32 {
        if self.moving() {
            glide(&mut self.inner, step);
            self.outer.target = self.inner.value;
            glide(&mut self.outer, step);
        }
        self.outer.value
    }
}

/// Eased 0…1: zero slope at both ends, so a fade has no corner.
#[inline]
fn ease(pos: f32) -> f32 {
    pos * pos * (3.0 - 2.0 * pos)
}

/// Samples in a [`FADE_MS`] crossfade, as a per-sample step.
fn fade_step(sample_rate: f32) -> f32 {
    1.0 / (FADE_MS * 0.001 * sample_rate.max(1.0)).max(1.0)
}

/// A 0…1 crossfade position: moves linearly toward its target over
/// [`FADE_MS`] and is read eased, so neither end of the fade has a corner.
/// Lands exactly on 0 or 1.
#[derive(Debug, Clone, Copy)]
struct Fade {
    pos: f32,
    target: f32,
    step: f32,
}

impl Fade {
    fn new(on: bool, sample_rate: f32) -> Self {
        let at = if on { 1.0 } else { 0.0 };
        Self {
            pos: at,
            target: at,
            step: fade_step(sample_rate),
        }
    }

    fn set(&mut self, on: bool) {
        self.target = if on { 1.0 } else { 0.0 };
    }

    fn settle(&mut self) {
        self.pos = self.target;
    }

    /// One sample further; the eased weight of the "on" side.
    #[inline]
    fn next(&mut self) -> f32 {
        if self.pos < self.target {
            self.pos = (self.pos + self.step).min(self.target);
        } else if self.pos > self.target {
            self.pos = (self.pos - self.step).max(self.target);
        }
        ease(self.pos)
    }
}

/// `a` at `t` 0, `b` at `t` 1 (both exactly), a straight blend between.
#[inline]
fn blend(a: f32, b: f32, t: f32) -> f32 {
    if t <= 0.0 {
        a
    } else if t >= 1.0 {
        b
    } else {
        a + (b - a) * t
    }
}

/// A delay time that changes while audio runs: instead of jumping the read
/// position (a click), the output crossfades from the old position to the
/// new one over [`FADE_MS`]. A change that arrives mid-fade waits for the
/// fade to finish, so a dragged knob walks over in clean steps and always
/// lands on the latest value.
#[derive(Debug, Clone, Copy)]
struct DelaySwitch {
    from: usize,
    to: usize,
    /// The latest delay asked for: what the next fade goes to.
    pending: usize,
    pos: f32,
    step: f32,
}

impl DelaySwitch {
    fn new(delay: usize, sample_rate: f32) -> Self {
        Self {
            from: delay,
            to: delay,
            pending: delay,
            pos: 1.0,
            step: fade_step(sample_rate),
        }
    }

    fn request(&mut self, delay: usize) {
        self.pending = delay;
    }

    /// Straight to the latest delay, no fade.
    fn settle(&mut self) {
        self.from = self.pending;
        self.to = self.pending;
        self.pos = 1.0;
    }

    /// One sample further: `(from, to, weight of to)`. Settled, the weight
    /// is exactly 1.
    #[inline]
    fn next(&mut self) -> (usize, usize, f32) {
        if self.pos >= 1.0 {
            if self.pending == self.to {
                return (self.to, self.to, 1.0);
            }
            self.from = self.to;
            self.to = self.pending;
            self.pos = 0.0;
        }
        self.pos = (self.pos + self.step).min(1.0);
        (self.from, self.to, ease(self.pos))
    }
}

/// The lookahead delay. Written every sample whatever the delay, so a change
/// reads real history; a change crossfades between the old and the new read
/// position instead of jumping (or clearing) the line.
#[derive(Debug, Clone)]
struct LookaheadLine {
    left: [f32; MAX_LOOKAHEAD_SAMPLES],
    right: [f32; MAX_LOOKAHEAD_SAMPLES],
    write: usize,
    switch: DelaySwitch,
}

impl LookaheadLine {
    fn new(sample_rate: f32) -> Self {
        Self {
            left: [0.0; MAX_LOOKAHEAD_SAMPLES],
            right: [0.0; MAX_LOOKAHEAD_SAMPLES],
            write: 0,
            switch: DelaySwitch::new(0, sample_rate),
        }
    }

    fn reset(&mut self) {
        self.left.fill(0.0);
        self.right.fill(0.0);
        self.write = 0;
        self.switch.settle();
    }

    /// The delay the line is heading to (and reports).
    fn delay(&self) -> usize {
        self.switch.pending
    }

    fn set_delay(&mut self, delay: usize) {
        self.switch.request(delay.min(MAX_LOOKAHEAD_SAMPLES - 1));
    }

    #[inline]
    fn push_read(&mut self, left: f32, right: f32) -> (f32, f32) {
        let write = self.write;
        self.left[write] = left;
        self.right[write] = right;
        self.write = (write + 1) & LOOKAHEAD_MASK;
        let at = |delay: usize| (write + MAX_LOOKAHEAD_SAMPLES - delay) & LOOKAHEAD_MASK;
        let (from, to, weight) = self.switch.next();
        if weight >= 1.0 {
            (self.left[at(to)], self.right[at(to)])
        } else {
            let (old, new) = (at(from), at(to));
            (
                blend(self.left[old], self.left[new], weight),
                blend(self.right[old], self.right[new], weight),
            )
        }
    }
}

#[derive(Debug, Clone)]
pub struct Dsp {
    params: Params,
    /// Audio has run since construction or [`StereoEffect::reset`]. Until
    /// it has, an edit lands at once — an insert or session being set up
    /// replays its stored values, and there is nothing yet to glide over.
    started: bool,
    sample_rate: f32,
    /// Linear input gain, gliding.
    input_gain: Glide,
    /// The ceiling in dB, gliding; `ceiling_linear` follows it.
    ceiling_db: Glide,
    ceiling_linear: f32,
    mix_amount: Glide,
    smooth_step: f32,
    /// Dry ↔ limited: Power crossfades instead of switching.
    power: Fade,
    attack_coeff: f32,
    release_coeff: f32,
    knee_db: f32,
    envelope_l: f32,
    envelope_r: f32,
    gr_db: f32,
    true_peak_l: TruePeakDetector,
    true_peak_r: TruePeakDetector,
    true_peak_gain_l: f32,
    true_peak_gain_r: f32,
    true_peak_hold_l: usize,
    true_peak_hold_r: usize,
    delay: LookaheadLine,
    meters: Meters,
}

impl Dsp {
    pub fn new(sample_rate: f32) -> Self {
        let sr = sample_rate.max(1.0);
        let mut dsp = Self {
            started: false,
            params: default_params(),
            sample_rate: sr,
            input_gain: Glide::at(1.0),
            ceiling_db: Glide::at(0.0),
            ceiling_linear: 1.0,
            mix_amount: Glide::at(1.0),
            smooth_step: smoothing_step(SMOOTH_MS, sr),
            power: Fade::new(true, sr),
            attack_coeff: 0.0,
            release_coeff: 0.0,
            knee_db: 1.5,
            envelope_l: 0.0,
            envelope_r: 0.0,
            gr_db: 0.0,
            true_peak_l: TruePeakDetector::default(),
            true_peak_r: TruePeakDetector::default(),
            true_peak_gain_l: 1.0,
            true_peak_gain_r: 1.0,
            true_peak_hold_l: 0,
            true_peak_hold_r: 0,
            delay: LookaheadLine::new(sr),
            meters: Meters::new(sr),
        };
        dsp.apply_params();
        dsp.settle();
        dsp
    }

    pub fn params(&self) -> &Params {
        &self.params
    }

    pub fn gain_reduction_db(&self) -> f32 {
        self.gr_db.max(0.0)
    }

    /// Replace every parameter at once — a state load. Lands on the new
    /// values without a glide or a fade.
    pub fn set_params(&mut self, params: Params) {
        self.params = params;
        ipc::sanitize_params(&mut self.params);
        self.apply_params();
        self.settle();
    }

    pub fn meter_frame(&self) -> MeterFrame {
        MeterFrame {
            in_peak: self.meters.in_peak,
            in_rms: self.meters.in_ms.max(0.0).sqrt(),
            out_peak: self.meters.out_peak,
            out_rms: self.meters.out_ms.max(0.0).sqrt(),
            gain_reduction_db: if self.params.power {
                self.gr_db.max(0.0)
            } else {
                0.0
            },
            in_clip: self.meters.in_clip,
            out_clip: self.meters.out_clip,
        }
    }

    pub fn clear_clip(&mut self) {
        self.meters.in_clip = false;
        self.meters.out_clip = false;
    }

    pub fn apply_wire_param(&mut self, wire_index: u32, value: f32) -> bool {
        if !ipc::apply_wire_param(&mut self.params, wire_index, value) {
            return false;
        }
        self.apply_params();
        if !self.started {
            self.settle();
        }
        true
    }

    pub fn apply_ui_param(&mut self, id: &str, value: f32) -> bool {
        match ipc::ui_param_index(id) {
            Some(index) => self.apply_wire_param(index, value),
            None => false,
        }
    }

    pub fn latency_samples(&self) -> usize {
        self.delay.delay()
    }

    fn apply_params(&mut self) {
        let style = self.params.style;
        self.knee_db = style.knee_db();
        self.attack_coeff = time_constant(self.sample_rate, style.attack_sec());
        self.release_coeff = time_constant(self.sample_rate, self.params.release_ms * 0.001);
        self.input_gain.set(db_to_linear(self.params.gain_db));
        self.ceiling_db.set(self.params.ceiling_db);
        self.mix_amount.set(self.params.mix / 100.0);
        self.power.set(self.params.power);

        let mut delay = ((self.params.lookahead_ms * 0.001) * self.sample_rate).round() as usize;
        if self.params.true_peak {
            delay = delay.max(TRUE_PEAK_MIN_DELAY_SAMPLES);
        } else {
            self.true_peak_l.reset();
            self.true_peak_r.reset();
            self.true_peak_gain_l = 1.0;
            self.true_peak_gain_r = 1.0;
            self.true_peak_hold_l = 0;
            self.true_peak_hold_r = 0;
        }
        self.delay.set_delay(delay.min(MAX_LOOKAHEAD_SAMPLES - 1));
    }

    /// Land the gains, the Power fade and the lookahead on their targets.
    fn settle(&mut self) {
        self.input_gain.settle();
        self.ceiling_db.settle();
        self.ceiling_linear = db_to_linear(self.ceiling_db.outer.value);
        self.mix_amount.settle();
        self.power.settle();
        self.delay.switch.settle();
    }

    /// Gain computer: how much linear gain keeps `level` at or under the ceiling.
    #[inline]
    fn compute_gain(&self, level: f32) -> f32 {
        if level <= 1.0e-12 {
            return 1.0;
        }
        let ceiling_db = self.ceiling_db.outer.value;
        let gr_db = static_reduction_db(linear_to_db(level), ceiling_db, self.knee_db);
        db_to_linear(-gr_db)
    }

    #[inline]
    fn smooth_envelope(envelope: &mut f32, target: f32, attack: f32, release: f32) {
        let coeff = if target > *envelope { attack } else { release };
        *envelope = coeff * *envelope + (1.0 - coeff) * target;
    }

    #[inline]
    fn smooth_safety_gain(gain: &mut f32, hold: &mut usize, target: f32, release: f32) {
        if target <= *gain {
            *gain = target;
            *hold = TRUE_PEAK_GAIN_HOLD_SAMPLES;
        } else if *hold > 0 {
            *hold -= 1;
        } else {
            *gain = release * *gain + (1.0 - release) * target;
        }
    }
}

impl StereoEffect for Dsp {
    fn reset(&mut self) {
        self.started = false;
        self.envelope_l = 0.0;
        self.envelope_r = 0.0;
        self.gr_db = 0.0;
        self.true_peak_l.reset();
        self.true_peak_r.reset();
        self.true_peak_gain_l = 1.0;
        self.true_peak_gain_r = 1.0;
        self.true_peak_hold_l = 0;
        self.true_peak_hold_r = 0;
        self.delay.reset();
        self.meters.reset();
        self.settle();
    }

    fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        self.meters = Meters::new(self.sample_rate);
        self.smooth_step = smoothing_step(SMOOTH_MS, self.sample_rate);
        self.power.step = fade_step(self.sample_rate);
        self.delay.switch.step = fade_step(self.sample_rate);
        self.apply_params();
        self.settle();
    }

    fn process_stereo(&mut self, left: f32, right: f32) -> (f32, f32) {
        self.started = true;
        // The delay always carries the unprocessed signal. This keeps bypass
        // and dry/wet mixing aligned with the latency reported to the host.
        let (delayed_l, delayed_r) = self.delay.push_read(left, right);
        let on = self.power.next();
        if on <= 0.0 {
            // Off and faded out: only the delay runs.
            self.meters.push_stereo(left, right, delayed_l, delayed_r);
            self.envelope_l = 0.0;
            self.envelope_r = 0.0;
            self.gr_db = 0.0;
            self.true_peak_l.reset();
            self.true_peak_r.reset();
            self.true_peak_gain_l = 1.0;
            self.true_peak_gain_r = 1.0;
            self.true_peak_hold_l = 0;
            self.true_peak_hold_r = 0;
            return (delayed_l, delayed_r);
        }

        let step = self.smooth_step;
        let input_gain = self.input_gain.next(step);
        if self.ceiling_db.moving() {
            self.ceiling_linear = db_to_linear(self.ceiling_db.next(step));
        }
        let amount = self.mix_amount.next(step);

        let driven_l = left * input_gain;
        let driven_r = right * input_gain;

        // Detect on the undelayed path so GR can start before the peak exits.
        let peak_l = if self.params.true_peak {
            self.true_peak_l.push(driven_l)
        } else {
            driven_l.abs()
        };
        let peak_r = if self.params.true_peak {
            self.true_peak_r.push(driven_r)
        } else {
            driven_r.abs()
        };
        if self.params.true_peak {
            let safety_l = (self.ceiling_linear / peak_l.max(1.0e-12)).min(1.0);
            let safety_r = (self.ceiling_linear / peak_r.max(1.0e-12)).min(1.0);
            if self.params.stereo_link {
                let linked = safety_l.min(safety_r);
                Self::smooth_safety_gain(
                    &mut self.true_peak_gain_l,
                    &mut self.true_peak_hold_l,
                    linked,
                    self.release_coeff,
                );
                self.true_peak_gain_r = self.true_peak_gain_l;
                self.true_peak_hold_r = self.true_peak_hold_l;
            } else {
                Self::smooth_safety_gain(
                    &mut self.true_peak_gain_l,
                    &mut self.true_peak_hold_l,
                    safety_l,
                    self.release_coeff,
                );
                Self::smooth_safety_gain(
                    &mut self.true_peak_gain_r,
                    &mut self.true_peak_hold_r,
                    safety_r,
                    self.release_coeff,
                );
            }
        }
        if self.params.stereo_link {
            let linked = peak_l.max(peak_r);
            Self::smooth_envelope(
                &mut self.envelope_l,
                linked,
                self.attack_coeff,
                self.release_coeff,
            );
            self.envelope_r = self.envelope_l;
        } else {
            Self::smooth_envelope(
                &mut self.envelope_l,
                peak_l,
                self.attack_coeff,
                self.release_coeff,
            );
            Self::smooth_envelope(
                &mut self.envelope_r,
                peak_r,
                self.attack_coeff,
                self.release_coeff,
            );
        }

        let gain_l = self.compute_gain(self.envelope_l);
        let gain_r = self.compute_gain(self.envelope_r);
        let delayed_driven_l = delayed_l * input_gain;
        let delayed_driven_r = delayed_r * input_gain;
        // A final sample-peak safety gain makes ceiling compliance independent
        // of style attack. The lookahead envelope still supplies the musical
        // shape; this guard only catches what would otherwise overshoot.
        let safety_l = (self.ceiling_linear / delayed_driven_l.abs().max(1.0e-12)).min(1.0);
        let safety_r = (self.ceiling_linear / delayed_driven_r.abs().max(1.0e-12)).min(1.0);
        let (applied_l, applied_r) = if self.params.stereo_link {
            let linked = gain_l
                .min(gain_r)
                .min(self.true_peak_gain_l)
                .min(self.true_peak_gain_r)
                .min(safety_l)
                .min(safety_r);
            (linked, linked)
        } else {
            (
                gain_l.min(self.true_peak_gain_l).min(safety_l),
                gain_r.min(self.true_peak_gain_r).min(safety_r),
            )
        };
        let deepest_gain = applied_l.min(applied_r);
        self.gr_db = -linear_to_db(deepest_gain.max(1.0e-12));

        let wet_l = delayed_driven_l * applied_l;
        let wet_r = delayed_driven_r * applied_r;

        // The limited signal eases in and out with Power: scaling the wet
        // share is the same as blending the dry and processed outputs.
        let amount = amount * on;
        let out_l = mix(delayed_l, wet_l, amount);
        let out_r = mix(delayed_r, wet_r, amount);
        self.meters.push_stereo(driven_l, driven_r, out_l, out_r);
        (out_l, out_r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOCK: usize = 128;
    /// About a second of blocks: one drag across a range and back.
    const DRAG_BLOCKS: usize = 375;

    /// Two low tones: smooth, so a gain that steps shows as a kink.
    fn tone(n: usize) -> (f32, f32) {
        let t = n as f64 / 48_000.0;
        let x = (std::f64::consts::TAU * 110.0 * t).sin() * 0.3
            + (std::f64::consts::TAU * 330.0 * t).sin() * 0.1;
        (x as f32, (x * 0.8) as f32)
    }

    /// The parameter sweep's measure: the biggest second difference where a
    /// block meets the last one, against the biggest one inside a block,
    /// while `change` edits the DSP before each 128-frame block. About 1 is
    /// smooth; a value that steps once per block stands out many times over.
    fn edge_jump(dsp: &mut Dsp, mut change: impl FnMut(&mut Dsp, usize)) -> f32 {
        let mut n = 0;
        let mut last = [(0.0f32, 0.0f32); 2];
        for _ in 0..200 * BLOCK {
            let (l, r) = tone(n);
            n += 1;
            last = [last[1], dsp.process_stereo(l, r)];
        }
        let d2 = |a: f32, b: f32, c: f32| (c - 2.0 * b + a).abs();
        let mut worst = 0.0f32;
        for block in 0..DRAG_BLOCKS {
            change(dsp, block);
            let (mut edge, mut inside) = (0.0f32, 1.0e-7f32);
            for i in 0..BLOCK {
                let (l, r) = tone(n);
                n += 1;
                let out = dsp.process_stereo(l, r);
                assert!(out.0.is_finite() && out.1.is_finite());
                let kink = d2(last[0].0, last[1].0, out.0).max(d2(last[0].1, last[1].1, out.1));
                if i < 2 {
                    edge = edge.max(kink);
                } else {
                    inside = inside.max(kink);
                }
                last = [last[1], out];
            }
            worst = worst.max(edge / inside);
        }
        worst
    }

    /// Up and back down across `[min, max]`, a value per block.
    fn drag(min: f32, max: f32, block: usize) -> f32 {
        let phase = block as f32 / DRAG_BLOCKS as f32 * 2.0;
        let x = if phase < 1.0 { phase } else { 2.0 - phase };
        min + (max - min) * x
    }

    /// A click every quarter second, through every step of a switch.
    fn click(dsp: &mut Dsp, index: u32, steps: usize, block: usize) {
        if block.is_multiple_of(94) {
            dsp.apply_wire_param(index, ((block / 94) % steps) as f32);
        }
    }

    /// A limiter the sweep's tone drives into the ceiling.
    fn limiting() -> Dsp {
        let mut dsp = Dsp::new(48_000.0);
        let mut params = default_params();
        params.gain_db = 12.0;
        dsp.set_params(params);
        dsp
    }

    #[test]
    fn dragged_gain_ceiling_and_mix_do_not_zipper() {
        for (index, min, max) in [
            (ipc::GAIN_INDEX, -12.0, 24.0),
            (ipc::CEILING_INDEX, -6.0, 0.0),
            (ipc::MIX_INDEX, 0.0, 100.0),
        ] {
            for start in [default_params(), limiting().params().clone()] {
                let mut dsp = Dsp::new(48_000.0);
                dsp.set_params(start);
                let jump = edge_jump(&mut dsp, |dsp, block| {
                    dsp.apply_wire_param(index, drag(min, max, block));
                });
                assert!(
                    jump < 4.0,
                    "{}: block-edge jump {jump}",
                    UI_PARAM_IDS[index as usize]
                );
            }
        }
    }

    #[test]
    fn a_gliding_gain_still_holds_the_ceiling() {
        let mut dsp = limiting();
        let ceiling = db_to_linear(dsp.params().ceiling_db);
        for block in 0..375 {
            dsp.apply_wire_param(ipc::GAIN_INDEX, drag(0.0, 24.0, block));
            for i in 0..128 {
                let x = ((block * 128 + i) as f32 * 0.06).sin() * 0.9;
                let (l, r) = dsp.process_stereo(x, -x);
                assert!(l.abs() <= ceiling + 1.0e-6 && r.abs() <= ceiling + 1.0e-6);
            }
        }
    }

    #[test]
    fn a_dragged_lookahead_crossfades_instead_of_clearing() {
        let mut dsp = limiting();
        let jump = edge_jump(&mut dsp, |dsp, block| {
            dsp.apply_wire_param(ipc::LOOKAHEAD_INDEX, drag(0.0, 10.0, block));
        });
        assert!(jump < 4.0, "lookahead: block-edge jump {jump}");
        // It lands on the latest value, which is what it reports.
        assert!(dsp.apply_ui_param("lookaheadMs", 5.0));
        assert_eq!(dsp.latency_samples(), 240);
        assert!(dsp.apply_ui_param("power", 0.0));
        for _ in 0..4_800 {
            dsp.process_stereo(0.0, 0.0);
        }
        let out: Vec<f32> = (0..480)
            .map(|i| dsp.process_stereo(if i == 0 { 0.5 } else { 0.0 }, 0.0).0)
            .collect();
        assert_eq!(out[240], 0.5);
        assert!(out.iter().enumerate().all(|(i, s)| i == 240 || *s == 0.0));
    }

    #[test]
    fn switches_crossfade_instead_of_stepping() {
        for (index, steps) in [
            (ipc::POWER_INDEX, 2),
            (ipc::TRUE_PEAK_INDEX, 2),
            (ipc::STYLE_INDEX, 4),
        ] {
            let mut dsp = limiting();
            let jump = edge_jump(&mut dsp, |dsp, block| click(dsp, index, steps, block));
            assert!(
                jump < 4.0,
                "{}: block-edge jump {jump}",
                UI_PARAM_IDS[index as usize]
            );
        }
    }

    #[test]
    fn faded_out_power_is_a_delayed_bit_exact_bypass() {
        let mut dsp = limiting();
        assert!(dsp.apply_ui_param("power", 0.0));
        let delay = dsp.latency_samples();
        for _ in 0..4_800 {
            dsp.process_stereo(0.0, 0.0);
        }
        let out: Vec<(f32, f32)> = (0..delay + 1)
            .map(|i| dsp.process_stereo(if i == 0 { 0.9 } else { 0.0 }, -0.25))
            .collect();
        assert_eq!(out[delay], (0.9, -0.25));
    }

    fn run_tone(dsp: &mut Dsp, amplitude: f32, samples: usize) -> MeterFrame {
        for n in 0..samples {
            let x = (n as f32 * 0.05).sin() * amplitude;
            let (l, r) = dsp.process_stereo(x, x);
            assert!(l.is_finite() && r.is_finite());
        }
        dsp.meter_frame()
    }

    #[test]
    fn descriptor_ids_are_unique_and_match_defaults() {
        let d = descriptor();
        assert_eq!(d.id, PLUGIN_ID);

        let mut ids: Vec<_> = d.params.iter().map(|p| p.id).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(count, ids.len());

        let defaults = ipc::ui_values(&default_params());
        for param in d.params {
            let (_, actual) = defaults
                .iter()
                .find(|(id, _)| *id == param.id)
                .copied()
                .unwrap_or_else(|| panic!("`{}` is missing from ui_values", param.id));
            assert!(
                (actual - param.default_value).abs() < 1.0e-5,
                "{} default drifted",
                param.id
            );
        }
    }

    #[test]
    fn hot_signal_is_held_under_ceiling() {
        let mut dsp = Dsp::new(48_000.0);
        let mut params = default_params();
        params.gain_db = 12.0;
        params.ceiling_db = -1.0;
        params.true_peak = false;
        params.style = Style::Clip;
        params.lookahead_ms = 3.0;
        params.release_ms = 50.0;
        dsp.set_params(params);

        let mut peak = 0.0f32;
        for _ in 0..8_000 {
            let (l, r) = dsp.process_stereo(0.9, 0.9);
            peak = peak.max(l.abs()).max(r.abs());
        }
        let ceiling = db_to_linear(-1.0);
        assert!(
            peak <= ceiling * 1.000_01,
            "peak {peak} exceeded ceiling {ceiling}"
        );
        assert!(dsp.gain_reduction_db() > 1.0);
    }

    #[test]
    fn transient_ceiling_is_strict_for_every_style() {
        for style in [Style::Clean, Style::Punch, Style::Modern, Style::Clip] {
            let mut dsp = Dsp::new(48_000.0);
            let mut params = default_params();
            params.style = style;
            params.gain_db = 24.0;
            params.ceiling_db = -3.0;
            params.true_peak = true;
            params.lookahead_ms = 0.0;
            params.mix = 100.0;
            dsp.set_params(params);

            let ceiling = db_to_linear(-3.0);
            for index in 0..512 {
                let sample = if index % 31 == 0 { 1.0 } else { -0.91 };
                let (left, right) = dsp.process_stereo(sample, -sample);
                assert!(
                    left.abs().max(right.abs()) <= ceiling * 1.000_01,
                    "{style:?} exceeded ceiling at sample {index}: {left}, {right}"
                );
            }
        }
    }

    #[test]
    fn lookahead_reports_latency() {
        let mut dsp = Dsp::new(48_000.0);
        let mut params = default_params();
        params.lookahead_ms = 5.0;
        dsp.set_params(params);
        assert_eq!(dsp.latency_samples(), 240);
    }

    #[test]
    fn true_peak_has_detector_latency_but_does_not_lower_the_user_ceiling() {
        let mut dsp = Dsp::new(48_000.0);
        let mut params = default_params();
        params.lookahead_ms = 0.0;
        params.true_peak = true;
        params.ceiling_db = -1.0;
        dsp.set_params(params);
        assert_eq!(dsp.latency_samples(), TRUE_PEAK_MIN_DELAY_SAMPLES);
        assert!((dsp.ceiling_linear - db_to_linear(-1.0)).abs() < 1.0e-6);
    }

    #[test]
    fn true_peak_detector_catches_intersample_overshoot() {
        let mut detector = TruePeakDetector::default();
        let peak = [-1.0, 1.0, 1.0, -1.0]
            .into_iter()
            .fold(0.0_f32, |peak, sample| peak.max(detector.push(sample)));
        assert!(peak > 1.2, "expected cubic overshoot, measured {peak}");
    }

    #[test]
    fn true_peak_output_holds_reconstructed_signal_under_ceiling() {
        let mut dsp = Dsp::new(48_000.0);
        let mut params = default_params();
        params.style = Style::Clip;
        params.gain_db = 0.0;
        params.ceiling_db = -1.0;
        params.lookahead_ms = 0.0;
        params.true_peak = true;
        params.mix = 100.0;
        dsp.set_params(params);

        // The samples themselves are below -1 dBFS, but Catmull-Rom
        // reconstruction reaches roughly +0.5 dBFS without TP limiting.
        let pattern = [-0.85, 0.85, 0.85, -0.85];
        let mut output_detector = TruePeakDetector::default();
        let mut reconstructed_peak = 0.0_f32;
        for index in 0..1_024 {
            let sample = pattern[index % pattern.len()];
            let (left, _) = dsp.process_stereo(sample, sample);
            reconstructed_peak = reconstructed_peak.max(output_detector.push(left));
        }

        let ceiling = db_to_linear(-1.0);
        assert!(
            reconstructed_peak <= ceiling * 1.001,
            "reconstructed peak {reconstructed_peak} exceeded ceiling {ceiling}"
        );
    }

    #[test]
    fn dry_wet_paths_are_latency_aligned() {
        let mut dsp = Dsp::new(48_000.0);
        let mut params = default_params();
        params.gain_db = 0.0;
        params.lookahead_ms = 1.0;
        params.true_peak = false;
        params.mix = 50.0;
        dsp.set_params(params);

        let delay = dsp.latency_samples();
        for index in 0..=delay {
            let input = if index == 0 { 0.25 } else { 0.0 };
            let (left, right) = dsp.process_stereo(input, input);
            let expected = if index == delay { 0.25 } else { 0.0 };
            assert!((left - expected).abs() < 1.0e-6);
            assert!((right - expected).abs() < 1.0e-6);
        }
    }

    #[test]
    fn anti_phase_input_does_not_cancel_the_meters() {
        let mut dsp = Dsp::new(48_000.0);
        for _ in 0..512 {
            let _ = dsp.process_stereo(0.75, -0.75);
        }
        let frame = dsp.meter_frame();
        assert!(frame.in_peak >= 0.74);
        assert!(frame.in_rms > 0.0);
        assert!(frame.out_peak > 0.0);
    }

    #[test]
    fn power_off_reports_zero_reduction() {
        let mut dsp = Dsp::new(48_000.0);
        let mut params = default_params();
        params.gain_db = 18.0;
        dsp.set_params(params);
        let _ = run_tone(&mut dsp, 0.9, 1_000);
        assert!(dsp.apply_ui_param("power", 0.0));
        let frame = run_tone(&mut dsp, 0.9, 256);
        assert_eq!(frame.gain_reduction_db, 0.0);
    }

    /// The output peak of a steady 1 kHz tone held at `input_db`, once the
    /// envelopes have settled.
    fn settled_peak_db(dsp: &mut Dsp, input_db: f32) -> f32 {
        let amplitude = db_to_linear(input_db);
        let rate = 48_000.0;
        let mut peak = 0.0f32;
        for n in 0..48_000 {
            let x = (std::f32::consts::TAU * 1_000.0 * n as f32 / rate).sin() * amplitude;
            let (l, _) = dsp.process_stereo(x, x);
            if n >= 36_000 {
                peak = peak.max(l.abs());
            }
        }
        linear_to_db(peak.max(1.0e-9))
    }

    /// The curve the editor draws is the level the limiter settles a tone
    /// at.
    #[test]
    fn the_transfer_curve_is_what_a_steady_tone_comes_out_at() {
        for (style, gain, input) in [
            (Style::Modern, 6.0, -12.0),
            (Style::Clean, 12.0, -6.0),
            (Style::Clip, 0.0, -20.0),
        ] {
            let mut params = default_params();
            params.style = style;
            params.gain_db = gain;
            let mut dsp = Dsp::new(48_000.0);
            dsp.set_params(params.clone());
            let measured = settled_peak_db(&mut dsp, input);
            let drawn = transfer_db(&params, input);
            assert!(
                (measured - drawn).abs() < 0.5,
                "{style:?} +{gain} dB at {input} dB: measured {measured}, drawn {drawn}"
            );
        }
    }

    /// An insert being set up replays its stored values before any audio:
    /// they land at once instead of gliding in.
    #[test]
    fn edits_before_the_first_sample_land_at_once() {
        let mut dsp = Dsp::new(48_000.0);
        assert!(dsp.apply_ui_param("gainDb", 6.0));
        assert!(dsp.apply_ui_param("lookaheadMs", 5.0));
        assert_eq!(dsp.input_gain.outer.value, db_to_linear(6.0));
        assert_eq!(dsp.delay.switch.next(), (240, 240, 1.0));
    }
}

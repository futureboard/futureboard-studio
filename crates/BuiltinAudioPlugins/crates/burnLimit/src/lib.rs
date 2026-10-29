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
pub mod loudness;
pub mod oversample;
pub mod presets;
pub mod telemetry;
pub mod ui;

pub use ipc::{UI_PARAM_IDS, ui_param_id, ui_param_index};
pub use presets::{FactoryPreset, factory_presets};

use loudness::Loudness;
use oversample::{AntiAlias, Ring, TAPS_PER_PHASE};

pub const PLUGIN_ID: &str = "futureboard.burnlimit";

const CLIP_THRESHOLD: f32 = 1.0;
const RMS_WINDOW_SECONDS: f32 = 0.300;
const PEAK_FALL_SECONDS: f32 = 0.400;

/// Maximum lookahead buffer. Covers the declared 10 ms range through 384 kHz,
/// plus the oversampler's round trip.
const MAX_LOOKAHEAD_SAMPLES: usize = 4_096;
/// The oversampled cores' lookahead lines, per factor (2×, 4×, 8×): the
/// same 10 ms at 384 kHz, times the factor.
const OVERSAMPLED_LOOKAHEAD: [usize; 3] = [8_192, 16_384, 32_768];
const TRUE_PEAK_PHASES: [f32; 3] = [0.25, 0.5, 0.75];
/// Four-point cubic interpolation needs one future sample. Three samples of
/// latency ensure the complete cubic support is known before its first sample
/// exits the delay line.
const TRUE_PEAK_MIN_DELAY_SAMPLES: usize = 3;
const TRUE_PEAK_GAIN_HOLD_SAMPLES: usize = 4;
/// Gain / ceiling / mix smoothing, per stage of a [`Glide`], so a dragged
/// knob cannot zipper.
const SMOOTH_MS: f32 = 7.0;
/// Length of the Power crossfade and of a lookahead change's read-position
/// crossfade.
const FADE_MS: f32 = 10.0;

/// Limiter character. Wire order is the persisted contract: Loud and Safe
/// were appended after Clip, so a saved `3` still means Clip. The editor
/// calls Clip "Burn" and lists the styles in [`Style::UI_ORDER`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Style {
    Clean,
    Punch,
    Modern,
    Clip,
    Loud,
    Safe,
}

impl Style {
    /// The order the editor shows the styles in.
    pub const UI_ORDER: [Style; 6] = [
        Style::Clean,
        Style::Punch,
        Style::Loud,
        Style::Modern,
        Style::Safe,
        Style::Clip,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::Punch => "punch",
            Self::Modern => "modern",
            Self::Clip => "clip",
            Self::Loud => "loud",
            Self::Safe => "safe",
        }
    }

    /// The editor's name for the style.
    pub fn label(self) -> &'static str {
        match self {
            Self::Clean => "Clean",
            Self::Punch => "Punch",
            Self::Modern => "Modern",
            Self::Clip => "Burn",
            Self::Loud => "Loud",
            Self::Safe => "Safe",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "clean" => Some(Self::Clean),
            "punch" => Some(Self::Punch),
            "modern" => Some(Self::Modern),
            "clip" | "burn" => Some(Self::Clip),
            "loud" => Some(Self::Loud),
            "safe" => Some(Self::Safe),
            _ => None,
        }
    }

    pub const fn to_wire(self) -> f32 {
        match self {
            Self::Clean => 0.0,
            Self::Punch => 1.0,
            Self::Modern => 2.0,
            Self::Clip => 3.0,
            Self::Loud => 4.0,
            Self::Safe => 5.0,
        }
    }

    pub fn from_wire(value: f32) -> Self {
        match value.round() as i32 {
            1 => Self::Punch,
            2 => Self::Modern,
            3 => Self::Clip,
            4 => Self::Loud,
            5 => Self::Safe,
            _ => Self::Clean,
        }
    }

    /// Soft-knee width in dB — Safe is softest, Clip is nearly hard. The
    /// user's Knee widens it further.
    pub fn knee_db(self) -> f32 {
        match self {
            Self::Clean => 4.0,
            Self::Punch => 2.0,
            Self::Modern => 1.5,
            Self::Clip => 0.2,
            Self::Loud => 3.5,
            Self::Safe => 6.0,
        }
    }

    /// Attack seconds layered under the user's release / lookahead.
    pub fn attack_sec(self) -> f32 {
        match self {
            Self::Clean => 0.002,
            Self::Punch => 0.0008,
            Self::Modern => 0.0004,
            Self::Clip => 0.00005,
            Self::Loud => 0.0003,
            Self::Safe => 0.004,
        }
    }

    /// What the style does to the user's release time: Loud lets go twice
    /// as fast, Safe more than twice as slow. The original four play the
    /// release as set.
    pub fn release_scale(self) -> f32 {
        match self {
            Self::Clean | Self::Punch | Self::Modern | Self::Clip => 1.0,
            Self::Loud => 0.5,
            Self::Safe => 2.2,
        }
    }
}

/// Output word-length reduction: TPDF dither at one LSB of the depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Dither {
    #[default]
    Off,
    Bits16,
    Bits24,
}

impl Dither {
    pub const ALL: [Dither; 3] = [Dither::Off, Dither::Bits16, Dither::Bits24];

    pub const fn to_wire(self) -> f32 {
        match self {
            Self::Off => 0.0,
            Self::Bits16 => 1.0,
            Self::Bits24 => 2.0,
        }
    }

    pub fn from_wire(value: f32) -> Self {
        match value.round() as i32 {
            1 => Self::Bits16,
            2 => Self::Bits24,
            _ => Self::Off,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Bits16 => "16-bit",
            Self::Bits24 => "24-bit",
        }
    }

    /// One least significant bit at the depth, full scale ±1; 0 when off.
    pub fn lsb(self) -> f32 {
        match self {
            Self::Off => 0.0,
            Self::Bits16 => 1.0 / 32_768.0,
            Self::Bits24 => 1.0 / 8_388_608.0,
        }
    }
}

/// How fast the limiter core runs, as a multiple of the host rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Oversampling {
    #[default]
    Off,
    X2,
    X4,
    X8,
}

impl Oversampling {
    pub const ALL: [Oversampling; 4] = [
        Oversampling::Off,
        Oversampling::X2,
        Oversampling::X4,
        Oversampling::X8,
    ];

    pub const fn to_wire(self) -> f32 {
        match self {
            Self::Off => 0.0,
            Self::X2 => 1.0,
            Self::X4 => 2.0,
            Self::X8 => 3.0,
        }
    }

    pub fn from_wire(value: f32) -> Self {
        match value.round() as i32 {
            1 => Self::X2,
            2 => Self::X4,
            3 => Self::X8,
            _ => Self::Off,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::X2 => "2x",
            Self::X4 => "4x",
            Self::X8 => "8x",
        }
    }

    pub const fn factor(self) -> usize {
        match self {
            Self::Off => 1,
            Self::X2 => 2,
            Self::X4 => 4,
            Self::X8 => 8,
        }
    }

    /// Index into the oversampled paths, `None` when off.
    const fn path(self) -> Option<usize> {
        match self {
            Self::Off => None,
            Self::X2 => Some(0),
            Self::X4 => Some(1),
            Self::X8 => Some(2),
        }
    }

    /// Host samples the resampler round trip adds.
    pub const fn latency_samples(self) -> usize {
        match self {
            Self::Off => 0,
            _ => TAPS_PER_PHASE,
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
    /// Per-channel peaks, `[in L, in R, out L, out R]`, with the same fall.
    channel_peaks: [f32; 4],
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
            channel_peaks: [0.0; 4],
        }
    }

    fn reset(&mut self) {
        self.in_peak = 0.0;
        self.out_peak = 0.0;
        self.in_ms = 0.0;
        self.out_ms = 0.0;
        self.in_clip = false;
        self.out_clip = false;
        self.channel_peaks = [0.0; 4];
    }

    #[inline]
    fn push_stereo(&mut self, in_l: f32, in_r: f32, out_l: f32, out_r: f32) {
        for (peak, sample) in self
            .channel_peaks
            .iter_mut()
            .zip([in_l, in_r, out_l, out_r])
        {
            let level = sample.abs();
            *peak = if level > *peak {
                level
            } else {
                *peak * self.peak_coeff
            };
        }
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
    /// The original on/off link. Kept in the state and the wire table for
    /// old projects and automation; [`Params::link_pct`] is the amount the
    /// DSP plays, and the two are kept in step (`stereoLink` sets the amount
    /// to 100 or 0; `linkPct` sets this to `amount > 0`).
    pub stereo_link: bool,
    /// Added to the style's own knee width, dB. 0 plays the style as it was.
    #[serde(default)]
    pub knee_db: f32,
    /// Stereo link amount, 0 (each channel its own limiter) … 100 (one gain
    /// for both). State written before it existed has no `linkPct`: it
    /// loads as [`LINK_UNSET`] and is resolved from `stereo_link` by
    /// [`ipc::sanitize_params`], which `BurnLimitState::from_json` and
    /// [`Dsp::set_params`] both run.
    #[serde(default = "link_unset")]
    pub link_pct: f32,
    /// Gain-matched monitoring: the limited signal comes out turned down by
    /// the input gain, so the limiting is heard at the input's level.
    #[serde(default)]
    pub unity: bool,
    #[serde(default)]
    pub dither: Dither,
    #[serde(default)]
    pub oversampling: Oversampling,
    /// The loudness target the editor measures against, LUFS. A display
    /// reference only: nothing in the DSP reads it.
    #[serde(default = "default_target_lufs")]
    pub target_lufs: f32,
    /// A trigger, not a setting: any change of value resets the program
    /// loudness readings (integrated, range, true-peak and GR maxima). Never
    /// saved.
    #[serde(skip)]
    pub meter_reset: f32,
}

/// `link_pct` before it is resolved from `stereo_link`.
pub const LINK_UNSET: f32 = -1.0;

fn link_unset() -> f32 {
    LINK_UNSET
}

fn default_target_lufs() -> f32 {
    -14.0
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
        knee_db: 0.0,
        link_pct: 100.0,
        unity: false,
        dither: Dither::Off,
        oversampling: Oversampling::Off,
        target_lufs: default_target_lufs(),
        meter_reset: 0.0,
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
    let knee_db = params.style.knee_db() + params.knee_db;
    let wet_db = (driven_db - static_reduction_db(driven_db, params.ceiling_db, knee_db))
        .min(params.ceiling_db);
    let amount = clamp(params.mix, 0.0, 100.0) / 100.0;
    linear_to_db(mix(db_to_linear(input_db), db_to_linear(wet_db), amount).max(1.0e-9))
}

/// The lookahead `params` run at `sample_rate`, in samples: at least the
/// true-peak detector's.
fn lookahead_samples_for(params: &Params, sample_rate: f32) -> usize {
    let delay = ((params.lookahead_ms * 0.001) * sample_rate).round() as usize;
    if params.true_peak {
        delay.max(TRUE_PEAK_MIN_DELAY_SAMPLES)
    } else {
        delay
    }
}

/// The latency an insert with `params` reports at `sample_rate`: the
/// lookahead plus the resampler's delay when oversampling. What
/// [`Dsp::latency_samples`] reports, for an editor's readout.
pub fn latency_samples_for(params: &Params, sample_rate: f32) -> usize {
    (lookahead_samples_for(params, sample_rate) + params.oversampling.latency_samples())
        .min(MAX_LOOKAHEAD_SAMPLES - 1)
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
                max: 5.0,
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
                min: -12.0,
                max: 0.0,
                unit: "dB",
            },
            ParamDescriptor {
                id: "releaseMs",
                name: "Release",
                default_value: 200.0,
                min: 1.0,
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
            ParamDescriptor {
                id: "kneeDb",
                name: "Knee",
                default_value: 0.0,
                min: 0.0,
                max: 12.0,
                unit: "dB",
            },
            ParamDescriptor {
                id: "linkPct",
                name: "Link Amount",
                default_value: 100.0,
                min: 0.0,
                max: 100.0,
                unit: "%",
            },
            ParamDescriptor {
                id: "unity",
                name: "Unity Monitor",
                default_value: 0.0,
                min: 0.0,
                max: 1.0,
                unit: "bool",
            },
            ParamDescriptor {
                id: "dither",
                name: "Dither",
                default_value: 0.0,
                min: 0.0,
                max: 2.0,
                unit: "enum",
            },
            ParamDescriptor {
                id: "oversampling",
                name: "Oversampling",
                default_value: 0.0,
                min: 0.0,
                max: 3.0,
                unit: "enum",
            },
            ParamDescriptor {
                id: "targetLufs",
                name: "Loudness Target",
                default_value: -14.0,
                min: -36.0,
                max: 0.0,
                unit: "LUFS",
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
/// position instead of jumping (or clearing) the line. Its length is a power
/// of two fixed at construction.
#[derive(Debug, Clone)]
struct LookaheadLine {
    left: Box<[f32]>,
    right: Box<[f32]>,
    mask: usize,
    write: usize,
    switch: DelaySwitch,
}

impl LookaheadLine {
    fn new(capacity: usize, sample_rate: f32) -> Self {
        let capacity = capacity.next_power_of_two();
        Self {
            left: vec![0.0; capacity].into_boxed_slice(),
            right: vec![0.0; capacity].into_boxed_slice(),
            mask: capacity - 1,
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

    fn set_delay(&mut self, delay: usize) {
        self.switch.request(delay.min(self.mask));
    }

    #[inline]
    fn push_read(&mut self, left: f32, right: f32) -> (f32, f32) {
        let write = self.write;
        let mask = self.mask;
        self.left[write] = left;
        self.right[write] = right;
        self.write = (write + 1) & mask;
        let at = |delay: usize| (write + mask + 1 - delay) & mask;
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

/// The limiter's gain computer at one rate: the detectors, the envelopes
/// and the true-peak safety gains. It sees the driven signal twice — as it
/// arrives, to detect ahead, and as it leaves the lookahead line, for the
/// sample-peak guard — and returns the gain each channel gets.
#[derive(Debug, Clone)]
struct Core {
    attack_coeff: f32,
    release_coeff: f32,
    knee_db: f32,
    true_peak: bool,
    envelope_l: f32,
    envelope_r: f32,
    true_peak_l: TruePeakDetector,
    true_peak_r: TruePeakDetector,
    true_peak_gain_l: f32,
    true_peak_gain_r: f32,
    true_peak_hold_l: usize,
    true_peak_hold_r: usize,
}

impl Core {
    fn new() -> Self {
        Self {
            attack_coeff: 0.0,
            release_coeff: 0.0,
            knee_db: 1.5,
            true_peak: true,
            envelope_l: 0.0,
            envelope_r: 0.0,
            true_peak_l: TruePeakDetector::default(),
            true_peak_r: TruePeakDetector::default(),
            true_peak_gain_l: 1.0,
            true_peak_gain_r: 1.0,
            true_peak_hold_l: 0,
            true_peak_hold_r: 0,
        }
    }

    /// Coefficients for `params` at `sample_rate` (the core's own rate).
    fn configure(&mut self, sample_rate: f32, params: &Params) {
        let style = params.style;
        self.knee_db = style.knee_db() + params.knee_db;
        self.attack_coeff = time_constant(sample_rate, style.attack_sec());
        self.release_coeff = time_constant(
            sample_rate,
            params.release_ms * 0.001 * style.release_scale(),
        );
        self.true_peak = params.true_peak;
        if !params.true_peak {
            self.reset_true_peak();
        }
    }

    fn reset_true_peak(&mut self) {
        self.true_peak_l.reset();
        self.true_peak_r.reset();
        self.true_peak_gain_l = 1.0;
        self.true_peak_gain_r = 1.0;
        self.true_peak_hold_l = 0;
        self.true_peak_hold_r = 0;
    }

    fn reset(&mut self) {
        self.envelope_l = 0.0;
        self.envelope_r = 0.0;
        self.reset_true_peak();
    }

    /// Gain computer: how much linear gain keeps `level` at or under the ceiling.
    #[inline]
    fn compute_gain(&self, level: f32, ceiling_db: f32) -> f32 {
        if level <= 1.0e-12 {
            return 1.0;
        }
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

    /// The gain each channel gets for this sample. `link` is the link
    /// amount 0…1: at exactly 1 both channels share one gain, at exactly 0
    /// each has its own (the original two modes, unchanged), and between
    /// them each channel's detection and gain lean toward the linked ones.
    #[inline]
    fn gains(
        &mut self,
        (driven_l, driven_r): (f32, f32),
        (delayed_l, delayed_r): (f32, f32),
        ceiling_linear: f32,
        ceiling_db: f32,
        link: f32,
    ) -> (f32, f32) {
        // Detect on the undelayed path so GR can start before the peak exits.
        let peak_l = if self.true_peak {
            self.true_peak_l.push(driven_l)
        } else {
            driven_l.abs()
        };
        let peak_r = if self.true_peak {
            self.true_peak_r.push(driven_r)
        } else {
            driven_r.abs()
        };
        let linked = link >= 1.0;
        let independent = link <= 0.0;
        if self.true_peak {
            let safety_l = (ceiling_linear / peak_l.max(1.0e-12)).min(1.0);
            let safety_r = (ceiling_linear / peak_r.max(1.0e-12)).min(1.0);
            if linked {
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
                let both = safety_l.min(safety_r);
                let (target_l, target_r) = if independent {
                    (safety_l, safety_r)
                } else {
                    (blend(safety_l, both, link), blend(safety_r, both, link))
                };
                Self::smooth_safety_gain(
                    &mut self.true_peak_gain_l,
                    &mut self.true_peak_hold_l,
                    target_l,
                    self.release_coeff,
                );
                Self::smooth_safety_gain(
                    &mut self.true_peak_gain_r,
                    &mut self.true_peak_hold_r,
                    target_r,
                    self.release_coeff,
                );
            }
        }
        if linked {
            let linked = peak_l.max(peak_r);
            Self::smooth_envelope(
                &mut self.envelope_l,
                linked,
                self.attack_coeff,
                self.release_coeff,
            );
            self.envelope_r = self.envelope_l;
        } else {
            let both = peak_l.max(peak_r);
            let (target_l, target_r) = if independent {
                (peak_l, peak_r)
            } else {
                (blend(peak_l, both, link), blend(peak_r, both, link))
            };
            Self::smooth_envelope(
                &mut self.envelope_l,
                target_l,
                self.attack_coeff,
                self.release_coeff,
            );
            Self::smooth_envelope(
                &mut self.envelope_r,
                target_r,
                self.attack_coeff,
                self.release_coeff,
            );
        }

        let gain_l = self.compute_gain(self.envelope_l, ceiling_db);
        let gain_r = self.compute_gain(self.envelope_r, ceiling_db);
        // A final sample-peak safety gain makes ceiling compliance independent
        // of style attack. The lookahead envelope still supplies the musical
        // shape; this guard only catches what would otherwise overshoot.
        let safety_l = (ceiling_linear / delayed_l.abs().max(1.0e-12)).min(1.0);
        let safety_r = (ceiling_linear / delayed_r.abs().max(1.0e-12)).min(1.0);
        if linked {
            let linked = gain_l
                .min(gain_r)
                .min(self.true_peak_gain_l)
                .min(self.true_peak_gain_r)
                .min(safety_l)
                .min(safety_r);
            (linked, linked)
        } else {
            let own_l = gain_l.min(self.true_peak_gain_l).min(safety_l);
            let own_r = gain_r.min(self.true_peak_gain_r).min(safety_r);
            if independent {
                (own_l, own_r)
            } else {
                // Each blend stays at or under its own channel's guard.
                let both = own_l.min(own_r);
                (blend(own_l, both, link), blend(own_r, both, link))
            }
        }
    }
}

/// One oversampled limiter: the core at `factor`× the host rate, its own
/// lookahead line at that rate, and what it took off, waiting for the
/// decimator.
#[derive(Debug, Clone)]
struct OversampledPath {
    factor: usize,
    filter: AntiAlias,
    core: Core,
    line: LookaheadLine,
    residual_l: Ring,
    residual_r: Ring,
}

impl OversampledPath {
    fn new(factor: usize, capacity: usize, sample_rate: f32) -> Self {
        let filter = AntiAlias::new(factor);
        let history = factor - 1 + filter.taps.len();
        Self {
            factor,
            core: Core::new(),
            line: LookaheadLine::new(capacity, sample_rate * factor as f32),
            residual_l: Ring::new(history),
            residual_r: Ring::new(history),
            filter,
        }
    }

    fn reset(&mut self) {
        self.core.reset();
        self.line.reset();
        self.residual_l.clear();
        self.residual_r.clear();
    }
}

/// Telemetry: the loudness meter and the held maxima, on the output as it
/// is heard.
#[derive(Debug, Clone)]
struct Program {
    loudness: Loudness,
    true_peak_l: TruePeakDetector,
    true_peak_r: TruePeakDetector,
    /// Held, linear.
    true_peak_max: f32,
    /// Held, positive dB.
    gr_max_db: f32,
}

impl Program {
    fn new(sample_rate: f32) -> Self {
        Self {
            loudness: Loudness::new(sample_rate),
            true_peak_l: TruePeakDetector::default(),
            true_peak_r: TruePeakDetector::default(),
            true_peak_max: 0.0,
            gr_max_db: 0.0,
        }
    }

    /// The program readings — integrated, range, the maxima — back to none.
    fn reset_program(&mut self) {
        self.loudness.reset_program();
        self.true_peak_max = 0.0;
        self.gr_max_db = 0.0;
    }

    fn reset(&mut self) {
        self.loudness.reset();
        self.true_peak_l.reset();
        self.true_peak_r.reset();
        self.true_peak_max = 0.0;
        self.gr_max_db = 0.0;
    }

    #[inline]
    fn push(&mut self, left: f32, right: f32, gr_db: f32) {
        self.loudness.push(left, right);
        let peak = self
            .true_peak_l
            .push(left)
            .max(self.true_peak_r.push(right));
        self.true_peak_max = self.true_peak_max.max(peak);
        self.gr_max_db = self.gr_max_db.max(gr_db);
    }
}

/// Cheap uniform noise for the dither: xorshift32, 24-bit mantissa.
#[derive(Debug, Clone, Copy)]
struct Noise(u32);

impl Noise {
    #[inline]
    fn next(&mut self) -> f32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        (x >> 8) as f32 * (1.0 / 16_777_216.0)
    }

    /// Triangular noise over ±1.
    #[inline]
    fn tpdf(&mut self) -> f32 {
        self.next() + self.next() - 1.0
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
    /// The Unity monitor's output gain: `1 / input gain` while on.
    unity_gain: Glide,
    link_amount: Glide,
    smooth_step: f32,
    /// Dry ↔ limited: Power crossfades instead of switching.
    power: Fade,
    gr_db: f32,
    /// The host-rate core: what plays with oversampling off.
    core: Core,
    /// The 2×, 4× and 8× cores, all built up front so a switch allocates
    /// nothing.
    oversampled: Box<[OversampledPath; 3]>,
    /// The oversampling the audio runs at. Trails `params.oversampling`
    /// across a switch's fade.
    active_oversampling: Oversampling,
    /// The output's level across an oversampling switch: down, switch,
    /// back up.
    switch_fade: Fade,
    /// The oversampled path was idle (Power off) and has stale history.
    oversampled_idle: bool,
    /// The driven host-rate input, newest first, for the interpolator.
    drive_l: Ring,
    drive_r: Ring,
    delay: LookaheadLine,
    meters: Meters,
    program: Box<Program>,
    noise: Noise,
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
            unity_gain: Glide::at(1.0),
            link_amount: Glide::at(1.0),
            smooth_step: smoothing_step(SMOOTH_MS, sr),
            power: Fade::new(true, sr),
            gr_db: 0.0,
            core: Core::new(),
            oversampled: Box::new([
                OversampledPath::new(2, OVERSAMPLED_LOOKAHEAD[0], sr),
                OversampledPath::new(4, OVERSAMPLED_LOOKAHEAD[1], sr),
                OversampledPath::new(8, OVERSAMPLED_LOOKAHEAD[2], sr),
            ]),
            active_oversampling: Oversampling::Off,
            switch_fade: Fade::new(true, sr),
            oversampled_idle: false,
            drive_l: Ring::new(TAPS_PER_PHASE + 1),
            drive_r: Ring::new(TAPS_PER_PHASE + 1),
            delay: LookaheadLine::new(MAX_LOOKAHEAD_SAMPLES, sr),
            meters: Meters::new(sr),
            program: Box::new(Program::new(sr)),
            noise: Noise(0x9E37_79B9),
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
        let reset = params.meter_reset != self.params.meter_reset;
        self.params = params;
        ipc::sanitize_params(&mut self.params);
        if reset {
            self.program.reset_program();
        }
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

    /// The editor's telemetry block (layout in [`telemetry`]). Cheap: a copy
    /// of readings kept up to date as audio runs.
    pub fn telemetry(&self) -> [f32; telemetry::SLOTS] {
        use telemetry::*;
        let mut block = [0.0; SLOTS];
        let [in_l, in_r, out_l, out_r] = self.meters.channel_peaks;
        block[IN_L] = in_l;
        block[IN_R] = in_r;
        block[OUT_L] = out_l;
        block[OUT_R] = out_r;
        block[GR_DB] = self.meter_frame().gain_reduction_db;
        let loudness = &self.program.loudness;
        block[MOMENTARY_LUFS] = loudness.momentary;
        block[SHORT_LUFS] = loudness.short_term;
        block[INTEGRATED_LUFS] = loudness.integrated;
        block[LRA_LU] = loudness.range;
        block[TRUE_PEAK_DBTP] = if self.program.true_peak_max > 0.0 {
            linear_to_db(self.program.true_peak_max)
        } else {
            NO_READING
        };
        block[GR_MAX_DB] = self.program.gr_max_db;
        block[VALID] = if loudness.gated_blocks > 0 { 1.0 } else { 0.0 };
        block[LATENCY_SAMPLES] = self.latency_samples() as f32;
        block
    }

    pub fn clear_clip(&mut self) {
        self.meters.in_clip = false;
        self.meters.out_clip = false;
    }

    pub fn apply_wire_param(&mut self, wire_index: u32, value: f32) -> bool {
        let before = self.params.meter_reset;
        if !ipc::apply_wire_param(&mut self.params, wire_index, value) {
            return false;
        }
        if self.params.meter_reset != before {
            self.program.reset_program();
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

    /// The lookahead, plus the oversampler's round trip while it is on. A
    /// function of the settings alone, so it never wanders mid-switch.
    pub fn latency_samples(&self) -> usize {
        latency_samples_for(&self.params, self.sample_rate)
    }

    /// The lookahead in host samples, at least the true-peak detector's.
    fn lookahead_samples(&self) -> usize {
        lookahead_samples_for(&self.params, self.sample_rate)
    }

    fn apply_params(&mut self) {
        self.core.configure(self.sample_rate, &self.params);
        let lookahead = self.lookahead_samples();
        for path in self.oversampled.iter_mut() {
            path.core
                .configure(self.sample_rate * path.factor as f32, &self.params);
            path.line.set_delay(lookahead * path.factor);
        }
        self.input_gain.set(db_to_linear(self.params.gain_db));
        self.ceiling_db.set(self.params.ceiling_db);
        self.mix_amount.set(self.params.mix / 100.0);
        self.unity_gain.set(if self.params.unity {
            db_to_linear(-self.params.gain_db)
        } else {
            1.0
        });
        self.link_amount.set(self.params.link_pct / 100.0);
        self.power.set(self.params.power);
        self.delay.set_delay(
            (lookahead + self.active_oversampling.latency_samples()).min(MAX_LOOKAHEAD_SAMPLES - 1),
        );
    }

    /// Land the gains, the Power fade, the oversampling and the lookahead on
    /// their targets.
    fn settle(&mut self) {
        self.input_gain.settle();
        self.ceiling_db.settle();
        self.ceiling_linear = db_to_linear(self.ceiling_db.outer.value);
        self.mix_amount.settle();
        self.unity_gain.settle();
        self.link_amount.settle();
        self.power.settle();
        if self.active_oversampling != self.params.oversampling {
            self.switch_oversampling();
        }
        self.switch_fade.set(true);
        self.switch_fade.settle();
        self.delay.switch.settle();
        for path in self.oversampled.iter_mut() {
            path.line.switch.settle();
        }
    }

    /// Moves the audio onto the oversampling the params ask for, from
    /// clean state. Runs while the output is faded out (or before audio).
    fn switch_oversampling(&mut self) {
        self.active_oversampling = self.params.oversampling;
        match self.active_oversampling.path() {
            Some(index) => self.oversampled[index].reset(),
            None => self.core.reset(),
        }
        self.oversampled_idle = false;
        self.delay.set_delay(self.latency_samples());
        self.delay.switch.settle();
        let lookahead = self.lookahead_samples();
        for path in self.oversampled.iter_mut() {
            path.line.set_delay(lookahead * path.factor);
            path.line.switch.settle();
        }
    }

    /// The oversampled limiter for one host sample: interpolate the driven
    /// input, run the core at the factor's rate, and decimate what it took
    /// off. The result is added to the host-rate driven signal, delayed to
    /// match, so a signal the limiter leaves alone passes untouched.
    #[inline]
    fn oversampled_reduction(
        &mut self,
        index: usize,
        link: f32,
        ceiling_db: f32,
    ) -> (f32, f32, f32) {
        let taps = TAPS_PER_PHASE + 1;
        let ceiling_linear = self.ceiling_linear;
        let path = &mut self.oversampled[index];
        let factor = path.factor;
        let mut deepest = 1.0f32;
        for phase in 0..factor {
            let ul = path.filter.interpolate(self.drive_l.recent(taps), phase);
            let ur = path.filter.interpolate(self.drive_r.recent(taps), phase);
            let (dl, dr) = path.line.push_read(ul, ur);
            let (gl, gr) = path
                .core
                .gains((ul, ur), (dl, dr), ceiling_linear, ceiling_db, link);
            deepest = deepest.min(gl.min(gr));
            path.residual_l.push(dl * gl - dl);
            path.residual_r.push(dr * gr - dr);
        }
        // The decimator's newest tap is this step's first phase, which lines
        // the residual up with the driven input the full latency ago.
        let len = path.filter.taps.len();
        let rl = path
            .filter
            .decimate(&path.residual_l.recent(factor - 1 + len)[factor - 1..]);
        let rr = path
            .filter
            .decimate(&path.residual_r.recent(factor - 1 + len)[factor - 1..]);
        (rl, rr, deepest)
    }
}

impl StereoEffect for Dsp {
    fn reset(&mut self) {
        self.started = false;
        self.gr_db = 0.0;
        self.core.reset();
        for path in self.oversampled.iter_mut() {
            path.reset();
        }
        self.drive_l.clear();
        self.drive_r.clear();
        self.oversampled_idle = false;
        self.delay.reset();
        self.meters.reset();
        self.program.reset();
        self.settle();
    }

    fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        self.meters = Meters::new(self.sample_rate);
        self.program.loudness = Loudness::new(self.sample_rate);
        self.smooth_step = smoothing_step(SMOOTH_MS, self.sample_rate);
        self.power.step = fade_step(self.sample_rate);
        self.switch_fade.step = fade_step(self.sample_rate);
        self.delay.switch.step = fade_step(self.sample_rate);
        for path in self.oversampled.iter_mut() {
            path.line.switch.step = fade_step(self.sample_rate * path.factor as f32);
        }
        self.apply_params();
        self.settle();
    }

    fn process_stereo(&mut self, left: f32, right: f32) -> (f32, f32) {
        self.started = true;
        // An oversampling change fades the output out, switches, and fades
        // back in: the latency moves with it, so there is nothing to blend.
        let switching = self.params.oversampling != self.active_oversampling;
        self.switch_fade.set(!switching);
        let level = self.switch_fade.next();
        if switching && level <= 0.0 {
            self.switch_oversampling();
            self.switch_fade.set(true);
        }
        let fade = |x: f32| if level >= 1.0 { x } else { x * level };

        // The delay always carries the unprocessed signal. This keeps bypass
        // and dry/wet mixing aligned with the latency reported to the host.
        let (delayed_l, delayed_r) = self.delay.push_read(left, right);
        let on = self.power.next();
        if on <= 0.0 {
            // Off and faded out: only the delay runs (and the interpolator's
            // history, so a return to Power has real input to work from).
            let gain = self.input_gain.outer.value;
            self.drive_l.push(left * gain);
            self.drive_r.push(right * gain);
            self.oversampled_idle = true;
            self.meters.push_stereo(left, right, delayed_l, delayed_r);
            self.core.reset();
            self.gr_db = 0.0;
            let out = (fade(delayed_l), fade(delayed_r));
            self.program.push(out.0, out.1, 0.0);
            return out;
        }

        let step = self.smooth_step;
        let input_gain = self.input_gain.next(step);
        let ceiling_db = if self.ceiling_db.moving() {
            let db = self.ceiling_db.next(step);
            self.ceiling_linear = db_to_linear(db);
            db
        } else {
            self.ceiling_db.outer.value
        };
        let amount = self.mix_amount.next(step);
        let unity = self.unity_gain.next(step);
        let link = self.link_amount.next(step);

        let driven_l = left * input_gain;
        let driven_r = right * input_gain;
        let delayed_driven_l = delayed_l * input_gain;
        let delayed_driven_r = delayed_r * input_gain;

        let (wet_l, wet_r, deepest_gain) = match self.active_oversampling.path() {
            None => {
                let (applied_l, applied_r) = self.core.gains(
                    (driven_l, driven_r),
                    (delayed_driven_l, delayed_driven_r),
                    self.ceiling_linear,
                    ceiling_db,
                    link,
                );
                (
                    delayed_driven_l * applied_l,
                    delayed_driven_r * applied_r,
                    applied_l.min(applied_r),
                )
            }
            Some(index) => {
                self.drive_l.push(driven_l);
                self.drive_r.push(driven_r);
                if self.oversampled_idle {
                    self.oversampled[index].reset();
                    self.oversampled_idle = false;
                }
                let (rl, rr, deepest) = self.oversampled_reduction(index, link, ceiling_db);
                let (wet_l, wet_r) = (delayed_driven_l + rl, delayed_driven_r + rr);
                // The decimated output can ring a hair past the ceiling the
                // oversampled core held: a host-rate sample-peak guard.
                let guard_l = (self.ceiling_linear / wet_l.abs().max(1.0e-12)).min(1.0);
                let guard_r = (self.ceiling_linear / wet_r.abs().max(1.0e-12)).min(1.0);
                let both = guard_l.min(guard_r);
                let (guard_l, guard_r) = (blend(guard_l, both, link), blend(guard_r, both, link));
                (
                    wet_l * guard_l,
                    wet_r * guard_r,
                    deepest.min(guard_l).min(guard_r),
                )
            }
        };
        if self.active_oversampling == Oversampling::Off {
            // Keep the interpolator's history real for a switch to come.
            self.drive_l.push(driven_l);
            self.drive_r.push(driven_r);
        }
        self.gr_db = -linear_to_db(deepest_gain.max(1.0e-12));

        // Unity monitoring turns the limited signal back down by the drive.
        let wet_l = wet_l * unity;
        let wet_r = wet_r * unity;

        // The limited signal eases in and out with Power: scaling the wet
        // share is the same as blending the dry and processed outputs.
        let amount = amount * on;
        let mut out_l = mix(delayed_l, wet_l, amount);
        let mut out_r = mix(delayed_r, wet_r, amount);
        let lsb = self.params.dither.lsb();
        if lsb > 0.0 {
            let depth = lsb * on;
            out_l += self.noise.tpdf() * depth;
            out_r += self.noise.tpdf() * depth;
        }
        let (out_l, out_r) = (fade(out_l), fade(out_r));
        self.meters.push_stereo(driven_l, driven_r, out_l, out_r);
        self.program.push(out_l, out_r, self.gr_db.max(0.0));
        (out_l, out_r)
    }
}

/// The reading a loudness slot holds before anything is measured.
pub use loudness::NO_READING;

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
            (ipc::STYLE_INDEX, 6),
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
        for style in Style::UI_ORDER {
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

    /// Settled output for `input` per channel, held long enough for the
    /// envelopes: the last sample's pair.
    fn settle_on(dsp: &mut Dsp, input: impl Fn(usize) -> (f32, f32), samples: usize) -> (f32, f32) {
        let mut out = (0.0, 0.0);
        for n in 0..samples {
            let (l, r) = input(n);
            out = dsp.process_stereo(l, r);
        }
        out
    }

    #[test]
    fn link_amount_decides_whether_the_quiet_channel_ducks() {
        let quiet = 0.1f32;
        let run = |link: f32| {
            let mut params = default_params();
            params.gain_db = 12.0;
            params.link_pct = link;
            let mut dsp = Dsp::new(48_000.0);
            dsp.set_params(params);
            // Left hot, right quiet, both steady DC so the reading is exact.
            settle_on(&mut dsp, |_| (0.9, quiet), 24_000)
        };
        let driven_quiet = quiet * db_to_linear(12.0);
        let (_, independent) = run(0.0);
        let (_, half) = run(50.0);
        let (_, linked) = run(100.0);
        assert!(
            (independent - driven_quiet).abs() < 1.0e-4,
            "unlinked right {independent} vs {driven_quiet}"
        );
        assert!(linked < driven_quiet * 0.5, "linked right {linked}");
        assert!(half < independent && half > linked, "half link {half}");
    }

    #[test]
    fn unity_monitoring_plays_the_limiter_at_the_input_level() {
        let mut params = default_params();
        params.gain_db = 12.0;
        params.unity = true;
        let mut dsp = Dsp::new(48_000.0);
        dsp.set_params(params);
        // Well under the ceiling: nothing is limited, so unity is the input.
        let (l, r) = settle_on(&mut dsp, |_| (0.05, -0.02), 4_800);
        assert!(
            (l - 0.05).abs() < 1.0e-6 && (r + 0.02).abs() < 1.0e-6,
            "{l} {r}"
        );
        assert!(dsp.apply_ui_param("unity", 0.0));
        let (l, _) = settle_on(&mut dsp, |_| (0.05, -0.02), 4_800);
        assert!((l - 0.05 * db_to_linear(12.0)).abs() < 1.0e-5, "{l}");
    }

    #[test]
    fn dither_off_is_transparent_and_on_is_one_lsb_of_noise() {
        let input = |n: usize| {
            (
                (n as f32 * 0.01).sin() * 0.25,
                (n as f32 * 0.013).cos() * 0.25,
            )
        };
        let mut params = default_params();
        params.lookahead_ms = 0.0;
        params.true_peak = false;
        let mut plain = Dsp::new(48_000.0);
        plain.set_params(params.clone());
        params.dither = Dither::Bits16;
        let mut dithered = Dsp::new(48_000.0);
        dithered.set_params(params);
        let (mut worst, mut sum, mut moved) = (0.0f32, 0.0f64, 0usize);
        for n in 0..48_000 {
            let (l, r) = input(n);
            let a = plain.process_stereo(l, r);
            // Off: the limiter passes this level untouched, bit for bit.
            assert_eq!(a, (l, r));
            let b = dithered.process_stereo(l, r);
            let noise = b.0 - a.0;
            worst = worst.max(noise.abs()).max((b.1 - a.1).abs());
            sum += f64::from(noise);
            moved += usize::from(noise != 0.0);
        }
        let lsb = 1.0 / 32_768.0;
        assert!(worst <= lsb * 1.001, "dither peak {worst}");
        assert!(moved > 40_000, "dither should move nearly every sample");
        assert!((sum / 48_000.0).abs() < lsb as f64 * 0.02, "dither mean");
    }

    #[test]
    fn every_oversampling_factor_is_transparent_below_the_ceiling_and_holds_it_above() {
        for os in Oversampling::ALL {
            let mut params = default_params();
            params.oversampling = os;
            params.lookahead_ms = 2.0;
            let mut dsp = Dsp::new(48_000.0);
            dsp.set_params(params);
            let latency = dsp.latency_samples();
            assert_eq!(latency, 96 + os.latency_samples(), "{os:?}");
            // An impulse under the ceiling comes out alone, exactly, after
            // the reported latency.
            let out: Vec<(f32, f32)> = (0..latency + 64)
                .map(|n| dsp.process_stereo(if n == 0 { 0.5 } else { 0.0 }, 0.0))
                .collect();
            assert_eq!(out[latency], (0.5, 0.0), "{os:?}");
            assert!(
                out.iter()
                    .enumerate()
                    .all(|(i, s)| i == latency || s.0 == 0.0)
            );

            // Driven 18 dB into a -1 dB ceiling: bounded, finite, held.
            assert!(dsp.apply_ui_param("gainDb", 18.0));
            assert!(dsp.apply_ui_param("ceilingDb", -1.0));
            let ceiling = db_to_linear(-1.0);
            for n in 0..48_000 {
                let x = (n as f32 * std::f32::consts::TAU * 220.0 / 48_000.0).sin() * 0.8;
                let (l, r) = dsp.process_stereo(x, -0.6 * x);
                assert!(l.is_finite() && r.is_finite());
                // Once the edited ceiling has glided in.
                if n > 9_600 {
                    assert!(
                        l.abs().max(r.abs()) <= ceiling * 1.000_01,
                        "{os:?} over the ceiling at {n}: {l} {r}"
                    );
                }
            }
            assert!(dsp.gain_reduction_db() > 6.0, "{os:?}");
        }
    }

    /// The oversampled limiter does what the host-rate one does, a
    /// resampler's round trip later: the reduction lines up with the audio.
    #[test]
    fn oversampled_limiting_lines_up_with_the_host_rate_limiter() {
        let tone = |n: usize| (n as f32 * std::f32::consts::TAU * 150.0 / 48_000.0).sin() * 0.7;
        let run = |os: Oversampling| {
            let mut params = default_params();
            params.gain_db = 9.0;
            params.true_peak = false;
            params.oversampling = os;
            let mut dsp = Dsp::new(48_000.0);
            dsp.set_params(params);
            (0..24_000)
                .map(|n| dsp.process_stereo(tone(n), tone(n)).0)
                .collect::<Vec<f32>>()
        };
        let base = run(Oversampling::Off);
        let over = run(Oversampling::X4);
        let shift = TAPS_PER_PHASE;
        let worst = (12_000..24_000 - shift)
            .map(|n| (base[n] - over[n + shift]).abs())
            .fold(0.0f32, f32::max);
        assert!(
            worst < 0.03,
            "oversampled vs host-rate limiting differ by {worst}"
        );
    }

    #[test]
    fn switching_oversampling_fades_and_keeps_the_latency_it_reports() {
        let mut dsp = limiting();
        for _ in 0..4_800 {
            dsp.process_stereo(0.3, 0.3);
        }
        assert!(dsp.apply_ui_param("oversampling", Oversampling::X8.to_wire()));
        let latency = dsp.latency_samples();
        assert_eq!(latency, dsp.lookahead_samples() + TAPS_PER_PHASE);
        let mut quietest = f32::MAX;
        for n in 0..9_600 {
            let x = (n as f32 * 0.05).sin() * 0.9;
            let (l, r) = dsp.process_stereo(x, x);
            assert!(l.is_finite() && r.is_finite() && l.abs() <= 1.0 && r.abs() <= 1.0);
            if n < 960 {
                quietest = quietest.min(dsp.switch_fade.pos);
            }
            assert_eq!(dsp.latency_samples(), latency);
        }
        assert_eq!(quietest, 0.0, "the switch fades the output out");
        assert_eq!(dsp.active_oversampling, Oversampling::X8);
        assert_eq!(dsp.switch_fade.pos, 1.0);
    }

    #[test]
    fn the_new_styles_hold_the_ceiling_and_release_at_their_own_rate() {
        for style in [Style::Loud, Style::Safe] {
            let mut params = default_params();
            params.style = style;
            params.gain_db = 24.0;
            params.ceiling_db = -3.0;
            params.lookahead_ms = 0.0;
            let mut dsp = Dsp::new(48_000.0);
            dsp.set_params(params);
            let ceiling = db_to_linear(-3.0);
            for index in 0..512 {
                let sample = if index % 31 == 0 { 1.0 } else { -0.91 };
                let (l, r) = dsp.process_stereo(sample, -sample);
                assert!(l.abs().max(r.abs()) <= ceiling * 1.000_01, "{style:?}");
            }
        }
        let release = |style: Style| {
            let mut dsp = Dsp::new(48_000.0);
            let mut params = default_params();
            params.style = style;
            dsp.set_params(params);
            dsp.core.release_coeff
        };
        assert!(release(Style::Loud) < release(Style::Modern));
        assert!(release(Style::Safe) > release(Style::Modern));
    }

    #[test]
    fn the_knee_control_softens_the_curve() {
        let mut params = default_params();
        params.gain_db = 0.0;
        params.ceiling_db = -6.0;
        let hard = transfer_db(&params, -6.5);
        params.knee_db = 8.0;
        let soft = transfer_db(&params, -6.5);
        assert!(soft < hard - 0.2, "knee {hard} -> {soft}");
    }

    #[test]
    fn telemetry_reads_the_output_loudness_and_resets_on_request() {
        let mut params = default_params();
        params.lookahead_ms = 0.0;
        params.true_peak = false;
        let mut dsp = Dsp::new(48_000.0);
        dsp.set_params(params);
        let before = dsp.telemetry();
        assert_eq!(before[telemetry::INTEGRATED_LUFS], NO_READING);
        assert_eq!(before[telemetry::VALID], 0.0);
        let amplitude = db_to_linear(-20.0);
        for n in 0..48_000 * 5 {
            let x = (n as f32 * std::f32::consts::TAU * 1_000.0 / 48_000.0).sin() * amplitude;
            dsp.process_stereo(x, x);
        }
        let block = dsp.telemetry();
        for slot in [
            telemetry::MOMENTARY_LUFS,
            telemetry::SHORT_LUFS,
            telemetry::INTEGRATED_LUFS,
        ] {
            assert!(
                (block[slot] + 20.0).abs() < 0.3,
                "slot {slot}: {}",
                block[slot]
            );
        }
        assert_eq!(block[telemetry::VALID], 1.0);
        assert!(block[telemetry::LRA_LU] < 0.5);
        assert!((block[telemetry::TRUE_PEAK_DBTP] + 20.0).abs() < 0.2);
        assert!((block[telemetry::OUT_L] - amplitude).abs() < 1.0e-3);
        assert!((block[telemetry::IN_R] - amplitude).abs() < 1.0e-3);
        assert_eq!(block[telemetry::GR_DB], 0.0);

        assert!(dsp.apply_ui_param("meterReset", 1.0));
        let reset = dsp.telemetry();
        assert_eq!(reset[telemetry::INTEGRATED_LUFS], NO_READING);
        assert_eq!(reset[telemetry::TRUE_PEAK_DBTP], NO_READING);
        assert_eq!(reset[telemetry::VALID], 0.0);
        // The momentary window runs on.
        assert!((reset[telemetry::MOMENTARY_LUFS] + 20.0).abs() < 0.3);
    }

    /// A state saved before the new controls existed plays as a fresh
    /// instance given the same original values: every new control neutral.
    #[test]
    fn an_old_state_plays_as_its_values_did() {
        let json = r#"{"version":1,"params":{"power":true,"style":"punch","gainDb":12.0,"ceilingDb":-1.0,"releaseMs":90.0,"lookaheadMs":3.0,"truePeak":true,"mix":100.0,"stereoLink":false}}"#;
        let loaded = ipc::BurnLimitState::from_json(json).unwrap().params;
        let mut expected = default_params();
        expected.style = Style::Punch;
        expected.gain_db = 12.0;
        expected.ceiling_db = -1.0;
        expected.release_ms = 90.0;
        expected.lookahead_ms = 3.0;
        expected.stereo_link = false;
        expected.link_pct = 0.0;
        let (mut a, mut b) = (Dsp::new(48_000.0), Dsp::new(48_000.0));
        a.set_params(loaded);
        b.set_params(expected);
        for n in 0..24_000 {
            let (l, r) = tone(n);
            assert_eq!(a.process_stereo(l * 3.0, r), b.process_stereo(l * 3.0, r));
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

//! 67Clipper — a mix/mastering clipper with a peak limiter mode.
//!
//! Signal path, per channel:
//!
//! ```text
//! in ─ DC filter ─ Input gain ─┬─ oversample ─ shaper ─ residual ─ decimate ─┐
//!                              └──────────────── delay ──────────────────────+─ ceiling ─ mix ─ out
//! ```
//!
//! The shaper only ever *removes* something: it is unity (bit for bit) under
//! the knee, and above it peaks bend into the clip level (`min(threshold,
//! ceiling)`) along a quadratic knee that `shape` widens from a hard corner
//! (0 %) to ±50 % of the level (100 %). Oversampling runs only the residual
//! — what the shaper took off — through the anti-alias filters and adds it to
//! the delayed input, so a signal under the knee comes out exactly as it went
//! in at any factor, and what the clipping creates above the host's Nyquist
//! is filtered out instead of folding back.
//!
//! * **Clip** — the shaper alone. No lookahead.
//! * **Hybrid** — a 1 ms lookahead limiter takes everything more than
//!   [`HYBRID_CLIP_DB`] over the knee, the clipper the rest.
//! * **Limit** — a 1 ms lookahead peak limiter holds peaks to the clip level.
//!
//! The ceiling then hard-limits the output's samples (it catches the small
//! overshoot the anti-alias filter puts back), and the dry/wet mix blends in
//! the latency-aligned input.

use builtin_dsp_core::delay::{Smoothed, smoothing_step};
use builtin_dsp_core::{
    ParamDescriptor, PluginCategory, PluginDescriptor, StereoEffect, clamp, db_to_linear,
    linear_to_db, mix, time_constant,
};
use serde::{Deserialize, Serialize};

pub mod ipc;
mod oversample;
pub mod presets;
pub mod ui;

pub use ipc::{UI_PARAM_IDS, ui_param_id, ui_param_index};
pub use oversample::TAPS_PER_PHASE;
pub use presets::{FactoryPreset, factory_presets};

use oversample::{AntiAlias, Lookahead, MAX_FACTOR, Ring};

pub const PLUGIN_ID: &str = "futureboard.clipper67";

/// The knee's half-width at `shape` 100 %, as a fraction of the clip level:
/// the curve leaves unity at `level·(1 − k)` and reaches the level at
/// `level·(1 + k)`.
pub const KNEE_MAX: f32 = 0.5;
/// In Hybrid, how far past the knee's end the clipper works before the
/// limiter takes over.
pub const HYBRID_CLIP_DB: f32 = 3.0;
/// Hybrid and Limit look this far ahead (reported as latency).
pub const LOOKAHEAD_SECONDS: f32 = 0.001;
/// The lookahead in host samples is capped here (1 ms at 384 kHz).
const MAX_LOOKAHEAD: usize = 384;
const LIMIT_RELEASE_SECONDS: f32 = 0.050;
const HYBRID_RELEASE_SECONDS: f32 = 0.150;

const CLIP_THRESHOLD: f32 = 1.0;
const RMS_WINDOW_SECONDS: f32 = 0.300;
const PEAK_FALL_SECONDS: f32 = 0.400;
const REDUCTION_FALL_SECONDS: f32 = 0.200;
const DC_BLOCK_HZ: f32 = 5.0;
/// Input / level / knee / ceiling / mix smoothing, per stage of a [`Glide`],
/// so a dragged knob cannot zipper.
const SMOOTH_MS: f32 = 7.0;
/// Length of the Power / Delta / DC-filter crossfades and of a mode or
/// oversampling switch.
const FADE_MS: f32 = 10.0;

/// Host-rate history: the largest latency plus the interpolator's taps.
const HISTORY: usize = 512;
/// Oversampled residual history: the longest decimator plus one block.
const RESIDUAL_HISTORY: usize = 512;
/// Oversampled lookahead delay.
const LOOKAHEAD_HISTORY: usize = MAX_LOOKAHEAD * MAX_FACTOR + 1;

/// Processing mode. Wire order: Clip, Hybrid, Limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Clip,
    Hybrid,
    Limit,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Clip => "clip",
            Self::Hybrid => "hybrid",
            Self::Limit => "limit",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "clip" => Some(Self::Clip),
            "hybrid" => Some(Self::Hybrid),
            "limit" => Some(Self::Limit),
            _ => None,
        }
    }

    pub const fn to_wire(self) -> f32 {
        match self {
            Self::Clip => 0.0,
            Self::Hybrid => 1.0,
            Self::Limit => 2.0,
        }
    }

    pub fn from_wire(value: f32) -> Self {
        match value.round() as i32 {
            1 => Self::Hybrid,
            2 => Self::Limit,
            _ => Self::Clip,
        }
    }

    /// Whether the mode runs the lookahead limiter.
    pub const fn looks_ahead(self) -> bool {
        !matches!(self, Self::Clip)
    }
}

/// Oversampling factor. Wire order: 1×, 2×, 4×, 8×.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Oversampling {
    #[serde(rename = "1x")]
    X1,
    #[serde(rename = "2x")]
    X2,
    #[serde(rename = "4x")]
    X4,
    #[serde(rename = "8x")]
    X8,
}

impl Oversampling {
    pub const ALL: [Oversampling; 4] = [Self::X1, Self::X2, Self::X4, Self::X8];

    pub const fn factor(self) -> usize {
        match self {
            Self::X1 => 1,
            Self::X2 => 2,
            Self::X4 => 4,
            Self::X8 => 8,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::X1 => "1×",
            Self::X2 => "2×",
            Self::X4 => "4×",
            Self::X8 => "8×",
        }
    }

    pub const fn to_wire(self) -> f32 {
        match self {
            Self::X1 => 0.0,
            Self::X2 => 1.0,
            Self::X4 => 2.0,
            Self::X8 => 3.0,
        }
    }

    pub fn from_wire(value: f32) -> Self {
        match value.round() as i32 {
            i32::MIN..=0 => Self::X1,
            1 => Self::X2,
            2 => Self::X4,
            _ => Self::X8,
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

/// A missing field (a project saved before it existed) takes its default.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Params {
    pub power: bool,
    pub mode: Mode,
    /// The clip level, dBFS. Signal under the knee passes untouched.
    pub threshold_db: f32,
    /// Knee width: 0 % a hard corner, 100 % ±[`KNEE_MAX`] of the level.
    pub shape: f32,
    /// The highest sample the output may carry, dBFS; the clip level never
    /// sits above it.
    pub ceiling_db: f32,
    pub mix: f32,
    pub stereo_link: bool,
    pub dc_filter: bool,
    /// Gain into the clipper, dB: how hard it is pushed, apart from where it
    /// clips.
    pub input_db: f32,
    pub oversampling: Oversampling,
    /// Output only what the clipper takes off, at the input's level.
    pub delta: bool,
}

impl Default for Params {
    fn default() -> Self {
        default_params()
    }
}

pub fn default_params() -> Params {
    Params {
        power: true,
        mode: Mode::Clip,
        threshold_db: 0.0,
        shape: 50.0,
        ceiling_db: -0.3,
        mix: 100.0,
        stereo_link: true,
        dc_filter: true,
        input_db: 0.0,
        oversampling: Oversampling::X4,
        delta: false,
    }
}

/// The level, dBFS, peaks are clipped to: the threshold, never above the
/// ceiling.
pub fn clip_level_db(params: &Params) -> f32 {
    params.threshold_db.min(params.ceiling_db)
}

/// The knee's half-width fraction for `shape` percent.
#[inline]
pub fn knee_of(shape: f32) -> f32 {
    clamp(shape, 0.0, 100.0) / 100.0 * KNEE_MAX
}

/// The clipper's static curve: `x` unchanged up to `level·(1 − knee)`, then a
/// quadratic bend — continuous in value and slope — that meets `level` flat
/// at `level·(1 + knee)` and holds it beyond. Odd-symmetric.
#[inline]
pub fn clip_curve(x: f32, level: f32, knee: f32) -> f32 {
    let a = x.abs();
    let start = level * (1.0 - knee);
    if a <= start {
        return x;
    }
    let y = if knee <= 0.0 || a >= level * (1.0 + knee) {
        level
    } else {
        let u = a - start;
        a - u * u / (4.0 * level * knee)
    };
    y.copysign(x)
}

/// The host samples Hybrid and Limit look ahead at `sample_rate`.
pub fn lookahead_samples(sample_rate: f32) -> usize {
    ((sample_rate.max(1.0) * LOOKAHEAD_SECONDS).round() as usize).clamp(1, MAX_LOOKAHEAD)
}

/// The delay, in host samples, `params` run with at `sample_rate`: the
/// oversampling filters' [`TAPS_PER_PHASE`] when oversampling, plus the
/// lookahead in Hybrid and Limit. Power does not change it (bypass stays
/// aligned).
pub fn latency_for(params: &Params, sample_rate: f32) -> usize {
    let filters = if params.oversampling.factor() > 1 {
        TAPS_PER_PHASE
    } else {
        0
    };
    let lookahead = if params.mode.looks_ahead() {
        lookahead_samples(sample_rate)
    } else {
        0
    };
    filters + lookahead
}

/// The steady-state output peak, in dBFS, of a peak held at `input_db`: the
/// input gain, the mode's clip or limit (each settles to the same static
/// curve), the ceiling, then the dry blend. Bypassed — or listening to the
/// delta — the curve is the plain output path's.
pub fn transfer_db(params: &Params, input_db: f32) -> f32 {
    if !params.power {
        return input_db;
    }
    let dry = db_to_linear(input_db);
    let driven = dry * db_to_linear(params.input_db);
    let level = db_to_linear(clip_level_db(params));
    let knee = knee_of(params.shape);
    let wet = match params.mode {
        Mode::Clip => clip_curve(driven, level, knee),
        Mode::Hybrid => clip_curve(driven.min(hybrid_target(level, knee)), level, knee),
        Mode::Limit => driven.min(level),
    };
    let wet = wet.min(db_to_linear(params.ceiling_db));
    let amount = clamp(params.mix, 0.0, 100.0) / 100.0;
    linear_to_db(mix(dry, wet, amount).max(1.0e-9))
}

/// Hybrid's limiter target: [`HYBRID_CLIP_DB`] past the knee's end.
fn hybrid_target(level: f32, knee: f32) -> f32 {
    level * (1.0 + knee) * db_to_linear(HYBRID_CLIP_DB)
}

pub fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PLUGIN_ID,
        name: "67Clipper",
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
                id: "mode",
                name: "Mode",
                default_value: 0.0,
                min: 0.0,
                max: 2.0,
                unit: "enum",
            },
            ParamDescriptor {
                id: "thresholdDb",
                name: "Threshold",
                default_value: 0.0,
                min: -24.0,
                max: 0.0,
                unit: "dB",
            },
            ParamDescriptor {
                id: "shape",
                name: "Shape",
                default_value: 50.0,
                min: 0.0,
                max: 100.0,
                unit: "%",
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
                id: "mix",
                name: "Mix",
                default_value: 100.0,
                min: 0.0,
                max: 100.0,
                unit: "%",
            },
            ParamDescriptor {
                id: "stereoLink",
                name: "Stereo Link",
                default_value: 1.0,
                min: 0.0,
                max: 1.0,
                unit: "bool",
            },
            ParamDescriptor {
                id: "dcFilter",
                name: "DC Filter",
                default_value: 1.0,
                min: 0.0,
                max: 1.0,
                unit: "bool",
            },
            ParamDescriptor {
                id: "inputDb",
                name: "Input",
                default_value: 0.0,
                min: -12.0,
                max: 24.0,
                unit: "dB",
            },
            ParamDescriptor {
                id: "oversampling",
                name: "Oversampling",
                default_value: 2.0,
                min: 0.0,
                max: 3.0,
                unit: "enum",
            },
            ParamDescriptor {
                id: "delta",
                name: "Delta",
                default_value: 0.0,
                min: 0.0,
                max: 1.0,
                unit: "bool",
            },
        ],
    }
}

/// One-pole DC blocker (~5 Hz). `y = x - x1 + r·y1`.
#[derive(Debug, Clone)]
struct DcBlock {
    r: f32,
    x1_l: f32,
    y1_l: f32,
    x1_r: f32,
    y1_r: f32,
}

impl DcBlock {
    fn new(sample_rate: f32) -> Self {
        let mut block = Self {
            r: 0.0,
            x1_l: 0.0,
            y1_l: 0.0,
            x1_r: 0.0,
            y1_r: 0.0,
        };
        block.set_sample_rate(sample_rate);
        block
    }

    fn set_sample_rate(&mut self, sample_rate: f32) {
        let sr = sample_rate.max(1.0);
        self.r = (1.0 - (2.0 * std::f32::consts::PI * DC_BLOCK_HZ) / sr).clamp(0.9, 0.999_99);
    }

    fn reset(&mut self) {
        self.x1_l = 0.0;
        self.y1_l = 0.0;
        self.x1_r = 0.0;
        self.y1_r = 0.0;
    }

    #[inline]
    fn run(&mut self, left: f32, right: f32) -> (f32, f32) {
        let yl = builtin_dsp_core::flush_denormal(left - self.x1_l + self.r * self.y1_l);
        let yr = builtin_dsp_core::flush_denormal(right - self.x1_r + self.r * self.y1_r);
        self.x1_l = left;
        self.y1_l = yl;
        self.x1_r = right;
        self.y1_r = yr;
        (yl, yr)
    }
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
    fn push(&mut self, input: f32, output: f32) {
        let in_abs = input.abs();
        let out_abs = output.abs();

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

        self.in_ms = self.rms_coeff * self.in_ms + (1.0 - self.rms_coeff) * input * input;
        self.out_ms = self.rms_coeff * self.out_ms + (1.0 - self.rms_coeff) * output * output;

        if in_abs >= CLIP_THRESHOLD {
            self.in_clip = true;
        }
        if out_abs >= CLIP_THRESHOLD {
            self.out_clip = true;
        }
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

/// What a change of these settings has to rebuild: the filters in use, the
/// lookahead and the reported latency.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Layout {
    factor: usize,
    mode: Mode,
    sample_rate: f32,
}

impl Layout {
    fn of(params: &Params, sample_rate: f32) -> Self {
        Self {
            factor: params.oversampling.factor(),
            mode: params.mode,
            sample_rate,
        }
    }
}

/// The curve every path shapes against this sample: the clip level and knee
/// (gliding), and whether they are moving.
#[derive(Debug, Clone, Copy)]
struct Curve {
    level: f32,
    knee: f32,
    knee_start: f32,
    hybrid_target: f32,
    stereo_link: bool,
    /// The level, knee or drive is gliding: the quiet shortcut, which
    /// compares the history against the knee, cannot be trusted.
    moving: bool,
}

/// The shaper for one layout: its oversampled residual, lookahead and
/// limiter state. Two exist so a mode or oversampling change can run the
/// new layout beside the old one and crossfade, instead of stepping.
#[derive(Debug, Clone)]
struct Path {
    layout: Layout,
    filter: usize,
    factor: usize,
    lookahead_top: usize,
    latency: usize,
    // Oversampled residual, for the decimator.
    residual_l: Ring,
    residual_r: Ring,
    // Oversampled lookahead delay and gain computers.
    ahead_l: Ring,
    ahead_r: Ring,
    limiter_l: Lookahead,
    limiter_r: Lookahead,
    top_time: u64,
    last_residual: u64,
    quiet_run: usize,
    /// The path has not run since its state was last current (built, set to
    /// a new layout, or idle while bypassed): it is primed from the host-rate
    /// history before it next runs.
    stale: bool,
}

impl Path {
    fn new(layout: Layout) -> Self {
        let mut path = Self {
            layout,
            filter: 0,
            factor: 1,
            lookahead_top: 0,
            latency: 0,
            residual_l: Ring::new(RESIDUAL_HISTORY),
            residual_r: Ring::new(RESIDUAL_HISTORY),
            ahead_l: Ring::new(LOOKAHEAD_HISTORY),
            ahead_r: Ring::new(LOOKAHEAD_HISTORY),
            limiter_l: Lookahead::new(LOOKAHEAD_HISTORY),
            limiter_r: Lookahead::new(LOOKAHEAD_HISTORY),
            top_time: 0,
            last_residual: 0,
            quiet_run: 0,
            stale: true,
        };
        path.configure(layout);
        // Write every buffer now, so the first switch onto this path does
        // not fault fresh pages in on the audio thread.
        path.clear();
        path
    }

    /// Point at `layout`'s filter and lookahead. Allocation-free; the state
    /// is cleared when the path is next primed.
    fn configure(&mut self, layout: Layout) {
        self.layout = layout;
        self.factor = layout.factor;
        self.filter = match layout.factor {
            2 => 0,
            4 => 1,
            _ => 2,
        };
        let lookahead = if layout.mode.looks_ahead() {
            lookahead_samples(layout.sample_rate)
        } else {
            0
        };
        self.lookahead_top = lookahead * self.factor;
        let filters = if self.factor > 1 { TAPS_PER_PHASE } else { 0 };
        self.latency = filters + lookahead;
        let top_rate = layout.sample_rate * self.factor as f32;
        let release = time_constant(
            top_rate,
            if layout.mode == Mode::Hybrid {
                HYBRID_RELEASE_SECONDS
            } else {
                LIMIT_RELEASE_SECONDS
            },
        );
        self.limiter_l.configure(self.lookahead_top + 1, release);
        self.limiter_r.configure(self.lookahead_top + 1, release);
        self.stale = true;
    }

    /// Clear what the shaper holds between samples.
    fn clear(&mut self) {
        self.residual_l.clear();
        self.residual_r.clear();
        self.ahead_l.clear();
        self.ahead_r.clear();
        self.limiter_l.reset();
        self.limiter_r.reset();
        self.top_time = 0;
        self.last_residual = 0;
        self.quiet_run = 0;
    }

    /// Start over from the host-rate history: clear, then replay the driven
    /// input that fills the lookahead and the decimator without output, so
    /// the path's first sample is what it would have been had it been
    /// running all along. Called on the audio thread at most once per switch
    /// or power-on; bounded (≤ the lookahead plus the filter span).
    fn prime(&mut self, filters: &[AntiAlias; 3], drive: (&Ring, &Ring), curve: &Curve) {
        self.clear();
        let lookahead = self.lookahead_top / self.factor;
        if self.factor == 1 {
            if self.layout.mode.looks_ahead() {
                for offset in (1..=lookahead).rev() {
                    let _ = self.shape(drive.0.at(offset), drive.1.at(offset), curve);
                }
            }
        } else {
            for offset in (1..=lookahead + TAPS_PER_PHASE + 2).rev() {
                self.advance(filters, drive, offset, curve);
            }
        }
        self.stale = false;
    }

    /// The gain a peak of `peak` needs to sit at `target`.
    #[inline]
    fn needed(peak: f32, target: f32) -> f32 {
        if peak > target { target / peak } else { 1.0 }
    }

    /// One (oversampled) sample pair through the mode's shaper. Returns the
    /// shaped pair and the input pair it lines up with (the lookahead delays
    /// both).
    #[inline]
    fn shape(&mut self, l: f32, r: f32, curve: &Curve) -> (f32, f32, f32, f32) {
        let (level, knee) = (curve.level, curve.knee);
        let mode = self.layout.mode;
        if mode == Mode::Clip {
            return (clip_curve(l, level, knee), clip_curve(r, level, knee), l, r);
        }
        self.ahead_l.push(l);
        self.ahead_r.push(r);
        let al = self.ahead_l.at(self.lookahead_top);
        let ar = self.ahead_r.at(self.lookahead_top);
        let target = if mode == Mode::Limit {
            level
        } else {
            curve.hybrid_target
        };
        let (gl, gr) = if curve.stereo_link {
            let g = self
                .limiter_l
                .next(Self::needed(l.abs().max(r.abs()), target));
            (g, g)
        } else {
            (
                self.limiter_l.next(Self::needed(l.abs(), target)),
                self.limiter_r.next(Self::needed(r.abs(), target)),
            )
        };
        let (yl, yr) = (al * gl, ar * gr);
        if mode == Mode::Limit {
            (yl.clamp(-level, level), yr.clamp(-level, level), al, ar)
        } else {
            (
                clip_curve(yl, level, knee),
                clip_curve(yr, level, knee),
                al,
                ar,
            )
        }
    }

    /// Oversample the driven input `offset` host samples ago through the
    /// shaper and push what it took off onto the residual history.
    #[inline]
    fn advance(
        &mut self,
        filters: &[AntiAlias; 3],
        drive: (&Ring, &Ring),
        offset: usize,
        curve: &Curve,
    ) {
        let factor = self.factor;
        let taps = TAPS_PER_PHASE + 1;
        let filter = &filters[self.filter];
        // Clip mode with nothing near the knee across the interpolator's
        // reach cannot put a residual out: skip the filter work.
        let (dl, dr) = (drive.0.at(offset), drive.1.at(offset));
        if !curve.moving && dl.abs().max(dr.abs()) * filter.peak_bound <= curve.knee_start {
            self.quiet_run = self.quiet_run.saturating_add(1);
        } else {
            self.quiet_run = 0;
        }
        let quiet = self.layout.mode == Mode::Clip && self.quiet_run > taps;
        for phase in 0..factor {
            let (el, er) = if quiet {
                (0.0, 0.0)
            } else {
                let ul = filter.interpolate(drive.0.recent_from(offset, taps), phase);
                let ur = filter.interpolate(drive.1.recent_from(offset, taps), phase);
                let (yl, yr, al, ar) = self.shape(ul, ur, curve);
                (yl - al, yr - ar)
            };
            self.residual_l.push(el);
            self.residual_r.push(er);
            self.top_time += 1;
            if el != 0.0 || er != 0.0 {
                self.last_residual = self.top_time;
            }
        }
    }

    /// The wet pair for the newest driven input, aligned with the driven
    /// input `latency` samples ago (`xl`, `xr`).
    #[inline]
    fn wet(
        &mut self,
        filters: &[AntiAlias; 3],
        drive: (&Ring, &Ring),
        xl: f32,
        xr: f32,
        curve: &Curve,
    ) -> (f32, f32) {
        if self.factor == 1 {
            let (yl, yr, _, _) = self.shape(drive.0.at(0), drive.1.at(0), curve);
            return (yl, yr);
        }
        self.advance(filters, drive, 0, curve);
        // The decimator's newest tap is this step's first phase, which lines
        // the residual up with the driven input `latency` samples ago.
        let factor = self.factor;
        let filter = &filters[self.filter];
        let len = filter.taps.len();
        if self.top_time.saturating_sub(self.last_residual) >= (len + factor) as u64 {
            return (xl, xr);
        }
        let rl = filter.decimate(&self.residual_l.recent(factor - 1 + len)[factor - 1..]);
        let rr = filter.decimate(&self.residual_r.recent(factor - 1 + len)[factor - 1..]);
        // Adding a zero residual would still turn −0 into +0.
        let add = |x: f32, residual: f32| if residual == 0.0 { x } else { x + residual };
        (add(xl, rl), add(xr, rr))
    }
}

/// One path's output for this sample.
#[derive(Debug, Clone, Copy)]
struct PathOut {
    out: (f32, f32),
    /// Driven input and wet output, for the reduction meter.
    driven: (f32, f32),
    wet: (f32, f32),
}

#[derive(Debug, Clone)]
pub struct Dsp {
    params: Params,
    /// Audio has run since construction or [`StereoEffect::reset`]. Until
    /// it has, an edit lands at once — an insert or session being set up
    /// replays its stored values, and there is nothing yet to glide over.
    started: bool,
    sample_rate: f32,
    dc_block: DcBlock,
    meters: Meters,

    // Resolved from the params; the continuous ones glide.
    drive: Glide,
    inv_drive: f32,
    level: Glide,
    knee: Glide,
    ceiling: Glide,
    mix_amount: Glide,
    smooth_step: f32,
    curve: Curve,
    /// Dry ↔ processed.
    power: Fade,
    /// Clipped ↔ what was clipped off.
    delta: Fade,
    /// Raw ↔ DC-filtered input.
    dc: Fade,

    // Layout: the active path, and the one it is crossfading from.
    filters: [AntiAlias; 3],
    paths: [Path; 2],
    active: usize,
    /// The layout the params ask for; the active path moves to it.
    pending: Layout,
    /// Old → active path; settled (1) when no switch is running.
    switch: Fade,
    latency: usize,

    // Host-rate history: the raw input (dry path) and the driven input
    // (interpolator taps and the delayed path the residual is added to).
    dry_l: Ring,
    dry_r: Ring,
    drive_l: Ring,
    drive_r: Ring,

    reduction_gain: f32,
    reduction_fall: f32,
}

impl Dsp {
    pub fn new(sample_rate: f32) -> Self {
        let sr = sample_rate.max(1.0);
        let params = default_params();
        let layout = Layout::of(&params, sr);
        let mut dsp = Self {
            started: false,
            sample_rate: sr,
            dc_block: DcBlock::new(sr),
            meters: Meters::new(sr),
            drive: Glide::at(1.0),
            inv_drive: 1.0,
            level: Glide::at(1.0),
            knee: Glide::at(0.0),
            ceiling: Glide::at(1.0),
            mix_amount: Glide::at(1.0),
            smooth_step: smoothing_step(SMOOTH_MS, sr),
            curve: Curve {
                level: 1.0,
                knee: 0.0,
                knee_start: 1.0,
                hybrid_target: 1.0,
                stereo_link: true,
                moving: false,
            },
            power: Fade::new(true, sr),
            delta: Fade::new(false, sr),
            dc: Fade::new(true, sr),
            filters: [AntiAlias::new(2), AntiAlias::new(4), AntiAlias::new(8)],
            paths: [Path::new(layout), Path::new(layout)],
            active: 0,
            pending: layout,
            switch: Fade::new(true, sr),
            latency: 0,
            dry_l: Ring::new(HISTORY),
            dry_r: Ring::new(HISTORY),
            drive_l: Ring::new(HISTORY),
            drive_r: Ring::new(HISTORY),
            reduction_gain: 1.0,
            reduction_fall: time_constant(sr, REDUCTION_FALL_SECONDS),
            params,
        };
        dsp.apply_params();
        dsp.settle();
        dsp
    }

    pub fn params(&self) -> &Params {
        &self.params
    }

    /// Replace every parameter at once — a state load. Lands on the new
    /// values (and layout) without a glide or a crossfade.
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
                (-linear_to_db(self.reduction_gain)).max(0.0)
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

    /// The delay the output runs behind the input, in host samples (see
    /// [`latency_for`]). After a mode or oversampling change it is the new
    /// layout's, while the audio crossfades over to it.
    pub fn latency_samples(&self) -> usize {
        self.latency
    }

    /// Retarget the glides and fades and note the layout. Allocation-free and
    /// cheap: a layout change is picked up by the audio path, which builds
    /// the new layout on the idle path and crossfades.
    fn apply_params(&mut self) {
        let p = &self.params;
        self.drive.set(db_to_linear(p.input_db));
        self.level.set(db_to_linear(clip_level_db(p)));
        self.knee.set(knee_of(p.shape));
        self.ceiling.set(db_to_linear(p.ceiling_db));
        self.mix_amount.set(clamp(p.mix, 0.0, 100.0) / 100.0);
        self.curve.stereo_link = p.stereo_link;
        self.power.set(p.power);
        self.delta.set(p.delta);
        self.dc.set(p.dc_filter);
        self.pending = Layout::of(p, self.sample_rate);
        self.latency = latency_for(p, self.sample_rate);
    }

    /// Land every glide and fade, and put the active path straight onto the
    /// pending layout.
    fn settle(&mut self) {
        self.drive.settle();
        self.level.settle();
        self.knee.settle();
        self.ceiling.settle();
        self.mix_amount.settle();
        self.power.settle();
        self.delta.settle();
        self.dc.settle();
        self.resolve_curve(false);
        self.switch.settle();
        let active = &mut self.paths[self.active];
        if active.layout != self.pending {
            active.configure(self.pending);
        }
    }

    /// The curve (and the drive's inverse) from the glides' current values.
    #[inline]
    fn resolve_curve(&mut self, moving: bool) {
        let (level, knee) = (self.level.outer.value, self.knee.outer.value);
        self.curve.level = level;
        self.curve.knee = knee;
        self.curve.knee_start = level * (1.0 - knee);
        self.curve.hybrid_target = hybrid_target(level, knee);
        self.curve.moving = moving;
        self.inv_drive = 1.0 / self.drive.outer.value;
    }

    fn set_sample_rate_internal(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        self.dc_block.set_sample_rate(self.sample_rate);
        self.meters = Meters::new(self.sample_rate);
        self.reduction_fall = time_constant(self.sample_rate, REDUCTION_FALL_SECONDS);
        self.smooth_step = smoothing_step(SMOOTH_MS, self.sample_rate);
        let step = fade_step(self.sample_rate);
        self.power.step = step;
        self.delta.step = step;
        self.dc.step = step;
        self.switch.step = step;
    }

    /// One path's output for the newest sample: its wet signal at its own
    /// latency, the ceiling, the delta and mix blends, and Power's fade
    /// against its latency-aligned dry. Faded out, the path does not run.
    #[inline]
    fn run_path(&mut self, index: usize, on: f32, delta: f32) -> PathOut {
        let path = &mut self.paths[index];
        let latency = path.latency;
        let dry = (self.dry_l.at(latency), self.dry_r.at(latency));
        let (xl, xr) = (self.drive_l.at(latency), self.drive_r.at(latency));
        if on <= 0.0 {
            path.stale = true;
            return PathOut {
                out: dry,
                driven: (xl, xr),
                wet: (xl, xr),
            };
        }
        let drive = (&self.drive_l, &self.drive_r);
        if path.stale {
            path.prime(&self.filters, drive, &self.curve);
        }
        let (wl, wr) = path.wet(&self.filters, drive, xl, xr, &self.curve);
        let ceiling = self.ceiling.outer.value;
        let (wl, wr) = (wl.clamp(-ceiling, ceiling), wr.clamp(-ceiling, ceiling));
        let amount = self.mix_amount.outer.value;
        let mut out = (mix(dry.0, wl, amount), mix(dry.1, wr, amount));
        if delta > 0.0 {
            let scale = self.inv_drive * amount;
            out = (
                blend(out.0, (xl - wl) * scale, delta),
                blend(out.1, (xr - wr) * scale, delta),
            );
        }
        PathOut {
            out: (blend(dry.0, out.0, on), blend(dry.1, out.1, on)),
            driven: (xl, xr),
            wet: (wl, wr),
        }
    }

    #[inline]
    fn track_reduction(&mut self, xl: f32, xr: f32, wl: f32, wr: f32) {
        let ratio = |x: f32, w: f32| {
            if x.abs() > 1.0e-6 {
                (w.abs() / x.abs()).min(1.0)
            } else {
                1.0
            }
        };
        let gain = ratio(xl, wl).min(ratio(xr, wr));
        self.reduction_gain = if gain < self.reduction_gain {
            gain
        } else {
            1.0 - (1.0 - self.reduction_gain) * self.reduction_fall
        };
    }
}

impl StereoEffect for Dsp {
    fn reset(&mut self) {
        self.started = false;
        self.dc_block.reset();
        self.meters.reset();
        self.dry_l.clear();
        self.dry_r.clear();
        self.drive_l.clear();
        self.drive_r.clear();
        for path in &mut self.paths {
            path.stale = true;
        }
        self.settle();
        self.reduction_gain = 1.0;
    }

    fn set_sample_rate(&mut self, sample_rate: f32) {
        self.set_sample_rate_internal(sample_rate);
        self.apply_params();
        self.settle();
    }

    fn process_stereo(&mut self, left: f32, right: f32) -> (f32, f32) {
        self.started = true;
        let in_sum = (left + right) * 0.5;

        // The glides move first, so the drive below is this sample's.
        if self.drive.moving() || self.level.moving() || self.knee.moving() {
            let step = self.smooth_step;
            self.drive.next(step);
            self.level.next(step);
            self.knee.next(step);
            self.resolve_curve(true);
        } else if self.curve.moving {
            self.curve.moving = false;
        }
        self.ceiling.next(self.smooth_step);
        self.mix_amount.next(self.smooth_step);

        // The history runs in bypass too, so power and layout changes find
        // the real past input. The DC filter runs always and is faded in and
        // out, so switching it cannot step.
        self.dry_l.push(left);
        self.dry_r.push(right);
        let (fl, fr) = self.dc_block.run(left, right);
        let dc = self.dc.next();
        let (pl, pr) = (blend(left, fl, dc), blend(right, fr, dc));
        let drive = self.drive.outer.value;
        self.drive_l.push(pl * drive);
        self.drive_r.push(pr * drive);

        let on = self.power.next();
        let delta = self.delta.next();

        // A layout change starts on the idle path once the last one is done.
        if self.switch.pos >= 1.0 && self.paths[self.active].layout != self.pending {
            self.active = 1 - self.active;
            self.paths[self.active].configure(self.pending);
            self.switch.pos = 0.0;
        }
        let new = self.run_path(self.active, on, delta);
        let out = if self.switch.pos < 1.0 {
            let weight = self.switch.next();
            let old = self.run_path(1 - self.active, on, delta);
            (
                blend(old.out.0, new.out.0, weight),
                blend(old.out.1, new.out.1, weight),
            )
        } else {
            new.out
        };

        if on > 0.0 {
            self.track_reduction(new.driven.0, new.driven.1, new.wet.0, new.wet.1);
        }
        self.meters.push(in_sum, (out.0 + out.1) * 0.5);
        out
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

    /// The sweep's tone pushed into the clipper, at 1× (no latency, so a
    /// step lands on the block edge) and at the default 4×.
    fn hot(oversampling: Oversampling) -> Dsp {
        let mut params = default_params();
        params.input_db = 9.0;
        params.threshold_db = -3.0;
        params.oversampling = oversampling;
        dsp_with(params)
    }

    /// Dragged knobs, each from settings that bend the tone through the
    /// knee across the whole drag but never clip it flat: a flat-topped
    /// block has no curvature of its own to measure the edge against.
    #[test]
    fn dragged_gains_levels_and_mix_do_not_zipper() {
        // (param, from, to, input dB, threshold dB)
        for (index, min, max, input_db, threshold_db) in [
            (ipc::INPUT_INDEX, -12.0, 9.0, 0.0, 0.0),
            (ipc::THRESHOLD_INDEX, -10.0, 0.0, 0.0, 0.0),
            (ipc::CEILING_INDEX, -6.0, 0.0, 0.0, 0.0),
            (ipc::SHAPE_INDEX, 0.0, 100.0, 0.0, -6.0),
            (ipc::MIX_INDEX, 0.0, 100.0, 3.0, -3.0),
        ] {
            for oversampling in [Oversampling::X1, Oversampling::X4] {
                let mut params = default_params();
                params.shape = 100.0;
                params.input_db = input_db;
                params.threshold_db = threshold_db;
                params.oversampling = oversampling;
                let mut dsp = dsp_with(params);
                let jump = edge_jump(&mut dsp, |dsp, block| {
                    dsp.apply_wire_param(index, drag(min, max, block));
                });
                assert!(
                    jump < 4.0,
                    "{} at {oversampling:?}: block-edge jump {jump}",
                    UI_PARAM_IDS[index as usize]
                );
            }
        }
    }

    #[test]
    fn switches_crossfade_instead_of_stepping() {
        for (index, steps) in [
            (ipc::POWER_INDEX, 2),
            (ipc::MODE_INDEX, 3),
            (ipc::OVERSAMPLING_INDEX, 4),
            (ipc::DELTA_INDEX, 2),
            (ipc::DC_FILTER_INDEX, 2),
        ] {
            for oversampling in [Oversampling::X1, Oversampling::X4] {
                let mut dsp = hot(oversampling);
                let jump = edge_jump(&mut dsp, |dsp, block| click(dsp, index, steps, block));
                assert!(
                    jump < 4.0,
                    "{} from {oversampling:?}: block-edge jump {jump}",
                    UI_PARAM_IDS[index as usize]
                );
            }
        }
    }

    /// A mode or oversampling change reports the new latency at once and,
    /// once its crossfade is over, plays exactly what a fresh instance at
    /// those settings plays.
    #[test]
    fn a_layout_switch_lands_on_the_new_layout() {
        let mut switched = hot(Oversampling::X1);
        let mut params = switched.params().clone();
        let input = |n: usize| sine(1_000.0, 0.8, n);
        for n in 0..4_800 {
            switched.process_stereo(input(n), input(n));
        }
        params.mode = Mode::Limit;
        params.oversampling = Oversampling::X4;
        assert!(switched.apply_ui_param("mode", Mode::Limit.to_wire()));
        assert!(switched.apply_ui_param("oversampling", Oversampling::X4.to_wire()));
        assert_eq!(switched.latency_samples(), latency_for(&params, RATE));
        let mut fresh = dsp_with(params);
        for n in 0..4_800 {
            fresh.process_stereo(input(n), input(n));
        }
        for n in 4_800..24_000 {
            let a = switched.process_stereo(input(n), input(n));
            let b = fresh.process_stereo(input(n), input(n));
            // The lookahead limiter's held gain, started over from the
            // history rather than run all along, settles within a hair
            // (about −76 dB here) of a fresh instance's.
            if n > 4_800 + 4_800 {
                assert!(
                    (a.0 - b.0).abs() < 1.0e-3 && (a.1 - b.1).abs() < 1.0e-3,
                    "{n}: {a:?} vs {b:?}"
                );
            }
        }
    }

    #[test]
    fn a_state_load_lands_without_a_crossfade() {
        let mut dsp = Dsp::new(RATE);
        let mut params = default_params();
        params.mode = Mode::Limit;
        params.oversampling = Oversampling::X1;
        params.mix = 40.0;
        dsp.set_params(params);
        assert_eq!(dsp.paths[dsp.active].layout.mode, Mode::Limit);
        assert_eq!(dsp.switch.pos, 1.0);
        assert_eq!(dsp.mix_amount.outer.value, 0.4);
    }

    const RATE: f32 = 48_000.0;
    const ALL_MODES: [Mode; 3] = [Mode::Clip, Mode::Hybrid, Mode::Limit];

    fn dsp_with(params: Params) -> Dsp {
        let mut dsp = Dsp::new(RATE);
        dsp.set_params(params);
        dsp
    }

    fn sine(freq: f32, amplitude: f32, n: usize) -> f32 {
        (std::f32::consts::TAU * freq * n as f32 / RATE).sin() * amplitude
    }

    #[test]
    fn descriptor_ids_are_unique_and_match_defaults() {
        let d = descriptor();
        assert_eq!(d.id, PLUGIN_ID);
        assert_eq!(d.name, "67Clipper");
        assert_eq!(d.category, PluginCategory::Effect);

        let mut ids: Vec<_> = d.params.iter().map(|p| p.id).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(count, ids.len(), "duplicate parameter id in descriptor");
        assert_eq!(count, ipc::PARAM_COUNT);

        let defaults = ipc::ui_values(&default_params());
        for param in d.params {
            let (_, actual) = defaults
                .iter()
                .find(|(id, _)| *id == param.id)
                .copied()
                .unwrap_or_else(|| panic!("`{}` is missing from ui_values", param.id));
            assert!(
                (actual - param.default_value).abs() < 1.0e-5,
                "{} default drifted: descriptor={} ui_values={}",
                param.id,
                param.default_value,
                actual
            );
        }
    }

    #[test]
    fn the_knee_is_continuous_in_value_and_slope() {
        let level = 0.5;
        for knee in [0.05, 0.25, KNEE_MAX] {
            let start = level * (1.0 - knee);
            let end = level * (1.0 + knee);
            let eps = 1.0e-4;
            for edge in [start, end] {
                let below = clip_curve(edge - eps, level, knee);
                let above = clip_curve(edge + eps, level, knee);
                assert!((above - below).abs() < 3.0 * eps, "value jumps at {edge}");
                let slope_below = (clip_curve(edge - eps, level, knee)
                    - clip_curve(edge - 2.0 * eps, level, knee))
                    / eps;
                let slope_above = (clip_curve(edge + 2.0 * eps, level, knee)
                    - clip_curve(edge + eps, level, knee))
                    / eps;
                assert!(
                    (slope_above - slope_below).abs() < 0.01,
                    "slope jumps at {edge}"
                );
            }
            // Monotone, never past the level, odd.
            let mut last = 0.0;
            for i in 0..=400 {
                let x = i as f32 / 100.0;
                let y = clip_curve(x, level, knee);
                assert!(y >= last - 1.0e-7 && y <= level + 1.0e-7);
                assert_eq!(clip_curve(-x, level, knee), -y);
                last = y;
            }
        }
        // Hard corner at shape 0.
        assert_eq!(clip_curve(0.49, 0.5, 0.0), 0.49);
        assert_eq!(clip_curve(0.8, 0.5, 0.0), 0.5);
    }

    #[test]
    fn latency_is_the_filters_plus_the_lookahead() {
        let lookahead = lookahead_samples(RATE);
        assert_eq!(lookahead, 48);
        for oversampling in Oversampling::ALL {
            for mode in ALL_MODES {
                let mut params = default_params();
                params.oversampling = oversampling;
                params.mode = mode;
                let expected = if oversampling == Oversampling::X1 {
                    0
                } else {
                    TAPS_PER_PHASE
                } + if mode == Mode::Clip { 0 } else { lookahead };
                let dsp = dsp_with(params.clone());
                assert_eq!(dsp.latency_samples(), expected, "{oversampling:?} {mode:?}");
                assert_eq!(latency_for(&params, RATE), expected);
            }
        }
        // Clip at 1× is the zero-latency setting.
        let mut live = default_params();
        live.oversampling = Oversampling::X1;
        assert_eq!(dsp_with(live).latency_samples(), 0);
    }

    /// Under the knee, at mix 100, the output is the input — bit for bit,
    /// just delayed by the reported latency — in every mode and at every
    /// oversampling factor.
    #[test]
    fn below_the_knee_is_bit_transparent() {
        for oversampling in Oversampling::ALL {
            for mode in ALL_MODES {
                let mut params = default_params();
                params.mode = mode;
                params.oversampling = oversampling;
                params.threshold_db = -6.0;
                params.dc_filter = false;
                let mut dsp = dsp_with(params);
                let latency = dsp.latency_samples();
                // -12 dBFS against a knee that starts near -8.5 dBFS.
                let input: Vec<(f32, f32)> = (0..4_000)
                    .map(|n| (sine(997.0, 0.25, n), sine(1_511.0, -0.2, n)))
                    .collect();
                for (n, &(l, r)) in input.iter().enumerate() {
                    let (out_l, out_r) = dsp.process_stereo(l, r);
                    let (want_l, want_r) = if n >= latency {
                        input[n - latency]
                    } else {
                        (0.0, 0.0)
                    };
                    assert_eq!(
                        out_l.to_bits(),
                        want_l.to_bits(),
                        "{oversampling:?} {mode:?} at {n}"
                    );
                    assert_eq!(
                        out_r.to_bits(),
                        want_r.to_bits(),
                        "{oversampling:?} {mode:?} at {n}"
                    );
                }
                assert_eq!(dsp.meter_frame().gain_reduction_db, 0.0);
            }
        }
    }

    #[test]
    fn delta_is_silent_under_the_knee_and_carries_what_was_clipped() {
        let mut params = default_params();
        params.delta = true;
        params.dc_filter = false;
        params.threshold_db = -6.0;
        let mut dsp = dsp_with(params);
        let mut quiet_peak = 0.0f32;
        for n in 0..2_000 {
            let (l, _) = dsp.process_stereo(sine(1_000.0, 0.2, n), 0.0);
            quiet_peak = quiet_peak.max(l.abs());
        }
        assert_eq!(quiet_peak, 0.0);
        let mut loud_peak = 0.0f32;
        for n in 0..4_000 {
            let (l, _) = dsp.process_stereo(sine(1_000.0, 0.9, n), 0.0);
            loud_peak = loud_peak.max(l.abs());
        }
        // A 0.9 peak clipped toward 0.5 leaves roughly 0.4 in the delta.
        assert!(loud_peak > 0.3 && loud_peak < 0.45, "{loud_peak}");
    }

    #[test]
    fn power_off_passes_signal_after_the_latency() {
        let mut dsp = Dsp::new(RATE);
        assert!(dsp.apply_ui_param("power", 0.0));
        let latency = dsp.latency_samples();
        assert!(latency > 0, "4× is the default");
        let mut outputs = Vec::new();
        for n in 0..latency + 8 {
            outputs.push(dsp.process_stereo(0.5 + n as f32 * 0.01, -0.4));
        }
        for (n, (l, r)) in outputs.iter().enumerate() {
            if n < latency {
                assert_eq!((*l, *r), (0.0, 0.0));
            } else {
                assert_eq!(*l, 0.5 + (n - latency) as f32 * 0.01);
                assert_eq!(*r, -0.4);
            }
        }
        assert_eq!(dsp.meter_frame().gain_reduction_db, 0.0);
    }

    /// The settled output peak of a steady tone through `dsp`.
    fn settled_peak(dsp: &mut Dsp, freq: f32, amplitude: f32) -> f32 {
        let mut peak = 0.0f32;
        for n in 0..36_000 {
            let x = sine(freq, amplitude, n);
            let (l, r) = dsp.process_stereo(x, x);
            assert!(l.is_finite() && r.is_finite());
            if n >= 24_000 {
                peak = peak.max(l.abs());
            }
        }
        peak
    }

    #[test]
    fn a_hot_sine_peaks_at_the_ceiling() {
        for oversampling in Oversampling::ALL {
            for mode in ALL_MODES {
                for shape in [0.0, 50.0, 100.0] {
                    let mut params = default_params();
                    params.mode = mode;
                    params.oversampling = oversampling;
                    params.shape = shape;
                    params.input_db = 12.0;
                    let ceiling = db_to_linear(params.ceiling_db);
                    let mut dsp = dsp_with(params);
                    let peak = settled_peak(&mut dsp, 1_000.0, 0.9);
                    assert!(
                        peak <= ceiling && peak > ceiling * db_to_linear(-0.2),
                        "{oversampling:?} {mode:?} shape {shape}: {peak} vs {ceiling}"
                    );
                    assert!(dsp.meter_frame().gain_reduction_db > 6.0);
                }
            }
        }
    }

    #[test]
    fn the_input_gain_is_separate_from_the_clip_level() {
        // Pushing the input raises quiet material by exactly that much...
        let mut params = default_params();
        params.input_db = 6.0;
        params.oversampling = Oversampling::X1;
        params.dc_filter = false;
        let mut dsp = dsp_with(params.clone());
        let (l, _) = dsp.process_stereo(0.1, 0.1);
        assert!((l - 0.1 * db_to_linear(6.0)).abs() < 1.0e-6);
        // ...while lowering the threshold leaves it alone.
        params.input_db = 0.0;
        params.threshold_db = -12.0;
        let mut dsp = dsp_with(params);
        let (l, _) = dsp.process_stereo(0.1, 0.1);
        assert_eq!(l, 0.1);
    }

    /// Energy of `signal` at `freq` (a whole number of cycles in the window).
    fn tone_level_db(signal: &[f32], freq: f32) -> f32 {
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (n, &x) in signal.iter().enumerate() {
            let phase = std::f64::consts::TAU * f64::from(freq) * n as f64 / f64::from(RATE);
            re += f64::from(x) * phase.cos();
            im += f64::from(x) * phase.sin();
        }
        let amplitude = 2.0 * (re * re + im * im).sqrt() / signal.len() as f64;
        20.0 * amplitude.max(1.0e-12).log10() as f32
    }

    #[test]
    fn oversampling_pushes_the_aliases_down() {
        // A hard-clipped 7 kHz tone: its 5th harmonic (35 kHz) folds to
        // 13 kHz and its 7th (49 kHz) to 1 kHz at 1×.
        let alias = |oversampling: Oversampling, freq: f32| {
            let mut params = default_params();
            params.oversampling = oversampling;
            params.shape = 0.0;
            params.threshold_db = -6.0;
            params.input_db = 6.0;
            params.dc_filter = false;
            let mut dsp = dsp_with(params);
            let out: Vec<f32> = (0..96_000)
                .map(|n| dsp.process_stereo(sine(7_000.0, 0.9, n), 0.0).0)
                .skip(48_000)
                .collect();
            tone_level_db(&out, freq)
        };
        for freq in [13_000.0, 1_000.0] {
            let plain = alias(Oversampling::X1, freq);
            assert!(
                plain > -50.0,
                "the 1× alias at {freq} should be clear: {plain}"
            );
            for oversampling in [Oversampling::X2, Oversampling::X4, Oversampling::X8] {
                let filtered = alias(oversampling, freq);
                assert!(
                    filtered < plain - 25.0,
                    "{oversampling:?} at {freq}: {filtered} dB vs {plain} dB at 1×"
                );
            }
        }
    }

    #[test]
    fn mode_wire_roundtrip() {
        let mut dsp = Dsp::new(RATE);
        for mode in ALL_MODES {
            assert!(dsp.apply_ui_param("mode", mode.to_wire()));
            assert_eq!(dsp.params().mode, mode);
        }
        for oversampling in Oversampling::ALL {
            assert!(dsp.apply_ui_param("oversampling", oversampling.to_wire()));
            assert_eq!(dsp.params().oversampling, oversampling);
        }
    }

    /// The curve the editor draws is the peak a steady tone comes out at, in
    /// every mode, with and without oversampling.
    #[test]
    fn the_transfer_curve_is_what_a_steady_tone_comes_out_at() {
        let cases = [
            // (input gain, threshold, shape, mix)
            (0.0, 0.0, 50.0, 100.0),
            (6.0, -6.0, 50.0, 100.0),
            (12.0, -9.0, 0.0, 100.0),
            (6.0, -6.0, 100.0, 60.0),
        ];
        for oversampling in [Oversampling::X1, Oversampling::X4] {
            for mode in ALL_MODES {
                for (input_gain, threshold, shape, amount) in cases {
                    for input in [-18.0, -6.0, -1.0] {
                        let mut params = default_params();
                        params.mode = mode;
                        params.oversampling = oversampling;
                        params.input_db = input_gain;
                        params.threshold_db = threshold;
                        params.shape = shape;
                        params.mix = amount;
                        params.dc_filter = false;
                        let mut dsp = dsp_with(params.clone());
                        let measured =
                            linear_to_db(settled_peak(&mut dsp, 1_000.0, db_to_linear(input)));
                        let drawn = transfer_db(&params, input);
                        assert!(
                            (measured - drawn).abs() < 0.3,
                            "{oversampling:?} {mode:?} {input_gain}/{threshold}/{shape}/{amount} \
                             at {input} dB: measured {measured}, drawn {drawn}"
                        );
                    }
                }
            }
        }
    }

    /// An insert being set up replays its stored values before any audio:
    /// they land at once (a new layout included) instead of crossfading in.
    #[test]
    fn edits_before_the_first_sample_land_at_once() {
        let mut dsp = Dsp::new(RATE);
        assert!(dsp.apply_ui_param("mode", Mode::Limit.to_wire()));
        assert!(dsp.apply_ui_param("mix", 40.0));
        assert_eq!(dsp.paths[dsp.active].layout.mode, Mode::Limit);
        assert_eq!(dsp.switch.pos, 1.0);
        assert_eq!(dsp.mix_amount.outer.value, 0.4);
        let _ = dsp.process_stereo(0.1, 0.1);
        assert!(dsp.apply_ui_param("mode", Mode::Clip.to_wire()));
        let _ = dsp.process_stereo(0.1, 0.1);
        assert!(dsp.switch.pos < 1.0, "a running instance crossfades");
    }
}

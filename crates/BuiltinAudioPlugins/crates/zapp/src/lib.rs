//! Zapp — oversampled multi-character saturation.
//!
//! ```text
//! in ─ M/S encode ─ upsample ─┬─ shape ─ DC block ─ low cut ─ high cut ─ tilt ─┐
//!                             └──────────────── dry ───────────────── mix ──────┴─
//!    ─ downsample ─ M/S decode ─ auto gain ─ output ─ (power crossfade) ─ out
//! ```
//!
//! Six characters share one static curve, [`saturate`]: the drive into a
//! waveshaper ([`shape`]) with a bias offset that is subtracted again, so the
//! curve passes through the origin and the bias only tilts it — an asymmetric
//! curve, and even harmonics. The editor draws its transfer curve, its test
//! tone and its harmonic bars from that same function.
//!
//! # Realtime shape
//!
//! Everything is sized in [`Dsp::new`]: the anti-alias filters for every
//! factor, the rings at the highest factor, the filter states. The audio path
//! indexes and multiplies; a change of factor, character or stereo mode dips
//! the output for a few milliseconds and switches at the bottom of the dip
//! rather than clicking.

use biquad::{Biquad, Coefficients, DirectForm1};
use builtin_dsp_core::delay::{Smoothed, smoothing_step};
use builtin_dsp_core::{
    ParamDescriptor, PluginCategory, PluginDescriptor, StereoEffect, clamp, db_to_linear,
    flush_denormal, linear_to_db, make_eq_coefficients, max_filter_frequency, time_constant,
};
use serde::{Deserialize, Serialize};

pub mod ipc;
pub mod oversample;
pub mod presets;
pub mod ui;

pub use ipc::{UI_PARAM_IDS, ui_param_id, ui_param_index};
pub use oversample::TAPS_PER_PHASE;
pub use presets::{FactoryPreset, factory_presets};

use oversample::{AntiAlias, MAX_FACTOR, Ring};

pub const PLUGIN_ID: &str = "futureboard.zapp";

const CLIP_THRESHOLD: f32 = 1.0;
const RMS_WINDOW_SECONDS: f32 = 0.300;
const PEAK_FALL_SECONDS: f32 = 0.400;
/// Drive / bias / mix / output smoothing.
const SMOOTH_MS: f32 = 20.0;
/// The power crossfade.
const FADE_MS: f32 = 10.0;
/// Each half of the dip a factor, character or stereo-mode change rides.
const SWITCH_MS: f32 = 4.0;
/// The wet path's DC blocker corner: removes what the bias leaves behind.
const DC_BLOCK_HZ: f32 = 5.0;
/// The auto gain's level followers.
const AUTO_GAIN_SECONDS: f32 = 0.300;
/// How fast the applied compensation follows its target.
const AUTO_GAIN_SMOOTH_MS: f32 = 60.0;
/// The most the auto gain moves the level either way.
const AUTO_GAIN_LIMIT_DB: f32 = 24.0;
/// Below this input mean square (−70 dBFS) the auto gain holds still.
const AUTO_GAIN_FLOOR_MS: f32 = 1.0e-7;
/// The tilt's pivot and its shelves' Q.
const TILT_PIVOT_HZ: f32 = 1_000.0;
const TILT_Q: f32 = 0.5;
/// Host samples between filter retunes while a cut or the tone moves.
const CONTROL_TICK: u32 = 32;
/// Share of the way a moving cut or tone goes per control tick.
const CONTROL_FOLLOW: f32 = 0.2;
/// Low cut at or under this is off; high cut at or over the next is off.
const LOW_CUT_OFF_HZ: f32 = 20.5;
const HIGH_CUT_OFF_HZ: f32 = 19_950.0;

/// The saturation curve. Wire order is the persisted contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Character {
    Tape,
    Tube,
    Transistor,
    Diode,
    Fold,
    Crush,
}

impl Character {
    pub const ALL: [Character; 6] = [
        Self::Tape,
        Self::Tube,
        Self::Transistor,
        Self::Diode,
        Self::Fold,
        Self::Crush,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tape => "tape",
            Self::Tube => "tube",
            Self::Transistor => "transistor",
            Self::Diode => "diode",
            Self::Fold => "fold",
            Self::Crush => "crush",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Tape => "Tape",
            Self::Tube => "Tube",
            Self::Transistor => "Transistor",
            Self::Diode => "Diode",
            Self::Fold => "Fold",
            Self::Crush => "Crush",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.as_str() == value)
    }

    pub const fn to_wire(self) -> f32 {
        match self {
            Self::Tape => 0.0,
            Self::Tube => 1.0,
            Self::Transistor => 2.0,
            Self::Diode => 3.0,
            Self::Fold => 4.0,
            Self::Crush => 5.0,
        }
    }

    pub fn from_wire(value: f32) -> Self {
        match value.round() as i32 {
            1 => Self::Tube,
            2 => Self::Transistor,
            3 => Self::Diode,
            4 => Self::Fold,
            5 => Self::Crush,
            _ => Self::Tape,
        }
    }
}

/// What part of the stereo image is saturated; the rest passes dry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StereoMode {
    Stereo,
    Mid,
    Side,
}

impl StereoMode {
    pub const fn to_wire(self) -> f32 {
        match self {
            Self::Stereo => 0.0,
            Self::Mid => 1.0,
            Self::Side => 2.0,
        }
    }

    pub fn from_wire(value: f32) -> Self {
        match value.round() as i32 {
            1 => Self::Mid,
            2 => Self::Side,
            _ => Self::Stereo,
        }
    }
}

/// Oversampling factor. Wire order: off, 2×, 4×, 8×.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Oversampling {
    Off,
    X2,
    X4,
    X8,
}

impl Oversampling {
    pub const fn factor(self) -> usize {
        match self {
            Self::Off => 1,
            Self::X2 => 2,
            Self::X4 => 4,
            Self::X8 => 8,
        }
    }

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
            0 => Self::Off,
            1 => Self::X2,
            3 => Self::X8,
            _ => Self::X4,
        }
    }

    /// The delay this setting adds, at the host rate.
    pub const fn latency_samples(self) -> usize {
        if self.factor() > 1 { TAPS_PER_PHASE } else { 0 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Params {
    pub power: bool,
    pub character: Character,
    pub drive_db: f32,
    pub bias: f32,
    pub tone_db: f32,
    pub low_cut_hz: f32,
    pub high_cut_hz: f32,
    /// Wet share, percent.
    pub mix: f32,
    pub output_db: f32,
    pub stereo_mode: StereoMode,
    pub oversampling: Oversampling,
    pub auto_gain: bool,
}

impl Default for Params {
    fn default() -> Self {
        default_params()
    }
}

pub fn default_params() -> Params {
    Params {
        power: true,
        character: Character::Tape,
        drive_db: 9.0,
        bias: 0.0,
        tone_db: 0.0,
        low_cut_hz: 20.0,
        high_cut_hz: 20_000.0,
        mix: 100.0,
        output_db: 0.0,
        stereo_mode: StereoMode::Stereo,
        oversampling: Oversampling::X4,
        auto_gain: true,
    }
}

/// One character's waveshaper at `u`. `levels` is Crush's quantizer step
/// count and ignored by the rest.
#[inline]
pub fn shape(character: Character, u: f32, levels: f32) -> f32 {
    match character {
        Character::Tape => u.tanh(),
        Character::Tube => {
            if u >= 0.0 {
                u.tanh()
            } else {
                (1.4 * u).tanh() / 1.4
            }
        }
        Character::Transistor => u / (1.0 + u.abs().powi(5)).powf(0.2),
        Character::Diode => {
            if u >= 0.0 {
                1.0 - (-u).exp()
            } else {
                -(1.0 - (1.6 * u).exp()) / 1.6
            }
        }
        Character::Fold => (u * std::f32::consts::FRAC_PI_2).sin(),
        Character::Crush => {
            let levels = levels.max(1.0);
            (clamp(u, -1.0, 1.0) * levels).round() / levels
        }
    }
}

/// Crush's step count at a linear drive `k`: harder drive, coarser steps.
#[inline]
fn crush_levels(k: f32) -> f32 {
    (64.0 / k.max(1.0e-6)).round().max(1.0)
}

/// The waveshaper at one drive and bias, with the bias's offset taken back
/// out so the curve passes through the origin.
#[derive(Debug, Clone, Copy)]
struct Curve {
    character: Character,
    k: f32,
    b: f32,
    levels: f32,
    offset: f32,
}

impl Curve {
    fn new(character: Character, k: f32, bias: f32) -> Self {
        let b = bias * 0.8;
        let levels = crush_levels(k);
        Self {
            character,
            k,
            b,
            levels,
            offset: shape(character, b, levels),
        }
    }

    /// The fully wet curve.
    #[inline]
    fn wet(&self, x: f32) -> f32 {
        shape(self.character, self.k * x + self.b, self.levels) - self.offset
    }
}

/// The static curve the DSP applies to an instantaneous sample before its
/// filters and output stage: drive into [`shape`] with the bias's offset
/// removed, blended with the dry sample by the mix. Bypassed, it is the
/// identity. The editor draws its displays from this.
pub fn saturate(params: &Params, x: f32) -> f32 {
    if !params.power {
        return x;
    }
    let curve = Curve::new(params.character, db_to_linear(params.drive_db), params.bias);
    let amount = clamp(params.mix, 0.0, 100.0) / 100.0;
    amount * curve.wet(x) + (1.0 - amount) * x
}

const fn param(
    id: &'static str,
    name: &'static str,
    default_value: f32,
    min: f32,
    max: f32,
    unit: &'static str,
) -> ParamDescriptor {
    ParamDescriptor {
        id,
        name,
        default_value,
        min,
        max,
        unit,
    }
}

const PARAMS: &[ParamDescriptor] = &[
    param("power", "Power", 1.0, 0.0, 1.0, "bool"),
    param("character", "Character", 0.0, 0.0, 5.0, "enum"),
    param("driveDb", "Drive", 9.0, 0.0, 30.0, "dB"),
    param("bias", "Bias", 0.0, -1.0, 1.0, ""),
    param("toneDb", "Tone", 0.0, -12.0, 12.0, "dB"),
    param("lowCutHz", "Low Cut", 20.0, 20.0, 1_000.0, "Hz"),
    param("highCutHz", "High Cut", 20_000.0, 1_000.0, 20_000.0, "Hz"),
    param("mix", "Mix", 100.0, 0.0, 100.0, "%"),
    param("outputDb", "Output", 0.0, -18.0, 18.0, "dB"),
    param("stereoMode", "Saturate", 0.0, 0.0, 2.0, "enum"),
    param("oversampling", "Oversampling", 2.0, 0.0, 3.0, "enum"),
    param("autoGain", "Auto Gain", 1.0, 0.0, 1.0, "bool"),
];

pub fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PLUGIN_ID,
        name: "Zapp",
        vendor: "Futureboard",
        category: PluginCategory::Effect,
        version: env!("CARGO_PKG_VERSION"),
        params: PARAMS,
    }
}

/// Telemetry the editor reads from the host's pad-level block.
pub mod telemetry {
    /// Slots in the block (the host's `BUILTIN_PAD_SLOTS`).
    pub const SLOTS: usize = 64;
    /// The auto gain's compensation as applied now, in dB (0 when off).
    pub const AUTO_GAIN_DB: usize = 0;
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct MeterFrame {
    pub in_peak: f32,
    pub in_rms: f32,
    pub out_peak: f32,
    pub out_rms: f32,
    /// Always 0: saturation reshapes, it does not take level off.
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
            flush_denormal(self.in_peak * self.peak_coeff)
        };
        self.out_peak = if out_abs > self.out_peak {
            out_abs
        } else {
            flush_denormal(self.out_peak * self.peak_coeff)
        };
        let input_ms = (in_l * in_l + in_r * in_r) * 0.5;
        let output_ms = (out_l * out_l + out_r * out_r) * 0.5;
        self.in_ms =
            flush_denormal(self.rms_coeff * self.in_ms + (1.0 - self.rms_coeff) * input_ms);
        self.out_ms =
            flush_denormal(self.rms_coeff * self.out_ms + (1.0 - self.rms_coeff) * output_ms);
        if in_abs >= CLIP_THRESHOLD {
            self.in_clip = true;
        }
        if out_abs >= CLIP_THRESHOLD {
            self.out_clip = true;
        }
    }
}

/// Eased 0…1: zero slope at both ends.
#[inline]
fn ease(pos: f32) -> f32 {
    pos * pos * (3.0 - 2.0 * pos)
}

fn ramp_step(ms: f32, sample_rate: f32) -> f32 {
    1.0 / (ms * 0.001 * sample_rate.max(1.0)).max(1.0)
}

/// A 0…1 crossfade position moving linearly toward its target, read eased.
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
            step: ramp_step(FADE_MS, sample_rate),
        }
    }

    fn set(&mut self, on: bool) {
        self.target = if on { 1.0 } else { 0.0 };
    }

    fn settle(&mut self) {
        self.pos = self.target;
    }

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

/// `a` at `t` 0, `b` at `t` 1 (both exactly).
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

/// One smoothed value that lands exactly once close.
#[inline]
fn glide(value: &mut Smoothed, step: f32) -> f32 {
    if value.value != value.target {
        value.next(step);
        if (value.target - value.value).abs() <= 1.0e-6 * (1.0 + value.target.abs()) {
            value.settle();
        }
    }
    value.value
}

/// Pass-through biquad coefficients.
fn neutral() -> Coefficients<f32> {
    Coefficients {
        a1: 0.0,
        a2: 0.0,
        b0: 1.0,
        b1: 0.0,
        b2: 0.0,
    }
}

/// What the wet path's filters do now, shared by both paths.
#[derive(Debug, Clone, Copy)]
struct Shaping {
    low_hz: f32,
    high_hz: f32,
    tone_db: f32,
    low_on: bool,
    high_on: bool,
    tone_on: bool,
}

/// One saturated path's state: its DC blocker and its filters.
#[derive(Debug, Clone)]
struct Path {
    dc_x: f32,
    dc_y: f32,
    low_cut: DirectForm1<f32>,
    high_cut: DirectForm1<f32>,
    low_shelf: DirectForm1<f32>,
    high_shelf: DirectForm1<f32>,
}

impl Path {
    fn new() -> Self {
        Self {
            dc_x: 0.0,
            dc_y: 0.0,
            low_cut: DirectForm1::<f32>::new(neutral()),
            high_cut: DirectForm1::<f32>::new(neutral()),
            low_shelf: DirectForm1::<f32>::new(neutral()),
            high_shelf: DirectForm1::<f32>::new(neutral()),
        }
    }

    fn reset(&mut self) {
        self.dc_x = 0.0;
        self.dc_y = 0.0;
        self.low_cut.reset_state();
        self.high_cut.reset_state();
        self.low_shelf.reset_state();
        self.high_shelf.reset_state();
    }

    fn tune(&mut self, shaping: &Shaping, rate: f32) {
        let set = |filter: &mut DirectForm1<f32>, kind: &str, hz: f32, gain: f32, q: f32| {
            let coefficients = make_eq_coefficients(kind, hz, gain, q, rate).unwrap_or(neutral());
            filter.update_coefficients(coefficients);
        };
        set(&mut self.low_cut, "highpass", shaping.low_hz, 0.0, 0.707);
        set(&mut self.high_cut, "lowpass", shaping.high_hz, 0.0, 0.707);
        let half = shaping.tone_db * 0.5;
        set(
            &mut self.low_shelf,
            "lowshelf",
            TILT_PIVOT_HZ,
            -half,
            TILT_Q,
        );
        set(
            &mut self.high_shelf,
            "highshelf",
            TILT_PIVOT_HZ,
            half,
            TILT_Q,
        );
    }

    /// One sample through the curve and the wet filters, blended with the
    /// dry sample by `mix`.
    #[inline]
    fn run(&mut self, x: f32, curve: &Curve, mix: f32, shaping: &Shaping, dc_r: f32) -> f32 {
        let shaped = curve.wet(x);
        let mut wet = shaped - self.dc_x + dc_r * self.dc_y;
        self.dc_x = shaped;
        self.dc_y = flush_denormal(wet);
        if shaping.low_on {
            wet = self.low_cut.run(wet);
        }
        if shaping.high_on {
            wet = self.high_cut.run(wet);
        }
        if shaping.tone_on {
            wet = self.high_shelf.run(self.low_shelf.run(wet));
        }
        mix * wet + (1.0 - mix) * x
    }
}

/// What the dip switches between: the settings a click would come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Layout {
    factor: usize,
    character: Character,
    stereo_mode: StereoMode,
}

impl Layout {
    fn of(params: &Params) -> Self {
        Self {
            factor: params.oversampling.factor(),
            character: params.character,
            stereo_mode: params.stereo_mode,
        }
    }
}

fn filter_index(factor: usize) -> usize {
    match factor {
        2 => 0,
        4 => 1,
        _ => 2,
    }
}

/// Top-rate history the decimator reads: its whole filter at the highest
/// factor plus the phases it skips.
const TOP_HISTORY: usize = MAX_FACTOR * TAPS_PER_PHASE + 1 + MAX_FACTOR;
const HOST_HISTORY: usize = TAPS_PER_PHASE + 2;

#[derive(Debug, Clone)]
pub struct Dsp {
    params: Params,
    /// Audio has run since construction or a reset; until it has, edits land
    /// at once.
    started: bool,
    sample_rate: f32,
    filters: [AntiAlias; 3],
    /// Host-rate input of path A (left or mid) and B (right or side).
    hist_a: Ring,
    hist_b: Ring,
    /// Top-rate output of each path, for the decimator.
    top_a: Ring,
    top_b: Ring,
    path_a: Path,
    path_b: Path,
    /// The unprocessed input, for the bypass branch at the same latency.
    dry_l: Ring,
    dry_r: Ring,
    /// The layout running now, and the dip toward the requested one.
    active: Layout,
    switch_env: f32,
    switch_step: f32,
    curve: Curve,
    drive: Smoothed,
    bias: Smoothed,
    mix: Smoothed,
    output: Smoothed,
    smooth_step: f32,
    power: Fade,
    shaping: Shaping,
    tick: u32,
    dc_r: f32,
    auto_in_ms: f32,
    auto_out_ms: f32,
    auto_coeff: f32,
    comp: Smoothed,
    comp_step: f32,
    meters: Meters,
}

impl Dsp {
    pub fn new(sample_rate: f32) -> Self {
        let sr = sample_rate.max(1.0);
        let params = default_params();
        let active = Layout::of(&params);
        let mut dsp = Self {
            started: false,
            sample_rate: sr,
            filters: [AntiAlias::new(2), AntiAlias::new(4), AntiAlias::new(8)],
            hist_a: Ring::new(HOST_HISTORY),
            hist_b: Ring::new(HOST_HISTORY),
            top_a: Ring::new(TOP_HISTORY),
            top_b: Ring::new(TOP_HISTORY),
            path_a: Path::new(),
            path_b: Path::new(),
            dry_l: Ring::new(HOST_HISTORY),
            dry_r: Ring::new(HOST_HISTORY),
            active,
            switch_env: 1.0,
            switch_step: ramp_step(SWITCH_MS, sr),
            curve: Curve::new(params.character, 1.0, 0.0),
            drive: Smoothed::at(1.0),
            bias: Smoothed::at(0.0),
            mix: Smoothed::at(1.0),
            output: Smoothed::at(1.0),
            smooth_step: smoothing_step(SMOOTH_MS, sr),
            power: Fade::new(true, sr),
            shaping: Shaping {
                low_hz: params.low_cut_hz,
                high_hz: params.high_cut_hz,
                tone_db: params.tone_db,
                low_on: false,
                high_on: false,
                tone_on: false,
            },
            tick: 0,
            dc_r: 0.0,
            auto_in_ms: 0.0,
            auto_out_ms: 0.0,
            auto_coeff: time_constant(sr, AUTO_GAIN_SECONDS),
            comp: Smoothed::at(1.0),
            comp_step: smoothing_step(AUTO_GAIN_SMOOTH_MS, sr),
            meters: Meters::new(sr),
            params,
        };
        dsp.apply_params();
        dsp.settle();
        dsp
    }

    pub fn params(&self) -> &Params {
        &self.params
    }

    /// Replace every parameter at once — a state load. Lands without a
    /// glide, a fade or a dip.
    pub fn set_params(&mut self, params: Params) {
        self.params = params;
        ipc::sanitize_params(&mut self.params);
        self.apply_params();
        self.settle();
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

    /// The delay the oversampling setting adds, at the host rate: what the
    /// host compensates. The dry and bypassed signal run at it too.
    pub fn latency_samples(&self) -> usize {
        self.params.oversampling.latency_samples()
    }

    pub fn meter_frame(&self) -> MeterFrame {
        MeterFrame {
            in_peak: self.meters.in_peak,
            in_rms: self.meters.in_ms.max(0.0).sqrt(),
            out_peak: self.meters.out_peak,
            out_rms: self.meters.out_ms.max(0.0).sqrt(),
            gain_reduction_db: 0.0,
            in_clip: self.meters.in_clip,
            out_clip: self.meters.out_clip,
        }
    }

    pub fn clear_clip(&mut self) {
        self.meters.in_clip = false;
        self.meters.out_clip = false;
    }

    /// The auto gain's compensation as applied now, in dB; 0 when it is
    /// off or the plug-in is bypassed.
    pub fn auto_gain_db(&self) -> f32 {
        if self.params.auto_gain && self.params.power {
            linear_to_db(self.comp.value)
        } else {
            0.0
        }
    }

    /// The editor's telemetry block ([`telemetry`]).
    pub fn telemetry(&self) -> [f32; telemetry::SLOTS] {
        let mut block = [0.0; telemetry::SLOTS];
        block[telemetry::AUTO_GAIN_DB] = self.auto_gain_db();
        block
    }

    fn top_rate(&self) -> f32 {
        self.sample_rate * self.active.factor as f32
    }

    fn apply_params(&mut self) {
        let p = &self.params;
        self.drive.target = db_to_linear(p.drive_db);
        self.bias.target = p.bias;
        self.mix.target = clamp(p.mix, 0.0, 100.0) / 100.0;
        self.output.target = db_to_linear(p.output_db);
        self.power.set(p.power);
        if !p.auto_gain {
            self.comp.target = 1.0;
        }
    }

    /// Land everything on its target: values, power, layout, filters.
    fn settle(&mut self) {
        self.drive.settle();
        self.bias.settle();
        self.mix.settle();
        self.output.settle();
        self.power.settle();
        if !self.params.auto_gain {
            self.comp.settle();
        }
        self.shaping.low_hz = self.params.low_cut_hz;
        self.shaping.high_hz = self.params.high_cut_hz;
        self.shaping.tone_db = self.params.tone_db;
        self.adopt_layout();
        self.switch_env = 1.0;
    }

    /// Switches to the requested layout from a clean state, retuned for it.
    fn adopt_layout(&mut self) {
        self.active = Layout::of(&self.params);
        self.top_a.clear();
        self.top_b.clear();
        self.path_a.reset();
        self.path_b.reset();
        self.dc_r = (-std::f32::consts::TAU * DC_BLOCK_HZ / self.top_rate()).exp();
        self.update_curve();
        self.retune();
    }

    fn update_curve(&mut self) {
        self.curve = Curve::new(self.active.character, self.drive.value, self.bias.value);
    }

    fn retune(&mut self) {
        let rate = self.top_rate();
        let s = &mut self.shaping;
        s.low_on = s.low_hz > LOW_CUT_OFF_HZ;
        s.high_on = s.high_hz < HIGH_CUT_OFF_HZ && s.high_hz < max_filter_frequency(rate);
        s.tone_on = s.tone_db.abs() > 0.01;
        let shaping = *s;
        self.path_a.tune(&shaping, rate);
        self.path_b.tune(&shaping, rate);
    }

    /// Moves the cuts and the tone a step toward their targets, retuning
    /// when they moved.
    fn control_tick(&mut self) {
        let mut moved = false;
        let s = &mut self.shaping;
        let mut follow_log = |now: &mut f32, target: f32| {
            if *now != target {
                let next = *now * (target / *now).powf(CONTROL_FOLLOW);
                *now = if (next / target - 1.0).abs() < 1.0e-3 {
                    target
                } else {
                    next
                };
                moved = true;
            }
        };
        follow_log(&mut s.low_hz, self.params.low_cut_hz);
        follow_log(&mut s.high_hz, self.params.high_cut_hz);
        let target = self.params.tone_db;
        if s.tone_db != target {
            let next = s.tone_db + (target - s.tone_db) * CONTROL_FOLLOW;
            s.tone_db = if (target - next).abs() < 0.01 {
                target
            } else {
                next
            };
            moved = true;
        }
        if moved {
            self.retune();
        }
    }

    /// Both paths for one host sample: through the resampler at the active
    /// factor, saturated where `sat_a` / `sat_b` say.
    #[inline]
    fn run_paths(&mut self, a: f32, b: f32, sat_a: bool, sat_b: bool, mix: f32) -> (f32, f32) {
        let curve = self.curve;
        let shaping = self.shaping;
        let dc_r = self.dc_r;
        self.hist_a.push(a);
        self.hist_b.push(b);
        let factor = self.active.factor;
        if factor == 1 {
            let ya = if sat_a {
                self.path_a.run(a, &curve, mix, &shaping, dc_r)
            } else {
                a
            };
            let yb = if sat_b {
                self.path_b.run(b, &curve, mix, &shaping, dc_r)
            } else {
                b
            };
            return (ya, yb);
        }
        let filter = &self.filters[filter_index(factor)];
        let taps = TAPS_PER_PHASE + 1;
        for phase in 0..factor {
            let ua = filter.interpolate(self.hist_a.recent(taps), phase);
            let ub = filter.interpolate(self.hist_b.recent(taps), phase);
            let ya = if sat_a {
                self.path_a.run(ua, &curve, mix, &shaping, dc_r)
            } else {
                ua
            };
            let yb = if sat_b {
                self.path_b.run(ub, &curve, mix, &shaping, dc_r)
            } else {
                ub
            };
            self.top_a.push(ya);
            self.top_b.push(yb);
        }
        // The decimator's newest tap is this step's first phase, which puts
        // the round trip at exactly `TAPS_PER_PHASE` host samples.
        let len = filter.taps.len();
        let skip = factor - 1;
        let oa = filter.decimate(&self.top_a.recent(skip + len)[skip..]);
        let ob = filter.decimate(&self.top_b.recent(skip + len)[skip..]);
        (oa, ob)
    }
}

impl StereoEffect for Dsp {
    fn reset(&mut self) {
        self.started = false;
        self.hist_a.clear();
        self.hist_b.clear();
        self.dry_l.clear();
        self.dry_r.clear();
        self.auto_in_ms = 0.0;
        self.auto_out_ms = 0.0;
        self.comp = Smoothed::at(1.0);
        self.meters.reset();
        self.settle();
    }

    fn set_sample_rate(&mut self, sample_rate: f32) {
        let sr = sample_rate.max(1.0);
        self.sample_rate = sr;
        self.meters = Meters::new(sr);
        self.smooth_step = smoothing_step(SMOOTH_MS, sr);
        self.power.step = ramp_step(FADE_MS, sr);
        self.switch_step = ramp_step(SWITCH_MS, sr);
        self.auto_coeff = time_constant(sr, AUTO_GAIN_SECONDS);
        self.comp_step = smoothing_step(AUTO_GAIN_SMOOTH_MS, sr);
        self.apply_params();
        self.settle();
    }

    fn process_stereo(&mut self, left: f32, right: f32) -> (f32, f32) {
        self.started = true;

        // A new factor, character or stereo mode: dip, switch at the bottom,
        // come back up.
        if Layout::of(&self.params) != self.active {
            self.switch_env = (self.switch_env - self.switch_step).max(0.0);
            if self.switch_env <= 0.0 {
                self.adopt_layout();
            }
        } else if self.switch_env < 1.0 {
            self.switch_env = (self.switch_env + self.switch_step).min(1.0);
        }

        self.tick += 1;
        if self.tick >= CONTROL_TICK {
            self.tick = 0;
            self.control_tick();
        }

        let step = self.smooth_step;
        if self.drive.value != self.drive.target || self.bias.value != self.bias.target {
            glide(&mut self.drive, step);
            glide(&mut self.bias, step);
            self.update_curve();
        }
        let mix = glide(&mut self.mix, step);
        let output = glide(&mut self.output, step);

        // The bypass branch, at the latency the wet path runs at.
        self.dry_l.push(left);
        self.dry_r.push(right);
        let latency = if self.active.factor > 1 {
            TAPS_PER_PHASE
        } else {
            0
        };
        let (dry_l, dry_r) = (self.dry_l.at(latency), self.dry_r.at(latency));

        let mode = self.active.stereo_mode;
        let (a, b, sat_a, sat_b) = match mode {
            StereoMode::Stereo => (left, right, true, true),
            StereoMode::Mid => ((left + right) * 0.5, (left - right) * 0.5, true, false),
            StereoMode::Side => ((left + right) * 0.5, (left - right) * 0.5, false, true),
        };
        let (pa, pb) = self.run_paths(a, b, sat_a, sat_b, mix);
        let (pl, pr) = match mode {
            StereoMode::Stereo => (pa, pb),
            StereoMode::Mid | StereoMode::Side => (pa + pb, pa - pb),
        };

        // Auto gain: hold the output's level at the input's.
        let coeff = self.auto_coeff;
        let in_ms = (left * left + right * right) * 0.5;
        let out_ms = (pl * pl + pr * pr) * 0.5;
        self.auto_in_ms = flush_denormal(coeff * self.auto_in_ms + (1.0 - coeff) * in_ms);
        self.auto_out_ms = flush_denormal(coeff * self.auto_out_ms + (1.0 - coeff) * out_ms);
        if self.params.auto_gain {
            if self.auto_in_ms > AUTO_GAIN_FLOOR_MS && self.auto_out_ms > 1.0e-12 {
                let limit = db_to_linear(AUTO_GAIN_LIMIT_DB);
                self.comp.target = clamp(
                    (self.auto_in_ms / self.auto_out_ms).sqrt(),
                    1.0 / limit,
                    limit,
                );
            }
        } else {
            self.comp.target = 1.0;
        }
        let comp = glide(&mut self.comp, self.comp_step);

        let gain = comp * output;
        let on = self.power.next();
        let envelope = ease(self.switch_env);
        let mut out_l = blend(dry_l, pl * gain, on);
        let mut out_r = blend(dry_r, pr * gain, on);
        if envelope < 1.0 {
            out_l *= envelope;
            out_r *= envelope;
        }
        self.meters.push_stereo(left, right, out_l, out_r);
        (out_l, out_r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    fn sine(freq: f32, amp: f32, n: usize) -> f32 {
        amp * (std::f32::consts::TAU * freq * n as f32 / SR).sin()
    }

    fn quiet_params() -> Params {
        Params {
            auto_gain: false,
            ..default_params()
        }
    }

    /// Harmonic `h`'s magnitude of the static curve on one cycle of a sine.
    fn harmonic(params: &Params, amp: f32, h: usize) -> f32 {
        const N: usize = 512;
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for n in 0..N {
            let x = amp * (std::f32::consts::TAU * n as f32 / N as f32).sin();
            let y = f64::from(saturate(params, x));
            let ph = std::f64::consts::TAU * (h * n) as f64 / N as f64;
            re += y * ph.cos();
            im += y * ph.sin();
        }
        (2.0 * (re * re + im * im).sqrt() / N as f64) as f32
    }

    #[test]
    fn every_shape_passes_the_origin_and_is_continuous() {
        for character in Character::ALL {
            assert_eq!(shape(character, 0.0, 64.0), 0.0, "{character:?}");
            if character == Character::Crush {
                continue;
            }
            let mut u = -3.0f32;
            let mut last = shape(character, u, 64.0);
            while u < 3.0 {
                u += 0.001;
                let now = shape(character, u, 64.0);
                assert!((now - last).abs() < 0.01, "{character:?} jumps at {u}");
                last = now;
            }
        }
    }

    #[test]
    fn crush_steps_coarsen_with_drive() {
        assert_eq!(shape(Character::Crush, 0.26, 4.0), 0.25);
        assert_eq!(shape(Character::Crush, 5.0, 4.0), 1.0);
        assert!(crush_levels(db_to_linear(24.0)) < crush_levels(1.0));
    }

    #[test]
    fn saturate_is_identity_bypassed_or_dry() {
        let mut params = default_params();
        params.drive_db = 24.0;
        params.power = false;
        assert_eq!(saturate(&params, 0.4), 0.4);
        params.power = true;
        params.mix = 0.0;
        assert_eq!(saturate(&params, 0.4), 0.4);
        params.mix = 100.0;
        assert!(saturate(&params, 0.4) < 1.0 && saturate(&params, 0.4) > 0.9);
    }

    #[test]
    fn bias_brings_in_even_harmonics() {
        let mut params = quiet_params();
        params.drive_db = 12.0;
        let h1 = harmonic(&params, 0.5, 1);
        assert!(harmonic(&params, 0.5, 2) / h1 < 1.0e-3);
        assert!(harmonic(&params, 0.5, 3) / h1 > 0.01);
        params.bias = 0.5;
        let h1 = harmonic(&params, 0.5, 1);
        assert!(harmonic(&params, 0.5, 2) / h1 > 0.01);
        // Tube and diode are asymmetric of their own.
        params.bias = 0.0;
        params.character = Character::Diode;
        let h1 = harmonic(&params, 0.5, 1);
        assert!(harmonic(&params, 0.5, 2) / h1 > 0.01);
    }

    #[test]
    fn every_character_and_factor_stays_finite_and_bounded() {
        for character in Character::ALL {
            for oversampling in [
                Oversampling::Off,
                Oversampling::X2,
                Oversampling::X4,
                Oversampling::X8,
            ] {
                let mut dsp = Dsp::new(SR);
                dsp.set_params(Params {
                    character,
                    oversampling,
                    drive_db: 30.0,
                    bias: 0.7,
                    tone_db: 12.0,
                    low_cut_hz: 200.0,
                    high_cut_hz: 5_000.0,
                    ..quiet_params()
                });
                let mut seed = 12_345u32;
                for n in 0..4_800 {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    let noise = (seed >> 9) as f32 / (1u32 << 23) as f32 - 0.5;
                    let x = sine(220.0, 1.0, n) + noise * 0.5;
                    let (l, r) = dsp.process_stereo(x, -x);
                    assert!(
                        l.is_finite() && r.is_finite(),
                        "{character:?} {oversampling:?}"
                    );
                    assert!(
                        l.abs() < 6.0 && r.abs() < 6.0,
                        "{character:?} {oversampling:?}: {l}"
                    );
                }
            }
        }
    }

    #[test]
    fn latency_matches_the_setting_and_the_impulse() {
        for oversampling in [
            Oversampling::Off,
            Oversampling::X2,
            Oversampling::X4,
            Oversampling::X8,
        ] {
            let mut dsp = Dsp::new(SR);
            dsp.set_params(Params {
                oversampling,
                drive_db: 0.0,
                ..quiet_params()
            });
            let expected = if oversampling == Oversampling::Off {
                0
            } else {
                TAPS_PER_PHASE
            };
            assert_eq!(dsp.latency_samples(), expected);
            let mut best = (0usize, 0.0f32);
            for n in 0..200 {
                let x = if n == 0 { 0.01 } else { 0.0 };
                let (l, _) = dsp.process_stereo(x, x);
                if l.abs() > best.1 {
                    best = (n, l.abs());
                }
            }
            assert_eq!(best.0, expected, "{oversampling:?}");
        }
    }

    #[test]
    fn power_off_passes_the_input_at_the_latency() {
        let mut dsp = Dsp::new(SR);
        dsp.set_params(Params {
            power: false,
            drive_db: 30.0,
            ..default_params()
        });
        let latency = dsp.latency_samples();
        let input: Vec<f32> = (0..1_000).map(|n| sine(440.0, 0.8, n)).collect();
        for (n, &x) in input.iter().enumerate() {
            let (l, r) = dsp.process_stereo(x, x * 0.5);
            let expected = if n >= latency {
                input[n - latency]
            } else {
                0.0
            };
            assert_eq!(l, expected);
            assert_eq!(r, expected * 0.5);
        }
        assert_eq!(dsp.telemetry()[telemetry::AUTO_GAIN_DB], 0.0);
    }

    #[test]
    fn auto_gain_holds_the_level_at_the_input() {
        let mut dsp = Dsp::new(SR);
        dsp.set_params(Params {
            drive_db: 18.0,
            ..default_params()
        });
        let total = (SR * 3.0) as usize;
        let tail = (SR * 0.5) as usize;
        let (mut in_sq, mut out_sq) = (0.0f64, 0.0f64);
        for n in 0..total {
            let x = sine(1_000.0, 0.3, n);
            let (l, _) = dsp.process_stereo(x, x);
            if n >= total - tail {
                in_sq += f64::from(x * x);
                out_sq += f64::from(l * l);
            }
        }
        let difference = 10.0 * (out_sq / in_sq).log10();
        assert!(difference.abs() < 1.5, "auto gain off by {difference} dB");
        let applied = dsp.telemetry()[telemetry::AUTO_GAIN_DB];
        assert!(applied < -6.0, "compensation {applied} dB");
    }

    #[test]
    fn switching_while_playing_dips_and_recovers() {
        let mut dsp = Dsp::new(SR);
        dsp.set_params(quiet_params());
        for n in 0..2_000 {
            dsp.process_stereo(sine(300.0, 0.5, n), 0.0);
        }
        let wire = |id| ui_param_index(id).unwrap();
        assert!(dsp.apply_wire_param(wire("oversampling"), Oversampling::X8.to_wire()));
        assert!(dsp.apply_wire_param(wire("character"), Character::Fold.to_wire()));
        assert!(dsp.apply_wire_param(wire("stereoMode"), StereoMode::Mid.to_wire()));
        let mut peak_late = 0.0f32;
        for n in 2_000..8_000 {
            let (l, r) = dsp.process_stereo(sine(300.0, 0.5, n), 0.0);
            assert!(l.is_finite() && r.is_finite());
            if n > 6_000 {
                peak_late = peak_late.max(l.abs());
            }
        }
        assert_eq!(dsp.active, Layout::of(dsp.params()));
        assert_eq!(dsp.switch_env, 1.0);
        assert!(peak_late > 0.1);
        assert_eq!(dsp.latency_samples(), TAPS_PER_PHASE);
    }

    #[test]
    fn mid_mode_leaves_the_side_dry() {
        let mut dsp = Dsp::new(SR);
        dsp.set_params(Params {
            stereo_mode: StereoMode::Mid,
            oversampling: Oversampling::Off,
            drive_db: 24.0,
            ..quiet_params()
        });
        // A pure side signal has no mid to saturate: it passes unchanged.
        for n in 0..500 {
            let x = sine(500.0, 0.6, n);
            let (l, r) = dsp.process_stereo(x, -x);
            assert!((l - x).abs() < 1.0e-6 && (r + x).abs() < 1.0e-6);
        }
    }

    #[test]
    fn the_descriptor_lists_the_wire_table() {
        let descriptor = descriptor();
        assert_eq!(descriptor.name, "Zapp");
        let ids: Vec<_> = descriptor.params.iter().map(|p| p.id).collect();
        assert_eq!(ids, UI_PARAM_IDS);
        for (character, label) in Character::ALL.iter().zip(["Tape", "Tube", "Transistor"]) {
            assert_eq!(character.label(), label);
        }
        for character in Character::ALL {
            assert_eq!(Character::from_wire(character.to_wire()), character);
            assert_eq!(Character::parse(character.as_str()), Some(character));
        }
    }
}

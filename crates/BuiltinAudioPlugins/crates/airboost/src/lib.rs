//! AirBoost — air EQ, presence, exciter and a built-in de-esser.
//!
//! ```text
//! in ─ M/S encode ─┬─ air ─ presence ─┬─(+)─ de-ess ─┐
//!                  └─ exciter ────────┘              ├─ mix ─ M/S decode ─ auto gain ─ output ─ out
//!                  └──────────────── dry ────────────┘
//! ```
//!
//! - **Air** is a high shelf or a bell at one of six frequencies; a corner
//!   above what the host rate can hold sits at 0.45·fs, so 40 kHz is a gentle
//!   lift of the top octave.
//! - **Presence** is a bell at 3.5 kHz.
//! - **Excite** high-passes the input at half the air frequency, normalises
//!   that band by its own envelope and runs it through second and third
//!   Chebyshev polynomials — pure 2nd and 3rd harmonics of a steady tone, so
//!   the even one makes it asymmetric — then high-passes what that adds and
//!   mixes it back in at the band's own level.
//! - **De-ess** compares a 5–9 kHz band's envelope with the broadband one, so
//!   it acts on sibilance whatever the level, and dips a 7 kHz band by up to
//!   8 dB while the band dominates.
//!
//! The editor draws the static curve from [`eq_response_db`], built from the
//! same coefficient functions the audio path runs.
//!
//! # Realtime shape
//!
//! Everything is sized in [`Dsp::new`]. The audio path indexes and
//! multiplies; the air and presence filters retune on a control tick while
//! they move, and a change of shape or of what the plug-in is applied to
//! dips the output for a few milliseconds and switches at the bottom of the
//! dip rather than clicking.

use biquad::{Biquad, Coefficients, DirectForm1};
use builtin_dsp_core::delay::{Smoothed, smoothing_step};
use builtin_dsp_core::{
    ParamDescriptor, PluginCategory, PluginDescriptor, StereoEffect, clamp, db_to_linear,
    flush_denormal, linear_to_db, make_eq_coefficients, time_constant,
};
use serde::{Deserialize, Serialize};

pub mod ipc;
pub mod presets;
pub mod ui;

pub use ipc::{UI_PARAM_IDS, ui_param_id, ui_param_index};
pub use presets::{FactoryPreset, factory_presets};

pub const PLUGIN_ID: &str = "futureboard.airboost";

const CLIP_THRESHOLD: f32 = 1.0;
const RMS_WINDOW_SECONDS: f32 = 0.300;
const PEAK_FALL_SECONDS: f32 = 0.400;
/// Mix / output / excite / de-ess amount smoothing.
const SMOOTH_MS: f32 = 20.0;
/// The power crossfade.
const FADE_MS: f32 = 10.0;
/// Each half of the dip a shape or stereo-mode change rides.
const SWITCH_MS: f32 = 4.0;
/// The auto gain's level followers.
const AUTO_GAIN_SECONDS: f32 = 0.300;
/// How fast the applied compensation follows its target.
const AUTO_GAIN_SMOOTH_MS: f32 = 60.0;
/// The most the auto gain moves the level either way.
const AUTO_GAIN_LIMIT_DB: f32 = 12.0;
/// Below this input mean square (−70 dBFS) the auto gain holds still.
const AUTO_GAIN_FLOOR_MS: f32 = 1.0e-7;
/// Host samples between filter retunes while the air or presence moves, and
/// between de-ess gain computations.
const CONTROL_TICK: u32 = 16;
/// Share of the way a moving air or presence setting goes per control tick.
const CONTROL_FOLLOW: f32 = 0.2;

/// The air band's Q as a shelf and as a bell.
pub const SHELF_Q: f32 = 0.55;
pub const BELL_Q: f32 = 0.6;
/// The highest the air corner sits, as a share of the host rate.
pub const AIR_MAX_RATIO: f32 = 0.45;
/// The presence bell.
pub const PRESENCE_HZ: f32 = 3_500.0;
pub const PRESENCE_Q: f32 = 0.9;
/// The de-esser's cut band and its detector band.
pub const DEESS_HZ: f32 = 7_000.0;
pub const DEESS_Q: f32 = 1.8;
const DETECT_HZ: f32 = 6_700.0;
const DETECT_Q: f32 = 1.4;
/// The most the de-esser cuts, at 100 %.
pub const DEESS_MAX_DB: f32 = 8.0;
/// Band-over-broadband ratio (dB) where the de-esser starts, and the span
/// over which it reaches its full cut.
const DEESS_ONSET_DB: f32 = -15.0;
const DEESS_SPAN_DB: f32 = 12.0;
/// Below this broadband level (−60 dBFS) the de-esser lets go.
const DEESS_GATE: f32 = 1.0e-3;
const DETECT_ATTACK_SECONDS: f32 = 0.0005;
const DETECT_RELEASE_SECONDS: f32 = 0.060;
const CUT_ATTACK_MS: f32 = 1.0;
const CUT_RELEASE_MS: f32 = 60.0;
/// The exciter: its band envelope, its harmonic weights and its level at
/// 100 %.
const EXCITE_ATTACK_SECONDS: f32 = 0.001;
const EXCITE_RELEASE_SECONDS: f32 = 0.040;
const EXCITE_SECOND: f32 = 0.5;
const EXCITE_THIRD: f32 = 0.3;
const EXCITE_GAIN: f32 = 1.0;
/// The exciter's corner stays at or under this share of the host rate.
const EXCITE_MAX_RATIO: f32 = 0.25;
/// The floor [`telemetry::EXCITE_DB`] reads when nothing is added.
pub const EXCITE_FLOOR_DB: f32 = -120.0;

/// The air band's frequency. Wire order is the persisted contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AirFreq {
    #[serde(rename = "2.5k")]
    K2_5,
    #[serde(rename = "5k")]
    K5,
    #[serde(rename = "8k")]
    K8,
    #[serde(rename = "12k")]
    K12,
    #[serde(rename = "20k")]
    K20,
    #[serde(rename = "40k")]
    K40,
}

impl AirFreq {
    pub const ALL: [AirFreq; 6] = [
        Self::K2_5,
        Self::K5,
        Self::K8,
        Self::K12,
        Self::K20,
        Self::K40,
    ];

    pub const fn hz(self) -> f32 {
        match self {
            Self::K2_5 => 2_500.0,
            Self::K5 => 5_000.0,
            Self::K8 => 8_000.0,
            Self::K12 => 12_000.0,
            Self::K20 => 20_000.0,
            Self::K40 => 40_000.0,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::K2_5 => "2.5k",
            Self::K5 => "5k",
            Self::K8 => "8k",
            Self::K12 => "12k",
            Self::K20 => "20k",
            Self::K40 => "40k",
        }
    }

    pub const fn to_wire(self) -> f32 {
        match self {
            Self::K2_5 => 0.0,
            Self::K5 => 1.0,
            Self::K8 => 2.0,
            Self::K12 => 3.0,
            Self::K20 => 4.0,
            Self::K40 => 5.0,
        }
    }

    pub fn from_wire(value: f32) -> Self {
        match value.round() as i32 {
            0 => Self::K2_5,
            1 => Self::K5,
            2 => Self::K8,
            4 => Self::K20,
            5 => Self::K40,
            _ => Self::K12,
        }
    }
}

/// The air band's shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Shape {
    Shelf,
    Bell,
}

impl Shape {
    pub const fn to_wire(self) -> f32 {
        match self {
            Self::Shelf => 0.0,
            Self::Bell => 1.0,
        }
    }

    pub fn from_wire(value: f32) -> Self {
        if value.round() as i32 == 1 {
            Self::Bell
        } else {
            Self::Shelf
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Shelf => "Shelf",
            Self::Bell => "Bell",
        }
    }
}

/// What part of the stereo image AirBoost works on; the rest passes dry.
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

    pub const fn label(self) -> &'static str {
        match self {
            Self::Stereo => "Stereo",
            Self::Mid => "Mid",
            Self::Side => "Side",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Params {
    pub power: bool,
    pub air_db: f32,
    pub air_freq: AirFreq,
    pub shape: Shape,
    pub presence_db: f32,
    /// Exciter amount, percent.
    pub excite: f32,
    /// De-esser amount, percent.
    pub deess: f32,
    pub stereo_mode: StereoMode,
    /// Wet share, percent.
    pub mix: f32,
    pub output_db: f32,
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
        air_db: 4.0,
        air_freq: AirFreq::K12,
        shape: Shape::Shelf,
        presence_db: 0.0,
        excite: 25.0,
        deess: 30.0,
        stereo_mode: StereoMode::Stereo,
        mix: 100.0,
        output_db: 0.0,
        auto_gain: true,
    }
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
    param("airDb", "Air", 4.0, 0.0, 12.0, "dB"),
    param("airFreq", "Air Frequency", 3.0, 0.0, 5.0, "enum"),
    param("shape", "Shape", 0.0, 0.0, 1.0, "enum"),
    param("presenceDb", "Presence", 0.0, -6.0, 6.0, "dB"),
    param("excite", "Excite", 25.0, 0.0, 100.0, "%"),
    param("deess", "De-Ess", 30.0, 0.0, 100.0, "%"),
    param("stereoMode", "Apply To", 0.0, 0.0, 2.0, "enum"),
    param("mix", "Mix", 100.0, 0.0, 100.0, "%"),
    param("outputDb", "Output", 0.0, -12.0, 12.0, "dB"),
    param("autoGain", "Auto Gain", 1.0, 0.0, 1.0, "bool"),
];

pub fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PLUGIN_ID,
        name: "AirBoost",
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
    /// The de-esser's cut now, in dB (0 or below).
    pub const DEESS_DB: usize = 1;
    /// What the exciter adds, relative to the input, in dB
    /// ([`crate::EXCITE_FLOOR_DB`] when nothing).
    pub const EXCITE_DB: usize = 2;
}

// ── Coefficients, shared by the audio path and the editor's curve ──────────

fn neutral() -> Coefficients<f32> {
    Coefficients {
        a1: 0.0,
        a2: 0.0,
        b0: 1.0,
        b1: 0.0,
        b2: 0.0,
    }
}

/// The frequency the air band runs at, at `sample_rate`.
pub fn air_corner_hz(air_hz: f32, sample_rate: f32) -> f32 {
    air_hz.min(sample_rate.max(1.0) * AIR_MAX_RATIO)
}

fn air_coefficients(shape: Shape, air_hz: f32, air_db: f32, rate: f32) -> Coefficients<f32> {
    let hz = air_corner_hz(air_hz, rate);
    let coefficients = match shape {
        Shape::Shelf => make_eq_coefficients("highshelf", hz, air_db, SHELF_Q, rate),
        Shape::Bell => make_eq_coefficients("bell", hz, air_db, BELL_Q, rate),
    };
    coefficients.unwrap_or(neutral())
}

fn presence_coefficients(presence_db: f32, rate: f32) -> Coefficients<f32> {
    make_eq_coefficients("bell", PRESENCE_HZ, presence_db, PRESENCE_Q, rate).unwrap_or(neutral())
}

fn deess_coefficients(rate: f32) -> Coefficients<f32> {
    make_eq_coefficients("bandpass", DEESS_HZ, 0.0, DEESS_Q, rate).unwrap_or(neutral())
}

fn detect_coefficients(rate: f32) -> Coefficients<f32> {
    make_eq_coefficients("bandpass", DETECT_HZ, 0.0, DETECT_Q, rate).unwrap_or(neutral())
}

/// The exciter's corner: half the air frequency, kept well under Nyquist.
pub fn excite_corner_hz(air_hz: f32, sample_rate: f32) -> f32 {
    (air_hz * 0.5).min(sample_rate.max(1.0) * EXCITE_MAX_RATIO)
}

fn excite_coefficients(air_hz: f32, rate: f32) -> [Coefficients<f32>; 3] {
    let hz = excite_corner_hz(air_hz, rate);
    // A 4th-order Butterworth high-pass ahead of the shaper, a 2nd-order one
    // after it.
    let hp = |q: f32| make_eq_coefficients("highpass", hz, 0.0, q, rate).unwrap_or(neutral());
    [hp(0.541), hp(1.307), hp(0.707)]
}

/// A biquad's complex response at `freq_hz`.
fn response(c: &Coefficients<f32>, freq_hz: f32, rate: f32) -> (f64, f64) {
    let w = std::f64::consts::TAU * f64::from(freq_hz) / f64::from(rate.max(1.0));
    let (c1, s1, c2, s2) = (w.cos(), w.sin(), (2.0 * w).cos(), (2.0 * w).sin());
    let nr = f64::from(c.b0) + f64::from(c.b1) * c1 + f64::from(c.b2) * c2;
    let ni = -(f64::from(c.b1) * s1 + f64::from(c.b2) * s2);
    let dr = 1.0 + f64::from(c.a1) * c1 + f64::from(c.a2) * c2;
    let di = -(f64::from(c.a1) * s1 + f64::from(c.a2) * s2);
    let d = (dr * dr + di * di).max(1e-30);
    ((nr * dr + ni * di) / d, (ni * dr - nr * di) / d)
}

fn mul((ar, ai): (f64, f64), (br, bi): (f64, f64)) -> (f64, f64) {
    (ar * br - ai * bi, ar * bi + ai * br)
}

fn magnitude_db((re, im): (f64, f64)) -> f32 {
    (10.0 * (re * re + im * im).max(1e-30).log10()) as f32
}

/// The static EQ — air and presence, blended with the dry signal by mix — in
/// dB at `freq_hz`, from the coefficients the audio path runs. 0 when the
/// plug-in is bypassed.
pub fn eq_response_db(params: &Params, freq_hz: f32, sample_rate: f32) -> f32 {
    if !params.power {
        return 0.0;
    }
    let rate = sample_rate.max(1.0);
    let air = air_coefficients(params.shape, params.air_freq.hz(), params.air_db, rate);
    let presence = presence_coefficients(params.presence_db, rate);
    let (re, im) = mul(
        response(&air, freq_hz, rate),
        response(&presence, freq_hz, rate),
    );
    let mix = f64::from(clamp(params.mix, 0.0, 100.0) / 100.0);
    magnitude_db((mix * re + (1.0 - mix), mix * im))
}

/// The static EQ with the de-esser cutting `cut_db` (0 or below), as
/// [`eq_response_db`].
pub fn eq_response_with_cut_db(
    params: &Params,
    cut_db: f32,
    freq_hz: f32,
    sample_rate: f32,
) -> f32 {
    if !params.power {
        return 0.0;
    }
    let rate = sample_rate.max(1.0);
    let air = air_coefficients(params.shape, params.air_freq.hz(), params.air_db, rate);
    let presence = presence_coefficients(params.presence_db, rate);
    let band = response(&deess_coefficients(rate), freq_hz, rate);
    let g = f64::from(db_to_linear(cut_db.min(0.0))) - 1.0;
    let deess = (1.0 + g * band.0, g * band.1);
    let (re, im) = mul(
        mul(
            response(&air, freq_hz, rate),
            response(&presence, freq_hz, rate),
        ),
        deess,
    );
    let mix = f64::from(clamp(params.mix, 0.0, 100.0) / 100.0);
    magnitude_db((mix * re + (1.0 - mix), mix * im))
}

// ── State ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct MeterFrame {
    pub in_peak: f32,
    pub in_rms: f32,
    pub out_peak: f32,
    pub out_rms: f32,
    /// What the de-esser takes off now, in dB (0 or above).
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

/// A peak follower's next value.
#[inline]
fn follow(env: f32, level: f32, attack: f32, release: f32) -> f32 {
    let coeff = if level > env { attack } else { release };
    flush_denormal(coeff * env + (1.0 - coeff) * level)
}

/// What the paths share for one sample.
#[derive(Debug, Clone, Copy)]
struct Knobs {
    excite: f32,
    /// The de-ess band's gain less one: 0 when not cutting.
    deess: f32,
    mix: f32,
    excite_attack: f32,
    excite_release: f32,
}

/// One processed path's filters and the exciter's state.
#[derive(Debug, Clone)]
struct Path {
    air: DirectForm1<f32>,
    presence: DirectForm1<f32>,
    ex_hp_a: DirectForm1<f32>,
    ex_hp_b: DirectForm1<f32>,
    ex_post: DirectForm1<f32>,
    ex_env: f32,
    deess: DirectForm1<f32>,
}

impl Path {
    fn new() -> Self {
        Self {
            air: DirectForm1::<f32>::new(neutral()),
            presence: DirectForm1::<f32>::new(neutral()),
            ex_hp_a: DirectForm1::<f32>::new(neutral()),
            ex_hp_b: DirectForm1::<f32>::new(neutral()),
            ex_post: DirectForm1::<f32>::new(neutral()),
            ex_env: 0.0,
            deess: DirectForm1::<f32>::new(neutral()),
        }
    }

    fn reset(&mut self) {
        self.air.reset_state();
        self.presence.reset_state();
        self.ex_hp_a.reset_state();
        self.ex_hp_b.reset_state();
        self.ex_post.reset_state();
        self.ex_env = 0.0;
        self.deess.reset_state();
    }

    /// One sample: the blended output and what the exciter added to it.
    #[inline]
    fn run(&mut self, x: f32, k: &Knobs) -> (f32, f32) {
        let eq = self.presence.run(self.air.run(x));

        // The exciter's band, normalised by its own envelope so its
        // harmonics follow the band's level rather than its square or cube.
        let band = self.ex_hp_b.run(self.ex_hp_a.run(x));
        self.ex_env = follow(self.ex_env, band.abs(), k.excite_attack, k.excite_release);
        let u = clamp(band / (self.ex_env + 1.0e-6), -1.0, 1.0);
        let t2 = 2.0 * u * u - 1.0;
        let t3 = (4.0 * u * u - 3.0) * u;
        let harmonics = (EXCITE_SECOND * t2 + EXCITE_THIRD * t3) * self.ex_env;
        let added = self.ex_post.run(harmonics) * k.excite;

        let sum = eq + added;
        let wet = sum + k.deess * self.deess.run(sum);
        (k.mix * wet + (1.0 - k.mix) * x, k.mix * added)
    }
}

/// What the dip switches between: the settings a click would come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Layout {
    shape: Shape,
    stereo_mode: StereoMode,
}

impl Layout {
    fn of(params: &Params) -> Self {
        Self {
            shape: params.shape,
            stereo_mode: params.stereo_mode,
        }
    }
}

/// Where the moving filter settings are now.
#[derive(Debug, Clone, Copy)]
struct Tuning {
    air_hz: f32,
    air_db: f32,
    presence_db: f32,
}

#[derive(Debug, Clone)]
pub struct Dsp {
    params: Params,
    /// Audio has run since construction or a reset; until it has, edits land
    /// at once.
    started: bool,
    sample_rate: f32,
    path_a: Path,
    path_b: Path,
    active: Layout,
    switch_env: f32,
    switch_step: f32,
    tuning: Tuning,
    tick: u32,
    excite: Smoothed,
    deess_amount: Smoothed,
    mix: Smoothed,
    output: Smoothed,
    smooth_step: f32,
    power: Fade,
    detect: DirectForm1<f32>,
    env_band: f32,
    env_full: f32,
    detect_attack: f32,
    detect_release: f32,
    excite_attack: f32,
    excite_release: f32,
    /// The de-ess cut's linear gain, and where it is heading.
    cut: f32,
    cut_target: f32,
    cut_attack: f32,
    cut_release: f32,
    added_ms: f32,
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
        let mut dsp = Self {
            started: false,
            sample_rate: sr,
            path_a: Path::new(),
            path_b: Path::new(),
            active: Layout::of(&params),
            switch_env: 1.0,
            switch_step: ramp_step(SWITCH_MS, sr),
            tuning: Tuning {
                air_hz: params.air_freq.hz(),
                air_db: params.air_db,
                presence_db: params.presence_db,
            },
            tick: 0,
            excite: Smoothed::at(0.0),
            deess_amount: Smoothed::at(0.0),
            mix: Smoothed::at(1.0),
            output: Smoothed::at(1.0),
            smooth_step: smoothing_step(SMOOTH_MS, sr),
            power: Fade::new(true, sr),
            detect: DirectForm1::<f32>::new(neutral()),
            env_band: 0.0,
            env_full: 0.0,
            detect_attack: 0.0,
            detect_release: 0.0,
            excite_attack: 0.0,
            excite_release: 0.0,
            cut: 1.0,
            cut_target: 1.0,
            cut_attack: 0.0,
            cut_release: 0.0,
            added_ms: 0.0,
            auto_in_ms: 0.0,
            auto_out_ms: 0.0,
            auto_coeff: time_constant(sr, AUTO_GAIN_SECONDS),
            comp: Smoothed::at(1.0),
            comp_step: smoothing_step(AUTO_GAIN_SMOOTH_MS, sr),
            meters: Meters::new(sr),
            params,
        };
        dsp.rate_changed();
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

    /// Every stage is a minimum-phase filter or a sample-by-sample gain.
    pub fn latency_samples(&self) -> usize {
        0
    }

    pub fn meter_frame(&self) -> MeterFrame {
        MeterFrame {
            in_peak: self.meters.in_peak,
            in_rms: self.meters.in_ms.max(0.0).sqrt(),
            out_peak: self.meters.out_peak,
            out_rms: self.meters.out_ms.max(0.0).sqrt(),
            gain_reduction_db: -self.deess_db(),
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

    /// The de-esser's cut now, in dB: 0 or below, 0 while bypassed.
    pub fn deess_db(&self) -> f32 {
        if !self.params.power {
            return 0.0;
        }
        let gain = 1.0 + (self.cut - 1.0) * self.deess_amount.value;
        let db = linear_to_db(gain);
        if db > -0.01 { 0.0 } else { db }
    }

    /// What the exciter adds, relative to the input, in dB.
    pub fn excite_db(&self) -> f32 {
        if !self.params.power || self.added_ms <= 1.0e-14 || self.meters.in_ms <= 1.0e-12 {
            return EXCITE_FLOOR_DB;
        }
        (10.0 * (self.added_ms / self.meters.in_ms).log10()).max(EXCITE_FLOOR_DB)
    }

    /// The editor's telemetry block ([`telemetry`]).
    pub fn telemetry(&self) -> [f32; telemetry::SLOTS] {
        let mut block = [0.0; telemetry::SLOTS];
        block[telemetry::AUTO_GAIN_DB] = self.auto_gain_db();
        block[telemetry::DEESS_DB] = self.deess_db();
        block[telemetry::EXCITE_DB] = self.excite_db();
        block
    }

    fn rate_changed(&mut self) {
        let sr = self.sample_rate;
        self.meters = Meters::new(sr);
        self.smooth_step = smoothing_step(SMOOTH_MS, sr);
        self.power.step = ramp_step(FADE_MS, sr);
        self.switch_step = ramp_step(SWITCH_MS, sr);
        self.auto_coeff = time_constant(sr, AUTO_GAIN_SECONDS);
        self.comp_step = smoothing_step(AUTO_GAIN_SMOOTH_MS, sr);
        self.detect_attack = time_constant(sr, DETECT_ATTACK_SECONDS);
        self.detect_release = time_constant(sr, DETECT_RELEASE_SECONDS);
        self.excite_attack = time_constant(sr, EXCITE_ATTACK_SECONDS);
        self.excite_release = time_constant(sr, EXCITE_RELEASE_SECONDS);
        // The cut moves once per control tick.
        let tick_rate = sr / CONTROL_TICK as f32;
        self.cut_attack = time_constant(tick_rate, CUT_ATTACK_MS * 0.001);
        self.cut_release = time_constant(tick_rate, CUT_RELEASE_MS * 0.001);
        self.detect.update_coefficients(detect_coefficients(sr));
        let deess = deess_coefficients(sr);
        self.path_a.deess.update_coefficients(deess);
        self.path_b.deess.update_coefficients(deess);
    }

    fn apply_params(&mut self) {
        let p = &self.params;
        self.excite.target = clamp(p.excite, 0.0, 100.0) / 100.0 * EXCITE_GAIN;
        self.deess_amount.target = clamp(p.deess, 0.0, 100.0) / 100.0;
        self.mix.target = clamp(p.mix, 0.0, 100.0) / 100.0;
        self.output.target = db_to_linear(p.output_db);
        self.power.set(p.power);
        if !p.auto_gain {
            self.comp.target = 1.0;
        }
    }

    /// Land everything on its target: values, power, layout, filters.
    fn settle(&mut self) {
        self.excite.settle();
        self.deess_amount.settle();
        self.mix.settle();
        self.output.settle();
        self.power.settle();
        if !self.params.auto_gain {
            self.comp.settle();
        }
        self.tuning = Tuning {
            air_hz: self.params.air_freq.hz(),
            air_db: self.params.air_db,
            presence_db: self.params.presence_db,
        };
        self.adopt_layout();
        self.switch_env = 1.0;
    }

    /// Switches to the requested layout from a clean state, retuned for it.
    fn adopt_layout(&mut self) {
        self.active = Layout::of(&self.params);
        self.path_a.reset();
        self.path_b.reset();
        self.retune();
    }

    fn retune(&mut self) {
        let rate = self.sample_rate;
        let t = self.tuning;
        let air = air_coefficients(self.active.shape, t.air_hz, t.air_db, rate);
        let presence = presence_coefficients(t.presence_db, rate);
        let [hp_a, hp_b, post] = excite_coefficients(t.air_hz, rate);
        for path in [&mut self.path_a, &mut self.path_b] {
            path.air.update_coefficients(air);
            path.presence.update_coefficients(presence);
            path.ex_hp_a.update_coefficients(hp_a);
            path.ex_hp_b.update_coefficients(hp_b);
            path.ex_post.update_coefficients(post);
        }
    }

    /// Moves the air and presence a step toward their targets, retuning when
    /// they moved, and sets the de-esser's next cut.
    fn control_tick(&mut self) {
        let mut moved = false;
        let t = &mut self.tuning;
        let target = self.params.air_freq.hz();
        if t.air_hz != target {
            let next = t.air_hz * (target / t.air_hz).powf(CONTROL_FOLLOW);
            t.air_hz = if (next / target - 1.0).abs() < 1.0e-3 {
                target
            } else {
                next
            };
            moved = true;
        }
        for (now, target) in [
            (&mut t.air_db, self.params.air_db),
            (&mut t.presence_db, self.params.presence_db),
        ] {
            if *now != target {
                let next = *now + (target - *now) * CONTROL_FOLLOW;
                *now = if (target - next).abs() < 0.01 {
                    target
                } else {
                    next
                };
                moved = true;
            }
        }
        if moved {
            self.retune();
        }

        // The de-esser: how far the band stands over the whole signal.
        self.cut_target = if self.env_full > DEESS_GATE {
            let ratio_db = linear_to_db(self.env_band / self.env_full);
            let amount = clamp((ratio_db - DEESS_ONSET_DB) / DEESS_SPAN_DB, 0.0, 1.0);
            db_to_linear(-DEESS_MAX_DB * amount)
        } else {
            1.0
        };
        let coeff = if self.cut_target < self.cut {
            self.cut_attack
        } else {
            self.cut_release
        };
        self.cut = coeff * self.cut + (1.0 - coeff) * self.cut_target;
        if (self.cut - 1.0).abs() < 1.0e-6 {
            self.cut = 1.0;
        }
    }
}

impl StereoEffect for Dsp {
    fn reset(&mut self) {
        self.started = false;
        self.detect.reset_state();
        self.env_band = 0.0;
        self.env_full = 0.0;
        self.cut = 1.0;
        self.cut_target = 1.0;
        self.added_ms = 0.0;
        self.auto_in_ms = 0.0;
        self.auto_out_ms = 0.0;
        self.comp = Smoothed::at(1.0);
        self.meters.reset();
        self.settle();
    }

    fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        self.rate_changed();
        self.apply_params();
        self.settle();
    }

    fn process_stereo(&mut self, left: f32, right: f32) -> (f32, f32) {
        self.started = true;

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
        let excite = glide(&mut self.excite, step);
        let amount = glide(&mut self.deess_amount, step);
        let mix = glide(&mut self.mix, step);
        let output = glide(&mut self.output, step);

        let mode = self.active.stereo_mode;
        let (a, b, on_a, on_b) = match mode {
            StereoMode::Stereo => (left, right, true, true),
            StereoMode::Mid => ((left + right) * 0.5, (left - right) * 0.5, true, false),
            StereoMode::Side => ((left + right) * 0.5, (left - right) * 0.5, false, true),
        };

        // The de-esser listens to what it works on.
        let heard = match mode {
            StereoMode::Stereo => (a + b) * 0.5,
            StereoMode::Mid => a,
            StereoMode::Side => b,
        };
        let band = self.detect.run(heard);
        self.env_band = follow(
            self.env_band,
            band.abs(),
            self.detect_attack,
            self.detect_release,
        );
        self.env_full = follow(
            self.env_full,
            heard.abs(),
            self.detect_attack,
            self.detect_release,
        );

        let knobs = Knobs {
            excite,
            deess: (self.cut - 1.0) * amount,
            mix,
            excite_attack: self.excite_attack,
            excite_release: self.excite_release,
        };
        let (pa, added_a) = if on_a {
            self.path_a.run(a, &knobs)
        } else {
            (a, 0.0)
        };
        let (pb, added_b) = if on_b {
            self.path_b.run(b, &knobs)
        } else {
            (b, 0.0)
        };
        let (pl, pr, added) = match mode {
            StereoMode::Stereo => (pa, pb, (added_a * added_a + added_b * added_b) * 0.5),
            StereoMode::Mid | StereoMode::Side => {
                (pa + pb, pa - pb, (added_a + added_b) * (added_a + added_b))
            }
        };

        // Auto gain: hold the output's level at the input's.
        let coeff = self.auto_coeff;
        let in_ms = (left * left + right * right) * 0.5;
        let out_ms = (pl * pl + pr * pr) * 0.5;
        self.added_ms = flush_denormal(
            self.meters.rms_coeff * self.added_ms + (1.0 - self.meters.rms_coeff) * added,
        );
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
        let mut out_l = blend(left, pl * gain, on);
        let mut out_r = blend(right, pr * gain, on);
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

    fn flat() -> Params {
        Params {
            air_db: 0.0,
            excite: 0.0,
            deess: 0.0,
            auto_gain: false,
            ..default_params()
        }
    }

    /// RMS of the left output over `measure` samples after `settle` samples
    /// of `signal`.
    fn rms_after(
        dsp: &mut Dsp,
        signal: impl Fn(usize) -> f32,
        settle: usize,
        measure: usize,
    ) -> f32 {
        let mut sum = 0.0f64;
        for n in 0..settle + measure {
            let x = signal(n);
            let (l, r) = dsp.process_stereo(x, x);
            assert!(l.is_finite() && r.is_finite());
            if n >= settle {
                sum += f64::from(l) * f64::from(l);
            }
        }
        (sum / measure as f64).sqrt() as f32
    }

    fn rms_db(params: Params, signal: impl Fn(usize) -> f32) -> f32 {
        let mut dsp = Dsp::new(SR);
        dsp.set_params(params);
        linear_to_db(rms_after(&mut dsp, signal, 9_600, 9_600))
    }

    #[test]
    fn the_air_shelf_lifts_the_top_by_its_gain_and_leaves_the_bottom() {
        let mut params = flat();
        params.air_db = 6.0;
        params.air_freq = AirFreq::K5;
        let top =
            rms_db(params.clone(), |n| sine(15_000.0, 0.1, n)) - linear_to_db(0.1 / 2f32.sqrt());
        let bottom =
            rms_db(params.clone(), |n| sine(200.0, 0.1, n)) - linear_to_db(0.1 / 2f32.sqrt());
        assert!((top - 6.0).abs() < 0.6, "top {top}");
        assert!(bottom.abs() < 0.3, "bottom {bottom}");
        // The drawn curve says the same.
        assert!((eq_response_db(&params, 15_000.0, SR) - top).abs() < 0.3);
    }

    #[test]
    fn a_bell_and_presence_land_on_their_centres() {
        let mut params = flat();
        params.shape = Shape::Bell;
        params.air_db = 9.0;
        params.air_freq = AirFreq::K8;
        assert!((eq_response_db(&params, 8_000.0, SR) - 9.0).abs() < 0.1);
        params.air_db = 0.0;
        params.presence_db = -4.0;
        assert!((eq_response_db(&params, 3_500.0, SR) + 4.0).abs() < 0.1);
        params.air_db = 9.0;
        assert!(eq_response_db(&params, 100.0, SR).abs() < 0.1);
        // 40 kHz at 48 kHz is a lift of the top octave, not nothing.
        params.shape = Shape::Shelf;
        params.presence_db = 0.0;
        params.air_freq = AirFreq::K40;
        let top = eq_response_db(&params, 20_000.0, SR);
        assert!(top > 1.0 && top < 9.0, "40k shelf at 20 kHz: {top}");
        assert!(eq_response_db(&params, 5_000.0, SR) < 0.5);
        // Half mix: half the dB, near enough.
        params.air_freq = AirFreq::K5;
        params.mix = 50.0;
        let half = eq_response_db(&params, 15_000.0, SR);
        assert!(half > 3.0 && half < 6.0, "half {half}");
    }

    #[test]
    fn excite_adds_energy_above_the_corner() {
        // Two high tones: the exciter adds their harmonics and products.
        let signal = |n: usize| sine(7_000.0, 0.2, n) + sine(250.0, 0.2, n);
        let mut params = flat();
        params.air_freq = AirFreq::K12; // corner 6 kHz
        let measure_hf = |params: Params| {
            let mut dsp = Dsp::new(SR);
            dsp.set_params(params);
            let mut hp = DirectForm1::<f32>::new(
                make_eq_coefficients("highpass", 10_000.0, 0.0, 0.707, SR).unwrap(),
            );
            let mut hp2 = DirectForm1::<f32>::new(
                make_eq_coefficients("highpass", 10_000.0, 0.0, 0.707, SR).unwrap(),
            );
            let mut sum = 0.0f64;
            for n in 0..19_200 {
                let x = signal(n);
                let (l, _) = dsp.process_stereo(x, x);
                let h = hp2.run(hp.run(l));
                if n >= 9_600 {
                    sum += f64::from(h) * f64::from(h);
                }
            }
            (sum / 9_600.0).sqrt() as f32
        };
        let dry = measure_hf(params.clone());
        params.excite = 100.0;
        let excited = measure_hf(params.clone());
        assert!(
            // +8 dB or more above 10 kHz, where the dry signal has only the
            // 7 kHz tone's filter skirt.
            excited > dry * 2.5,
            "excite added little above 10 kHz: {dry} → {excited}"
        );
        let mut dsp = Dsp::new(SR);
        dsp.set_params(params);
        let _ = rms_after(&mut dsp, signal, 0, 9_600);
        let db = dsp.telemetry()[telemetry::EXCITE_DB];
        assert!(db > -40.0 && db < 0.0, "excite telemetry {db}");
    }

    #[test]
    fn the_de_esser_cuts_a_sibilant_burst_and_spares_a_vowel() {
        let noise = |n: usize| {
            let mut x = (n as u32).wrapping_mul(2_654_435_761) ^ 0x9e37_79b9;
            x ^= x >> 15;
            x = x.wrapping_mul(0x2c1b_3c6d);
            x ^= x >> 12;
            (x as f32 / u32::MAX as f32) * 2.0 - 1.0
        };
        let mut params = flat();
        params.deess = 100.0;
        // A vowel: low harmonics only.
        let vowel = |n: usize| sine(220.0, 0.3, n) + sine(660.0, 0.15, n) + sine(1_100.0, 0.08, n);
        let mut dsp = Dsp::new(SR);
        dsp.set_params(params.clone());
        let _ = rms_after(&mut dsp, vowel, 4_800, 4_800);
        let vowel_cut = dsp.deess_db();
        // A sibilant: band-limited noise around 7 kHz.
        let mut bp = DirectForm1::<f32>::new(
            make_eq_coefficients("bandpass", 7_000.0, 0.0, 1.0, SR).unwrap(),
        );
        let sibilant: Vec<f32> = (0..9_600).map(|n| bp.run(noise(n)) * 0.6).collect();
        let mut dsp = Dsp::new(SR);
        dsp.set_params(params);
        let mut peak_cut = 0.0f32;
        for &x in &sibilant {
            let _ = dsp.process_stereo(x, x);
            peak_cut = peak_cut.min(dsp.deess_db());
        }
        assert!(vowel_cut > -0.5, "vowel cut {vowel_cut}");
        assert!(peak_cut < -4.0, "sibilant cut {peak_cut}");
        assert!(dsp.meter_frame().gain_reduction_db > 4.0);
        assert!(dsp.telemetry()[telemetry::DEESS_DB] < -4.0);
        // And it lets go when the burst ends.
        for _ in 0..48_000 {
            let _ = dsp.process_stereo(0.0, 0.0);
        }
        assert_eq!(dsp.deess_db(), 0.0);
    }

    #[test]
    fn bypassed_it_passes_the_input_untouched() {
        let mut params = default_params();
        params.air_db = 12.0;
        params.excite = 100.0;
        params.power = false;
        let mut dsp = Dsp::new(SR);
        dsp.set_params(params);
        for n in 0..4_800 {
            let x = sine(9_000.0, 0.5, n);
            let (l, r) = dsp.process_stereo(x, -x);
            assert_eq!((l, r), (x, -x));
        }
        // Switching power fades rather than stepping.
        assert!(dsp.apply_ui_param("power", 1.0));
        let mut last = 0.0f32;
        let mut biggest = 0.0f32;
        for n in 0..960 {
            let (l, _) = dsp.process_stereo(0.25, 0.25);
            if n > 0 {
                biggest = biggest.max((l - last).abs());
            }
            last = l;
        }
        assert!(biggest < 0.05, "power stepped by {biggest}");
    }

    #[test]
    fn mid_only_leaves_the_side_alone() {
        let mut params = flat();
        params.air_db = 12.0;
        params.air_freq = AirFreq::K5;
        params.stereo_mode = StereoMode::Mid;
        let mut dsp = Dsp::new(SR);
        dsp.set_params(params);
        // Pure side: L = −R.
        let mut sum = 0.0f64;
        for n in 0..9_600 {
            let x = sine(12_000.0, 0.2, n);
            let (l, r) = dsp.process_stereo(x, -x);
            assert!((l + r).abs() < 1.0e-4);
            if n >= 4_800 {
                sum += f64::from(l - x).powi(2);
            }
        }
        assert!(sum < 1.0e-6, "side changed: {sum}");
    }

    #[test]
    fn auto_gain_brings_the_level_back_toward_the_input() {
        let mut params = flat();
        params.air_db = 12.0;
        params.air_freq = AirFreq::K2_5;
        let signal = |n: usize| sine(8_000.0, 0.1, n);
        let boosted = rms_db(params.clone(), signal);
        params.auto_gain = true;
        let matched = {
            let mut dsp = Dsp::new(SR);
            dsp.set_params(params);
            let level = linear_to_db(rms_after(&mut dsp, signal, 96_000, 9_600));
            assert!(dsp.auto_gain_db() < -6.0);
            level
        };
        let input = linear_to_db(0.1 / 2f32.sqrt());
        assert!(boosted - input > 10.0);
        assert!(
            (matched - input).abs() < 1.5,
            "matched {matched} vs {input}"
        );
    }

    #[test]
    fn every_setting_stays_finite_while_moving() {
        let mut dsp = Dsp::new(44_100.0);
        let mut n = 0usize;
        for freq in AirFreq::ALL {
            for shape in [Shape::Shelf, Shape::Bell] {
                for mode in [StereoMode::Stereo, StereoMode::Mid, StereoMode::Side] {
                    dsp.apply_ui_param("airFreq", freq.to_wire());
                    dsp.apply_ui_param("shape", shape.to_wire());
                    dsp.apply_ui_param("stereoMode", mode.to_wire());
                    dsp.apply_ui_param("airDb", 12.0);
                    dsp.apply_ui_param("excite", 100.0);
                    dsp.apply_ui_param("deess", 100.0);
                    for _ in 0..2_000 {
                        let x = sine(6_000.0, 0.9, n) + sine(90.0, 0.5, n);
                        let (l, r) = dsp.process_stereo(x, x * 0.3);
                        assert!(l.is_finite() && r.is_finite() && l.abs() < 8.0);
                        n += 1;
                    }
                }
            }
        }
        for _ in 0..48_000 {
            let (l, r) = dsp.process_stereo(0.0, 0.0);
            assert!(l.is_finite() && r.is_finite());
        }
        let (l, r) = dsp.process_stereo(0.0, 0.0);
        assert!(l.abs() < 1.0e-9 && r.abs() < 1.0e-9);
    }

    #[test]
    fn a_reset_and_a_rate_change_keep_it_working() {
        let mut dsp = Dsp::new(48_000.0);
        dsp.set_sample_rate(96_000.0);
        dsp.reset();
        let level = rms_after(&mut dsp, |n| sine(1_000.0, 0.3, n), 4_800, 4_800);
        assert!(level > 0.1);
        assert_eq!(dsp.latency_samples(), 0);
    }
}

//! VerbSpace — algorithmic stereo reverb.
//!
//! Engine-agnostic like the other `BuiltinAudioPlugins` cores. The signal path
//! is
//!
//! ```txt
//! pre-delay ─┬─ early reflections (12 taps a side, spread ∝ Size) ─ early diffusion ─── early ─┐
//!            └─ (input + reflections) ─ input diffusion (4 allpasses a side, ∝ Size)      Early/Late
//!                 ─ 16-line FDN (Hadamard feedback, modulated reads, an allpass and a ── late ─┘
//!                   two-shelf absorption filter in every line; lengths ∝ Size)
//!                                                  ─ low cut / high cut ─ width ─ output ─ mix
//! ```
//!
//! Every control does one thing, and does it by a large, predictable amount:
//!
//! * **Decay** is the RT60 of the middle band, exactly: every line's
//!   absorption is designed from its own length. **Bass** multiplies it below
//!   **Bass Freq**, **Damping** divides it (down to a tenth) above **Damp
//!   Freq**. [`decay_profile`] is computed from the very coefficients the
//!   tank plays, so the editor's decay display is the decay that sounds.
//! * **Size** scales the room: tank lines from 5–16 ms (a booth) to
//!   58–180 ms (a cathedral), the early pattern from 5 to 140 ms, and the
//!   input diffusers with them. It does not touch the decay time.
//! * The tail's level is normalised by its expected energy, so stretching
//!   Decay or Size changes how long the room rings, not how loud it is.
//! * **Mode** is a label: picking one in an editor loads that space type's
//!   starting point ([`ReverbMode::starting_point`]) into the ordinary
//!   params. The DSP never reads it, so it can never override a knob.
//!
//! Every buffer is sized at construction for the widest reachable setting, so
//! a parameter edit only moves targets: line lengths, the pre-delay, the
//! early pattern and the diffusers glide to theirs and every gain and filter
//! corner is smoothed, so nothing a user does on a running tail clicks. No
//! realtime allocation.

use std::f32::consts::{FRAC_1_SQRT_2, FRAC_PI_2, PI, TAU};

use biquad::{Biquad, Coefficients, DirectForm1};
use builtin_dsp_core::delay::{Allpass, DelayRing, Smoothed, smoothing_step};
use builtin_dsp_core::{
    ParamDescriptor, PluginCategory, PluginDescriptor, StereoEffect, biquad_response_db, clamp,
    db_to_linear, flush_denormal, make_eq_coefficients,
};
use serde::{Deserialize, Serialize};

pub mod ipc;
#[cfg(test)]
mod measure;
pub mod presets;
pub mod ui;

/// Editor-facing parameter id table, re-exported at the crate root so the host
/// resolves ids the same way for every built-in (`<plugin>::ui_param_index`).
pub use ipc::{UI_PARAM_IDS, ui_param_id, ui_param_index};
pub use presets::{FactoryPreset, factory_presets};

pub const PLUGIN_ID: &str = "futureboard.verbspace";

/// Delay lines in the tank. Sixteen under a Hadamard matrix give a tail dense
/// enough for a hall without audible flutter, and the fast transform keeps
/// the mixing at 64 adds.
pub const LINE_COUNT: usize = 16;

/// Allpass diffusers per channel ahead of the tank.
pub const DIFFUSER_COUNT: usize = 4;

/// Allpasses per channel smearing the early reflections.
const EARLY_DIFFUSER_COUNT: usize = 2;

/// Early reflections per side.
pub const EARLY_TAPS: usize = 12;

/// Longest pre-delay the ring is sized for.
pub const MAX_PREDELAY_MS: f32 = 500.0;

/// The longest tank line at `size` 0 % and 100 %; the rest scale between
/// them exponentially, so every step of the knob is the same ratio.
pub const MIN_LONGEST_LINE_MS: f32 = 16.0;
pub const MAX_LONGEST_LINE_MS: f32 = 180.0;
/// The shortest line as a share of the longest.
const SHORTEST_LINE_SHARE: f32 = 0.32;

/// Arrival of the last early reflection at `size` 0 % and 100 %.
pub const MIN_EARLY_SPAN_MS: f32 = 5.0;
pub const MAX_EARLY_SPAN_MS: f32 = 140.0;

/// Input-diffuser length multiplier at `size` 0 % and 100 %.
const MIN_DIFFUSER_SCALE: f32 = 0.4;
const MAX_DIFFUSER_SCALE: f32 = 1.6;

/// Peak excursion of a tank read at `modDepth` 100 %. A read swings between
/// its line length and twice this past it.
pub const MAX_MOD_MS: f32 = 1.2;

/// Allpass gains at `diffusion` 100 %; 0 % is a plain delay everywhere.
const MAX_INPUT_DIFFUSION: f32 = 0.75;
const MAX_EARLY_DIFFUSION: f32 = 0.7;
const MAX_TANK_DIFFUSION: f32 = 0.6;

/// How fast a line, the pre-delay, the early pattern or the diffusers
/// travel toward a new length: a time constant, so a Size drag bends the
/// running tail instead of cutting it.
const GLIDE_MS: f32 = 70.0;

/// Time constant of every smoothed gain and filter corner.
const SMOOTH_MS: f32 = 25.0;

/// Loop gain while frozen. Not exactly one: a lossless loop is only
/// marginally stable in floating point, and this still loses well under
/// 0.1 dB a second.
const FREEZE_GAIN: f32 = 0.99995;

/// RT60 ceiling per band: `decaySec` tops out at 20 s, and `bassMult` can
/// triple that below the bass crossover.
const MAX_RT60_SEC: f32 = 60.0;

/// Top-band decay as a share of the middle band's at `damping` 100 %. The
/// share falls geometrically with the knob: 50 % is a third of the decay.
pub const MIN_HIGH_RATIO: f32 = 0.1;

/// Late tail into the output: with the energy normalisation this leaves the
/// tank's tail at the early pattern's level (unit energy), so Early/Late
/// trades one for the other without a level change.
const LATE_OUTPUT: f32 = 1.0;

/// Wet-path trim. With the default cuts the wet signal sits about 3–4 dB
/// under a broadband input at 100 % wet, whatever the room or its decay.
const WET_TRIM_DB: f32 = 0.0;

/// Space type. A label and a starting point, never a hidden voicing: picking
/// one loads [`ReverbMode::starting_point`]; the DSP does not read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReverbMode {
    Room,
    Chamber,
    Hall,
    Plate,
    Ambience,
}

impl ReverbMode {
    pub const ALL: [Self; 5] = [
        Self::Room,
        Self::Chamber,
        Self::Hall,
        Self::Plate,
        Self::Ambience,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Room => "room",
            Self::Chamber => "chamber",
            Self::Hall => "hall",
            Self::Plate => "plate",
            Self::Ambience => "ambience",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Room => "Room",
            Self::Chamber => "Chamber",
            Self::Hall => "Hall",
            Self::Plate => "Plate",
            Self::Ambience => "Ambience",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "room" => Some(Self::Room),
            "chamber" => Some(Self::Chamber),
            "hall" => Some(Self::Hall),
            "plate" => Some(Self::Plate),
            "ambience" | "ambient" => Some(Self::Ambience),
            _ => None,
        }
    }

    pub const fn to_wire(self) -> f32 {
        match self {
            Self::Room => 0.0,
            Self::Chamber => 1.0,
            Self::Hall => 2.0,
            Self::Plate => 3.0,
            Self::Ambience => 4.0,
        }
    }

    pub fn from_wire(value: f32) -> Self {
        match value.round() as i32 {
            0 => Self::Room,
            1 => Self::Chamber,
            3 => Self::Plate,
            4 => Self::Ambience,
            _ => Self::Hall,
        }
    }

    /// The type's space, as the values it puts on the ordinary knobs:
    /// `(predelayMs, size, decaySec, diffusion, damping, bassMult,
    /// bassFreqHz, dampFreqHz, earlyLate, modDepth, modRateHz)`.
    pub const fn space(self) -> [f32; 11] {
        match self {
            Self::Room => [
                4.0, 22.0, 0.7, 70.0, 45.0, 1.0, 250.0, 4_500.0, 40.0, 12.0, 0.6,
            ],
            Self::Chamber => [
                10.0, 40.0, 1.4, 85.0, 35.0, 1.1, 250.0, 5_000.0, 55.0, 20.0, 0.7,
            ],
            Self::Hall => [
                20.0, 60.0, 2.4, 80.0, 40.0, 1.2, 250.0, 4_000.0, 65.0, 25.0, 0.7,
            ],
            // A plate is a sheet, not a room: no discrete reflections, the
            // densest build-up, and a top end that rings longest.
            Self::Plate => [
                6.0, 35.0, 1.8, 95.0, 15.0, 0.8, 400.0, 8_000.0, 100.0, 30.0, 1.0,
            ],
            Self::Ambience => [
                0.0, 12.0, 0.5, 60.0, 50.0, 0.9, 250.0, 5_000.0, 25.0, 10.0, 0.5,
            ],
        }
    }

    /// `current` with this type's space loaded: the space knobs move to the
    /// type's values, the mode label follows, and everything that is not the
    /// room — cuts, width, mix, output, wet-only, freeze, power — stays.
    pub fn starting_point(self, current: &Params) -> Params {
        let [
            predelay_ms,
            size,
            decay_sec,
            diffusion,
            damping,
            bass_mult,
            bass_freq_hz,
            damp_freq_hz,
            early_late,
            mod_depth,
            mod_rate_hz,
        ] = self.space();
        Params {
            mode: self,
            predelay_ms,
            size,
            decay_sec,
            diffusion,
            damping,
            bass_mult,
            bass_freq_hz,
            damp_freq_hz,
            early_late,
            mod_depth,
            mod_rate_hz,
            ..current.clone()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Params {
    pub power: bool,
    /// The space type last loaded as a starting point. A label only.
    pub mode: ReverbMode,
    /// Silence before the reflections start, in milliseconds.
    pub predelay_ms: f32,
    /// Room scale, in percent: tank lines, early pattern and diffusers.
    pub size: f32,
    /// RT60 of the middle band, in seconds.
    pub decay_sec: f32,
    /// Input, early and in-tank allpass amount, in percent.
    pub diffusion: f32,
    /// How much shorter the top band decays, in percent.
    pub damping: f32,
    /// Decay multiplier below [`Params::bass_freq_hz`].
    pub bass_mult: f32,
    /// Tank read modulation depth, in percent of [`MAX_MOD_MS`].
    pub mod_depth: f32,
    /// Tank read modulation rate, in hertz.
    pub mod_rate_hz: f32,
    /// Wet-path high-pass corner, in hertz.
    pub low_cut_hz: f32,
    /// Wet-path low-pass corner, in hertz.
    pub high_cut_hz: f32,
    /// Wet-path stereo width, in percent (100 = unchanged).
    pub width: f32,
    /// Dry/wet balance, in percent, on an equal-power law.
    pub mix: f32,
    /// Wet-path trim, in decibels.
    pub output_db: f32,
    /// Hold the tail: loop pinned just under unity, input muted, absorption
    /// bypassed.
    pub freeze: bool,
    /// Crossover under which `bass_mult` scales the decay, in hertz.
    pub bass_freq_hz: f32,
    /// Crossover above which `damping` shortens the decay, in hertz.
    pub damp_freq_hz: f32,
    /// Early reflections against the late tail, in percent: 0 is early only,
    /// 100 late only, on an equal-power law.
    pub early_late: f32,
    /// No dry signal and the wet at full level, whatever `mix` says: for a
    /// send/return bus.
    pub wet_only: bool,
}

impl Default for Params {
    fn default() -> Self {
        default_params()
    }
}

pub fn default_params() -> Params {
    let base = Params {
        power: true,
        mode: ReverbMode::Hall,
        predelay_ms: 0.0,
        size: 0.0,
        decay_sec: 0.0,
        diffusion: 0.0,
        damping: 0.0,
        bass_mult: 0.0,
        mod_depth: 0.0,
        mod_rate_hz: 0.0,
        low_cut_hz: 80.0,
        high_cut_hz: 12_000.0,
        width: 100.0,
        mix: 30.0,
        output_db: 0.0,
        freeze: false,
        bass_freq_hz: 0.0,
        damp_freq_hz: 0.0,
        early_late: 0.0,
        wet_only: false,
    };
    ReverbMode::Hall.starting_point(&base)
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

/// The parameter table. Defaults are the Hall starting point, as
/// [`default_params`]; a test holds the two together.
pub fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PLUGIN_ID,
        name: "VerbSpace",
        vendor: "Futureboard",
        category: PluginCategory::Effect,
        version: env!("CARGO_PKG_VERSION"),
        params: PARAMS,
    }
}

static PARAMS: &[ParamDescriptor] = &[
    param("power", "Power", 1.0, 0.0, 1.0, "bool"),
    param("mode", "Mode", 2.0, 0.0, 4.0, "enum"),
    param("predelayMs", "Pre-Delay", 20.0, 0.0, MAX_PREDELAY_MS, "ms"),
    param("size", "Size", 60.0, 0.0, 100.0, "%"),
    param("decaySec", "Decay", 2.4, 0.1, 20.0, "s"),
    param("diffusion", "Diffusion", 80.0, 0.0, 100.0, "%"),
    param("damping", "Damping", 40.0, 0.0, 100.0, "%"),
    param("bassMult", "Bass", 1.2, 0.2, 3.0, "x"),
    param("modDepth", "Mod Depth", 25.0, 0.0, 100.0, "%"),
    param("modRateHz", "Mod Rate", 0.7, 0.05, 5.0, "Hz"),
    param("lowCutHz", "Low Cut", 80.0, 20.0, 1_000.0, "Hz"),
    param("highCutHz", "High Cut", 12_000.0, 1_000.0, 20_000.0, "Hz"),
    param("width", "Width", 100.0, 0.0, 200.0, "%"),
    param("mix", "Mix", 30.0, 0.0, 100.0, "%"),
    param("outputDb", "Output", 0.0, -24.0, 12.0, "dB"),
    param("freeze", "Freeze", 0.0, 0.0, 1.0, "bool"),
    param("bassFreqHz", "Bass Freq", 250.0, 50.0, 1_000.0, "Hz"),
    param("dampFreqHz", "Damp Freq", 4_000.0, 1_000.0, 16_000.0, "Hz"),
    param("earlyLate", "Early/Late", 65.0, 0.0, 100.0, "%"),
    param("wetOnly", "Wet Only", 0.0, 0.0, 1.0, "bool"),
];

// ── Fixed tables ─────────────────────────────────────────────────────────────

/// Where each line sits between the shortest and longest length, before the
/// permutation below: an even spread nudged off the grid, so no two lines
/// share a simple ratio and the modes never stack into a comb.
const LINE_SPREAD_JITTER: [f32; LINE_COUNT] = [
    0.0, 0.21, -0.17, 0.31, -0.08, 0.27, -0.29, 0.12, -0.22, 0.33, -0.11, 0.19, -0.31, 0.07, -0.24,
    0.0,
];

/// Which line takes the n-th shortest length. Neighbouring lengths land far
/// apart in the Hadamard butterflies.
const LINE_ORDER: [usize; LINE_COUNT] = [0, 9, 3, 12, 6, 15, 1, 10, 4, 13, 7, 2, 11, 5, 14, 8];

/// The allpass inside each line, in milliseconds. Fixed, not scaled by size.
const TANK_ALLPASS_MS: [f32; LINE_COUNT] = [
    0.61, 0.73, 0.89, 0.97, 1.13, 1.27, 1.39, 1.51, 1.67, 1.79, 1.93, 2.11, 2.29, 2.41, 2.63, 2.87,
];

/// Input diffusers at a diffuser scale of 1, offset between the channels so
/// the two decorrelate before they reach the tank.
const DIFFUSER_MS_L: [f32; DIFFUSER_COUNT] = [1.9, 3.1, 4.7, 7.3];
const DIFFUSER_MS_R: [f32; DIFFUSER_COUNT] = [2.2, 3.5, 5.3, 7.9];

/// The allpasses that smear the early reflections.
const EARLY_DIFFUSER_MS_L: [f32; EARLY_DIFFUSER_COUNT] = [1.37, 2.71];
const EARLY_DIFFUSER_MS_R: [f32; EARLY_DIFFUSER_COUNT] = [1.61, 3.03];

/// Every line's LFO runs at the rate times its own factor, so the tank never
/// sweeps in step.
const MOD_RATE_SPREAD: [f32; LINE_COUNT] = [
    1.00, 0.83, 1.17, 0.71, 1.29, 0.91, 1.07, 0.77, 1.37, 0.87, 1.11, 0.74, 1.23, 0.95, 1.31, 0.79,
];
const MOD_DEPTH_SPREAD: [f32; LINE_COUNT] = [
    1.00, 1.18, 0.86, 1.24, 0.79, 1.09, 0.93, 1.21, 0.82, 1.13, 0.90, 1.25, 0.76, 1.05, 0.97, 1.16,
];

/// Arrival of each early reflection as a share of the pattern's span. The
/// two sides interleave so they never coincide.
const EARLY_AT_L: [f32; EARLY_TAPS] = [
    0.043, 0.087, 0.131, 0.179, 0.233, 0.297, 0.367, 0.443, 0.531, 0.629, 0.743, 0.887,
];
const EARLY_AT_R: [f32; EARLY_TAPS] = [
    0.051, 0.097, 0.149, 0.199, 0.257, 0.319, 0.389, 0.471, 0.557, 0.661, 0.781, 0.937,
];
/// Polarity of each reflection; mixed signs keep the pattern from summing
/// into a comb.
const EARLY_SIGN: [f32; EARLY_TAPS] = [
    1.0, -1.0, 1.0, 1.0, -1.0, 1.0, -1.0, -1.0, 1.0, -1.0, 1.0, -1.0,
];

/// Sign scrambles applied to Walsh rows to make the tank's input and output
/// vectors. Scrambling keeps two rows orthogonal (so left and right stay
/// decorrelated) but stops them being rows of the feedback matrix itself,
/// which would hand an injection back to a single line one pass later.
const SCRAMBLE_IN: [f32; LINE_COUNT] = [
    1.0, 1.0, 1.0, -1.0, 1.0, 1.0, -1.0, 1.0, 1.0, -1.0, -1.0, 1.0, -1.0, 1.0, -1.0, -1.0,
];
const SCRAMBLE_OUT: [f32; LINE_COUNT] = [
    1.0, -1.0, 1.0, 1.0, 1.0, -1.0, -1.0, 1.0, -1.0, 1.0, 1.0, 1.0, -1.0, -1.0, 1.0, -1.0,
];

/// Entry `(row, column)` of the 16×16 Sylvester–Hadamard matrix.
const fn walsh(row: usize, column: usize) -> f32 {
    if (row & column).count_ones() % 2 == 0 {
        1.0
    } else {
        -1.0
    }
}

const fn signed_row(row: usize, scramble: &[f32; LINE_COUNT]) -> [f32; LINE_COUNT] {
    let mut out = [0.0; LINE_COUNT];
    let mut i = 0;
    while i < LINE_COUNT {
        out[i] = walsh(row, i) * scramble[i];
        i += 1;
    }
    out
}

const INPUT_L: [f32; LINE_COUNT] = signed_row(5, &SCRAMBLE_IN);
const INPUT_R: [f32; LINE_COUNT] = signed_row(10, &SCRAMBLE_IN);
const OUTPUT_L: [f32; LINE_COUNT] = signed_row(3, &SCRAMBLE_OUT);
const OUTPUT_R: [f32; LINE_COUNT] = signed_row(12, &SCRAMBLE_OUT);

/// In-place fast Walsh–Hadamard transform, scaled to be orthonormal: the
/// feedback matrix neither adds nor removes energy, so decay comes from the
/// absorption filters alone.
#[inline]
fn hadamard(v: &mut [f32; LINE_COUNT]) {
    let mut half = 1;
    while half < LINE_COUNT {
        let mut start = 0;
        while start < LINE_COUNT {
            for i in start..start + half {
                let (a, b) = (v[i], v[i + half]);
                v[i] = a + b;
                v[i + half] = a - b;
            }
            start += half * 2;
        }
        half *= 2;
    }
    for x in v.iter_mut() {
        *x *= 0.25;
    }
}

// ── The space curves: one place, read by the DSP and the editors ─────────────

/// `size` 0–100 % onto 0–1.
fn space(size: f32) -> f32 {
    clamp(size / 100.0, 0.0, 1.0)
}

/// Exponential travel from `min` at 0 to `max` at 1.
fn sweep(min: f32, max: f32, at: f32) -> f32 {
    min * (max / min).powf(at)
}

/// The longest tank line at `size`, in milliseconds.
pub fn longest_line_ms(size: f32) -> f32 {
    sweep(MIN_LONGEST_LINE_MS, MAX_LONGEST_LINE_MS, space(size))
}

/// Arrival of the last early reflection at `size`, in milliseconds.
pub fn early_span_ms(size: f32) -> f32 {
    sweep(MIN_EARLY_SPAN_MS, MAX_EARLY_SPAN_MS, space(size))
}

fn diffuser_scale(size: f32) -> f32 {
    sweep(MIN_DIFFUSER_SCALE, MAX_DIFFUSER_SCALE, space(size))
}

/// `damping` 0–100 % onto the top band's share of the middle band's decay.
pub fn high_ratio(damping: f32) -> f32 {
    MIN_HIGH_RATIO.powf(clamp(damping / 100.0, 0.0, 1.0))
}

/// `earlyLate` onto the `(early, late)` output gains, equal power.
pub fn early_late_gains(early_late: f32) -> (f32, f32) {
    let b = clamp(early_late / 100.0, 0.0, 1.0);
    if b >= 1.0 {
        (0.0, 1.0)
    } else if b <= 0.0 {
        (1.0, 0.0)
    } else {
        ((b * FRAC_PI_2).cos(), (b * FRAC_PI_2).sin())
    }
}

// ── Tuning: every derived value, from one place ─────────────────────────────

/// Bilinear one-pole low-pass `y = b (x + x₁) + a y₁`: unity at DC, an exact
/// zero at Nyquist, monotonic between — which is what lets a shelf built
/// from it hit both its end gains exactly. `b` is always `(1 − a) / 2`.
#[derive(Debug, Clone, Copy, PartialEq)]
struct OnePole {
    a: f32,
    b: f32,
}

impl OnePole {
    fn a_for(corner_hz: f32, sample_rate: f32) -> f32 {
        let corner = clamp(corner_hz, 1.0, sample_rate * 0.45);
        let k = (PI * corner / sample_rate).tan();
        (1.0 - k) / (1.0 + k)
    }

    #[inline]
    fn from_a(a: f32) -> Self {
        Self {
            a,
            b: (1.0 - a) * 0.5,
        }
    }

    /// Complex response at `hz`, as `(re, im)`.
    fn response(self, hz: f32, sample_rate: f32) -> (f64, f64) {
        // b (1 + e^-jw) / (1 - a e^-jw).
        let w = std::f64::consts::TAU * f64::from(hz) / f64::from(sample_rate);
        let (a, b) = (f64::from(self.a), f64::from(self.b));
        let (num_re, num_im) = (b * (1.0 + w.cos()), -b * w.sin());
        let (den_re, den_im) = (1.0 - a * w.cos(), a * w.sin());
        let den = den_re * den_re + den_im * den_im;
        (
            (num_re * den_re + num_im * den_im) / den,
            (num_im * den_re - num_re * den_im) / den,
        )
    }
}

/// One line's per-pass loss: `gain × low shelf × high shelf`. The low shelf
/// runs from `low` (DC) to 1, the high shelf from 1 to `high` (Nyquist), each
/// a ratio against the middle band's `gain`.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Absorption {
    gain: f32,
    low: f32,
    high: f32,
}

impl Absorption {
    /// Loss per pass for a loop of `loop_samples` under three RT60s.
    fn for_loop(loop_samples: f32, sample_rate: f32, rt: [f32; 3]) -> Self {
        let per_pass = |rt60: f32| 10.0f32.powf(-3.0 * loop_samples / (sample_rate * rt60));
        let [low, mid, high] = rt.map(per_pass);
        Self {
            gain: mid,
            low: low / mid,
            high: high / mid,
        }
    }

    const FROZEN: Self = Self {
        gain: FREEZE_GAIN,
        low: 1.0,
        high: 1.0,
    };

    /// Magnitude of the loss at `hz`.
    fn magnitude(self, low: OnePole, high: OnePole, hz: f32, sample_rate: f32) -> f64 {
        let (lr, li) = low.response(hz, sample_rate);
        let (hr, hi) = high.response(hz, sample_rate);
        // Low shelf 1 + (low − 1)·LP, high shelf high + (1 − high)·LP.
        let low_shelf = (
            1.0 + (f64::from(self.low) - 1.0) * lr,
            (f64::from(self.low) - 1.0) * li,
        );
        let high_shelf = (
            f64::from(self.high) + (1.0 - f64::from(self.high)) * hr,
            (1.0 - f64::from(self.high)) * hi,
        );
        f64::from(self.gain) * low_shelf.0.hypot(low_shelf.1) * high_shelf.0.hypot(high_shelf.1)
    }
}

/// Everything the audio path reads, resolved from `Params` at one sample
/// rate. Computed off the per-sample path, on every edit, without
/// allocating; [`decay_profile`] reads the same values the tank plays.
#[derive(Debug, Clone, Copy)]
struct Tuning {
    predelay: f32,
    /// Tank line lengths, in samples.
    lines: [f32; LINE_COUNT],
    /// Line plus its allpass: one trip round the loop.
    loops: [f32; LINE_COUNT],
    absorption: [Absorption; LINE_COUNT],
    /// The crossovers' one-pole coefficient `a`.
    low_split_a: f32,
    high_split_a: f32,
    rt: [f32; 3],
    input_diffusion: f32,
    early_diffusion: f32,
    tank_diffusion: f32,
    /// Span of the early pattern, in samples.
    early_span: f32,
    diffuser_scale: f32,
    mod_depth: f32,
    /// Per-line LFO rotation `(cos, sin)` per sample.
    mod_step: [(f32, f32); LINE_COUNT],
    /// Gain into the tank that takes back the energy a longer decay or a
    /// shorter loop piles up, so the tail plays at one level.
    late_norm: f32,
    input_gain: f32,
    early_gain: f32,
    late_gain: f32,
    mid_gain: f32,
    side_gain: f32,
    dry_gain: f32,
    wet_gain: f32,
}

impl Tuning {
    fn resolve(p: &Params, sample_rate: f32) -> Self {
        let sr = sample_rate;
        let ms = |ms: f32| ms * 0.001 * sr;

        let longest = longest_line_ms(p.size);
        let shortest = longest * SHORTEST_LINE_SHARE;
        let mut lines = [0.0; LINE_COUNT];
        for rank in 0..LINE_COUNT {
            let at = clamp(
                (rank as f32 + LINE_SPREAD_JITTER[rank]) / (LINE_COUNT - 1) as f32,
                0.0,
                1.0,
            );
            lines[LINE_ORDER[rank]] = ms(sweep(shortest, longest, at)).max(4.0);
        }
        let loops: [f32; LINE_COUNT] =
            std::array::from_fn(|i| lines[i] + allpass_len(TANK_ALLPASS_MS[i], sr) as f32);

        let rt_mid = clamp(p.decay_sec, 0.05, MAX_RT60_SEC);
        let rt_low = clamp(p.decay_sec * p.bass_mult, 0.05, MAX_RT60_SEC);
        let rt_high = clamp(rt_mid * high_ratio(p.damping), 0.02, MAX_RT60_SEC);
        let rt = [rt_low, rt_mid, rt_high];
        let decaying: [Absorption; LINE_COUNT] =
            std::array::from_fn(|i| Absorption::for_loop(loops[i], sr, rt));
        let absorption = if p.freeze {
            [Absorption::FROZEN; LINE_COUNT]
        } else {
            decaying
        };

        // A loop that keeps g² of its energy each pass holds 1 / (1 − g²) of
        // what went in. Taking that back leaves the tail at one level from a
        // 0.1 s booth to a 20 s cathedral. Read from the unfrozen decay, so
        // Freeze holds the level it caught.
        let kept = decaying.iter().map(|a| a.gain * a.gain).sum::<f32>() / LINE_COUNT as f32;
        let late_norm = (1.0 - kept).max(1.0e-5).sqrt();

        let diffusion = clamp(p.diffusion / 100.0, 0.0, 1.0);
        let rate = clamp(p.mod_rate_hz, 0.0, 20.0);
        let mod_step = std::array::from_fn(|i| {
            let w = TAU * rate * MOD_RATE_SPREAD[i] / sr;
            (w.cos(), w.sin())
        });

        let (early_gain, late_gain) = early_late_gains(p.early_late);
        let width = clamp(p.width / 100.0, 0.0, 2.0);
        // Width as a mid/side balance whose powers sum to a constant: 0 % is
        // mono, 100 % untouched, 200 % twice the side over a quieter mid —
        // wide, and it still folds down.
        let width_norm = (2.0 / (1.0 + width * width)).sqrt();
        let mix = clamp(p.mix / 100.0, 0.0, 1.0);
        let (dry_gain, wet_mix) = if p.wet_only {
            (0.0, 1.0)
        } else if mix >= 1.0 {
            (0.0, 1.0)
        } else {
            // Dry and wet are uncorrelated, so an equal-power law keeps the
            // loudness steady across the whole travel.
            ((mix * FRAC_PI_2).cos(), (mix * FRAC_PI_2).sin())
        };
        Self {
            predelay: ms(clamp(p.predelay_ms, 0.0, MAX_PREDELAY_MS)),
            lines,
            loops,
            absorption,
            low_split_a: OnePole::a_for(p.bass_freq_hz, sr),
            high_split_a: OnePole::a_for(p.damp_freq_hz, sr),
            rt,
            input_diffusion: diffusion * MAX_INPUT_DIFFUSION,
            early_diffusion: diffusion * MAX_EARLY_DIFFUSION,
            tank_diffusion: diffusion * MAX_TANK_DIFFUSION,
            early_span: ms(early_span_ms(p.size)),
            diffuser_scale: diffuser_scale(p.size),
            mod_depth: ms(clamp(p.mod_depth / 100.0, 0.0, 1.0) * MAX_MOD_MS),
            mod_step,
            late_norm,
            input_gain: if p.freeze { 0.0 } else { 1.0 },
            early_gain,
            late_gain,
            mid_gain: width_norm,
            side_gain: width * width_norm,
            dry_gain,
            wet_gain: wet_mix * db_to_linear(p.output_db + WET_TRIM_DB),
        }
    }
}

/// What [`Allpass::for_ms`] rounds `ms` to, without building one.
fn allpass_len(ms: f32, sample_rate: f32) -> usize {
    ((ms * 0.001 * sample_rate).round() as usize).max(1)
}

/// Gains of the early pattern: signed, falling with arrival, and normalised
/// to unit energy per side.
fn early_gains(at: &[f32; EARLY_TAPS]) -> [f32; EARLY_TAPS] {
    let raw: [f32; EARLY_TAPS] = std::array::from_fn(|i| (1.0 - 0.75 * at[i]) * EARLY_SIGN[i]);
    let norm = raw.iter().map(|g| g * g).sum::<f32>().sqrt();
    raw.map(|g| g / norm)
}

// ── What the editor draws ────────────────────────────────────────────────────

/// One early reflection, as the editor draws it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EarlyReflection {
    /// After the dry signal, pre-delay included.
    pub at_ms: f32,
    /// Signed linear gain into its side, Early/Late balance included.
    pub gain: f32,
    pub right: bool,
}

/// The reverb's shape for the current params — derived from the very
/// coefficients the tank runs, not from a separate model.
#[derive(Debug, Clone, Copy)]
pub struct DecayProfile {
    pub predelay_ms: f32,
    pub early: [EarlyReflection; EARLY_TAPS * 2],
    /// First arrival of the tail, after the pre-delay.
    pub first_late_ms: f32,
    /// Level the tail starts at, in dB, on the same scale as the early
    /// reflections' gains: the Early/Late balance's late share.
    pub late_db: f32,
    /// RT60 of the three bands; infinite while frozen.
    pub rt_low_sec: f32,
    pub rt_mid_sec: f32,
    pub rt_high_sec: f32,
    pub low_split_hz: f32,
    pub high_split_hz: f32,
    pub frozen: bool,
    sample_rate: f32,
    loop_samples: f32,
    absorption: Absorption,
    low_split: OnePole,
    high_split: OnePole,
}

impl DecayProfile {
    /// RT60 at `hz`, in seconds: the decay one trip round a typical line
    /// implies at that frequency. Infinite while frozen.
    pub fn rt_at(&self, hz: f32) -> f32 {
        if self.frozen {
            return f32::INFINITY;
        }
        let magnitude = self
            .absorption
            .magnitude(self.low_split, self.high_split, hz, self.sample_rate)
            .max(1.0e-12);
        let seconds = f64::from(self.loop_samples) / f64::from(self.sample_rate);
        (-3.0 * seconds / magnitude.log10()) as f32
    }

    /// Longest RT60 across the bands, for choosing a time axis.
    pub fn longest_sec(&self) -> f32 {
        self.rt_low_sec.max(self.rt_mid_sec).max(self.rt_high_sec)
    }
}

/// The decay [`Dsp`] plays for `params` at `sample_rate`.
pub fn decay_profile(params: &Params, sample_rate: f32) -> DecayProfile {
    let sr = sample_rate.max(1.0);
    let tuning = Tuning::resolve(params, sr);
    let to_ms = 1000.0 / sr;
    let gains_l = early_gains(&EARLY_AT_L);
    let gains_r = early_gains(&EARLY_AT_R);
    let early = std::array::from_fn(|i| {
        let (at, gain, right) = if i < EARLY_TAPS {
            (EARLY_AT_L[i], gains_l[i], false)
        } else {
            (EARLY_AT_R[i - EARLY_TAPS], gains_r[i - EARLY_TAPS], true)
        };
        EarlyReflection {
            at_ms: (tuning.predelay + at * tuning.early_span) * to_ms,
            gain: gain * tuning.early_gain,
            right,
        }
    });
    // The line whose loop is the median: what "a typical line" means above.
    let mut order: [usize; LINE_COUNT] = std::array::from_fn(|i| i);
    order.sort_by(|a, b| tuning.loops[*a].total_cmp(&tuning.loops[*b]));
    let typical = order[LINE_COUNT / 2];
    let frozen = params.freeze;
    let rt = |sec: f32| if frozen { f32::INFINITY } else { sec };
    DecayProfile {
        predelay_ms: tuning.predelay * to_ms,
        early,
        first_late_ms: tuning.lines.iter().copied().fold(f32::MAX, f32::min) * to_ms,
        late_db: 20.0 * tuning.late_gain.max(1.0e-6).log10(),
        rt_low_sec: rt(tuning.rt[0]),
        rt_mid_sec: rt(tuning.rt[1]),
        rt_high_sec: rt(tuning.rt[2]),
        low_split_hz: params.bass_freq_hz,
        high_split_hz: params.damp_freq_hz,
        frozen,
        sample_rate: sr,
        loop_samples: tuning.loops[typical],
        absorption: tuning.absorption[typical],
        low_split: OnePole::from_a(tuning.low_split_a),
        high_split: OnePole::from_a(tuning.high_split_a),
    }
}

/// Coefficients of the wet path's two cuts.
fn cut_coefficients(params: &Params, sample_rate: f32) -> [Option<Coefficients<f32>>; 2] {
    let guard = sample_rate * 0.45;
    [
        make_eq_coefficients(
            "highpass",
            clamp(params.low_cut_hz, 20.0, guard),
            0.0,
            0.707,
            sample_rate,
        ),
        make_eq_coefficients(
            "lowpass",
            clamp(params.high_cut_hz, 200.0, guard),
            0.0,
            0.707,
            sample_rate,
        ),
    ]
}

/// Level of the wet path's low and high cut at `hz`, in decibels.
pub fn wet_filter_response_db(params: &Params, hz: f32, sample_rate: f32) -> f32 {
    cut_coefficients(params, sample_rate)
        .iter()
        .flatten()
        .map(|c| biquad_response_db(c, hz, sample_rate))
        .sum()
}

// ── Building blocks ──────────────────────────────────────────────────────────

/// One tank line: its ring, the allpass inside it, its absorption states and
/// its LFO.
#[derive(Debug, Clone)]
struct Line {
    ring: DelayRing,
    allpass: Allpass,
    /// Length now, gliding toward `Tuning::lines`.
    delay: f32,
    /// Loss now, gliding toward `Tuning::absorption`: a gain that jumped
    /// inside the loop would step the running tail.
    loss: Absorption,
    low_x: f32,
    low_y: f32,
    high_x: f32,
    high_y: f32,
    lfo_sin: f32,
    lfo_cos: f32,
}

impl Line {
    fn clear(&mut self) {
        self.ring.clear();
        self.allpass.clear();
        self.low_x = 0.0;
        self.low_y = 0.0;
        self.high_x = 0.0;
        self.high_y = 0.0;
    }

    #[inline]
    fn glide_loss(&mut self, target: Absorption, step: f32) {
        self.loss.gain += (target.gain - self.loss.gain) * step;
        self.loss.low += (target.low - self.loss.low) * step;
        self.loss.high += (target.high - self.loss.high) * step;
    }

    #[inline]
    fn absorb(&mut self, x: f32, low: OnePole, high: OnePole) -> f32 {
        let loss = self.loss;
        let low_lp = low.b * (x + self.low_x) + low.a * self.low_y;
        self.low_x = x;
        self.low_y = flush_denormal(low_lp);
        let shelved = x + (loss.low - 1.0) * low_lp;
        let high_lp = high.b * (shelved + self.high_x) + high.a * self.high_y;
        self.high_x = shelved;
        self.high_y = flush_denormal(high_lp);
        loss.gain * (loss.high * shelved + (1.0 - loss.high) * high_lp)
    }
}

/// A Schroeder allpass whose length follows Size: a ring read at a gliding
/// fractional delay. Outside every feedback loop, so a length change only
/// bends the pitch of what passes for the moment it lasts.
#[derive(Debug, Clone)]
struct GlideAllpass {
    ring: DelayRing,
    /// Length at a diffuser scale of 1, in samples.
    base: f32,
}

impl GlideAllpass {
    fn new(ms: f32, sample_rate: f32) -> Self {
        Self {
            ring: DelayRing::for_ms(ms * MAX_DIFFUSER_SCALE + 1.0, sample_rate),
            base: ms * 0.001 * sample_rate,
        }
    }

    #[inline]
    fn process(&mut self, input: f32, gain: f32, scale: f32) -> f32 {
        let delayed = self.ring.read_linear(self.base * scale);
        let stored = flush_denormal(input + delayed * gain);
        self.ring.push(stored);
        delayed - stored * gain
    }
}

#[derive(Debug, Clone)]
pub struct Dsp {
    sample_rate: f32,
    params: Params,
    tuning: Tuning,

    predelay_l: DelayRing,
    predelay_r: DelayRing,
    early_l: DelayRing,
    early_r: DelayRing,
    early_diffusers_l: [Allpass; EARLY_DIFFUSER_COUNT],
    early_diffusers_r: [Allpass; EARLY_DIFFUSER_COUNT],
    diffusers_l: [GlideAllpass; DIFFUSER_COUNT],
    diffusers_r: [GlideAllpass; DIFFUSER_COUNT],
    lines: [Line; LINE_COUNT],
    early_gain_l: [f32; EARLY_TAPS],
    early_gain_r: [f32; EARLY_TAPS],
    low_cut_l: Option<DirectForm1<f32>>,
    low_cut_r: Option<DirectForm1<f32>>,
    high_cut_l: Option<DirectForm1<f32>>,
    high_cut_r: Option<DirectForm1<f32>>,

    glide_step: f32,
    smooth_step: f32,
    predelay: Smoothed,
    early_span: Smoothed,
    diffuser_scale: Smoothed,
    late_norm: Smoothed,
    input_gain: Smoothed,
    early_gain: Smoothed,
    late_gain: Smoothed,
    mid_gain: Smoothed,
    side_gain: Smoothed,
    dry_gain: Smoothed,
    wet_gain: Smoothed,
    input_diffusion: Smoothed,
    early_diffusion: Smoothed,
    tank_diffusion: Smoothed,
    low_split_a: Smoothed,
    high_split_a: Smoothed,
    /// Samples since the last LFO renormalisation.
    lfo_tick: u32,
    /// Nothing has played since construction or the last reset: there is no
    /// tail to glide on, so a retune lands at once.
    fresh: bool,
}

impl Dsp {
    pub fn new(sample_rate: f32) -> Self {
        let sr = sample_rate.max(1.0);
        let params = default_params();
        let tuning = Tuning::resolve(&params, sr);
        let mut dsp = Self {
            sample_rate: sr,
            predelay_l: DelayRing::for_ms(MAX_PREDELAY_MS, sr),
            predelay_r: DelayRing::for_ms(MAX_PREDELAY_MS, sr),
            early_l: DelayRing::for_ms(MAX_EARLY_SPAN_MS + 2.0, sr),
            early_r: DelayRing::for_ms(MAX_EARLY_SPAN_MS + 2.0, sr),
            early_diffusers_l: std::array::from_fn(|i| Allpass::for_ms(EARLY_DIFFUSER_MS_L[i], sr)),
            early_diffusers_r: std::array::from_fn(|i| Allpass::for_ms(EARLY_DIFFUSER_MS_R[i], sr)),
            diffusers_l: std::array::from_fn(|i| GlideAllpass::new(DIFFUSER_MS_L[i], sr)),
            diffusers_r: std::array::from_fn(|i| GlideAllpass::new(DIFFUSER_MS_R[i], sr)),
            lines: Self::build_lines(sr, &tuning),
            early_gain_l: early_gains(&EARLY_AT_L),
            early_gain_r: early_gains(&EARLY_AT_R),
            low_cut_l: None,
            low_cut_r: None,
            high_cut_l: None,
            high_cut_r: None,
            glide_step: smoothing_step(GLIDE_MS, sr),
            smooth_step: smoothing_step(SMOOTH_MS, sr),
            predelay: Smoothed::at(tuning.predelay),
            early_span: Smoothed::at(tuning.early_span),
            diffuser_scale: Smoothed::at(tuning.diffuser_scale),
            late_norm: Smoothed::at(tuning.late_norm),
            input_gain: Smoothed::at(tuning.input_gain),
            early_gain: Smoothed::at(tuning.early_gain),
            late_gain: Smoothed::at(tuning.late_gain),
            mid_gain: Smoothed::at(tuning.mid_gain),
            side_gain: Smoothed::at(tuning.side_gain),
            dry_gain: Smoothed::at(tuning.dry_gain),
            wet_gain: Smoothed::at(tuning.wet_gain),
            input_diffusion: Smoothed::at(tuning.input_diffusion),
            early_diffusion: Smoothed::at(tuning.early_diffusion),
            tank_diffusion: Smoothed::at(tuning.tank_diffusion),
            low_split_a: Smoothed::at(tuning.low_split_a),
            high_split_a: Smoothed::at(tuning.high_split_a),
            lfo_tick: 0,
            fresh: true,
            tuning,
            params,
        };
        dsp.rebuild_filters();
        dsp
    }

    /// The longest tank line Size reaches, plus the modulation's swing.
    fn max_line_ms() -> f32 {
        MAX_LONGEST_LINE_MS + MAX_MOD_MS * 2.5 + 2.0
    }

    fn build_lines(sample_rate: f32, tuning: &Tuning) -> [Line; LINE_COUNT] {
        let max_ms = Self::max_line_ms();
        std::array::from_fn(|i| {
            // LFOs start spread round the circle, so the lines never sweep
            // together.
            let phase = TAU * i as f32 / LINE_COUNT as f32;
            Line {
                ring: DelayRing::for_ms(max_ms, sample_rate),
                allpass: Allpass::for_ms(TANK_ALLPASS_MS[i], sample_rate),
                delay: tuning.lines[i],
                loss: tuning.absorption[i],
                low_x: 0.0,
                low_y: 0.0,
                high_x: 0.0,
                high_y: 0.0,
                lfo_sin: phase.sin(),
                lfo_cos: phase.cos(),
            }
        })
    }

    pub fn params(&self) -> &Params {
        &self.params
    }

    pub fn set_params(&mut self, params: Params) {
        let rebuild = params.low_cut_hz != self.params.low_cut_hz
            || params.high_cut_hz != self.params.high_cut_hz;
        self.params = params;
        ipc::sanitize_params(&mut self.params);
        self.retune();
        if rebuild || self.low_cut_l.is_none() {
            self.rebuild_filters();
        }
    }

    /// Apply a compact wire update already resolved by the UI/control thread.
    ///
    /// The audio path never parses JSON or looks up string parameter ids.
    /// Every arm below only re-resolves the tuning table or the two cut
    /// filters, so this stays allocation-free and safe to call from the
    /// producer thread between blocks.
    pub fn apply_wire_param(&mut self, wire_index: u32, value: f32) -> bool {
        if !ipc::apply_wire_param(&mut self.params, wire_index, value) {
            return false;
        }
        match wire_index {
            ipc::LOW_CUT_INDEX | ipc::HIGH_CUT_INDEX => self.rebuild_filters(),
            // A label: nothing to retune.
            ipc::MODE_INDEX => {}
            _ => self.retune(),
        }
        true
    }

    /// Resolve a string id off the realtime path (project restore, tests).
    pub fn apply_ui_param(&mut self, id: &str, value: f32) -> bool {
        match ipc::ui_param_index(id) {
            Some(index) => self.apply_wire_param(index, value),
            None => false,
        }
    }

    /// VerbSpace introduces no lookahead: the pre-delay is a musical
    /// parameter, not a processing latency the graph should compensate for.
    pub fn latency_samples(&self) -> usize {
        0
    }

    /// Re-resolves the tuning and hands every smoothed value its new target.
    /// Nothing jumps: the per-sample path glides there.
    fn retune(&mut self) {
        let t = Tuning::resolve(&self.params, self.sample_rate);
        self.predelay.target = t.predelay;
        self.early_span.target = t.early_span;
        self.diffuser_scale.target = t.diffuser_scale;
        self.late_norm.target = t.late_norm;
        self.input_gain.target = t.input_gain;
        self.early_gain.target = t.early_gain;
        self.late_gain.target = t.late_gain;
        self.mid_gain.target = t.mid_gain;
        self.side_gain.target = t.side_gain;
        self.dry_gain.target = t.dry_gain;
        self.wet_gain.target = t.wet_gain;
        self.input_diffusion.target = t.input_diffusion;
        self.early_diffusion.target = t.early_diffusion;
        self.tank_diffusion.target = t.tank_diffusion;
        self.low_split_a.target = t.low_split_a;
        self.high_split_a.target = t.high_split_a;
        self.tuning = t;
        if self.fresh {
            self.snap();
        }
    }

    /// Lands every gliding value on its target.
    fn snap(&mut self) {
        for smoothed in [
            &mut self.predelay,
            &mut self.early_span,
            &mut self.diffuser_scale,
            &mut self.late_norm,
            &mut self.input_gain,
            &mut self.early_gain,
            &mut self.late_gain,
            &mut self.mid_gain,
            &mut self.side_gain,
            &mut self.dry_gain,
            &mut self.wet_gain,
            &mut self.input_diffusion,
            &mut self.early_diffusion,
            &mut self.tank_diffusion,
            &mut self.low_split_a,
            &mut self.high_split_a,
        ] {
            smoothed.settle();
        }
        for (i, line) in self.lines.iter_mut().enumerate() {
            line.delay = self.tuning.lines[i];
            line.loss = self.tuning.absorption[i];
        }
    }

    fn rebuild_filters(&mut self) {
        let [low, high] = cut_coefficients(&self.params, self.sample_rate);
        let keep = |filter: &mut Option<DirectForm1<f32>>,
                    coefficients: Option<Coefficients<f32>>| {
            match (filter.as_mut(), coefficients) {
                // Swapping coefficients under a running filter keeps its
                // state: a cut sweep does not restart the tail.
                (Some(f), Some(c)) => f.update_coefficients(c),
                (_, c) => *filter = c.map(DirectForm1::<f32>::new),
            }
        };
        keep(&mut self.low_cut_l, low);
        keep(&mut self.low_cut_r, low);
        keep(&mut self.high_cut_l, high);
        keep(&mut self.high_cut_r, high);
    }

    /// Early reflections of the pre-delayed input. Taps on even positions
    /// read their own side, odd ones the other, so the pattern carries some
    /// of each channel across.
    #[inline]
    fn early(&self, span: f32) -> (f32, f32) {
        let mut left = 0.0;
        let mut right = 0.0;
        for i in 0..EARLY_TAPS {
            let (own_l, own_r) = if i % 2 == 0 {
                (&self.early_l, &self.early_r)
            } else {
                (&self.early_r, &self.early_l)
            };
            left += own_l.read_linear(1.0 + EARLY_AT_L[i] * span) * self.early_gain_l[i];
            right += own_r.read_linear(1.0 + EARLY_AT_R[i] * span) * self.early_gain_r[i];
        }
        (left, right)
    }
}

impl StereoEffect for Dsp {
    fn reset(&mut self) {
        self.predelay_l.clear();
        self.predelay_r.clear();
        self.early_l.clear();
        self.early_r.clear();
        for ap in self
            .early_diffusers_l
            .iter_mut()
            .chain(self.early_diffusers_r.iter_mut())
        {
            ap.clear();
        }
        for ap in self
            .diffusers_l
            .iter_mut()
            .chain(self.diffusers_r.iter_mut())
        {
            ap.ring.clear();
        }
        for line in self.lines.iter_mut() {
            line.clear();
        }
        for filter in [
            self.low_cut_l.as_mut(),
            self.low_cut_r.as_mut(),
            self.high_cut_l.as_mut(),
            self.high_cut_r.as_mut(),
        ]
        .into_iter()
        .flatten()
        {
            filter.reset_state();
        }
        // Silence has nothing to glide from.
        self.fresh = true;
        self.snap();
    }

    fn set_sample_rate(&mut self, sample_rate: f32) {
        let sr = sample_rate.max(1.0);
        if (sr - self.sample_rate).abs() < f32::EPSILON {
            return;
        }
        let params = self.params.clone();
        *self = Self::new(sr);
        self.set_params(params);
        self.reset();
    }

    fn process_stereo(&mut self, left: f32, right: f32) -> (f32, f32) {
        if !self.params.power {
            return (left, right);
        }
        self.fresh = false;
        let glide = self.glide_step;
        let smooth = self.smooth_step;

        // Pre-delay, gliding to its length so a drag does not click.
        let predelay = self.predelay.next(glide);
        self.predelay_l.push(left);
        self.predelay_r.push(right);
        let (pre_l, pre_r) = if predelay < 0.5 {
            (left, right)
        } else {
            (
                self.predelay_l.read_linear(predelay + 1.0),
                self.predelay_r.read_linear(predelay + 1.0),
            )
        };
        // Freeze mutes what enters the room; the room keeps what it has.
        let input = self.input_gain.next(smooth);
        let (x_l, x_r) = (pre_l * input, pre_r * input);

        // Early reflections, spread over the room's span.
        self.early_l.push(x_l);
        self.early_r.push(x_r);
        let span = self.early_span.next(glide);
        let (er_l, er_r) = self.early(span);
        let early_g = self.early_diffusion.next(smooth);
        let (mut early_l, mut early_r) = (er_l, er_r);
        for i in 0..EARLY_DIFFUSER_COUNT {
            early_l = self.early_diffusers_l[i].process(early_l, early_g);
            early_r = self.early_diffusers_r[i].process(early_r, early_g);
        }

        // The tank hears the direct sound and its reflections, so the tail
        // builds over the early span the way a room's does.
        let norm = self.late_norm.next(smooth) * FRAC_1_SQRT_2;
        let mut diff_l = (x_l + er_l) * norm;
        let mut diff_r = (x_r + er_r) * norm;
        let scale = self.diffuser_scale.next(glide);
        let g = self.input_diffusion.next(smooth);
        for i in 0..DIFFUSER_COUNT {
            diff_l = self.diffusers_l[i].process(diff_l, g, scale);
            diff_r = self.diffusers_r[i].process(diff_r, g, scale);
        }

        // Tank: read every line before any of them is written — the
        // Hadamard mix combines the whole tank at one instant.
        let t = &self.tuning;
        let depth = t.mod_depth;
        let mut taps = [0.0f32; LINE_COUNT];
        let mut late_l = 0.0f32;
        let mut late_r = 0.0f32;
        for (i, line) in self.lines.iter_mut().enumerate() {
            line.delay += (t.lines[i] - line.delay) * glide;
            let (c, s) = t.mod_step[i];
            let (sin, cos) = (line.lfo_sin, line.lfo_cos);
            line.lfo_sin = sin * c + cos * s;
            line.lfo_cos = cos * c - sin * s;
            let swing = depth * MOD_DEPTH_SPREAD[i] * (1.0 + line.lfo_sin);
            let tap = line.ring.read_cubic(line.delay + swing);
            taps[i] = tap;
            late_l += tap * OUTPUT_L[i];
            late_r += tap * OUTPUT_R[i];
        }
        // The rotation's magnitude drifts by rounding; pull it back to one
        // now and then.
        self.lfo_tick += 1;
        if self.lfo_tick >= 64 {
            self.lfo_tick = 0;
            for line in self.lines.iter_mut() {
                let norm = 1.5 - 0.5 * (line.lfo_sin * line.lfo_sin + line.lfo_cos * line.lfo_cos);
                line.lfo_sin *= norm;
                line.lfo_cos *= norm;
            }
        }

        let tank_g = self.tank_diffusion.next(smooth);
        let low_split = OnePole::from_a(self.low_split_a.next(smooth));
        let high_split = OnePole::from_a(self.high_split_a.next(smooth));
        for (i, line) in self.lines.iter_mut().enumerate() {
            line.glide_loss(t.absorption[i], smooth);
            let absorbed = line.absorb(taps[i], low_split, high_split);
            taps[i] = line.allpass.process(absorbed, tank_g);
        }
        hadamard(&mut taps);
        for (i, line) in self.lines.iter_mut().enumerate() {
            let injected = (diff_l * INPUT_L[i] + diff_r * INPUT_R[i]) * 0.25;
            line.ring.push(flush_denormal(taps[i] + injected));
        }

        let early_out = self.early_gain.next(smooth);
        let late_out = self.late_gain.next(smooth) * LATE_OUTPUT;
        let mut wet_l = late_l * late_out + early_l * early_out;
        let mut wet_r = late_r * late_out + early_r * early_out;
        if let Some(f) = self.low_cut_l.as_mut() {
            wet_l = f.run(wet_l);
        }
        if let Some(f) = self.low_cut_r.as_mut() {
            wet_r = f.run(wet_r);
        }
        if let Some(f) = self.high_cut_l.as_mut() {
            wet_l = f.run(wet_l);
        }
        if let Some(f) = self.high_cut_r.as_mut() {
            wet_r = f.run(wet_r);
        }

        let mid = (wet_l + wet_r) * 0.5 * self.mid_gain.next(smooth);
        let side = (wet_l - wet_r) * 0.5 * self.side_gain.next(smooth);
        let wet = self.wet_gain.next(smooth);
        let dry = self.dry_gain.next(smooth);
        (
            flush_denormal(left * dry + (mid + side) * wet),
            flush_denormal(right * dry + (mid - side) * wet),
        )
    }
}

#[cfg(test)]
mod tests;

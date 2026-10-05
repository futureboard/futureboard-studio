//! VerbSpace — algorithmic stereo reverb.
//!
//! Engine-agnostic like the other `BuiltinAudioPlugins` cores. The signal path
//! is
//!
//! ```txt
//! pre-delay ─┬─ early reflections (12 taps a side) ─────────────────────┐
//!            └─ input diffusion (4 allpasses a side) ─ 16-line FDN ─────┴─ low cut / high cut
//!                 (Hadamard feedback, modulated cubic reads, an allpass     ─ width ─ mix
//!                  and a two-shelf absorption filter in every line)
//! ```
//!
//! The tank decays in three bands: `decaySec` is the RT60 of the middle,
//! `bassMult` scales it below the low split and `damping` shortens it above
//! the high split. Every line's absorption filter is designed from its own
//! length, so each one loses exactly what its share of the RT60 says — the
//! decay the editor draws ([`decay_profile`]) is computed from the very same
//! coefficients.
//!
//! Every buffer is sized at construction for the widest reachable setting, so
//! a parameter edit only moves targets: line lengths, the pre-delay and the
//! early pattern glide to theirs, and every gain is smoothed, so nothing a
//! user does on a running tail clicks. No realtime allocation.

use std::f32::consts::{FRAC_1_SQRT_2, FRAC_PI_2, PI, TAU};

use biquad::{Biquad, Coefficients, DirectForm1};
use builtin_dsp_core::delay::{Allpass, DelayRing, Smoothed, smoothing_step};
use builtin_dsp_core::{
    ParamDescriptor, PluginCategory, PluginDescriptor, StereoEffect, biquad_response_db, clamp,
    db_to_linear, flush_denormal, make_eq_coefficients,
};
use serde::{Deserialize, Serialize};

pub mod ipc;
pub mod presets;
pub mod ui;

/// Editor-facing parameter id table, re-exported at the crate root so the host
/// resolves ids the same way for every built-in (`<plugin>::ui_param_index`).
pub use ipc::{UI_PARAM_IDS, ui_param_id, ui_param_index};
pub use presets::{FactoryPreset, factory_presets};

pub const PLUGIN_ID: &str = "futureboard.verbspace";

/// Delay lines in the tank. Sixteen under a Hadamard matrix give a tail dense
/// enough for a hall from the first few hundred milliseconds without any
/// audible flutter, and the fast transform keeps the mixing at 64 adds.
pub const LINE_COUNT: usize = 16;

/// Allpass diffusers per channel ahead of the tank.
pub const DIFFUSER_COUNT: usize = 4;

/// Early reflections per side.
pub const EARLY_TAPS: usize = 12;

/// Longest pre-delay the ring is sized for.
pub const MAX_PREDELAY_MS: f32 = 500.0;

/// Peak modulation excursion of a tank read at `modDepth` 100 %.
const MAX_MOD_MS: f32 = 0.9;

/// How far a line, the pre-delay or the early pattern travels toward a new
/// length per second: a time constant, so a size or mode change bends the
/// running tail instead of cutting it.
const GLIDE_MS: f32 = 70.0;

/// Time constant of every smoothed gain (mix, output, width, input).
const SMOOTH_MS: f32 = 25.0;

/// Loop gain while frozen. Not exactly one: a lossless loop is only
/// marginally stable in floating point, and this still loses less than
/// 0.01 dB a second.
const FREEZE_GAIN: f32 = 0.99995;

/// RT60 ceiling per band. `decaySec` tops out at 20 s, and `bassMult` can
/// double that below the low split.
const MAX_RT60_SEC: f32 = 40.0;

/// The tank's low band ends here; `bassMult` scales the decay below it.
pub const LOW_SPLIT_HZ: f32 = 320.0;

/// Shortest decay of the top band at `damping` 100 %, as a share of the
/// middle band's.
const MIN_HIGH_RATIO: f32 = 0.12;

/// Reverb character. Picks the tank's line lengths, the early pattern, how
/// much diffusion the mode brings, and where its highs start to fall away.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReverbMode {
    Room,
    Chamber,
    Hall,
    Plate,
    Ambience,
}

/// What a mode is made of, at `size` 100 %.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Character {
    /// Shortest and longest tank line.
    pub shortest_ms: f32,
    pub longest_ms: f32,
    /// Arrival of the last early reflection; zero for none.
    pub early_span_ms: f32,
    /// Early reflections' level against the tail.
    pub early_level: f32,
    /// Input allpass gain at `diffusion` 0 %; 100 % reaches
    /// [`MAX_INPUT_DIFFUSION`].
    pub input_diffusion: f32,
    /// In-tank allpass gain at `diffusion` 100 %; 0 % is half of it.
    pub tank_diffusion: f32,
    /// Where the top band, the one `damping` shortens, begins.
    pub high_split_hz: f32,
    /// Wet trim that brings the mode to the same loudness as the others:
    /// about 3 dB under a broadband input at 100 % wet.
    pub trim_db: f32,
}

const MAX_INPUT_DIFFUSION: f32 = 0.78;

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

    pub const fn character(self) -> Character {
        match self {
            Self::Room => Character {
                shortest_ms: 8.0,
                longest_ms: 38.0,
                early_span_ms: 36.0,
                early_level: 0.62,
                input_diffusion: 0.25,
                tank_diffusion: 0.45,
                high_split_hz: 5_200.0,
                trim_db: 3.7,
            },
            Self::Chamber => Character {
                shortest_ms: 13.0,
                longest_ms: 58.0,
                early_span_ms: 52.0,
                early_level: 0.48,
                input_diffusion: 0.30,
                tank_diffusion: 0.50,
                high_split_hz: 4_800.0,
                trim_db: 5.1,
            },
            Self::Hall => Character {
                shortest_ms: 22.0,
                longest_ms: 104.0,
                early_span_ms: 74.0,
                early_level: 0.32,
                input_diffusion: 0.30,
                tank_diffusion: 0.50,
                high_split_hz: 4_000.0,
                trim_db: 6.8,
            },
            // A plate is a sheet, not a room: no discrete reflections, the
            // densest build-up, and a top end that rings longest.
            Self::Plate => Character {
                shortest_ms: 5.0,
                longest_ms: 44.0,
                early_span_ms: 0.0,
                early_level: 0.0,
                input_diffusion: 0.45,
                tank_diffusion: 0.62,
                high_split_hz: 7_000.0,
                trim_db: 7.8,
            },
            Self::Ambience => Character {
                shortest_ms: 4.0,
                longest_ms: 22.0,
                early_span_ms: 30.0,
                early_level: 0.85,
                input_diffusion: 0.20,
                tank_diffusion: 0.40,
                high_split_hz: 6_000.0,
                trim_db: 1.7,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Params {
    pub power: bool,
    pub mode: ReverbMode,
    /// Silence before the reflections start, in milliseconds.
    pub predelay_ms: f32,
    /// Tank line-length and early-pattern scaling, in percent.
    pub size: f32,
    /// RT60 of the middle band, in seconds.
    pub decay_sec: f32,
    /// Input and in-tank allpass amount, in percent.
    pub diffusion: f32,
    /// How much shorter the top band decays, in percent.
    pub damping: f32,
    /// Decay multiplier below [`LOW_SPLIT_HZ`].
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
}

pub fn default_params() -> Params {
    Params {
        power: true,
        mode: ReverbMode::Hall,
        predelay_ms: 20.0,
        size: 60.0,
        decay_sec: 2.4,
        diffusion: 72.0,
        damping: 45.0,
        bass_mult: 1.1,
        mod_depth: 25.0,
        mod_rate_hz: 0.6,
        low_cut_hz: 90.0,
        high_cut_hz: 9_500.0,
        width: 110.0,
        mix: 28.0,
        output_db: 0.0,
        freeze: false,
    }
}

pub fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PLUGIN_ID,
        name: "VerbSpace",
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
                default_value: 2.0,
                min: 0.0,
                max: 4.0,
                unit: "enum",
            },
            ParamDescriptor {
                id: "predelayMs",
                name: "Pre-Delay",
                default_value: 20.0,
                min: 0.0,
                max: MAX_PREDELAY_MS,
                unit: "ms",
            },
            ParamDescriptor {
                id: "size",
                name: "Size",
                default_value: 60.0,
                min: 10.0,
                max: 100.0,
                unit: "%",
            },
            ParamDescriptor {
                id: "decaySec",
                name: "Decay",
                default_value: 2.4,
                min: 0.1,
                max: 20.0,
                unit: "s",
            },
            ParamDescriptor {
                id: "diffusion",
                name: "Diffusion",
                default_value: 72.0,
                min: 0.0,
                max: 100.0,
                unit: "%",
            },
            ParamDescriptor {
                id: "damping",
                name: "Damping",
                default_value: 45.0,
                min: 0.0,
                max: 100.0,
                unit: "%",
            },
            ParamDescriptor {
                id: "bassMult",
                name: "Bass",
                default_value: 1.1,
                min: 0.2,
                max: 2.0,
                unit: "x",
            },
            ParamDescriptor {
                id: "modDepth",
                name: "Mod Depth",
                default_value: 25.0,
                min: 0.0,
                max: 100.0,
                unit: "%",
            },
            ParamDescriptor {
                id: "modRateHz",
                name: "Mod Rate",
                default_value: 0.6,
                min: 0.05,
                max: 5.0,
                unit: "Hz",
            },
            ParamDescriptor {
                id: "lowCutHz",
                name: "Low Cut",
                default_value: 90.0,
                min: 20.0,
                max: 1_000.0,
                unit: "Hz",
            },
            ParamDescriptor {
                id: "highCutHz",
                name: "High Cut",
                default_value: 9_500.0,
                min: 1_000.0,
                max: 20_000.0,
                unit: "Hz",
            },
            ParamDescriptor {
                id: "width",
                name: "Width",
                default_value: 110.0,
                min: 0.0,
                max: 200.0,
                unit: "%",
            },
            ParamDescriptor {
                id: "mix",
                name: "Mix",
                default_value: 28.0,
                min: 0.0,
                max: 100.0,
                unit: "%",
            },
            ParamDescriptor {
                id: "outputDb",
                name: "Output",
                default_value: 0.0,
                min: -24.0,
                max: 12.0,
                unit: "dB",
            },
            ParamDescriptor {
                id: "freeze",
                name: "Freeze",
                default_value: 0.0,
                min: 0.0,
                max: 1.0,
                unit: "bool",
            },
        ],
    }
}

// ── Fixed tables ─────────────────────────────────────────────────────────────

/// Where each line sits between a mode's shortest and longest length, before
/// the permutation below: an even spread nudged off the grid, so no two lines
/// share a simple ratio and the modes never stack into a comb.
const LINE_SPREAD_JITTER: [f32; LINE_COUNT] = [
    0.0, 0.21, -0.17, 0.31, -0.08, 0.27, -0.29, 0.12, -0.22, 0.33, -0.11, 0.19, -0.31, 0.07, -0.24,
    0.0,
];

/// Which line takes the n-th shortest length. Neighbouring lengths land far
/// apart in the Hadamard butterflies.
const LINE_ORDER: [usize; LINE_COUNT] = [0, 9, 3, 12, 6, 15, 1, 10, 4, 13, 7, 2, 11, 5, 14, 8];

/// The allpass inside each line, in milliseconds. Fixed, not scaled by size:
/// a length change inside a feedback loop would click.
const TANK_ALLPASS_MS: [f32; LINE_COUNT] = [
    0.61, 0.73, 0.89, 0.97, 1.13, 1.27, 1.39, 1.51, 1.67, 1.79, 1.93, 2.11, 2.29, 2.41, 2.63, 2.87,
];

/// Input diffusers, offset between the channels so the two decorrelate before
/// they reach the tank.
const DIFFUSER_MS_L: [f32; DIFFUSER_COUNT] = [3.13, 4.71, 7.93, 11.37];
const DIFFUSER_MS_R: [f32; DIFFUSER_COUNT] = [3.53, 5.19, 8.41, 12.07];

/// Every line's LFO runs at the rate times its own factor, so the tank never
/// sweeps in step.
const MOD_RATE_SPREAD: [f32; LINE_COUNT] = [
    1.00, 0.83, 1.17, 0.71, 1.29, 0.91, 1.07, 0.77, 1.37, 0.87, 1.11, 0.74, 1.23, 0.95, 1.31, 0.79,
];
const MOD_DEPTH_SPREAD: [f32; LINE_COUNT] = [
    1.00, 1.18, 0.86, 1.24, 0.79, 1.09, 0.93, 1.21, 0.82, 1.13, 0.90, 1.25, 0.76, 1.05, 0.97, 1.16,
];

/// Arrival of each early reflection as a share of the mode's span. The two
/// sides interleave so they never coincide.
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

// ── Tuning: every derived value, from one place ─────────────────────────────

/// Bilinear one-pole low-pass `y = b (x + x₁) + a y₁`: unity at DC, an exact
/// zero at Nyquist, monotonic between — which is what lets a shelf built
/// from it hit both its end gains exactly.
#[derive(Debug, Clone, Copy, PartialEq)]
struct OnePole {
    a: f32,
    b: f32,
}

impl OnePole {
    fn new(corner_hz: f32, sample_rate: f32) -> Self {
        let corner = clamp(corner_hz, 1.0, sample_rate * 0.45);
        let k = (PI * corner / sample_rate).tan();
        Self {
            a: (1.0 - k) / (1.0 + k),
            b: k / (1.0 + k),
        }
    }

    /// Magnitude at `hz`.
    fn magnitude(self, hz: f32, sample_rate: f32) -> (f64, f64) {
        // Complex response b (1 + e^-jw) / (1 - a e^-jw), as (re, im).
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
        let (lr, li) = low.magnitude(hz, sample_rate);
        let (hr, hi) = high.magnitude(hz, sample_rate);
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
    low_split: OnePole,
    high_split: OnePole,
    rt: [f32; 3],
    input_diffusion: f32,
    tank_diffusion: f32,
    /// Span of the early pattern, in samples.
    early_span: f32,
    early_level: f32,
    mod_depth: f32,
    /// Per-line LFO rotation `(cos, sin)` per sample.
    mod_step: [(f32, f32); LINE_COUNT],
    /// Gain into the tank, so a long decay sounds bigger without arriving
    /// many decibels louder than a short one.
    late_gain: f32,
    input_gain: f32,
    output_gain: f32,
    mid_gain: f32,
    side_gain: f32,
    dry_gain: f32,
    wet_gain: f32,
}

impl Tuning {
    fn resolve(p: &Params, sample_rate: f32) -> Self {
        let sr = sample_rate;
        let character = p.mode.character();
        let scale = size_scale(p.size);
        let ms = |ms: f32| ms * 0.001 * sr;

        let mut lines = [0.0; LINE_COUNT];
        let ratio = character.longest_ms / character.shortest_ms;
        for rank in 0..LINE_COUNT {
            let at = clamp(
                (rank as f32 + LINE_SPREAD_JITTER[rank]) / (LINE_COUNT - 1) as f32,
                0.0,
                1.0,
            );
            lines[LINE_ORDER[rank]] = ms(character.shortest_ms * ratio.powf(at) * scale).max(4.0);
        }
        let loops: [f32; LINE_COUNT] =
            std::array::from_fn(|i| lines[i] + allpass_len(TANK_ALLPASS_MS[i], sr) as f32);

        let rt_mid = clamp(p.decay_sec, 0.05, MAX_RT60_SEC);
        let rt_low = clamp(p.decay_sec * p.bass_mult, 0.05, MAX_RT60_SEC);
        let damping = clamp(p.damping / 100.0, 0.0, 1.0);
        let rt_high = rt_mid * (1.0 - damping * (1.0 - MIN_HIGH_RATIO));
        let rt = [rt_low, rt_mid, rt_high];
        let absorption = std::array::from_fn(|i| {
            if p.freeze {
                Absorption::FROZEN
            } else {
                Absorption::for_loop(loops[i], sr, rt)
            }
        });

        let diffusion = clamp(p.diffusion / 100.0, 0.0, 1.0);
        let input_diffusion = character.input_diffusion
            + diffusion * (MAX_INPUT_DIFFUSION - character.input_diffusion);
        let tank_diffusion = character.tank_diffusion * (0.5 + 0.5 * diffusion);

        let rate = clamp(p.mod_rate_hz, 0.0, 20.0);
        let mod_step = std::array::from_fn(|i| {
            let w = TAU * rate * MOD_RATE_SPREAD[i] / sr;
            (w.cos(), w.sin())
        });

        // The tail's energy grows with RT60 over loop length. Taking 0.35 of
        // it back leaves ten times the decay about 3 dB louder — bigger, not
        // overwhelming. Read from the unfrozen decay, so Freeze holds the
        // level it caught.
        let mean_loop = loops.iter().sum::<f32>() / LINE_COUNT as f32;
        let energy = (rt_mid * sr / (6.0 * std::f32::consts::LN_10 * mean_loop)).max(0.5);
        let late_gain = energy.powf(-0.35);

        let width = clamp(p.width / 100.0, 0.0, 2.0);
        let mix = clamp(p.mix / 100.0, 0.0, 1.0);
        Self {
            predelay: ms(clamp(p.predelay_ms, 0.0, MAX_PREDELAY_MS)),
            lines,
            loops,
            absorption,
            low_split: OnePole::new(LOW_SPLIT_HZ, sr),
            high_split: OnePole::new(character.high_split_hz, sr),
            rt,
            input_diffusion,
            tank_diffusion,
            early_span: ms(character.early_span_ms * scale),
            early_level: character.early_level,
            mod_depth: ms(clamp(p.mod_depth / 100.0, 0.0, 1.0) * MAX_MOD_MS),
            mod_step,
            late_gain,
            input_gain: if p.freeze { 0.0 } else { 1.0 },
            output_gain: db_to_linear(p.output_db + character.trim_db),
            // Width as a mid/side trim whose two gains sum in power, so 0 %
            // and 200 % stay near the loudness of 100 %.
            mid_gain: (2.0 - width).sqrt() * FRAC_1_SQRT_2,
            side_gain: width.sqrt() * FRAC_1_SQRT_2,
            // Dry and wet are uncorrelated, so an equal-power law keeps the
            // loudness steady across the whole travel.
            dry_gain: (mix * FRAC_PI_2).cos(),
            wet_gain: (mix * FRAC_PI_2).sin(),
        }
    }
}

/// `size` 10–100 % onto a length multiplier.
fn size_scale(size: f32) -> f32 {
    0.3 + 0.7 * clamp(size / 100.0, 0.0, 1.0)
}

/// What [`Allpass::for_ms`] rounds `ms` to, without building one.
fn allpass_len(ms: f32, sample_rate: f32) -> usize {
    ((ms * 0.001 * sample_rate).round() as usize).max(1)
}

/// Gains of the early pattern: signed, falling with arrival, and normalised
/// to unit energy per side so `early_level` is the level that plays.
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
    /// Signed linear gain into its side.
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
            gain: gain * tuning.early_level,
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
        rt_low_sec: rt(tuning.rt[0]),
        rt_mid_sec: rt(tuning.rt[1]),
        rt_high_sec: rt(tuning.rt[2]),
        low_split_hz: LOW_SPLIT_HZ,
        high_split_hz: params.mode.character().high_split_hz,
        frozen,
        sample_rate: sr,
        loop_samples: tuning.loops[typical],
        absorption: tuning.absorption[typical],
        low_split: tuning.low_split,
        high_split: tuning.high_split,
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

#[derive(Debug, Clone)]
pub struct Dsp {
    sample_rate: f32,
    params: Params,
    tuning: Tuning,

    predelay_l: DelayRing,
    predelay_r: DelayRing,
    early_l: DelayRing,
    early_r: DelayRing,
    diffusers_l: [Allpass; DIFFUSER_COUNT],
    diffusers_r: [Allpass; DIFFUSER_COUNT],
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
    early_level: Smoothed,
    late_gain: Smoothed,
    input_gain: Smoothed,
    output_gain: Smoothed,
    mid_gain: Smoothed,
    side_gain: Smoothed,
    dry_gain: Smoothed,
    wet_gain: Smoothed,
    input_diffusion: Smoothed,
    tank_diffusion: Smoothed,
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
            early_l: DelayRing::for_ms(Self::max_early_ms(), sr),
            early_r: DelayRing::for_ms(Self::max_early_ms(), sr),
            diffusers_l: Self::build_diffusers(sr, &DIFFUSER_MS_L),
            diffusers_r: Self::build_diffusers(sr, &DIFFUSER_MS_R),
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
            early_level: Smoothed::at(tuning.early_level),
            late_gain: Smoothed::at(tuning.late_gain),
            input_gain: Smoothed::at(tuning.input_gain),
            output_gain: Smoothed::at(tuning.output_gain),
            mid_gain: Smoothed::at(tuning.mid_gain),
            side_gain: Smoothed::at(tuning.side_gain),
            dry_gain: Smoothed::at(tuning.dry_gain),
            wet_gain: Smoothed::at(tuning.wet_gain),
            input_diffusion: Smoothed::at(tuning.input_diffusion),
            tank_diffusion: Smoothed::at(tuning.tank_diffusion),
            lfo_tick: 0,
            fresh: true,
            tuning,
            params,
        };
        dsp.rebuild_filters();
        dsp
    }

    /// The longest early pattern any mode reaches at size 100 %.
    fn max_early_ms() -> f32 {
        ReverbMode::ALL
            .iter()
            .map(|mode| mode.character().early_span_ms)
            .fold(0.0, f32::max)
            + 2.0
    }

    /// The longest tank line any mode reaches, plus the modulation's swing.
    fn max_line_ms() -> f32 {
        ReverbMode::ALL
            .iter()
            .map(|mode| mode.character().longest_ms)
            .fold(0.0, f32::max)
            * size_scale(100.0)
            + MAX_MOD_MS * 2.5
            + 2.0
    }

    fn build_diffusers(
        sample_rate: f32,
        lengths: &[f32; DIFFUSER_COUNT],
    ) -> [Allpass; DIFFUSER_COUNT] {
        std::array::from_fn(|i| Allpass::for_ms(lengths[i], sample_rate))
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
        self.early_level.target = t.early_level;
        self.late_gain.target = t.late_gain;
        self.input_gain.target = t.input_gain;
        self.output_gain.target = t.output_gain;
        self.mid_gain.target = t.mid_gain;
        self.side_gain.target = t.side_gain;
        self.dry_gain.target = t.dry_gain;
        self.wet_gain.target = t.wet_gain;
        self.input_diffusion.target = t.input_diffusion;
        self.tank_diffusion.target = t.tank_diffusion;
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
            &mut self.early_level,
            &mut self.late_gain,
            &mut self.input_gain,
            &mut self.output_gain,
            &mut self.mid_gain,
            &mut self.side_gain,
            &mut self.dry_gain,
            &mut self.wet_gain,
        ] {
            smoothed.settle();
        }
        for (i, line) in self.lines.iter_mut().enumerate() {
            line.delay = self.tuning.lines[i];
            line.loss = self.tuning.absorption[i];
        }
        self.input_diffusion.settle();
        self.tank_diffusion.settle();
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
            .diffusers_l
            .iter_mut()
            .chain(self.diffusers_r.iter_mut())
        {
            ap.clear();
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

        // Early reflections.
        self.early_l.push(pre_l);
        self.early_r.push(pre_r);
        let span = self.early_span.next(glide);
        let early_level = self.early_level.next(smooth);
        let (early_l, early_r) = if early_level > 1.0e-4 {
            self.early(span)
        } else {
            (0.0, 0.0)
        };

        // Input diffusion.
        let input_gain = self.input_gain.next(smooth) * self.late_gain.next(smooth);
        let mut diff_l = pre_l * input_gain;
        let mut diff_r = pre_r * input_gain;
        let g = self.input_diffusion.next(smooth);
        for i in 0..DIFFUSER_COUNT {
            diff_l = self.diffusers_l[i].process(diff_l, g);
            diff_r = self.diffusers_r[i].process(diff_r, g);
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
        let (low_split, high_split) = (t.low_split, t.high_split);
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

        let mut wet_l = late_l * 0.25 + early_l * early_level;
        let mut wet_r = late_r * 0.25 + early_r * early_level;
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

        let mid = (wet_l + wet_r) * FRAC_1_SQRT_2 * self.mid_gain.next(smooth);
        let side = (wet_l - wet_r) * FRAC_1_SQRT_2 * self.side_gain.next(smooth);
        let wet = self.wet_gain.next(smooth) * self.output_gain.next(smooth);
        let dry = self.dry_gain.next(smooth);
        (
            left * dry + (mid + side) * wet,
            right * dry + (mid - side) * wet,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    fn wet_only(mut params: Params) -> Params {
        params.mix = 100.0;
        params
    }

    fn dsp_with(params: Params) -> Dsp {
        let mut dsp = Dsp::new(SR);
        dsp.set_params(params);
        dsp.reset();
        dsp
    }

    /// The wet impulse response, left and right.
    fn impulse_response(params: Params, frames: usize) -> (Vec<f32>, Vec<f32>) {
        let mut dsp = dsp_with(wet_only(params));
        let mut left = Vec::with_capacity(frames);
        let mut right = Vec::with_capacity(frames);
        for n in 0..frames {
            let x = if n == 0 { 1.0 } else { 0.0 };
            let (l, r) = dsp.process_stereo(x, x);
            assert!(l.is_finite() && r.is_finite(), "non-finite at {n}");
            left.push(l);
            right.push(r);
        }
        (left, right)
    }

    /// Seconds for the backward-integrated energy (Schroeder) to fall from
    /// −5 to −25 dB, scaled to 60 dB.
    fn measured_rt60(signal: &[f32]) -> f32 {
        let mut energy: Vec<f64> = signal.iter().map(|x| f64::from(*x).powi(2)).collect();
        for i in (0..energy.len() - 1).rev() {
            energy[i] += energy[i + 1];
        }
        let total = energy[0];
        let level = |i: usize| 10.0 * (energy[i] / total).log10();
        let start = (0..energy.len()).find(|&i| level(i) <= -5.0).unwrap();
        let end = (0..energy.len()).find(|&i| level(i) <= -25.0).unwrap();
        (end - start) as f32 / SR * 3.0
    }

    #[test]
    fn descriptor_ids_are_unique_and_match_defaults() {
        let d = descriptor();
        assert_eq!(d.id, PLUGIN_ID);
        assert_eq!(d.category, PluginCategory::Effect);

        let mut ids: Vec<_> = d.params.iter().map(|p| p.id).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(count, ids.len(), "duplicate parameter id in descriptor");

        let defaults = ipc::ui_values(&default_params());
        for param in d.params {
            let (_, actual) = defaults
                .iter()
                .find(|(id, _)| *id == param.id)
                .copied()
                .unwrap_or_else(|| panic!("`{}` is missing from ui_values", param.id));
            assert!(
                (param.default_value - actual).abs() < 1.0e-6,
                "`{}`: descriptor says {}, default_params() says {actual}",
                param.id,
                param.default_value,
            );
            assert!(
                param.default_value >= param.min && param.default_value <= param.max,
                "`{}`: default {} is outside {}..{}",
                param.id,
                param.default_value,
                param.min,
                param.max,
            );
        }
    }

    #[test]
    fn bypass_when_power_off() {
        let mut params = default_params();
        params.power = false;
        let mut dsp = dsp_with(params);
        assert_eq!(dsp.process_stereo(0.25, -0.25), (0.25, -0.25));
    }

    #[test]
    fn mix_at_zero_is_the_dry_signal() {
        let mut params = default_params();
        params.mix = 0.0;
        let mut dsp = dsp_with(params);
        for n in 0..4_800 {
            let x = (n as f32 * 0.05).sin() * 0.5;
            let (l, r) = dsp.process_stereo(x, -x);
            assert!((l - x).abs() < 1.0e-6 && (r + x).abs() < 1.0e-6);
        }
    }

    #[test]
    fn the_feedback_matrix_preserves_energy() {
        let mut v: [f32; LINE_COUNT] = std::array::from_fn(|i| (i as f32 * 0.37).sin());
        let before: f32 = v.iter().map(|x| x * x).sum();
        hadamard(&mut v);
        let after: f32 = v.iter().map(|x| x * x).sum();
        assert!((before - after).abs() < 1.0e-5);
    }

    /// Two orthogonal output vectors are what decorrelate left and right;
    /// injections that are not rows of the matrix are what spread at once.
    #[test]
    fn the_tank_vectors_are_orthogonal_and_spread() {
        let dot = |a: &[f32; LINE_COUNT], b: &[f32; LINE_COUNT]| -> f32 {
            a.iter().zip(b).map(|(x, y)| x * y).sum()
        };
        assert_eq!(dot(&INPUT_L, &INPUT_R), 0.0);
        assert_eq!(dot(&OUTPUT_L, &OUTPUT_R), 0.0);
        for input in [INPUT_L, INPUT_R] {
            let mut mixed = input;
            hadamard(&mut mixed);
            let loudest = mixed.iter().fold(0.0f32, |m, x| m.max(x.abs()));
            assert!(loudest < 3.0, "an injection collapsed onto one line");
        }
    }

    #[test]
    fn impulse_builds_a_tail_at_every_rate_and_mode() {
        for &sr in &[44_100.0f32, 48_000.0, 96_000.0] {
            for mode in ReverbMode::ALL {
                let mut dsp = Dsp::new(sr);
                let mut params = default_params();
                params.mode = mode;
                params.mix = 100.0;
                dsp.set_params(params);
                dsp.reset();
                let _ = dsp.process_stereo(1.0, 1.0);
                let mut late = 0.0f32;
                for n in 0..(sr as usize / 2) {
                    let (l, r) = dsp.process_stereo(0.0, 0.0);
                    assert!(l.is_finite() && r.is_finite());
                    if n > sr as usize / 5 {
                        late = late.max(l.abs()).max(r.abs());
                    }
                }
                assert!(late > 1.0e-4, "{mode:?} @ {sr} produced no tail");
            }
        }
    }

    /// The tank is a feedback loop around a unit-gain mixing matrix; the
    /// longest decay at the largest size is where a coefficient slip shows up
    /// as a slow build to infinity rather than as a tail.
    #[test]
    fn longest_decay_stays_bounded() {
        let mut params = wet_only(default_params());
        params.decay_sec = 20.0;
        params.size = 100.0;
        params.damping = 0.0;
        params.bass_mult = 2.0;
        params.mod_depth = 100.0;
        params.diffusion = 100.0;
        let mut dsp = dsp_with(params);
        let mut peak = 0.0f32;
        for n in 0..(48_000 * 8) {
            let x = if n < 4_800 {
                (n as f32 * 0.01).sin() * 0.7
            } else {
                0.0
            };
            let (l, r) = dsp.process_stereo(x, x);
            assert!(l.is_finite() && r.is_finite(), "non-finite at sample {n}");
            peak = peak.max(l.abs()).max(r.abs());
        }
        assert!(peak < 4.0, "tank ran away: peak {peak}");
    }

    /// The decay the editor draws is the decay that plays.
    #[test]
    fn the_measured_decay_is_the_drawn_decay() {
        for (mode, decay) in [
            (ReverbMode::Hall, 1.5f32),
            (ReverbMode::Room, 0.8),
            (ReverbMode::Plate, 2.5),
        ] {
            let mut params = default_params();
            params.mode = mode;
            params.decay_sec = decay;
            params.damping = 0.0;
            params.bass_mult = 1.0;
            params.low_cut_hz = 20.0;
            params.high_cut_hz = 20_000.0;
            params.mod_depth = 0.0;
            params.predelay_ms = 0.0;
            let profile = decay_profile(&params, SR);
            assert!((profile.rt_at(1_000.0) - decay).abs() < decay * 0.02);
            let (left, right) = impulse_response(params, (SR * decay * 1.6) as usize);
            let sum: Vec<f32> = left.iter().zip(&right).map(|(l, r)| l + r).collect();
            let measured = measured_rt60(&sum);
            assert!(
                (measured - decay).abs() < decay * 0.15,
                "{mode:?}: drew {decay} s, measured {measured} s"
            );
        }
    }

    #[test]
    fn damping_shortens_the_top_and_bass_lengthens_the_bottom() {
        let mut params = default_params();
        params.decay_sec = 2.0;
        params.damping = 80.0;
        params.bass_mult = 1.6;
        let profile = decay_profile(&params, SR);
        assert!(
            (profile.rt_at(30.0) - 3.2).abs() < 0.1,
            "{}",
            profile.rt_at(30.0)
        );
        assert!((profile.rt_at(1_200.0) - 2.0).abs() < 0.25);
        assert!(profile.rt_at(16_000.0) < 0.8, "{}", profile.rt_at(16_000.0));
        assert!(profile.rt_at(200.0) > profile.rt_at(2_000.0));
        assert!(profile.rt_at(2_000.0) > profile.rt_at(10_000.0));
    }

    /// A mono source comes back as two different tails that still sum: wide,
    /// and safe to fold down.
    #[test]
    fn a_mono_source_gets_a_wide_tail_that_folds_down() {
        let mut params = wet_only(default_params());
        params.width = 100.0;
        params.predelay_ms = 0.0;
        let mut dsp = dsp_with(params);
        let mut seed = 0x1234_5678u32;
        let (mut ll, mut rr, mut lr, mut mono) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
        for n in 0..(48_000 * 2) {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let x = (seed >> 9) as f32 / (1u32 << 23) as f32 - 0.5;
            let (l, r) = dsp.process_stereo(x, x);
            if n > 24_000 {
                let (l, r) = (f64::from(l), f64::from(r));
                ll += l * l;
                rr += r * r;
                lr += l * r;
                mono += (l + r) * (l + r) * 0.25;
            }
        }
        let correlation = lr / (ll * rr).sqrt();
        assert!(correlation.abs() < 0.35, "correlation {correlation}");
        assert!(
            mono > 0.3 * (ll + rr) * 0.5,
            "the fold-down cancels: {mono} vs {}",
            (ll + rr) * 0.5
        );
        // Balanced, too.
        assert!((10.0 * (ll / rr).log10()).abs() < 1.5);
    }

    #[test]
    fn nothing_arrives_before_the_predelay() {
        let mut params = default_params();
        params.predelay_ms = 50.0;
        let (left, right) = impulse_response(params, 4_800);
        let onset = left
            .iter()
            .zip(&right)
            .position(|(l, r)| l.abs() > 1.0e-6 || r.abs() > 1.0e-6)
            .unwrap();
        assert!(onset >= 2_400, "arrived at {onset}");
    }

    #[test]
    fn freeze_holds_the_tail_after_the_input_stops() {
        let mut params = wet_only(default_params());
        params.decay_sec = 2.0;
        let mut dsp = dsp_with(params);
        for n in 0..24_000 {
            let x = (n as f32 * 0.03).sin() * 0.5;
            let _ = dsp.process_stereo(x, x);
        }
        assert!(dsp.apply_ui_param("freeze", 1.0));

        let mut early = 0.0f32;
        for _ in 0..4_800 {
            let (l, r) = dsp.process_stereo(0.0, 0.0);
            early = early.max(l.abs()).max(r.abs());
        }
        let mut late = 0.0f32;
        for _ in 0..(48_000 * 4) {
            let (l, r) = dsp.process_stereo(0.0, 0.0);
            late = late.max(l.abs()).max(r.abs());
        }
        assert!(early > 1.0e-4, "nothing in the tank to freeze");
        assert!(
            late > early * 0.5,
            "freeze decayed away: early {early}, late {late}"
        );

        // And unfreezing lets it go.
        assert!(dsp.apply_ui_param("freeze", 0.0));
        for _ in 0..(48_000 * 6) {
            let _ = dsp.process_stereo(0.0, 0.0);
        }
        let (l, r) = dsp.process_stereo(0.0, 0.0);
        assert!(l.abs().max(r.abs()) < early * 0.01);
    }

    /// Dragging Size or switching mode on a running tail bends it; it never
    /// steps. A step shows as a sample-to-sample jump out of proportion to
    /// the level around it; a glide only bends the pitch, which at most
    /// doubles the slope of a tone for the moment it lasts.
    #[test]
    fn a_size_or_mode_change_does_not_click() {
        for (id, value) in [
            ("size", 15.0f32),
            ("mode", ReverbMode::Room.to_wire()),
            ("decaySec", 0.3),
            ("freeze", 1.0),
            ("diffusion", 0.0),
        ] {
            let mut params = wet_only(default_params());
            params.low_cut_hz = 20.0;
            params.high_cut_hz = 20_000.0;
            let mut dsp = dsp_with(params);
            let mut previous = (0.0f32, 0.0f32);
            let mut window_step = 0.0f32;
            let mut window_peak = 0.0f32;
            let mut steady = 0.0f32;
            let mut worst = 0.0f32;
            for n in 0..(48_000 * 2) {
                let x = (n as f32 * TAU * 220.0 / SR).sin() * 0.3;
                if n == 48_000 {
                    assert!(dsp.apply_ui_param(id, value));
                }
                let (l, r) = dsp.process_stereo(x, x);
                window_step = window_step.max((l - previous.0).abs().max((r - previous.1).abs()));
                window_peak = window_peak.max(l.abs()).max(r.abs());
                previous = (l, r);
                if n % 1_000 == 999 && n > 24_000 {
                    let ratio = window_step / window_peak.max(1.0e-6);
                    if n < 48_000 {
                        steady = steady.max(ratio);
                    } else {
                        worst = worst.max(ratio);
                    }
                    window_step = 0.0;
                    window_peak = 0.0;
                }
            }
            assert!(
                worst < steady * 2.5,
                "{id}: the change stepped ({worst} against {steady})"
            );
        }
    }
    #[test]
    fn reset_clears_the_tail() {
        let mut dsp = dsp_with(wet_only(default_params()));
        for _ in 0..4_800 {
            let _ = dsp.process_stereo(0.5, -0.5);
        }
        dsp.reset();
        let (l, r) = dsp.process_stereo(0.0, 0.0);
        assert!(l.abs() < 1.0e-6 && r.abs() < 1.0e-6);
    }

    #[test]
    fn sample_rate_change_keeps_delays_inside_the_new_rings() {
        let mut params = wet_only(default_params());
        params.size = 100.0;
        params.predelay_ms = MAX_PREDELAY_MS;
        let mut dsp = dsp_with(params);
        dsp.set_sample_rate(44_100.0);
        assert_eq!(dsp.params().size, 100.0, "the params survive a rate change");
        for line in &dsp.lines {
            assert!(line.delay + MAX_MOD_MS * 0.001 * 44_100.0 * 2.5 < line.ring.max_cubic_delay());
        }
        assert!(dsp.tuning.predelay + 2.0 < dsp.predelay_l.len() as f32);
        for _ in 0..44_100 {
            let (l, r) = dsp.process_stereo(0.3, -0.3);
            assert!(l.is_finite() && r.is_finite());
        }
    }

    #[test]
    fn wire_update_changes_only_authoritative_params() {
        let mut dsp = Dsp::new(48_000.0);
        assert!(dsp.apply_wire_param(ipc::DECAY_INDEX, 6.0));
        assert_eq!(dsp.params().decay_sec, 6.0);
        assert!(!dsp.apply_wire_param(u32::MAX, 0.0));
        assert!(!dsp.apply_wire_param(ipc::DECAY_INDEX, f32::NAN));
    }

    #[test]
    fn the_wet_cuts_are_what_the_editor_draws() {
        let params = default_params();
        assert!(wet_filter_response_db(&params, 1_000.0, SR).abs() < 0.5);
        assert!(wet_filter_response_db(&params, 30.0, SR) < -12.0);
        assert!(wet_filter_response_db(&params, 19_000.0, SR) < -6.0);
    }

    #[test]
    fn plate_has_no_early_reflections_and_rooms_do() {
        let plate = decay_profile(
            &Params {
                mode: ReverbMode::Plate,
                ..default_params()
            },
            SR,
        );
        assert!(plate.early.iter().all(|e| e.gain == 0.0));
        let room = decay_profile(
            &Params {
                mode: ReverbMode::Room,
                ..default_params()
            },
            SR,
        );
        assert!(room.early.iter().any(|e| e.gain.abs() > 0.05));
        assert!(room.early.iter().all(|e| e.at_ms >= room.predelay_ms));
    }

    /// Every mode, at any size or decay, comes back at about the same
    /// loudness: switching character or stretching the decay changes the
    /// room, not the fader.
    #[test]
    fn every_mode_sits_at_the_same_loudness() {
        for mode in ReverbMode::ALL {
            for (size, decay) in [(20.0f32, 0.5f32), (60.0, 2.4), (100.0, 12.0)] {
                let mut params = wet_only(default_params());
                params.mode = mode;
                params.size = size;
                params.decay_sec = decay;
                let mut dsp = dsp_with(params);
                let mut seed = 0x1234_5678u32;
                let (mut input, mut output) = (0.0f64, 0.0f64);
                for n in 0..(48_000 * 2) {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    let x = (seed >> 9) as f32 / (1u32 << 23) as f32 - 0.5;
                    let (l, r) = dsp.process_stereo(x, x);
                    if n > 36_000 {
                        input += f64::from(x * x);
                        output += f64::from((l * l + r * r) * 0.5);
                    }
                }
                let db = 10.0 * (output / input).log10();
                assert!(
                    (-5.0..=-1.0).contains(&db),
                    "{mode:?} at size {size}, decay {decay}: {db:.1} dB"
                );
            }
        }
    }
}

//! Duckstep — a ducker keyed by the track's own signal, and a tempo-synced
//! volume shaper.
//!
//! ```text
//!            ┌─ SC high-pass ─ SC low-pass ─ level ─ soft knee ─ A/H/R ─┐ (Sidechain)
//! in ─ ─ ─ ─ ┤                                                           ├─ gain
//!            └─ host beat / audio trigger ─ phase ─ shape ─ depth ─ smooth┘ (Shaper)
//!
//! in ─ lookahead ─ [Full | LR4 split: Low or High band] × gain ─ mix ─ monitor ─ output ─ out
//! ```
//!
//! Sidechain mode detects from the insert's own input: the plug-in host has
//! no sidechain bus, so the External key source is kept in the state but
//! does nothing. The Shaper restarts on the host's beat grid (the transport
//! the host hands over each block), or when the input crosses a threshold;
//! effect inserts receive no MIDI, so the MIDI trigger free-runs at the tempo.
//!
//! # Realtime shape
//!
//! Everything is sized in [`Dsp::new`]: the lookahead rings at the longest
//! lookahead, the detector and crossover filters. The audio path indexes and
//! multiplies; a change of mode, band or lookahead dips the output for a few
//! milliseconds and switches at the bottom of the dip rather than clicking.

use biquad::{Biquad, Coefficients, DirectForm1};
use builtin_dsp_core::delay::{DelayRing, Smoothed, smoothing_step};
use builtin_dsp_core::{
    ParamDescriptor, PluginCategory, PluginDescriptor, StereoEffect, clamp, db_to_linear,
    flush_denormal, linear_to_db, make_eq_coefficients, max_filter_frequency, time_constant,
};
use serde::{Deserialize, Serialize};

pub mod ipc;
pub mod presets;
pub mod ui;

pub use ipc::{UI_PARAM_IDS, ui_param_id, ui_param_index};
pub use presets::{
    DETECTOR_PRESETS, DetectorPreset, FactoryPreset, SHAPE_PRESETS, ShapePreset, factory_presets,
};

pub const PLUGIN_ID: &str = "futureboard.duckstep";

/// Shape points a shape can have.
pub const MAX_POINTS: usize = 16;
/// The longest lookahead, in milliseconds.
pub const MAX_LOOKAHEAD_MS: f32 = 10.0;
/// The tempo used until the host reports one.
pub const FALLBACK_TEMPO_BPM: f32 = 120.0;

const CLIP_THRESHOLD: f32 = 1.0;
const RMS_WINDOW_SECONDS: f32 = 0.300;
/// Fast enough that a ducked kick shows as a dip in the level history.
const PEAK_FALL_SECONDS: f32 = 0.120;
/// The detector's peak follower: holds a low kick's cycles together.
const DETECTOR_RELEASE_SECONDS: f32 = 0.015;
/// The detector reading the meter frame carries, held a little longer so the
/// telemetry rate does not miss a short hit.
const DETECTOR_DISPLAY_SECONDS: f32 = 0.080;
/// The shaper's audio trigger: its input follower and re-arm hysteresis.
const TRIGGER_RELEASE_SECONDS: f32 = 0.050;
const TRIGGER_HYSTERESIS_DB: f32 = 3.0;
/// The trigger flash's fall.
const FLASH_SECONDS: f32 = 0.150;
/// Mix / output smoothing.
const SMOOTH_MS: f32 = 20.0;
/// The power crossfade.
const FADE_MS: f32 = 10.0;
/// Each half of the dip a mode, band or lookahead change rides.
const SWITCH_MS: f32 = 4.0;
/// Host samples between control updates (filter retunes, time constants).
const CONTROL_TICK: u32 = 32;
/// Share of the way a moving filter goes per control tick.
const CONTROL_FOLLOW: f32 = 0.2;
/// SC high-pass at or under this is off; SC low-pass at or over the next is off.
const HPF_OFF_HZ: f32 = 20.5;
const LPF_OFF_HZ: f32 = 19_950.0;
/// A synced release is this share of its note value.
const SYNC_RELEASE_SHARE: f32 = 0.35;
const BUTTERWORTH_Q: f32 = std::f32::consts::FRAC_1_SQRT_2;

/// A choice param: wire values are positions in `ALL`, the persisted
/// contract.
macro_rules! wire_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident => $label:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(rename_all = "camelCase")]
        pub enum $name {
            $($variant),+
        }

        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$variant),+];

            pub fn to_wire(self) -> f32 {
                Self::ALL.iter().position(|v| *v == self).unwrap_or(0) as f32
            }

            pub fn from_wire(value: f32) -> Self {
                let index = if value.is_finite() {
                    value.round().max(0.0) as usize
                } else {
                    0
                };
                Self::ALL[index.min(Self::ALL.len() - 1)]
            }

            pub fn label(self) -> &'static str {
                match self {
                    $($name::$variant => $label),+
                }
            }
        }
    };
}

wire_enum!(
    /// What drives the gain.
    Mode { Sidechain => "Sidechain", Shaper => "Shaper" }
);
wire_enum!(
    /// The ducker's key. External needs a sidechain bus the host does not
    /// have yet: it is kept in the state, and the detector listens to the
    /// insert's own input either way.
    Source { External => "External key", Internal => "Internal" }
);
wire_enum!(
    /// The ducker's release, locked to a note value, or free.
    ReleaseSync { Off => "Off", Sixteenth => "1/16", Eighth => "1/8", Quarter => "1/4" }
);
wire_enum!(
    /// What restarts the shaper's cycle. Effect inserts receive no MIDI, so
    /// `Midi` free-runs at the tempo.
    Trigger { Host => "Host", Midi => "MIDI", Audio => "Audio" }
);
wire_enum!(
    /// The shaper's cycle length.
    Rate { Whole => "1/1", Half => "1/2", Quarter => "1/4", Eighth => "1/8", Sixteenth => "1/16" }
);
wire_enum!(
    /// Straight, dotted or triplet cycle.
    Feel { Straight => "Straight", Dotted => "Dotted", Triplet => "Triplet" }
);
wire_enum!(
    /// Which part of the spectrum the gain applies to.
    Band { Full => "Full", Low => "Lows", High => "Highs" }
);
wire_enum!(
    /// The processed signal, or only what the ducking takes away.
    Monitor { Output => "Output", Delta => "Delta" }
);

impl ReleaseSync {
    /// The note value in beats; `None` when the release is free.
    pub fn beats(self) -> Option<f32> {
        match self {
            Self::Off => None,
            Self::Sixteenth => Some(0.25),
            Self::Eighth => Some(0.5),
            Self::Quarter => Some(1.0),
        }
    }
}

impl Rate {
    pub fn beats(self) -> f32 {
        match self {
            Self::Whole => 4.0,
            Self::Half => 2.0,
            Self::Quarter => 1.0,
            Self::Eighth => 0.5,
            Self::Sixteenth => 0.25,
        }
    }
}

impl Feel {
    pub fn factor(self) -> f32 {
        match self {
            Self::Straight => 1.0,
            Self::Dotted => 1.5,
            Self::Triplet => 2.0 / 3.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Params {
    pub power: bool,
    pub mode: Mode,
    /// Sidechain mode: hear the filtered detector instead of the output.
    pub listen: bool,
    pub source: Source,
    pub sc_hpf_hz: f32,
    pub sc_lpf_hz: f32,
    pub threshold_db: f32,
    pub ratio: f32,
    pub knee_db: f32,
    pub attack_ms: f32,
    pub hold_ms: f32,
    pub release_ms: f32,
    pub lookahead_ms: f32,
    /// The most the ducker takes off, dB.
    pub range_db: f32,
    pub release_sync: ReleaseSync,
    pub trigger: Trigger,
    pub rate: Rate,
    pub feel: Feel,
    /// Percent.
    pub depth: f32,
    /// −100…100: bows each segment of the shape.
    pub curve: f32,
    pub smooth_ms: f32,
    /// −50…50 percent of a cycle: the point of the shape the beat (or the
    /// trigger) lands on.
    pub offset: f32,
    pub shaper_threshold_db: f32,
    pub band: Band,
    pub xover_hz: f32,
    /// Percent.
    pub mix: f32,
    pub output_db: f32,
    pub monitor: Monitor,
    /// Points in use, 2…[`MAX_POINTS`]; the first sits at x 0, the last at 1.
    pub point_count: u32,
    pub point_x: [f32; MAX_POINTS],
    pub point_y: [f32; MAX_POINTS],
}

impl Default for Params {
    fn default() -> Self {
        default_params()
    }
}

pub fn default_params() -> Params {
    let mut params = Params {
        power: true,
        mode: Mode::Sidechain,
        listen: false,
        source: Source::Internal,
        sc_hpf_hz: 30.0,
        sc_lpf_hz: 20_000.0,
        threshold_db: -24.0,
        ratio: 8.0,
        knee_db: 6.0,
        attack_ms: 1.0,
        hold_ms: 20.0,
        release_ms: 180.0,
        lookahead_ms: 2.0,
        range_db: 24.0,
        release_sync: ReleaseSync::Off,
        trigger: Trigger::Host,
        rate: Rate::Quarter,
        feel: Feel::Straight,
        depth: 100.0,
        curve: 45.0,
        smooth_ms: 4.0,
        offset: 0.0,
        shaper_threshold_db: -20.0,
        band: Band::Full,
        xover_hz: 150.0,
        mix: 100.0,
        output_db: 0.0,
        monitor: Monitor::Output,
        point_count: 0,
        point_x: [0.0; MAX_POINTS],
        point_y: [0.0; MAX_POINTS],
    };
    params.set_shape(&SHAPE_PRESETS[0]);
    params
}

impl Params {
    /// The points in use, as `(x, y)`.
    pub fn points(&self) -> impl Iterator<Item = (f32, f32)> + '_ {
        let count = (self.point_count as usize).clamp(2, MAX_POINTS);
        (0..count).map(|i| (self.point_x[i], self.point_y[i]))
    }

    /// Takes `preset`'s points and curve.
    pub fn set_shape(&mut self, preset: &ShapePreset) {
        let count = preset.points.len().clamp(2, MAX_POINTS);
        self.point_count = count as u32;
        self.point_x = [0.0; MAX_POINTS];
        self.point_y = [0.0; MAX_POINTS];
        for (i, &(x, y)) in preset.points.iter().take(count).enumerate() {
            self.point_x[i] = x;
            self.point_y[i] = y;
        }
        self.curve = preset.curve;
    }

    /// Whether the shape is `preset`'s.
    pub fn has_shape(&self, preset: &ShapePreset) -> bool {
        let near = |a: f32, b: f32| (a - b).abs() < 1.0e-3;
        self.point_count as usize == preset.points.len()
            && near(self.curve, preset.curve)
            && self
                .points()
                .zip(preset.points)
                .all(|((x, y), &(px, py))| near(x, px) && near(y, py))
    }

    /// Takes `preset`'s detector and envelope settings.
    pub fn set_detector(&mut self, preset: &DetectorPreset) {
        self.threshold_db = preset.threshold_db;
        self.ratio = preset.ratio;
        self.knee_db = preset.knee_db;
        self.attack_ms = preset.attack_ms;
        self.hold_ms = preset.hold_ms;
        self.release_ms = preset.release_ms;
        self.lookahead_ms = preset.lookahead_ms;
        self.range_db = preset.range_db;
        self.release_sync = preset.release_sync;
    }

    /// Whether the detector and envelope are `preset`'s.
    pub fn has_detector(&self, preset: &DetectorPreset) -> bool {
        let near = |a: f32, b: f32| (a - b).abs() <= 1.0e-3 * a.abs().max(1.0);
        near(self.threshold_db, preset.threshold_db)
            && near(self.ratio, preset.ratio)
            && near(self.knee_db, preset.knee_db)
            && near(self.attack_ms, preset.attack_ms)
            && near(self.hold_ms, preset.hold_ms)
            && near(self.release_ms, preset.release_ms)
            && near(self.lookahead_ms, preset.lookahead_ms)
            && near(self.range_db, preset.range_db)
            && self.release_sync == preset.release_sync
    }

    /// One cycle of the shaper, in beats.
    pub fn cycle_beats(&self) -> f32 {
        self.rate.beats() * self.feel.factor()
    }

    /// The release the ducker runs at `tempo_bpm`, ms: synced or free.
    pub fn release_at(&self, tempo_bpm: f32) -> f32 {
        match self.release_sync.beats() {
            Some(beats) => beats * SYNC_RELEASE_SHARE * 60_000.0 / sane_tempo(tempo_bpm),
            None => self.release_ms,
        }
    }

    /// The lookahead the audio path is delayed by, in samples at `rate`; none
    /// while the shaper runs, which has nothing to look ahead for.
    pub fn latency_at(&self, sample_rate: f32) -> usize {
        match self.mode {
            Mode::Sidechain => lookahead_samples(self.lookahead_ms, sample_rate),
            Mode::Shaper => 0,
        }
    }
}

fn sane_tempo(tempo_bpm: f32) -> f32 {
    if tempo_bpm.is_finite() && tempo_bpm >= 1.0 {
        tempo_bpm.min(999.0)
    } else {
        FALLBACK_TEMPO_BPM
    }
}

fn lookahead_samples(ms: f32, sample_rate: f32) -> usize {
    (clamp(ms, 0.0, MAX_LOOKAHEAD_MS) * 0.001 * sample_rate.max(1.0)).round() as usize
}

/// The shape at cycle position `x` (0…1), before depth: the points joined by
/// segments bowed by the curve. The editor draws this very function.
pub fn shape_at(params: &Params, x: f32) -> f32 {
    let count = (params.point_count as usize).clamp(2, MAX_POINTS);
    let xs = &params.point_x;
    let ys = &params.point_y;
    let curve = clamp(params.curve, -100.0, 100.0);
    let exponent = if curve >= 0.0 {
        1.0 / (1.0 + curve / 50.0)
    } else {
        1.0 - curve / 50.0
    };
    if x <= xs[0] {
        return clamp(ys[0], 0.0, 1.0);
    }
    for i in 1..count {
        if x <= xs[i] {
            let (ax, ay, bx, by) = (xs[i - 1], ys[i - 1], xs[i], ys[i]);
            let span = bx - ax;
            let f = if span < 1.0e-6 {
                1.0
            } else {
                clamp((x - ax) / span, 0.0, 1.0)
            };
            let eased = if by >= ay {
                f.powf(exponent)
            } else {
                1.0 - (1.0 - f).powf(exponent)
            };
            return clamp(ay + (by - ay) * eased, 0.0, 1.0);
        }
    }
    clamp(ys[count - 1], 0.0, 1.0)
}

/// Where in the shape the shaper is at cycle `phase` (0 on the beat or the
/// trigger): the offset moves the point of the shape the restart lands on.
pub fn shape_position(params: &Params, phase: f32) -> f32 {
    (phase + clamp(params.offset, -50.0, 50.0) / 100.0).rem_euclid(1.0)
}

/// The shaper's gain at cycle position `x`: the shape scaled by depth.
pub fn shaper_gain(params: &Params, x: f32) -> f32 {
    let depth = clamp(params.depth, 0.0, 100.0) / 100.0;
    1.0 - depth * (1.0 - shape_at(params, x))
}

/// The ducker's static curve: dB to take off for a detector at `level_db`,
/// soft-kneed and capped at the range.
pub fn static_reduction(params: &Params, level_db: f32) -> f32 {
    let over = level_db - params.threshold_db;
    let knee = params.knee_db.max(0.0);
    let slope = 1.0 - 1.0 / params.ratio.max(1.0);
    let reduction = if knee > 0.0 && 2.0 * over.abs() <= knee {
        slope * (over + knee * 0.5).powi(2) / (2.0 * knee)
    } else if over > 0.0 {
        slope * over
    } else {
        0.0
    };
    clamp(reduction, 0.0, params.range_db.max(0.0))
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
    param("mode", "Mode", 0.0, 0.0, 1.0, "enum"),
    param("listen", "Listen", 0.0, 0.0, 1.0, "bool"),
    param("source", "Source", 1.0, 0.0, 1.0, "enum"),
    param("scHpfHz", "SC High-Pass", 30.0, 20.0, 500.0, "Hz"),
    param("scLpfHz", "SC Low-Pass", 20_000.0, 100.0, 20_000.0, "Hz"),
    param("thresholdDb", "Threshold", -24.0, -60.0, 0.0, "dB"),
    param("ratio", "Ratio", 8.0, 1.0, 20.0, ":1"),
    param("kneeDb", "Knee", 6.0, 0.0, 24.0, "dB"),
    param("attackMs", "Attack", 1.0, 0.1, 50.0, "ms"),
    param("holdMs", "Hold", 20.0, 0.0, 200.0, "ms"),
    param("releaseMs", "Release", 180.0, 10.0, 1_000.0, "ms"),
    param("lookaheadMs", "Lookahead", 2.0, 0.0, 10.0, "ms"),
    param("rangeDb", "Range", 24.0, 0.0, 48.0, "dB"),
    param("releaseSync", "Release Sync", 0.0, 0.0, 3.0, "enum"),
    param("trigger", "Trigger", 0.0, 0.0, 2.0, "enum"),
    param("rate", "Rate", 2.0, 0.0, 4.0, "enum"),
    param("feel", "Feel", 0.0, 0.0, 2.0, "enum"),
    param("depth", "Depth", 100.0, 0.0, 100.0, "%"),
    param("curve", "Curve", 45.0, -100.0, 100.0, "%"),
    param("smoothMs", "Smooth", 4.0, 0.0, 50.0, "ms"),
    param("offset", "Offset", 0.0, -50.0, 50.0, "%"),
    param(
        "shaperThresholdDb",
        "Trigger Threshold",
        -20.0,
        -60.0,
        0.0,
        "dB",
    ),
    param("band", "Band", 0.0, 0.0, 2.0, "enum"),
    param("xoverHz", "Crossover", 150.0, 40.0, 2_000.0, "Hz"),
    param("mix", "Mix", 100.0, 0.0, 100.0, "%"),
    param("outputDb", "Output", 0.0, -12.0, 12.0, "dB"),
    param("monitor", "Monitor", 0.0, 0.0, 1.0, "enum"),
];

pub fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PLUGIN_ID,
        name: "Duckstep",
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
    /// The gain taken off now, dB (positive; 0 when bypassed).
    pub const GR_DB: usize = 0;
    /// The shaper's cycle position, 0…1, before the offset.
    pub const PHASE: usize = 1;
    /// 1 as a cycle restarts, falling back to 0.
    pub const TRIGGER: usize = 2;
    /// The host tempo the plug-in runs at, BPM (0 until the host reports).
    pub const TEMPO_BPM: usize = 3;
    /// 1 while the host transport plays.
    pub const PLAYING: usize = 4;
    /// The detector's level now, dBFS.
    pub const DETECTOR_DB: usize = 5;
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct MeterFrame {
    pub in_peak: f32,
    pub in_rms: f32,
    pub out_peak: f32,
    pub out_rms: f32,
    pub gain_reduction_db: f32,
    /// The detector's level (linear peak): what the host's meter frame
    /// carries in slot 0's input peak, so the editor's history records it.
    pub detector_peak: f32,
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

fn coefficients(kind: &str, hz: f32, sample_rate: f32) -> Coefficients<f32> {
    make_eq_coefficients(kind, hz, 0.0, BUTTERWORTH_Q, sample_rate).unwrap_or(neutral())
}

/// One channel's detector filters.
#[derive(Debug, Clone)]
struct DetectorFilter {
    hpf: DirectForm1<f32>,
    lpf: DirectForm1<f32>,
}

impl DetectorFilter {
    fn new() -> Self {
        Self {
            hpf: DirectForm1::<f32>::new(neutral()),
            lpf: DirectForm1::<f32>::new(neutral()),
        }
    }

    fn reset(&mut self) {
        self.hpf.reset_state();
        self.lpf.reset_state();
    }

    #[inline]
    fn run(&mut self, x: f32, hpf_on: bool, lpf_on: bool) -> f32 {
        let mut y = x;
        if hpf_on {
            y = self.hpf.run(y);
        }
        if lpf_on {
            y = self.lpf.run(y);
        }
        y
    }
}

/// One channel's 4th-order Linkwitz–Riley split: two Butterworth sections a
/// side, so low plus high is an all-pass of the input.
#[derive(Debug, Clone)]
struct Split {
    low: [DirectForm1<f32>; 2],
    high: [DirectForm1<f32>; 2],
}

impl Split {
    fn new() -> Self {
        Self {
            low: [
                DirectForm1::<f32>::new(neutral()),
                DirectForm1::<f32>::new(neutral()),
            ],
            high: [
                DirectForm1::<f32>::new(neutral()),
                DirectForm1::<f32>::new(neutral()),
            ],
        }
    }

    fn tune(&mut self, hz: f32, sample_rate: f32) {
        let low = coefficients("lowpass", hz, sample_rate);
        let high = coefficients("highpass", hz, sample_rate);
        for f in &mut self.low {
            f.update_coefficients(low);
        }
        for f in &mut self.high {
            f.update_coefficients(high);
        }
    }

    fn reset(&mut self) {
        for f in self.low.iter_mut().chain(self.high.iter_mut()) {
            f.reset_state();
        }
    }

    #[inline]
    fn run(&mut self, x: f32) -> (f32, f32) {
        let low = self.low[0].run(x);
        let low = self.low[1].run(low);
        let high = self.high[0].run(x);
        let high = self.high[1].run(high);
        (low, high)
    }
}

/// What the dip switches between: the settings a click would come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Layout {
    mode: Mode,
    band: Band,
    latency: usize,
}

impl Layout {
    fn of(params: &Params, sample_rate: f32) -> Self {
        Self {
            mode: params.mode,
            band: params.band,
            latency: params.latency_at(sample_rate),
        }
    }
}

/// Values the control tick derives from the params, so the audio path only
/// multiplies.
#[derive(Debug, Clone, Copy)]
struct Control {
    hpf_hz: f32,
    lpf_hz: f32,
    xover_hz: f32,
    hpf_on: bool,
    lpf_on: bool,
    attack: f32,
    release: f32,
    hold: u32,
    shape_step: f32,
    phase_step: f64,
}

#[derive(Debug, Clone)]
pub struct Dsp {
    params: Params,
    /// Audio has run since construction or a reset; until it has, edits land
    /// at once.
    started: bool,
    sample_rate: f32,
    tempo: f32,
    /// Whether the host has reported a tempo yet.
    tempo_known: bool,
    playing: bool,
    /// The block's quarter-note position, waiting for its first sample.
    ppq_pending: Option<f64>,
    ring_l: DelayRing,
    ring_r: DelayRing,
    det_ring_l: DelayRing,
    det_ring_r: DelayRing,
    detect: [DetectorFilter; 2],
    split: [Split; 2],
    control: Control,
    tick: u32,
    follow: f32,
    follow_coeff: f32,
    display: f32,
    display_coeff: f32,
    gr_db: f32,
    hold_left: u32,
    phase: f64,
    shape_gain: f32,
    trigger_follow: f32,
    trigger_coeff: f32,
    armed: bool,
    flash: f32,
    flash_coeff: f32,
    /// The gain applied on the last sample, for the meters.
    applied: f32,
    active: Layout,
    switch_env: f32,
    switch_step: f32,
    mix: Smoothed,
    output: Smoothed,
    smooth_step: f32,
    power: Fade,
    meters: Meters,
}

impl Dsp {
    pub fn new(sample_rate: f32) -> Self {
        let sr = sample_rate.max(1.0);
        let params = default_params();
        let ring = || DelayRing::for_ms(MAX_LOOKAHEAD_MS, sr);
        let mut dsp = Self {
            started: false,
            sample_rate: sr,
            tempo: FALLBACK_TEMPO_BPM,
            tempo_known: false,
            playing: false,
            ppq_pending: None,
            ring_l: ring(),
            ring_r: ring(),
            det_ring_l: ring(),
            det_ring_r: ring(),
            detect: [DetectorFilter::new(), DetectorFilter::new()],
            split: [Split::new(), Split::new()],
            control: Control {
                hpf_hz: params.sc_hpf_hz,
                lpf_hz: params.sc_lpf_hz,
                xover_hz: params.xover_hz,
                hpf_on: false,
                lpf_on: false,
                attack: 0.0,
                release: 0.0,
                hold: 0,
                shape_step: 1.0,
                phase_step: 0.0,
            },
            tick: 0,
            follow: 0.0,
            follow_coeff: time_constant(sr, DETECTOR_RELEASE_SECONDS),
            display: 0.0,
            display_coeff: time_constant(sr, DETECTOR_DISPLAY_SECONDS),
            gr_db: 0.0,
            hold_left: 0,
            phase: 0.0,
            shape_gain: 1.0,
            trigger_follow: 0.0,
            trigger_coeff: time_constant(sr, TRIGGER_RELEASE_SECONDS),
            armed: true,
            flash: 0.0,
            flash_coeff: time_constant(sr, FLASH_SECONDS),
            applied: 1.0,
            active: Layout::of(&params, sr),
            switch_env: 1.0,
            switch_step: ramp_step(SWITCH_MS, sr),
            mix: Smoothed::at(1.0),
            output: Smoothed::at(1.0),
            smooth_step: smoothing_step(SMOOTH_MS, sr),
            power: Fade::new(true, sr),
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

    /// The block's transport: the host tempo, its position in quarter notes
    /// and whether it plays. The shaper locks its cycle to the position on
    /// the block's first sample.
    pub fn set_transport(&mut self, tempo_bpm: f32, ppq_position: f64, playing: bool) {
        if tempo_bpm.is_finite() && tempo_bpm >= 1.0 {
            self.tempo = tempo_bpm.min(999.0);
            self.tempo_known = true;
        }
        self.playing = playing;
        self.ppq_pending = (playing && ppq_position.is_finite()).then_some(ppq_position);
    }

    /// The lookahead the audio path runs behind its input, in samples at the
    /// host rate: what the host compensates. The bypassed signal runs at it
    /// too.
    pub fn latency_samples(&self) -> usize {
        self.params.latency_at(self.sample_rate)
    }

    pub fn meter_frame(&self) -> MeterFrame {
        MeterFrame {
            in_peak: self.meters.in_peak,
            in_rms: self.meters.in_ms.max(0.0).sqrt(),
            out_peak: self.meters.out_peak,
            out_rms: self.meters.out_ms.max(0.0).sqrt(),
            gain_reduction_db: self.reduction_db(),
            detector_peak: self.display,
            in_clip: self.meters.in_clip,
            out_clip: self.meters.out_clip,
        }
    }

    pub fn clear_clip(&mut self) {
        self.meters.in_clip = false;
        self.meters.out_clip = false;
    }

    /// The gain taken off now, dB; 0 while bypassed.
    pub fn reduction_db(&self) -> f32 {
        if self.params.power {
            (-linear_to_db(self.applied)).clamp(0.0, 60.0)
        } else {
            0.0
        }
    }

    /// The shaper's cycle position now, 0…1.
    pub fn phase(&self) -> f32 {
        self.phase as f32
    }

    /// The editor's telemetry block ([`telemetry`]).
    pub fn telemetry(&self) -> [f32; telemetry::SLOTS] {
        let mut block = [0.0; telemetry::SLOTS];
        block[telemetry::GR_DB] = self.reduction_db();
        block[telemetry::PHASE] = self.phase as f32;
        block[telemetry::TRIGGER] = self.flash;
        block[telemetry::TEMPO_BPM] = if self.tempo_known { self.tempo } else { 0.0 };
        block[telemetry::PLAYING] = f32::from(u8::from(self.playing));
        block[telemetry::DETECTOR_DB] = linear_to_db(self.display).max(-120.0);
        block
    }

    fn apply_params(&mut self) {
        let p = &self.params;
        self.mix.target = clamp(p.mix, 0.0, 100.0) / 100.0;
        self.output.target = db_to_linear(p.output_db);
        self.power.set(p.power);
    }

    /// Land everything on its target: values, power, layout, filters.
    fn settle(&mut self) {
        self.mix.settle();
        self.output.settle();
        self.power.settle();
        self.control.hpf_hz = self.params.sc_hpf_hz;
        self.control.lpf_hz = self.params.sc_lpf_hz;
        self.control.xover_hz = self.params.xover_hz;
        self.adopt_layout();
        self.switch_env = 1.0;
    }

    /// Switches to the requested layout from a clean state, retuned for it.
    fn adopt_layout(&mut self) {
        self.active = Layout::of(&self.params, self.sample_rate);
        for split in &mut self.split {
            split.reset();
        }
        self.retune();
        self.derive();
    }

    fn retune(&mut self) {
        let sr = self.sample_rate;
        let c = &mut self.control;
        c.hpf_on = c.hpf_hz > HPF_OFF_HZ;
        c.lpf_on = c.lpf_hz < LPF_OFF_HZ && c.lpf_hz < max_filter_frequency(sr);
        let hpf = coefficients("highpass", c.hpf_hz, sr);
        let lpf = coefficients("lowpass", c.lpf_hz, sr);
        for d in &mut self.detect {
            d.hpf.update_coefficients(hpf);
            d.lpf.update_coefficients(lpf);
        }
        let xover = c.xover_hz;
        for split in &mut self.split {
            split.tune(xover, sr);
        }
    }

    /// The time constants and steps the params and the tempo come to.
    fn derive(&mut self) {
        let sr = self.sample_rate;
        let p = &self.params;
        let c = &mut self.control;
        c.attack = time_constant(sr, clamp(p.attack_ms, 0.01, 1_000.0) * 0.001);
        c.release = time_constant(sr, clamp(p.release_at(self.tempo), 1.0, 10_000.0) * 0.001);
        c.hold = (clamp(p.hold_ms, 0.0, 1_000.0) * 0.001 * sr) as u32;
        c.shape_step = if p.smooth_ms <= 0.01 {
            1.0
        } else {
            smoothing_step(p.smooth_ms, sr)
        };
        let beats_per_sample = f64::from(self.tempo) / 60.0 / f64::from(sr);
        c.phase_step = beats_per_sample / f64::from(p.cycle_beats().max(1.0e-3));
    }

    /// Moves the filters a step toward their targets, retuning when they
    /// moved, and refreshes the time constants.
    fn control_tick(&mut self) {
        let mut moved = false;
        let c = &mut self.control;
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
        follow_log(&mut c.hpf_hz, self.params.sc_hpf_hz);
        follow_log(&mut c.lpf_hz, self.params.sc_lpf_hz);
        follow_log(&mut c.xover_hz, self.params.xover_hz);
        if moved {
            self.retune();
        }
        self.derive();
    }

    /// The ducker's gain for this sample, from the detector level.
    #[inline]
    fn duck(&mut self, level: f32) -> f32 {
        self.follow = if level > self.follow {
            level
        } else {
            flush_denormal(self.follow * self.follow_coeff)
        };
        let target = static_reduction(&self.params, linear_to_db(self.follow));
        let c = self.control;
        if target > self.gr_db {
            self.gr_db = target + (self.gr_db - target) * c.attack;
            self.hold_left = c.hold;
        } else if self.hold_left > 0 {
            self.hold_left -= 1;
        } else {
            self.gr_db = flush_denormal(target + (self.gr_db - target) * c.release);
        }
        db_to_linear(-self.gr_db)
    }

    /// The shaper's gain for this sample: the cycle moves on, restarting on
    /// the beat or a trigger.
    #[inline]
    fn shape(&mut self, level: f32) -> f32 {
        let before = self.phase;
        let mut restarted = false;
        match self.params.trigger {
            Trigger::Host => {
                if let Some(ppq) = self.ppq_pending.take() {
                    let cycle = f64::from(self.params.cycle_beats().max(1.0e-3));
                    self.phase = (ppq / cycle).rem_euclid(1.0);
                } else {
                    self.phase += self.control.phase_step;
                }
            }
            Trigger::Midi => {
                self.ppq_pending = None;
                self.phase += self.control.phase_step;
            }
            Trigger::Audio => {
                self.ppq_pending = None;
                self.trigger_follow = if level > self.trigger_follow {
                    level
                } else {
                    flush_denormal(self.trigger_follow * self.trigger_coeff)
                };
                let db = linear_to_db(self.trigger_follow);
                let threshold = self.params.shaper_threshold_db;
                if self.armed && db >= threshold {
                    self.armed = false;
                    self.phase = 0.0;
                    restarted = true;
                } else {
                    if !self.armed && db < threshold - TRIGGER_HYSTERESIS_DB {
                        self.armed = true;
                    }
                    self.phase += self.control.phase_step;
                }
            }
        }
        if self.phase >= 1.0 {
            self.phase = self.phase.fract();
        }
        if before - self.phase > 0.5 {
            restarted = true;
        }
        self.flash = if restarted {
            1.0
        } else {
            flush_denormal(self.flash * self.flash_coeff)
        };
        let x = shape_position(&self.params, self.phase as f32);
        let target = shaper_gain(&self.params, x);
        self.shape_gain += (target - self.shape_gain) * self.control.shape_step;
        self.shape_gain
    }

    /// One channel through the band split and the gain: what is heard
    /// unprocessed (the split's sum, or the input) and what is processed.
    #[inline]
    fn apply_band(&mut self, channel: usize, x: f32, gain: f32) -> (f32, f32) {
        match self.active.band {
            Band::Full => (x, x * gain),
            Band::Low => {
                let (low, high) = self.split[channel].run(x);
                (low + high, low * gain + high)
            }
            Band::High => {
                let (low, high) = self.split[channel].run(x);
                (low + high, low + high * gain)
            }
        }
    }
}

impl StereoEffect for Dsp {
    fn reset(&mut self) {
        self.started = false;
        self.ring_l.clear();
        self.ring_r.clear();
        self.det_ring_l.clear();
        self.det_ring_r.clear();
        for d in &mut self.detect {
            d.reset();
        }
        self.follow = 0.0;
        self.display = 0.0;
        self.gr_db = 0.0;
        self.hold_left = 0;
        self.phase = 0.0;
        self.shape_gain = 1.0;
        self.trigger_follow = 0.0;
        self.armed = true;
        self.flash = 0.0;
        self.applied = 1.0;
        self.meters.reset();
        self.settle();
    }

    fn set_sample_rate(&mut self, sample_rate: f32) {
        let sr = sample_rate.max(1.0);
        self.sample_rate = sr;
        let ring = || DelayRing::for_ms(MAX_LOOKAHEAD_MS, sr);
        self.ring_l = ring();
        self.ring_r = ring();
        self.det_ring_l = ring();
        self.det_ring_r = ring();
        self.meters = Meters::new(sr);
        self.follow_coeff = time_constant(sr, DETECTOR_RELEASE_SECONDS);
        self.display_coeff = time_constant(sr, DETECTOR_DISPLAY_SECONDS);
        self.trigger_coeff = time_constant(sr, TRIGGER_RELEASE_SECONDS);
        self.flash_coeff = time_constant(sr, FLASH_SECONDS);
        self.smooth_step = smoothing_step(SMOOTH_MS, sr);
        self.power.step = ramp_step(FADE_MS, sr);
        self.switch_step = ramp_step(SWITCH_MS, sr);
        self.apply_params();
        self.settle();
    }

    fn process_stereo(&mut self, left: f32, right: f32) -> (f32, f32) {
        self.started = true;

        // A new mode, band or lookahead: dip, switch at the bottom, come
        // back up.
        if Layout::of(&self.params, self.sample_rate) != self.active {
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
        let mix = glide(&mut self.mix, step);
        let output = glide(&mut self.output, step);

        // The detector hears the input now; the audio runs the lookahead
        // behind it.
        let (hpf_on, lpf_on) = (self.control.hpf_on, self.control.lpf_on);
        let det_l = self.detect[0].run(left, hpf_on, lpf_on);
        let det_r = self.detect[1].run(right, hpf_on, lpf_on);
        self.ring_l.push(left);
        self.ring_r.push(right);
        self.det_ring_l.push(det_l);
        self.det_ring_r.push(det_r);
        let at = self.active.latency + 1;
        let (dry_l, dry_r) = (self.ring_l.at(at), self.ring_r.at(at));

        let gain = match self.active.mode {
            Mode::Sidechain => {
                let level = det_l.abs().max(det_r.abs());
                self.display = if level > self.display {
                    level
                } else {
                    flush_denormal(self.display * self.display_coeff)
                };
                self.duck(level)
            }
            Mode::Shaper => {
                let level = left.abs().max(right.abs());
                self.display = if level > self.display {
                    level
                } else {
                    flush_denormal(self.display * self.display_coeff)
                };
                self.shape(level)
            }
        };
        self.applied = blend(1.0, gain, mix);

        let (ref_l, wet_l) = self.apply_band(0, dry_l, gain);
        let (ref_r, wet_r) = self.apply_band(1, dry_r, gain);
        let processed_l = ref_l + (wet_l - ref_l) * mix;
        let processed_r = ref_r + (wet_r - ref_r) * mix;
        let (mut pl, mut pr) = match self.params.monitor {
            Monitor::Output => (processed_l, processed_r),
            Monitor::Delta => (ref_l - processed_l, ref_r - processed_r),
        };
        if self.params.listen && self.active.mode == Mode::Sidechain {
            pl = self.det_ring_l.at(at);
            pr = self.det_ring_r.at(at);
        }

        let on = self.power.next();
        let envelope = ease(self.switch_env);
        let mut out_l = blend(dry_l, pl * output, on);
        let mut out_r = blend(dry_r, pr * output, on);
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
    use std::f32::consts::TAU;

    const SR: f32 = 48_000.0;

    fn run(dsp: &mut Dsp, samples: usize, mut input: impl FnMut(usize) -> f32) -> Vec<f32> {
        (0..samples)
            .map(|n| {
                let x = input(n);
                dsp.process_stereo(x, x).0
            })
            .collect()
    }

    fn peak(samples: &[f32]) -> f32 {
        samples.iter().fold(0.0f32, |p, s| p.max(s.abs()))
    }

    fn sine(hz: f32, level: f32) -> impl FnMut(usize) -> f32 {
        move |n| level * (TAU * hz * n as f32 / SR).sin()
    }

    #[test]
    fn the_static_curve_is_soft_kneed_and_capped_at_the_range() {
        let mut p = default_params();
        p.threshold_db = -24.0;
        p.ratio = 4.0;
        p.knee_db = 0.0;
        p.range_db = 48.0;
        assert_eq!(static_reduction(&p, -30.0), 0.0);
        assert!((static_reduction(&p, -12.0) - 9.0).abs() < 1.0e-4);
        p.knee_db = 6.0;
        let at_threshold = static_reduction(&p, -24.0);
        assert!(at_threshold > 0.0 && at_threshold < 1.0, "{at_threshold}");
        p.range_db = 3.0;
        assert_eq!(static_reduction(&p, 0.0), 3.0);
    }

    #[test]
    fn the_shape_follows_the_points_and_depth() {
        let mut p = default_params();
        // Pump starts at the bottom and ends up open.
        assert!(shape_at(&p, 0.0) < 1.0e-6);
        assert!((shape_at(&p, 1.0) - 1.0).abs() < 1.0e-6);
        p.depth = 60.0;
        assert!((shaper_gain(&p, 0.0) - 0.4).abs() < 1.0e-5);
        p.set_shape(&SHAPE_PRESETS[3]); // Gate: open, then shut at half way.
        p.depth = 100.0;
        assert!((shaper_gain(&p, 0.25) - 1.0).abs() < 1.0e-5);
        assert!(shaper_gain(&p, 0.75) < 1.0e-5);
        // A straight segment with no curve is linear.
        p.point_count = 2;
        p.point_x[..2].copy_from_slice(&[0.0, 1.0]);
        p.point_y[..2].copy_from_slice(&[0.0, 1.0]);
        p.curve = 0.0;
        assert!((shape_at(&p, 0.3) - 0.3).abs() < 1.0e-5);
        p.curve = 50.0;
        assert!(shape_at(&p, 0.3) > 0.3, "a positive curve bows a rise up");
    }

    #[test]
    fn a_loud_burst_is_ducked_by_the_static_curve_and_released() {
        let mut dsp = Dsp::new(SR);
        let mut p = default_params();
        p.lookahead_ms = 0.0;
        p.hold_ms = 0.0;
        p.release_ms = 50.0;
        p.knee_db = 0.0;
        p.ratio = 20.0;
        p.range_db = 48.0;
        p.sc_hpf_hz = 20.0;
        dsp.set_params(p.clone());
        // A −6 dBFS tone for 200 ms: 18 dB over at 20:1 takes ~17 dB off.
        let loud = run(&mut dsp, 9_600, sine(1_000.0, 0.5));
        let expected = static_reduction(&p, linear_to_db(0.5));
        let gr = dsp.reduction_db();
        assert!((gr - expected).abs() < 1.0, "gr {gr}, curve {expected}");
        assert!(peak(&loud[8_000..]) < 0.5 * db_to_linear(-expected + 1.5));
        // Silence: it lets go within a few release times.
        let _ = run(&mut dsp, 24_000, |_| 0.0);
        assert!(dsp.reduction_db() < 0.1, "{}", dsp.reduction_db());
    }

    #[test]
    fn the_range_caps_the_reduction() {
        let mut dsp = Dsp::new(SR);
        let mut p = default_params();
        p.range_db = 6.0;
        p.threshold_db = -60.0;
        p.ratio = 20.0;
        dsp.set_params(p);
        let _ = run(&mut dsp, 9_600, sine(500.0, 0.9));
        let gr = dsp.reduction_db();
        assert!(gr <= 6.0 + 1.0e-3 && gr > 5.5, "{gr}");
    }

    #[test]
    fn a_synced_release_follows_the_tempo() {
        let mut p = default_params();
        p.release_sync = ReleaseSync::Eighth;
        assert!((p.release_at(120.0) - 0.5 * 0.35 * 500.0).abs() < 1.0e-3);
        assert!((p.release_at(60.0) - 0.5 * 0.35 * 1_000.0).abs() < 1.0e-3);
        p.release_sync = ReleaseSync::Off;
        assert_eq!(p.release_at(60.0), p.release_ms);
    }

    #[test]
    fn latency_is_the_lookahead_in_sidechain_mode_only() {
        let mut dsp = Dsp::new(SR);
        assert_eq!(dsp.latency_samples(), 96, "2 ms at 48 kHz");
        assert!(dsp.apply_ui_param("lookaheadMs", 10.0));
        assert_eq!(dsp.latency_samples(), 480);
        assert!(dsp.apply_ui_param("mode", Mode::Shaper.to_wire()));
        assert_eq!(dsp.latency_samples(), 0);
    }

    #[test]
    fn power_off_is_transparent_at_the_reported_latency() {
        let mut dsp = Dsp::new(SR);
        let mut p = default_params();
        p.power = false;
        p.lookahead_ms = 1.0;
        dsp.set_params(p);
        let latency = dsp.latency_samples();
        let mut input = sine(220.0, 0.8);
        let x: Vec<f32> = (0..2_000).map(&mut input).collect();
        let y: Vec<f32> = x.iter().map(|&s| dsp.process_stereo(s, s).0).collect();
        for n in latency..2_000 {
            assert!((y[n] - x[n - latency]).abs() < 1.0e-6);
        }
    }

    #[test]
    fn the_host_beat_locks_the_shaper_phase() {
        let mut dsp = Dsp::new(SR);
        assert!(dsp.apply_ui_param("mode", Mode::Shaper.to_wire()));
        // Rate 1/4: one cycle a beat. Half way through beat 8.
        dsp.set_transport(120.0, 8.5, true);
        let _ = dsp.process_stereo(0.0, 0.0);
        assert!((dsp.phase() - 0.5).abs() < 1.0e-3, "{}", dsp.phase());
        // A quarter beat later at 120 BPM is 6000 samples.
        let _ = run(&mut dsp, 5_999, |_| 0.0);
        assert!((dsp.phase() - 0.75).abs() < 1.0e-3, "{}", dsp.phase());
        // Re-locked by the next block's position.
        dsp.set_transport(120.0, 3.1, true);
        let _ = dsp.process_stereo(0.0, 0.0);
        assert!((dsp.phase() - 0.1).abs() < 1.0e-3, "{}", dsp.phase());
    }

    #[test]
    fn pump_ducks_on_the_beat_and_opens_after_it() {
        let mut dsp = Dsp::new(SR);
        let mut p = default_params();
        p.mode = Mode::Shaper;
        p.smooth_ms = 0.0;
        dsp.set_params(p);
        dsp.set_transport(120.0, 4.0, true);
        let out = run(&mut dsp, 24_000, |_| 0.5);
        assert!(out[0].abs() < 0.02, "on the beat: {}", out[0]);
        assert!(
            (out[23_000] - 0.5).abs() < 0.02,
            "late in the beat: {}",
            out[23_000]
        );
        assert!(dsp.reduction_db() < 0.5);
    }

    #[test]
    fn the_offset_moves_where_the_beat_lands_in_the_shape() {
        let mut p = default_params();
        assert_eq!(shape_position(&p, 0.0), 0.0);
        p.offset = 50.0;
        assert!((shape_position(&p, 0.0) - 0.5).abs() < 1.0e-6);
        p.offset = -25.0;
        assert!((shape_position(&p, 0.1) - 0.85).abs() < 1.0e-6);
    }

    #[test]
    fn an_audio_trigger_restarts_the_cycle() {
        let mut dsp = Dsp::new(SR);
        let mut p = default_params();
        p.mode = Mode::Shaper;
        p.trigger = Trigger::Audio;
        p.shaper_threshold_db = -20.0;
        dsp.set_params(p);
        let _ = run(&mut dsp, 10_000, |_| 0.0);
        let free = dsp.phase();
        assert!(free > 0.3, "free-runs at the tempo: {free}");
        let _ = dsp.process_stereo(0.5, 0.5);
        assert!(dsp.phase() < 1.0e-6, "restarted: {}", dsp.phase());
        assert_eq!(dsp.telemetry()[telemetry::TRIGGER], 1.0);
        // Held above threshold it does not restart again.
        let _ = run(&mut dsp, 1_000, |_| 0.5);
        assert!(dsp.phase() > 0.0);
    }

    #[test]
    fn the_low_band_ducks_lows_and_leaves_highs() {
        let process = |hz: f32| {
            let mut dsp = Dsp::new(SR);
            let mut p = default_params();
            p.mode = Mode::Shaper;
            p.band = Band::Low;
            p.xover_hz = 300.0;
            p.depth = 100.0;
            p.smooth_ms = 0.0;
            p.point_count = 2;
            p.point_x[..2].copy_from_slice(&[0.0, 1.0]);
            p.point_y[..2].copy_from_slice(&[0.0, 0.0]);
            dsp.set_params(p);
            let out = run(&mut dsp, 24_000, sine(hz, 0.5));
            peak(&out[12_000..])
        };
        assert!(process(50.0) < 0.05, "lows ducked");
        assert!(process(5_000.0) > 0.45, "highs pass");
    }

    #[test]
    fn delta_is_what_the_ducking_removed() {
        let render = |monitor: Monitor| {
            let mut dsp = Dsp::new(SR);
            let mut p = default_params();
            p.mode = Mode::Shaper;
            p.smooth_ms = 0.0;
            p.depth = 50.0;
            p.monitor = monitor;
            dsp.set_params(p);
            dsp.set_transport(120.0, 0.0, true);
            run(&mut dsp, 4_000, sine(300.0, 0.5))
        };
        let out = render(Monitor::Output);
        let delta = render(Monitor::Delta);
        let mut input = sine(300.0, 0.5);
        for n in 0..4_000 {
            let x = input(n);
            assert!((out[n] + delta[n] - x).abs() < 1.0e-5, "at {n}");
        }
    }

    #[test]
    fn listen_plays_the_filtered_detector() {
        let mut dsp = Dsp::new(SR);
        let mut p = default_params();
        p.listen = true;
        p.sc_hpf_hz = 500.0;
        p.lookahead_ms = 0.0;
        dsp.set_params(p);
        let out = run(&mut dsp, 9_600, sine(40.0, 0.5));
        assert!(
            peak(&out[4_800..]) < 0.05,
            "the high-pass takes the lows out"
        );
    }

    #[test]
    fn output_stays_finite() {
        let mut dsp = Dsp::new(SR);
        let mut p = default_params();
        p.band = Band::High;
        p.lookahead_ms = 10.0;
        p.attack_ms = 0.1;
        p.release_sync = ReleaseSync::Sixteenth;
        dsp.set_params(p);
        dsp.set_transport(f32::NAN, f64::NAN, true);
        let mut seed = 1u32;
        let out = run(&mut dsp, 48_000, |_| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            (seed as f32 / u32::MAX as f32) * 2.0 - 1.0
        });
        assert!(out.iter().all(|s| s.is_finite()));
        let block = dsp.telemetry();
        assert!(block.iter().all(|s| s.is_finite()));
        assert_eq!(block[telemetry::TEMPO_BPM], 0.0, "no tempo reported yet");
    }
}

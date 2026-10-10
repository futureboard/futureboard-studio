//! EchoSpace — stereo, ping-pong and mono delay.
//!
//! ```txt
//!              ┌──────────── feedback · cross (normalised, smoothed) ─────────────┐
//! input ─ duck ┴→ + → low cut → high cut → diffusion → saturation → line ──┬──→ wet
//! sense                                                                       │
//!   └──────────────────────────────── ducks the wet ─────────────────────────┘
//! ```
//!
//! The tone stage sits on the write path, the way a tape loop's record head
//! does: the first repeat already carries the cuts and the drive, and every
//! pass after it a little more.
//!
//! A time change never clicks and never warbles: the line keeps reading at
//! its old length while a second read head fades in at the new one, which
//! is also how a synced delay follows a tempo change. Wow and flutter move
//! the heads with a cubic read; every gain is smoothed. The rings are sized
//! for the longest delay at the current rate, so nothing allocates after
//! construction.

use std::f32::consts::{FRAC_1_SQRT_2, FRAC_PI_2, TAU};

use biquad::{Biquad, Coefficients, DirectForm1};
use builtin_dsp_core::delay::{Allpass, DelayRing, Smoothed, smoothing_step};
use builtin_dsp_core::{
    ParamDescriptor, PluginCategory, PluginDescriptor, StereoEffect, biquad_response_db, clamp,
    db_to_linear, make_eq_coefficients,
};
use serde::{Deserialize, Serialize};

pub mod ipc;
pub mod presets;
pub mod ui;

/// Editor-facing parameter id table, re-exported at the crate root so the host
/// resolves ids the same way for every built-in (`<plugin>::ui_param_index`).
pub use ipc::{UI_PARAM_IDS, ui_param_id, ui_param_index};
pub use presets::{FactoryPreset, factory_presets};

pub const PLUGIN_ID: &str = "futureboard.echospace";

/// Longest delay either line can be set to. The rings are sized for this at the
/// current sample rate, so a time edit only moves a read head.
pub const MAX_DELAY_MS: f32 = 4_000.0;

/// Peak swing of a read head at `modDepth` 100 %.
const MAX_MOD_MS: f32 = 3.0;

/// Fastest wow `modRateHz` reaches.
pub const MAX_MOD_RATE_HZ: f32 = 8.0;
pub const DEFAULT_MOD_RATE_HZ: f32 = 0.8;

/// How long a read head takes to fade over to a new delay time.
const FADE_MS: f32 = 45.0;

/// Time constant of every smoothed control.
const SMOOTH_MS: f32 = 20.0;

/// How long Power and Mode take to crossfade.
const SWITCH_MS: f32 = 10.0;

/// Loop gain while frozen: just under unity, which holds a phrase for
/// minutes without ever building.
const FREEZE_GAIN: f32 = 0.9999;

/// The diffusion allpasses on each side's write path. Their delay is taken
/// off the line's, so the repeats land on time however much they smear.
const DIFFUSER_MS: [[f32; 2]; 2] = [[2.7, 4.9], [3.1, 5.3]];
const MAX_DIFFUSION_GAIN: f32 = 0.62;

/// The ducker's follower.
const DUCK_ATTACK_MS: f32 = 3.0;
const DUCK_RELEASE_MS: f32 = 150.0;

/// Tempo window a synced delay time is derived from. The transport publishes a
/// real tempo every block, but a region read before the engine's first publish —
/// or a project with a nonsense tempo — must not turn into an infinite or
/// zero-length delay, so the conversion clamps rather than trusts.
pub const MIN_TEMPO_BPM: f32 = 20.0;
pub const MAX_TEMPO_BPM: f32 = 999.0;

/// Tempo assumed until the host publishes a transport block.
pub const DEFAULT_TEMPO_BPM: f32 = 120.0;

/// Note divisions a synced line can lock to, shortest first. Straight, dotted
/// and triplet forms are interleaved in duration order so stepping the control
/// sweeps the musical range monotonically instead of jumping between families.
///
/// Index *is* the wire value for `divisionL` / `divisionR`, so this table is
/// part of the persisted contract — append only, and only at the end.
pub const DIVISION_LABELS: [&str; DIVISION_COUNT] = [
    "1/32T", "1/32", "1/16T", "1/32.", "1/16", "1/8T", "1/16.", "1/8", "1/4T", "1/8.", "1/4",
    "1/2T", "1/4.", "1/2", "1/1T", "1/2.", "1/1", "1/1.",
];

/// Quarter notes each entry of [`DIVISION_LABELS`] spans. A dotted note is
/// 1.5x its straight form, a triplet 2/3 of it.
pub const DIVISION_BEATS: [f32; DIVISION_COUNT] = [
    0.083_333_336,
    0.125,
    0.166_666_67,
    0.1875,
    0.25,
    0.333_333_34,
    0.375,
    0.5,
    0.666_666_7,
    0.75,
    1.0,
    1.333_333_4,
    1.5,
    2.0,
    2.666_666_7,
    3.0,
    4.0,
    6.0,
];

pub const DIVISION_COUNT: usize = 18;

/// Highest valid `divisionL` / `divisionR` wire value.
pub const MAX_DIVISION_WIRE: f32 = 17.0;

/// Default division per side, chosen so switching Sync on from the factory
/// settings lands on the same dotted-eighth / quarter pattern the free times
/// describe at 120 BPM.
pub const DEFAULT_DIVISION_L: u8 = 9;
pub const DEFAULT_DIVISION_R: u8 = 10;

/// Delay time one division spans at `tempo_bpm`, already inside the line's
/// reachable range. Allocation-free and total: an out-of-table index saturates
/// and the tempo is clamped, so this can be called from the producer thread.
#[inline]
pub fn division_ms(division: u8, tempo_bpm: f32) -> f32 {
    let beats = DIVISION_BEATS[(division as usize).min(DIVISION_COUNT - 1)];
    let bpm = if tempo_bpm.is_finite() {
        clamp(tempo_bpm, MIN_TEMPO_BPM, MAX_TEMPO_BPM)
    } else {
        DEFAULT_TEMPO_BPM
    };
    clamp(beats * 60_000.0 / bpm, 1.0, MAX_DELAY_MS)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DelayMode {
    /// Each side repeats on its own, with `crossFeedback` bleeding between.
    Stereo,
    /// The input, summed to mono, starts on the left and bounces: left after
    /// `timeMsL`, right `timeMsR` after that, and so on.
    PingPong,
    /// One line at `timeMsL`, on both sides.
    Mono,
}

impl DelayMode {
    pub const ALL: [Self; 3] = [Self::Stereo, Self::PingPong, Self::Mono];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stereo => "stereo",
            Self::PingPong => "pingpong",
            Self::Mono => "mono",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Stereo => "Stereo",
            Self::PingPong => "Ping-Pong",
            Self::Mono => "Mono",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "stereo" => Some(Self::Stereo),
            "pingpong" | "ping-pong" => Some(Self::PingPong),
            "mono" => Some(Self::Mono),
            _ => None,
        }
    }

    pub const fn to_wire(self) -> f32 {
        match self {
            Self::Stereo => 0.0,
            Self::PingPong => 1.0,
            Self::Mono => 2.0,
        }
    }

    pub fn from_wire(value: f32) -> Self {
        match value.round() as i32 {
            0 => Self::Stereo,
            2 => Self::Mono,
            _ => Self::PingPong,
        }
    }

    /// Whether the right line runs at all.
    pub fn uses_right_time(self) -> bool {
        self != Self::Mono
    }

    /// Whether `crossFeedback` does anything.
    pub fn uses_cross(self) -> bool {
        self != Self::Mono
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Params {
    pub power: bool,
    pub mode: DelayMode,
    pub time_ms_l: f32,
    pub time_ms_r: f32,
    /// Share of each repeat fed back, in percent; below 100 so the loop
    /// always decays.
    pub feedback: f32,
    /// Share of the feedback that crosses to the other side, in percent. The
    /// two paths are normalised, so this moves the repeats, never their level.
    pub cross_feedback: f32,
    pub low_cut_hz: f32,
    pub high_cut_hz: f32,
    /// Tape-style drive on the write path, in percent.
    pub saturation: f32,
    /// Dry/wet balance, in percent, on an equal-power law.
    pub mix: f32,
    /// Wet-path trim, in decibels.
    pub output_db: f32,
    pub freeze: bool,
    /// Derive both delay times from the host tempo and the divisions below,
    /// instead of from `time_ms_l` / `time_ms_r`.
    ///
    /// The free times are kept untouched while this is on, so switching back
    /// returns the line to exactly where it was rather than to a default.
    #[serde(default)]
    pub sync: bool,
    /// Note division per side while `sync` is on; an index into
    /// [`DIVISION_BEATS`].
    #[serde(default = "default_division_l")]
    pub division_l: u8,
    #[serde(default = "default_division_r")]
    pub division_r: u8,
    /// Both sides move together: an edit to either time or division is applied
    /// to the other as well.
    #[serde(default)]
    pub link: bool,
    /// Wow and flutter on the read heads, in percent of [`MAX_MOD_MS`].
    #[serde(default)]
    pub mod_depth: f32,
    #[serde(default = "default_mod_rate_hz")]
    pub mod_rate_hz: f32,
    /// How far the repeats dip while the input plays, in percent.
    #[serde(default)]
    pub duck: f32,
    /// Smear added on every pass, in percent.
    #[serde(default)]
    pub diffusion: f32,
    /// Wet-path stereo width, in percent (100 = unchanged).
    #[serde(default = "default_width")]
    pub width: f32,
}

/// Serde fallbacks for fields that projects written before EchoSpace had them
/// do not carry. Without these the whole blob would be rejected and the insert
/// would silently open at factory settings.
fn default_division_l() -> u8 {
    DEFAULT_DIVISION_L
}

fn default_division_r() -> u8 {
    DEFAULT_DIVISION_R
}

fn default_mod_rate_hz() -> f32 {
    DEFAULT_MOD_RATE_HZ
}

fn default_width() -> f32 {
    100.0
}

pub fn default_params() -> Params {
    Params {
        power: true,
        mode: DelayMode::PingPong,
        time_ms_l: 375.0,
        time_ms_r: 563.0,
        feedback: 34.0,
        cross_feedback: 65.0,
        low_cut_hz: 180.0,
        high_cut_hz: 9_000.0,
        saturation: 8.0,
        mix: 20.0,
        output_db: 0.0,
        freeze: false,
        sync: false,
        division_l: DEFAULT_DIVISION_L,
        division_r: DEFAULT_DIVISION_R,
        link: false,
        mod_depth: 0.0,
        mod_rate_hz: DEFAULT_MOD_RATE_HZ,
        duck: 0.0,
        diffusion: 0.0,
        width: 100.0,
    }
}

impl Params {
    /// Delay time the left line actually runs at: the tempo-derived division
    /// while synced, the free time otherwise.
    #[inline]
    pub fn effective_time_ms_l(&self, tempo_bpm: f32) -> f32 {
        if self.sync {
            division_ms(self.division_l, tempo_bpm)
        } else {
            self.time_ms_l
        }
    }

    #[inline]
    pub fn effective_time_ms_r(&self, tempo_bpm: f32) -> f32 {
        if self.sync {
            division_ms(self.division_r, tempo_bpm)
        } else {
            self.time_ms_r
        }
    }
}

pub fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PLUGIN_ID,
        name: "EchoSpace",
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
                default_value: 1.0,
                min: 0.0,
                max: 2.0,
                unit: "enum",
            },
            ParamDescriptor {
                id: "timeMsL",
                name: "Time L",
                default_value: 375.0,
                min: 1.0,
                max: MAX_DELAY_MS,
                unit: "ms",
            },
            ParamDescriptor {
                id: "timeMsR",
                name: "Time R",
                default_value: 563.0,
                min: 1.0,
                max: MAX_DELAY_MS,
                unit: "ms",
            },
            ParamDescriptor {
                id: "feedback",
                name: "Feedback",
                default_value: 34.0,
                min: 0.0,
                max: 98.0,
                unit: "%",
            },
            ParamDescriptor {
                id: "crossFeedback",
                name: "Cross",
                default_value: 65.0,
                min: 0.0,
                max: 100.0,
                unit: "%",
            },
            ParamDescriptor {
                id: "lowCutHz",
                name: "Low Cut",
                default_value: 180.0,
                min: 20.0,
                max: 2_000.0,
                unit: "Hz",
            },
            ParamDescriptor {
                id: "highCutHz",
                name: "High Cut",
                default_value: 9_000.0,
                min: 1_000.0,
                max: 20_000.0,
                unit: "Hz",
            },
            ParamDescriptor {
                id: "saturation",
                name: "Saturation",
                default_value: 8.0,
                min: 0.0,
                max: 100.0,
                unit: "%",
            },
            ParamDescriptor {
                id: "mix",
                name: "Mix",
                default_value: 20.0,
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
            ParamDescriptor {
                id: "sync",
                name: "Tempo Sync",
                default_value: 0.0,
                min: 0.0,
                max: 1.0,
                unit: "bool",
            },
            ParamDescriptor {
                id: "divisionL",
                name: "Division L",
                default_value: DEFAULT_DIVISION_L as f32,
                min: 0.0,
                max: MAX_DIVISION_WIRE,
                unit: "note",
            },
            ParamDescriptor {
                id: "divisionR",
                name: "Division R",
                default_value: DEFAULT_DIVISION_R as f32,
                min: 0.0,
                max: MAX_DIVISION_WIRE,
                unit: "note",
            },
            ParamDescriptor {
                id: "link",
                name: "Link L/R",
                default_value: 0.0,
                min: 0.0,
                max: 1.0,
                unit: "bool",
            },
            ParamDescriptor {
                id: "modDepth",
                name: "Mod Depth",
                default_value: 0.0,
                min: 0.0,
                max: 100.0,
                unit: "%",
            },
            ParamDescriptor {
                id: "modRateHz",
                name: "Mod Rate",
                default_value: DEFAULT_MOD_RATE_HZ,
                min: 0.05,
                max: MAX_MOD_RATE_HZ,
                unit: "Hz",
            },
            ParamDescriptor {
                id: "duck",
                name: "Duck",
                default_value: 0.0,
                min: 0.0,
                max: 100.0,
                unit: "%",
            },
            ParamDescriptor {
                id: "diffusion",
                name: "Diffusion",
                default_value: 0.0,
                min: 0.0,
                max: 100.0,
                unit: "%",
            },
            ParamDescriptor {
                id: "width",
                name: "Width",
                default_value: 100.0,
                min: 0.0,
                max: 200.0,
                unit: "%",
            },
        ],
    }
}

// ── Shared by the DSP and the editor ─────────────────────────────────────────

/// Coefficients of the write path's two cuts.
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

/// What one pass through the tone stage does at `hz`, in decibels.
pub fn tone_response_db(params: &Params, hz: f32, sample_rate: f32) -> f32 {
    cut_coefficients(params, sample_rate)
        .iter()
        .flatten()
        .map(|c| biquad_response_db(c, hz, sample_rate))
        .sum()
}

/// Loop gain per pass: `feedback`, or just under unity while frozen.
pub fn loop_gain(params: &Params) -> f32 {
    if params.freeze {
        FREEZE_GAIN
    } else {
        clamp(params.feedback / 100.0, 0.0, 0.98)
    }
}

/// One repeat, as the editor draws it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Echo {
    /// After the dry signal.
    pub at_ms: f32,
    /// Linear level against the dry signal, at 1 kHz.
    pub gain: f32,
    pub right: bool,
    /// Round trips; 1 is the first repeat.
    pub pass: u32,
}

/// The repeats a centred hit produces, down to `floor_db`: when each lands,
/// on which side and how loud — following the same routing and gains the
/// DSP runs (wow, diffusion and saturation aside). Sorted by arrival, at most
/// `max_echoes` long, so a frozen or near-unity loop still ends.
pub fn echo_pattern(
    params: &Params,
    tempo_bpm: f32,
    sample_rate: f32,
    floor_db: f32,
    max_echoes: usize,
) -> Vec<Echo> {
    let times = [
        params.effective_time_ms_l(tempo_bpm),
        params.effective_time_ms_r(tempo_bpm),
    ];
    let tone = db_to_linear(tone_response_db(params, 1_000.0, sample_rate));
    let fb = loop_gain(params);
    let cross = if params.mode.uses_cross() {
        clamp(params.cross_feedback / 100.0, 0.0, 1.0)
    } else {
        0.0
    };
    let norm = fb / (1.0 + cross);
    let floor = db_to_linear(floor_db);
    // Paths are pruned well under the floor, not at it: two that land
    // together can sum above it.
    let keep = floor * 0.01;

    // Signal waiting to come out of a line: (line, written at ms, gain).
    let mut pending: Vec<(usize, f32, f32, u32)> = match params.mode {
        DelayMode::Stereo => vec![(0, 0.0, tone, 1), (1, 0.0, tone, 1)],
        DelayMode::PingPong | DelayMode::Mono => vec![(0, 0.0, tone, 1)],
    };
    let mut echoes = Vec::new();
    while let Some(index) = pending
        .iter()
        .enumerate()
        .min_by(|a, b| (a.1.1 + times[a.1.0]).total_cmp(&(b.1.1 + times[b.1.0])))
        .map(|(i, _)| i)
    {
        if echoes.len() >= max_echoes {
            break;
        }
        let (line, written, gain, pass) = pending.swap_remove(index);
        let at = written + times[line];
        if gain >= floor {
            if params.mode == DelayMode::Mono {
                echoes.push(Echo {
                    at_ms: at,
                    gain,
                    right: false,
                    pass,
                });
                echoes.push(Echo {
                    at_ms: at,
                    gain,
                    right: true,
                    pass,
                });
            } else {
                echoes.push(Echo {
                    at_ms: at,
                    gain,
                    right: line == 1,
                    pass,
                });
            }
        }
        // Where this tap goes next, by the DSP's routing.
        let (own, other) = match params.mode {
            DelayMode::Stereo => (norm, norm * cross),
            DelayMode::PingPong => (norm * cross, norm),
            DelayMode::Mono => (fb, 0.0),
        };
        for (target, share) in [(line, own), (1 - line, other)] {
            let next = gain * share * tone;
            if next < keep || share == 0.0 {
                continue;
            }
            // Two paths landing together are one repeat, as the line hears it.
            if let Some(merged) = pending
                .iter_mut()
                .find(|p| p.0 == target && (p.1 - at).abs() < 0.05)
            {
                merged.2 += next;
            } else {
                pending.push((target, at, next, pass + 1));
            }
        }
        if pending.len() > max_echoes * 4 {
            break;
        }
    }
    echoes
}

// ── Building blocks ──────────────────────────────────────────────────────────

/// One side's line with two read heads, so a new delay time fades in
/// instead of jumping.
#[derive(Debug, Clone)]
struct Line {
    ring: DelayRing,
    /// The length being read, in samples, before the diffusers' share.
    current: f32,
    /// The length fading in, while `fade` runs from 0 to 1.
    next: f32,
    fade: Option<f32>,
}

impl Line {
    fn new(capacity_ms: f32, sample_rate: f32, delay: f32) -> Self {
        Self {
            ring: DelayRing::for_ms(capacity_ms, sample_rate),
            current: delay,
            next: delay,
            fade: None,
        }
    }

    fn clear(&mut self) {
        self.ring.clear();
    }

    /// Reads the line toward `target`, `swing` samples off it.
    #[inline]
    fn read(&mut self, target: f32, swing: f32, fade_step: f32) -> f32 {
        if self.fade.is_none() && (target - self.current).abs() > 0.5 {
            self.next = target;
            self.fade = Some(0.0);
        }
        let Some(progress) = self.fade else {
            return self.ring.read_cubic(self.current + swing);
        };
        let progress = progress + fade_step;
        if progress >= 1.0 {
            self.current = self.next;
            self.fade = None;
            return self.ring.read_cubic(self.current + swing);
        }
        self.fade = Some(progress);
        // Raised-cosine weights summing to one: a time nudge (the two reads
        // nearly alike) neither dips nor bumps.
        let w = 0.5 - 0.5 * (progress * std::f32::consts::PI).cos();
        let old = self.ring.read_cubic(self.current + swing);
        let new = self.ring.read_cubic(self.next + swing);
        old + (new - old) * w
    }
}

/// One side's tone stage: cuts, diffusers.
#[derive(Debug, Clone)]
struct Tone {
    low_cut: Option<DirectForm1<f32>>,
    high_cut: Option<DirectForm1<f32>>,
    diffusers: [Allpass; 2],
}

impl Tone {
    fn clear(&mut self) {
        for filter in [self.low_cut.as_mut(), self.high_cut.as_mut()]
            .into_iter()
            .flatten()
        {
            filter.reset_state();
        }
        for ap in self.diffusers.iter_mut() {
            ap.clear();
        }
    }

    #[inline]
    fn filter(&mut self, x: f32) -> f32 {
        let mut y = x;
        if let Some(f) = self.low_cut.as_mut() {
            y = f.run(y);
        }
        if let Some(f) = self.high_cut.as_mut() {
            y = f.run(y);
        }
        y
    }
}

/// A pair of quadrature oscillators: slow wow and a faster flutter.
#[derive(Debug, Clone, Copy)]
struct Wobble {
    wow: (f32, f32),
    flutter: (f32, f32),
}

impl Wobble {
    fn at_phase(phase: f32) -> Self {
        Self {
            wow: (phase.sin(), phase.cos()),
            flutter: ((phase * 2.3).sin(), (phase * 2.3).cos()),
        }
    }

    #[inline]
    fn rotate(osc: &mut (f32, f32), step: (f32, f32)) {
        let (s, c) = *osc;
        *osc = (s * step.0 + c * step.1, c * step.0 - s * step.1);
    }

    /// The next swing, −1 to 1.
    #[inline]
    fn next(&mut self, wow_step: (f32, f32), flutter_step: (f32, f32)) -> f32 {
        Self::rotate(&mut self.wow, wow_step);
        Self::rotate(&mut self.flutter, flutter_step);
        0.85 * self.wow.0 + 0.15 * self.flutter.0
    }

    fn renormalise(&mut self) {
        for osc in [&mut self.wow, &mut self.flutter] {
            let norm = 1.5 - 0.5 * (osc.0 * osc.0 + osc.1 * osc.1);
            osc.0 *= norm;
            osc.1 *= norm;
        }
    }
}

/// A switch's 0–1 crossfade. It walks linearly toward its end and is heard
/// through a smoothstep, so a fade both leaves and lands without a kink.
#[derive(Debug, Clone, Copy)]
struct Fade {
    position: f32,
    target: f32,
    step: f32,
}

impl Fade {
    fn new(on: bool, sample_rate: f32) -> Self {
        let at = if on { 1.0 } else { 0.0 };
        Self {
            position: at,
            target: at,
            step: 1.0 / (SWITCH_MS * 0.001 * sample_rate).max(1.0),
        }
    }

    fn set(&mut self, on: bool) {
        self.target = if on { 1.0 } else { 0.0 };
    }

    /// Starts a fresh fade from 0 to 1.
    fn restart(&mut self) {
        self.position = 0.0;
        self.target = 1.0;
    }

    fn settle(&mut self) {
        self.position = self.target;
    }

    fn is_settled(&self) -> bool {
        self.position == self.target
    }

    /// Off, and staying off.
    fn is_off(&self) -> bool {
        self.position == 0.0 && self.target == 0.0
    }

    /// One sample's step; the weight of the "on" side.
    #[inline]
    fn next(&mut self) -> f32 {
        if self.position < self.target {
            self.position = (self.position + self.step).min(self.target);
        } else if self.position > self.target {
            self.position = (self.position - self.step).max(self.target);
        }
        let x = self.position;
        x * x * (3.0 - 2.0 * x)
    }
}

/// What each line is written with and what the wet pair reads, for `mode`.
#[allow(clippy::too_many_arguments)]
#[inline]
fn route(
    mode: DelayMode,
    left: f32,
    right: f32,
    input_gain: f32,
    taps: [f32; 2],
    fb: f32,
    cross: f32,
    norm: f32,
) -> ([f32; 2], [f32; 2]) {
    let mono_in = (left + right) * 0.5;
    match mode {
        DelayMode::Stereo => (
            [
                left * input_gain + (taps[0] + cross * taps[1]) * norm,
                right * input_gain + (taps[1] + cross * taps[0]) * norm,
            ],
            taps,
        ),
        DelayMode::PingPong => (
            [
                mono_in * input_gain + (taps[1] + cross * taps[0]) * norm,
                (taps[0] + cross * taps[1]) * norm,
            ],
            taps,
        ),
        DelayMode::Mono => (
            [mono_in * input_gain + taps[0] * fb, 0.0],
            [taps[0], taps[0]],
        ),
    }
}

/// Tape-style drive with unit slope at rest: quiet repeats pass untouched,
/// loud ones round off. Its slope never exceeds one, so it can only take
/// gain out of the loop.
#[inline]
fn saturate(x: f32, amount: f32) -> f32 {
    if amount <= 1.0e-4 {
        return x;
    }
    let drive = 1.0 + 3.0 * amount;
    let shaped = (x * drive).tanh() / drive;
    x + (shaped - x) * (amount * 4.0).min(1.0)
}

#[derive(Debug, Clone)]
pub struct Dsp {
    sample_rate: f32,
    params: Params,
    /// Latest transport tempo, republished by the host each block. Only the
    /// synced delay times read it; the free times ignore it entirely.
    tempo_bpm: f32,
    lines: [Line; 2],
    tones: [Tone; 2],
    wobbles: [Wobble; 2],
    /// Line lengths the heads head for, in samples, net of the diffusers.
    targets: [f32; 2],
    /// The diffusers' delay per side, taken off the line's.
    diffuser_samples: [f32; 2],
    fade_step: f32,
    smooth_step: f32,
    wow_step: (f32, f32),
    flutter_step: (f32, f32),
    duck_attack: f32,
    duck_release: f32,
    duck_env: f32,
    feedback: Smoothed,
    cross: Smoothed,
    input_gain: Smoothed,
    /// 1 while frozen: the tone stage is bypassed so the loop holds.
    freeze_mix: Smoothed,
    diffusion: Smoothed,
    drive: Smoothed,
    swing: Smoothed,
    duck_depth: Smoothed,
    mid_gain: Smoothed,
    side_gain: Smoothed,
    dry_gain: Smoothed,
    wet_gain: Smoothed,
    output_gain: Smoothed,
    wobble_tick: u32,
    /// Dry (0) to the running delay (1). Switching Power fades rather than
    /// cuts, so switching off fades the repeats out with it; once off and
    /// settled nothing runs.
    power: Fade,
    /// A Mode change fades the lines' writes and the wet pair from
    /// `mode_from`'s routing to `mode_to`'s.
    mode_from: DelayMode,
    mode_to: DelayMode,
    mode_fade: Fade,
    /// Nothing has played since construction or the last reset, so a retune
    /// lands at once rather than gliding.
    fresh: bool,
}

impl Dsp {
    pub fn new(sample_rate: f32) -> Self {
        let sr = sample_rate.max(1.0);
        let capacity = MAX_DELAY_MS + MAX_MOD_MS * 2.0 + 2.0;
        let diffusers =
            |side: usize| std::array::from_fn(|i| Allpass::for_ms(DIFFUSER_MS[side][i], sr));
        let tone = |side: usize| Tone {
            low_cut: None,
            high_cut: None,
            diffusers: diffusers(side),
        };
        let tones = [tone(0), tone(1)];
        let diffuser_samples = std::array::from_fn(|side| {
            tones[side].diffusers.iter().map(|ap| ap.len() as f32).sum()
        });
        let mut dsp = Self {
            sample_rate: sr,
            params: default_params(),
            tempo_bpm: DEFAULT_TEMPO_BPM,
            lines: [Line::new(capacity, sr, 2.0), Line::new(capacity, sr, 2.0)],
            tones,
            wobbles: [Wobble::at_phase(0.0), Wobble::at_phase(FRAC_PI_2)],
            targets: [2.0; 2],
            diffuser_samples,
            fade_step: 1.0 / (FADE_MS * 0.001 * sr),
            smooth_step: smoothing_step(SMOOTH_MS, sr),
            wow_step: (1.0, 0.0),
            flutter_step: (1.0, 0.0),
            duck_attack: smoothing_step(DUCK_ATTACK_MS, sr),
            duck_release: smoothing_step(DUCK_RELEASE_MS, sr),
            duck_env: 0.0,
            feedback: Smoothed::at(0.0),
            cross: Smoothed::at(0.0),
            input_gain: Smoothed::at(1.0),
            freeze_mix: Smoothed::at(0.0),
            diffusion: Smoothed::at(0.0),
            drive: Smoothed::at(0.0),
            swing: Smoothed::at(0.0),
            duck_depth: Smoothed::at(0.0),
            mid_gain: Smoothed::at(FRAC_1_SQRT_2),
            side_gain: Smoothed::at(FRAC_1_SQRT_2),
            dry_gain: Smoothed::at(1.0),
            wet_gain: Smoothed::at(0.0),
            output_gain: Smoothed::at(1.0),
            wobble_tick: 0,
            power: Fade::new(true, sr),
            mode_from: DelayMode::PingPong,
            mode_to: DelayMode::PingPong,
            mode_fade: Fade::new(true, sr),
            fresh: true,
        };
        dsp.rebuild_filters();
        dsp.retune();
        dsp
    }

    pub fn params(&self) -> &Params {
        &self.params
    }

    pub fn set_params(&mut self, params: Params) {
        let rebuild = params.low_cut_hz != self.params.low_cut_hz
            || params.high_cut_hz != self.params.high_cut_hz;
        self.params = params;
        ipc::sanitize_params(&mut self.params);
        if rebuild {
            self.rebuild_filters();
        }
        self.retune();
        // A loaded state starts as it was saved: no switch fades in.
        self.power.settle();
        self.mode_fade.settle();
    }

    /// Publish the block's transport tempo. Called from the host producer
    /// before `process_stereo`, so it must stay allocation-free: it is a
    /// compare plus, at most, the same retune a time edit does, and only
    /// while a synced line is actually reading it.
    pub fn set_tempo_bpm(&mut self, tempo_bpm: f32) {
        if !tempo_bpm.is_finite() || (tempo_bpm - self.tempo_bpm).abs() < 1.0e-4 {
            return;
        }
        self.tempo_bpm = tempo_bpm;
        if self.params.sync {
            self.retune();
        }
    }

    pub fn tempo_bpm(&self) -> f32 {
        self.tempo_bpm
    }

    /// Apply a compact wire update already resolved by the UI/control thread.
    ///
    /// The audio path never parses JSON or looks up string parameter ids. Every
    /// arm below only retargets smoothed values or retunes the two cuts, so
    /// this stays allocation-free and safe to call from the producer thread
    /// between blocks.
    pub fn apply_wire_param(&mut self, wire_index: u32, value: f32) -> bool {
        if !ipc::apply_wire_param(&mut self.params, wire_index, value) {
            return false;
        }
        if matches!(wire_index, ipc::LOW_CUT_INDEX | ipc::HIGH_CUT_INDEX) {
            self.rebuild_filters();
        }
        self.retune();
        true
    }

    /// Resolve a string id off the realtime path (project restore, tests).
    pub fn apply_ui_param(&mut self, id: &str, value: f32) -> bool {
        match ipc::ui_param_index(id) {
            Some(index) => self.apply_wire_param(index, value),
            None => false,
        }
    }

    /// EchoSpace introduces no lookahead: the delay time is a musical
    /// parameter, not a processing latency the graph should compensate for.
    pub fn latency_samples(&self) -> usize {
        0
    }

    /// The length each line reads at — what the heads are heading for —
    /// in samples, net of the diffusers.
    fn line_targets(&self) -> [f32; 2] {
        let times = [
            self.params.effective_time_ms_l(self.tempo_bpm),
            self.params.effective_time_ms_r(self.tempo_bpm),
        ];
        let max =
            self.lines[0].ring.max_cubic_delay() - MAX_MOD_MS * 0.001 * self.sample_rate - 2.0;
        std::array::from_fn(|side| {
            clamp(
                times[side] * 0.001 * self.sample_rate - self.diffuser_samples[side],
                2.0,
                max,
            )
        })
    }

    /// Hands every smoothed value its new target from `params`.
    fn retune(&mut self) {
        let p = &self.params;
        let sr = self.sample_rate;
        self.targets = self.line_targets();
        self.feedback.target = loop_gain(p);
        self.cross.target = if p.mode.uses_cross() {
            clamp(p.cross_feedback / 100.0, 0.0, 1.0)
        } else {
            0.0
        };
        self.input_gain.target = if p.freeze { 0.0 } else { 1.0 };
        self.freeze_mix.target = if p.freeze { 1.0 } else { 0.0 };
        self.diffusion.target = clamp(p.diffusion / 100.0, 0.0, 1.0) * MAX_DIFFUSION_GAIN;
        self.drive.target = clamp(p.saturation / 100.0, 0.0, 1.0);
        self.swing.target = clamp(p.mod_depth / 100.0, 0.0, 1.0) * MAX_MOD_MS * 0.001 * sr;
        let duck = clamp(p.duck / 100.0, 0.0, 1.0);
        self.duck_depth.target = 16.0 * duck * duck;
        let width = clamp(p.width / 100.0, 0.0, 2.0);
        self.mid_gain.target = (2.0 - width).sqrt() * FRAC_1_SQRT_2;
        self.side_gain.target = width.sqrt() * FRAC_1_SQRT_2;
        let mix = clamp(p.mix / 100.0, 0.0, 1.0);
        self.dry_gain.target = (mix * FRAC_PI_2).cos();
        self.wet_gain.target = (mix * FRAC_PI_2).sin();
        self.output_gain.target = db_to_linear(p.output_db);
        let rate = clamp(p.mod_rate_hz, 0.0, MAX_MOD_RATE_HZ);
        let step = |hz: f32| {
            let w = TAU * hz / sr;
            (w.cos(), w.sin())
        };
        self.wow_step = step(rate);
        self.flutter_step = step(rate * 5.3);
        // Waking from a settled off: the lines hold whatever was playing when
        // it went off, which must not come back as repeats.
        let waking = self.params.power && self.power.is_off();
        self.power.set(self.params.power);
        if self.params.mode != self.mode_to {
            self.mode_from = self.mode_to;
            self.mode_to = self.params.mode;
            self.mode_fade.restart();
        }
        if self.fresh {
            self.snap();
            self.power.settle();
        } else if waking {
            self.clear_lines();
            self.snap();
        }
    }

    /// Lands every gliding value on its target, and both heads on their
    /// lengths.
    fn snap(&mut self) {
        for smoothed in [
            &mut self.feedback,
            &mut self.cross,
            &mut self.input_gain,
            &mut self.freeze_mix,
            &mut self.diffusion,
            &mut self.drive,
            &mut self.swing,
            &mut self.duck_depth,
            &mut self.mid_gain,
            &mut self.side_gain,
            &mut self.dry_gain,
            &mut self.wet_gain,
            &mut self.output_gain,
        ] {
            smoothed.settle();
        }
        for (line, target) in self.lines.iter_mut().zip(self.targets) {
            line.current = target;
            line.next = target;
            line.fade = None;
        }
        self.mode_from = self.mode_to;
        self.mode_fade.settle();
    }

    /// Empties both lines and the tone stage. A memset of the rings:
    /// allocation-free.
    fn clear_lines(&mut self) {
        for line in self.lines.iter_mut() {
            line.clear();
        }
        for tone in self.tones.iter_mut() {
            tone.clear();
        }
        self.duck_env = 0.0;
    }

    fn rebuild_filters(&mut self) {
        let [low, high] = cut_coefficients(&self.params, self.sample_rate);
        for tone in self.tones.iter_mut() {
            for (filter, coefficients) in [(&mut tone.low_cut, low), (&mut tone.high_cut, high)] {
                match (filter.as_mut(), coefficients) {
                    // Retuned in place: a sweep keeps the repeats running.
                    (Some(f), Some(c)) => f.update_coefficients(c),
                    (_, c) => *filter = c.map(DirectForm1::<f32>::new),
                }
            }
        }
    }
}

impl StereoEffect for Dsp {
    fn reset(&mut self) {
        self.clear_lines();
        self.fresh = true;
        self.snap();
        self.power.settle();
    }

    fn set_sample_rate(&mut self, sample_rate: f32) {
        let sr = sample_rate.max(1.0);
        if (sr - self.sample_rate).abs() < f32::EPSILON {
            return;
        }
        let (params, tempo) = (self.params.clone(), self.tempo_bpm);
        *self = Self::new(sr);
        self.tempo_bpm = tempo;
        self.set_params(params);
        self.reset();
    }

    fn process_stereo(&mut self, left: f32, right: f32) -> (f32, f32) {
        if self.power.is_off() {
            return (left, right);
        }
        let on = self.power.next();
        // What enters the lines fades with the switch too: a line woken empty
        // would otherwise record the input starting mid-wave, and play that
        // step back a delay later.
        let (l, r) = self.run(left, right, on);
        if on >= 1.0 {
            (l, r)
        } else {
            (left + (l - left) * on, right + (r - right) * on)
        }
    }
}

impl Dsp {
    /// One frame through the delay, power aside; `feed` scales what is
    /// written into the lines.
    #[inline]
    fn run(&mut self, left: f32, right: f32, feed: f32) -> (f32, f32) {
        self.fresh = false;
        let smooth = self.smooth_step;

        // Read heads, wobbling.
        let swing = self.swing.next(smooth);
        let mut taps = [0.0f32; 2];
        for side in 0..2 {
            let wobble = self.wobbles[side].next(self.wow_step, self.flutter_step) * swing;
            taps[side] = self.lines[side].read(self.targets[side], wobble, self.fade_step);
        }
        self.wobble_tick += 1;
        if self.wobble_tick >= 64 {
            self.wobble_tick = 0;
            for wobble in self.wobbles.iter_mut() {
                wobble.renormalise();
            }
        }

        // Feedback routing, normalised so cross-feed moves the repeats
        // without changing the loop's level.
        let fb = self.feedback.next(smooth);
        let cross = self.cross.next(smooth);
        let norm = fb / (1.0 + cross);
        let input_gain = self.input_gain.next(smooth) * feed;
        let (mut writes, mut wet) =
            route(self.mode_to, left, right, input_gain, taps, fb, cross, norm);
        if !self.mode_fade.is_settled() {
            // A mode change: both routings run and the new one fades in, on
            // what the lines are written with as well as on what they play.
            let w = self.mode_fade.next();
            let (old_writes, old_wet) = route(
                self.mode_from,
                left,
                right,
                input_gain,
                taps,
                fb,
                cross,
                norm,
            );
            for side in 0..2 {
                writes[side] = old_writes[side] + (writes[side] - old_writes[side]) * w;
                wet[side] = old_wet[side] + (wet[side] - old_wet[side]) * w;
            }
        }

        // Tone stage on the write path, bypassed while frozen.
        let freeze_mix = self.freeze_mix.next(smooth);
        let diffusion = self.diffusion.next(smooth);
        let drive = self.drive.next(smooth);
        for side in 0..2 {
            let tone = &mut self.tones[side];
            let raw = writes[side];
            let mut x = tone.filter(raw);
            x += (raw - x) * freeze_mix;
            for ap in tone.diffusers.iter_mut() {
                x = ap.process(x, diffusion);
            }
            self.lines[side].ring.push(saturate(x, drive));
        }

        // Duck: the repeats step back while the input plays.
        let level = left.abs().max(right.abs());
        let rate = if level > self.duck_env {
            self.duck_attack
        } else {
            self.duck_release
        };
        self.duck_env += (level - self.duck_env) * rate;
        let duck = 1.0 / (1.0 + self.duck_depth.next(smooth) * self.duck_env);

        let mid = (wet[0] + wet[1]) * FRAC_1_SQRT_2 * self.mid_gain.next(smooth);
        let side = (wet[0] - wet[1]) * FRAC_1_SQRT_2 * self.side_gain.next(smooth);
        let wet_gain = self.wet_gain.next(smooth) * self.output_gain.next(smooth) * duck;
        let dry = self.dry_gain.next(smooth);
        (
            left * dry + (mid + side) * wet_gain,
            right * dry + (mid - side) * wet_gain,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    /// Wet only, tone stage wide open and clean: what the routing alone does.
    fn plain(mode: DelayMode) -> Params {
        Params {
            mode,
            mix: 100.0,
            low_cut_hz: 20.0,
            high_cut_hz: 20_000.0,
            saturation: 0.0,
            cross_feedback: 0.0,
            ..default_params()
        }
    }

    fn dsp_with(params: Params) -> Dsp {
        let mut dsp = Dsp::new(SR);
        dsp.set_params(params);
        dsp.reset();
        dsp
    }

    fn impulse_response(dsp: &mut Dsp, frames: usize) -> (Vec<f32>, Vec<f32>) {
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

    /// Length of the 1 kHz test burst, and where its envelope peaks.
    const BURST: usize = 480;
    const BURST_PEAK_MS: f32 = 5.0;

    /// The response to a Hann-windowed 1 kHz burst: each repeat comes back
    /// as a burst whose height is the repeat's level at 1 kHz — the level
    /// [`echo_pattern`] draws — whatever the cuts do to an impulse's shape.
    fn burst_response(dsp: &mut Dsp, frames: usize) -> (Vec<f32>, Vec<f32>) {
        let mut left = Vec::with_capacity(frames);
        let mut right = Vec::with_capacity(frames);
        for n in 0..frames {
            let x = if n < BURST {
                let window = 0.5 - 0.5 * (TAU * n as f32 / BURST as f32).cos();
                window * (TAU * 1_000.0 * n as f32 / SR).sin()
            } else {
                0.0
            };
            let (l, r) = dsp.process_stereo(x, x);
            left.push(l);
            right.push(r);
        }
        (left, right)
    }

    /// The highest point of a burst arriving `at_ms` after the input's.
    fn burst_level(signal: &[f32], at_ms: f32) -> f32 {
        let centre = ((at_ms + BURST_PEAK_MS) * 0.001 * SR) as usize;
        (centre.saturating_sub(120)..(centre + 120).min(signal.len()))
            .map(|i| signal[i].abs())
            .fold(0.0, f32::max)
    }

    /// Where the impulse response peaks near `at_ms`, and how high.
    fn peak_near(signal: &[f32], at_ms: f32) -> (f32, f32) {
        let centre = (at_ms * 0.001 * SR) as usize;
        let (index, value) = (centre.saturating_sub(48)..(centre + 48).min(signal.len()))
            .map(|i| (i, signal[i].abs()))
            .fold(
                (0, 0.0f32),
                |best, now| if now.1 > best.1 { now } else { best },
            );
        (index as f32 / SR * 1000.0, value)
    }

    #[test]
    fn descriptor_ids_are_unique_and_match_defaults() {
        let d = descriptor();
        assert_eq!(d.id, PLUGIN_ID);
        assert_eq!(d.category, PluginCategory::Effect);
        assert_eq!(d.params.len(), ipc::PARAM_COUNT);

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
        for n in 0..9_600 {
            let x = (n as f32 * 0.05).sin() * 0.5;
            let (l, r) = dsp.process_stereo(x, -x);
            assert!((l - x).abs() < 1.0e-6 && (r + x).abs() < 1.0e-6);
        }
    }

    /// Every repeat lands where the editor draws it, at the level it draws.
    #[test]
    fn the_repeats_land_where_the_pattern_says() {
        for mode in DelayMode::ALL {
            let mut params = plain(mode);
            params.time_ms_l = 120.0;
            params.time_ms_r = 190.0;
            params.feedback = 60.0;
            params.cross_feedback = 40.0;
            let mut dsp = dsp_with(params.clone());
            let (left, right) = impulse_response(&mut dsp, (SR * 1.2) as usize);
            dsp.reset();
            let (burst_l, burst_r) = burst_response(&mut dsp, (SR * 1.2) as usize);
            let pattern = echo_pattern(&params, DEFAULT_TEMPO_BPM, SR, -30.0, 64);
            assert!(pattern.len() >= 4, "{mode:?}: too few repeats drawn");
            for echo in pattern.iter().filter(|e| e.at_ms < 1_150.0) {
                let side = if echo.right { &right } else { &left };
                let (at, _) = peak_near(side, echo.at_ms);
                let level = burst_level(if echo.right { &burst_r } else { &burst_l }, echo.at_ms);
                assert!(
                    (at - echo.at_ms).abs() < 0.2,
                    "{mode:?}: repeat {} drawn at {} ms, heard at {at} ms",
                    echo.pass,
                    echo.at_ms
                );
                assert!(
                    (level - echo.gain).abs() < 0.02 + echo.gain * 0.06,
                    "{mode:?}: repeat {} at {} ms drawn {} heard {level}",
                    echo.pass,
                    echo.at_ms,
                    echo.gain
                );
            }
        }
    }

    #[test]
    fn ping_pong_starts_left_and_bounces() {
        let mut params = plain(DelayMode::PingPong);
        params.time_ms_l = 100.0;
        params.time_ms_r = 100.0;
        params.feedback = 50.0;
        let mut dsp = dsp_with(params);
        let (left, right) = burst_response(&mut dsp, (SR * 0.35) as usize);
        assert!(burst_level(&left, 100.0) > 0.95 && burst_level(&right, 100.0) < 0.01);
        assert!(burst_level(&right, 200.0) > 0.47 && burst_level(&left, 200.0) < 0.01);
        assert!(burst_level(&left, 300.0) > 0.23);
    }

    /// The echo must land at the requested time, not at whatever the ring
    /// happens to be long enough for, at every rate.
    #[test]
    fn the_longest_delay_is_reachable_at_every_rate() {
        for &sr in &[44_100.0f32, 48_000.0, 96_000.0, 192_000.0] {
            let mut dsp = Dsp::new(sr);
            let mut params = plain(DelayMode::Stereo);
            params.time_ms_l = MAX_DELAY_MS;
            params.time_ms_r = MAX_DELAY_MS;
            params.feedback = 0.0;
            dsp.set_params(params);
            dsp.reset();

            let expected = (MAX_DELAY_MS * 0.001 * sr) as usize;
            let _ = dsp.process_stereo(1.0, 1.0);
            let mut early = 0.0f32;
            for _ in 0..(expected - 16) {
                let (l, _) = dsp.process_stereo(0.0, 0.0);
                early = early.max(l.abs());
            }
            assert!(early < 1.0e-4, "echo arrived early at {sr} Hz: {early}");
            let mut heard = 0.0f32;
            for _ in 0..32 {
                let (l, _) = dsp.process_stereo(0.0, 0.0);
                heard += l;
            }
            assert!(heard > 0.8, "no echo at {sr} Hz after {expected} samples");
        }
    }

    #[test]
    fn maximum_feedback_stays_bounded_in_every_mode() {
        for mode in DelayMode::ALL {
            let mut params = plain(mode);
            params.time_ms_l = 120.0;
            params.time_ms_r = 180.0;
            params.feedback = 98.0;
            params.cross_feedback = 100.0;
            params.diffusion = 100.0;
            params.mod_depth = 100.0;
            let mut dsp = dsp_with(params);
            let mut peak = 0.0f32;
            for n in 0..(48_000 * 12) {
                let x = if n < 4_800 {
                    (n as f32 * 0.02).sin() * 0.7
                } else {
                    0.0
                };
                let (l, r) = dsp.process_stereo(x, x);
                assert!(l.is_finite() && r.is_finite(), "{mode:?} diverged at {n}");
                peak = peak.max(l.abs()).max(r.abs());
            }
            assert!(peak < 2.0, "{mode:?} ran away: peak {peak}");
        }
    }

    /// Cross-feed changes where the repeats go, not how loud the loop is.
    #[test]
    fn cross_feedback_does_not_change_the_loop_gain() {
        let tail_energy = |cross: f32| {
            let mut params = plain(DelayMode::Stereo);
            params.time_ms_l = 100.0;
            params.time_ms_r = 100.0;
            params.feedback = 80.0;
            params.cross_feedback = cross;
            let mut dsp = dsp_with(params);
            let (left, right) = impulse_response(&mut dsp, 48_000 * 2);
            left.iter()
                .chain(&right)
                .skip(9_600)
                .map(|x| x * x)
                .sum::<f32>()
        };
        let none = tail_energy(0.0);
        let full = tail_energy(100.0);
        assert!(none > 0.01, "no tail to compare");
        assert!((none - full).abs() < none * 0.05, "{none} vs {full}");
    }

    /// The old saturator gained up to 7x at rest, so a quiet repeat came back
    /// louder than it went in; this one only ever takes level off.
    #[test]
    fn saturation_never_adds_gain() {
        for amount in [0.0f32, 0.1, 0.5, 1.0] {
            for x in [-1.5f32, -0.6, -0.01, 0.0, 0.003, 0.2, 0.9, 2.0] {
                assert!(saturate(x, amount).abs() <= x.abs() + 1.0e-6);
            }
            assert!((saturate(0.001, amount) - 0.001).abs() < 1.0e-5);
        }
        assert!(saturate(1.0, 1.0) < 0.5);
    }

    /// Turning the time knob crossfades between two heads: no step, and no
    /// pitch smear.
    #[test]
    fn a_time_change_does_not_click() {
        let mut params = plain(DelayMode::Stereo);
        params.time_ms_l = 300.0;
        params.time_ms_r = 300.0;
        params.feedback = 50.0;
        let mut dsp = dsp_with(params);
        let mut previous = 0.0f32;
        let mut steady = 0.0f32;
        let mut changed = 0.0f32;
        for n in 0..(48_000 * 2) {
            let x = (n as f32 * TAU * 330.0 / SR).sin() * 0.4;
            if n == 48_000 {
                assert!(dsp.apply_ui_param("timeMsL", 1_100.0));
            }
            if n == 52_000 {
                assert!(dsp.apply_ui_param("timeMsL", 450.0));
            }
            let (l, _) = dsp.process_stereo(x, x);
            let step = (l - previous).abs();
            previous = l;
            if (24_000..48_000).contains(&n) {
                steady = steady.max(step);
            } else if n >= 48_000 {
                changed = changed.max(step);
            }
        }
        assert!(changed < steady * 1.5, "{changed} against {steady}");
    }

    #[test]
    fn freeze_holds_the_repeats_after_the_input_stops() {
        let mut params = default_params();
        params.mix = 100.0;
        params.time_ms_l = 250.0;
        params.time_ms_r = 250.0;
        params.feedback = 40.0;
        let mut dsp = dsp_with(params);
        for n in 0..24_000 {
            let x = (n as f32 * 0.03).sin() * 0.5;
            let _ = dsp.process_stereo(x, x);
        }
        assert!(dsp.apply_ui_param("freeze", 1.0));

        let mut early = 0.0f32;
        for _ in 0..12_000 {
            let (l, r) = dsp.process_stereo(0.0, 0.0);
            early = early.max(l.abs()).max(r.abs());
        }
        let mut late = 0.0f32;
        for _ in 0..(48_000 * 4) {
            let (l, r) = dsp.process_stereo(0.0, 0.0);
            late = late.max(l.abs()).max(r.abs());
        }
        assert!(early > 1.0e-4, "nothing in the line to freeze");
        assert!(
            late > early * 0.5,
            "freeze decayed away: early {early}, late {late}"
        );
    }

    /// While the input plays the repeats step back; when it stops they return.
    #[test]
    fn ducking_clears_room_for_the_input() {
        let level_of = |duck: f32| {
            let mut params = plain(DelayMode::Stereo);
            params.time_ms_l = 50.0;
            params.time_ms_r = 50.0;
            params.feedback = 70.0;
            params.duck = duck;
            let mut dsp = dsp_with(params);
            let mut playing = 0.0f32;
            let mut after = 0.0f32;
            for n in 0..(48_000 * 2) {
                let x = if n < 48_000 {
                    (n as f32 * TAU * 200.0 / SR).sin() * 0.5
                } else {
                    0.0
                };
                let (l, _) = dsp.process_stereo(x, x);
                // Wet only: take the dry back out.
                if (24_000..48_000).contains(&n) {
                    playing = playing.max(l.abs());
                } else if (76_000..78_000).contains(&n) {
                    after = after.max(l.abs());
                }
            }
            (playing, after)
        };
        let (open_playing, open_after) = level_of(0.0);
        let (ducked_playing, ducked_after) = level_of(100.0);
        assert!(
            ducked_playing < open_playing * 0.3,
            "{ducked_playing} vs {open_playing}"
        );
        assert!(
            ducked_after > open_after * 0.7,
            "{ducked_after} vs {open_after}"
        );
    }

    #[test]
    fn reset_clears_the_repeats() {
        let mut dsp = dsp_with(Params {
            mix: 100.0,
            ..default_params()
        });
        for _ in 0..4_800 {
            let _ = dsp.process_stereo(0.5, -0.5);
        }
        dsp.reset();
        let (l, r) = dsp.process_stereo(0.0, 0.0);
        assert!(l.abs() < 1.0e-6 && r.abs() < 1.0e-6);
    }

    #[test]
    fn sample_rate_change_keeps_taps_inside_the_new_rings() {
        let mut params = default_params();
        params.time_ms_l = MAX_DELAY_MS;
        params.time_ms_r = MAX_DELAY_MS;
        params.mix = 100.0;
        params.mod_depth = 100.0;
        let mut dsp = Dsp::new(192_000.0);
        dsp.set_params(params);
        dsp.set_sample_rate(44_100.0);
        assert_eq!(dsp.params().time_ms_l, MAX_DELAY_MS, "the params survive");
        let capacity = dsp.lines[0].ring.max_cubic_delay();
        assert!(dsp.targets[0] + MAX_MOD_MS * 0.001 * 44_100.0 < capacity);
        for _ in 0..4_410 {
            let (l, r) = dsp.process_stereo(0.3, -0.3);
            assert!(l.is_finite() && r.is_finite());
        }
    }

    /// A synced line's spacing is a note length: it has to come out of the
    /// transport tempo, not out of the free `time_ms_*` the control still
    /// holds.
    #[test]
    fn a_synced_line_takes_its_delay_from_the_tempo() {
        let mut params = plain(DelayMode::Stereo);
        params.time_ms_l = 12.0;
        params.time_ms_r = 12.0;
        params.feedback = 0.0;
        params.sync = true;
        params.division_l = DIVISION_LABELS.iter().position(|l| *l == "1/4").unwrap() as u8;
        params.division_r = params.division_l;
        let mut dsp = dsp_with(params);
        dsp.set_tempo_bpm(120.0);
        dsp.reset();
        // A quarter note at 120 BPM is 500 ms.
        let (left, _) = impulse_response(&mut dsp, 30_000);
        assert!(peak_near(&left, 500.0).1 > 0.5);
        assert!((peak_near(&left, 500.0).0 - 500.0).abs() < 0.1);

        // Half the tempo, twice the spacing — without any parameter edit.
        dsp.set_tempo_bpm(60.0);
        dsp.reset();
        let (left, _) = impulse_response(&mut dsp, 50_000);
        assert!((peak_near(&left, 1_000.0).0 - 1_000.0).abs() < 0.1);

        // ...and switching sync off returns the line to the time the control
        // was holding all along.
        assert!(dsp.apply_ui_param("sync", 0.0));
        dsp.reset();
        let (left, _) = impulse_response(&mut dsp, 2_000);
        assert!((peak_near(&left, 12.0).0 - 12.0).abs() < 0.1);
    }

    #[test]
    fn a_free_line_ignores_the_tempo() {
        let mut params = default_params();
        params.time_ms_l = 250.0;
        params.sync = false;
        let mut dsp = dsp_with(params);
        let before = dsp.targets;
        dsp.set_tempo_bpm(174.0);
        assert_eq!(dsp.targets, before);
        assert_eq!(dsp.tempo_bpm(), 174.0);
    }

    /// A nonsense tempo — a region read before the engine's first publish, or a
    /// project with a zero in it — must not turn into a zero-length or infinite
    /// delay.
    #[test]
    fn a_nonsense_tempo_cannot_break_the_delay_line() {
        let mut params = default_params();
        params.sync = true;
        let mut dsp = dsp_with(params);
        for bpm in [0.0, -240.0, f32::NAN, f32::INFINITY, 1.0e12] {
            dsp.set_tempo_bpm(bpm);
            assert!(dsp.targets[0] >= 2.0);
            assert!(dsp.targets[0] < dsp.lines[0].ring.max_cubic_delay());
            let (l, r) = dsp.process_stereo(0.4, -0.4);
            assert!(l.is_finite() && r.is_finite(), "diverged at {bpm} BPM");
        }
    }

    #[test]
    fn every_division_stays_inside_the_line_at_any_tempo() {
        let mut dsp = Dsp::new(48_000.0);
        for bpm in [MIN_TEMPO_BPM, 60.0, 120.0, 174.0, MAX_TEMPO_BPM] {
            for division in 0..DIVISION_COUNT as u8 {
                let mut params = default_params();
                params.sync = true;
                params.division_l = division;
                params.division_r = division;
                dsp.set_params(params);
                dsp.set_tempo_bpm(bpm);
                assert!(
                    dsp.targets[0] >= 2.0 && dsp.targets[0] < dsp.lines[0].ring.max_cubic_delay(),
                    "division {division} at {bpm} BPM left the ring"
                );
                let ms = division_ms(division, bpm);
                assert!((1.0..=MAX_DELAY_MS).contains(&ms));
            }
        }
    }

    #[test]
    fn wire_update_changes_only_authoritative_params() {
        let mut dsp = Dsp::new(48_000.0);
        assert!(dsp.apply_wire_param(ipc::TIME_L_INDEX, 500.0));
        assert_eq!(dsp.params().time_ms_l, 500.0);
        assert!(dsp.apply_wire_param(ipc::DUCK_INDEX, 40.0));
        assert_eq!(dsp.params().duck, 40.0);
        assert!(!dsp.apply_wire_param(u32::MAX, 0.0));
        assert!(!dsp.apply_wire_param(ipc::TIME_L_INDEX, f32::NAN));
    }

    /// Two low tones: smooth, so a step anywhere shows as a kink.
    fn two_tone(n: usize) -> (f32, f32) {
        let t = n as f32 / SR;
        let x = (TAU * 110.0 * t).sin() * 0.3 + (TAU * 330.0 * t).sin() * 0.1;
        (x, x * 0.8)
    }

    /// The biggest second difference of the output over `frames` frames
    /// after a second of settling, with `edit` applied before each frame.
    fn worst_kink(dsp: &mut Dsp, frames: usize, mut edit: impl FnMut(&mut Dsp, usize)) -> f32 {
        let settle = 48_000;
        let mut history = [(0.0f32, 0.0f32); 2];
        let mut worst = 0.0f32;
        for n in 0..settle + frames {
            if n >= settle {
                edit(dsp, n - settle);
            }
            let (l, r) = two_tone(n);
            let out = dsp.process_stereo(l, r);
            if n >= settle {
                let d2 = |a: f32, b: f32, c: f32| (c - 2.0 * b + a).abs();
                let kink_l = d2(history[0].0, history[1].0, out.0);
                let kink_r = d2(history[0].1, history[1].1, out.1);
                worst = worst.max(kink_l).max(kink_r);
            }
            history = [history[1], out];
        }
        worst
    }

    /// Power and Mode crossfade: flipping either is no rougher than the
    /// delay left alone (it used to step by the whole wet signal).
    #[test]
    fn switching_power_and_mode_does_not_step() {
        let frames = 96_000;
        let mut params = default_params();
        params.mix = 50.0;
        params.feedback = 50.0;
        // Short times, so repeats of what was written since the switch came
        // on play back while it is still on.
        params.time_ms_l = 90.0;
        params.time_ms_r = 140.0;
        let still = worst_kink(&mut dsp_with(params.clone()), frames, |_, _| {});
        let power = worst_kink(&mut dsp_with(params.clone()), frames, |dsp, n| {
            if n % 12_000 == 0 {
                let on = (n / 12_000) % 2 == 1;
                assert!(dsp.apply_wire_param(ipc::POWER_INDEX, if on { 1.0 } else { 0.0 }));
            }
        });
        let mode = worst_kink(&mut dsp_with(params), frames, |dsp, n| {
            if n % 6_000 == 0 {
                let mode = DelayMode::ALL[(n / 6_000) % 3];
                assert!(dsp.apply_wire_param(ipc::MODE_INDEX, mode.to_wire()));
            }
        });
        assert!(power < still * 2.0, "power: {power} against {still}");
        assert!(mode < still * 2.0, "mode: {mode} against {still}");
    }

    /// Off and settled is a bit-exact pass-through, and switching back on
    /// starts from empty lines rather than what played before.
    #[test]
    fn power_off_settles_to_the_dry_signal_and_wakes_empty() {
        let mut params = default_params();
        params.mix = 100.0;
        params.feedback = 80.0;
        let mut dsp = dsp_with(params);
        for n in 0..24_000 {
            let (l, r) = two_tone(n);
            let _ = dsp.process_stereo(l, r);
        }
        assert!(dsp.apply_wire_param(ipc::POWER_INDEX, 0.0));
        for _ in 0..960 {
            let _ = dsp.process_stereo(0.1, 0.1);
        }
        assert_eq!(dsp.process_stereo(0.25, -0.25), (0.25, -0.25));
        assert!(dsp.apply_wire_param(ipc::POWER_INDEX, 1.0));
        let mut heard = 0.0f32;
        for _ in 0..4_800 {
            let (l, r) = dsp.process_stereo(0.0, 0.0);
            heard = heard.max(l.abs()).max(r.abs());
        }
        assert!(heard < 1.0e-6, "old repeats came back: {heard}");
    }

    /// A state load lands as saved, with no fade.
    #[test]
    fn a_loaded_power_off_does_not_fade() {
        let mut dsp = Dsp::new(SR);
        for _ in 0..100 {
            let _ = dsp.process_stereo(0.2, 0.2);
        }
        let mut params = default_params();
        params.power = false;
        dsp.set_params(params);
        assert_eq!(dsp.process_stereo(0.25, -0.25), (0.25, -0.25));
    }

    #[test]
    fn the_tone_the_editor_draws_is_the_tone_that_plays() {
        let params = default_params();
        assert!(tone_response_db(&params, 1_000.0, SR).abs() < 0.5);
        assert!(tone_response_db(&params, 40.0, SR) < -20.0);
        assert!(tone_response_db(&params, 18_000.0, SR) < -9.0);
    }
}

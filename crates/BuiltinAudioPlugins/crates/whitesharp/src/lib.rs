//! WhiteSharp — realtime pitch correction (Auto mode).
//!
//! ```txt
//! input ─┬─ mono sum ─ decimate ─ YIN ─ target (key, scale, removed/bypassed notes)
//!        │                               ─ retune speed · humanize · flex-tune
//!        │                               ─ natural vibrato · transpose · detune
//!        │                                        │ ratio, period, formant
//!        └─ stereo TD-PSOLA ◄────────────────────┘ ─ mix ─ output
//! ```
//!
//! Every few milliseconds the detector measures the voice's period. The note
//! it is pulled to is the nearest one the key and scale allow; how fast the
//! pitch gets there is the retune speed (zero is the hard, stepped effect),
//! slowed by Humanize on held notes; Flex-Tune lets expressive notes that
//! stray far from any target alone. The correction then drives a
//! pitch-synchronous shifter that keeps the voice's formants — or, with
//! formant correction off, moves them with the pitch — and Throat lengthens
//! or shortens the vocal tract on top. Classic mode swaps that response for
//! the older one: the nearest note at once, reached at a constant rate. A
//! vibrato of its own (shape, rate, delay, onset, pitch and amplitude depth,
//! variation) can be laid on every corrected note.
//!
//! The shifter runs a fixed delay per input type (reported as latency, so
//! the graph compensates for it). Live latency swaps it for a path with no
//! lookahead (`splice`): a delay-line head read at the shifted speed and
//! spliced a period at a time — two samples of fixed delay, the formants
//! moving with the pitch. A change of latency mode crossfades the two
//! paths. Everything is sized at construction for the widest input type:
//! nothing allocates on the audio path.

use builtin_dsp_core::delay::{Smoothed, smoothing_step};
use builtin_dsp_core::{
    ParamDescriptor, PluginCategory, PluginDescriptor, StereoEffect, clamp, db_to_linear,
};
use serde::{Deserialize, Serialize};

pub mod detect;
pub mod ipc;
pub mod presets;
pub mod scale;
pub mod shifter;
pub mod splice;
pub mod telemetry;
pub mod ui;

pub use ipc::{UI_PARAM_IDS, ui_param_id, ui_param_index};
pub use presets::{FactoryPreset, factory_presets};
pub use scale::{NOTE_NAMES, Scale, scale_mask};

use detect::{Detector, Estimate};
use scale::{ALL_NOTES, nearest_target, pitch_class};
use shifter::{Control, MAX_FORMANT, Shifter};
use splice::Splicer;
use telemetry::{History, Reading};

pub const PLUGIN_ID: &str = "futureboard.whitesharp";

/// Longest Retune Speed, in milliseconds.
pub const MAX_RETUNE_MS: f32 = 400.0;
/// How much Humanize at 100 % slows the retune of a long-held note.
const HUMANIZE_MS: f32 = 350.0;
/// How long a note must be held before Humanize slows it fully.
const HUMANIZE_HOLD_MS: f32 = 400.0;
/// The natural vibrato is what the pitch does above this rate.
const VIBRATO_SPLIT_HZ: f32 = 3.0;
/// A voice silent this long starts its next note uncorrected.
const RELEASE_MS: f32 = 150.0;
/// Readout and power crossfades.
const SMOOTH_MS: f32 = 10.0;
/// The crossfade between the two paths when the latency mode changes. They
/// play the voice at different delays, so the fade keeps power, not gain.
const MODE_FADE_MS: f32 = 20.0;

/// The voice the detector listens for. Each narrows the pitch range, which
/// makes tracking surer and — the low ones aside — the delay shorter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum InputType {
    Soprano,
    AltoTenor,
    LowMale,
    Instrument,
    BassInstrument,
}

impl InputType {
    pub const ALL: [Self; 5] = [
        Self::Soprano,
        Self::AltoTenor,
        Self::LowMale,
        Self::Instrument,
        Self::BassInstrument,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Soprano => "Soprano",
            Self::AltoTenor => "Alto/Tenor",
            Self::LowMale => "Low Male",
            Self::Instrument => "Instrument",
            Self::BassInstrument => "Bass Inst.",
        }
    }

    /// Lowest and highest fundamental tracked, in hertz.
    pub const fn range_hz(self) -> (f32, f32) {
        match self {
            Self::Soprano => (150.0, 1_100.0),
            Self::AltoTenor => (90.0, 750.0),
            Self::LowMale => (60.0, 450.0),
            Self::Instrument => (50.0, 1_600.0),
            Self::BassInstrument => (30.0, 450.0),
        }
    }

    pub const fn to_wire(self) -> f32 {
        self as u8 as f32
    }

    pub fn from_wire(value: f32) -> Self {
        Self::ALL[(value.round().max(0.0) as usize).min(Self::ALL.len() - 1)]
    }
}

/// The shape of the vibrato WhiteSharp creates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum VibratoShape {
    #[default]
    None,
    Sine,
    Square,
    Sawtooth,
}

impl VibratoShape {
    pub const ALL: [Self; 4] = [Self::None, Self::Sine, Self::Square, Self::Sawtooth];

    pub fn label(self) -> &'static str {
        match self {
            Self::None => "Off",
            Self::Sine => "Sine",
            Self::Square => "Square",
            Self::Sawtooth => "Saw",
        }
    }

    pub const fn to_wire(self) -> f32 {
        self as u8 as f32
    }

    pub fn from_wire(value: f32) -> Self {
        Self::ALL[(value.round().max(0.0) as usize).min(Self::ALL.len() - 1)]
    }

    /// The wave at `phase` (cycles), −1 to 1.
    #[inline]
    pub fn at(self, phase: f32) -> f32 {
        let sine = (std::f32::consts::TAU * phase).sin();
        match self {
            Self::None => 0.0,
            Self::Sine => sine,
            // Rounded corners: a hard step in pitch every half cycle would
            // click in the grains.
            Self::Square => (5.0 * sine).tanh() / 5.0f32.tanh(),
            Self::Sawtooth => 2.0 * phase.fract() - 1.0,
        }
    }
}

/// How the correction is played: the delay it costs against what it can do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum LatencyMode {
    /// Pitch-synchronous grains (PSOLA): formant correction and throat, at
    /// a fixed delay per input type that the graph compensates.
    #[default]
    Quality,
    /// No lookahead: a delay-line head spliced a period at a time. Two
    /// samples of fixed delay, plus up to about a period while the pitch
    /// is moved; the formants move with the pitch.
    Live,
}

impl LatencyMode {
    pub const ALL: [Self; 2] = [Self::Quality, Self::Live];

    pub fn label(self) -> &'static str {
        match self {
            Self::Quality => "Quality",
            Self::Live => "Live",
        }
    }

    /// Whether formant correction and throat work in this mode.
    pub const fn shapes_formants(self) -> bool {
        matches!(self, Self::Quality)
    }

    pub const fn to_wire(self) -> f32 {
        self as u8 as f32
    }

    pub fn from_wire(value: f32) -> Self {
        Self::ALL[(value.round().max(0.0) as usize).min(Self::ALL.len() - 1)]
    }
}

/// The lowest pitch any input type reaches: what the buffers are sized for.
const LOWEST_HZ: f32 = 30.0;

/// The latency a WhiteSharp at `sample_rate` reports for `input_type` in
/// `mode`, without building one — what an editor shows. Matches
/// [`Dsp::latency_samples`] exactly.
pub fn latency_samples_for(sample_rate: f32, input_type: InputType, mode: LatencyMode) -> usize {
    match mode {
        LatencyMode::Live => splice::LATENCY,
        LatencyMode::Quality => {
            let sr = sample_rate.max(1.0);
            let (lowest, _) = input_type.range_hz();
            let (centre_lag, hop) = detect::timing(sr, lowest);
            Shifter::latency_for_input(sr, sr / LOWEST_HZ, sr / lowest, centre_lag + hop) as usize
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Params {
    pub power: bool,
    /// Tonic, 0 = C.
    pub key: u8,
    pub scale: Scale,
    pub input_type: InputType,
    /// How long the pitch takes to reach its note, in milliseconds.
    pub retune_ms: f32,
    /// How much a long-held note's retune is slowed, in percent.
    pub humanize: f32,
    /// How far a note may stray from its target and still be left alone, in
    /// percent: 0 corrects everything.
    pub flex_tune: f32,
    /// The voice's own vibrato, scaled, in decibels.
    pub vibrato_db: f32,
    /// Keep the formants where they were while the pitch moves.
    pub formant: bool,
    /// Vocal tract length, in percent.
    pub throat: f32,
    /// Output shift, in semitones.
    pub transpose: f32,
    /// Reference pitch offset from A = 440 Hz, in cents.
    pub detune: f32,
    /// How periodic a frame must be to be tracked, in percent: higher is
    /// more relaxed.
    pub tracking: f32,
    /// Pitch classes never targeted.
    pub remove_mask: u16,
    /// Pitch classes left uncorrected.
    pub bypass_mask: u16,
    /// Dry/wet, in percent.
    pub mix: f32,
    pub output_db: f32,
    /// The older Auto-Tune response: the nearest note at once, reached at a
    /// constant rate; Humanize and Flex-Tune step aside.
    #[serde(default)]
    pub classic: bool,
    /// Create Vibrato: the shape laid on corrected notes, or none.
    #[serde(default)]
    pub vibrato_shape: VibratoShape,
    #[serde(default = "default_vibrato_rate")]
    pub vibrato_rate_hz: f32,
    /// How long a note is held before the vibrato starts, in milliseconds.
    #[serde(default = "default_vibrato_delay")]
    pub vibrato_delay_ms: f32,
    /// How long it takes to reach full depth after that, in milliseconds.
    #[serde(default = "default_vibrato_onset")]
    pub vibrato_onset_ms: f32,
    /// Pitch depth, in cents either side.
    #[serde(default = "default_vibrato_pitch")]
    pub vibrato_pitch: f32,
    /// Amplitude depth, in percent.
    #[serde(default)]
    pub vibrato_amp: f32,
    /// How much each cycle's rate and depth wander, in percent.
    #[serde(default)]
    pub vibrato_variation: f32,
    /// Grains with formant control at a compensated delay, or the live path
    /// with none. A state saved before this existed is Quality.
    #[serde(default)]
    pub latency: LatencyMode,
}

fn default_vibrato_rate() -> f32 {
    5.5
}

fn default_vibrato_delay() -> f32 {
    500.0
}

fn default_vibrato_onset() -> f32 {
    300.0
}

fn default_vibrato_pitch() -> f32 {
    35.0
}

pub fn default_params() -> Params {
    Params {
        power: true,
        key: 0,
        scale: Scale::Chromatic,
        input_type: InputType::AltoTenor,
        retune_ms: 20.0,
        humanize: 0.0,
        flex_tune: 0.0,
        vibrato_db: 0.0,
        formant: true,
        throat: 100.0,
        transpose: 0.0,
        detune: 0.0,
        tracking: 50.0,
        remove_mask: 0,
        bypass_mask: 0,
        mix: 100.0,
        output_db: 0.0,
        classic: false,
        vibrato_shape: VibratoShape::None,
        vibrato_rate_hz: default_vibrato_rate(),
        vibrato_delay_ms: default_vibrato_delay(),
        vibrato_onset_ms: default_vibrato_onset(),
        vibrato_pitch: default_vibrato_pitch(),
        vibrato_amp: 0.0,
        vibrato_variation: 0.0,
        latency: LatencyMode::Quality,
    }
}

impl Params {
    /// The notes a voice can be pulled to.
    pub fn allowed_notes(&self) -> u16 {
        scale_mask(self.key, self.scale) & !self.remove_mask & ALL_NOTES
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
    param("key", "Key", 0.0, 0.0, 11.0, "note"),
    param("scale", "Scale", 0.0, 0.0, 11.0, "enum"),
    param("inputType", "Input Type", 1.0, 0.0, 4.0, "enum"),
    param("retuneMs", "Retune Speed", 20.0, 0.0, MAX_RETUNE_MS, "ms"),
    param("humanize", "Humanize", 0.0, 0.0, 100.0, "%"),
    param("flexTune", "Flex-Tune", 0.0, 0.0, 100.0, "%"),
    param("vibratoDb", "Natural Vibrato", 0.0, -12.0, 12.0, "dB"),
    param("formant", "Formant", 1.0, 0.0, 1.0, "bool"),
    param("throat", "Throat", 100.0, 70.0, 140.0, "%"),
    param("transpose", "Transpose", 0.0, -24.0, 24.0, "st"),
    param("detune", "Detune", 0.0, -100.0, 100.0, "cents"),
    param("tracking", "Tracking", 50.0, 0.0, 100.0, "%"),
    param("removeMask", "Removed Notes", 0.0, 0.0, 4_095.0, "mask"),
    param("bypassMask", "Bypassed Notes", 0.0, 0.0, 4_095.0, "mask"),
    param("mix", "Mix", 100.0, 0.0, 100.0, "%"),
    param("outputDb", "Output", 0.0, -24.0, 12.0, "dB"),
    param("classic", "Classic", 0.0, 0.0, 1.0, "bool"),
    param("vibratoShape", "Vibrato Shape", 0.0, 0.0, 3.0, "enum"),
    param("vibratoRateHz", "Vibrato Rate", 5.5, 0.1, 10.0, "Hz"),
    param("vibratoDelayMs", "Vibrato Delay", 500.0, 0.0, 2_000.0, "ms"),
    param("vibratoOnsetMs", "Vibrato Onset", 300.0, 0.0, 2_000.0, "ms"),
    param("vibratoPitch", "Vibrato Pitch", 35.0, 0.0, 100.0, "cents"),
    param("vibratoAmp", "Vibrato Amplitude", 0.0, 0.0, 100.0, "%"),
    param(
        "vibratoVariation",
        "Vibrato Variation",
        0.0,
        0.0,
        100.0,
        "%",
    ),
    param("latency", "Latency", 0.0, 0.0, 1.0, "enum"),
];

pub fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PLUGIN_ID,
        name: "WhiteSharp",
        vendor: "Futureboard",
        category: PluginCategory::Effect,
        version: env!("CARGO_PKG_VERSION"),
        params: PARAMS,
    }
}
/// Everything per-estimate work reads, resolved from `Params` on each edit.
#[derive(Debug, Clone, Copy)]
struct Tuning {
    allowed: u16,
    bypass: u16,
    threshold: f32,
    reference_hz: f32,
    retune_ms: f32,
    humanize_ms: f32,
    /// Flex-Tune's zone: full correction within `inner` cents of the
    /// target, none beyond `outer`.
    flex: Option<(f32, f32)>,
    vibrato_extra: f32,
    transpose_cents: f32,
    preserve_formants: bool,
    throat: f32,
    mix: f32,
    output_gain: f32,
    classic: bool,
    vibrato: VibratoShape,
    vibrato_rate: f32,
    vibrato_delay: f32,
    vibrato_onset: f32,
    vibrato_pitch: f32,
    vibrato_amp: f32,
    vibrato_variation: f32,
    live: bool,
}

impl Tuning {
    fn resolve(p: &Params) -> Self {
        let flex = clamp(p.flex_tune / 100.0, 0.0, 1.0);
        let inner = 15.0 + (1.0 - flex) * 585.0;
        Self {
            allowed: p.allowed_notes(),
            bypass: p.bypass_mask & ALL_NOTES,
            threshold: 0.06 + 0.24 * clamp(p.tracking / 100.0, 0.0, 1.0),
            reference_hz: 440.0 * 2f32.powf(clamp(p.detune, -100.0, 100.0) / 1_200.0),
            retune_ms: clamp(p.retune_ms, 0.0, MAX_RETUNE_MS),
            humanize_ms: clamp(p.humanize / 100.0, 0.0, 1.0) * HUMANIZE_MS,
            flex: (flex > 0.0).then_some((inner, inner + 25.0)),
            vibrato_extra: db_to_linear(clamp(p.vibrato_db, -12.0, 12.0)) - 1.0,
            transpose_cents: clamp(p.transpose.round(), -24.0, 24.0) * 100.0,
            preserve_formants: p.formant,
            throat: clamp(p.throat, 70.0, 140.0) / 100.0,
            mix: clamp(p.mix / 100.0, 0.0, 1.0),
            output_gain: db_to_linear(clamp(p.output_db, -24.0, 12.0)),
            classic: p.classic,
            vibrato: p.vibrato_shape,
            vibrato_rate: clamp(p.vibrato_rate_hz, 0.1, 10.0),
            vibrato_delay: clamp(p.vibrato_delay_ms, 0.0, 2_000.0) * 0.001,
            vibrato_onset: clamp(p.vibrato_onset_ms, 0.0, 2_000.0) * 0.001,
            vibrato_pitch: clamp(p.vibrato_pitch, 0.0, 100.0),
            vibrato_amp: clamp(p.vibrato_amp / 100.0, 0.0, 1.0),
            vibrato_variation: clamp(p.vibrato_variation / 100.0, 0.0, 1.0),
            live: p.latency == LatencyMode::Live,
        }
    }
}

/// MIDI note number of `hz` against an A of `reference_hz`.
#[inline]
pub fn midi_of(hz: f32, reference_hz: f32) -> f32 {
    69.0 + 12.0 * (hz / reference_hz).log2()
}

#[derive(Debug, Clone)]
pub struct Dsp {
    sample_rate: f32,
    params: Params,
    tuning: Tuning,
    detector: Detector,
    shifter: Shifter,
    splicer: Splicer,
    /// Where the output stands between the paths: 0 is the PSOLA shifter,
    /// 1 the live splicer. Moves by `mode_step` a sample on a mode change.
    mode_mix: f32,
    mode_step: f32,
    /// Whether each path rendered the last sample. A path that idled
    /// restarts cleanly before it plays again.
    quality_running: bool,
    live_running: bool,
    history: History,
    /// Input samples taken.
    now: u64,
    hop_seconds: f32,
    target: Option<i32>,
    /// Since the target last changed, in seconds.
    held: f32,
    /// Since the voice was last voiced, in seconds.
    silent: f32,
    /// The correction applied now, in cents.
    correction: f32,
    /// The pitch's slow part, which the vibrato rides on.
    slow_pitch: Option<f32>,
    vibrato_step: f32,
    /// The created vibrato: its phase in cycles, and this cycle's rate and
    /// depth factors.
    vibrato_phase: f32,
    vibrato_rate_factor: f32,
    vibrato_depth_factor: f32,
    /// Seed of the variation's wander.
    seed: u32,
    smooth_step: f32,
    active: Smoothed,
    mix: Smoothed,
    output_gain: Smoothed,
}

impl Dsp {
    pub fn new(sample_rate: f32) -> Self {
        let sr = sample_rate.max(1.0);
        let detector = Detector::new(sr, LOWEST_HZ);
        let shifter = Shifter::new(sr, sr / LOWEST_HZ);
        let splicer = Splicer::new(sr, sr / LOWEST_HZ);
        let hop_seconds = detector.hop_samples() as f32 / sr;
        let params = default_params();
        let tuning = Tuning::resolve(&params);
        let mut dsp = Self {
            sample_rate: sr,
            tuning,
            detector,
            shifter,
            splicer,
            mode_mix: if tuning.live { 1.0 } else { 0.0 },
            mode_step: 1.0 / (MODE_FADE_MS * 0.001 * sr).max(1.0),
            quality_running: true,
            live_running: true,
            history: History::default(),
            now: 0,
            hop_seconds,
            target: None,
            held: 0.0,
            silent: 0.0,
            correction: 0.0,
            slow_pitch: None,
            vibrato_step: 1.0 - (-std::f32::consts::TAU * VIBRATO_SPLIT_HZ * hop_seconds).exp(),
            vibrato_phase: 0.0,
            vibrato_rate_factor: 1.0,
            vibrato_depth_factor: 1.0,
            seed: 0x2545_F491,
            smooth_step: smoothing_step(SMOOTH_MS, sr),
            active: Smoothed::at(1.0),
            mix: Smoothed::at(tuning.mix),
            output_gain: Smoothed::at(tuning.output_gain),
            params,
        };
        dsp.apply_input_type();
        dsp
    }

    pub fn params(&self) -> &Params {
        &self.params
    }

    pub fn set_params(&mut self, params: Params) {
        let input_changed = params.input_type != self.params.input_type;
        self.params = params;
        ipc::sanitize_params(&mut self.params);
        self.retune();
        if input_changed {
            self.apply_input_type();
        }
    }

    /// Apply a compact wire update already resolved by the UI/control thread.
    /// Allocation-free: every arm re-resolves the tuning table, and an input
    /// type change re-ranges the detector and the shifter's delay.
    pub fn apply_wire_param(&mut self, wire_index: u32, value: f32) -> bool {
        let before = self.params.input_type;
        if !ipc::apply_wire_param(&mut self.params, wire_index, value) {
            return false;
        }
        self.retune();
        if self.params.input_type != before {
            self.apply_input_type();
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

    /// The delay the correction runs at, for graph delay compensation: fixed
    /// per input type in Quality, the live path's two samples in Live. The
    /// new mode's from the moment it is set; the crossfade between the
    /// paths follows it.
    pub fn latency_samples(&self) -> usize {
        match self.params.latency {
            LatencyMode::Quality => self.shifter.latency() as usize,
            LatencyMode::Live => self.splicer.latency(),
        }
    }

    /// The live path's head delay now (its fixed latency plus what the
    /// shifting has wandered) and how many splices it has made — for
    /// measuring it.
    pub fn live_head(&self) -> (f64, u64) {
        (self.splicer.delay(), self.splicer.splices())
    }

    /// The newest pitch readings, as the editor's meter and keyboard read
    /// them.
    pub fn telemetry(&self) -> [f32; telemetry::SLOTS] {
        self.history.encode()
    }

    fn retune(&mut self) {
        self.tuning = Tuning::resolve(&self.params);
        self.mix.target = self.tuning.mix;
        self.output_gain.target = self.tuning.output_gain;
        self.active.target = if self.params.power { 1.0 } else { 0.0 };
        if self.now == 0 {
            self.mix.settle();
            self.output_gain.settle();
            self.active.settle();
            self.settle_mode();
        }
    }

    /// Puts the output wholly on the mode's path, without a fade.
    fn settle_mode(&mut self) {
        self.mode_mix = if self.tuning.live { 1.0 } else { 0.0 };
    }

    fn apply_input_type(&mut self) {
        let (lowest, highest) = self.params.input_type.range_hz();
        self.detector.set_range(lowest, highest);
        self.shifter.set_longest_period(
            self.sample_rate / lowest,
            self.detector.centre_lag() + self.detector.hop_samples(),
        );
        self.target = None;
        self.correction = 0.0;
        self.slow_pitch = None;
    }

    /// One estimate's worth of correction: the target, the smoothed pull
    /// toward it, and the control frame the shifter will play it with.
    fn on_estimate(&mut self, estimate: Estimate) {
        let t = self.tuning;
        let centre = self.now.saturating_sub(self.detector.centre_lag() as u64);
        let hop = self.hop_seconds;
        let Some(period) = estimate.period else {
            self.silent += hop;
            if self.silent * 1_000.0 >= RELEASE_MS {
                self.target = None;
                self.correction = 0.0;
            }
            self.slow_pitch = None;
            self.vibrato_phase = 0.0;
            self.shifter.push_control(Control {
                centre,
                period: None,
                ratio: 1.0,
                formant: 1.0,
                gain: 1.0,
            });
            self.splicer.set_control(None, 1.0, 1.0);
            self.history.push(Reading::SILENT);
            return;
        };
        self.silent = 0.0;

        let midi = midi_of(self.sample_rate / period, t.reference_hz);
        let slow = match self.slow_pitch {
            Some(slow) => slow + (midi - slow) * self.vibrato_step,
            None => midi,
        };
        self.slow_pitch = Some(slow);
        let vibrato = clamp(midi - slow, -2.0, 2.0);

        // Classic snaps to whichever note is nearest now; the modern response
        // holds a note until the voice is clearly past the boundary.
        let held_note = if t.classic { None } else { self.target };
        let target = nearest_target(midi, t.allowed, held_note);
        if target != self.target {
            self.held = 0.0;
        }
        self.target = target;
        self.held += hop;

        let corrected = target.filter(|note| t.bypass & (1 << pitch_class(*note)) == 0);
        let desired = match corrected {
            Some(note) => {
                let cents = (note as f32 - midi) * 100.0;
                let weight = match t.flex.filter(|_| !t.classic) {
                    Some((inner, outer)) => {
                        clamp((outer - cents.abs()) / (outer - inner), 0.0, 1.0)
                    }
                    None => 1.0,
                };
                cents * weight
            }
            None => 0.0,
        };
        if t.classic {
            // A constant rate: a semitone per retune time.
            let step = if t.retune_ms <= 0.0 {
                f32::INFINITY
            } else {
                100.0 * hop * 1_000.0 / t.retune_ms
            };
            self.correction += clamp(desired - self.correction, -step, step);
        } else {
            let hold = clamp(self.held * 1_000.0 / HUMANIZE_HOLD_MS, 0.0, 1.0);
            let tau_ms = t.retune_ms + t.humanize_ms * hold;
            let pull = if tau_ms <= 0.0 {
                1.0
            } else {
                1.0 - (-hop * 1_000.0 / tau_ms).exp()
            };
            self.correction += (desired - self.correction) * pull;
        }

        let (created_cents, gain) = if corrected.is_some() {
            self.created_vibrato()
        } else {
            self.vibrato_phase = 0.0;
            (0.0, 1.0)
        };
        let cents =
            self.correction + vibrato * 100.0 * t.vibrato_extra + created_cents + t.transpose_cents;
        let ratio = 2f32.powf(cents / 1_200.0);
        let formant = if t.preserve_formants { 1.0 } else { ratio } / t.throat;
        self.shifter.push_control(Control {
            centre,
            period: Some(period),
            ratio,
            formant: clamp(formant, 1.0 / MAX_FORMANT, MAX_FORMANT),
            gain,
        });
        // The live path plays the newest estimate at once: there is no
        // delay to line it up with the audio it measured.
        self.splicer.set_control(Some(period), ratio, gain);
        self.history.push(Reading {
            input: Some(midi),
            output: Some(midi + cents / 100.0),
            target: target.map(|note| note + (t.transpose_cents / 100.0) as i32),
        });
    }

    /// Runs whichever path the latency mode plays — both while one fades
    /// into the other — and returns the wet frame and the input at the same
    /// delay. The idle path still takes its input, so it can take over
    /// without a gap.
    #[inline]
    fn play_paths(&mut self, left: f32, right: f32) -> ([f32; 2], [f32; 2]) {
        let target = if self.tuning.live { 1.0 } else { 0.0 };
        if self.mode_mix != target {
            self.mode_mix = if self.mode_mix < target {
                (self.mode_mix + self.mode_step).min(target)
            } else {
                (self.mode_mix - self.mode_step).max(target)
            };
        }
        let m = self.mode_mix;
        let quality = if m < 1.0 {
            if !self.quality_running {
                self.shifter.resume();
                self.quality_running = true;
            }
            Some(self.shifter.process(left, right))
        } else {
            self.shifter.skip(left, right);
            self.quality_running = false;
            None
        };
        let live = if m > 0.0 {
            if !self.live_running {
                self.splicer.restart();
                self.live_running = true;
            }
            Some(self.splicer.process(left, right))
        } else {
            self.splicer.skip(left, right);
            self.live_running = false;
            None
        };
        match (quality, live) {
            (Some(q), Some(z)) => {
                let angle = std::f32::consts::FRAC_PI_2 * m;
                let (gq, gz) = (angle.cos(), angle.sin());
                let blend =
                    |a: [f32; 2], b: [f32; 2]| [a[0] * gq + b[0] * gz, a[1] * gq + b[1] * gz];
                (blend(q.0, z.0), blend(q.1, z.1))
            }
            (Some(path), None) | (None, Some(path)) => path,
            (None, None) => ([0.0; 2], [0.0; 2]),
        }
    }

    /// One hop of the created vibrato: its pitch offset in cents and its
    /// amplitude gain. Silent until the note has been held for the delay,
    /// then fading in over the onset; each cycle's rate and depth wander by
    /// the variation.
    fn created_vibrato(&mut self) -> (f32, f32) {
        let t = self.tuning;
        if t.vibrato == VibratoShape::None {
            return (0.0, 1.0);
        }
        let since = self.held - t.vibrato_delay;
        if since <= 0.0 {
            // Each note's vibrato starts from the centre of its wave.
            self.vibrato_phase = 0.0;
            return (0.0, 1.0);
        }
        let envelope = if t.vibrato_onset <= 0.0 {
            1.0
        } else {
            clamp(since / t.vibrato_onset, 0.0, 1.0)
        };
        self.vibrato_phase += t.vibrato_rate * self.vibrato_rate_factor * self.hop_seconds;
        if self.vibrato_phase >= 1.0 {
            self.vibrato_phase -= self.vibrato_phase.floor();
            let mut wander = || {
                self.seed ^= self.seed << 13;
                self.seed ^= self.seed >> 17;
                self.seed ^= self.seed << 5;
                (self.seed >> 8) as f32 / (1u32 << 23) as f32 - 1.0
            };
            let (rate, depth) = (wander(), wander());
            self.vibrato_rate_factor = 1.0 + 0.3 * t.vibrato_variation * rate;
            self.vibrato_depth_factor = (1.0 + 0.5 * t.vibrato_variation * depth).max(0.0);
        }
        let wave = t.vibrato.at(self.vibrato_phase) * envelope * self.vibrato_depth_factor;
        (wave * t.vibrato_pitch, 1.0 + 0.5 * t.vibrato_amp * wave)
    }
}

impl StereoEffect for Dsp {
    fn reset(&mut self) {
        self.detector.reset();
        self.shifter.reset();
        self.splicer.reset();
        self.settle_mode();
        self.quality_running = true;
        self.live_running = true;
        self.target = None;
        self.correction = 0.0;
        self.slow_pitch = None;
        self.held = 0.0;
        self.silent = 0.0;
        self.vibrato_phase = 0.0;
    }

    fn set_sample_rate(&mut self, sample_rate: f32) {
        let sr = sample_rate.max(1.0);
        if (sr - self.sample_rate).abs() < f32::EPSILON {
            return;
        }
        let params = self.params.clone();
        *self = Self::new(sr);
        self.set_params(params);
        self.apply_input_type();
    }

    fn process_stereo(&mut self, left: f32, right: f32) -> (f32, f32) {
        if let Some(estimate) = self
            .detector
            .push((left + right) * 0.5, self.tuning.threshold)
        {
            self.on_estimate(estimate);
        }
        let (wet, dry) = self.play_paths(left, right);
        self.now += 1;

        let step = self.smooth_step;
        let active = self.active.next(step);
        let mix = self.mix.next(step);
        let gain = self.output_gain.next(step);
        let out = |side: usize| {
            let processed = (dry[side] + (wet[side] - dry[side]) * mix) * gain;
            // Off is the delayed input, so the latency the graph compensates
            // for holds either way.
            dry[side] + (processed - dry[side]) * active
        };
        (out(0), out(1))
    }
}

#[cfg(test)]
mod tests;

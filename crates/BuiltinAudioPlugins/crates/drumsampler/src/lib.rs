//! Drum Sampler - a realtime-safe 16-pad one-shot drum instrument.
//!
//! Each pad plays a region of a user-loaded sample on its own mapped MIDI
//! note, through its own attack/hold/decay envelope and state-variable
//! filter, and the kit sums into a master gain and tune. Sample bytes are
//! decoded off the audio thread (native plugin host, IPC thread) and handed
//! to the audio thread through a wait-free single-slot cell, the same shape
//! rodharerist uses for impulse responses
//! (`rodharerist::dsp::handoff::HandoffCell`) - decode, resample or FFT work
//! never happens on the realtime path, only a pointer swap does.
//!
//! Realtime contract: voices, filter coefficients and meters are fixed-size
//! arrays owned by [`Dsp`]; `note_on`, `process_stereo` and
//! `apply_wire_param` only do arithmetic on them. Envelope and filter
//! coefficients are resolved at control rate (on a parameter edit, or once
//! per note), never per sample.

pub mod handoff;
pub mod ipc;
pub mod ui;

use builtin_dsp_core::{
    Instrument, ParamDescriptor, PluginCategory, PluginDescriptor, clamp, db_to_linear,
    flush_denormal, max_filter_frequency, time_constant,
};
use handoff::HandoffCell;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub use ipc::{UI_PARAM_IDS, ui_param_id, ui_param_index};

pub const PLUGIN_ID: &str = "futureboard.drumsampler";
pub const PADS: usize = 16;
pub const MAX_VOICES: usize = 32;
/// Fixed fade applied when one pad chokes another in the same group - short
/// enough to read as an instant cutoff (closed hat stopping an open hat)
/// regardless of the choked pad's own envelope.
const CHOKE_FADE_MS: f32 = 15.0;

/// Shortest region a start/end pair can leave, as a fraction of the sample,
/// so a pad can never be trimmed to nothing.
pub const MIN_REGION: f32 = 0.001;

pub const MIN_CUTOFF_HZ: f32 = 20.0;
pub const MAX_CUTOFF_HZ: f32 = 20_000.0;
pub const MAX_HOLD_MS: f32 = 5_000.0;
/// `decay_ms` of zero means "no decay": the sample plays out at full level,
/// which is how every pad behaved before the envelope had a decay stage.
pub const MAX_DECAY_MS: f32 = 10_000.0;

/// The level a decaying envelope treats as silence (−60 dB); the decay time
/// is the time taken to reach it.
const DECAY_FLOOR: f32 = 0.001;
/// Filter resonance travel: 0 % is a Butterworth response (no peak), 100 %
/// a sharp peak of Q 12.
const MIN_Q: f32 = std::f32::consts::FRAC_1_SQRT_2;
const MAX_Q: f32 = 12.0;
/// Pad meters fall back over this long.
const METER_RELEASE_SEC: f32 = 0.3;
const RMS_SEC: f32 = 0.3;

/// Points in the waveform overview the host sends the editor with a loaded
/// sample.
pub const WAVEFORM_POINTS: usize = 256;

/// One decoded one-shot sample, interleaved, normalized to -1.0..1.0.
#[derive(Debug)]
pub struct PadBuffer {
    pub samples: Box<[f32]>,
    pub channels: usize,
    pub frames: usize,
    pub sample_rate: f32,
}

/// Control-thread <-> audio-thread hand-off for one pad's sample buffer.
///
/// Thread contract mirrors `rodharerist`'s `IrLoader`: exactly one control
/// thread calls `submit`/`collect_garbage`; only the audio thread calls
/// `adopt_pending`, and only ever from [`Dsp::note_on`] or [`Dsp::reset`].
struct PadChannel {
    pending: HandoffCell<PadBuffer>,
    retired: HandoffCell<PadBuffer>,
}

impl PadChannel {
    fn new() -> Self {
        Self {
            pending: HandoffCell::new(),
            retired: HandoffCell::new(),
        }
    }
}

/// Cloneable control-side handle for loading a pad's sample.
#[derive(Clone)]
pub struct PadLoader {
    channel: Arc<PadChannel>,
}

impl PadLoader {
    /// Drop any buffer the audio thread has retired (safe: it was already
    /// unlinked from the live pad on the audio thread).
    pub fn collect_garbage(&self) {
        if let Some(dead) = self.channel.retired.take() {
            drop(dead);
        }
    }

    /// Submit a freshly decoded buffer for the audio thread to adopt the next
    /// time this pad is triggered. A not-yet-adopted buffer already waiting
    /// is dropped here (safe: the audio thread never touched it).
    pub fn submit(&self, buffer: Box<PadBuffer>) {
        self.collect_garbage();
        if let Some(bumped) = self.channel.pending.put(buffer) {
            drop(bumped);
        }
    }
}

struct PadRuntime {
    channel: Arc<PadChannel>,
    buffer: Option<Box<PadBuffer>>,
}

impl PadRuntime {
    fn new() -> Self {
        Self {
            channel: Arc::new(PadChannel::new()),
            buffer: None,
        }
    }

    fn loader(&self) -> PadLoader {
        PadLoader {
            channel: self.channel.clone(),
        }
    }

    /// Audio-thread only: adopt a pending buffer if one is waiting, retiring
    /// (never dropping in place) whatever was live before it.
    fn adopt_pending(&mut self) {
        if let Some(fresh) = self.channel.pending.take() {
            if let Some(old) = self.buffer.replace(fresh) {
                if let Some(bumped) = self.channel.retired.put(old) {
                    // The control thread never drained the previous retiree -
                    // drop it here. This only happens if two loads land
                    // before a single `collect_garbage`, which is not the
                    // realtime path.
                    drop(bumped);
                }
            }
        }
    }
}

/// A pad's filter response. The wire value is the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FilterMode {
    #[default]
    Off,
    LowPass,
    HighPass,
    BandPass,
}

impl FilterMode {
    pub const ALL: [Self; 4] = [Self::Off, Self::LowPass, Self::HighPass, Self::BandPass];

    pub fn to_wire(self) -> f32 {
        Self::ALL.iter().position(|mode| *mode == self).unwrap_or(0) as f32
    }

    pub fn from_wire(value: f32) -> Self {
        Self::ALL[clamp(value, 0.0, (Self::ALL.len() - 1) as f32).round() as usize]
    }
}

/// `sample_name` is display/persistence-only — the audio referenced by a
/// project reload is not itself in this struct, only its file name in the
/// plugin's Samples sandbox (see [`PadLoader`]). It is never read on the
/// audio thread, only cloned on the control thread for `ui_values`/state
/// serialization, so a `Pad` is intentionally not `Copy`.
///
/// Every field added after the first release carries a serde default equal
/// to the behaviour pads had before it existed, so an older project opens
/// sounding the way it was saved.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pad {
    pub note: u8,
    pub tune_semitones: f32,
    pub gain_db: f32,
    pub pan: f32,
    pub choke_group: u8,
    pub attack_ms: f32,
    /// Legacy. The first release stored a release time that no stage of the
    /// voice ever read (a one-shot is not released by its key). It is kept so
    /// older projects round-trip unchanged, and still accepted on the wire,
    /// but the envelope is attack / hold / decay and nothing plays it.
    pub release_ms: f32,
    pub reverse: bool,
    pub muted: bool,
    pub solo: bool,
    #[serde(default)]
    pub sample_name: Option<String>,
    /// Region start, as a fraction of the sample.
    #[serde(default)]
    pub start: f32,
    /// Region end, as a fraction of the sample.
    #[serde(default = "full_region_end")]
    pub end: f32,
    #[serde(default)]
    pub filter_mode: FilterMode,
    #[serde(default = "default_cutoff")]
    pub cutoff_hz: f32,
    /// 0..100 %: Butterworth at 0, a Q-12 peak at 100.
    #[serde(default)]
    pub resonance: f32,
    /// 0..100 %: how much velocity changes the level. 100 % is fully
    /// velocity-scaled (the original behaviour); 0 % plays every hit at the
    /// pad's gain.
    #[serde(default = "default_velocity_sensitivity")]
    pub velocity_sensitivity: f32,
    /// Time at full level after the attack, before the decay starts.
    #[serde(default)]
    pub hold_ms: f32,
    /// Time from full level to −60 dB. `0` plays the region out untouched.
    #[serde(default)]
    pub decay_ms: f32,
}

fn full_region_end() -> f32 {
    1.0
}

fn default_cutoff() -> f32 {
    MAX_CUTOFF_HZ
}

fn default_velocity_sensitivity() -> f32 {
    100.0
}

pub fn default_pad(index: usize) -> Pad {
    Pad {
        note: 36 + index as u8,
        tune_semitones: 0.0,
        gain_db: 0.0,
        pan: 0.0,
        choke_group: 0,
        attack_ms: 1.0,
        release_ms: 60.0,
        reverse: false,
        muted: false,
        solo: false,
        sample_name: None,
        start: 0.0,
        end: full_region_end(),
        filter_mode: FilterMode::Off,
        cutoff_hz: default_cutoff(),
        resonance: 0.0,
        velocity_sensitivity: default_velocity_sensitivity(),
        hold_ms: 0.0,
        decay_ms: 0.0,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Params {
    pub pads: [Pad; PADS],
    /// Whole-kit level, applied before the output soft clip.
    #[serde(default)]
    pub master_gain_db: f32,
    /// Whole-kit pitch, added to every pad's own tune.
    #[serde(default)]
    pub master_tune: f32,
}

pub fn default_params() -> Params {
    Params {
        pads: std::array::from_fn(default_pad),
        master_gain_db: 0.0,
        master_tune: 0.0,
    }
}

pub fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PLUGIN_ID,
        name: "Drum Sampler",
        vendor: "Futureboard",
        category: PluginCategory::Instrument,
        version: env!("CARGO_PKG_VERSION"),
        params: &[
            ParamDescriptor {
                id: "masterGain",
                name: "Master Gain",
                default_value: 0.0,
                min: -60.0,
                max: 12.0,
                unit: "dB",
            },
            ParamDescriptor {
                id: "masterTune",
                name: "Master Tune",
                default_value: 0.0,
                min: -24.0,
                max: 24.0,
                unit: "st",
            },
            ParamDescriptor {
                id: "pad0Gain",
                name: "Pad 1 Gain",
                default_value: 0.0,
                min: -60.0,
                max: 12.0,
                unit: "dB",
            },
            ParamDescriptor {
                id: "pad0Tune",
                name: "Pad 1 Tune",
                default_value: 0.0,
                min: -24.0,
                max: 24.0,
                unit: "st",
            },
            ParamDescriptor {
                id: "pad0Pan",
                name: "Pad 1 Pan",
                default_value: 0.0,
                min: -1.0,
                max: 1.0,
                unit: "",
            },
        ],
    }
}

/// A downsampled overview of a decoded sample for the editor: the loudest
/// absolute sample (any channel) in each of `points` equal slices, scaled to
/// `0..=255`. Control thread only — it walks the whole buffer.
pub fn waveform_peaks(samples: &[f32], channels: usize, frames: usize, points: usize) -> Vec<u8> {
    let channels = channels.max(1);
    let frames = frames.min(samples.len() / channels);
    if frames == 0 || points == 0 {
        return vec![0; points];
    }
    (0..points)
        .map(|point| {
            let from = point * frames / points;
            let to = ((point + 1) * frames / points).max(from + 1).min(frames);
            let peak =
                samples[from * channels..to * channels]
                    .iter()
                    .fold(0.0f32, |peak, sample| {
                        if sample.is_finite() {
                            peak.max(sample.abs())
                        } else {
                            peak
                        }
                    });
            (peak.min(1.0) * 255.0).round() as u8
        })
        .collect()
}

/// The frame range `[first, last)` a pad plays of a `frames`-long buffer.
/// Start and end are sorted and kept at least [`MIN_REGION`] apart, so a
/// hand-edited or automated pair can never leave an empty or inverted region.
pub fn region_frames(pad: &Pad, frames: usize) -> (usize, usize) {
    let (a, b) = (clamp(pad.start, 0.0, 1.0), clamp(pad.end, 0.0, 1.0));
    let (mut lo, mut hi) = if a <= b { (a, b) } else { (b, a) };
    if hi - lo < MIN_REGION {
        hi = (lo + MIN_REGION).min(1.0);
        lo = hi - MIN_REGION;
    }
    let first = ((lo * frames as f32) as usize).min(frames.saturating_sub(1));
    let last = ((hi * frames as f32).ceil() as usize).clamp(first + 1, frames.max(first + 1));
    (first, last)
}

/// Topology-preserving-transform state-variable filter coefficients
/// (Zavalishin). Resolved per pad at control rate.
#[derive(Debug, Clone, Copy)]
struct SvfCoeffs {
    mode: FilterMode,
    k: f32,
    a1: f32,
    a2: f32,
    a3: f32,
}

impl SvfCoeffs {
    const BYPASS: Self = Self {
        mode: FilterMode::Off,
        k: 0.0,
        a1: 0.0,
        a2: 0.0,
        a3: 0.0,
    };

    fn for_pad(pad: &Pad, sample_rate: f32) -> Self {
        if pad.filter_mode == FilterMode::Off {
            return Self::BYPASS;
        }
        let cutoff = clamp(
            pad.cutoff_hz,
            MIN_CUTOFF_HZ,
            max_filter_frequency(sample_rate).min(MAX_CUTOFF_HZ),
        );
        let resonance = clamp(pad.resonance, 0.0, 100.0) / 100.0;
        let q = MIN_Q * (MAX_Q / MIN_Q).powf(resonance);
        let g = (std::f32::consts::PI * cutoff / sample_rate).tan();
        let k = 1.0 / q;
        let a1 = 1.0 / (1.0 + g * (g + k));
        let a2 = g * a1;
        let a3 = g * a2;
        Self {
            mode: pad.filter_mode,
            k,
            a1,
            a2,
            a3,
        }
    }
}

/// One channel of SVF state, carried by the voice.
#[derive(Debug, Clone, Copy, Default)]
struct SvfState {
    ic1: f32,
    ic2: f32,
}

impl SvfState {
    #[inline]
    fn run(&mut self, c: &SvfCoeffs, x: f32) -> f32 {
        let v3 = x - self.ic2;
        let v1 = c.a1 * self.ic1 + c.a2 * v3;
        let v2 = self.ic2 + c.a2 * self.ic1 + c.a3 * v3;
        self.ic1 = flush_denormal(2.0 * v1 - self.ic1);
        self.ic2 = flush_denormal(2.0 * v2 - self.ic2);
        match c.mode {
            FilterMode::Off => x,
            FilterMode::LowPass => v2,
            FilterMode::BandPass => v1,
            FilterMode::HighPass => x - c.k * v1 - v2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnvStage {
    Attack,
    Hold,
    Decay,
    /// Full level until the region ends (a pad with no decay).
    Sustain,
    /// Choked or cut: a short linear fade.
    Release,
}

#[derive(Debug, Clone, Copy)]
struct Voice {
    active: bool,
    pad_index: usize,
    /// Frames into the region, fractional.
    position: f64,
    step: f64,
    /// Region of the buffer this voice plays, captured at the trigger so a
    /// start/end edit shapes the next hit rather than jumping a sounding one.
    first: usize,
    last: usize,
    reverse: bool,
    level: f32,
    age: u64,
    env: f32,
    stage: EnvStage,
    attack_step: f32,
    hold_left: u32,
    decay_coeff: f32,
    release_per_sample: f32,
    filter_l: SvfState,
    filter_r: SvfState,
}

impl Voice {
    const fn silent() -> Self {
        Self {
            active: false,
            pad_index: 0,
            position: 0.0,
            step: 1.0,
            first: 0,
            last: 0,
            reverse: false,
            level: 0.0,
            age: 0,
            env: 0.0,
            stage: EnvStage::Sustain,
            attack_step: 1.0,
            hold_left: 0,
            decay_coeff: 0.0,
            release_per_sample: 0.0,
            filter_l: SvfState { ic1: 0.0, ic2: 0.0 },
            filter_r: SvfState { ic1: 0.0, ic2: 0.0 },
        }
    }

    /// Advance the envelope one sample and return its level.
    #[inline]
    fn envelope(&mut self) -> f32 {
        match self.stage {
            EnvStage::Attack => {
                self.env += self.attack_step;
                if self.env >= 1.0 {
                    self.env = 1.0;
                    self.stage = if self.decay_coeff > 0.0 {
                        EnvStage::Hold
                    } else {
                        EnvStage::Sustain
                    };
                }
            }
            EnvStage::Hold => {
                if self.hold_left == 0 {
                    self.stage = EnvStage::Decay;
                } else {
                    self.hold_left -= 1;
                }
            }
            EnvStage::Decay => {
                self.env *= self.decay_coeff;
                if self.env <= DECAY_FLOOR {
                    self.env = 0.0;
                    self.active = false;
                }
            }
            EnvStage::Sustain => {}
            EnvStage::Release => {
                self.env -= self.release_per_sample;
                if self.env <= 0.0 {
                    self.env = 0.0;
                    self.active = false;
                }
            }
        }
        self.env
    }
}

/// Output levels, in the shape every metering built-in hands the host. A
/// drum sampler has no audio input, so the input side stays zero.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct MeterFrame {
    pub out_peak: f32,
    pub out_rms: f32,
    pub out_clip: bool,
}

pub struct Dsp {
    sample_rate: f32,
    params: Params,
    pads: [PadRuntime; PADS],
    filters: [SvfCoeffs; PADS],
    voices: [Voice; MAX_VOICES],
    age: u64,
    master_gain: f32,
    /// Held peak of what each pad is contributing, for the pad meters.
    pad_peak: [f32; PADS],
    out_peak: f32,
    out_mean_square: f32,
    out_clip: bool,
    meter_release: f32,
    rms_coeff: f32,
}

impl std::fmt::Debug for Dsp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dsp")
            .field("sample_rate", &self.sample_rate)
            .field("params", &self.params)
            .field(
                "active_voices",
                &self.voices.iter().filter(|voice| voice.active).count(),
            )
            .finish()
    }
}

impl Dsp {
    pub fn new(sample_rate: f32) -> Self {
        let sample_rate = sample_rate.max(1.0);
        let mut dsp = Self {
            sample_rate,
            params: default_params(),
            pads: std::array::from_fn(|_| PadRuntime::new()),
            filters: [SvfCoeffs::BYPASS; PADS],
            voices: [Voice::silent(); MAX_VOICES],
            age: 0,
            master_gain: 1.0,
            pad_peak: [0.0; PADS],
            out_peak: 0.0,
            out_mean_square: 0.0,
            out_clip: false,
            meter_release: 0.0,
            rms_coeff: 0.0,
        };
        dsp.update_time_constants();
        dsp.refresh_all();
        dsp
    }

    pub fn params(&self) -> &Params {
        &self.params
    }

    pub fn set_params(&mut self, mut params: Params) {
        ipc::sanitize_params(&mut params);
        self.params = params;
        self.refresh_all();
    }

    /// Apply a compact wire update already resolved by the UI/control thread.
    /// Allocation-free: at most one pad's filter coefficients are recomputed.
    pub fn apply_wire_param(&mut self, index: u32, value: f32) -> bool {
        if !ipc::apply_wire_param(&mut self.params, index, value) {
            return false;
        }
        match ipc::affected(index) {
            ipc::Affects::Filter(pad) => {
                self.filters[pad] = SvfCoeffs::for_pad(&self.params.pads[pad], self.sample_rate);
            }
            ipc::Affects::MasterGain => {
                self.master_gain = db_to_linear(self.params.master_gain_db);
            }
            ipc::Affects::Nothing => {}
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

    /// Control-side handle used by the plugin host to submit a decoded sample
    /// for `pad_index`. Returns `None` for an out-of-range pad.
    pub fn pad_loader(&self, pad_index: usize) -> Option<PadLoader> {
        self.pads.get(pad_index).map(PadRuntime::loader)
    }

    pub fn all_notes_off(&mut self) {
        let sample_rate = self.sample_rate;
        for voice in &mut self.voices {
            if voice.active {
                begin_release(sample_rate, voice, CHOKE_FADE_MS);
            }
        }
    }

    /// Held peak of each pad's output, linear, for the editor's pad meters.
    pub fn pad_levels(&self) -> [f32; PADS] {
        self.pad_peak
    }

    pub fn meter_frame(&self) -> MeterFrame {
        MeterFrame {
            out_peak: self.out_peak,
            out_rms: self.out_mean_square.max(0.0).sqrt(),
            out_clip: self.out_clip,
        }
    }

    /// No lookahead: a trigger sounds on the sample it lands on.
    pub fn latency_samples(&self) -> usize {
        0
    }

    fn update_time_constants(&mut self) {
        self.meter_release = time_constant(self.sample_rate, METER_RELEASE_SEC);
        self.rms_coeff = time_constant(self.sample_rate, RMS_SEC);
    }

    fn refresh_all(&mut self) {
        for (pad, coeffs) in self.params.pads.iter().zip(self.filters.iter_mut()) {
            *coeffs = SvfCoeffs::for_pad(pad, self.sample_rate);
        }
        self.master_gain = db_to_linear(self.params.master_gain_db);
    }

    fn allocate_voice(&self) -> usize {
        self.voices
            .iter()
            .position(|voice| !voice.active)
            .unwrap_or_else(|| {
                self.voices
                    .iter()
                    .enumerate()
                    .min_by(|(_, left), (_, right)| {
                        left.env
                            .partial_cmp(&right.env)
                            .unwrap_or(std::cmp::Ordering::Equal)
                            .then_with(|| left.age.cmp(&right.age))
                    })
                    .map_or(0, |(index, _)| index)
            })
    }

    fn pad_index_for_note(&self, note: u8) -> Option<usize> {
        self.params.pads.iter().position(|pad| pad.note == note)
    }

    #[inline]
    fn read(buffer: &PadBuffer, frame: usize) -> (f32, f32) {
        let channels = buffer.channels.max(1);
        let base = frame * channels;
        if channels >= 2 {
            (buffer.samples[base], buffer.samples[base + 1])
        } else {
            let mono = buffer.samples[base];
            (mono, mono)
        }
    }

    /// One sample of `voice` before envelope and gain, with linear
    /// interpolation across the region. Ends the voice when the region runs
    /// out.
    #[inline]
    fn render_raw(buffer: &PadBuffer, voice: &mut Voice) -> (f32, f32) {
        let length = voice.last.saturating_sub(voice.first);
        let offset = voice.position.floor() as usize;
        if length < 2 || offset + 1 >= length {
            voice.active = false;
            return (0.0, 0.0);
        }
        let frac = (voice.position - voice.position.floor()) as f32;
        let (a, b) = if voice.reverse {
            (voice.last - 1 - offset, voice.last - 2 - offset)
        } else {
            (voice.first + offset, voice.first + offset + 1)
        };
        let (l0, r0) = Self::read(buffer, a);
        let (l1, r1) = Self::read(buffer, b);
        voice.position += voice.step;
        (l0 + (l1 - l0) * frac, r0 + (r1 - r0) * frac)
    }
}

impl Instrument for Dsp {
    fn reset(&mut self) {
        self.voices = [Voice::silent(); MAX_VOICES];
        for pad in &mut self.pads {
            pad.adopt_pending();
        }
        self.pad_peak = [0.0; PADS];
        self.out_peak = 0.0;
        self.out_mean_square = 0.0;
        self.out_clip = false;
    }

    fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        self.update_time_constants();
        self.refresh_all();
    }

    fn note_on(&mut self, note: u8, velocity: u8) {
        if velocity == 0 {
            self.note_off(note);
            return;
        }
        let Some(pad_index) = self.pad_index_for_note(note) else {
            return;
        };
        self.pads[pad_index].adopt_pending();
        let Some(buffer) = self.pads[pad_index].buffer.as_ref() else {
            return;
        };
        // Scalars only: `Pad` also carries `sample_name` (a `String`), which
        // must never be cloned on this thread.
        let pad = &self.params.pads[pad_index];
        let choke_group = pad.choke_group;
        let (first, last) = region_frames(pad, buffer.frames);
        let tune = f64::from(pad.tune_semitones + self.params.master_tune);
        let sensitivity = clamp(pad.velocity_sensitivity, 0.0, 100.0) / 100.0;
        let velocity = f32::from(velocity.min(127)) / 127.0;
        let level = db_to_linear(pad.gain_db) * (1.0 - sensitivity + sensitivity * velocity);
        let sample_rate = self.sample_rate;
        let attack_samples = (pad.attack_ms * 0.001 * sample_rate).max(1.0);
        let decay_coeff = if pad.decay_ms > 0.0 {
            // Reach −60 dB in `decay_ms`.
            (DECAY_FLOOR.ln() / (pad.decay_ms * 0.001 * sample_rate).max(1.0)).exp()
        } else {
            0.0
        };
        let hold_left = (pad.hold_ms.max(0.0) * 0.001 * sample_rate) as u32;
        let reverse = pad.reverse;
        let step =
            2.0f64.powf(tune / 12.0) * f64::from(buffer.sample_rate) / f64::from(sample_rate);

        // Choke: cut every other active voice sharing this pad's nonzero group.
        if choke_group != 0 {
            let pads = &self.params.pads;
            for voice in &mut self.voices {
                if voice.active
                    && voice.pad_index != pad_index
                    && pads[voice.pad_index].choke_group == choke_group
                {
                    begin_release(sample_rate, voice, CHOKE_FADE_MS);
                }
            }
        }

        self.age = self.age.wrapping_add(1);
        let index = self.allocate_voice();
        self.voices[index] = Voice {
            active: true,
            pad_index,
            position: 0.0,
            step,
            first,
            last,
            reverse,
            level,
            age: self.age,
            env: 0.0,
            stage: EnvStage::Attack,
            attack_step: 1.0 / attack_samples,
            hold_left,
            decay_coeff,
            release_per_sample: 0.0,
            filter_l: SvfState::default(),
            filter_r: SvfState::default(),
        };
    }

    fn note_off(&mut self, _note: u8) {
        // One-shots: releasing the MIDI key does not stop playback. Only a
        // choke (another pad in the same group), the decay reaching silence
        // or the region running out ends a voice.
    }

    fn process_stereo(&mut self) -> (f32, f32) {
        let any_solo = self.params.pads.iter().any(|pad| pad.solo);
        let release = self.meter_release;
        for peak in &mut self.pad_peak {
            *peak = flush_denormal(*peak * release);
        }
        let mut left = 0.0;
        let mut right = 0.0;
        for voice in &mut self.voices {
            if !voice.active {
                continue;
            }
            let pad_index = voice.pad_index;
            let Some(buffer) = self.pads[pad_index].buffer.as_ref() else {
                voice.active = false;
                continue;
            };
            let env = voice.envelope();
            if !voice.active {
                continue;
            }
            let (raw_l, raw_r) = Self::render_raw(buffer, voice);
            let pad = &self.params.pads[pad_index];
            let audible = if any_solo { pad.solo } else { !pad.muted };
            if !audible {
                continue;
            }
            let filter = &self.filters[pad_index];
            let (fl, fr) = if filter.mode == FilterMode::Off {
                (raw_l, raw_r)
            } else {
                (
                    voice.filter_l.run(filter, raw_l),
                    voice.filter_r.run(filter, raw_r),
                )
            };
            let pan = clamp(pad.pan, -1.0, 1.0);
            let gain = voice.level * env;
            let l = fl * gain * (0.5 * (1.0 - pan)).sqrt();
            let r = fr * gain * (0.5 * (1.0 + pan)).sqrt();
            let magnitude = l.abs().max(r.abs());
            if magnitude > self.pad_peak[pad_index] {
                self.pad_peak[pad_index] = magnitude;
            }
            left += l;
            right += r;
        }
        let out_l = soft_clip(left * self.master_gain);
        let out_r = soft_clip(right * self.master_gain);

        let peak = out_l.abs().max(out_r.abs());
        self.out_peak = if peak > self.out_peak {
            peak
        } else {
            flush_denormal(self.out_peak * release)
        };
        let square = 0.5 * (out_l * out_l + out_r * out_r);
        self.out_mean_square =
            flush_denormal(self.rms_coeff * self.out_mean_square + (1.0 - self.rms_coeff) * square);
        // The soft clip never reaches full scale; what it reports is the sum
        // hitting it hard enough to squash audibly.
        self.out_clip |= left.abs().max(right.abs()) * self.master_gain >= 1.0;
        (out_l, out_r)
    }
}

#[inline]
fn soft_clip(sample: f32) -> f32 {
    sample / (1.0 + sample.abs())
}

#[inline]
fn begin_release(sample_rate: f32, voice: &mut Voice, release_ms: f32) {
    voice.stage = EnvStage::Release;
    voice.release_per_sample = voice.env / (release_ms * 0.001 * sample_rate).max(1.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f32 = 48_000.0;

    fn buffer_with_tone(frames: usize, sample_rate: f32) -> PadBuffer {
        let samples: Vec<f32> = (0..frames)
            .map(|i| (i as f32 / frames as f32 * std::f32::consts::TAU * 4.0).sin() * 0.5)
            .collect();
        PadBuffer {
            samples: samples.into_boxed_slice(),
            channels: 1,
            frames,
            sample_rate,
        }
    }

    /// A buffer whose sample value is its own frame index / frames, so the
    /// output shows exactly which part of the region is playing.
    fn ramp(frames: usize) -> PadBuffer {
        PadBuffer {
            samples: (0..frames)
                .map(|i| i as f32 / frames as f32)
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            channels: 1,
            frames,
            sample_rate: RATE,
        }
    }

    fn load(dsp: &mut Dsp, pad: usize, buffer: PadBuffer) {
        dsp.pad_loader(pad).unwrap().submit(Box::new(buffer));
    }

    fn edit(dsp: &mut Dsp, f: impl FnOnce(&mut Params)) {
        let mut params = dsp.params().clone();
        f(&mut params);
        dsp.set_params(params);
    }

    fn render(dsp: &mut Dsp, samples: usize) -> Vec<(f32, f32)> {
        (0..samples).map(|_| dsp.process_stereo()).collect()
    }

    fn peak(frames: &[(f32, f32)]) -> f32 {
        frames
            .iter()
            .fold(0.0f32, |peak, (l, r)| peak.max(l.abs()).max(r.abs()))
    }

    #[test]
    fn silent_pad_produces_no_sound() {
        let mut dsp = Dsp::new(RATE);
        dsp.note_on(36, 110);
        let (l, r) = dsp.process_stereo();
        assert_eq!((l, r), (0.0, 0.0));
    }

    #[test]
    fn loaded_pad_plays_and_finishes() {
        let mut dsp = Dsp::new(RATE);
        load(&mut dsp, 0, buffer_with_tone(4_800, RATE));
        dsp.note_on(36, 127);
        let out = render(&mut dsp, 10_000);
        assert!(out.iter().all(|(l, r)| l.is_finite() && r.is_finite()));
        assert!(peak(&out) > 0.01);
        assert_eq!(dsp.process_stereo(), (0.0, 0.0));
    }

    #[test]
    fn choke_group_cuts_other_pad() {
        let mut dsp = Dsp::new(RATE);
        load(&mut dsp, 0, buffer_with_tone(48_000, RATE));
        load(&mut dsp, 1, buffer_with_tone(48_000, RATE));
        edit(&mut dsp, |params| {
            params.pads[0].choke_group = 1;
            params.pads[1].choke_group = 1;
        });
        dsp.note_on(36, 127);
        render(&mut dsp, 100);
        dsp.note_on(37, 127);
        // Voice 0 should now be releasing (choked), not steady-state active.
        assert!(dsp.voices[0].active);
        assert_eq!(dsp.voices[0].stage, EnvStage::Release);
    }

    #[test]
    fn voice_pool_is_bounded() {
        let mut dsp = Dsp::new(RATE);
        for pad_index in 0..PADS {
            load(&mut dsp, pad_index, buffer_with_tone(48_000, RATE));
        }
        for _ in 0..(MAX_VOICES + 20) {
            dsp.note_on(36, 100);
        }
        assert_eq!(
            dsp.voices.iter().filter(|voice| voice.active).count(),
            MAX_VOICES
        );
    }

    #[test]
    fn the_region_trims_where_playback_starts_and_stops() {
        let mut dsp = Dsp::new(RATE);
        load(&mut dsp, 0, ramp(10_000));
        edit(&mut dsp, |params| {
            params.pads[0].start = 0.5;
            params.pads[0].end = 0.6;
            params.pads[0].attack_ms = 0.0;
        });
        dsp.note_on(36, 127);
        let out = render(&mut dsp, 1_200);
        // The ramp's value is its position: the first sample is at 0.5 (through
        // the centre pan law and the output soft clip), and nothing past 0.6
        // ever plays.
        let expected = soft_clip(0.5 * std::f32::consts::FRAC_1_SQRT_2);
        assert!(
            (out[0].0 - expected).abs() < 0.005,
            "{} vs {expected}",
            out[0].0
        );
        let playing: Vec<_> = out.iter().filter(|(l, _)| *l != 0.0).collect();
        assert!(playing.len() <= 1_001, "{} samples played", playing.len());
        assert!(playing.len() >= 990);
        assert_eq!(out[1_100], (0.0, 0.0));
    }

    #[test]
    fn reverse_plays_the_region_backwards() {
        let mut dsp = Dsp::new(RATE);
        load(&mut dsp, 0, ramp(1_000));
        edit(&mut dsp, |params| {
            params.pads[0].reverse = true;
            params.pads[0].attack_ms = 0.0;
            params.pads[0].pan = 0.0;
        });
        dsp.note_on(36, 127);
        let out = render(&mut dsp, 900);
        // Descending: a later sample is quieter than an earlier one.
        assert!(out[50].0 > out[800].0);
    }

    #[test]
    fn region_frames_never_inverts_or_empties() {
        let mut pad = default_pad(0);
        pad.start = 0.8;
        pad.end = 0.2;
        assert_eq!(region_frames(&pad, 1_000), (200, 800));
        pad.start = 0.5;
        pad.end = 0.5;
        let (first, last) = region_frames(&pad, 100_000);
        assert!(last > first);
        pad.start = 1.0;
        pad.end = 1.0;
        let (first, last) = region_frames(&pad, 10);
        assert!(first < last && last <= 10);
    }

    #[test]
    fn decay_ends_the_voice_on_time() {
        let mut dsp = Dsp::new(RATE);
        load(&mut dsp, 0, buffer_with_tone(96_000, RATE));
        edit(&mut dsp, |params| {
            params.pads[0].attack_ms = 0.0;
            params.pads[0].hold_ms = 10.0;
            params.pads[0].decay_ms = 100.0;
        });
        dsp.note_on(36, 127);
        // 10 ms hold + 100 ms decay ≈ 5 280 samples; well before the sample's
        // own 2 s end.
        render(&mut dsp, 5_400);
        assert!(dsp.voices.iter().all(|voice| !voice.active));
    }

    #[test]
    fn no_decay_plays_the_region_out_at_full_level() {
        let mut dsp = Dsp::new(RATE);
        load(&mut dsp, 0, buffer_with_tone(24_000, RATE));
        dsp.note_on(36, 127);
        render(&mut dsp, 20_000);
        assert!(dsp.voices[0].active, "still inside the region");
        assert_eq!(dsp.voices[0].env, 1.0);
    }

    #[test]
    fn velocity_sensitivity_scales_the_hit() {
        let hit = |sensitivity: f32, velocity: u8| {
            let mut dsp = Dsp::new(RATE);
            load(&mut dsp, 0, buffer_with_tone(4_800, RATE));
            edit(&mut dsp, |params| {
                params.pads[0].velocity_sensitivity = sensitivity;
            });
            dsp.note_on(36, velocity);
            peak(&render(&mut dsp, 4_000))
        };
        let full_soft = hit(100.0, 32);
        let full_hard = hit(100.0, 127);
        assert!(full_soft < full_hard * 0.5);
        let flat_soft = hit(0.0, 32);
        let flat_hard = hit(0.0, 127);
        assert!((flat_soft - flat_hard).abs() < 1e-4);
    }

    #[test]
    fn low_pass_filter_dulls_a_bright_pad() {
        let bright = || {
            let frames = 24_000;
            PadBuffer {
                samples: (0..frames)
                    .map(|i| if i % 2 == 0 { 0.5 } else { -0.5 })
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
                channels: 1,
                frames,
                sample_rate: RATE,
            }
        };
        let mut open = Dsp::new(RATE);
        load(&mut open, 0, bright());
        open.note_on(36, 127);
        let open_peak = peak(&render(&mut open, 4_000)[2_000..]);

        let mut closed = Dsp::new(RATE);
        load(&mut closed, 0, bright());
        assert!(closed.apply_ui_param("pad0FilterMode", FilterMode::LowPass.to_wire()));
        assert!(closed.apply_ui_param("pad0Cutoff", 500.0));
        closed.note_on(36, 127);
        let closed_peak = peak(&render(&mut closed, 4_000)[2_000..]);
        assert!(
            closed_peak < open_peak * 0.05,
            "{closed_peak} vs {open_peak}"
        );
    }

    #[test]
    fn filters_stay_finite_at_every_extreme() {
        let mut dsp = Dsp::new(RATE);
        load(&mut dsp, 0, buffer_with_tone(24_000, RATE));
        for mode in FilterMode::ALL {
            for (cutoff, resonance) in [(20.0, 100.0), (20_000.0, 100.0), (1_000.0, 0.0)] {
                edit(&mut dsp, |params| {
                    params.pads[0].filter_mode = mode;
                    params.pads[0].cutoff_hz = cutoff;
                    params.pads[0].resonance = resonance;
                });
                dsp.reset();
                dsp.note_on(36, 127);
                let out = render(&mut dsp, 2_000);
                assert!(out.iter().all(|(l, r)| l.is_finite() && r.is_finite()));
            }
        }
    }

    #[test]
    fn master_gain_and_tune_apply_to_the_whole_kit() {
        let mut dsp = Dsp::new(RATE);
        load(&mut dsp, 0, buffer_with_tone(4_800, RATE));
        dsp.note_on(36, 127);
        let loud = peak(&render(&mut dsp, 4_000));

        let mut quiet = Dsp::new(RATE);
        load(&mut quiet, 0, buffer_with_tone(4_800, RATE));
        assert!(quiet.apply_ui_param("masterGain", -20.0));
        quiet.note_on(36, 127);
        assert!(peak(&render(&mut quiet, 4_000)) < loud * 0.2);

        let mut high = Dsp::new(RATE);
        load(&mut high, 0, buffer_with_tone(4_800, RATE));
        assert!(high.apply_ui_param("masterTune", 12.0));
        high.note_on(36, 127);
        // An octave up plays the 4 800-frame region in 2 400 samples.
        render(&mut high, 2_500);
        assert!(high.voices.iter().all(|voice| !voice.active));
    }

    #[test]
    fn pad_meters_follow_the_pad_that_played() {
        let mut dsp = Dsp::new(RATE);
        load(&mut dsp, 3, buffer_with_tone(4_800, RATE));
        dsp.note_on(39, 127);
        render(&mut dsp, 1_000);
        let levels = dsp.pad_levels();
        assert!(levels[3] > 0.05);
        assert!(levels.iter().enumerate().all(|(i, l)| i == 3 || *l == 0.0));
        assert!(dsp.meter_frame().out_peak > 0.0);
        // …and fall back once it stops.
        render(&mut dsp, 96_000);
        assert!(dsp.pad_levels()[3] < 0.001);
    }

    #[test]
    fn waveform_peaks_cover_the_whole_sample() {
        let mut samples = vec![0.0f32; 1_000];
        samples[999] = -0.5;
        samples[0] = 1.0;
        let peaks = waveform_peaks(&samples, 1, 1_000, 10);
        assert_eq!(peaks.len(), 10);
        assert_eq!(peaks[0], 255);
        assert_eq!(peaks[9], 128);
        assert!(peaks[1..9].iter().all(|p| *p == 0));
        // Stereo: either channel counts.
        let stereo = [0.0, 0.25, 0.0, 0.0];
        assert_eq!(waveform_peaks(&stereo, 2, 2, 2), vec![64, 0]);
        assert_eq!(waveform_peaks(&[], 1, 0, 4), vec![0; 4]);
    }

    /// A project saved before the region, filter, AHD and master existed
    /// opens with each at the behaviour pads had then.
    #[test]
    fn a_first_release_state_still_loads_and_sounds_the_same() {
        let legacy_pad = r#"{"note":36,"tuneSemitones":0.0,"gainDb":0.0,"pan":0.0,
            "chokeGroup":0,"attackMs":1.0,"releaseMs":60.0,"reverse":false,
            "muted":false,"solo":false,"sampleName":"kick.wav"}"#;
        let pads = vec![legacy_pad; PADS].join(",");
        let json = format!(r#"{{"version":1,"params":{{"pads":[{pads}]}}}}"#);
        let state = ipc::DrumSamplerState::from_json(&json).expect("legacy state decodes");
        let pad = &state.params.pads[0];
        assert_eq!(pad.sample_name.as_deref(), Some("kick.wav"));
        assert_eq!((pad.start, pad.end), (0.0, 1.0));
        assert_eq!(pad.filter_mode, FilterMode::Off);
        assert_eq!(pad.velocity_sensitivity, 100.0);
        assert_eq!(pad.decay_ms, 0.0);
        assert_eq!(state.params.master_gain_db, 0.0);
    }
}

//! WrapSynth — a realtime-safe polyphonic wavetable synthesizer.
//!
//! Per voice: two band-limited, mipmapped wavetable oscillators
//! ([`wavetable`]) each with up to seven unison voices ([`unison`]), a sub
//! sine and noise; an optional drive; one zero-delay-feedback filter
//! ([`filter`]: state-variable, ladder or formant); three analog-style
//! envelopes ([`envelope`]: amp, filter, modulation); two LFOs ([`lfo`]); and
//! an eight-slot modulation matrix ([`modulation`]). After the voices: a DC
//! blocker, the effects ([`fx`]: drive, chorus, delay, reverb), the master
//! level and a transparent ceiling.
//!
//! Notes are played poly, mono or legato with glide; pitch bend, the mod
//! wheel, aftertouch and the sustain pedal are followed. A retriggered note
//! continues from where its envelopes are, and a stolen voice fades out over
//! a few milliseconds before the new note starts, so voice management never
//! clicks. Continuous controls are smoothed, so knob moves and automation do
//! not zipper.
//!
//! MIDI arrives with sample offsets ([`Dsp::queue_midi`]) and is applied at
//! that sample of the next [`Dsp::process_block`]. Parameters arrive as
//! compact wire edits ([`Dsp::apply_wire_param`]). The audio path uses fixed
//! voice, unison, queue and table storage and performs no heap allocation,
//! locking, logging or string lookup.

pub mod envelope;
pub mod filter;
pub mod fx;
pub mod ipc;
pub mod lfo;
pub mod modulation;
pub mod presets;
pub mod ui;
pub mod unison;
pub mod wavetable;

use builtin_dsp_core::{Instrument, ParamDescriptor, PluginCategory, PluginDescriptor};
use serde::{Deserialize, Serialize};

use envelope::{EnvCoefficients, EnvTimes, Envelope};
use filter::{Coefficients, FilterState};
use fx::FxRack;
use modulation::{ActiveRoutes, PITCH_MOD_SEMITONES, SourceValues, apply_routes};
use unison::{ANALOG_MAX_CENTS, Drift, UnisonLayout, UnisonOscillator, cents_ratio_fast};
use wavetable::{Bank, mip_for};

pub use filter::{FilterModel, FilterType};
pub use ipc::{UI_PARAM_IDS, ui_param_id, ui_param_index};
pub use lfo::LfoShape;
pub use modulation::{MOD_SLOTS, ModDest, ModSlot, ModSource};
pub use presets::{FactoryPreset, factory_presets};
pub use wavetable::Wavetable;

/// The oscillator shape type under its first name. Its first four tables are
/// the original four shapes, under the names saved projects use.
pub type Waveform = Wavetable;

pub const PLUGIN_ID: &str = "futureboard.wrapsynth";
pub const MAX_VOICES: usize = 16;
pub const MAX_UNISON: usize = 7;
/// MIDI events one block can carry; more are dropped and counted.
pub const MIDI_QUEUE: usize = 512;

/// Lowest and highest cutoff the knob reaches.
pub const CUTOFF_MIN_HZ: f32 = 40.0;
pub const CUTOFF_MAX_HZ: f32 = 20_000.0;
/// The cutoff knob's span in octaves: a modulation of 1 sweeps all of it.
pub const CUTOFF_OCTAVES: f32 = 8.965_784;

/// One voice at full velocity with both envelopes open, before the master.
const VOICE_GAIN: f32 = 0.42;
/// How long a stolen voice takes to fade before the new note starts.
const STEAL_FADE_MS: f32 = 3.0;
/// Time constant of the smoothing on continuous controls.
const SMOOTH_MS: f32 = 12.0;
/// How long switching the synth off takes to fade.
const POWER_FADE_MS: f32 = 10.0;
/// Held keys a mono patch remembers, for returning to an earlier key.
const NOTE_STACK: usize = 16;
/// Samples between filter coefficient updates: 0.17 ms at 48 kHz, far
/// below a step anyone hears, and an eighth of the `tan` the filter costs.
const FILTER_CONTROL_INTERVAL: u8 = 8;
/// The output ceiling: transparent below, rounded into full scale above.
const CEILING_KNEE: f32 = 0.95;

/// How notes are given voices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum VoiceMode {
    #[default]
    Poly,
    /// One voice; every new key restarts the envelopes.
    Mono,
    /// One voice; overlapping keys glide without restarting the envelopes.
    Legato,
}

impl VoiceMode {
    pub const ALL: [VoiceMode; 3] = [Self::Poly, Self::Mono, Self::Legato];

    pub fn to_wire(self) -> f32 {
        self as usize as f32
    }

    pub fn from_wire(value: f32) -> Self {
        Self::ALL[value.round().clamp(0.0, (Self::ALL.len() - 1) as f32) as usize]
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Poly => "Poly",
            Self::Mono => "Mono",
            Self::Legato => "Legato",
        }
    }
}

/// A WrapSynth patch.
///
/// The first 22 fields are the original WrapSynth's, with their saved names.
/// A patch saved before the rest existed fills them from [`legacy_params`]:
/// the values that keep it the patch it was (a 12 dB low-pass, no filter
/// envelope, no modulation, no effects).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default = "legacy_params")]
pub struct Params {
    pub power: bool,
    pub osc_a_wave: Wavetable,
    pub osc_a_position: f32,
    pub osc_a_level: f32,
    pub osc_b_wave: Wavetable,
    pub osc_b_position: f32,
    pub osc_b_level: f32,
    pub osc_b_semitones: f32,
    pub osc_b_detune_cents: f32,
    pub unison: u8,
    pub unison_detune_cents: f32,
    pub stereo_width: f32,
    pub sub_level: f32,
    pub noise_level: f32,
    pub cutoff_hz: f32,
    pub resonance: f32,
    pub filter_drive: f32,
    pub attack_ms: f32,
    pub decay_ms: f32,
    pub sustain: f32,
    pub release_ms: f32,
    pub master_db: f32,

    // ── Filter ──
    pub filter_type: FilterType,
    /// 0..1: how far the cutoff follows the key (1 = an octave per octave).
    pub filter_key_track: f32,
    /// −1..1 of the cutoff knob the filter envelope moves it.
    pub filter_env_amount: f32,
    pub filter_attack_ms: f32,
    pub filter_decay_ms: f32,
    pub filter_sustain: f32,
    pub filter_release_ms: f32,

    // ── Modulation envelope and LFOs ──
    pub mod_attack_ms: f32,
    pub mod_decay_ms: f32,
    pub mod_sustain: f32,
    pub mod_release_ms: f32,
    pub lfo1_shape: LfoShape,
    pub lfo1_rate_hz: f32,
    pub lfo1_sync: bool,
    /// Index into [`echospace::DIVISION_BEATS`].
    pub lfo1_division: u8,
    pub lfo1_retrigger: bool,
    pub lfo2_shape: LfoShape,
    pub lfo2_rate_hz: f32,
    pub lfo2_sync: bool,
    pub lfo2_division: u8,
    pub lfo2_retrigger: bool,
    pub mod_slots: [ModSlot; MOD_SLOTS],

    // ── Voicing ──
    pub voice_mode: VoiceMode,
    pub glide_ms: f32,
    pub pitch_bend_range: f32,
    /// 0..1: how much velocity sets the level (0 = every note full).
    pub velocity_amount: f32,
    /// 0..1: the outer unison voices against the centre one.
    pub unison_blend: f32,
    /// 0..1: slow per-voice pitch and level drift.
    pub analog: f32,
    /// 0..1: how much of each unison voice's start phase is random.
    pub phase_random: f32,

    // ── Effects ──
    pub dist_on: bool,
    pub dist_drive: f32,
    pub dist_mix: f32,
    pub chorus_on: bool,
    pub chorus_rate_hz: f32,
    pub chorus_depth: f32,
    pub chorus_mix: f32,
    pub delay_on: bool,
    pub delay_sync: bool,
    pub delay_division: u8,
    pub delay_time_ms: f32,
    pub delay_feedback: f32,
    pub delay_mix: f32,
    pub delay_ping_pong: bool,
    pub reverb_on: bool,
    pub reverb_size: f32,
    pub reverb_decay_sec: f32,
    pub reverb_damping: f32,
    pub reverb_mix: f32,
}

/// The note division the delay and the LFOs lock to by default: an eighth.
fn default_division() -> u8 {
    echospace::default_params().division_l
}

/// What a patch saved by the first WrapSynth means: its 22 controls at their
/// defaults, everything since off or neutral.
pub fn legacy_params() -> Params {
    Params {
        power: true,
        osc_a_wave: Wavetable::Saw,
        osc_a_position: 0.18,
        osc_a_level: 0.78,
        osc_b_wave: Wavetable::Square,
        osc_b_position: 0.42,
        osc_b_level: 0.38,
        osc_b_semitones: 0.0,
        osc_b_detune_cents: 7.0,
        unison: 3,
        unison_detune_cents: 14.0,
        stereo_width: 0.72,
        sub_level: 0.16,
        noise_level: 0.025,
        cutoff_hz: 6_400.0,
        resonance: 0.18,
        filter_drive: 0.12,
        attack_ms: 8.0,
        decay_ms: 220.0,
        sustain: 0.72,
        release_ms: 420.0,
        master_db: -8.0,

        filter_type: FilterType::LowPass12,
        filter_key_track: 0.0,
        filter_env_amount: 0.0,
        filter_attack_ms: 2.0,
        filter_decay_ms: 400.0,
        filter_sustain: 0.3,
        filter_release_ms: 400.0,

        mod_attack_ms: 2.0,
        mod_decay_ms: 600.0,
        mod_sustain: 0.0,
        mod_release_ms: 400.0,
        lfo1_shape: LfoShape::Sine,
        lfo1_rate_hz: 2.0,
        lfo1_sync: false,
        lfo1_division: default_division(),
        lfo1_retrigger: true,
        lfo2_shape: LfoShape::Triangle,
        lfo2_rate_hz: 0.3,
        lfo2_sync: false,
        lfo2_division: default_division(),
        lfo2_retrigger: false,
        mod_slots: [ModSlot::EMPTY; MOD_SLOTS],

        voice_mode: VoiceMode::Poly,
        glide_ms: 0.0,
        pitch_bend_range: 2.0,
        velocity_amount: 1.0,
        unison_blend: 0.5,
        analog: 0.0,
        phase_random: 0.0,

        dist_on: false,
        dist_drive: 0.3,
        dist_mix: 1.0,
        chorus_on: false,
        chorus_rate_hz: 0.6,
        chorus_depth: 0.5,
        chorus_mix: 0.4,
        delay_on: false,
        delay_sync: true,
        delay_division: default_division(),
        delay_time_ms: 320.0,
        delay_feedback: 0.35,
        delay_mix: 0.25,
        delay_ping_pong: true,
        reverb_on: false,
        reverb_size: 0.6,
        reverb_decay_sec: 2.4,
        reverb_damping: 0.45,
        reverb_mix: 0.22,
    }
}

/// A new WrapSynth's patch: a clean, open two-saw sound with a little unison
/// and a filter that follows the key and opens with the note.
pub fn default_params() -> Params {
    Params {
        osc_a_wave: Wavetable::ClassicSaw,
        osc_a_position: 0.0,
        osc_a_level: 0.8,
        osc_b_wave: Wavetable::ClassicSaw,
        osc_b_position: 0.0,
        osc_b_level: 0.45,
        osc_b_semitones: 0.0,
        osc_b_detune_cents: 9.0,
        unison: 3,
        unison_detune_cents: 12.0,
        stereo_width: 0.75,
        sub_level: 0.0,
        noise_level: 0.0,
        cutoff_hz: 5_200.0,
        resonance: 0.15,
        filter_drive: 0.0,
        attack_ms: 4.0,
        decay_ms: 400.0,
        sustain: 0.85,
        release_ms: 260.0,
        master_db: -6.0,
        filter_type: FilterType::LowPass24,
        filter_key_track: 0.5,
        filter_env_amount: 0.18,
        filter_attack_ms: 1.0,
        filter_decay_ms: 520.0,
        filter_sustain: 0.25,
        filter_release_ms: 300.0,
        velocity_amount: 0.7,
        analog: 0.12,
        phase_random: 1.0,
        ..legacy_params()
    }
}

pub fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PLUGIN_ID,
        name: "WrapSynth",
        vendor: "Futureboard",
        category: PluginCategory::Instrument,
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
                id: "oscAPosition",
                name: "Osc A Position",
                default_value: 0.0,
                min: 0.0,
                max: 1.0,
                unit: "",
            },
            ParamDescriptor {
                id: "oscALevel",
                name: "Osc A Level",
                default_value: 0.8,
                min: 0.0,
                max: 1.0,
                unit: "",
            },
            ParamDescriptor {
                id: "oscBPosition",
                name: "Osc B Position",
                default_value: 0.0,
                min: 0.0,
                max: 1.0,
                unit: "",
            },
            ParamDescriptor {
                id: "oscBLevel",
                name: "Osc B Level",
                default_value: 0.45,
                min: 0.0,
                max: 1.0,
                unit: "",
            },
            ParamDescriptor {
                id: "cutoffHz",
                name: "Filter Cutoff",
                default_value: 5_200.0,
                min: CUTOFF_MIN_HZ,
                max: CUTOFF_MAX_HZ,
                unit: "Hz",
            },
            ParamDescriptor {
                id: "resonance",
                name: "Resonance",
                default_value: 0.15,
                min: 0.0,
                max: 0.95,
                unit: "",
            },
            ParamDescriptor {
                id: "attackMs",
                name: "Attack",
                default_value: 4.0,
                min: 0.5,
                max: 5_000.0,
                unit: "ms",
            },
            ParamDescriptor {
                id: "releaseMs",
                name: "Release",
                default_value: 260.0,
                min: 5.0,
                max: 8_000.0,
                unit: "ms",
            },
            ParamDescriptor {
                id: "masterDb",
                name: "Master",
                default_value: -6.0,
                min: -24.0,
                max: 3.0,
                unit: "dB",
            },
        ],
    }
}

/// One cycle of `table` at `position`, from its 256-harmonic level — what
/// a note around middle C plays, with the band-limit's ripple too narrow
/// to see at the editor's size: what the editor paints as the oscillator.
pub fn osc_cycle(table: Wavetable, position: f32, phase: f32) -> f32 {
    let level = (0..wavetable::LEVELS)
        .find(|&level| wavetable::level_harmonics(level) <= 256)
        .unwrap_or(0);
    wavetable::bank().read(table, level, position, phase.clamp(0.0, 0.999_999))
}

// ── Voices ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
struct PendingNote {
    note: u8,
    velocity: f32,
    start_pitch: f32,
    /// The key went up before the note could start.
    released: bool,
}

#[derive(Debug, Clone, Copy)]
struct Voice {
    active: bool,
    note: u8,
    velocity: f32,
    age: u64,
    /// The key is down (or held by the pedal).
    gate: bool,
    /// Released while the sustain pedal was down: it ends when the pedal does.
    sustained: bool,
    pitch: f32,
    target_pitch: f32,
    amp: Envelope,
    filter_env: Envelope,
    mod_env: Envelope,
    lfo_phase: [f32; 2],
    lfo_held: [f32; 2],
    /// This note's random modulation value, −1..1.
    random: f32,
    osc_a: [UnisonOscillator; MAX_UNISON],
    osc_b: [UnisonOscillator; MAX_UNISON],
    sub_phase: f32,
    filter_l: FilterState,
    filter_r: FilterState,
    coefficients: Coefficients,
    /// Samples until the coefficients are worked out again.
    coefficient_countdown: u8,
    rng: u32,
    drift_countdown: u32,
    /// 1 while sounding; ramps to 0 while the voice is stolen.
    fade: f32,
    fading: bool,
    pending: Option<PendingNote>,
}

impl Voice {
    const SILENT: Self = Self {
        active: false,
        note: 0,
        velocity: 0.0,
        age: 0,
        gate: false,
        sustained: false,
        pitch: 60.0,
        target_pitch: 60.0,
        amp: Envelope::IDLE,
        filter_env: Envelope::IDLE,
        mod_env: Envelope::IDLE,
        lfo_phase: [0.0; 2],
        lfo_held: [0.0; 2],
        random: 0.0,
        osc_a: [UnisonOscillator::ZERO; MAX_UNISON],
        osc_b: [UnisonOscillator::ZERO; MAX_UNISON],
        sub_phase: 0.0,
        filter_l: FilterState::ZERO,
        filter_r: FilterState::ZERO,
        coefficients: Coefficients::NEUTRAL,
        coefficient_countdown: 0,
        rng: 0x1234_5678,
        drift_countdown: 0,
        fade: 1.0,
        fading: false,
        pending: None,
    };

    #[inline]
    fn uniform(&mut self) -> f32 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 17;
        self.rng ^= self.rng << 5;
        self.rng as f32 / u32::MAX as f32
    }

    #[inline]
    fn bipolar(&mut self) -> f32 {
        self.uniform() * 2.0 - 1.0
    }

    fn gate_on(&mut self) {
        self.gate = true;
        self.sustained = false;
        self.amp.gate_on();
        self.filter_env.gate_on();
        self.mod_env.gate_on();
    }

    fn gate_off(&mut self) {
        self.gate = false;
        self.sustained = false;
        self.amp.gate_off();
        self.filter_env.gate_off();
        self.mod_env.gate_off();
    }

    /// The voice could be taken by a new note without cutting a held key.
    fn is_releasing(&self) -> bool {
        self.active && !self.gate && !self.fading
    }
}

/// What the voices read every sample, gathered so a voice can be rendered
/// while the synth is borrowed elsewhere.
struct Shared<'a> {
    p: &'a Params,
    d: &'a Derived,
    s: SmoothedValues,
    bank: &'a Bank,
    sample_rate: f32,
    bend_semitones: f32,
    mod_wheel: f32,
    aftertouch: f32,
}

/// Everything a patch edit, a tempo change or a new sample rate decides,
/// worked out once off the per-sample path.
#[derive(Debug, Clone, Copy)]
struct Derived {
    amp_env: EnvCoefficients,
    filter_env: EnvCoefficients,
    mod_env: EnvCoefficients,
    layout: UnisonLayout,
    /// Equal-power (left, right) gains per unison voice.
    pans: [(f32, f32); MAX_UNISON],
    routes: ActiveRoutes,
    glide_coef: f32,
    lfo_increment: [f32; 2],
    drift: Drift,
    smoothing: f32,
    fade_step: f32,
    power_step: f32,
    master_gain: f32,
}

impl Derived {
    fn new(p: &Params, sample_rate: f32, tempo_bpm: f32) -> Self {
        let layout = UnisonLayout::new(p.unison, 0.07, p.unison_blend);
        let mut pans = [(0.0, 0.0); MAX_UNISON];
        for (i, pan) in pans.iter_mut().enumerate().take(layout.count) {
            let position = (layout.pan[i] * p.stereo_width).clamp(-1.0, 1.0);
            *pan = (
                (0.5 * (1.0 - position)).sqrt(),
                (0.5 * (1.0 + position)).sqrt(),
            );
        }
        let lfo = |rate: f32, sync: bool, division: u8| {
            let beats = echospace::DIVISION_BEATS
                .get(usize::from(division))
                .copied()
                .unwrap_or(0.5);
            lfo::rate_hz(rate, sync, beats, tempo_bpm) / sample_rate
        };
        let per_sample = |ms: f32| ms * 0.001 * sample_rate;
        Self {
            amp_env: EnvCoefficients::new(
                EnvTimes {
                    attack_ms: p.attack_ms,
                    decay_ms: p.decay_ms,
                    sustain: p.sustain,
                    release_ms: p.release_ms,
                },
                sample_rate,
            ),
            filter_env: EnvCoefficients::new(
                EnvTimes {
                    attack_ms: p.filter_attack_ms,
                    decay_ms: p.filter_decay_ms,
                    sustain: p.filter_sustain,
                    release_ms: p.filter_release_ms,
                },
                sample_rate,
            ),
            mod_env: EnvCoefficients::new(
                EnvTimes {
                    attack_ms: p.mod_attack_ms,
                    decay_ms: p.mod_decay_ms,
                    sustain: p.mod_sustain,
                    release_ms: p.mod_release_ms,
                },
                sample_rate,
            ),
            layout,
            pans,
            routes: ActiveRoutes::new(&p.mod_slots),
            // The glide time is how long a jump takes to arrive (to within
            // 1 %), not a time constant five times shorter than that.
            glide_coef: if p.glide_ms > 0.0 {
                (-(100.0f32).ln() / per_sample(p.glide_ms).max(1.0)).exp()
            } else {
                0.0
            },
            lfo_increment: [
                lfo(p.lfo1_rate_hz, p.lfo1_sync, p.lfo1_division),
                lfo(p.lfo2_rate_hz, p.lfo2_sync, p.lfo2_division),
            ],
            drift: Drift::new(sample_rate),
            smoothing: 1.0 - (-1.0 / per_sample(SMOOTH_MS).max(1.0)).exp(),
            fade_step: 1.0 / per_sample(STEAL_FADE_MS).max(1.0),
            power_step: 1.0 / per_sample(POWER_FADE_MS).max(1.0),
            master_gain: builtin_dsp_core::db_to_linear(p.master_db),
        }
    }
}

/// One smoothed control: a one-pole glide toward where the patch puts it.
#[derive(Debug, Clone, Copy)]
struct Smooth {
    value: f32,
    target: f32,
}

impl Smooth {
    fn at(value: f32) -> Self {
        Self {
            value,
            target: value,
        }
    }

    #[inline]
    fn step(&mut self, coef: f32) -> f32 {
        let delta = self.target - self.value;
        if delta.abs() < 1.0e-6 {
            self.value = self.target;
        } else {
            self.value += delta * coef;
        }
        self.value
    }
}

/// The smoothed controls' values at one sample.
#[derive(Debug, Clone, Copy)]
struct SmoothedValues {
    osc_a_position: f32,
    osc_a_level: f32,
    osc_b_position: f32,
    osc_b_level: f32,
    sub_level: f32,
    noise_level: f32,
    cutoff_octaves: f32,
    resonance: f32,
    drive: f32,
    detune_cents: f32,
    master_gain: f32,
}

#[derive(Debug, Clone, Copy)]
struct Smoothers {
    osc_a_position: Smooth,
    osc_a_level: Smooth,
    osc_b_position: Smooth,
    osc_b_level: Smooth,
    sub_level: Smooth,
    noise_level: Smooth,
    /// log2 of the cutoff in Hz: a sweep is even in pitch.
    cutoff_octaves: Smooth,
    resonance: Smooth,
    drive: Smooth,
    detune_cents: Smooth,
    master_gain: Smooth,
}

impl Smoothers {
    fn at(p: &Params, d: &Derived) -> Self {
        Self {
            osc_a_position: Smooth::at(p.osc_a_position),
            osc_a_level: Smooth::at(p.osc_a_level),
            osc_b_position: Smooth::at(p.osc_b_position),
            osc_b_level: Smooth::at(p.osc_b_level),
            sub_level: Smooth::at(p.sub_level),
            noise_level: Smooth::at(p.noise_level),
            cutoff_octaves: Smooth::at(p.cutoff_hz.max(1.0).log2()),
            resonance: Smooth::at(p.resonance),
            drive: Smooth::at(p.filter_drive),
            detune_cents: Smooth::at(p.unison_detune_cents),
            master_gain: Smooth::at(d.master_gain),
        }
    }

    fn retarget(&mut self, p: &Params, d: &Derived) {
        self.osc_a_position.target = p.osc_a_position;
        self.osc_a_level.target = p.osc_a_level;
        self.osc_b_position.target = p.osc_b_position;
        self.osc_b_level.target = p.osc_b_level;
        self.sub_level.target = p.sub_level;
        self.noise_level.target = p.noise_level;
        self.cutoff_octaves.target = p.cutoff_hz.max(1.0).log2();
        self.resonance.target = p.resonance;
        self.drive.target = p.filter_drive;
        self.detune_cents.target = p.unison_detune_cents;
        self.master_gain.target = d.master_gain;
    }

    #[inline]
    fn step(&mut self, coef: f32) -> SmoothedValues {
        SmoothedValues {
            osc_a_position: self.osc_a_position.step(coef),
            osc_a_level: self.osc_a_level.step(coef),
            osc_b_position: self.osc_b_position.step(coef),
            osc_b_level: self.osc_b_level.step(coef),
            sub_level: self.sub_level.step(coef),
            noise_level: self.noise_level.step(coef),
            cutoff_octaves: self.cutoff_octaves.step(coef),
            resonance: self.resonance.step(coef),
            drive: self.drive.step(coef),
            detune_cents: self.detune_cents.step(coef),
            master_gain: self.master_gain.step(coef),
        }
    }
}

/// A one-pole DC blocker at about 10 Hz.
#[derive(Debug, Clone, Copy, Default)]
struct DcBlocker {
    x1: f32,
    y1: f32,
}

impl DcBlocker {
    #[inline]
    fn process(&mut self, x: f32, r: f32) -> f32 {
        let y = x - self.x1 + r * self.y1;
        self.x1 = x;
        self.y1 = builtin_dsp_core::flush_denormal(y);
        self.y1
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct QueuedMidi {
    offset: u32,
    status: u8,
    data1: u8,
    data2: u8,
}

/// The keys a mono patch holds, oldest first.
#[derive(Debug, Clone, Copy)]
struct NoteStack {
    notes: [u8; NOTE_STACK],
    len: usize,
}

impl NoteStack {
    const EMPTY: Self = Self {
        notes: [0; NOTE_STACK],
        len: 0,
    };

    fn remove(&mut self, note: u8) {
        if let Some(at) = self.notes[..self.len].iter().position(|&n| n == note) {
            self.notes.copy_within(at + 1..self.len, at);
            self.len -= 1;
        }
    }

    fn push(&mut self, note: u8) {
        self.remove(note);
        if self.len == NOTE_STACK {
            self.notes.copy_within(1.., 0);
            self.len -= 1;
        }
        self.notes[self.len] = note;
        self.len += 1;
    }

    fn top(&self) -> Option<u8> {
        self.len.checked_sub(1).map(|i| self.notes[i])
    }
}

pub struct Dsp {
    sample_rate: f32,
    params: Params,
    derived: Derived,
    smooth: Smoothers,
    bank: &'static Bank,
    voices: [Voice; MAX_VOICES],
    age: u64,
    midi: [QueuedMidi; MIDI_QUEUE],
    midi_len: usize,
    midi_dropped: u64,
    held: NoteStack,
    sustain_pedal: bool,
    mod_wheel: f32,
    aftertouch: f32,
    /// −1..1, from the pitch wheel; smoothed into `bend_value`.
    bend_target: f32,
    bend_value: f32,
    last_pitch: Option<f32>,
    tempo_bpm: f32,
    lfo_phase: [f32; 2],
    fx: FxRack,
    dc: [DcBlocker; 2],
    dc_r: f32,
    power_gain: f32,
    seed: u32,
}

impl std::fmt::Debug for Dsp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dsp")
            .field("sample_rate", &self.sample_rate)
            .field("params", &self.params)
            .field("active_voices", &self.active_voices())
            .field("midi_dropped", &self.midi_dropped)
            .finish()
    }
}

impl Dsp {
    pub fn new(sample_rate: f32) -> Self {
        let sample_rate = sample_rate.max(1.0);
        let params = default_params();
        let tempo_bpm = 120.0;
        let derived = Derived::new(&params, sample_rate, tempo_bpm);
        let mut dsp = Self {
            sample_rate,
            params,
            derived,
            smooth: Smoothers::at(&params, &derived),
            bank: wavetable::bank(),
            voices: [Voice::SILENT; MAX_VOICES],
            age: 0,
            midi: [QueuedMidi::default(); MIDI_QUEUE],
            midi_len: 0,
            midi_dropped: 0,
            held: NoteStack::EMPTY,
            sustain_pedal: false,
            mod_wheel: 0.0,
            aftertouch: 0.0,
            bend_target: 0.0,
            bend_value: 0.0,
            last_pitch: None,
            tempo_bpm,
            lfo_phase: [0.0; 2],
            fx: FxRack::new(sample_rate),
            dc: [DcBlocker::default(); 2],
            dc_r: dc_coefficient(sample_rate),
            power_gain: 1.0,
            seed: 0x9e37_79b9,
        };
        dsp.fx.configure(&dsp.params);
        dsp
    }

    pub fn params(&self) -> &Params {
        &self.params
    }

    pub fn active_voices(&self) -> usize {
        self.voices.iter().filter(|voice| voice.active).count()
    }

    /// MIDI events dropped because a block carried more than [`MIDI_QUEUE`].
    pub fn midi_dropped(&self) -> u64 {
        self.midi_dropped
    }

    pub fn set_params(&mut self, mut params: Params) {
        ipc::sanitize_params(&mut params);
        self.params = params;
        self.refresh();
    }

    pub fn apply_wire_param(&mut self, index: u32, value: f32) -> bool {
        let applied = ipc::apply_wire_param(&mut self.params, index, value);
        if applied {
            self.refresh();
        }
        applied
    }

    /// The transport tempo, for tempo-synced LFOs and the synced delay.
    pub fn set_tempo_bpm(&mut self, tempo_bpm: f32) {
        if !tempo_bpm.is_finite() || tempo_bpm <= 0.0 || (tempo_bpm - self.tempo_bpm).abs() < 1.0e-3
        {
            return;
        }
        self.tempo_bpm = tempo_bpm;
        self.fx.set_tempo_bpm(tempo_bpm);
        self.derived = Derived::new(&self.params, self.sample_rate, self.tempo_bpm);
    }

    /// Recompute what the patch decides, after an edit. Allocation-free.
    fn refresh(&mut self) {
        let previous_layout = self.derived.layout;
        self.derived = Derived::new(&self.params, self.sample_rate, self.tempo_bpm);
        self.smooth.retarget(&self.params, &self.derived);
        self.fx.configure(&self.params);
        if previous_layout.count < self.derived.layout.count {
            // A grown stack keeps its voices' phases; the new ones start
            // spread round the cycle like a fresh note's would.
            for voice in self.voices.iter_mut().filter(|voice| voice.active) {
                for i in previous_layout.count..self.derived.layout.count {
                    let a = UnisonOscillator::start(i, 0.0, 0.0, 0.0, || 0.5);
                    voice.osc_a[i] = a;
                    voice.osc_b[i] = UnisonOscillator {
                        phase: (a.phase + 0.37).fract(),
                        ..a
                    };
                }
            }
        }
        if self.params.voice_mode != VoiceMode::Poly {
            // A patch switched to mono keeps only its first voice.
            for voice in self.voices.iter_mut().skip(1).filter(|voice| voice.active) {
                voice.fading = true;
                voice.pending = None;
            }
        }
    }

    pub fn all_notes_off(&mut self) {
        self.held = NoteStack::EMPTY;
        self.sustain_pedal = false;
        for voice in &mut self.voices {
            if voice.active {
                voice.gate_off();
                if let Some(pending) = voice.pending.as_mut() {
                    pending.released = true;
                }
            }
        }
    }

    /// Silence at once (all sound off): every voice fades out over a few
    /// milliseconds and nothing is left to start.
    pub fn all_sound_off(&mut self) {
        self.held = NoteStack::EMPTY;
        for voice in self.voices.iter_mut().filter(|voice| voice.active) {
            voice.fading = true;
            voice.pending = None;
        }
    }

    /// Queue one MIDI event for the next block, `offset` samples into it.
    /// Allocation-free; a block's events beyond [`MIDI_QUEUE`] are dropped and
    /// counted.
    pub fn queue_midi(&mut self, offset: u32, status: u8, data1: u8, data2: u8) {
        if self.midi_len == MIDI_QUEUE {
            self.midi_dropped = self.midi_dropped.wrapping_add(1);
            return;
        }
        self.midi[self.midi_len] = QueuedMidi {
            offset,
            status,
            data1,
            data2,
        };
        self.midi_len += 1;
    }

    /// Apply one MIDI message now.
    pub fn midi(&mut self, status: u8, data1: u8, data2: u8) {
        match status & 0xf0 {
            0x90 if data2 > 0 => self.handle_note_on(data1 & 0x7f, data2 & 0x7f),
            0x80 | 0x90 => self.handle_note_off(data1 & 0x7f),
            0xb0 => self.control_change(data1, data2),
            0xd0 => self.aftertouch = f32::from(data1 & 0x7f) / 127.0,
            0xe0 => {
                let value = u16::from(data1 & 0x7f) | (u16::from(data2 & 0x7f) << 7);
                self.bend_target = ((f32::from(value) - 8_192.0) / 8_192.0).clamp(-1.0, 1.0);
            }
            _ => {}
        }
    }

    fn control_change(&mut self, controller: u8, value: u8) {
        let unit = f32::from(value & 0x7f) / 127.0;
        match controller {
            1 => self.mod_wheel = unit,
            64 => {
                let down = value >= 64;
                if self.sustain_pedal && !down {
                    self.release_sustained();
                }
                self.sustain_pedal = down;
            }
            120 => self.all_sound_off(),
            121 => {
                self.mod_wheel = 0.0;
                self.aftertouch = 0.0;
                self.bend_target = 0.0;
                if self.sustain_pedal {
                    self.release_sustained();
                }
                self.sustain_pedal = false;
            }
            123 => self.all_notes_off(),
            _ => {}
        }
    }

    fn release_sustained(&mut self) {
        let held_top = self.held.top();
        for voice in self.voices.iter_mut().filter(|voice| voice.sustained) {
            // A mono voice still playing a held key keeps going.
            if self.params.voice_mode != VoiceMode::Poly && held_top.is_some() {
                voice.sustained = false;
                continue;
            }
            voice.gate_off();
        }
    }

    fn next_seed(&mut self) -> u32 {
        self.seed = self
            .seed
            .wrapping_mul(1_664_525)
            .wrapping_add(1_013_904_223);
        self.seed | 1
    }

    /// Where a new note's pitch starts: the last note's, gliding, when glide
    /// is on.
    fn start_pitch(&self, note: u8) -> f32 {
        match self.last_pitch {
            Some(pitch) if self.params.glide_ms > 0.0 => pitch,
            _ => f32::from(note),
        }
    }

    fn handle_note_on(&mut self, note: u8, velocity: u8) {
        if !self.params.power {
            return;
        }
        let velocity = f32::from(velocity) / 127.0;
        self.age = self.age.wrapping_add(1);
        let start_pitch = self.start_pitch(note);
        let seed = self.next_seed();
        match self.params.voice_mode {
            VoiceMode::Poly => self.note_on_poly(note, velocity, start_pitch, seed),
            mode => self.note_on_mono(note, velocity, start_pitch, seed, mode),
        }
        self.last_pitch = Some(f32::from(note));
    }

    fn note_on_poly(&mut self, note: u8, velocity: f32, start_pitch: f32, seed: u32) {
        let age = self.age;
        // The same key still sounding: play it again from where it is.
        if let Some(voice) = self
            .voices
            .iter_mut()
            .find(|voice| voice.active && !voice.fading && voice.note == note)
        {
            voice.velocity = velocity;
            voice.age = age;
            voice.target_pitch = f32::from(note);
            voice.gate_on();
            return;
        }
        if let Some(index) = self.voices.iter().position(|voice| !voice.active) {
            let shared = self.shared_for_start();
            start_voice(
                &mut self.voices[index],
                &shared,
                PendingNote {
                    note,
                    velocity,
                    start_pitch,
                    released: false,
                },
                age,
                seed,
            );
            return;
        }
        // Steal: the quietest releasing voice, else the oldest held one —
        // never a note this same chord has just started.
        let index = self
            .voices
            .iter()
            .enumerate()
            .filter(|(_, voice)| voice.is_releasing())
            .min_by(|(_, a), (_, b)| a.amp.value.total_cmp(&b.amp.value))
            .map(|(index, _)| index)
            .or_else(|| {
                self.voices
                    .iter()
                    .enumerate()
                    .filter(|(_, voice)| !voice.fading)
                    .min_by_key(|(_, voice)| voice.age)
                    .map(|(index, _)| index)
            })
            .unwrap_or_else(|| {
                self.voices
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, voice)| voice.age)
                    .map_or(0, |(index, _)| index)
            });
        let voice = &mut self.voices[index];
        voice.fading = true;
        voice.age = age;
        voice.pending = Some(PendingNote {
            note,
            velocity,
            start_pitch,
            released: false,
        });
    }

    fn note_on_mono(
        &mut self,
        note: u8,
        velocity: f32,
        start_pitch: f32,
        seed: u32,
        mode: VoiceMode,
    ) {
        self.held.push(note);
        let age = self.age;
        let glide = self.params.glide_ms > 0.0;
        let voice = &mut self.voices[0];
        if voice.active && !voice.fading {
            let overlapping = voice.gate;
            voice.note = note;
            voice.target_pitch = f32::from(note);
            if !glide {
                voice.pitch = voice.target_pitch;
            }
            voice.age = age;
            if !(overlapping && mode == VoiceMode::Legato) {
                voice.velocity = velocity;
                voice.gate_on();
            } else {
                voice.gate = true;
                voice.sustained = false;
            }
            return;
        }
        let shared = self.shared_for_start();
        start_voice(
            &mut self.voices[0],
            &shared,
            PendingNote {
                note,
                velocity,
                start_pitch,
                released: false,
            },
            age,
            seed,
        );
    }

    fn handle_note_off(&mut self, note: u8) {
        if self.params.voice_mode != VoiceMode::Poly {
            self.note_off_mono(note);
            return;
        }
        let pedal = self.sustain_pedal;
        for voice in self.voices.iter_mut().filter(|voice| voice.active) {
            if let Some(pending) = voice.pending.as_mut() {
                if pending.note == note {
                    pending.released = true;
                }
            }
            if voice.note == note && voice.gate && !voice.fading {
                if pedal {
                    voice.sustained = true;
                } else {
                    voice.gate_off();
                }
            }
        }
    }

    fn note_off_mono(&mut self, note: u8) {
        self.held.remove(note);
        let mode = self.params.voice_mode;
        let glide = self.params.glide_ms > 0.0;
        let top = self.held.top();
        let pedal = self.sustain_pedal;
        let voice = &mut self.voices[0];
        if !voice.active || voice.note != note {
            return;
        }
        match top {
            // Back to the key still held.
            Some(previous) => {
                voice.note = previous;
                voice.target_pitch = f32::from(previous);
                if !glide {
                    voice.pitch = voice.target_pitch;
                }
                if mode == VoiceMode::Mono {
                    voice.gate_on();
                }
                self.last_pitch = Some(f32::from(previous));
            }
            None if pedal => voice.sustained = true,
            None => voice.gate_off(),
        }
    }

    fn shared_for_start(&self) -> StartContext {
        StartContext {
            phase_random: self.params.phase_random,
            analog: self.params.analog,
            lfo_retrigger: [self.params.lfo1_retrigger, self.params.lfo2_retrigger],
            global_lfo_phase: self.lfo_phase,
            drift_step: self.derived.drift.step,
        }
    }

    /// Render `frames` stereo frames into `out` (interleaved L, R), applying
    /// queued MIDI at its sample. Allocation-free.
    pub fn process_block(&mut self, out: &mut [f32], frames: usize) {
        let frames = frames.min(out.len() / 2);
        // Events usually arrive in order; keep them so without allocating.
        let queued = self.midi_len;
        for i in 1..queued {
            let mut j = i;
            while j > 0 && self.midi[j - 1].offset > self.midi[j].offset {
                self.midi.swap(j - 1, j);
                j -= 1;
            }
        }
        // An offset past the block's end plays on its last frame.
        let last = frames.saturating_sub(1) as u32;
        let mut next = 0;
        for frame in 0..frames {
            while next < queued && self.midi[next].offset.min(last) as usize <= frame {
                let event = self.midi[next];
                self.midi(event.status, event.data1, event.data2);
                next += 1;
            }
            let (left, right) = self.render_frame();
            out[frame * 2] = left;
            out[frame * 2 + 1] = right;
        }
        // A zero-frame block still delivers its events.
        while next < queued {
            let event = self.midi[next];
            self.midi(event.status, event.data1, event.data2);
            next += 1;
        }
        self.midi_len = 0;
    }

    #[inline]
    fn render_frame(&mut self) -> (f32, f32) {
        let power_target = if self.params.power { 1.0 } else { 0.0 };
        if self.power_gain != power_target {
            let step = self.derived.power_step;
            self.power_gain = if power_target > self.power_gain {
                (self.power_gain + step).min(1.0)
            } else {
                (self.power_gain - step).max(0.0)
            };
            if self.power_gain == 0.0 {
                // Fully off: nothing left sounding, nothing to come back.
                self.voices = [Voice::SILENT; MAX_VOICES];
                self.held = NoteStack::EMPTY;
                self.fx.reset();
                self.dc = [DcBlocker::default(); 2];
            }
        }
        if self.power_gain == 0.0 {
            return (0.0, 0.0);
        }

        let s = self.smooth.step(self.derived.smoothing);
        self.bend_value += (self.bend_target - self.bend_value) * self.derived.smoothing;
        for (phase, increment) in self.lfo_phase.iter_mut().zip(self.derived.lfo_increment) {
            *phase += increment;
            if *phase >= 1.0 {
                *phase -= 1.0;
            }
        }
        let shared = Shared {
            p: &self.params,
            d: &self.derived,
            s,
            bank: self.bank,
            sample_rate: self.sample_rate,
            bend_semitones: self.bend_value * self.params.pitch_bend_range,
            mod_wheel: self.mod_wheel,
            aftertouch: self.aftertouch,
        };
        let start = self.shared_for_start();
        let mut left = 0.0;
        let mut right = 0.0;
        for voice in self.voices.iter_mut().filter(|voice| voice.active) {
            let (l, r) = render_voice(voice, &shared, &start);
            left += l;
            right += r;
        }
        let left = self.dc[0].process(left, self.dc_r);
        let right = self.dc[1].process(right, self.dc_r);
        let (left, right) = self.fx.process(&self.params, left, right);
        let gain = s.master_gain * self.power_gain;
        (ceiling(left * gain), ceiling(right * gain))
    }
}

/// What starting a note needs from the synth.
#[derive(Debug, Clone, Copy)]
struct StartContext {
    phase_random: f32,
    analog: f32,
    lfo_retrigger: [bool; 2],
    global_lfo_phase: [f32; 2],
    drift_step: u32,
}

/// Turn `voice` into a fresh voice playing `note`.
fn start_voice(voice: &mut Voice, start: &StartContext, note: PendingNote, age: u64, seed: u32) {
    *voice = Voice::SILENT;
    voice.active = true;
    voice.note = note.note;
    voice.velocity = note.velocity;
    voice.age = age;
    voice.pitch = note.start_pitch;
    voice.target_pitch = f32::from(note.note);
    voice.rng = seed;
    voice.random = voice.bipolar();
    voice.drift_countdown = start.drift_step;
    for k in 0..2 {
        voice.lfo_phase[k] = if start.lfo_retrigger[k] {
            0.0
        } else {
            start.global_lfo_phase[k]
        };
        voice.lfo_held[k] = voice.bipolar();
    }
    for i in 0..MAX_UNISON {
        let a =
            UnisonOscillator::start(i, 0.0, start.phase_random, start.analog, || voice.uniform());
        voice.osc_a[i] = a;
        let b = UnisonOscillator::start(i, 0.37, start.phase_random, start.analog, || {
            voice.uniform()
        });
        voice.osc_b[i] = b;
    }
    voice.gate_on();
    if note.released {
        voice.gate_off();
    }
}

#[inline]
fn hz_for(pitch: f32) -> f32 {
    440.0 * ((pitch - 69.0) * (1.0 / 12.0)).exp2()
}

/// A soft saturator: tanh's shape, cheaply. Full scale at ±3.
#[inline]
fn saturate(x: f32) -> f32 {
    let x = x.clamp(-3.0, 3.0);
    x * (27.0 + x * x) / (27.0 + 9.0 * x * x)
}

/// The original WrapSynth's clipper, kept for the effects' drive.
#[inline]
pub(crate) fn soft_clip(sample: f32) -> f32 {
    sample / (1.0 + sample.abs())
}

/// Transparent below [`CEILING_KNEE`]; above, rounded so the output never
/// passes full scale.
#[inline]
fn ceiling(x: f32) -> f32 {
    let magnitude = x.abs();
    if magnitude <= CEILING_KNEE {
        return x;
    }
    let room = 1.0 - CEILING_KNEE;
    let over = (magnitude - CEILING_KNEE) / room;
    (CEILING_KNEE + room * saturate(over * 3.0)).copysign(x)
}

fn dc_coefficient(sample_rate: f32) -> f32 {
    1.0 - std::f32::consts::TAU * 10.0 / sample_rate.max(1.0)
}

/// One oscillator's unison stack at one sample: `(left, right)`.
#[allow(clippy::too_many_arguments)]
#[inline]
fn osc_stack(
    oscs: &mut [UnisonOscillator; MAX_UNISON],
    bank: &Bank,
    table: Wavetable,
    position: f32,
    increment: f32,
    detune_cents: f32,
    layout: &UnisonLayout,
    pans: &[(f32, f32); MAX_UNISON],
) -> (f32, f32) {
    let count = layout.count;
    // The level for the sharpest voice of the stack, so none of it aliases.
    let widest = increment * cents_ratio_fast(detune_cents + ANALOG_MAX_CENTS);
    let cursor = bank.cursor(table, mip_for(widest), position);
    let mut left = 0.0;
    let mut right = 0.0;
    for i in 0..count {
        let osc = &mut oscs[i];
        let ratio = cents_ratio_fast(layout.detune[i] * detune_cents + osc.drift_cents);
        let phase = osc.advance(f64::from(increment * ratio));
        let sample = cursor.read(phase) * layout.weight[i] * osc.gain;
        left += sample * pans[i].0;
        right += sample * pans[i].1;
    }
    (left, right)
}

/// One voice at one sample: `(left, right)`.
#[inline]
fn render_voice(voice: &mut Voice, shared: &Shared<'_>, start: &StartContext) -> (f32, f32) {
    let p = shared.p;
    let d = shared.d;
    let s = &shared.s;

    if voice.fading {
        voice.fade -= d.fade_step;
        if voice.fade <= 0.0 {
            match voice.pending.take() {
                Some(note) => {
                    let (age, seed) = (voice.age, voice.rng ^ 0x5bd1_e995);
                    start_voice(voice, start, note, age, seed);
                }
                None => {
                    *voice = Voice::SILENT;
                    return (0.0, 0.0);
                }
            }
        }
    }

    let amp = voice.amp.next(&d.amp_env);
    let filter_env = voice.filter_env.next(&d.filter_env);
    let mod_env = voice.mod_env.next(&d.mod_env);
    if voice.amp.is_idle() && !voice.fading {
        voice.active = false;
        return (0.0, 0.0);
    }

    // LFOs, each a step further on; a sample-and-hold draws at every wrap.
    let mut lfo = [0.0f32; 2];
    let shapes = [p.lfo1_shape, p.lfo2_shape];
    for k in 0..2 {
        let mut phase = voice.lfo_phase[k] + d.lfo_increment[k];
        if phase >= 1.0 {
            phase -= 1.0;
            voice.lfo_held[k] = voice.bipolar();
        }
        voice.lfo_phase[k] = phase;
        lfo[k] = shapes[k].at(phase, voice.lfo_held[k]);
    }

    // Glide.
    if voice.pitch != voice.target_pitch {
        let next = voice.target_pitch + (voice.pitch - voice.target_pitch) * d.glide_coef;
        voice.pitch = if (next - voice.target_pitch).abs() < 1.0e-4 {
            voice.target_pitch
        } else {
            next
        };
    }

    let mut sources: SourceValues = [0.0; ModSource::COUNT];
    sources[ModSource::Env1.index()] = amp;
    sources[ModSource::Env2.index()] = filter_env;
    sources[ModSource::Env3.index()] = mod_env;
    sources[ModSource::Lfo1.index()] = lfo[0];
    sources[ModSource::Lfo2.index()] = lfo[1];
    sources[ModSource::Velocity.index()] = voice.velocity;
    sources[ModSource::ModWheel.index()] = shared.mod_wheel;
    sources[ModSource::Aftertouch.index()] = shared.aftertouch;
    sources[ModSource::Note.index()] = ((voice.pitch - 60.0) / 60.0).clamp(-1.0, 1.0);
    sources[ModSource::Random.index()] = voice.random;
    let m = apply_routes(&d.routes, &sources);
    let at = |dest: ModDest| m[dest.index()];

    // Pitches.
    let base = voice.pitch + shared.bend_semitones;
    let pitch_a = base + at(ModDest::OscAPitch) * PITCH_MOD_SEMITONES;
    let pitch_b = base
        + p.osc_b_semitones
        + p.osc_b_detune_cents * 0.01
        + at(ModDest::OscBPitch) * PITCH_MOD_SEMITONES;
    let increment_a = (hz_for(pitch_a) / shared.sample_rate).min(0.49);
    let increment_b = (hz_for(pitch_b) / shared.sample_rate).min(0.49);
    let detune = (s.detune_cents + at(ModDest::UnisonDetune) * 50.0).clamp(0.0, 100.0);

    // Analog drift: a new target every step, approached smoothly.
    if p.analog > 0.0 {
        if voice.drift_countdown == 0 {
            voice.drift_countdown = d.drift.step;
            let reach = p.analog * ANALOG_MAX_CENTS;
            for i in 0..d.layout.count {
                voice.osc_a[i].drift_target = reach * voice.bipolar();
                voice.osc_b[i].drift_target = reach * voice.bipolar();
            }
        } else {
            voice.drift_countdown -= 1;
        }
        for i in 0..d.layout.count {
            voice.osc_a[i].drift(d.drift.smoothing);
            voice.osc_b[i].drift(d.drift.smoothing);
        }
    }

    let level_a = (s.osc_a_level + at(ModDest::OscALevel)).clamp(0.0, 1.0);
    let level_b = (s.osc_b_level + at(ModDest::OscBLevel)).clamp(0.0, 1.0);
    let (mut left, mut right) = (0.0, 0.0);
    if level_a > 0.0 {
        let (l, r) = osc_stack(
            &mut voice.osc_a,
            shared.bank,
            p.osc_a_wave,
            (s.osc_a_position + at(ModDest::OscAPosition)).clamp(0.0, 1.0),
            increment_a,
            detune,
            &d.layout,
            &d.pans,
        );
        left += l * level_a;
        right += r * level_a;
    }
    if level_b > 0.0 {
        let (l, r) = osc_stack(
            &mut voice.osc_b,
            shared.bank,
            p.osc_b_wave,
            (s.osc_b_position + at(ModDest::OscBPosition)).clamp(0.0, 1.0),
            increment_b,
            detune,
            &d.layout,
            &d.pans,
        );
        left += l * level_b;
        right += r * level_b;
    }
    // Sub: a sine an octave under oscillator A.
    voice.sub_phase += increment_a * 0.5;
    if voice.sub_phase >= 1.0 {
        voice.sub_phase -= 1.0;
    }
    let sub_level = (s.sub_level + at(ModDest::SubLevel)).clamp(0.0, 1.0);
    let noise_level = (s.noise_level + at(ModDest::NoiseLevel)).clamp(0.0, 1.0);
    let mut mono = 0.0;
    if sub_level > 0.0 {
        mono += (std::f32::consts::TAU * voice.sub_phase).sin() * sub_level * 0.7;
    }
    if noise_level > 0.0 {
        mono += voice.bipolar() * noise_level * 0.5;
    }
    left += mono;
    right += mono;

    // Drive: off at zero, so a clean patch stays clean.
    let drive = (s.drive + at(ModDest::FilterDrive)).clamp(0.0, 1.0);
    if drive > 1.0e-3 {
        let gain = 1.0 + drive * 7.0;
        let makeup = gain.sqrt().recip();
        left = saturate(left * gain) * makeup;
        right = saturate(right * gain) * makeup;
    }

    // Filter, its coefficients worked out at control rate.
    if voice.coefficient_countdown == 0 {
        voice.coefficient_countdown = FILTER_CONTROL_INTERVAL - 1;
        let octaves = s.cutoff_octaves
            + filter_env * p.filter_env_amount * CUTOFF_OCTAVES
            + p.filter_key_track * (voice.pitch - 60.0) / 12.0
            + at(ModDest::FilterCutoff) * CUTOFF_OCTAVES;
        let cutoff = octaves.exp2().clamp(20.0, shared.sample_rate * 0.45);
        let resonance = (s.resonance + at(ModDest::FilterResonance) * 0.95).clamp(0.0, 0.98);
        voice.coefficients =
            Coefficients::new(p.filter_type, cutoff, resonance, shared.sample_rate);
    } else {
        voice.coefficient_countdown -= 1;
    }
    let left = voice.filter_l.process(&voice.coefficients, left);
    let right = voice.filter_r.process(&voice.coefficients, right);

    // Amp.
    let velocity = 1.0 - p.velocity_amount + p.velocity_amount * voice.velocity;
    let gain = amp * velocity * (1.0 + at(ModDest::Amp)).max(0.0) * VOICE_GAIN * voice.fade;
    let pan = at(ModDest::Pan).clamp(-1.0, 1.0);
    (
        left * gain * (1.0 - pan).min(1.0),
        right * gain * (1.0 + pan).min(1.0),
    )
}

impl Instrument for Dsp {
    fn reset(&mut self) {
        self.voices = [Voice::SILENT; MAX_VOICES];
        self.held = NoteStack::EMPTY;
        self.midi_len = 0;
        self.sustain_pedal = false;
        self.fx.reset();
        self.dc = [DcBlocker::default(); 2];
        self.smooth = Smoothers::at(&self.params, &self.derived);
    }

    /// Not for the audio path: the effects' buffers are rebuilt.
    fn set_sample_rate(&mut self, sample_rate: f32) {
        let sample_rate = sample_rate.max(1.0);
        if (sample_rate - self.sample_rate).abs() < f32::EPSILON {
            return;
        }
        self.sample_rate = sample_rate;
        self.dc_r = dc_coefficient(sample_rate);
        self.fx.set_sample_rate(sample_rate);
        self.derived = Derived::new(&self.params, sample_rate, self.tempo_bpm);
        self.fx.configure(&self.params);
    }

    fn note_on(&mut self, note: u8, velocity: u8) {
        if velocity == 0 {
            self.handle_note_off(note);
        } else {
            self.handle_note_on(note, velocity);
        }
    }

    fn note_off(&mut self, note: u8) {
        self.handle_note_off(note);
    }

    fn process_stereo(&mut self) -> (f32, f32) {
        let mut frame = [0.0; 2];
        self.process_block(&mut frame, 1);
        (frame[0], frame[1])
    }
}

#[cfg(test)]
mod tests;

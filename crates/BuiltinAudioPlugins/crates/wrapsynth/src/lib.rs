//! WrapSynth - a realtime-safe dual-wavetable polyphonic instrument.
//!
//! MIDI and compact numeric parameter events are applied by the native plugin
//! host. The audio path uses fixed voice/unison storage and performs no heap
//! allocation, locking, logging, or string lookup.

pub mod ipc;
pub mod ui;

use builtin_dsp_core::{Instrument, ParamDescriptor, PluginCategory, PluginDescriptor, clamp};
use serde::{Deserialize, Serialize};

pub use ipc::{UI_PARAM_IDS, ui_param_id, ui_param_index};

pub const PLUGIN_ID: &str = "futureboard.wrapsynth";
pub const MAX_VOICES: usize = 16;
pub const MAX_UNISON: usize = 7;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Waveform {
    Saw,
    Square,
    Triangle,
    Sine,
}

impl Waveform {
    pub const ALL: [Waveform; 4] = [Self::Saw, Self::Square, Self::Triangle, Self::Sine];

    pub fn to_wire(self) -> f32 {
        match self {
            Self::Saw => 0.0,
            Self::Square => 1.0,
            Self::Triangle => 2.0,
            Self::Sine => 3.0,
        }
    }

    pub fn from_wire(value: f32) -> Self {
        match value.round() as i32 {
            1 => Self::Square,
            2 => Self::Triangle,
            3 => Self::Sine,
            _ => Self::Saw,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Params {
    pub power: bool,
    pub osc_a_wave: Waveform,
    pub osc_a_position: f32,
    pub osc_a_level: f32,
    pub osc_b_wave: Waveform,
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
}

pub fn default_params() -> Params {
    Params {
        power: true,
        osc_a_wave: Waveform::Saw,
        osc_a_position: 0.18,
        osc_a_level: 0.78,
        osc_b_wave: Waveform::Square,
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
                default_value: 0.18,
                min: 0.0,
                max: 1.0,
                unit: "",
            },
            ParamDescriptor {
                id: "oscALevel",
                name: "Osc A Level",
                default_value: 0.78,
                min: 0.0,
                max: 1.0,
                unit: "",
            },
            ParamDescriptor {
                id: "oscBPosition",
                name: "Osc B Position",
                default_value: 0.42,
                min: 0.0,
                max: 1.0,
                unit: "",
            },
            ParamDescriptor {
                id: "oscBLevel",
                name: "Osc B Level",
                default_value: 0.38,
                min: 0.0,
                max: 1.0,
                unit: "",
            },
            ParamDescriptor {
                id: "cutoffHz",
                name: "Filter Cutoff",
                default_value: 6_400.0,
                min: 40.0,
                max: 20_000.0,
                unit: "Hz",
            },
            ParamDescriptor {
                id: "resonance",
                name: "Resonance",
                default_value: 0.18,
                min: 0.0,
                max: 0.95,
                unit: "",
            },
            ParamDescriptor {
                id: "attackMs",
                name: "Attack",
                default_value: 8.0,
                min: 0.5,
                max: 5_000.0,
                unit: "ms",
            },
            ParamDescriptor {
                id: "releaseMs",
                name: "Release",
                default_value: 420.0,
                min: 5.0,
                max: 8_000.0,
                unit: "ms",
            },
            ParamDescriptor {
                id: "masterDb",
                name: "Master",
                default_value: -8.0,
                min: -24.0,
                max: 3.0,
                unit: "dB",
            },
        ],
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnvStage {
    Idle,
    Attack,
    Decay,
    Sustain,
    Release,
}

#[derive(Debug, Clone, Copy)]
struct Voice {
    active: bool,
    note: u8,
    velocity: f32,
    age: u64,
    env: f32,
    stage: EnvStage,
    phases_a: [f32; MAX_UNISON],
    phases_b: [f32; MAX_UNISON],
    sub_phase: f32,
    filter_low_l: f32,
    filter_band_l: f32,
    filter_low_r: f32,
    filter_band_r: f32,
    noise: u32,
}

impl Voice {
    const fn silent() -> Self {
        Self {
            active: false,
            note: 0,
            velocity: 0.0,
            age: 0,
            env: 0.0,
            stage: EnvStage::Idle,
            phases_a: [0.0; MAX_UNISON],
            phases_b: [0.0; MAX_UNISON],
            sub_phase: 0.0,
            filter_low_l: 0.0,
            filter_band_l: 0.0,
            filter_low_r: 0.0,
            filter_band_r: 0.0,
            noise: 0x1234_5678,
        }
    }
}

pub struct Dsp {
    sample_rate: f32,
    params: Params,
    voices: [Voice; MAX_VOICES],
    age: u64,
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
        Self {
            sample_rate: sample_rate.max(1.0),
            params: default_params(),
            voices: [Voice::silent(); MAX_VOICES],
            age: 0,
        }
    }

    pub fn params(&self) -> &Params {
        &self.params
    }

    pub fn set_params(&mut self, mut params: Params) {
        ipc::sanitize_params(&mut params);
        self.params = params;
    }

    pub fn apply_wire_param(&mut self, index: u32, value: f32) -> bool {
        ipc::apply_wire_param(&mut self.params, index, value)
    }

    pub fn all_notes_off(&mut self) {
        for voice in &mut self.voices {
            voice.stage = EnvStage::Release;
        }
    }

    /// The voice a new note takes: the same key if it still sounds, a free
    /// voice, then the quietest releasing voice, and only then the oldest
    /// held one. Stealing the quietest voice outright took the one a note of
    /// the same chord had just started (its envelope is still at zero, and
    /// every note of a block starts before any is rendered), so a sequenced
    /// chord on a busy synth kept only its first and last notes.
    fn allocate_voice(&self, note: u8) -> usize {
        self.voices
            .iter()
            .position(|voice| voice.active && voice.note == note)
            .or_else(|| self.voices.iter().position(|voice| !voice.active))
            .or_else(|| {
                self.voices
                    .iter()
                    .enumerate()
                    .filter(|(_, voice)| voice.stage == EnvStage::Release)
                    .min_by(|(_, left), (_, right)| {
                        left.env
                            .partial_cmp(&right.env)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
                    .map(|(index, _)| index)
            })
            .unwrap_or_else(|| {
                self.voices
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, voice)| voice.age)
                    .map_or(0, |(index, _)| index)
            })
    }

    #[inline]
    fn advance_envelope(params: &Params, sample_rate: f32, voice: &mut Voice) -> f32 {
        match voice.stage {
            EnvStage::Idle => {
                voice.active = false;
                voice.env = 0.0;
            }
            EnvStage::Attack => {
                voice.env += 1.0 / (params.attack_ms * 0.001 * sample_rate).max(1.0);
                if voice.env >= 1.0 {
                    voice.env = 1.0;
                    voice.stage = EnvStage::Decay;
                }
            }
            EnvStage::Decay => {
                voice.env -=
                    (1.0 - params.sustain) / (params.decay_ms * 0.001 * sample_rate).max(1.0);
                if voice.env <= params.sustain {
                    voice.env = params.sustain;
                    voice.stage = EnvStage::Sustain;
                }
            }
            EnvStage::Sustain => voice.env = params.sustain,
            EnvStage::Release => {
                voice.env -= 1.0 / (params.release_ms * 0.001 * sample_rate).max(1.0);
                if voice.env <= 0.0 {
                    voice.env = 0.0;
                    voice.active = false;
                    voice.stage = EnvStage::Idle;
                }
            }
        }
        voice.env
    }

    #[inline]
    fn render_voice(params: &Params, sample_rate: f32, voice: &mut Voice) -> (f32, f32) {
        let env = Self::advance_envelope(params, sample_rate, voice);
        if !voice.active {
            return (0.0, 0.0);
        }
        let base_hz = midi_to_hz(voice.note);
        let count = usize::from(params.unison).clamp(1, MAX_UNISON);
        let mut left = 0.0;
        let mut right = 0.0;
        for unison_index in 0..count {
            let spread = if count == 1 {
                0.0
            } else {
                unison_index as f32 / (count - 1) as f32 * 2.0 - 1.0
            };
            let detune = cents_ratio(spread * params.unison_detune_cents);
            let freq_a = base_hz * detune;
            let freq_b = base_hz
                * 2.0f32.powf(params.osc_b_semitones / 12.0)
                * cents_ratio(params.osc_b_detune_cents)
                * detune;
            voice.phases_a[unison_index] =
                (voice.phases_a[unison_index] + freq_a / sample_rate).fract();
            voice.phases_b[unison_index] =
                (voice.phases_b[unison_index] + freq_b / sample_rate).fract();
            let a = wavetable(
                voice.phases_a[unison_index],
                params.osc_a_wave,
                params.osc_a_position,
            ) * params.osc_a_level;
            let b = wavetable(
                voice.phases_b[unison_index],
                params.osc_b_wave,
                params.osc_b_position,
            ) * params.osc_b_level;
            let sample = (a + b) / count as f32;
            let pan = spread * params.stereo_width;
            left += sample * (0.5 * (1.0 - pan)).sqrt();
            right += sample * (0.5 * (1.0 + pan)).sqrt();
        }
        voice.sub_phase = (voice.sub_phase + base_hz * 0.5 / sample_rate).fract();
        let sub = (std::f32::consts::TAU * voice.sub_phase).sin() * params.sub_level;
        voice.noise ^= voice.noise << 13;
        voice.noise ^= voice.noise >> 17;
        voice.noise ^= voice.noise << 5;
        let noise = (voice.noise as f32 / u32::MAX as f32 * 2.0 - 1.0) * params.noise_level;
        left += sub + noise;
        right += sub + noise;

        let drive = 1.0 + params.filter_drive * 8.0;
        left = soft_clip(left * drive);
        right = soft_clip(right * drive);
        let coefficient =
            (2.0 * (std::f32::consts::PI * params.cutoff_hz / sample_rate).sin()).min(0.99);
        let damping = (2.0 - 1.9 * params.resonance).max(0.08);
        voice.filter_low_l += coefficient * voice.filter_band_l;
        let high_l = left - voice.filter_low_l - damping * voice.filter_band_l;
        voice.filter_band_l += coefficient * high_l;
        voice.filter_low_r += coefficient * voice.filter_band_r;
        let high_r = right - voice.filter_low_r - damping * voice.filter_band_r;
        voice.filter_band_r += coefficient * high_r;

        let gain = 10.0f32.powf(params.master_db / 20.0) * voice.velocity * env * 0.42;
        (voice.filter_low_l * gain, voice.filter_low_r * gain)
    }
}

impl Instrument for Dsp {
    fn reset(&mut self) {
        self.voices = [Voice::silent(); MAX_VOICES];
    }

    fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
    }

    fn note_on(&mut self, note: u8, velocity: u8) {
        if !self.params.power || velocity == 0 {
            self.note_off(note);
            return;
        }
        self.age = self.age.wrapping_add(1);
        let index = self.allocate_voice(note);
        let mut voice = Voice::silent();
        voice.active = true;
        voice.note = note;
        voice.velocity = f32::from(velocity) / 127.0;
        voice.age = self.age;
        voice.stage = EnvStage::Attack;
        voice.noise = 0x9e37_79b9 ^ u32::from(note) ^ self.age as u32;
        for unison_index in 0..MAX_UNISON {
            let offset = unison_index as f32 / MAX_UNISON as f32;
            voice.phases_a[unison_index] = offset;
            voice.phases_b[unison_index] = (offset + 0.37).fract();
        }
        self.voices[index] = voice;
    }

    fn note_off(&mut self, note: u8) {
        for voice in &mut self.voices {
            if voice.active && voice.note == note {
                voice.stage = EnvStage::Release;
            }
        }
    }

    fn process_stereo(&mut self) -> (f32, f32) {
        if !self.params.power {
            return (0.0, 0.0);
        }
        let mut left = 0.0;
        let mut right = 0.0;
        for voice in &mut self.voices {
            if voice.active {
                let rendered = Self::render_voice(&self.params, self.sample_rate, voice);
                left += rendered.0;
                right += rendered.1;
            }
        }
        (soft_clip(left), soft_clip(right))
    }
}

#[inline]
fn midi_to_hz(note: u8) -> f32 {
    440.0 * 2.0f32.powf((f32::from(note) - 69.0) / 12.0)
}

#[inline]
fn cents_ratio(cents: f32) -> f32 {
    2.0f32.powf(cents / 1_200.0)
}

#[inline]
fn soft_clip(sample: f32) -> f32 {
    sample / (1.0 + sample.abs())
}

/// One oscillator's sample at `phase` (0..1) of its cycle: `wave` warped
/// and blended toward its partner shape by `position`. The editor draws the
/// oscillator with this same function.
#[inline]
pub fn wavetable(phase: f32, wave: Waveform, position: f32) -> f32 {
    let warped = phase.powf(0.45 + position * 1.55);
    let primary = waveform_sample(warped, wave);
    let secondary = waveform_sample(
        phase,
        match wave {
            Waveform::Saw => Waveform::Triangle,
            Waveform::Square => Waveform::Sine,
            Waveform::Triangle => Waveform::Saw,
            Waveform::Sine => Waveform::Square,
        },
    );
    primary * (1.0 - position * 0.55) + secondary * position * 0.55
}

#[inline]
fn waveform_sample(phase: f32, wave: Waveform) -> f32 {
    match wave {
        Waveform::Saw => phase * 2.0 - 1.0,
        Waveform::Square => {
            if phase < 0.5 {
                1.0
            } else {
                -1.0
            }
        }
        Waveform::Triangle => 1.0 - 4.0 * (phase - 0.5).abs(),
        Waveform::Sine => (std::f32::consts::TAU * phase).sin(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_on_produces_finite_stereo_audio_without_allocating_voice_storage() {
        let mut dsp = Dsp::new(48_000.0);
        dsp.note_on(60, 110);
        let mut peak = 0.0f32;
        for _ in 0..2_000 {
            let (left, right) = dsp.process_stereo();
            assert!(left.is_finite() && right.is_finite());
            peak = peak.max(left.abs()).max(right.abs());
        }
        assert!(peak > 0.01);
    }

    #[test]
    fn note_off_reaches_silence_after_release() {
        let mut dsp = Dsp::new(48_000.0);
        let mut params = default_params();
        params.release_ms = 10.0;
        dsp.set_params(params);
        dsp.note_on(64, 100);
        for _ in 0..1_000 {
            let _ = dsp.process_stereo();
        }
        dsp.note_off(64);
        for _ in 0..2_000 {
            let _ = dsp.process_stereo();
        }
        let (left, right) = dsp.process_stereo();
        assert!(left.abs() < 1.0e-4 && right.abs() < 1.0e-4);
    }

    #[test]
    fn voice_stealing_remains_bounded() {
        let mut dsp = Dsp::new(48_000.0);
        for note in 24..100 {
            dsp.note_on(note, 90);
        }
        assert_eq!(
            dsp.voices.iter().filter(|voice| voice.active).count(),
            MAX_VOICES
        );
    }

    /// Every voice busy (the last chords still releasing), then a whole
    /// chord in one block: every note of it sounds, instead of each new note
    /// stealing the voice the previous one had just started.
    #[test]
    fn a_chord_on_a_full_synth_keeps_every_note() {
        let mut dsp = Dsp::new(48_000.0);
        for note in 40..(40 + MAX_VOICES as u8) {
            dsp.note_on(note, 90);
        }
        for _ in 0..200 {
            let _ = dsp.process_stereo();
        }
        for note in 40..48 {
            dsp.note_off(note);
        }
        let chord = [60u8, 64, 67, 71, 74];
        for &note in &chord {
            dsp.note_on(note, 100);
        }
        for note in chord {
            assert!(
                dsp.voices
                    .iter()
                    .any(|voice| voice.active && voice.note == note),
                "note {note} of the chord was stolen by the chord itself"
            );
        }
    }
}

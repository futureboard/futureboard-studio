//! MeowSyn — compact polyphonic soft-synth.
//!
//! Phase 3 (hard). Oscillators come from the MIT/Apache [`fundsp`] crate
//! (`default-features = false`). Voice graphs are rebuilt on note-on (control
//! path); the audio callback only ticks prebuilt units.

use std::fmt;

use builtin_dsp_core::{
    Instrument, ParamDescriptor, PluginCategory, PluginDescriptor, clamp, db_to_linear,
};
use fundsp::prelude32::*;

pub const PLUGIN_ID: &str = "futureboard.meowsyn";
pub const MAX_VOICES: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OscShape {
    SoftSaw,
    Square,
    Sine,
}

#[derive(Debug, Clone)]
pub struct Params {
    pub power: bool,
    pub shape: OscShape,
    pub cutoff_hz: f32,
    pub resonance: f32,
    pub attack_ms: f32,
    pub decay_ms: f32,
    pub sustain: f32,
    pub release_ms: f32,
    pub detune_cents: f32,
    pub gain_db: f32,
}

pub fn default_params() -> Params {
    Params {
        power: true,
        shape: OscShape::SoftSaw,
        cutoff_hz: 2_400.0,
        resonance: 0.2,
        attack_ms: 5.0,
        decay_ms: 120.0,
        sustain: 0.7,
        release_ms: 180.0,
        detune_cents: 6.0,
        gain_db: -6.0,
    }
}

pub fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PLUGIN_ID,
        name: "MeowSyn",
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
                id: "cutoffHz",
                name: "Cutoff",
                default_value: 2_400.0,
                min: 80.0,
                max: 16_000.0,
                unit: "Hz",
            },
            ParamDescriptor {
                id: "resonance",
                name: "Resonance",
                default_value: 0.2,
                min: 0.0,
                max: 1.0,
                unit: "",
            },
            ParamDescriptor {
                id: "attackMs",
                name: "Attack",
                default_value: 5.0,
                min: 0.5,
                max: 2_000.0,
                unit: "ms",
            },
            ParamDescriptor {
                id: "releaseMs",
                name: "Release",
                default_value: 180.0,
                min: 5.0,
                max: 5_000.0,
                unit: "ms",
            },
            ParamDescriptor {
                id: "gainDb",
                name: "Gain",
                default_value: -6.0,
                min: -24.0,
                max: 6.0,
                unit: "dB",
            },
        ],
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Idle,
    Attack,
    Decay,
    Sustain,
    Release,
}

struct Voice {
    active: bool,
    note: u8,
    velocity: f32,
    stage: Stage,
    env: f32,
    /// When the voice's note started, in note-ons: the oldest is stolen last
    /// resort, never one a note of the same chord just started.
    age: u64,
    unit: Box<dyn AudioUnit>,
}

impl Voice {
    fn silent(sample_rate: f32) -> Self {
        let mut unit: Box<dyn AudioUnit> = Box::new(soft_saw_hz(440.0));
        unit.set_sample_rate(f64::from(sample_rate));
        Self {
            active: false,
            note: 0,
            velocity: 0.0,
            stage: Stage::Idle,
            env: 0.0,
            age: 0,
            unit,
        }
    }
}

fn build_unit(
    shape: OscShape,
    freq_hz: f32,
    detune_cents: f32,
    sample_rate: f32,
) -> Box<dyn AudioUnit> {
    let detune = 2.0f32.powf(detune_cents / 1200.0);
    let f1 = freq_hz;
    let f2 = freq_hz * detune;
    let mut unit: Box<dyn AudioUnit> = match shape {
        OscShape::SoftSaw => Box::new((soft_saw_hz(f1) + soft_saw_hz(f2)) * 0.5),
        OscShape::Square => Box::new((square_hz(f1) + square_hz(f2)) * 0.5),
        OscShape::Sine => Box::new((sine_hz(f1) + sine_hz(f2)) * 0.5),
    };
    unit.set_sample_rate(f64::from(sample_rate));
    unit
}

pub struct Dsp {
    sample_rate: f32,
    params: Params,
    voices: [Voice; MAX_VOICES],
    /// Note-ons so far, stamped on each voice as its age.
    age: u64,
    lp_z_l: f32,
    lp_z_r: f32,
    lp_coeff: f32,
    output_gain: f32,
    attack_step: f32,
    decay_step: f32,
    release_step: f32,
}

impl fmt::Debug for Dsp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Dsp")
            .field("sample_rate", &self.sample_rate)
            .field("params", &self.params)
            .field(
                "active_voices",
                &self.voices.iter().filter(|v| v.active).count(),
            )
            .finish()
    }
}

impl Dsp {
    pub fn new(sample_rate: f32) -> Self {
        let sr = sample_rate.max(1.0);
        let voices = std::array::from_fn(|_| Voice::silent(sr));
        let mut dsp = Self {
            sample_rate: sr,
            params: default_params(),
            voices,
            age: 0,
            lp_z_l: 0.0,
            lp_z_r: 0.0,
            lp_coeff: 0.0,
            output_gain: 1.0,
            attack_step: 0.0,
            decay_step: 0.0,
            release_step: 0.0,
        };
        dsp.apply_params();
        dsp
    }

    pub fn params(&self) -> &Params {
        &self.params
    }

    pub fn set_params(&mut self, params: Params) {
        self.params = Params {
            power: params.power,
            shape: params.shape,
            cutoff_hz: clamp(params.cutoff_hz, 80.0, 16_000.0),
            resonance: clamp(params.resonance, 0.0, 1.0),
            attack_ms: clamp(params.attack_ms, 0.5, 2_000.0),
            decay_ms: clamp(params.decay_ms, 1.0, 2_000.0),
            sustain: clamp(params.sustain, 0.0, 1.0),
            release_ms: clamp(params.release_ms, 5.0, 5_000.0),
            detune_cents: clamp(params.detune_cents, 0.0, 50.0),
            gain_db: clamp(params.gain_db, -24.0, 6.0),
        };
        self.apply_params();
    }

    fn apply_params(&mut self) {
        self.output_gain = db_to_linear(self.params.gain_db);
        self.attack_step = 1.0 / (self.sample_rate * self.params.attack_ms * 0.001).max(1.0);
        self.decay_step = 1.0 / (self.sample_rate * self.params.decay_ms * 0.001).max(1.0);
        self.release_step = 1.0 / (self.sample_rate * self.params.release_ms * 0.001).max(1.0);
        let x = (-std::f32::consts::TAU * self.params.cutoff_hz / self.sample_rate).exp();
        self.lp_coeff = clamp(1.0 - x, 0.0, 1.0);
    }

    fn alloc_voice(&mut self, note: u8) -> usize {
        if let Some((idx, _)) = self
            .voices
            .iter()
            .enumerate()
            .find(|(_, v)| v.active && v.note == note)
        {
            return idx;
        }
        if let Some((idx, _)) = self.voices.iter().enumerate().find(|(_, v)| !v.active) {
            return idx;
        }
        // The quietest releasing voice, then the oldest held one. The quietest
        // voice outright is the one a note of the same chord just started.
        if let Some((idx, _)) = self
            .voices
            .iter()
            .enumerate()
            .filter(|(_, v)| v.stage == Stage::Release)
            .min_by(|(_, a), (_, b)| {
                a.env
                    .partial_cmp(&b.env)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
        {
            return idx;
        }
        self.voices
            .iter()
            .enumerate()
            .min_by_key(|(_, v)| v.age)
            .map(|(i, _)| i)
            .unwrap_or(0)
    }

    #[inline]
    fn advance_env(&mut self, index: usize) -> f32 {
        let sustain = self.params.sustain;
        let attack_step = self.attack_step;
        let decay_step = self.decay_step;
        let release_step = self.release_step;
        let voice = &mut self.voices[index];
        match voice.stage {
            Stage::Idle => {
                voice.env = 0.0;
                voice.active = false;
            }
            Stage::Attack => {
                voice.env += attack_step;
                if voice.env >= 1.0 {
                    voice.env = 1.0;
                    voice.stage = Stage::Decay;
                }
            }
            Stage::Decay => {
                voice.env -= decay_step * (1.0 - sustain).max(0.001);
                if voice.env <= sustain {
                    voice.env = sustain;
                    voice.stage = Stage::Sustain;
                }
            }
            Stage::Sustain => {
                voice.env = sustain;
            }
            Stage::Release => {
                voice.env -= release_step;
                if voice.env <= 0.0 {
                    voice.env = 0.0;
                    voice.stage = Stage::Idle;
                    voice.active = false;
                }
            }
        }
        voice.env
    }
}

fn midi_to_hz(note: u8) -> f32 {
    440.0 * 2.0f32.powf((f32::from(note) - 69.0) / 12.0)
}

impl Instrument for Dsp {
    fn reset(&mut self) {
        for voice in &mut self.voices {
            voice.active = false;
            voice.stage = Stage::Idle;
            voice.env = 0.0;
            voice.unit.reset();
        }
        self.lp_z_l = 0.0;
        self.lp_z_r = 0.0;
    }

    fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        for voice in &mut self.voices {
            voice.unit.set_sample_rate(f64::from(self.sample_rate));
        }
        self.apply_params();
    }

    fn note_on(&mut self, note: u8, velocity: u8) {
        if !self.params.power || velocity == 0 {
            self.note_off(note);
            return;
        }
        let idx = self.alloc_voice(note);
        let unit = build_unit(
            self.params.shape,
            midi_to_hz(note),
            self.params.detune_cents,
            self.sample_rate,
        );
        self.age = self.age.wrapping_add(1);
        let age = self.age;
        let voice = &mut self.voices[idx];
        voice.active = true;
        voice.note = note;
        voice.velocity = f32::from(velocity) / 127.0;
        voice.stage = Stage::Attack;
        voice.env = 0.0;
        voice.age = age;
        voice.unit = unit;
    }

    fn note_off(&mut self, note: u8) {
        for voice in &mut self.voices {
            if voice.active && voice.note == note && voice.stage != Stage::Release {
                voice.stage = Stage::Release;
            }
        }
    }

    fn process_stereo(&mut self) -> (f32, f32) {
        if !self.params.power {
            return (0.0, 0.0);
        }

        let mut mix_l = 0.0f32;
        let mut mix_r = 0.0f32;
        for i in 0..MAX_VOICES {
            if !self.voices[i].active {
                continue;
            }
            let env = self.advance_env(i);
            if !self.voices[i].active {
                continue;
            }
            let sample = self.voices[i].unit.get_mono() * env * self.voices[i].velocity;
            mix_l += sample;
            mix_r += sample;
        }

        let res = 1.0 + self.params.resonance * 2.5;
        mix_l *= res;
        mix_r *= res;

        self.lp_z_l += self.lp_coeff * (mix_l - self.lp_z_l);
        self.lp_z_r += self.lp_coeff * (mix_r - self.lp_z_r);

        (
            (self.lp_z_l * self.output_gain).clamp(-1.5, 1.5),
            (self.lp_z_r * self.output_gain).clamp(-1.5, 1.5),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_renders_nonzero() {
        let mut dsp = Dsp::new(48_000.0);
        dsp.note_on(60, 100);
        let mut peak = 0.0f32;
        for _ in 0..2_000 {
            let (l, r) = dsp.process_stereo();
            peak = peak.max(l.abs()).max(r.abs());
        }
        assert!(peak > 0.01);
    }

    #[test]
    fn note_off_releases() {
        let mut dsp = Dsp::new(48_000.0);
        dsp.note_on(64, 90);
        for _ in 0..1_000 {
            let _ = dsp.process_stereo();
        }
        dsp.note_off(64);
        for _ in 0..20_000 {
            let _ = dsp.process_stereo();
        }
        let (l, r) = dsp.process_stereo();
        assert!(l.abs() < 0.05 && r.abs() < 0.05);
    }
}

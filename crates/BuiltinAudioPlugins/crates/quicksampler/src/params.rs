//! What a Quick Sampler plays its sample with.
//!
//! [`QuickSamplerParams`] is one plain `Copy` value: the track stores it, the
//! project file persists it field by field, the engine snapshot carries it,
//! and the audio thread takes a new one without allocating. Every field is
//! kept in range by [`QuickSamplerParams::sanitized`], so a hand-edited project
//! or a stray knob value can never reach the voices out of bounds.

use serde::{Deserialize, Serialize};

/// Most voices a sampler can be set to play at once. The pool is allocated
/// to this size when the sampler is built.
pub const MAX_POLYPHONY: u8 = 64;
/// Longest attack, decay or release, in milliseconds.
pub const ENVELOPE_MAX_MS: f32 = 10_000.0;
pub const CUTOFF_MIN_HZ: f32 = 20.0;
pub const CUTOFF_MAX_HZ: f32 = 20_000.0;
pub const TRANSPOSE_RANGE: i8 = 48;
pub const PITCH_BEND_MAX: u8 = 24;

/// How the sample repeats while a note is held.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LoopMode {
    /// Plays once, start to end.
    #[default]
    Off,
    /// Jumps from the loop end back to the loop start.
    Forward,
    /// Bounces between the loop points.
    PingPong,
}

impl LoopMode {
    pub const ALL: [LoopMode; 3] = [LoopMode::Off, LoopMode::Forward, LoopMode::PingPong];

    pub fn label(self) -> &'static str {
        match self {
            LoopMode::Off => "Off",
            LoopMode::Forward => "Loop",
            LoopMode::PingPong => "Ping-pong",
        }
    }

    /// Stable key the project file stores.
    pub fn key(self) -> &'static str {
        match self {
            LoopMode::Off => "off",
            LoopMode::Forward => "forward",
            LoopMode::PingPong => "pingpong",
        }
    }

    pub fn from_key(key: &str) -> Self {
        match key {
            "forward" => LoopMode::Forward,
            "pingpong" => LoopMode::PingPong,
            _ => LoopMode::Off,
        }
    }
}

/// The voice filter.
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
    pub const ALL: [FilterMode; 4] = [
        FilterMode::Off,
        FilterMode::LowPass,
        FilterMode::HighPass,
        FilterMode::BandPass,
    ];

    pub fn label(self) -> &'static str {
        match self {
            FilterMode::Off => "Off",
            FilterMode::LowPass => "LP",
            FilterMode::HighPass => "HP",
            FilterMode::BandPass => "BP",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            FilterMode::Off => "off",
            FilterMode::LowPass => "lowpass",
            FilterMode::HighPass => "highpass",
            FilterMode::BandPass => "bandpass",
        }
    }

    pub fn from_key(key: &str) -> Self {
        match key {
            "lowpass" => FilterMode::LowPass,
            "highpass" => FilterMode::HighPass,
            "bandpass" => FilterMode::BandPass,
            _ => FilterMode::Off,
        }
    }
}

/// Everything about how the sample plays, apart from which sample it is.
///
/// Positions (`start`, `end`, `loop_start`, `loop_end`) are fractions of the
/// sample's length, so they stay on the same audio when the project runs at
/// another sample rate.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct QuickSamplerParams {
    /// The key the sample plays at its own pitch.
    pub root_note: u8,
    /// Semitones added to every note.
    pub transpose: i8,
    /// Cents added to every note, -100..=100.
    pub fine_cents: f32,
    /// Off: every key plays the sample at its own pitch (a one-shot or a drum
    /// hit), and the keys only choose when it plays.
    pub keytrack: bool,
    /// Where playback starts, 0..=1 of the sample.
    pub start: f32,
    /// Where playback stops, 0..=1 of the sample. Never before `start`.
    pub end: f32,
    pub reverse: bool,
    pub loop_mode: LoopMode,
    pub loop_start: f32,
    pub loop_end: f32,
    /// Plays the sample at full scale, whatever level it was recorded at.
    pub normalize: bool,
    pub attack_ms: f32,
    pub decay_ms: f32,
    /// Sustain level, 0..=1.
    pub sustain: f32,
    pub release_ms: f32,
    pub filter: FilterMode,
    pub cutoff_hz: f32,
    /// 0..=1; 1 is just short of self-oscillation.
    pub resonance: f32,
    /// Output level, 0..=1 of unity gain.
    pub volume: f32,
    /// -1 (left) ..= 1 (right).
    pub pan: f32,
    /// How much note velocity sets the level, 0..=1. 0 plays every note at
    /// full level.
    pub velocity: f32,
    /// Voices that sound at once, 1..=[`MAX_POLYPHONY`]. 1 is mono: a new
    /// note takes over from the one sounding.
    pub polyphony: u8,
    /// Pitch-bend range, semitones either way.
    pub pitch_bend_range: u8,
}

impl Default for QuickSamplerParams {
    fn default() -> Self {
        Self {
            root_note: 60,
            transpose: 0,
            fine_cents: 0.0,
            keytrack: true,
            start: 0.0,
            end: 1.0,
            reverse: false,
            loop_mode: LoopMode::Off,
            loop_start: 0.0,
            loop_end: 1.0,
            normalize: false,
            attack_ms: 0.0,
            decay_ms: 0.0,
            sustain: 1.0,
            release_ms: 30.0,
            filter: FilterMode::Off,
            cutoff_hz: CUTOFF_MAX_HZ,
            resonance: 0.0,
            volume: 0.8,
            pan: 0.0,
            velocity: 1.0,
            polyphony: 16,
            pitch_bend_range: 2,
        }
    }
}

fn finite_or(value: f32, fallback: f32) -> f32 {
    if value.is_finite() { value } else { fallback }
}

impl QuickSamplerParams {
    /// Every field in range. Idempotent.
    pub fn sanitized(self) -> Self {
        let d = Self::default();
        let unit = |value: f32, fallback: f32| finite_or(value, fallback).clamp(0.0, 1.0);
        let ms = |value: f32, fallback: f32| finite_or(value, fallback).clamp(0.0, ENVELOPE_MAX_MS);
        let start = unit(self.start, d.start);
        let end = unit(self.end, d.end).max(start);
        let loop_start = unit(self.loop_start, d.loop_start);
        let loop_end = unit(self.loop_end, d.loop_end).max(loop_start);
        Self {
            root_note: self.root_note.min(127),
            transpose: self.transpose.clamp(-TRANSPOSE_RANGE, TRANSPOSE_RANGE),
            fine_cents: finite_or(self.fine_cents, 0.0).clamp(-100.0, 100.0),
            keytrack: self.keytrack,
            start,
            end,
            reverse: self.reverse,
            loop_mode: self.loop_mode,
            loop_start,
            loop_end,
            normalize: self.normalize,
            attack_ms: ms(self.attack_ms, d.attack_ms),
            decay_ms: ms(self.decay_ms, d.decay_ms),
            sustain: unit(self.sustain, d.sustain),
            release_ms: ms(self.release_ms, d.release_ms),
            filter: self.filter,
            cutoff_hz: finite_or(self.cutoff_hz, d.cutoff_hz).clamp(CUTOFF_MIN_HZ, CUTOFF_MAX_HZ),
            resonance: unit(self.resonance, d.resonance),
            volume: unit(self.volume, d.volume),
            pan: finite_or(self.pan, 0.0).clamp(-1.0, 1.0),
            velocity: unit(self.velocity, d.velocity),
            polyphony: self.polyphony.clamp(1, MAX_POLYPHONY),
            pitch_bend_range: self.pitch_bend_range.min(PITCH_BEND_MAX),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizing_keeps_every_field_in_range() {
        let wild = QuickSamplerParams {
            root_note: 200,
            transpose: 120,
            fine_cents: f32::NAN,
            start: 0.7,
            end: 0.2,
            loop_start: 2.0,
            loop_end: -1.0,
            attack_ms: -5.0,
            cutoff_hz: 1.0e9,
            polyphony: 0,
            pitch_bend_range: 99,
            ..QuickSamplerParams::default()
        }
        .sanitized();
        assert_eq!(wild.root_note, 127);
        assert_eq!(wild.transpose, TRANSPOSE_RANGE);
        assert_eq!(wild.fine_cents, 0.0);
        assert_eq!((wild.start, wild.end), (0.7, 0.7));
        assert_eq!((wild.loop_start, wild.loop_end), (1.0, 1.0));
        assert_eq!(wild.attack_ms, 0.0);
        assert_eq!(wild.cutoff_hz, CUTOFF_MAX_HZ);
        assert_eq!(wild.polyphony, 1);
        assert_eq!(wild.pitch_bend_range, PITCH_BEND_MAX);
        assert_eq!(wild.sanitized(), wild);
    }

    #[test]
    fn mode_keys_round_trip() {
        for mode in LoopMode::ALL {
            assert_eq!(LoopMode::from_key(mode.key()), mode);
        }
        for mode in FilterMode::ALL {
            assert_eq!(FilterMode::from_key(mode.key()), mode);
        }
        assert_eq!(LoopMode::from_key("??"), LoopMode::Off);
    }
}

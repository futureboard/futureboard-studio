//! WrapSynth's LFOs: six shapes, free-running in Hz or locked to the
//! transport tempo by note division.
//!
//! Each voice keeps its own phase. With retrigger on, a note starts its LFO
//! from the top of the cycle; off, it joins the synth's free-running phase,
//! so every voice moves together.

use serde::{Deserialize, Serialize};

/// Slowest and fastest free-running rates.
pub const MIN_RATE_HZ: f32 = 0.01;
pub const MAX_RATE_HZ: f32 = 30.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum LfoShape {
    #[default]
    Sine,
    Triangle,
    SawUp,
    SawDown,
    Square,
    /// A new random value at the top of every cycle.
    SampleHold,
}

impl LfoShape {
    pub const ALL: [LfoShape; 6] = [
        Self::Sine,
        Self::Triangle,
        Self::SawUp,
        Self::SawDown,
        Self::Square,
        Self::SampleHold,
    ];

    pub fn to_wire(self) -> f32 {
        self as usize as f32
    }

    pub fn from_wire(value: f32) -> Self {
        Self::ALL[value.round().clamp(0.0, (Self::ALL.len() - 1) as f32) as usize]
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Sine => "Sine",
            Self::Triangle => "Tri",
            Self::SawUp => "Saw ↑",
            Self::SawDown => "Saw ↓",
            Self::Square => "Square",
            Self::SampleHold => "S&H",
        }
    }

    /// The shape at `phase` (0..1), −1..1. `held` is the sample-and-hold
    /// value for this cycle.
    #[inline]
    pub fn at(self, phase: f32, held: f32) -> f32 {
        match self {
            Self::Sine => (std::f32::consts::TAU * phase).sin(),
            // 0 at the start, rising: +1 a quarter in, −1 three quarters in.
            Self::Triangle => 1.0 - 4.0 * ((phase + 0.25).rem_euclid(1.0) - 0.5).abs(),
            Self::SawUp => 2.0 * phase - 1.0,
            Self::SawDown => 1.0 - 2.0 * phase,
            Self::Square => {
                if phase < 0.5 {
                    1.0
                } else {
                    -1.0
                }
            }
            Self::SampleHold => held,
        }
    }
}

/// Cycles per second for a free rate, or for `division_beats` quarter notes
/// at `tempo_bpm` when synced.
#[inline]
pub fn rate_hz(rate_hz: f32, sync: bool, division_beats: f32, tempo_bpm: f32) -> f32 {
    if sync {
        let beats = division_beats.max(1.0e-3);
        (tempo_bpm.max(1.0) / 60.0) / beats
    } else {
        rate_hz.clamp(MIN_RATE_HZ, MAX_RATE_HZ)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_shape_is_bipolar_and_starts_where_expected() {
        for shape in LfoShape::ALL {
            for i in 0..100 {
                let value = shape.at(i as f32 / 100.0, 0.3);
                assert!((-1.0..=1.0).contains(&value), "{shape:?}: {value}");
            }
        }
        assert!(LfoShape::Sine.at(0.0, 0.0).abs() < 1.0e-6);
        assert!((LfoShape::Triangle.at(0.25, 0.0) - 1.0).abs() < 1.0e-5);
        assert!((LfoShape::Triangle.at(0.75, 0.0) + 1.0).abs() < 1.0e-5);
        assert_eq!(LfoShape::SampleHold.at(0.6, -0.4), -0.4);
        for shape in LfoShape::ALL {
            assert_eq!(LfoShape::from_wire(shape.to_wire()), shape);
        }
    }

    #[test]
    fn a_synced_rate_follows_the_tempo() {
        // A quarter note at 120 BPM: twice a second.
        assert!((rate_hz(5.0, true, 1.0, 120.0) - 2.0).abs() < 1.0e-6);
        assert_eq!(rate_hz(500.0, false, 1.0, 120.0), MAX_RATE_HZ);
    }
}

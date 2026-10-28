//! Keys, scales, and hysteresis-stabilized target pitch resolution.
//! Borrowed from `Futureboard-release/crates/BuiltinAudioPlugins/crates/whitesharp/src/scale.rs`.

use serde::{Deserialize, Serialize};

pub const NOTE_NAMES: [&str; 12] = [
    "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
];

pub const ALL_NOTES: u16 = 0x0FFF;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Scale {
    Chromatic,
    Major,
    Minor,
    HarmonicMinor,
    MelodicMinor,
    MajorPentatonic,
    MinorPentatonic,
    Blues,
    Dorian,
    Phrygian,
    Lydian,
    Mixolydian,
}

impl Scale {
    pub const ALL: [Self; 12] = [
        Self::Chromatic,
        Self::Major,
        Self::Minor,
        Self::HarmonicMinor,
        Self::MelodicMinor,
        Self::MajorPentatonic,
        Self::MinorPentatonic,
        Self::Blues,
        Self::Dorian,
        Self::Phrygian,
        Self::Lydian,
        Self::Mixolydian,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Chromatic => "Chromatic",
            Self::Major => "Major",
            Self::Minor => "Minor",
            Self::HarmonicMinor => "Harmonic Minor",
            Self::MelodicMinor => "Melodic Minor",
            Self::MajorPentatonic => "Major Pentatonic",
            Self::MinorPentatonic => "Minor Pentatonic",
            Self::Blues => "Blues",
            Self::Dorian => "Dorian",
            Self::Phrygian => "Phrygian",
            Self::Lydian => "Lydian",
            Self::Mixolydian => "Mixolydian",
        }
    }

    pub const fn steps(self) -> &'static [u8] {
        match self {
            Self::Chromatic => &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11],
            Self::Major => &[0, 2, 4, 5, 7, 9, 11],
            Self::Minor => &[0, 2, 3, 5, 7, 8, 10],
            Self::HarmonicMinor => &[0, 2, 3, 5, 7, 8, 11],
            Self::MelodicMinor => &[0, 2, 3, 5, 7, 9, 11],
            Self::MajorPentatonic => &[0, 2, 4, 7, 9],
            Self::MinorPentatonic => &[0, 3, 5, 7, 10],
            Self::Blues => &[0, 3, 5, 6, 7, 10],
            Self::Dorian => &[0, 2, 3, 5, 7, 9, 10],
            Self::Phrygian => &[0, 1, 3, 5, 7, 8, 10],
            Self::Lydian => &[0, 2, 4, 6, 7, 9, 11],
            Self::Mixolydian => &[0, 2, 4, 5, 7, 9, 10],
        }
    }

    pub const fn to_wire(self) -> f32 {
        self as u8 as f32
    }

    pub fn from_wire(value: f32) -> Self {
        Self::ALL[(value.round().max(0.0) as usize).min(Self::ALL.len() - 1)]
    }
}

pub fn scale_mask(key: u8, scale: Scale) -> u16 {
    scale.steps().iter().fold(0, |mask, step| {
        mask | 1 << ((key as u16 + *step as u16) % 12)
    })
}

#[inline]
pub fn pitch_class(note: i32) -> u16 {
    note.rem_euclid(12) as u16
}

pub const HYSTERESIS: f32 = 0.15;

pub fn nearest_target(midi: f32, allowed: u16, current: Option<i32>) -> Option<i32> {
    if allowed & ALL_NOTES == 0 || !midi.is_finite() {
        return None;
    }
    let centre = midi.round() as i32;
    let mut best: Option<(i32, f32)> = None;
    for note in centre - 12..=centre + 12 {
        if allowed & (1 << pitch_class(note)) == 0 {
            continue;
        }
        let distance = (midi - note as f32).abs();
        if best.is_none_or(|(_, d)| distance < d) {
            best = Some((note, distance));
        }
    }
    let (nearest, distance) = best?;
    match current {
        Some(held)
            if held != nearest
                && allowed & (1 << pitch_class(held)) != 0
                && (midi - held as f32).abs() < distance + HYSTERESIS =>
        {
            Some(held)
        }
        _ => Some(nearest),
    }
}

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

    /// Stable name for saved analyses and the apps. The minor pentatonic is
    /// `penta`, its name from before the other scales could be detected.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Chromatic => "chromatic",
            Self::Major => "major",
            Self::Minor => "minor",
            Self::HarmonicMinor => "harmonicMinor",
            Self::MelodicMinor => "melodicMinor",
            Self::MajorPentatonic => "majorPentatonic",
            Self::MinorPentatonic => "penta",
            Self::Blues => "blues",
            Self::Dorian => "dorian",
            Self::Phrygian => "phrygian",
            Self::Lydian => "lydian",
            Self::Mixolydian => "mixolydian",
        }
    }

    /// The scale a [`Self::name`] stands for (also `minorPentatonic`).
    pub fn from_name(name: &str) -> Option<Self> {
        if name == "minorPentatonic" {
            return Some(Self::MinorPentatonic);
        }
        Self::ALL.into_iter().find(|s| s.name() == name)
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

/// A mode wins over plain major / minor only when it explains at least this
/// much more of the sung material: a few stray frames on a raised fourth do
/// not make a take Lydian.
const MODE_MARGIN: f32 = 0.03;

/// The two degrees a pentatonic leaves out of its parent scale may carry at
/// most this much of the material together.
const PENTATONIC_GAP: f32 = 0.015;

/// A pentatonic note counts as sung from this share of the material on.
const PENTATONIC_NOTE: f32 = 0.03;

/// Refines a major / minor key estimate into the scale the take actually
/// uses. `chroma` is the sung material per pitch class (any scale), `key`
/// the tonic and `base` the major / minor verdict.
///
/// From the tonic, every candidate is scored by how much of the material
/// its notes cover. Major takes may turn Lydian or Mixolydian, minor takes
/// harmonic or melodic minor, Dorian, Phrygian or Blues, when that covers
/// clearly more (see [`MODE_MARGIN`]); a take that never sings the notes a
/// pentatonic leaves out becomes that pentatonic.
pub fn refine_scale(chroma: &[f32; 12], key: u8, base: Scale) -> Scale {
    let total: f32 = chroma.iter().sum();
    if total <= 1e-6 {
        return base;
    }
    let share = |degree: u8| chroma[((key + degree) % 12) as usize] / total;
    let covers = |scale: Scale| scale.steps().iter().map(|&d| share(d)).sum::<f32>();

    let major = base != Scale::Minor;
    let (plain, modes): (Scale, &[Scale]) = if major {
        (Scale::Major, &[Scale::Lydian, Scale::Mixolydian])
    } else {
        (
            Scale::Minor,
            &[
                Scale::HarmonicMinor,
                Scale::MelodicMinor,
                Scale::Dorian,
                Scale::Phrygian,
                Scale::Blues,
            ],
        )
    };

    let plain_cover = covers(plain);
    let best_mode = modes
        .iter()
        .copied()
        .map(|s| (s, covers(s)))
        .max_by(|a, b| a.1.total_cmp(&b.1));
    if let Some((mode, cover)) = best_mode
        && cover > plain_cover + MODE_MARGIN
    {
        return mode;
    }

    // Pentatonic: the parent scale's fourth and seventh (major), or second
    // and minor sixth (minor), are never really sung, while the pentatonic's
    // own notes are: a take of two or three pitches says nothing either way.
    let (left_out, pentatonic) = if major {
        (share(5) + share(11), Scale::MajorPentatonic)
    } else {
        (share(2) + share(8), Scale::MinorPentatonic)
    };
    let sung_notes = pentatonic
        .steps()
        .iter()
        .filter(|&&d| share(d) >= PENTATONIC_NOTE)
        .count();
    if left_out < PENTATONIC_GAP && sung_notes >= 4 {
        return if major {
            Scale::MajorPentatonic
        } else {
            Scale::MinorPentatonic
        };
    }
    plain
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Material on the given degrees of `key`, as (degree, weight) pairs.
    fn sung(key: u8, degrees: &[(u8, f32)]) -> [f32; 12] {
        let mut chroma = [0.0; 12];
        for &(d, w) in degrees {
            chroma[((key + d) % 12) as usize] += w;
        }
        chroma
    }

    #[test]
    fn every_scale_keeps_its_name() {
        for s in Scale::ALL {
            assert_eq!(Scale::from_name(s.name()), Some(s));
        }
        assert_eq!(Scale::from_name("penta"), Some(Scale::MinorPentatonic));
        assert_eq!(
            Scale::from_name("minorPentatonic"),
            Some(Scale::MinorPentatonic)
        );
        assert_eq!(Scale::from_name("nonsense"), None);
    }

    #[test]
    fn a_plain_major_or_minor_take_stays_plain() {
        let major = sung(
            2,
            &[
                (0, 5.),
                (2, 3.),
                (4, 4.),
                (5, 2.),
                (7, 5.),
                (9, 2.),
                (11, 2.),
            ],
        );
        assert_eq!(refine_scale(&major, 2, Scale::Major), Scale::Major);
        let minor = sung(
            9,
            &[
                (0, 5.),
                (2, 2.),
                (3, 4.),
                (5, 2.),
                (7, 5.),
                (8, 2.),
                (10, 2.),
            ],
        );
        assert_eq!(refine_scale(&minor, 9, Scale::Minor), Scale::Minor);
    }

    #[test]
    fn a_few_stray_frames_do_not_change_the_scale() {
        // 2% on the raised fourth, the natural fourth sung too: still major.
        let major = sung(
            0,
            &[
                (0, 5.),
                (2, 3.),
                (4, 4.),
                (5, 2.),
                (6, 0.4),
                (7, 5.),
                (9, 2.),
                (11, 2.),
            ],
        );
        assert_eq!(refine_scale(&major, 0, Scale::Major), Scale::Major);
    }

    #[test]
    fn modes_are_told_apart_by_the_notes_they_change() {
        // Raised fourth instead of the fourth.
        let lydian = sung(
            5,
            &[
                (0, 5.),
                (2, 3.),
                (4, 4.),
                (6, 3.),
                (7, 5.),
                (9, 2.),
                (11, 2.),
            ],
        );
        assert_eq!(refine_scale(&lydian, 5, Scale::Major), Scale::Lydian);
        // Flat seventh instead of the leading note.
        let mixo = sung(
            7,
            &[
                (0, 5.),
                (2, 3.),
                (4, 4.),
                (5, 2.),
                (7, 5.),
                (9, 2.),
                (10, 3.),
            ],
        );
        assert_eq!(refine_scale(&mixo, 7, Scale::Major), Scale::Mixolydian);
        // Minor sixth with the raised seventh.
        let harmonic = sung(
            9,
            &[
                (0, 5.),
                (2, 2.),
                (3, 4.),
                (5, 2.),
                (7, 5.),
                (8, 3.),
                (11, 3.),
            ],
        );
        assert_eq!(
            refine_scale(&harmonic, 9, Scale::Minor),
            Scale::HarmonicMinor
        );
        // Raised sixth and seventh.
        let melodic = sung(
            9,
            &[
                (0, 5.),
                (2, 2.),
                (3, 4.),
                (5, 2.),
                (7, 5.),
                (9, 3.),
                (11, 3.),
            ],
        );
        assert_eq!(refine_scale(&melodic, 9, Scale::Minor), Scale::MelodicMinor);
        // Raised sixth with the flat seventh.
        let dorian = sung(
            2,
            &[
                (0, 5.),
                (2, 2.),
                (3, 4.),
                (5, 2.),
                (7, 5.),
                (9, 3.),
                (10, 3.),
            ],
        );
        assert_eq!(refine_scale(&dorian, 2, Scale::Minor), Scale::Dorian);
        // Flat second.
        let phrygian = sung(
            4,
            &[
                (0, 5.),
                (1, 3.),
                (3, 4.),
                (5, 2.),
                (7, 5.),
                (8, 2.),
                (10, 2.),
            ],
        );
        assert_eq!(refine_scale(&phrygian, 4, Scale::Minor), Scale::Phrygian);
        // Minor pentatonic plus the flat fifth.
        let blues = sung(9, &[(0, 5.), (3, 4.), (5, 3.), (6, 3.), (7, 5.), (10, 3.)]);
        assert_eq!(refine_scale(&blues, 9, Scale::Minor), Scale::Blues);
    }

    #[test]
    fn a_take_that_never_sings_the_left_out_notes_is_pentatonic() {
        let major = sung(7, &[(0, 5.), (2, 3.), (4, 4.), (7, 5.), (9, 3.)]);
        assert_eq!(
            refine_scale(&major, 7, Scale::Major),
            Scale::MajorPentatonic
        );
        let minor = sung(4, &[(0, 5.), (3, 4.), (5, 3.), (7, 5.), (10, 3.)]);
        assert_eq!(
            refine_scale(&minor, 4, Scale::Minor),
            Scale::MinorPentatonic
        );
    }

    #[test]
    fn a_take_of_a_few_pitches_is_not_called_pentatonic() {
        let two = sung(9, &[(0, 5.), (3, 4.)]);
        assert_eq!(refine_scale(&two, 9, Scale::Minor), Scale::Minor);
        let three = sung(0, &[(0, 5.), (4, 4.), (7, 3.)]);
        assert_eq!(refine_scale(&three, 0, Scale::Major), Scale::Major);
    }

    #[test]
    fn nothing_sung_keeps_the_estimate() {
        assert_eq!(refine_scale(&[0.0; 12], 0, Scale::Major), Scale::Major);
    }
}

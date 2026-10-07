//! What a Slicer plays: where its slices are, which keys play them, and how.
//!
//! [`SlicerParams`] is one plain `Copy` value, so the audio thread takes a
//! wire edit into it without allocating. The slices are their start points,
//! as fractions of the sample; each one plays up to the next, the last one to
//! the end of the sample. The editor keeps them in order; nothing on the audio
//! thread sorts them.

use serde::{Deserialize, Serialize};

use quicksampler::QuickSamplerParams;

/// Most slices one sample is cut into — and keys they take, from
/// [`SlicerParams::first_key`] up.
pub const MAX_SLICES: usize = 64;
/// C3 (as Futureboard names keys, middle C being C4): where the slices start.
pub const DEFAULT_FIRST_KEY: u8 = 48;
pub const MIN_SAMPLE_BPM: f32 = 40.0;
pub const MAX_SAMPLE_BPM: f32 = 300.0;
pub const MIN_EQUAL_SLICES: u8 = 2;

/// How the editor cuts a sample when asked to slice it again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SliceMode {
    /// At the hits: wherever the audio jumps.
    #[default]
    Transient,
    /// On the beat grid of the sample's own tempo.
    Beat,
    /// Into equal lengths.
    Equal,
}

impl SliceMode {
    pub const ALL: [SliceMode; 3] = [SliceMode::Transient, SliceMode::Beat, SliceMode::Equal];

    pub fn label(self) -> &'static str {
        match self {
            SliceMode::Transient => "Transient",
            SliceMode::Beat => "Beat",
            SliceMode::Equal => "Equal",
        }
    }
}

/// The note value one slice lasts in [`SliceMode::Beat`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BeatDivision {
    Quarter,
    #[default]
    Eighth,
    Sixteenth,
}

impl BeatDivision {
    pub const ALL: [BeatDivision; 3] = [
        BeatDivision::Quarter,
        BeatDivision::Eighth,
        BeatDivision::Sixteenth,
    ];

    pub fn label(self) -> &'static str {
        match self {
            BeatDivision::Quarter => "1/4",
            BeatDivision::Eighth => "1/8",
            BeatDivision::Sixteenth => "1/16",
        }
    }

    /// Slices per beat (a quarter note).
    pub fn per_beat(self) -> f32 {
        match self {
            BeatDivision::Quarter => 1.0,
            BeatDivision::Eighth => 2.0,
            BeatDivision::Sixteenth => 4.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SlicerParams {
    /// How every slice plays: transpose, fine tune, direction, envelope,
    /// filter and output. Its region, loop, root key and key tracking are
    /// not used — a slice is its own region, never loops, and plays at the
    /// sample's pitch on whichever key it sits.
    pub voice: QuickSamplerParams,
    /// The key that plays the first slice; the next key plays the next.
    pub first_key: u8,
    /// A new slice cuts off whatever is sounding, as hits in one loop do.
    pub choke: bool,
    /// A slice plays to its end whatever the note length; off, the note-off
    /// releases it.
    pub one_shot: bool,
    pub slice_mode: SliceMode,
    /// Transient detection, 0..=1: higher finds quieter hits.
    pub sensitivity: f32,
    /// Slices for [`SliceMode::Equal`].
    pub equal_slices: u8,
    pub beat_division: BeatDivision,
    /// The sample's own tempo, for [`SliceMode::Beat`]; 0 until measured.
    pub sample_bpm: f32,
    /// How many of `slices` are in use.
    pub slice_count: u8,
    /// Slice start points, 0..=1 of the sample, in order. Entries past
    /// `slice_count` are unused.
    #[serde(with = "slice_points")]
    pub slices: [f32; MAX_SLICES],
}

impl Default for SlicerParams {
    fn default() -> Self {
        let mut slices = [1.0; MAX_SLICES];
        slices[0] = 0.0;
        Self {
            voice: QuickSamplerParams {
                keytrack: false,
                ..QuickSamplerParams::default()
            },
            first_key: DEFAULT_FIRST_KEY,
            choke: true,
            one_shot: true,
            slice_mode: SliceMode::Transient,
            sensitivity: 0.5,
            equal_slices: 8,
            beat_division: BeatDivision::Eighth,
            sample_bpm: 0.0,
            slice_count: 1,
            slices,
        }
    }
}

fn unit(value: f32, fallback: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        fallback
    }
}

/// 0 (not measured) or a tempo in range.
pub fn sanitize_bpm(bpm: f32) -> f32 {
    if bpm.is_finite() && bpm > 0.0 {
        bpm.clamp(MIN_SAMPLE_BPM, MAX_SAMPLE_BPM)
    } else {
        0.0
    }
}

impl SlicerParams {
    /// Every field in range. Idempotent, and cheap enough for the audio
    /// thread: it clamps, it never reorders the slices.
    pub fn sanitized(self) -> Self {
        let d = Self::default();
        let mut slices = self.slices;
        for point in slices.iter_mut() {
            *point = unit(*point, 1.0);
        }
        Self {
            voice: self.voice.sanitized(),
            first_key: self.first_key.min(127),
            choke: self.choke,
            one_shot: self.one_shot,
            slice_mode: self.slice_mode,
            sensitivity: unit(self.sensitivity, d.sensitivity),
            equal_slices: self.equal_slices.clamp(MIN_EQUAL_SLICES, MAX_SLICES as u8),
            beat_division: self.beat_division,
            sample_bpm: sanitize_bpm(self.sample_bpm),
            slice_count: self.slice_count.min(MAX_SLICES as u8),
            slices,
        }
    }

    /// The slices in use.
    pub fn points(&self) -> &[f32] {
        &self.slices[..self.slice_count as usize]
    }

    /// `(start, end)` of slice `index`, as fractions of the sample.
    pub fn slice_region(&self, index: usize) -> Option<(f32, f32)> {
        let points = self.points();
        let start = *points.get(index)?;
        let end = points.get(index + 1).copied().unwrap_or(1.0);
        Some((start, end.max(start)))
    }

    /// The slice `note` plays, if any.
    pub fn slice_for_note(&self, note: u8) -> Option<usize> {
        let index = note.checked_sub(self.first_key)? as usize;
        (index < self.slice_count as usize).then_some(index)
    }

    /// The key that plays slice `index`, if it is on the keyboard.
    pub fn note_for_slice(&self, index: usize) -> Option<u8> {
        let note = self.first_key as usize + index;
        (note <= 127).then_some(note as u8)
    }
}

/// The slice points travel as a list (serde has no arrays this long); a
/// shorter list leaves the rest unused, a longer one is cut.
mod slice_points {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::MAX_SLICES;

    pub fn serialize<S: Serializer>(
        points: &[f32; MAX_SLICES],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        points.as_slice().serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<[f32; MAX_SLICES], D::Error> {
        let list = Vec::<f32>::deserialize(deserializer)?;
        let mut points = [1.0; MAX_SLICES];
        for (point, value) in points.iter_mut().zip(list) {
            *point = value;
        }
        Ok(points)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slices_play_up_to_the_next_and_the_last_to_the_end() {
        let mut p = SlicerParams {
            slice_count: 3,
            ..SlicerParams::default()
        };
        p.slices[..3].copy_from_slice(&[0.0, 0.25, 0.6]);
        assert_eq!(p.slice_region(0), Some((0.0, 0.25)));
        assert_eq!(p.slice_region(2), Some((0.6, 1.0)));
        assert_eq!(p.slice_region(3), None);
        assert_eq!(p.slice_for_note(DEFAULT_FIRST_KEY + 1), Some(1));
        assert_eq!(p.slice_for_note(DEFAULT_FIRST_KEY + 3), None);
        assert_eq!(p.slice_for_note(DEFAULT_FIRST_KEY - 1), None);
    }

    #[test]
    fn sanitizing_clamps_without_reordering() {
        let mut p = SlicerParams {
            slice_count: 200,
            sensitivity: f32::NAN,
            equal_slices: 0,
            sample_bpm: 1_000.0,
            first_key: 200,
            ..SlicerParams::default()
        };
        p.slices[0] = 0.5;
        p.slices[1] = -1.0;
        p.slices[2] = f32::INFINITY;
        let s = p.sanitized();
        assert_eq!(s.slice_count as usize, MAX_SLICES);
        assert_eq!(&s.slices[..3], &[0.5, 0.0, 1.0]);
        assert_eq!(s.sensitivity, 0.5);
        assert_eq!(s.equal_slices, MIN_EQUAL_SLICES);
        assert_eq!(s.sample_bpm, MAX_SAMPLE_BPM);
        assert_eq!(s.first_key, 127);
        assert_eq!(s.sanitized(), s);
    }
}

//! Scale-aware pitch editing model.
//!
//! Pure data: a root note + a set of semitone intervals, plus a small runtime
//! toggle ([`PitchTransformContext`]) that the piano-roll editor consults when
//! constraining a drag or snapping notes to the nearest in-scale pitch. This
//! module has no dependency on `MidiNoteState` or any editor/UI type so it can
//! be reused by the Drum Editor / Tracker modes later.

/// The 12 chromatic root notes a scale can be built on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScaleRoot {
    C,
    CSharp,
    D,
    DSharp,
    E,
    F,
    FSharp,
    G,
    GSharp,
    A,
    ASharp,
    B,
}

impl ScaleRoot {
    pub const ALL: [ScaleRoot; 12] = [
        ScaleRoot::C,
        ScaleRoot::CSharp,
        ScaleRoot::D,
        ScaleRoot::DSharp,
        ScaleRoot::E,
        ScaleRoot::F,
        ScaleRoot::FSharp,
        ScaleRoot::G,
        ScaleRoot::GSharp,
        ScaleRoot::A,
        ScaleRoot::ASharp,
        ScaleRoot::B,
    ];

    /// Pitch class 0..=11 (C = 0), matching MIDI pitch % 12.
    pub fn pitch_class(self) -> u8 {
        self as u8
    }

    pub fn label(self) -> &'static str {
        match self {
            ScaleRoot::C => "C",
            ScaleRoot::CSharp => "C#",
            ScaleRoot::D => "D",
            ScaleRoot::DSharp => "D#",
            ScaleRoot::E => "E",
            ScaleRoot::F => "F",
            ScaleRoot::FSharp => "F#",
            ScaleRoot::G => "G",
            ScaleRoot::GSharp => "G#",
            ScaleRoot::A => "A",
            ScaleRoot::ASharp => "A#",
            ScaleRoot::B => "B",
        }
    }

    pub fn cycle(self) -> Self {
        Self::ALL[(self.pitch_class() as usize + 1) % 12]
    }

    /// Root for a pitch class; any integer folds into `0..12`.
    pub fn from_pitch_class(pitch_class: u8) -> Self {
        Self::ALL[(pitch_class % 12) as usize]
    }
}

/// Scale (interval set) choices. `Chromatic` disables constraint semantics —
/// every pitch is "in scale".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScaleKind {
    Chromatic,
    Major,
    NaturalMinor,
    HarmonicMinor,
    MelodicMinor,
    MajorPentatonic,
    MinorPentatonic,
    Dorian,
    Phrygian,
    Lydian,
    Mixolydian,
    Locrian,
}

impl ScaleKind {
    pub const ALL: [ScaleKind; 12] = [
        ScaleKind::Chromatic,
        ScaleKind::Major,
        ScaleKind::NaturalMinor,
        ScaleKind::HarmonicMinor,
        ScaleKind::MelodicMinor,
        ScaleKind::MajorPentatonic,
        ScaleKind::MinorPentatonic,
        ScaleKind::Dorian,
        ScaleKind::Phrygian,
        ScaleKind::Lydian,
        ScaleKind::Mixolydian,
        ScaleKind::Locrian,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ScaleKind::Chromatic => "Chromatic",
            ScaleKind::Major => "Major",
            ScaleKind::NaturalMinor => "Natural Minor",
            ScaleKind::HarmonicMinor => "Harmonic Minor",
            ScaleKind::MelodicMinor => "Melodic Minor",
            ScaleKind::MajorPentatonic => "Major Pentatonic",
            ScaleKind::MinorPentatonic => "Minor Pentatonic",
            ScaleKind::Dorian => "Dorian",
            ScaleKind::Phrygian => "Phrygian",
            ScaleKind::Lydian => "Lydian",
            ScaleKind::Mixolydian => "Mixolydian",
            ScaleKind::Locrian => "Locrian",
        }
    }

    /// Semitone offsets from the root, within one octave (ascending form for
    /// melodic minor — the descending form is a separate scale in strict
    /// theory, out of scope here).
    pub fn intervals(self) -> &'static [u8] {
        match self {
            ScaleKind::Chromatic => &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11],
            ScaleKind::Major => &[0, 2, 4, 5, 7, 9, 11],
            ScaleKind::NaturalMinor => &[0, 2, 3, 5, 7, 8, 10],
            ScaleKind::HarmonicMinor => &[0, 2, 3, 5, 7, 8, 11],
            ScaleKind::MelodicMinor => &[0, 2, 3, 5, 7, 9, 11],
            ScaleKind::MajorPentatonic => &[0, 2, 4, 7, 9],
            ScaleKind::MinorPentatonic => &[0, 3, 5, 7, 10],
            ScaleKind::Dorian => &[0, 2, 3, 5, 7, 9, 10],
            ScaleKind::Phrygian => &[0, 1, 3, 5, 7, 8, 10],
            ScaleKind::Lydian => &[0, 2, 4, 6, 7, 9, 11],
            ScaleKind::Mixolydian => &[0, 2, 4, 5, 7, 9, 10],
            ScaleKind::Locrian => &[0, 1, 3, 5, 6, 8, 10],
        }
    }

    pub fn cycle(self) -> Self {
        let idx = Self::ALL.iter().position(|k| *k == self).unwrap_or(0);
        Self::ALL[(idx + 1) % Self::ALL.len()]
    }

    /// Compact name for tight readouts ("maj", "min", "Dorian").
    pub fn short_label(self) -> &'static str {
        match self {
            ScaleKind::Chromatic => "chrom",
            ScaleKind::Major => "maj",
            ScaleKind::NaturalMinor => "min",
            ScaleKind::HarmonicMinor => "harm min",
            ScaleKind::MelodicMinor => "mel min",
            ScaleKind::MajorPentatonic => "maj pent",
            ScaleKind::MinorPentatonic => "min pent",
            ScaleKind::Dorian => "Dorian",
            ScaleKind::Phrygian => "Phrygian",
            ScaleKind::Lydian => "Lydian",
            ScaleKind::Mixolydian => "Mixolydian",
            ScaleKind::Locrian => "Locrian",
        }
    }

    /// Stable on-disk tag. Never renumber: projects store it.
    pub fn to_tag(self) -> u8 {
        match self {
            ScaleKind::Chromatic => 0,
            ScaleKind::Major => 1,
            ScaleKind::NaturalMinor => 2,
            ScaleKind::HarmonicMinor => 3,
            ScaleKind::MelodicMinor => 4,
            ScaleKind::MajorPentatonic => 5,
            ScaleKind::MinorPentatonic => 6,
            ScaleKind::Dorian => 7,
            ScaleKind::Phrygian => 8,
            ScaleKind::Lydian => 9,
            ScaleKind::Mixolydian => 10,
            ScaleKind::Locrian => 11,
        }
    }

    /// Inverse of [`Self::to_tag`]; `None` for a tag this build does not know.
    pub fn from_tag(tag: u8) -> Option<Self> {
        Self::ALL.iter().copied().find(|kind| kind.to_tag() == tag)
    }
}

/// A root + scale pair — the musical scale itself, independent of whether the
/// editor is currently constraining to it (see [`PitchTransformContext`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MidiScale {
    pub root: ScaleRoot,
    pub kind: ScaleKind,
}

impl Default for MidiScale {
    fn default() -> Self {
        Self {
            root: ScaleRoot::C,
            kind: ScaleKind::Chromatic,
        }
    }
}

impl MidiScale {
    pub fn new(root: ScaleRoot, kind: ScaleKind) -> Self {
        Self { root, kind }
    }

    /// Scales a project key can be set to. Chromatic is "no key", which the
    /// project stores as the absence of a key rather than as a scale.
    pub const KEY_KINDS: [ScaleKind; 11] = [
        ScaleKind::Major,
        ScaleKind::NaturalMinor,
        ScaleKind::HarmonicMinor,
        ScaleKind::MelodicMinor,
        ScaleKind::MajorPentatonic,
        ScaleKind::MinorPentatonic,
        ScaleKind::Dorian,
        ScaleKind::Phrygian,
        ScaleKind::Lydian,
        ScaleKind::Mixolydian,
        ScaleKind::Locrian,
    ];

    /// "A Natural Minor".
    pub fn label(&self) -> String {
        format!("{} {}", self.root.label(), self.kind.label())
    }

    /// `true` if `pitch` (0..=127) is a member of this scale.
    pub fn contains_pitch(&self, pitch: u8) -> bool {
        if self.kind == ScaleKind::Chromatic {
            return true;
        }
        let pc = (pitch as i32 - self.root.pitch_class() as i32).rem_euclid(12) as u8;
        self.kind.intervals().contains(&pc)
    }

    /// Nearest in-scale pitch to `pitch`, clamped to 0..=127. Ties (equal
    /// distance up/down) resolve to the lower pitch. Chromatic always returns
    /// `pitch` unchanged — raw behavior is preserved when constraint is off.
    pub fn nearest_pitch(&self, pitch: u8) -> u8 {
        if self.contains_pitch(pitch) {
            return pitch;
        }
        for dist in 1..=6i32 {
            let down = pitch as i32 - dist;
            if down >= 0 && self.contains_pitch(down as u8) {
                return down as u8;
            }
            let up = pitch as i32 + dist;
            if up <= 127 && self.contains_pitch(up as u8) {
                return up as u8;
            }
        }
        pitch
    }

    /// Tertian chord built from `root`. `voices` is 1 (the root), 3 (triad) or
    /// 4 (seventh). The root is snapped to the scale first; additional voices
    /// stack diatonic thirds. Chromatic uses a major triad / major seventh so
    /// chord drawing still produces a real chord without a scale selected.
    pub fn chord_pitches(&self, root: u8, voices: usize) -> Vec<u8> {
        let voices = voices.clamp(1, 4);
        let root = self.nearest_pitch(root);
        if voices == 1 {
            return vec![root];
        }
        if self.kind == ScaleKind::Chromatic {
            let intervals: &[u8] = if voices >= 4 {
                &[0, 4, 7, 11]
            } else {
                &[0, 4, 7]
            };
            return intervals
                .iter()
                .filter_map(|interval| root.checked_add(*interval))
                .filter(|pitch| *pitch <= 127)
                .collect();
        }
        let mut pitches = Vec::with_capacity(voices);
        let mut pitch = root;
        pitches.push(pitch);
        for _ in 1..voices {
            let mut in_scale_steps = 0;
            let mut next = None;
            for candidate in (pitch + 1)..=127 {
                if self.contains_pitch(candidate) {
                    in_scale_steps += 1;
                    if in_scale_steps == 2 {
                        next = Some(candidate);
                        break;
                    }
                }
            }
            let Some(found) = next else {
                break;
            };
            pitch = found;
            pitches.push(pitch);
        }
        pitches
    }
}

/// Chord stacked on Draw in the MIDI editor. `Off` keeps single-note drawing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ChordInsertKind {
    #[default]
    Off,
    Triad,
    Seventh,
}

impl ChordInsertKind {
    pub const ALL: [Self; 3] = [Self::Off, Self::Triad, Self::Seventh];

    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "Chord",
            Self::Triad => "Triad",
            Self::Seventh => "7th",
        }
    }

    pub fn voices(self) -> usize {
        match self {
            Self::Off => 1,
            Self::Triad => 3,
            Self::Seventh => 4,
        }
    }

    pub fn cycle(self) -> Self {
        match self {
            Self::Off => Self::Triad,
            Self::Triad => Self::Seventh,
            Self::Seventh => Self::Off,
        }
    }
}

/// Runtime editor state wrapping a [`MidiScale`]: whether note-drag/draw
/// gestures should actually constrain to it right now. Kept separate from
/// `MidiScale` itself so a stored scale choice never implies the editor is
/// constraining — raw chromatic editing remains the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PitchTransformContext {
    pub scale: MidiScale,
    pub constrain: bool,
}

impl Default for PitchTransformContext {
    fn default() -> Self {
        Self {
            scale: MidiScale::default(),
            constrain: false,
        }
    }
}

impl PitchTransformContext {
    /// Constrain `pitch` to the active scale if constraining is enabled;
    /// otherwise return it unchanged.
    pub fn constrain_pitch(&self, pitch: u8) -> u8 {
        if self.constrain {
            self.scale.nearest_pitch(pitch)
        } else {
            pitch
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_tags_are_stable_and_round_trip() {
        for kind in ScaleKind::ALL {
            assert_eq!(ScaleKind::from_tag(kind.to_tag()), Some(kind));
        }
        // Stored in projects: these numbers must never move.
        assert_eq!(ScaleKind::Major.to_tag(), 1);
        assert_eq!(ScaleKind::NaturalMinor.to_tag(), 2);
        assert_eq!(ScaleKind::Locrian.to_tag(), 11);
        assert_eq!(ScaleKind::from_tag(200), None);
        assert_eq!(ScaleRoot::from_pitch_class(9), ScaleRoot::A);
        assert_eq!(ScaleRoot::from_pitch_class(21), ScaleRoot::A);
        assert!(!MidiScale::KEY_KINDS.contains(&ScaleKind::Chromatic));
    }

    #[test]
    fn chromatic_scale_contains_every_pitch() {
        let scale = MidiScale::new(ScaleRoot::C, ScaleKind::Chromatic);
        for pitch in 0..=127u8 {
            assert!(scale.contains_pitch(pitch));
            assert_eq!(scale.nearest_pitch(pitch), pitch);
        }
    }

    #[test]
    fn c_major_contains_white_keys_only() {
        let scale = MidiScale::new(ScaleRoot::C, ScaleKind::Major);
        // C4=60 .. B4=71
        let expect_in = [60, 62, 64, 65, 67, 69, 71];
        let expect_out = [61, 63, 66, 68, 70];
        for p in expect_in {
            assert!(scale.contains_pitch(p), "expected {p} in C major");
        }
        for p in expect_out {
            assert!(!scale.contains_pitch(p), "expected {p} out of C major");
        }
    }

    #[test]
    fn nearest_pitch_snaps_black_keys_to_nearest_white_key() {
        let scale = MidiScale::new(ScaleRoot::C, ScaleKind::Major);
        // C#4 (61) is equidistant from C4 (60) and D4 (62); ties favor lower.
        assert_eq!(scale.nearest_pitch(61), 60);
        // D#4 (63) is closer to D4 (62, dist 1) than E4 (64, dist 1) -> tie, lower wins.
        assert_eq!(scale.nearest_pitch(63), 62);
        // F#4 (66) is equidistant from F4 (65) and G4 (67) -> lower wins.
        assert_eq!(scale.nearest_pitch(66), 65);
    }

    #[test]
    fn nearest_pitch_transposed_root() {
        // A minor pentatonic: A(9), C(0), D(2), E(4), G(7) pitch classes.
        let scale = MidiScale::new(ScaleRoot::A, ScaleKind::MinorPentatonic);
        assert!(scale.contains_pitch(69)); // A4
        assert!(scale.contains_pitch(72)); // C5
        assert!(!scale.contains_pitch(71)); // B4 not in scale
        assert_eq!(scale.nearest_pitch(71), 72); // snaps up to C5 (dist 1) over A4 (dist 2)
    }

    #[test]
    fn pitch_transform_context_disabled_is_identity() {
        let ctx = PitchTransformContext {
            scale: MidiScale::new(ScaleRoot::C, ScaleKind::Major),
            constrain: false,
        };
        for pitch in [60, 61, 66, 70] {
            assert_eq!(ctx.constrain_pitch(pitch), pitch);
        }
    }

    #[test]
    fn pitch_transform_context_enabled_constrains() {
        let ctx = PitchTransformContext {
            scale: MidiScale::new(ScaleRoot::C, ScaleKind::Major),
            constrain: true,
        };
        assert_eq!(ctx.constrain_pitch(61), 60);
        assert_eq!(ctx.constrain_pitch(64), 64);
    }

    #[test]
    fn root_and_kind_cycle_wrap_around() {
        assert_eq!(ScaleRoot::B.cycle(), ScaleRoot::C);
        assert_eq!(ScaleKind::Locrian.cycle(), ScaleKind::Chromatic);
    }

    #[test]
    fn c_major_triad_stacks_diatonic_thirds() {
        let scale = MidiScale::new(ScaleRoot::C, ScaleKind::Major);
        assert_eq!(scale.chord_pitches(60, 3), vec![60, 64, 67]);
        assert_eq!(scale.chord_pitches(62, 3), vec![62, 65, 69]);
        assert_eq!(scale.chord_pitches(60, 4), vec![60, 64, 67, 71]);
    }

    #[test]
    fn chord_from_out_of_scale_pitch_snaps_the_root() {
        let scale = MidiScale::new(ScaleRoot::C, ScaleKind::Major);
        // C#4 (61) snaps down to C4, then the C major triad.
        assert_eq!(scale.chord_pitches(61, 3), vec![60, 64, 67]);
    }

    #[test]
    fn chromatic_chord_is_a_major_triad() {
        let scale = MidiScale::new(ScaleRoot::C, ScaleKind::Chromatic);
        assert_eq!(scale.chord_pitches(60, 3), vec![60, 64, 67]);
        assert_eq!(scale.chord_pitches(60, 4), vec![60, 64, 67, 71]);
    }
}

//! Chord theory for the chord generator and the Chord Track.
//!
//! Pure data and functions — no UI, no engine, no allocation-sensitive paths.
//! Everything here is deterministic: generation takes an explicit seed so a
//! progression can be regenerated, tested and reproduced exactly.
//!
//! * [`Scale`] — the harmonic palette: the diatonic modes, harmonic and
//!   melodic minor, exotic heptatonics, and the symmetric scales (whole tone,
//!   both diminished orderings) whose harmony is built by fitting chords to
//!   the scale rather than by stacking thirds.
//! * [`Chord`] — a root, a [`ChordQuality`] and an optional slash bass. The
//!   **Black Adder** chord is a quality of its own: an augmented triad over a
//!   bass a whole step below its root (`Faug/G`), a whole-tone sonority that
//!   classically resolves to vi or I.
//! * [`generate`] — a progression from a style's templates plus optional
//!   colour (sevenths, sus, borrowed chords, secondary dominants, passing
//!   diminished, Black Adder), placed only where it resolves musically.
//! * [`voice_progression`] — MIDI notes per chord with a bass note and a
//!   voice-led upper structure (each chord takes the inversion that moves the
//!   hands least).

use serde::{Deserialize, Serialize};

/// Sharp and flat spellings of the twelve pitch classes.
const SHARP_NAMES: [&str; 12] = [
    "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
];
const FLAT_NAMES: [&str; 12] = [
    "C", "Db", "D", "Eb", "E", "F", "Gb", "G", "Ab", "A", "Bb", "B",
];

/// Name of pitch class `pc` (0 = C) with the requested accidental style.
pub fn pitch_name(pc: u8, flats: bool) -> &'static str {
    let pc = (pc % 12) as usize;
    if flats {
        FLAT_NAMES[pc]
    } else {
        SHARP_NAMES[pc]
    }
}

// ── Scales ──────────────────────────────────────────────────────────────────

/// The harmonic palette a progression is drawn from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Scale {
    Major,
    Minor,
    HarmonicMinor,
    MelodicMinor,
    Dorian,
    Phrygian,
    Lydian,
    Mixolydian,
    Locrian,
    PhrygianDominant,
    HungarianMinor,
    WholeTone,
    /// Octatonic, half step first (dominant diminished).
    DiminishedHalfWhole,
    /// Octatonic, whole step first.
    DiminishedWholeHalf,
}

impl Scale {
    pub const ALL: [Scale; 14] = [
        Scale::Major,
        Scale::Minor,
        Scale::HarmonicMinor,
        Scale::MelodicMinor,
        Scale::Dorian,
        Scale::Phrygian,
        Scale::Lydian,
        Scale::Mixolydian,
        Scale::Locrian,
        Scale::PhrygianDominant,
        Scale::HungarianMinor,
        Scale::WholeTone,
        Scale::DiminishedHalfWhole,
        Scale::DiminishedWholeHalf,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Scale::Major => "Major",
            Scale::Minor => "Minor",
            Scale::HarmonicMinor => "Harmonic Minor",
            Scale::MelodicMinor => "Melodic Minor",
            Scale::Dorian => "Dorian",
            Scale::Phrygian => "Phrygian",
            Scale::Lydian => "Lydian",
            Scale::Mixolydian => "Mixolydian",
            Scale::Locrian => "Locrian",
            Scale::PhrygianDominant => "Phrygian Dominant",
            Scale::HungarianMinor => "Hungarian Minor",
            Scale::WholeTone => "Whole Tone",
            Scale::DiminishedHalfWhole => "Diminished (H–W)",
            Scale::DiminishedWholeHalf => "Diminished (W–H)",
        }
    }

    /// Semitone steps from the tonic.
    pub fn intervals(self) -> &'static [u8] {
        match self {
            Scale::Major => &[0, 2, 4, 5, 7, 9, 11],
            Scale::Minor => &[0, 2, 3, 5, 7, 8, 10],
            Scale::HarmonicMinor => &[0, 2, 3, 5, 7, 8, 11],
            Scale::MelodicMinor => &[0, 2, 3, 5, 7, 9, 11],
            Scale::Dorian => &[0, 2, 3, 5, 7, 9, 10],
            Scale::Phrygian => &[0, 1, 3, 5, 7, 8, 10],
            Scale::Lydian => &[0, 2, 4, 6, 7, 9, 11],
            Scale::Mixolydian => &[0, 2, 4, 5, 7, 9, 10],
            Scale::Locrian => &[0, 1, 3, 5, 6, 8, 10],
            Scale::PhrygianDominant => &[0, 1, 4, 5, 7, 8, 10],
            Scale::HungarianMinor => &[0, 2, 3, 6, 7, 8, 11],
            Scale::WholeTone => &[0, 2, 4, 6, 8, 10],
            Scale::DiminishedHalfWhole => &[0, 1, 3, 4, 6, 7, 9, 10],
            Scale::DiminishedWholeHalf => &[0, 2, 3, 5, 6, 8, 9, 11],
        }
    }

    /// Seven-note scales build their harmony by stacking thirds.
    pub fn is_heptatonic(self) -> bool {
        self.intervals().len() == 7
    }

    /// Whether the tonic triad has a minor third — decides which template
    /// family a style uses and how chords are spelled.
    pub fn is_minor(self) -> bool {
        self.intervals().contains(&3) && !self.intervals().contains(&4)
    }

    /// Pitch classes of this scale on `tonic`.
    pub fn pitch_classes(self, tonic: u8) -> Vec<u8> {
        self.intervals().iter().map(|i| (tonic + i) % 12).collect()
    }

    /// Whether spelling in this key prefers flats: decided by the parent
    /// major key's signature (F, Bb, Eb, Ab, Db, Gb use flats).
    pub fn prefers_flats(self, tonic: u8) -> bool {
        let parent = match self {
            Scale::Major | Scale::Lydian | Scale::Mixolydian => {
                // Lydian's parent is a fifth below, Mixolydian's a fourth.
                match self {
                    Scale::Lydian => (tonic + 7) % 12,
                    Scale::Mixolydian => (tonic + 5) % 12,
                    _ => tonic,
                }
            }
            Scale::Dorian => (tonic + 10) % 12,
            Scale::Phrygian | Scale::PhrygianDominant => (tonic + 8) % 12,
            Scale::Locrian => (tonic + 1) % 12,
            _ => (tonic + 3) % 12,
        };
        matches!(parent, 1 | 3 | 5 | 6 | 8 | 10)
    }
}

// ── Chords ──────────────────────────────────────────────────────────────────

/// Chord quality: the interval structure above the root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ChordQuality {
    Major,
    Minor,
    Diminished,
    Augmented,
    Sus2,
    Sus4,
    Major7,
    Dominant7,
    Minor7,
    HalfDiminished7,
    Diminished7,
    MinorMajor7,
    AugmentedMajor7,
    Augmented7,
    Dominant7Sus4,
    Dominant7Flat9,
    Major9,
    Dominant9,
    Minor9,
    Add9,
    MinorAdd9,
    Sixth,
    Minor6,
    /// Augmented triad over a bass a whole step below the root (`Faug/G`).
    BlackAdder,
}

impl ChordQuality {
    pub const ALL: [ChordQuality; 24] = [
        ChordQuality::Major,
        ChordQuality::Minor,
        ChordQuality::Diminished,
        ChordQuality::Augmented,
        ChordQuality::Sus2,
        ChordQuality::Sus4,
        ChordQuality::Major7,
        ChordQuality::Dominant7,
        ChordQuality::Minor7,
        ChordQuality::HalfDiminished7,
        ChordQuality::Diminished7,
        ChordQuality::MinorMajor7,
        ChordQuality::AugmentedMajor7,
        ChordQuality::Augmented7,
        ChordQuality::Dominant7Sus4,
        ChordQuality::Dominant7Flat9,
        ChordQuality::Major9,
        ChordQuality::Dominant9,
        ChordQuality::Minor9,
        ChordQuality::Add9,
        ChordQuality::MinorAdd9,
        ChordQuality::Sixth,
        ChordQuality::Minor6,
        ChordQuality::BlackAdder,
    ];

    /// Semitones above the root (the root included). The Black Adder's bass
    /// is below the root and is added by [`Chord::bass_pc`].
    pub fn intervals(self) -> &'static [u8] {
        match self {
            ChordQuality::Major => &[0, 4, 7],
            ChordQuality::Minor => &[0, 3, 7],
            ChordQuality::Diminished => &[0, 3, 6],
            ChordQuality::Augmented | ChordQuality::BlackAdder => &[0, 4, 8],
            ChordQuality::Sus2 => &[0, 2, 7],
            ChordQuality::Sus4 => &[0, 5, 7],
            ChordQuality::Major7 => &[0, 4, 7, 11],
            ChordQuality::Dominant7 => &[0, 4, 7, 10],
            ChordQuality::Minor7 => &[0, 3, 7, 10],
            ChordQuality::HalfDiminished7 => &[0, 3, 6, 10],
            ChordQuality::Diminished7 => &[0, 3, 6, 9],
            ChordQuality::MinorMajor7 => &[0, 3, 7, 11],
            ChordQuality::AugmentedMajor7 => &[0, 4, 8, 11],
            ChordQuality::Augmented7 => &[0, 4, 8, 10],
            ChordQuality::Dominant7Sus4 => &[0, 5, 7, 10],
            ChordQuality::Dominant7Flat9 => &[0, 4, 7, 10, 13],
            ChordQuality::Major9 => &[0, 4, 7, 11, 14],
            ChordQuality::Dominant9 => &[0, 4, 7, 10, 14],
            ChordQuality::Minor9 => &[0, 3, 7, 10, 14],
            ChordQuality::Add9 => &[0, 4, 7, 14],
            ChordQuality::MinorAdd9 => &[0, 3, 7, 14],
            ChordQuality::Sixth => &[0, 4, 7, 9],
            ChordQuality::Minor6 => &[0, 3, 7, 9],
        }
    }

    /// Symbol suffix after the root name.
    pub fn suffix(self) -> &'static str {
        match self {
            ChordQuality::Major => "",
            ChordQuality::Minor => "m",
            ChordQuality::Diminished => "dim",
            ChordQuality::Augmented | ChordQuality::BlackAdder => "aug",
            ChordQuality::Sus2 => "sus2",
            ChordQuality::Sus4 => "sus4",
            ChordQuality::Major7 => "maj7",
            ChordQuality::Dominant7 => "7",
            ChordQuality::Minor7 => "m7",
            ChordQuality::HalfDiminished7 => "m7b5",
            ChordQuality::Diminished7 => "dim7",
            ChordQuality::MinorMajor7 => "mMaj7",
            ChordQuality::AugmentedMajor7 => "maj7#5",
            ChordQuality::Augmented7 => "7#5",
            ChordQuality::Dominant7Sus4 => "7sus4",
            ChordQuality::Dominant7Flat9 => "7b9",
            ChordQuality::Major9 => "maj9",
            ChordQuality::Dominant9 => "9",
            ChordQuality::Minor9 => "m9",
            ChordQuality::Add9 => "add9",
            ChordQuality::MinorAdd9 => "madd9",
            ChordQuality::Sixth => "6",
            ChordQuality::Minor6 => "m6",
        }
    }

    /// Human label for pickers.
    pub fn label(self) -> &'static str {
        match self {
            ChordQuality::Major => "Major",
            ChordQuality::Minor => "Minor",
            ChordQuality::Diminished => "Diminished",
            ChordQuality::Augmented => "Augmented",
            ChordQuality::Sus2 => "Sus2",
            ChordQuality::Sus4 => "Sus4",
            ChordQuality::Major7 => "Major 7",
            ChordQuality::Dominant7 => "Dominant 7",
            ChordQuality::Minor7 => "Minor 7",
            ChordQuality::HalfDiminished7 => "Half-dim 7",
            ChordQuality::Diminished7 => "Dim 7",
            ChordQuality::MinorMajor7 => "Minor-Major 7",
            ChordQuality::AugmentedMajor7 => "Aug Major 7",
            ChordQuality::Augmented7 => "Aug 7",
            ChordQuality::Dominant7Sus4 => "7 sus4",
            ChordQuality::Dominant7Flat9 => "7 b9",
            ChordQuality::Major9 => "Major 9",
            ChordQuality::Dominant9 => "Dominant 9",
            ChordQuality::Minor9 => "Minor 9",
            ChordQuality::Add9 => "Add 9",
            ChordQuality::MinorAdd9 => "Minor add 9",
            ChordQuality::Sixth => "6",
            ChordQuality::Minor6 => "Minor 6",
            ChordQuality::BlackAdder => "Black Adder",
        }
    }

    /// Minor-third qualities read as lower-case Roman numerals.
    fn is_minor_family(self) -> bool {
        matches!(
            self,
            ChordQuality::Minor
                | ChordQuality::Minor7
                | ChordQuality::MinorMajor7
                | ChordQuality::Minor9
                | ChordQuality::MinorAdd9
                | ChordQuality::Minor6
                | ChordQuality::Diminished
                | ChordQuality::HalfDiminished7
                | ChordQuality::Diminished7
        )
    }

    /// The triad family, used when colouring or extending a chord.
    fn triad(self) -> ChordQuality {
        match self {
            ChordQuality::Minor
            | ChordQuality::Minor7
            | ChordQuality::MinorMajor7
            | ChordQuality::Minor9
            | ChordQuality::MinorAdd9
            | ChordQuality::Minor6 => ChordQuality::Minor,
            ChordQuality::Diminished
            | ChordQuality::HalfDiminished7
            | ChordQuality::Diminished7 => ChordQuality::Diminished,
            ChordQuality::Augmented
            | ChordQuality::AugmentedMajor7
            | ChordQuality::Augmented7
            | ChordQuality::BlackAdder => ChordQuality::Augmented,
            ChordQuality::Sus2 => ChordQuality::Sus2,
            ChordQuality::Sus4 | ChordQuality::Dominant7Sus4 => ChordQuality::Sus4,
            _ => ChordQuality::Major,
        }
    }
}

/// A chord symbol: root pitch class, quality, and an optional slash bass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Chord {
    /// Root pitch class, 0 = C.
    pub root: u8,
    pub quality: ChordQuality,
    /// Slash bass pitch class, when not the root.
    #[serde(default)]
    pub bass: Option<u8>,
}

impl Chord {
    pub fn new(root: u8, quality: ChordQuality) -> Self {
        Self {
            root: root % 12,
            quality,
            bass: None,
        }
    }

    /// The Black Adder chord resolving to `target`: an augmented triad a
    /// major third below the target over a bass a whole step below it
    /// (to A: `Faug/G`; to C: `Abaug/Bb`).
    pub fn black_adder_to(target: u8) -> Self {
        Self::new((target + 8) % 12, ChordQuality::BlackAdder)
    }

    /// Bass pitch class: the slash bass, the Black Adder's whole step below
    /// the root, or the root.
    pub fn bass_pc(&self) -> u8 {
        if let Some(bass) = self.bass {
            return bass % 12;
        }
        match self.quality {
            ChordQuality::BlackAdder => (self.root + 2) % 12,
            _ => self.root,
        }
    }

    /// Distinct pitch classes, bass first.
    pub fn pitch_classes(&self) -> Vec<u8> {
        let mut out = vec![self.bass_pc()];
        for interval in self.quality.intervals() {
            let pc = (self.root + interval) % 12;
            if !out.contains(&pc) {
                out.push(pc);
            }
        }
        out
    }

    /// Symbol, e.g. `Am7`, `F#m7b5`, `Faug/G`, `C/E`.
    pub fn name(&self, flats: bool) -> String {
        let mut name = format!("{}{}", pitch_name(self.root, flats), self.quality.suffix());
        let bass = self.bass_pc();
        if bass != self.root {
            name.push('/');
            name.push_str(pitch_name(bass, flats));
        }
        name
    }

    /// Roman numeral relative to `tonic`, e.g. `vi`, `V7`, `bVII`, `IVaug/V`.
    pub fn roman(&self, tonic: u8) -> String {
        let degree = |pc: u8, lower: bool| -> String {
            const ROMAN: [&str; 12] = [
                "I", "bII", "II", "bIII", "III", "IV", "#IV", "V", "bVI", "VI", "bVII", "VII",
            ];
            let numeral = ROMAN[((pc + 12 - tonic % 12) % 12) as usize];
            if lower {
                numeral.to_lowercase()
            } else {
                numeral.to_string()
            }
        };
        let lower = self.quality.is_minor_family();
        let head = degree(self.root, lower);
        let tail = match self.quality {
            ChordQuality::Minor | ChordQuality::Major => "",
            ChordQuality::Diminished => "°",
            ChordQuality::Diminished7 => "°7",
            ChordQuality::HalfDiminished7 => "ø7",
            ChordQuality::Augmented | ChordQuality::BlackAdder => "aug",
            ChordQuality::Minor7 => "7",
            ChordQuality::Minor9 => "9",
            ChordQuality::MinorAdd9 => "add9",
            ChordQuality::Minor6 => "6",
            ChordQuality::MinorMajor7 => "(maj7)",
            other => other.suffix(),
        };
        let mut out = format!("{head}{tail}");
        let bass = self.bass_pc();
        if bass != self.root {
            out.push('/');
            out.push_str(&degree(bass, false));
        }
        out
    }

    /// The same chord with a seventh (or its natural extension) added.
    fn with_seventh(self, scale_pcs: &[u8]) -> Chord {
        let seventh = |minor: bool| {
            let major7 = (self.root + 11) % 12;
            if !minor && scale_pcs.contains(&major7) {
                ChordQuality::Major7
            } else if minor
                && scale_pcs.contains(&major7)
                && !scale_pcs.contains(&((self.root + 10) % 12))
            {
                ChordQuality::MinorMajor7
            } else if minor {
                ChordQuality::Minor7
            } else {
                ChordQuality::Dominant7
            }
        };
        let quality = match self.quality {
            ChordQuality::Major => seventh(false),
            ChordQuality::Minor => seventh(true),
            ChordQuality::Diminished => {
                if scale_pcs.contains(&((self.root + 9) % 12)) {
                    ChordQuality::Diminished7
                } else {
                    ChordQuality::HalfDiminished7
                }
            }
            ChordQuality::Augmented => {
                if scale_pcs.contains(&((self.root + 11) % 12)) {
                    ChordQuality::AugmentedMajor7
                } else {
                    ChordQuality::Augmented7
                }
            }
            ChordQuality::Sus4 => ChordQuality::Dominant7Sus4,
            other => other,
        };
        Chord { quality, ..self }
    }

    /// The seventh chord with a ninth on top, where one exists.
    fn with_ninth(self, scale_pcs: &[u8]) -> Chord {
        let ninth = (self.root + 2) % 12;
        if !scale_pcs.contains(&ninth) {
            return self;
        }
        let quality = match self.quality {
            ChordQuality::Major7 => ChordQuality::Major9,
            ChordQuality::Dominant7 => ChordQuality::Dominant9,
            ChordQuality::Minor7 => ChordQuality::Minor9,
            other => other,
        };
        Chord { quality, ..self }
    }
}

/// The diatonic triad on scale degree `degree` (0-based) of a seven-note scale.
pub fn diatonic_triad(tonic: u8, scale: Scale, degree: usize) -> Chord {
    let steps = scale.intervals();
    let n = steps.len();
    let note = |k: usize| (tonic as usize + steps[(degree + k) % n] as usize) % 12;
    let root = note(0) as u8;
    let third = (note(2) + 12 - root as usize) % 12;
    let fifth = (note(4) + 12 - root as usize) % 12;
    let quality = match (third, fifth) {
        (4, 7) => ChordQuality::Major,
        (3, 7) => ChordQuality::Minor,
        (3, 6) => ChordQuality::Diminished,
        (4, 8) => ChordQuality::Augmented,
        (2, 7) => ChordQuality::Sus2,
        (5, 7) => ChordQuality::Sus4,
        _ => ChordQuality::Major,
    };
    Chord::new(root, quality)
}

// ── Generation ──────────────────────────────────────────────────────────────

/// Progression family: which templates the generator draws from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Style {
    Pop,
    JPop,
    Ballad,
    Jazz,
    LoFi,
    Cinematic,
    Edm,
}

impl Style {
    pub const ALL: [Style; 7] = [
        Style::Pop,
        Style::JPop,
        Style::Ballad,
        Style::Jazz,
        Style::LoFi,
        Style::Cinematic,
        Style::Edm,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Style::Pop => "Pop",
            Style::JPop => "J-Pop",
            Style::Ballad => "Ballad",
            Style::Jazz => "Jazz",
            Style::LoFi => "Lo-Fi",
            Style::Cinematic => "Cinematic",
            Style::Edm => "EDM",
        }
    }

    /// Scale degrees (0-based) of each template, for major-family scales and
    /// minor-family scales. Degrees are of the chosen scale, so a Dorian or
    /// Lydian progression keeps its modal colour.
    fn templates(self, minor: bool) -> &'static [&'static [usize]] {
        match (self, minor) {
            (Style::Pop, false) => &[
                &[0, 4, 5, 3],
                &[5, 3, 0, 4],
                &[0, 5, 3, 4],
                &[0, 3, 5, 4],
                &[3, 0, 4, 5],
            ],
            (Style::JPop, false) => &[
                // The "royal road" IV–V–iii–vi and its long form.
                &[3, 4, 2, 5],
                &[3, 4, 2, 5, 1, 4, 0, 0],
                &[0, 4, 5, 2, 3, 0, 3, 4],
                &[5, 3, 4, 0],
            ],
            (Style::Ballad, false) => &[&[0, 2, 3, 0], &[5, 3, 0, 4], &[0, 5, 1, 4], &[3, 4, 5, 5]],
            (Style::Jazz, false) => &[&[1, 4, 0, 0], &[0, 5, 1, 4], &[2, 5, 1, 4], &[3, 2, 1, 0]],
            (Style::LoFi, false) => &[&[3, 2, 1, 0], &[1, 4, 0, 5], &[0, 2, 3, 1]],
            (Style::Cinematic, false) => &[&[0, 5, 3, 4], &[5, 0, 3, 4], &[0, 2, 5, 3]],
            (Style::Edm, false) => &[&[5, 3, 0, 4], &[0, 4, 5, 3], &[3, 4, 5, 5]],
            (Style::Pop, true) => &[&[0, 5, 2, 6], &[0, 3, 5, 4], &[0, 6, 5, 6], &[5, 6, 0, 0]],
            (Style::JPop, true) => &[&[5, 6, 4, 0], &[0, 5, 6, 2], &[3, 4, 2, 5]],
            (Style::Ballad, true) => &[&[0, 5, 2, 6], &[0, 3, 6, 2], &[5, 2, 6, 0]],
            (Style::Jazz, true) => &[&[1, 4, 0, 0], &[0, 3, 1, 4], &[0, 5, 1, 4]],
            (Style::LoFi, true) => &[&[0, 3, 6, 2], &[3, 6, 0, 5], &[0, 2, 3, 4]],
            (Style::Cinematic, true) => &[
                // Andalusian cadence and its relatives.
                &[0, 6, 5, 4],
                &[0, 5, 2, 6],
                &[0, 3, 5, 4],
            ],
            (Style::Edm, true) => &[&[0, 5, 2, 6], &[0, 3, 0, 6], &[5, 2, 6, 0]],
        }
    }

    /// Default chord richness for the style.
    fn prefers_sevenths(self) -> bool {
        matches!(self, Style::Jazz | Style::LoFi)
    }
}

/// How rich each chord is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Richness {
    Triads,
    Sevenths,
    Ninths,
}

/// Optional harmonic colour. Each one is applied at most where it resolves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Colors {
    /// Occasional sus4 on dominants and sus2 on tonics.
    pub sus: bool,
    /// Chords borrowed from the parallel key (iv in major, IV in minor, bVII).
    pub borrowed: bool,
    /// A dominant seventh a fifth above the next chord (V7/x).
    pub secondary_dominants: bool,
    /// A diminished seventh a half step below the next chord.
    pub passing_diminished: bool,
    /// The Black Adder chord leading into vi or I.
    pub black_adder: bool,
}

/// Everything the generator needs.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GeneratorSettings {
    pub tonic: u8,
    pub scale: Scale,
    pub style: Style,
    pub richness: Richness,
    pub colors: Colors,
    /// Chords in the progression (templates repeat or truncate to fit).
    pub length: usize,
}

impl Default for GeneratorSettings {
    fn default() -> Self {
        Self {
            tonic: 0,
            scale: Scale::Major,
            style: Style::Pop,
            richness: Richness::Triads,
            colors: Colors::default(),
            length: 4,
        }
    }
}

/// Small deterministic PRNG (SplitMix64) — reproducible progressions without
/// a dependency.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed ^ 0x9E37_79B9_7F4A_7C15)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next_u64() % n as u64) as usize
        }
    }

    pub fn chance(&mut self, p: f32) -> bool {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32 <= p
    }
}

/// Generate a progression. The same settings and seed always give the same
/// chords.
pub fn generate(settings: &GeneratorSettings, seed: u64) -> Vec<Chord> {
    let mut rng = Rng::new(seed);
    let length = settings.length.clamp(1, 16);
    let scale_pcs = settings.scale.pitch_classes(settings.tonic);
    let mut chords = if settings.scale.is_heptatonic() {
        let templates = settings.style.templates(settings.scale.is_minor());
        let template = templates[rng.below(templates.len())];
        (0..length)
            .map(|i| diatonic_triad(settings.tonic, settings.scale, template[i % template.len()]))
            .collect::<Vec<_>>()
    } else {
        symmetric_progression(settings, length, &mut rng)
    };

    apply_colors(&mut chords, settings, &scale_pcs, &mut rng);

    let richness = match settings.richness {
        Richness::Triads if settings.style.prefers_sevenths() => Richness::Sevenths,
        other => other,
    };
    for chord in &mut chords {
        if chord.quality == ChordQuality::BlackAdder || chord.bass.is_some() {
            continue;
        }
        match richness {
            Richness::Triads => {}
            Richness::Sevenths => *chord = chord.with_seventh(&scale_pcs),
            Richness::Ninths => *chord = chord.with_seventh(&scale_pcs).with_ninth(&scale_pcs),
        }
    }
    chords
}

/// Symmetric scales have no stacked-thirds harmony. Chords are the ones that
/// fit the scale, and the progression moves by the scale's own symmetry
/// (whole steps for whole tone, minor thirds for the diminished scales),
/// starting and ending on a chord that holds the tonic.
fn symmetric_progression(settings: &GeneratorSettings, length: usize, rng: &mut Rng) -> Vec<Chord> {
    let tonic = settings.tonic;
    let palette = fitting_chords(settings.scale, tonic);
    if palette.is_empty() {
        return vec![Chord::new(tonic, ChordQuality::Major); length];
    }
    let holds_tonic: Vec<Chord> = palette
        .iter()
        .copied()
        .filter(|c| c.pitch_classes().contains(&tonic))
        .collect();
    let step: u8 = match settings.scale {
        Scale::WholeTone => 2,
        _ => 3,
    };
    let mut out = Vec::with_capacity(length);
    let mut current = holds_tonic[rng.below(holds_tonic.len())];
    for i in 0..length {
        if i + 1 == length && length > 1 {
            // Resolve: back to a chord holding the tonic, different from the
            // one before if possible.
            let choices: Vec<Chord> = holds_tonic
                .iter()
                .copied()
                .filter(|c| out.last() != Some(c))
                .collect();
            let pool = if choices.is_empty() {
                &holds_tonic
            } else {
                &choices
            };
            out.push(pool[rng.below(pool.len())]);
            break;
        }
        out.push(current);
        let direction = if rng.chance(0.5) { step } else { 12 - step };
        let next_root = (current.root + direction) % 12;
        let candidates: Vec<Chord> = palette
            .iter()
            .copied()
            .filter(|c| c.root == next_root)
            .collect();
        current = if candidates.is_empty() {
            palette[rng.below(palette.len())]
        } else {
            candidates[rng.below(candidates.len())]
        };
    }
    out
}

/// Every chord of the core qualities whose notes all lie in the scale.
pub fn fitting_chords(scale: Scale, tonic: u8) -> Vec<Chord> {
    let pcs = scale.pitch_classes(tonic);
    let qualities: &[ChordQuality] = match scale {
        Scale::WholeTone => &[
            ChordQuality::Augmented,
            ChordQuality::Augmented7,
            ChordQuality::BlackAdder,
        ],
        _ => &[
            ChordQuality::Major,
            ChordQuality::Minor,
            ChordQuality::Diminished7,
            ChordQuality::Dominant7,
            ChordQuality::Dominant7Flat9,
            ChordQuality::Minor7,
            ChordQuality::HalfDiminished7,
        ],
    };
    let mut out = Vec::new();
    for &root in &pcs {
        for &quality in qualities {
            let chord = Chord::new(root, quality);
            if chord.pitch_classes().iter().all(|pc| pcs.contains(pc)) {
                out.push(chord);
            }
        }
    }
    out
}

fn apply_colors(
    chords: &mut [Chord],
    settings: &GeneratorSettings,
    scale_pcs: &[u8],
    rng: &mut Rng,
) {
    let tonic = settings.tonic;
    let n = chords.len();
    if n < 2 {
        return;
    }
    let colors = settings.colors;
    // Slots already recoloured stay as they are, so colours never stack.
    let mut touched = vec![false; n];

    // Black Adder: the chord before vi (major) / i / I, replaced.
    if colors.black_adder {
        let relative_minor = (tonic + 9) % 12;
        let targets: Vec<usize> = (1..n)
            .filter(|&i| {
                let root = chords[i].root;
                chords[i].quality.triad() != ChordQuality::Diminished
                    && (root == tonic || (!settings.scale.is_minor() && root == relative_minor))
            })
            .collect();
        if let Some(&target) = pick(&targets, rng) {
            chords[target - 1] = Chord::black_adder_to(chords[target].root);
            touched[target - 1] = true;
        }
    }

    // Secondary dominant: V7 of a non-tonic diatonic target.
    if colors.secondary_dominants {
        let targets: Vec<usize> = (1..n)
            .filter(|&i| {
                !touched[i - 1]
                    && chords[i].root != tonic
                    && chords[i].quality.triad() != ChordQuality::Diminished
            })
            .collect();
        if let Some(&target) = pick(&targets, rng) {
            chords[target - 1] =
                Chord::new((chords[target].root + 7) % 12, ChordQuality::Dominant7);
            touched[target - 1] = true;
        }
    }

    // Passing diminished seventh a half step below the next chord's root.
    if colors.passing_diminished {
        let targets: Vec<usize> = (1..n)
            .filter(|&i| !touched[i - 1] && chords[i].quality.triad() == ChordQuality::Minor)
            .collect();
        if let Some(&target) = pick(&targets, rng) {
            chords[target - 1] =
                Chord::new((chords[target].root + 11) % 12, ChordQuality::Diminished7);
            touched[target - 1] = true;
        }
    }

    // Borrowed chord from the parallel key.
    if colors.borrowed && settings.scale.is_heptatonic() {
        let fourth = (tonic + 5) % 12;
        let candidates: Vec<usize> = (0..n)
            .filter(|&i| !touched[i] && chords[i].root == fourth)
            .collect();
        if let Some(&slot) = pick(&candidates, rng) {
            let quality = if settings.scale.is_minor() {
                ChordQuality::Major
            } else {
                ChordQuality::Minor
            };
            chords[slot] = Chord::new(fourth, quality);
            touched[slot] = true;
        } else if !settings.scale.is_minor() {
            // No IV to darken: bVII before the last chord.
            let slot = n - 2;
            if !touched[slot] {
                chords[slot] = Chord::new((tonic + 10) % 12, ChordQuality::Major);
                touched[slot] = true;
            }
        }
    }

    // Sus: a dominant becomes 7sus4, or a tonic becomes sus2.
    if colors.sus {
        let dominant = (tonic + 7) % 12;
        let candidates: Vec<usize> = (0..n)
            .filter(|&i| {
                !touched[i]
                    && ((chords[i].root == dominant
                        && chords[i].quality.triad() == ChordQuality::Major)
                        || (chords[i].root == tonic
                            && chords[i].quality.triad() == ChordQuality::Major))
            })
            .collect();
        if let Some(&slot) = pick(&candidates, rng) {
            let quality = if chords[slot].root == dominant {
                ChordQuality::Dominant7Sus4
            } else {
                ChordQuality::Sus2
            };
            chords[slot] = Chord::new(chords[slot].root, quality);
        }
    }
    let _ = scale_pcs;
}

fn pick<'a, T>(items: &'a [T], rng: &mut Rng) -> Option<&'a T> {
    if items.is_empty() {
        None
    } else {
        items.get(rng.below(items.len()))
    }
}

/// Alternatives for one slot, for the "swap chord" picker: the diatonic
/// chords of the scale (with the current richness applied) plus the colour
/// chords that lead into `next`.
pub fn alternatives(settings: &GeneratorSettings, next: Option<Chord>) -> Vec<Chord> {
    let scale_pcs = settings.scale.pitch_classes(settings.tonic);
    let mut out: Vec<Chord> = if settings.scale.is_heptatonic() {
        (0..7)
            .map(|d| diatonic_triad(settings.tonic, settings.scale, d))
            .collect()
    } else {
        fitting_chords(settings.scale, settings.tonic)
    };
    if matches!(settings.richness, Richness::Sevenths | Richness::Ninths) {
        for chord in &mut out {
            *chord = chord.with_seventh(&scale_pcs);
        }
    }
    if let Some(next) = next {
        for extra in [
            Chord::new((next.root + 7) % 12, ChordQuality::Dominant7),
            Chord::new((next.root + 11) % 12, ChordQuality::Diminished7),
            Chord::black_adder_to(next.root),
        ] {
            if !out.contains(&extra) {
                out.push(extra);
            }
        }
    }
    out
}

// ── Voicing ─────────────────────────────────────────────────────────────────

/// How chords are laid out as MIDI notes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoicingOptions {
    /// Play the bass note an octave or two below the upper structure.
    pub bass: bool,
    /// Centre of the upper structure (MIDI note).
    pub center: u8,
    /// Spread the upper voices an octave wider (open voicing).
    pub open: bool,
}

impl Default for VoicingOptions {
    fn default() -> Self {
        Self {
            bass: true,
            center: 64,
            open: false,
        }
    }
}

const BASS_LOW: i32 = 36;

/// MIDI notes for each chord: a bass note (optional) plus an upper structure
/// that takes, for every chord after the first, the inversion and octave
/// moving the voices the least from the previous chord.
pub fn voice_progression(chords: &[Chord], options: VoicingOptions) -> Vec<Vec<u8>> {
    let mut previous: Option<Vec<i32>> = None;
    let mut out = Vec::with_capacity(chords.len());
    for chord in chords {
        let upper = voice_upper(chord, options, previous.as_deref());
        let mut notes: Vec<u8> = Vec::with_capacity(upper.len() + 1);
        if options.bass {
            let bass = BASS_LOW + chord.bass_pc() as i32;
            notes.push(bass as u8);
        }
        notes.extend(upper.iter().map(|&n| n.clamp(0, 127) as u8));
        notes.sort_unstable();
        notes.dedup();
        out.push(notes);
        previous = Some(upper);
    }
    out
}

fn voice_upper(chord: &Chord, options: VoicingOptions, previous: Option<&[i32]>) -> Vec<i32> {
    // Upper voices: the chord's own tones (the Black Adder's bass stays in
    // the bass; its upper structure is the augmented triad).
    let tones: Vec<i32> = chord
        .quality
        .intervals()
        .iter()
        .map(|i| (chord.root as i32 + *i as i32) % 12)
        .collect();
    let center = options.center as i32;
    let mut best: Option<(i32, Vec<i32>)> = None;
    for rotation in 0..tones.len() {
        for base in (center - 18..=center + 6).filter(|n| n.rem_euclid(12) == tones[rotation]) {
            let mut voicing = Vec::with_capacity(tones.len());
            let mut last = base - 1;
            for k in 0..tones.len() {
                let pc = tones[(rotation + k) % tones.len()];
                let mut note = last + 1;
                while note.rem_euclid(12) != pc {
                    note += 1;
                }
                voicing.push(note);
                last = note;
            }
            if options.open && voicing.len() >= 3 {
                // Drop-2 style: the second voice from the top up an octave.
                let i = voicing.len() - 2;
                voicing[i] += 12;
                voicing.sort_unstable();
            }
            let cost = match previous {
                Some(prev) => movement(prev, &voicing),
                None => {
                    let mean = voicing.iter().sum::<i32>() / voicing.len() as i32;
                    (mean - center).abs() * 4
                }
            } + (voicing.iter().sum::<i32>() / voicing.len() as i32 - center).abs();
            if best.as_ref().is_none_or(|(c, _)| cost < *c) {
                best = Some((cost, voicing));
            }
        }
    }
    best.map(|(_, v)| v).unwrap_or_default()
}

/// Total voice movement between two voicings, matching each new voice to its
/// nearest old one.
fn movement(prev: &[i32], next: &[i32]) -> i32 {
    next.iter()
        .map(|n| prev.iter().map(|p| (n - p).abs()).min().unwrap_or(0))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(chords: &[Chord], flats: bool) -> Vec<String> {
        chords.iter().map(|c| c.name(flats)).collect()
    }

    #[test]
    fn diatonic_triads_of_c_major_and_a_minor() {
        let major: Vec<String> = (0..7)
            .map(|d| diatonic_triad(0, Scale::Major, d).name(false))
            .collect();
        assert_eq!(major, ["C", "Dm", "Em", "F", "G", "Am", "Bdim"]);
        let minor: Vec<String> = (0..7)
            .map(|d| diatonic_triad(9, Scale::Minor, d).name(false))
            .collect();
        assert_eq!(minor, ["Am", "Bdim", "C", "Dm", "Em", "F", "G"]);
        let harmonic = diatonic_triad(9, Scale::HarmonicMinor, 4);
        assert_eq!(harmonic.name(false), "E");
    }

    #[test]
    fn black_adder_is_an_augmented_triad_over_a_whole_step_below() {
        let to_a = Chord::black_adder_to(9);
        assert_eq!(to_a.name(false), "Faug/G");
        assert_eq!(to_a.roman(0), "IVaug/V");
        let to_c = Chord::black_adder_to(0);
        assert_eq!(to_c.name(true), "Abaug/Bb");
        // A whole-tone sonority: bass, 9th, #11, #5 relative to the bass.
        let mut pcs = to_a.pitch_classes();
        pcs.sort_unstable();
        assert_eq!(pcs, vec![1, 5, 7, 9]);
        let wt = Scale::WholeTone.pitch_classes(1);
        assert!(to_a.pitch_classes().iter().all(|pc| wt.contains(pc)));
    }

    #[test]
    fn roman_numerals_follow_quality() {
        assert_eq!(Chord::new(9, ChordQuality::Minor).roman(0), "vi");
        assert_eq!(Chord::new(7, ChordQuality::Dominant7).roman(0), "V7");
        assert_eq!(Chord::new(10, ChordQuality::Major).roman(0), "bVII");
        assert_eq!(
            Chord::new(11, ChordQuality::HalfDiminished7).roman(0),
            "viiø7"
        );
        assert_eq!(Chord::new(1, ChordQuality::Diminished7).roman(0), "bii°7");
    }

    #[test]
    fn generation_is_deterministic_and_in_key() {
        let settings = GeneratorSettings::default();
        let a = generate(&settings, 42);
        let b = generate(&settings, 42);
        assert_eq!(a, b);
        assert_eq!(a.len(), 4);
        let scale = Scale::Major.pitch_classes(0);
        for chord in &a {
            assert!(
                chord.pitch_classes().iter().all(|pc| scale.contains(pc)),
                "{a:?}"
            );
        }
        // Different seeds reach different templates.
        let distinct: std::collections::HashSet<Vec<Chord>> =
            (0..32).map(|seed| generate(&settings, seed)).collect();
        assert!(distinct.len() > 2);
    }

    #[test]
    fn jazz_uses_sevenths_and_ninths_when_asked() {
        let settings = GeneratorSettings {
            style: Style::Jazz,
            richness: Richness::Ninths,
            ..GeneratorSettings::default()
        };
        let chords = generate(&settings, 3);
        assert!(
            chords.iter().all(|c| c.quality.intervals().len() >= 4),
            "{chords:?}"
        );
    }

    #[test]
    fn black_adder_color_resolves_into_vi_or_i() {
        let settings = GeneratorSettings {
            colors: Colors {
                black_adder: true,
                ..Colors::default()
            },
            length: 8,
            ..GeneratorSettings::default()
        };
        let mut found = 0;
        for seed in 0..64 {
            let chords = generate(&settings, seed);
            for i in 0..chords.len() - 1 {
                if chords[i].quality == ChordQuality::BlackAdder {
                    found += 1;
                    assert_eq!(chords[i], Chord::black_adder_to(chords[i + 1].root));
                    assert!(matches!(chords[i + 1].root, 0 | 9), "{chords:?}");
                }
            }
        }
        assert!(found > 0, "the colour should apply when a target exists");
    }

    #[test]
    fn symmetric_scales_only_use_fitting_chords() {
        for scale in [
            Scale::WholeTone,
            Scale::DiminishedHalfWhole,
            Scale::DiminishedWholeHalf,
        ] {
            let settings = GeneratorSettings {
                scale,
                length: 8,
                ..GeneratorSettings::default()
            };
            let pcs = scale.pitch_classes(0);
            for seed in 0..16 {
                let chords = generate(&settings, seed);
                assert_eq!(chords.len(), 8);
                for chord in &chords {
                    assert!(
                        chord.pitch_classes().iter().all(|pc| pcs.contains(pc)),
                        "{scale:?} {chords:?}"
                    );
                }
                assert!(chords.last().unwrap().pitch_classes().contains(&0));
            }
        }
    }

    #[test]
    fn flats_follow_the_key_signature() {
        assert!(Scale::Major.prefers_flats(5)); // F major
        assert!(!Scale::Major.prefers_flats(7)); // G major
        assert!(Scale::Minor.prefers_flats(2)); // D minor (F major)
        assert!(!Scale::Minor.prefers_flats(4)); // E minor (G major)
        assert_eq!(names(&[Chord::new(10, ChordQuality::Major)], true), ["Bb"]);
    }

    #[test]
    fn voicing_keeps_bass_low_and_moves_little() {
        let chords = [
            Chord::new(0, ChordQuality::Major),
            Chord::new(7, ChordQuality::Major),
            Chord::new(9, ChordQuality::Minor),
            Chord::new(5, ChordQuality::Major),
        ];
        let voiced = voice_progression(&chords, VoicingOptions::default());
        assert_eq!(voiced.len(), 4);
        for (chord, notes) in chords.iter().zip(&voiced) {
            assert_eq!(notes[0] % 12, chord.bass_pc());
            assert!(notes[0] < 48);
            let pcs: Vec<u8> = notes.iter().map(|n| n % 12).collect();
            for pc in chord.pitch_classes() {
                assert!(pcs.contains(&pc), "{chord:?} {notes:?}");
            }
        }
        // Voice leading: no upper voice jumps more than a fourth between
        // these common-tone chords.
        for pair in voiced.windows(2) {
            let (a, b) = (&pair[0][1..], &pair[1][1..]);
            assert!(
                movement(
                    &a.iter().map(|&n| n as i32).collect::<Vec<_>>(),
                    &b.iter().map(|&n| n as i32).collect::<Vec<_>>()
                ) <= 8,
                "{voiced:?}"
            );
        }
    }

    #[test]
    fn black_adder_voicing_puts_its_bass_under_the_augmented_triad() {
        let voiced = voice_progression(&[Chord::black_adder_to(9)], VoicingOptions::default());
        let notes = &voiced[0];
        assert_eq!(notes[0] % 12, 7); // G bass
        let upper: Vec<u8> = notes[1..].iter().map(|n| n % 12).collect();
        for pc in [5, 9, 1] {
            assert!(upper.contains(&pc), "{notes:?}");
        }
    }
}

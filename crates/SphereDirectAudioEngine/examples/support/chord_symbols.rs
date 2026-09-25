//! Chord symbols for the chord corpus and benchmark: `Em7`, `C#m7b5`, `G/B`,
//! `Bb:maj7`, `N`. Evaluation needs the *reference* chord in full — whatever
//! the detector's vocabulary — so it is parsed to a root, the set of
//! intervals above it and the bass, and reduced from there to the MIREX
//! categories (root, maj/min, sevenths).

#![allow(dead_code)]

pub const NAMES: [&str; 12] = [
    "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
];

/// A parsed chord symbol.
#[derive(Clone, Debug, PartialEq)]
pub struct Symbol {
    pub root: u8,
    /// Intervals above the root, semitones, sorted, root (0) included.
    pub intervals: Vec<u8>,
    /// Bass pitch class (the root unless a slash bass is given).
    pub bass: u8,
    /// Quality as written, normalised (`""`, `m`, `7`, `maj7`, …).
    pub quality: String,
}

pub fn pitch_class(name: &str) -> Option<u8> {
    let mut chars = name.chars();
    let base: i32 = match chars.next()?.to_ascii_uppercase() {
        'C' => 0,
        'D' => 2,
        'E' => 4,
        'F' => 5,
        'G' => 7,
        'A' => 9,
        'B' => 11,
        _ => return None,
    };
    let mut accidental = 0;
    for c in chars {
        accidental += match c {
            '#' | '♯' => 1,
            'b' | '♭' => -1,
            _ => return None,
        };
    }
    Some((base + accidental).rem_euclid(12) as u8)
}

/// Intervals of a quality string, `None` if unknown.
pub fn quality_intervals(quality: &str) -> Option<(&'static str, &'static [u8])> {
    Some(match quality {
        "" | "maj" | "M" => ("", &[0, 4, 7]),
        "m" | "min" | "-" => ("m", &[0, 3, 7]),
        "5" => ("5", &[0, 7]),
        "dim" | "°" | "o" => ("dim", &[0, 3, 6]),
        "aug" | "+" => ("aug", &[0, 4, 8]),
        "sus2" => ("sus2", &[0, 2, 7]),
        "sus4" | "sus" => ("sus4", &[0, 5, 7]),
        "7" | "dom7" => ("7", &[0, 4, 7, 10]),
        "maj7" | "M7" | "Δ" | "Δ7" => ("maj7", &[0, 4, 7, 11]),
        "m7" | "min7" | "-7" => ("m7", &[0, 3, 7, 10]),
        "m7b5" | "hdim7" | "ø" | "ø7" => ("m7b5", &[0, 3, 6, 10]),
        "dim7" | "°7" | "o7" => ("dim7", &[0, 3, 6, 9]),
        "mmaj7" | "mMaj7" | "minmaj7" | "m(maj7)" => ("mmaj7", &[0, 3, 7, 11]),
        "6" | "maj6" => ("6", &[0, 4, 7, 9]),
        "m6" | "min6" => ("m6", &[0, 3, 7, 9]),
        "9" => ("9", &[0, 2, 4, 7, 10]),
        "maj9" => ("maj9", &[0, 2, 4, 7, 11]),
        "m9" | "min9" => ("m9", &[0, 2, 3, 7, 10]),
        "add9" => ("add9", &[0, 2, 4, 7]),
        "madd9" => ("madd9", &[0, 2, 3, 7]),
        "7sus4" => ("7sus4", &[0, 5, 7, 10]),
        "aug7" | "7#5" => ("aug7", &[0, 4, 8, 10]),
        "11" => ("11", &[0, 2, 4, 5, 7, 10]),
        "13" => ("13", &[0, 2, 4, 7, 9, 10]),
        _ => return None,
    })
}

/// Parse a symbol; `Ok(None)` is no chord (`N`, `X`, empty).
pub fn parse(symbol: &str) -> Result<Option<Symbol>, String> {
    let symbol = symbol.trim();
    if symbol.is_empty() || matches!(symbol, "N" | "X" | "NC" | "N.C.") {
        return Ok(None);
    }
    let (body, bass) = match symbol.rsplit_once('/') {
        Some((body, bass)) if pitch_class(bass).is_some() => (body, Some(bass)),
        _ => (symbol, None),
    };
    let (root_part, quality) = match body.split_once(':') {
        Some((r, q)) => (r, q),
        None => {
            let split = body
                .char_indices()
                .skip(1)
                .find(|(_, c)| !matches!(c, '#' | 'b' | '♯' | '♭'))
                .map_or(body.len(), |(i, _)| i);
            // "Bb" is B-flat, but "Bbmaj7" must not eat the "b" of nothing:
            // accidentals are only the characters straight after the letter.
            (&body[..split], &body[split..])
        }
    };
    let root = pitch_class(root_part).ok_or_else(|| format!("bad root in {symbol:?}"))?;
    let (quality, intervals) =
        quality_intervals(quality).ok_or_else(|| format!("unknown quality in {symbol:?}"))?;
    let bass = match bass {
        Some(b) => pitch_class(b).ok_or_else(|| format!("bad bass in {symbol:?}"))?,
        None => root,
    };
    Ok(Some(Symbol {
        root,
        intervals: intervals.to_vec(),
        bass,
        quality: quality.to_string(),
    }))
}

impl Symbol {
    pub fn transposed(&self, semitones: i32) -> Symbol {
        let shift = |pc: u8| ((pc as i32 + semitones).rem_euclid(12)) as u8;
        Symbol {
            root: shift(self.root),
            intervals: self.intervals.clone(),
            bass: shift(self.bass),
            quality: self.quality.clone(),
        }
    }

    pub fn name(&self) -> String {
        let mut s = format!("{}{}", NAMES[self.root as usize], self.quality);
        if self.bass != self.root {
            s.push('/');
            s.push_str(NAMES[self.bass as usize]);
        }
        s
    }

    fn has(&self, interval: u8) -> bool {
        self.intervals.contains(&interval)
    }

    /// MIREX "majmin": major or minor triad, or `None` when the chord is
    /// neither (dim, aug, sus, power chord) — such chords are left out of
    /// the majmin score, as in MIREX.
    pub fn majmin(&self) -> Option<bool> {
        let fifth = self.has(7);
        if self.has(4) && !self.has(3) && fifth && !self.has(5) {
            Some(false)
        } else if self.has(3) && !self.has(4) && fifth {
            Some(true)
        } else {
            None
        }
    }

    /// MIREX "sevenths": maj, min, 7, maj7, min7 — or `None` when the chord
    /// reduces to none of them (left out of the score).
    pub fn sevenths(&self) -> Option<&'static str> {
        let minor = self.majmin()?;
        let seventh = if self.has(10) {
            Some(10)
        } else if self.has(11) {
            Some(11)
        } else {
            None
        };
        Some(match (minor, seventh) {
            (false, None) => "maj",
            (true, None) => "min",
            (false, Some(10)) => "7",
            (false, Some(_)) => "maj7",
            (true, Some(10)) => "min7",
            // mMaj7 is outside the sevenths vocabulary.
            (true, Some(_)) => return None,
        })
    }

    /// Pitch classes of the chord.
    pub fn pitch_classes(&self) -> Vec<u8> {
        self.intervals
            .iter()
            .map(|i| (self.root + i) % 12)
            .collect()
    }
}

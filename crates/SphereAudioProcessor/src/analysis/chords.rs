//! Chord recognition over beats.
//!
//! Harmony is summarised per beat (chroma between one beat and the next), so
//! every chord change lands on the beat grid the arrangement will use.
//! Each beat is scored against chord templates — the treble chroma for the
//! chord's notes, the bass chroma for its root — and a Viterbi pass picks the
//! chord sequence that explains the whole song with the fewest changes,
//! letting chords change more easily on a downbeat than inside a bar. An
//! optional key makes diatonic chords slightly more likely, the way a
//! listener hears an ambiguous beat.
//!
//! Offline / control-thread only.

use serde::{Deserialize, Serialize};

use super::chroma::ChromaFrames;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ChordKind {
    Major,
    Minor,
    Dominant7,
    Major7,
    Minor7,
}

impl ChordKind {
    pub const TRIADS: [ChordKind; 2] = [ChordKind::Major, ChordKind::Minor];
    pub const SEVENTHS: [ChordKind; 5] = [
        ChordKind::Major,
        ChordKind::Minor,
        ChordKind::Dominant7,
        ChordKind::Major7,
        ChordKind::Minor7,
    ];

    /// Chord tones as semitones above the root, with template weights.
    fn tones(self) -> &'static [(u8, f32)] {
        match self {
            ChordKind::Major => &[(0, 1.0), (4, 1.0), (7, 1.0)],
            ChordKind::Minor => &[(0, 1.0), (3, 1.0), (7, 1.0)],
            ChordKind::Dominant7 => &[(0, 1.0), (4, 1.0), (7, 1.0), (10, 0.9)],
            ChordKind::Major7 => &[(0, 1.0), (4, 1.0), (7, 1.0), (11, 0.9)],
            ChordKind::Minor7 => &[(0, 1.0), (3, 1.0), (7, 1.0), (10, 0.9)],
        }
    }

    /// Harte-syntax quality (`maj`, `min`, `7`, …), as used by chord datasets.
    pub fn harte(self) -> &'static str {
        match self {
            ChordKind::Major => "maj",
            ChordKind::Minor => "min",
            ChordKind::Dominant7 => "7",
            ChordKind::Major7 => "maj7",
            ChordKind::Minor7 => "min7",
        }
    }

    fn is_seventh(self) -> bool {
        !matches!(self, ChordKind::Major | ChordKind::Minor)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChordLabel {
    /// Root pitch class, 0 = C.
    pub root: u8,
    pub kind: ChordKind,
}

impl ChordLabel {
    pub fn harte(&self) -> String {
        const NAMES: [&str; 12] = [
            "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
        ];
        format!("{}:{}", NAMES[self.root as usize % 12], self.kind.harte())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChordSegment {
    pub start_seconds: f64,
    pub end_seconds: f64,
    /// `None` where nothing chordal sounds (drums only, silence).
    pub chord: Option<ChordLabel>,
    /// Mean template match over the segment, `0..1`.
    pub confidence: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChordOptions {
    /// Recognise 7th chords as well as triads.
    pub sevenths: bool,
    /// `(tonic pitch class, minor)`: nudges toward diatonic chords.
    pub key: Option<(u8, bool)>,
}

impl Default for ChordOptions {
    fn default() -> Self {
        Self {
            sevenths: true,
            key: None,
        }
    }
}

/// Weight of the bass note matching the chord root.
const BASS_WEIGHT: f32 = 0.35;
/// Score a beat must beat to count as a chord rather than "no chord".
const NO_CHORD_SCORE: f32 = 0.55;
/// Beats quieter than this share of the song's median level are "no chord".
const SILENCE_RATIO: f32 = 0.08;
/// Cost of changing chord inside a bar / on a downbeat, in score units.
const CHANGE_COST: f32 = 0.45;
const CHANGE_COST_DOWNBEAT: f32 = 0.2;
/// Extra evidence a 7th chord needs over its triad.
const SEVENTH_COST: f32 = 0.04;
/// Bonus for chords diatonic to the given key.
const DIATONIC_BONUS: f32 = 0.05;

struct State {
    label: Option<ChordLabel>,
    template: [f32; 12],
    norm: f32,
}

fn states(options: &ChordOptions) -> Vec<State> {
    let kinds: &[ChordKind] = if options.sevenths {
        &ChordKind::SEVENTHS
    } else {
        &ChordKind::TRIADS
    };
    let mut out = vec![State {
        label: None,
        template: [0.0; 12],
        norm: 1.0,
    }];
    for &kind in kinds {
        for root in 0..12u8 {
            let mut template = [0.0_f32; 12];
            for &(tone, w) in kind.tones() {
                template[((root + tone) % 12) as usize] = w;
            }
            let norm = template.iter().map(|v| v * v).sum::<f32>().sqrt();
            out.push(State {
                label: Some(ChordLabel { root, kind }),
                template,
                norm,
            });
        }
    }
    out
}

/// Diatonic chord roots/qualities of a key (triads and 7ths).
fn diatonic(key: (u8, bool), label: ChordLabel) -> bool {
    let (tonic, minor) = key;
    let degree = (label.root + 12 - tonic % 12) % 12;
    use ChordKind::*;
    let allowed: &[(u8, &[ChordKind])] = if minor {
        &[
            (0, &[Minor, Minor7]),
            (3, &[Major, Major7]),
            (5, &[Minor, Minor7]),
            (7, &[Minor, Major, Minor7, Dominant7]),
            (8, &[Major, Major7]),
            (10, &[Major, Dominant7]),
        ]
    } else {
        &[
            (0, &[Major, Major7]),
            (2, &[Minor, Minor7]),
            (4, &[Minor, Minor7]),
            (5, &[Major, Major7]),
            (7, &[Major, Dominant7]),
            (9, &[Minor, Minor7]),
        ]
    };
    allowed
        .iter()
        .any(|(d, kinds)| *d == degree && kinds.contains(&label.kind))
}

/// Recognise chords over `beats` (seconds). `downbeats` marks which beats
/// start a bar (same length as `beats`), or is empty.
pub fn recognize_chords(
    chroma: &ChromaFrames,
    beats: &[f64],
    downbeats: &[bool],
    options: ChordOptions,
) -> Vec<ChordSegment> {
    if beats.len() < 2 || chroma.treble.is_empty() {
        return Vec::new();
    }
    let spans: Vec<(f64, f64)> = beats.windows(2).map(|w| (w[0], w[1])).collect();
    let summaries: Vec<([f32; 12], [f32; 12], f32)> = spans
        .iter()
        .map(|&(a, b)| chroma.span(a, b))
        .collect();
    let mut levels: Vec<f32> = summaries.iter().map(|s| s.0.iter().sum::<f32>()).collect();
    levels.sort_by(|a, b| a.total_cmp(b));
    let median_level = levels[levels.len() / 2].max(1e-9);

    let states = states(&options);
    let k = states.len();
    // Emission: cosine of treble chroma with the template, plus bass support
    // for the root, minus a small cost for the richer 7th templates.
    let emissions: Vec<Vec<f32>> = summaries
        .iter()
        .map(|(treble, bass, _)| {
            let level: f32 = treble.iter().sum();
            let silent = level < median_level * SILENCE_RATIO;
            let t_norm = treble.iter().map(|v| v * v).sum::<f32>().sqrt().max(1e-9);
            let b_max = bass.iter().copied().fold(0.0_f32, f32::max).max(1e-9);
            states
                .iter()
                .map(|state| match state.label {
                    None => {
                        if silent {
                            1.0
                        } else {
                            super::rhythm::tune("cnone", NO_CHORD_SCORE)
                        }
                    }
                    Some(label) => {
                        if silent {
                            return 0.0;
                        }
                        let dot: f32 = treble
                            .iter()
                            .zip(&state.template)
                            .map(|(a, b)| a * b)
                            .sum();
                        let mut score = dot / (t_norm * state.norm);
                        score += super::rhythm::tune("cbass", BASS_WEIGHT) * (bass[label.root as usize] / b_max - 0.5);
                        if label.kind.is_seventh() {
                            score -= super::rhythm::tune("c7", SEVENTH_COST);
                        }
                        if let Some(key) = options.key {
                            if diatonic(key, label) {
                                score += super::rhythm::tune("cdia", DIATONIC_BONUS);
                            }
                        }
                        score
                    }
                })
                .collect()
        })
        .collect();

    // Viterbi: a change costs less on a downbeat.
    let n = emissions.len();
    let mut score = emissions[0].clone();
    let mut back = vec![vec![0u16; k]; n];
    for i in 1..n {
        let cost = if downbeats.get(i).copied().unwrap_or(false) {
            super::rhythm::tune("cdown", CHANGE_COST_DOWNBEAT)
        } else {
            super::rhythm::tune("cchange", CHANGE_COST)
        };
        let (best_prev, best_score) = score
            .iter()
            .enumerate()
            .fold((0, f32::NEG_INFINITY), |acc, (j, &s)| {
                if s > acc.1 { (j, s) } else { acc }
            });
        let mut next = vec![0.0_f32; k];
        for j in 0..k {
            let stay = score[j];
            let (from, v) = if stay >= best_score - cost {
                (j, stay)
            } else {
                (best_prev, best_score - cost)
            };
            next[j] = v + emissions[i][j];
            back[i][j] = from as u16;
        }
        score = next;
    }
    let mut state = (0..k)
        .max_by(|&a, &b| score[a].total_cmp(&score[b]))
        .unwrap_or(0);
    let mut path = vec![0usize; n];
    for i in (0..n).rev() {
        path[i] = state;
        state = back[i][state] as usize;
    }

    // Merge runs.
    let mut segments: Vec<ChordSegment> = Vec::new();
    let mut run_start = 0;
    for i in 1..=n {
        if i == n || path[i] != path[run_start] {
            let s = path[run_start];
            let conf = (run_start..i).map(|b| emissions[b][s]).sum::<f32>() / (i - run_start) as f32;
            segments.push(ChordSegment {
                start_seconds: spans[run_start].0,
                end_seconds: spans[i - 1].1,
                chord: states[s].label,
                confidence: conf.clamp(0.0, 1.0),
            });
            run_start = i;
        }
    }
    segments
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::chroma::chroma_frames;

    const SR: f32 = 22_050.0;

    /// Four chords, one bar each at 120 BPM, voiced with a bass note.
    fn progression(chords: &[(f32, &[f32])], bar_seconds: f32) -> Vec<f32> {
        let per = (bar_seconds * SR) as usize;
        let mut out = vec![0.0_f32; per * chords.len()];
        for (c, (bass, notes)) in chords.iter().enumerate() {
            for k in 0..per {
                let t = (c * per + k) as f32 / SR;
                let env = 1.0 - (k as f32 / per as f32) * 0.3;
                let mut v = 0.3 * (std::f32::consts::TAU * bass * t).sin();
                for f in *notes {
                    // A few harmonics, like a real instrument.
                    for (h, a) in [(1.0, 0.25), (2.0, 0.1), (3.0, 0.05)] {
                        v += a * (std::f32::consts::TAU * f * h * t).sin();
                    }
                }
                out[c * per + k] = v * env * 0.3;
            }
        }
        out
    }

    #[test]
    fn a_pop_progression_is_read_chord_by_chord() {
        // C - Am - F - G, two bars each.
        let c = (65.41, &[261.63, 329.63, 392.0][..]);
        let am = (110.0, &[220.0, 261.63, 329.63][..]);
        let f = (87.31, &[349.23, 440.0, 523.25][..]);
        let g = (98.0, &[392.0, 493.88, 587.33][..]);
        let audio = progression(&[c, c, am, am, f, f, g, g], 2.0);
        let chroma = chroma_frames(&audio, SR).unwrap();
        let beats: Vec<f64> = (0..=32).map(|i| i as f64 * 0.5).collect();
        let downbeats: Vec<bool> = (0..=32).map(|i| i % 4 == 0).collect();
        let segments = recognize_chords(&chroma, &beats, &downbeats, ChordOptions::default());
        let names: Vec<String> = segments
            .iter()
            .filter(|s| s.end_seconds - s.start_seconds >= 1.0)
            .map(|s| s.chord.map(|c| c.harte()).unwrap_or_else(|| "N".into()))
            .collect();
        assert_eq!(names, vec!["C:maj", "A:min", "F:maj", "G:maj"], "{segments:?}");
        // Changes land on the bar lines.
        for s in &segments[1..] {
            let bar = s.start_seconds / 4.0;
            assert!((bar - bar.round()).abs() < 0.13, "{s:?}");
        }
    }

    #[test]
    fn a_dominant_seventh_is_told_from_its_triad() {
        let g7 = (98.0, &[392.0, 493.88, 587.33, 698.46][..]);
        let audio = progression(&[g7, g7], 2.0);
        let chroma = chroma_frames(&audio, SR).unwrap();
        let beats: Vec<f64> = (0..=8).map(|i| i as f64 * 0.5).collect();
        let segments = recognize_chords(&chroma, &beats, &[], ChordOptions::default());
        let main = segments
            .iter()
            .max_by(|a, b| {
                (a.end_seconds - a.start_seconds).total_cmp(&(b.end_seconds - b.start_seconds))
            })
            .unwrap();
        assert_eq!(main.chord.unwrap().harte(), "G:7");
    }

    #[test]
    fn silence_is_no_chord() {
        let mut audio = progression(&[(65.41, &[261.63, 329.63, 392.0][..])], 2.0);
        audio.extend(vec![0.0; (2.0 * SR) as usize]);
        let chroma = chroma_frames(&audio, SR).unwrap();
        let beats: Vec<f64> = (0..=8).map(|i| i as f64 * 0.5).collect();
        let segments = recognize_chords(&chroma, &beats, &[], ChordOptions::default());
        assert_eq!(segments.last().unwrap().chord, None, "{segments:?}");
    }
}

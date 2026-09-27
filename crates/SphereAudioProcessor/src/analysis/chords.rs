//! Chord recognition: timestamped chord segments from local harmony.
//!
//! Global key and local chords are separate questions. The key detector
//! (`key.rs`) integrates pitch evidence over the whole file; this module
//! only ever looks at one short stretch of music at a time, and takes the
//! key at most as a small, optional prior.
//!
//! Pipeline:
//!
//! 1. **Harmonic spans.** The beat grid is cut into spans no longer than
//!    [`MAX_SPAN_SECONDS`] (a long beat is split into equal sub-beats), so
//!    a chord can change between beats and a half-time or double-time beat
//!    grid yields nearly the same spans. Without a beat grid, fixed spans
//!    are used.
//! 2. **Span chroma.** Treble and bass chroma (peak-based, tuning
//!    corrected, overtones damped — see `chroma.rs`) summed over the span.
//! 3. **Chord likelihoods.** Each chord is a *probability template*: its
//!    tones share `1 − NON_CHORD_MASS` of the pitch-class distribution, the
//!    other pitch classes share the rest. The span's chroma is sharpened
//!    (raised to [`CHROMA_SHARPEN`], so the notes actually held stand out
//!    of the leakage every dense mix spreads over the scale), normalised to
//!    a distribution, and scored by cross-entropy against every template;
//!    no-chord is the uniform distribution. Because every template spreads
//!    the same probability, a four-note chord dilutes each of its notes: a
//!    seventh only beats its triad when the seventh really sounds about as
//!    strongly as the triad's notes — not because a dense mix leaks a
//!    little of every scale note (the old cosine templates rewarded every
//!    extra note, which is how whole songs read as one m7 chord). A chord
//!    must also explain most of the span's (sharpened) pitch energy to beat
//!    no-chord, so drums, noise and speech read as N. The bass chroma is
//!    scored the same way against a bass template that favours the root.
//! 4. **Decoding.** Likelihoods are evidence per second, so a span's weight
//!    is its duration, and a chord change costs a fixed amount of evidence
//!    (less on a downbeat, more between beats) — the same whether the beat
//!    grid runs at 85 or at 170 BPM. A Viterbi pass picks the chord
//!    sequence; consecutive spans with the same chord merge into one
//!    segment.
//! 5. **Confidence.** Per segment, from the evidence margin over the best
//!    other chord, per second — see [`ChordSegment::confidence`].
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
    Sus2,
    Sus4,
    Diminished,
    Augmented,
}

impl ChordKind {
    /// Major and minor triads only.
    pub const TRIADS: [ChordKind; 2] = [ChordKind::Major, ChordKind::Minor];
    /// Triads and the three common sevenths.
    pub const SEVENTHS: [ChordKind; 5] = [
        ChordKind::Major,
        ChordKind::Minor,
        ChordKind::Dominant7,
        ChordKind::Major7,
        ChordKind::Minor7,
    ];
    /// Every kind this module recognises.
    pub const ALL: [ChordKind; 9] = [
        ChordKind::Major,
        ChordKind::Minor,
        ChordKind::Dominant7,
        ChordKind::Major7,
        ChordKind::Minor7,
        ChordKind::Sus2,
        ChordKind::Sus4,
        ChordKind::Diminished,
        ChordKind::Augmented,
    ];

    /// Chord tones as semitones above the root.
    pub fn intervals(self) -> &'static [u8] {
        match self {
            ChordKind::Major => &[0, 4, 7],
            ChordKind::Minor => &[0, 3, 7],
            ChordKind::Dominant7 => &[0, 4, 7, 10],
            ChordKind::Major7 => &[0, 4, 7, 11],
            ChordKind::Minor7 => &[0, 3, 7, 10],
            ChordKind::Sus2 => &[0, 2, 7],
            ChordKind::Sus4 => &[0, 5, 7],
            ChordKind::Diminished => &[0, 3, 6],
            ChordKind::Augmented => &[0, 4, 8],
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
            ChordKind::Sus2 => "sus2",
            ChordKind::Sus4 => "sus4",
            ChordKind::Diminished => "dim",
            ChordKind::Augmented => "aug",
        }
    }

    /// Lead-sheet suffix (`""`, `m`, `7`, `maj7`, `m7`, `sus2`, …).
    pub fn suffix(self) -> &'static str {
        match self {
            ChordKind::Major => "",
            ChordKind::Minor => "m",
            ChordKind::Dominant7 => "7",
            ChordKind::Major7 => "maj7",
            ChordKind::Minor7 => "m7",
            ChordKind::Sus2 => "sus2",
            ChordKind::Sus4 => "sus4",
            ChordKind::Diminished => "dim",
            ChordKind::Augmented => "aug",
        }
    }

    /// Evidence, per second, a chord of this kind gives up against a plain
    /// triad: the vocabulary prior. Popular music is mostly major and minor
    /// triads; sevenths are common, suspended chords less so, diminished
    /// and augmented triads rare. A rarer kind has to be heard a little more
    /// clearly to be named.
    fn prior_cost(self) -> f32 {
        match self {
            ChordKind::Major | ChordKind::Minor => 0.0,
            ChordKind::Dominant7 | ChordKind::Major7 | ChordKind::Minor7 => SEVENTH_COST,
            ChordKind::Sus2 | ChordKind::Sus4 => SUS_COST,
            ChordKind::Diminished | ChordKind::Augmented => RARE_COST,
        }
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
        format!("{}:{}", NAMES[self.root as usize % 12], self.kind.harte())
    }

    /// Lead-sheet name with sharps, e.g. `F#m7`.
    pub fn name(&self) -> String {
        format!("{}{}", NAMES[self.root as usize % 12], self.kind.suffix())
    }

    /// Pitch classes of the chord.
    pub fn pitch_classes(&self) -> impl Iterator<Item = u8> + '_ {
        self.kind
            .intervals()
            .iter()
            .map(move |i| (self.root + i) % 12)
    }
}

const NAMES: [&str; 12] = [
    "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
];

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChordSegment {
    pub start_seconds: f64,
    pub end_seconds: f64,
    /// `None` where nothing chordal sounds (drums only, silence, speech).
    pub chord: Option<ChordLabel>,
    /// How clearly the audio names this chord, `0..1`: the evidence margin
    /// per second over the best other reading (another chord or no-chord),
    /// mapped so a margin of [`CONFIDENT_MARGIN`] reads 0.5. A score, not a
    /// probability; [`ChordSegment::certainty`] buckets it.
    pub confidence: f32,
}

/// Coarse certainty of a segment, for display.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Certainty {
    Low,
    Medium,
    High,
}

impl ChordSegment {
    pub fn certainty(&self) -> Certainty {
        if self.confidence >= HIGH_CONFIDENCE {
            Certainty::High
        } else if self.confidence >= MEDIUM_CONFIDENCE {
            Certainty::Medium
        } else {
            Certainty::Low
        }
    }
}

/// The key, as a soft prior for chord decoding.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct KeyContext {
    /// Tonic pitch class, 0 = C.
    pub tonic: u8,
    pub minor: bool,
}

impl KeyContext {
    /// Whether the chord is built on the key's scale: the diatonic triads
    /// and sevenths, and in minor also the harmonic-minor dominant (V, V7).
    /// Anything else — secondary dominants, borrowed chords, a modulation —
    /// is still recognised; it only misses the small prior bonus.
    pub fn is_diatonic(&self, chord: ChordLabel) -> bool {
        let scale: [u8; 7] = if self.minor {
            [0, 2, 3, 5, 7, 8, 10]
        } else {
            [0, 2, 4, 5, 7, 9, 11]
        };
        let in_scale = |pc: u8| scale.contains(&((pc + 12 - self.tonic) % 12));
        if chord.pitch_classes().all(in_scale) {
            return true;
        }
        self.minor
            && chord.root == (self.tonic + 7) % 12
            && matches!(chord.kind, ChordKind::Major | ChordKind::Dominant7)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChordOptions {
    /// Recognise 7th chords (7, maj7, m7) as well as triads.
    pub sevenths: bool,
    /// Recognise suspended, diminished and augmented triads.
    #[serde(default = "default_true")]
    pub extended: bool,
    /// The song's key, used as a small prior toward its diatonic chords.
    #[serde(default)]
    pub key: Option<KeyContext>,
}

fn default_true() -> bool {
    true
}

impl Default for ChordOptions {
    fn default() -> Self {
        Self {
            sevenths: true,
            extended: true,
            key: None,
        }
    }
}

// Model constants. Each has a meaning of its own; the values were set on
// the development split of the chord corpus (examples/chord_corpus.rs,
// examples/chord_benchmark.rs), confirmed once on the validation split, and
// the test split was left for the final report. Accuracy is flat around
// every chosen value (see the benchmark notes in the commit), so none is
// balanced on a knife edge.

/// Share of a span's pitch-class distribution a chord template leaves to
/// notes outside the chord (melody, passing tones, reverb of the last
/// chord).
const NON_CHORD_MASS: f32 = 0.18;
/// Exponent applied to span chroma before it is normalised: contrast
/// between the notes held and the leakage of the rest of the scale.
const CHROMA_SHARPEN: f32 = 1.5;
/// Bass template: share on the root, and on notes outside the chord; the
/// other chord tones (inversions) share the rest.
const BASS_ROOT_MASS: f32 = 0.5;
const BASS_NON_CHORD_MASS: f32 = 0.15;
/// Weight of the bass likelihood against the treble's. Below 1: the bass
/// confirms a root, it does not name the chord on its own.
const BASS_WEIGHT: f32 = 0.5;
/// Evidence per second of music, in the likelihood's units: how strongly a
/// second of chroma counts against a chord change (with [`CHANGE_COST`],
/// about a third of a second of clear evidence pays for a change on a
/// beat). Labelled accuracy is flat from 6.5 to 10; the low end is used
/// because on real mixes, where a sung note can outweigh the chord for a
/// beat, it halves one-beat flicker.
const EVIDENCE_PER_SECOND: f32 = 6.5;
/// Cost of a chord change, in evidence units: on a beat, on a downbeat
/// (chords change on bar lines most often) and between beats.
const CHANGE_COST: f32 = 2.0;
const CHANGE_COST_DOWNBEAT: f32 = 1.2;
const CHANGE_COST_OFFBEAT: f32 = 3.0;
/// Vocabulary prior, evidence per second given up against a triad.
const SEVENTH_COST: f32 = 0.08;
/// Suspended, diminished and augmented triads share two notes with a major
/// or minor triad on the same root; below this cost they took over plain
/// triads whenever a melody note touched the 2nd or 4th.
const SUS_COST: f32 = 0.6;
const RARE_COST: f32 = 0.6;
/// Key prior: evidence per second a diatonic chord gains. Small against
/// the difference between a right and a wrong chord (~0.5–1 per second),
/// so it only settles near ties.
const KEY_BONUS: f32 = 0.05;
/// Longest harmonic span; longer beats are split into sub-beats.
const MAX_SPAN_SECONDS: f64 = 0.4;
/// Span length without a beat grid.
const FALLBACK_SPAN_SECONDS: f64 = 0.25;
/// Spans quieter (broadband) than this share of the song's typical level
/// are silence: no-chord.
const SILENCE_RATIO: f32 = 0.05;
/// Bass chroma counts only when it holds at least this share of the
/// span's total (treble + bass) pitch weight.
const MIN_BASS_SHARE: f32 = 0.05;
/// Evidence margin per second that maps to confidence 0.5.
const CONFIDENT_MARGIN: f32 = 0.3;
/// Confidence buckets, from how often segments were right on the corpus's
/// development split: at 0.3 and above about 94% of chord time was correct
/// (root and maj/min), 0.15–0.3 about 88%, below that about 82% — never a
/// sure thing, which is why the top bucket is "high", not "certain".
const HIGH_CONFIDENCE: f32 = 0.3;
const MEDIUM_CONFIDENCE: f32 = 0.15;
/// Candidates kept per span and per segment in [`ChordAnalysis`].
const CANDIDATES: usize = 5;

struct State {
    label: Option<ChordLabel>,
    /// ln of the treble template per pitch class.
    treble: [f32; 12],
    /// ln of the bass template per pitch class.
    bass: [f32; 12],
    /// Vocabulary and key prior, evidence per second.
    prior: f32,
}

fn states(options: &ChordOptions) -> Vec<State> {
    let mut kinds: Vec<ChordKind> = if options.sevenths {
        ChordKind::SEVENTHS.to_vec()
    } else {
        ChordKind::TRIADS.to_vec()
    };
    if options.extended {
        kinds.extend([
            ChordKind::Sus2,
            ChordKind::Sus4,
            ChordKind::Diminished,
            ChordKind::Augmented,
        ]);
    }
    let uniform = (1.0f32 / 12.0).ln();
    let mut out = vec![State {
        label: None,
        treble: [uniform; 12],
        bass: [uniform; 12],
        prior: 0.0,
    }];
    for kind in kinds {
        for root in 0..12u8 {
            let label = ChordLabel { root, kind };
            let tones: Vec<u8> = label.pitch_classes().collect();
            let n = tones.len() as f32;
            let mut treble = [(NON_CHORD_MASS / (12.0 - n)).ln(); 12];
            let mut bass = [(BASS_NON_CHORD_MASS / (12.0 - n)).ln(); 12];
            let other = ((1.0 - BASS_ROOT_MASS - BASS_NON_CHORD_MASS) / (n - 1.0)).ln();
            for &pc in &tones {
                treble[pc as usize] = ((1.0 - NON_CHORD_MASS) / n).ln();
                bass[pc as usize] = other;
            }
            bass[root as usize] = BASS_ROOT_MASS.ln();
            let mut prior = -kind.prior_cost();
            if options.key.is_some_and(|k| k.is_diatonic(label)) {
                prior += KEY_BONUS;
            }
            out.push(State {
                label: Some(label),
                treble,
                bass,
                prior,
            });
        }
    }
    out
}

/// One harmonic span: its chroma, and the evidence for every state.
#[derive(Clone, Debug)]
pub struct HarmonicSpan {
    pub start_seconds: f64,
    pub end_seconds: f64,
    /// Summed treble and bass chroma over the span.
    pub chroma: [f32; 12],
    pub bass: [f32; 12],
    /// Whether the span starts on a beat / a downbeat.
    pub on_beat: bool,
    pub downbeat: bool,
    /// The strongest readings, `(chord, evidence per second)`, best first.
    pub candidates: Vec<(Option<ChordLabel>, f32)>,
}

/// Chord analysis with its diagnostics: the segments, the harmonic spans
/// they were decoded from, and per segment the best readings with their
/// mean evidence per second. The production path ([`recognize_chords`])
/// keeps only the segments.
#[derive(Clone, Debug, Default)]
pub struct ChordAnalysis {
    pub segments: Vec<ChordSegment>,
    /// Per segment, the strongest readings over its whole length.
    pub segment_candidates: Vec<Vec<(Option<ChordLabel>, f32)>>,
    pub spans: Vec<HarmonicSpan>,
    /// Tuning offset removed from the chroma, cents.
    pub tuning_cents: f32,
}

/// Recognise chords over `beats` (seconds). `downbeats` marks which beats
/// start a bar (same length as `beats`), or is empty. With fewer than two
/// beats the audio is cut into fixed spans instead.
pub fn recognize_chords(
    chroma: &ChromaFrames,
    beats: &[f64],
    downbeats: &[bool],
    options: ChordOptions,
) -> Vec<ChordSegment> {
    analyze_chords(chroma, beats, downbeats, options).segments
}

/// [`recognize_chords`] with diagnostics.
pub fn analyze_chords(
    chroma: &ChromaFrames,
    beats: &[f64],
    downbeats: &[bool],
    options: ChordOptions,
) -> ChordAnalysis {
    let spans = harmonic_spans(chroma, beats, downbeats);
    if spans.is_empty() {
        return ChordAnalysis::default();
    }
    let states = states(&options);
    let k = states.len();

    // Span chroma and the song's typical broadband level.
    let summaries: Vec<([f32; 12], [f32; 12], f32)> =
        spans.iter().map(|s| chroma.span(s.start, s.end)).collect();
    let mut levels: Vec<f32> = summaries.iter().map(|s| s.2).collect();
    levels.sort_by(|a, b| a.total_cmp(b));
    let typical = levels[levels.len() * 3 / 4].max(1e-12);

    // Evidence per second for every span and state.
    let per_second: Vec<Vec<f32>> = summaries
        .iter()
        .map(|(treble, bass, level)| {
            if *level < typical * SILENCE_RATIO {
                return states
                    .iter()
                    .map(|s| if s.label.is_none() { 0.0 } else { -10.0 })
                    .collect();
            }
            let treble: [f32; 12] = std::array::from_fn(|i| treble[i].powf(CHROMA_SHARPEN));
            let bass: [f32; 12] = std::array::from_fn(|i| bass[i].powf(CHROMA_SHARPEN));
            let t_sum: f32 = treble.iter().sum();
            let b_sum: f32 = bass.iter().sum();
            if t_sum <= f32::EPSILON {
                return states
                    .iter()
                    .map(|s| if s.label.is_none() { 0.0 } else { -10.0 })
                    .collect();
            }
            let use_bass = b_sum >= MIN_BASS_SHARE * (t_sum + b_sum);
            states
                .iter()
                .map(|state| {
                    let mut e: f32 = (0..12)
                        .map(|pc| treble[pc] / t_sum * state.treble[pc])
                        .sum();
                    if use_bass {
                        e += BASS_WEIGHT
                            * (0..12)
                                .map(|pc| bass[pc] / b_sum * state.bass[pc])
                                .sum::<f32>();
                    }
                    e + state.prior
                })
                .collect()
        })
        .collect();

    // Viterbi over spans: evidence weighted by duration, a fixed cost per
    // change.
    let n = spans.len();
    let weight = |i: usize| EVIDENCE_PER_SECOND * (spans[i].end - spans[i].start) as f32;
    let mut score: Vec<f32> = per_second[0].iter().map(|e| e * weight(0)).collect();
    let mut back = vec![vec![0u16; k]; n];
    for i in 1..n {
        let cost = if spans[i].downbeat {
            CHANGE_COST_DOWNBEAT
        } else if spans[i].on_beat {
            CHANGE_COST
        } else {
            CHANGE_COST_OFFBEAT
        };
        let (best_prev, best_score) =
            score
                .iter()
                .enumerate()
                .fold(
                    (0, f32::NEG_INFINITY),
                    |acc, (j, &s)| {
                        if s > acc.1 { (j, s) } else { acc }
                    },
                );
        let w = weight(i);
        let mut next = vec![0.0_f32; k];
        for j in 0..k {
            let stay = score[j];
            let (from, v) = if stay >= best_score - cost {
                (j, stay)
            } else {
                (best_prev, best_score - cost)
            };
            next[j] = v + per_second[i][j] * w;
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

    let top = |evidence: &[f32]| -> Vec<(Option<ChordLabel>, f32)> {
        let mut order: Vec<usize> = (0..k).collect();
        order.sort_by(|&a, &b| evidence[b].total_cmp(&evidence[a]));
        order
            .into_iter()
            .take(CANDIDATES)
            .map(|s| (states[s].label, evidence[s]))
            .collect()
    };

    // Merge runs into segments.
    let mut segments = Vec::new();
    let mut segment_candidates = Vec::new();
    let mut run = 0;
    for i in 1..=n {
        if i < n && path[i] == path[run] {
            continue;
        }
        let s = path[run];
        let seconds: f32 = (run..i)
            .map(|j| (spans[j].end - spans[j].start) as f32)
            .sum::<f32>()
            .max(1e-6);
        // Mean evidence per second over the segment, per state.
        let mean: Vec<f32> = (0..k)
            .map(|state| {
                (run..i)
                    .map(|j| per_second[j][state] * (spans[j].end - spans[j].start) as f32)
                    .sum::<f32>()
                    / seconds
            })
            .collect();
        let rival = (0..k)
            .filter(|&o| o != s)
            .map(|o| mean[o])
            .fold(f32::NEG_INFINITY, f32::max);
        let margin = (mean[s] - rival).max(0.0);
        segments.push(ChordSegment {
            start_seconds: spans[run].start,
            end_seconds: spans[i - 1].end,
            chord: states[s].label,
            confidence: margin / (margin + CONFIDENT_MARGIN),
        });
        segment_candidates.push(top(&mean));
        run = i;
    }

    ChordAnalysis {
        segments,
        segment_candidates,
        spans: spans
            .iter()
            .zip(&summaries)
            .zip(&per_second)
            .map(|((span, (treble, bass, _)), evidence)| HarmonicSpan {
                start_seconds: span.start,
                end_seconds: span.end,
                chroma: *treble,
                bass: *bass,
                on_beat: span.on_beat,
                downbeat: span.downbeat,
                candidates: top(evidence),
            })
            .collect(),
        tuning_cents: chroma.tuning * 100.0,
    }
}

#[derive(Clone, Copy, Debug)]
struct Span {
    start: f64,
    end: f64,
    on_beat: bool,
    downbeat: bool,
}

/// Beats cut into spans of at most [`MAX_SPAN_SECONDS`]; fixed spans over
/// the chroma's extent when there is no beat grid.
fn harmonic_spans(chroma: &ChromaFrames, beats: &[f64], downbeats: &[bool]) -> Vec<Span> {
    if chroma.treble.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    if beats.len() < 2 {
        let end = chroma.start_seconds + chroma.treble.len() as f64 * chroma.hop_seconds;
        let mut t = 0.0;
        while t < end {
            out.push(Span {
                start: t,
                end: (t + FALLBACK_SPAN_SECONDS).min(end),
                on_beat: false,
                downbeat: false,
            });
            t += FALLBACK_SPAN_SECONDS;
        }
        return out;
    }
    for (i, w) in beats.windows(2).enumerate() {
        let (a, b) = (w[0], w[1]);
        if b <= a {
            continue;
        }
        let parts = ((b - a) / MAX_SPAN_SECONDS).ceil().max(1.0) as usize;
        let step = (b - a) / parts as f64;
        for p in 0..parts {
            out.push(Span {
                start: a + p as f64 * step,
                end: a + (p + 1) as f64 * step,
                on_beat: p == 0,
                downbeat: p == 0 && downbeats.get(i).copied().unwrap_or(false),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::chroma::chroma_frames;

    const SR: f32 = 22_050.0;

    fn midi_hz(midi: f32) -> f32 {
        440.0 * 2f32.powf((midi - 69.0) / 12.0)
    }

    /// Chords as (bass MIDI note, chord MIDI notes), one per `seconds`,
    /// each note with a few harmonics like a real instrument, detuned by
    /// `cents`.
    fn render(chords: &[(f32, &[f32])], seconds: f32, cents: f32) -> Vec<f32> {
        let per = (seconds * SR) as usize;
        let detune = cents / 100.0;
        let mut out = vec![0.0_f32; per * chords.len()];
        for (c, (bass, notes)) in chords.iter().enumerate() {
            for k in 0..per {
                let t = k as f32 / SR;
                let env = 1.0 - (k as f32 / per as f32) * 0.3;
                let mut v = 0.3 * (std::f32::consts::TAU * midi_hz(bass + detune) * t).sin();
                for &m in *notes {
                    let f = midi_hz(m + detune);
                    for (h, a) in [(1.0, 0.25), (2.0, 0.1), (3.0, 0.05)] {
                        v += a * (std::f32::consts::TAU * f * h * t).sin();
                    }
                }
                out[c * per + k] = v * env * 0.3;
            }
        }
        out
    }

    fn beats(seconds: f64, period: f64) -> Vec<f64> {
        (0..=(seconds / period) as usize)
            .map(|i| i as f64 * period)
            .collect()
    }

    /// The chord held longest.
    fn main_chord(segments: &[ChordSegment]) -> Option<String> {
        segments
            .iter()
            .max_by(|a, b| {
                (a.end_seconds - a.start_seconds).total_cmp(&(b.end_seconds - b.start_seconds))
            })
            .and_then(|s| s.chord)
            .map(|c| c.name())
    }

    fn read_one(bass: f32, notes: &[f32]) -> Option<String> {
        let audio = render(&[(bass, notes)], 4.0, 0.0);
        let chroma = chroma_frames(&audio, SR).unwrap();
        main_chord(&recognize_chords(
            &chroma,
            &beats(4.0, 0.5),
            &[],
            ChordOptions::default(),
        ))
    }

    #[test]
    fn triads_sevenths_and_suspensions_are_named() {
        // C3/C E G, A2/A C E, G2/G B D F, C3/C E G B, A2/A C E G, D3/D E A,
        // D3/D G A.
        assert_eq!(read_one(48.0, &[60.0, 64.0, 67.0]).as_deref(), Some("C"));
        assert_eq!(read_one(45.0, &[57.0, 60.0, 64.0]).as_deref(), Some("Am"));
        assert_eq!(
            read_one(43.0, &[55.0, 59.0, 62.0, 65.0]).as_deref(),
            Some("G7")
        );
        assert_eq!(
            read_one(48.0, &[60.0, 64.0, 67.0, 71.0]).as_deref(),
            Some("Cmaj7")
        );
        assert_eq!(
            read_one(45.0, &[57.0, 60.0, 64.0, 67.0]).as_deref(),
            Some("Am7")
        );
        assert_eq!(
            read_one(50.0, &[62.0, 64.0, 69.0]).as_deref(),
            Some("Dsus2")
        );
        assert_eq!(
            read_one(50.0, &[62.0, 67.0, 69.0]).as_deref(),
            Some("Dsus4")
        );
    }

    #[test]
    fn a_faint_seventh_does_not_turn_a_triad_into_a_seventh_chord() {
        // E minor with a D far below the triad's notes (a passing tone).
        let per = (4.0 * SR) as usize;
        let mut audio = render(&[(40.0, &[64.0, 67.0, 71.0])], 4.0, 0.0);
        for (k, v) in audio.iter_mut().enumerate().take(per) {
            let t = k as f32 / SR;
            *v += 0.012 * (std::f32::consts::TAU * midi_hz(74.0) * t).sin();
        }
        let chroma = chroma_frames(&audio, SR).unwrap();
        let segments = recognize_chords(&chroma, &beats(4.0, 0.5), &[], ChordOptions::default());
        assert_eq!(main_chord(&segments).as_deref(), Some("Em"));
    }

    #[test]
    fn a_pop_progression_is_read_chord_by_chord_on_the_bar_lines() {
        let c = (48.0, &[60.0, 64.0, 67.0][..]);
        let am = (45.0, &[57.0, 60.0, 64.0][..]);
        let f = (41.0, &[53.0, 57.0, 60.0][..]);
        let g = (43.0, &[55.0, 59.0, 62.0][..]);
        let audio = render(&[c, c, am, am, f, f, g, g], 2.0, 0.0);
        let chroma = chroma_frames(&audio, SR).unwrap();
        let b = beats(16.0, 0.5);
        let downbeats: Vec<bool> = (0..b.len()).map(|i| i % 4 == 0).collect();
        let segments = recognize_chords(&chroma, &b, &downbeats, ChordOptions::default());
        let names: Vec<String> = segments
            .iter()
            .filter(|s| s.end_seconds - s.start_seconds >= 1.0)
            .map(|s| s.chord.map(|c| c.name()).unwrap_or_else(|| "N".into()))
            .collect();
        assert_eq!(names, vec!["C", "Am", "F", "G"], "{segments:?}");
        for s in &segments[1..] {
            let bar = s.start_seconds / 4.0;
            assert!((bar - bar.round()).abs() < 0.13, "{s:?}");
        }
    }

    #[test]
    fn a_held_chord_stays_one_segment_through_a_short_foreign_note() {
        // Four seconds of C major with a loud 150 ms F#-A# blip in the middle.
        let mut audio = render(&[(48.0, &[60.0, 64.0, 67.0])], 4.0, 0.0);
        let (a, b) = ((2.0 * SR) as usize, (2.15 * SR) as usize);
        for (k, v) in audio.iter_mut().enumerate().take(b).skip(a) {
            let t = k as f32 / SR;
            *v += 0.2 * (std::f32::consts::TAU * midi_hz(66.0) * t).sin()
                + 0.2 * (std::f32::consts::TAU * midi_hz(70.0) * t).sin();
        }
        let chroma = chroma_frames(&audio, SR).unwrap();
        let segments = recognize_chords(&chroma, &beats(4.0, 0.5), &[], ChordOptions::default());
        let chords: Vec<String> = segments
            .iter()
            .filter_map(|s| s.chord.map(|c| c.name()))
            .collect();
        assert_eq!(chords, vec!["C"], "{segments:?}");
    }

    #[test]
    fn the_bass_note_does_not_rename_an_inversion() {
        // C major over E in the bass (C/E): still C, not Em.
        assert_eq!(read_one(40.0, &[60.0, 64.0, 67.0]).as_deref(), Some("C"));
    }

    #[test]
    fn detuned_audio_is_read_after_tuning_correction() {
        let audio = render(&[(45.0, &[57.0, 60.0, 64.0])], 4.0, 35.0);
        let chroma = chroma_frames(&audio, SR).unwrap();
        let analysis = analyze_chords(&chroma, &beats(4.0, 0.5), &[], ChordOptions::default());
        assert_eq!(main_chord(&analysis.segments).as_deref(), Some("Am"));
        assert!(
            (analysis.tuning_cents - 35.0).abs() < 8.0,
            "{}",
            analysis.tuning_cents
        );
    }

    #[test]
    fn silence_and_noise_are_no_chord() {
        let mut audio = render(&[(48.0, &[60.0, 64.0, 67.0])], 2.0, 0.0);
        audio.extend(vec![0.0; (2.0 * SR) as usize]);
        let mut seed = 1u32;
        audio.extend((0..(3.0 * SR) as usize).map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            (seed as f32 / u32::MAX as f32 - 0.5) * 0.3
        }));
        let chroma = chroma_frames(&audio, SR).unwrap();
        let segments = recognize_chords(&chroma, &beats(7.0, 0.5), &[], ChordOptions::default());
        let at = |t: f64| {
            segments
                .iter()
                .find(|s| s.start_seconds <= t && t < s.end_seconds)
                .and_then(|s| s.chord)
        };
        assert_eq!(at(1.0).map(|c| c.name()).as_deref(), Some("C"));
        assert_eq!(at(3.0), None, "silence: {segments:?}");
        assert_eq!(at(5.5), None, "noise: {segments:?}");
    }

    #[test]
    fn a_half_time_beat_grid_reads_the_same_chords() {
        let c = (48.0, &[60.0, 64.0, 67.0][..]);
        let g = (43.0, &[55.0, 59.0, 62.0][..]);
        let audio = render(&[c, g, c, g], 1.5, 0.0);
        let chroma = chroma_frames(&audio, SR).unwrap();
        let read = |period: f64| -> Vec<(String, f64)> {
            recognize_chords(&chroma, &beats(6.0, period), &[], ChordOptions::default())
                .iter()
                .filter_map(|s| {
                    s.chord
                        .map(|c| (c.name(), (s.start_seconds * 10.0).round()))
                })
                .collect()
        };
        assert_eq!(read(0.375), read(0.75));
    }

    #[test]
    fn a_non_diatonic_chord_is_still_named_under_a_key_prior() {
        // B-flat major in C major: outside the key, but plainly heard.
        let audio = render(&[(46.0, &[58.0, 62.0, 65.0])], 4.0, 0.0);
        let chroma = chroma_frames(&audio, SR).unwrap();
        let options = ChordOptions {
            key: Some(KeyContext {
                tonic: 0,
                minor: false,
            }),
            ..ChordOptions::default()
        };
        let segments = recognize_chords(&chroma, &beats(4.0, 0.5), &[], options);
        assert_eq!(main_chord(&segments).as_deref(), Some("A#"));
    }

    #[test]
    fn without_beats_chords_are_still_found() {
        let c = (48.0, &[60.0, 64.0, 67.0][..]);
        let f = (41.0, &[53.0, 57.0, 60.0][..]);
        let audio = render(&[c, f], 2.0, 0.0);
        let chroma = chroma_frames(&audio, SR).unwrap();
        let names: Vec<String> = recognize_chords(&chroma, &[], &[], ChordOptions::default())
            .iter()
            .filter(|s| s.end_seconds - s.start_seconds >= 1.0)
            .filter_map(|s| s.chord.map(|c| c.name()))
            .collect();
        assert_eq!(names, vec!["C", "F"]);
    }
}

//! Performance patterns: how a chord progression is played.
//!
//! A progression says *which* chords sound and when; a performance pattern
//! says *how* — held, pulsed, arpeggiated, strummed, and what the bass does
//! under it. Patterns are one bar long and repeat from the start of the
//! stretch they are placed on, so a pattern dropped at bar 3 starts its bar
//! at bar 3. Every note ends at the next chord change: a pattern never holds
//! a chord's notes into the next chord.
//!
//! [`render_performance`] turns chord spans plus pattern spans into MIDI
//! notes. Where no pattern covers a chord, that part plays nothing — an
//! empty lane is silence, not a default.
//!
//! UI/control path only (no realtime use).

use serde::{Deserialize, Serialize};

use crate::chords::{Chord, VoicingOptions, voice_progression};

/// Beats in a pattern bar.
pub const PATTERN_BEATS: f64 = 4.0;
/// Lowest bass note (C2); bass notes sit in the octave above it.
const BASS_LOW: i32 = 36;
/// Delay between strummed voices, beats (~12 ms at 120 BPM).
const STRUM_STEP: f64 = 0.025;
/// Gap before the next hit so a repeated pitch re-attacks.
const GAP: f64 = 0.02;

/// How the chord's upper voices are played.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CompPattern {
    /// Held for as long as the chord lasts.
    Sustain,
    /// Half notes.
    Halves,
    /// Quarter-note pulse.
    Quarters,
    /// Eighth-note pulse, accented on the beat.
    Eighths,
    /// Off-beat eighths (the "and" of every beat).
    Offbeats,
    /// Eighth-note arpeggio, low to high.
    ArpeggioUp,
    /// Eighth-note arpeggio, up and back down.
    ArpeggioUpDown,
    /// Guitar strum: down on 1, up on 2-and, down on 3-and and 4.
    Strum,
    /// 3+3+2 eighths: hits on 1, the and of 2, and 4.
    Syncopated,
}

impl CompPattern {
    pub const ALL: [CompPattern; 9] = [
        CompPattern::Sustain,
        CompPattern::Halves,
        CompPattern::Quarters,
        CompPattern::Eighths,
        CompPattern::Offbeats,
        CompPattern::ArpeggioUp,
        CompPattern::ArpeggioUpDown,
        CompPattern::Strum,
        CompPattern::Syncopated,
    ];

    pub fn label(self) -> &'static str {
        match self {
            CompPattern::Sustain => "Sustain",
            CompPattern::Halves => "Halves",
            CompPattern::Quarters => "Quarters",
            CompPattern::Eighths => "Eighths",
            CompPattern::Offbeats => "Off-beats",
            CompPattern::ArpeggioUp => "Arpeggio up",
            CompPattern::ArpeggioUpDown => "Arpeggio up-down",
            CompPattern::Strum => "Strum",
            CompPattern::Syncopated => "3 + 3 + 2",
        }
    }

    /// Hit times within the bar, beats — for drawing the pattern's rhythm.
    pub fn onsets(self) -> Vec<f64> {
        self.hits().iter().map(|h| h.at).collect()
    }

    fn hits(self) -> Vec<Hit> {
        let every = |step: f64, offset: f64, length: f64| -> Vec<Hit> {
            let mut out = Vec::new();
            let mut at = offset;
            while at < PATTERN_BEATS - 1e-9 {
                out.push(Hit {
                    at,
                    length,
                    voices: Voices::All,
                    velocity: if at.fract() < 1e-9 { 92 } else { 76 },
                });
                at += step;
            }
            out
        };
        match self {
            CompPattern::Sustain => vec![Hit {
                at: 0.0,
                length: PATTERN_BEATS,
                voices: Voices::All,
                velocity: 84,
            }],
            CompPattern::Halves => every(2.0, 0.0, 2.0),
            CompPattern::Quarters => every(1.0, 0.0, 0.9),
            CompPattern::Eighths => every(0.5, 0.0, 0.45),
            CompPattern::Offbeats => every(1.0, 0.5, 0.4),
            CompPattern::ArpeggioUp | CompPattern::ArpeggioUpDown => (0..8)
                .map(|step| Hit {
                    at: step as f64 * 0.5,
                    length: 0.5,
                    voices: Voices::Step {
                        step,
                        bounce: self == CompPattern::ArpeggioUpDown,
                    },
                    velocity: if step % 2 == 0 { 88 } else { 74 },
                })
                .collect(),
            CompPattern::Strum => [
                (0.0, 1.5, 96, true),
                (1.5, 1.0, 72, false),
                (2.5, 1.0, 84, true),
                (3.5, 0.5, 70, false),
            ]
            .into_iter()
            .map(|(at, length, velocity, down)| Hit {
                at,
                length,
                voices: Voices::Strum { down },
                velocity,
            })
            .collect(),
            CompPattern::Syncopated => [(0.0, 1.5, 94), (1.5, 1.5, 84), (3.0, 1.0, 88)]
                .into_iter()
                .map(|(at, length, velocity)| Hit {
                    at,
                    length,
                    voices: Voices::All,
                    velocity,
                })
                .collect(),
        }
    }
}

/// What the bass plays under each chord.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BassPattern {
    /// The chord's bass note, held.
    Root,
    /// The bass note on every beat.
    RootPulse,
    /// Bass note on 1, the fifth on 3.
    RootFifth,
    /// Eighth-note octaves.
    Octaves,
    /// A walking line: bass note, third, fifth, then a half step into the
    /// next chord.
    Walking,
}

impl BassPattern {
    pub const ALL: [BassPattern; 5] = [
        BassPattern::Root,
        BassPattern::RootPulse,
        BassPattern::RootFifth,
        BassPattern::Octaves,
        BassPattern::Walking,
    ];

    pub fn label(self) -> &'static str {
        match self {
            BassPattern::Root => "Root",
            BassPattern::RootPulse => "Root pulse",
            BassPattern::RootFifth => "Root – fifth",
            BassPattern::Octaves => "Octaves",
            BassPattern::Walking => "Walking",
        }
    }

    /// Hit times within the bar, beats.
    pub fn onsets(self) -> Vec<f64> {
        self.hits().iter().map(|h| h.0).collect()
    }

    /// `(at, length, note, velocity)` over one bar.
    fn hits(self) -> Vec<(f64, f64, BassNote, u8)> {
        match self {
            BassPattern::Root => vec![(0.0, PATTERN_BEATS, BassNote::Root, 96)],
            BassPattern::RootPulse => (0..4)
                .map(|b| (b as f64, 0.9, BassNote::Root, if b == 0 { 100 } else { 88 }))
                .collect(),
            BassPattern::RootFifth => vec![
                (0.0, 2.0, BassNote::Root, 98),
                (2.0, 2.0, BassNote::Fifth, 88),
            ],
            BassPattern::Octaves => (0..8)
                .map(|i| {
                    let note = if i % 2 == 0 {
                        BassNote::Root
                    } else {
                        BassNote::Octave
                    };
                    (i as f64 * 0.5, 0.45, note, if i % 2 == 0 { 96 } else { 80 })
                })
                .collect(),
            BassPattern::Walking => vec![
                (0.0, 1.0, BassNote::Root, 98),
                (1.0, 1.0, BassNote::Third, 86),
                (2.0, 1.0, BassNote::Fifth, 88),
                (3.0, 1.0, BassNote::Approach, 84),
            ],
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Hit {
    at: f64,
    length: f64,
    voices: Voices,
    velocity: u8,
}

#[derive(Debug, Clone, Copy)]
enum Voices {
    All,
    /// One voice of an arpeggio: step `step` of the pattern, low to high,
    /// turning back at the top when `bounce`.
    Step {
        step: usize,
        bounce: bool,
    },
    /// All voices, spread low-to-high (down strum) or high-to-low (up).
    Strum {
        down: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BassNote {
    Root,
    Third,
    Fifth,
    Octave,
    /// A half step below or above the next chord's bass note, whichever is
    /// nearer the current one.
    Approach,
}

/// A chord and where it sounds, beats.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChordSpan {
    pub chord: Chord,
    pub start: f64,
    pub length: f64,
}

/// A pattern and the stretch it covers, beats.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PatternSpan<P> {
    pub pattern: P,
    pub start: f64,
    pub length: f64,
}

/// One generated MIDI note, beats from the performance's origin.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GeneratedNote {
    pub pitch: u8,
    pub start: f64,
    pub length: f64,
    pub velocity: u8,
}

/// Notes for `chords` played by the comp and bass patterns, sorted by start.
/// The upper voices are voice-led across the whole progression
/// (`voicing.center` / `voicing.open`); `voicing.bass` is ignored — the bass
/// comes from the bass lane.
pub fn render_performance(
    chords: &[ChordSpan],
    comp: &[PatternSpan<CompPattern>],
    bass: &[PatternSpan<BassPattern>],
    voicing: VoicingOptions,
) -> Vec<GeneratedNote> {
    let mut sorted: Vec<ChordSpan> = chords.iter().copied().filter(|c| c.length > 0.0).collect();
    sorted.sort_by(|a, b| a.start.total_cmp(&b.start));
    let upper = voice_progression(
        &sorted.iter().map(|c| c.chord).collect::<Vec<_>>(),
        VoicingOptions {
            bass: false,
            ..voicing
        },
    );
    let chord_at = |t: f64| {
        sorted
            .iter()
            .position(|c| c.start <= t + 1e-9 && t < c.start + c.length - 1e-9)
    };
    let mut notes = Vec::new();

    for span in comp {
        for (at, hit) in bar_hits(span, span.pattern.hits(), |h| h.at) {
            let Some(index) = chord_at(at) else {
                continue;
            };
            let chord = &sorted[index];
            let end = (at + hit.length)
                .min(span.start + span.length)
                .min(chord.start + chord.length);
            let voices = &upper[index];
            if voices.is_empty() || end - at <= GAP {
                continue;
            }
            let length = end - at - GAP;
            match hit.voices {
                Voices::All => {
                    for &pitch in voices {
                        notes.push(GeneratedNote {
                            pitch,
                            start: at,
                            length,
                            velocity: hit.velocity,
                        });
                    }
                }
                Voices::Step { step, bounce } => {
                    let n = voices.len();
                    let index = if bounce && n > 1 {
                        let period = 2 * (n - 1);
                        let k = step % period;
                        if k < n { k } else { period - k }
                    } else {
                        step % n
                    };
                    notes.push(GeneratedNote {
                        pitch: voices[index],
                        start: at,
                        length,
                        velocity: hit.velocity,
                    });
                }
                Voices::Strum { down } => {
                    let n = voices.len();
                    for (k, &pitch) in voices.iter().enumerate() {
                        let order = if down { k } else { n - 1 - k };
                        let start = at + order as f64 * STRUM_STEP;
                        if end - start <= GAP {
                            continue;
                        }
                        notes.push(GeneratedNote {
                            pitch,
                            start,
                            length: end - start - GAP,
                            velocity: hit.velocity,
                        });
                    }
                }
            }
        }
    }

    for span in bass {
        for (at, (_, length, note, velocity)) in bar_hits(span, span.pattern.hits(), |h| h.0) {
            let Some(index) = chord_at(at) else {
                continue;
            };
            let chord = &sorted[index];
            let end = (at + length)
                .min(span.start + span.length)
                .min(chord.start + chord.length);
            if end - at <= GAP {
                continue;
            }
            let root = BASS_LOW + chord.chord.bass_pc() as i32;
            let pitch = match note {
                BassNote::Root => root,
                BassNote::Octave => root + 12,
                BassNote::Fifth => root + 7,
                BassNote::Third => root + chord_third(&chord.chord),
                BassNote::Approach => match sorted.get(index + 1) {
                    Some(next) => {
                        let target = BASS_LOW + next.chord.bass_pc() as i32;
                        // The next bass note from either side; the side
                        // nearer the current root.
                        let below = target - 1;
                        let above = target + 1;
                        if (below - root).abs() <= (above - root).abs() {
                            below
                        } else {
                            above
                        }
                    }
                    None => root + 7,
                },
            };
            notes.push(GeneratedNote {
                pitch: pitch.clamp(0, 127) as u8,
                start: at,
                length: end - at - GAP,
                velocity,
            });
        }
    }

    notes.sort_by(|a, b| a.start.total_cmp(&b.start).then(a.pitch.cmp(&b.pitch)));
    notes
}

/// The pattern's hits laid over `span`, bar by bar from the span's start,
/// stopping at its end: `(absolute beat, hit)`.
fn bar_hits<P, H: Copy>(
    span: &PatternSpan<P>,
    hits: Vec<H>,
    at: impl Fn(&H) -> f64,
) -> Vec<(f64, H)> {
    let mut out = Vec::new();
    let end = span.start + span.length;
    let mut bar = span.start;
    while bar < end - 1e-9 {
        for hit in &hits {
            let t = bar + at(hit);
            if t < end - 1e-9 {
                out.push((t, *hit));
            }
        }
        bar += PATTERN_BEATS;
    }
    out
}

/// Semitones from the bass note to the chord's third (a sus chord's 2nd or
/// 4th, a power chord's fifth).
fn chord_third(chord: &Chord) -> i32 {
    let intervals = chord.quality.intervals();
    let third = if intervals.contains(&3) {
        3
    } else if intervals.contains(&4) {
        4
    } else {
        intervals.iter().copied().find(|&i| i > 0).unwrap_or(7) as i32
    };
    // Measured from the root; a slash bass moves the line, not the chord.
    let from_bass = (chord.root as i32 + third - chord.bass_pc() as i32).rem_euclid(12);
    if from_bass == 0 { 12 } else { from_bass }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chords::ChordQuality;

    fn span(root: u8, quality: ChordQuality, start: f64, length: f64) -> ChordSpan {
        ChordSpan {
            chord: Chord::new(root, quality),
            start,
            length,
        }
    }

    fn comp(pattern: CompPattern, start: f64, length: f64) -> PatternSpan<CompPattern> {
        PatternSpan {
            pattern,
            start,
            length,
        }
    }

    fn bass(pattern: BassPattern, start: f64, length: f64) -> PatternSpan<BassPattern> {
        PatternSpan {
            pattern,
            start,
            length,
        }
    }

    fn c_then_g() -> Vec<ChordSpan> {
        vec![
            span(0, ChordQuality::Major, 0.0, 4.0),
            span(7, ChordQuality::Major, 4.0, 4.0),
        ]
    }

    #[test]
    fn sustain_holds_each_chord_until_the_next() {
        let notes = render_performance(
            &c_then_g(),
            &[comp(CompPattern::Sustain, 0.0, 8.0)],
            &[],
            VoicingOptions::default(),
        );
        let at_zero: Vec<u8> = notes
            .iter()
            .filter(|n| n.start == 0.0)
            .map(|n| n.pitch % 12)
            .collect();
        assert_eq!(at_zero.len(), 3);
        assert!(at_zero.contains(&0) && at_zero.contains(&4) && at_zero.contains(&7));
        for note in &notes {
            let chord_end = if note.start < 4.0 { 4.0 } else { 8.0 };
            assert!(
                note.start + note.length <= chord_end,
                "{note:?} crosses a chord change"
            );
        }
    }

    #[test]
    fn eighths_play_eight_hits_a_bar() {
        let notes = render_performance(
            &c_then_g()[..1],
            &[comp(CompPattern::Eighths, 0.0, 4.0)],
            &[],
            VoicingOptions::default(),
        );
        let mut starts: Vec<f64> = notes.iter().map(|n| n.start).collect();
        starts.dedup();
        assert_eq!(starts.len(), 8);
    }

    #[test]
    fn arpeggios_climb_and_turn() {
        let render = |pattern| {
            render_performance(
                &c_then_g()[..1],
                &[comp(pattern, 0.0, 4.0)],
                &[],
                VoicingOptions::default(),
            )
            .iter()
            .map(|n| n.pitch)
            .collect::<Vec<u8>>()
        };
        let up = render(CompPattern::ArpeggioUp);
        assert!(up[0] < up[1] && up[1] < up[2] && up[3] == up[0], "{up:?}");
        let bounce = render(CompPattern::ArpeggioUpDown);
        assert!(
            bounce[0] < bounce[1] && bounce[1] < bounce[2] && bounce[3] == bounce[1],
            "{bounce:?}"
        );
    }

    #[test]
    fn a_strum_spreads_its_voices() {
        let notes = render_performance(
            &c_then_g()[..1],
            &[comp(CompPattern::Strum, 0.0, 4.0)],
            &[],
            VoicingOptions::default(),
        );
        let first: Vec<&GeneratedNote> = notes.iter().filter(|n| n.start < 0.2).collect();
        assert_eq!(first.len(), 3);
        assert!(first[0].start < first[2].start);
    }

    #[test]
    fn a_pattern_repeats_from_where_it_was_placed() {
        // Quarters placed at beat 2: hits on 2, 3, 4, 5, …
        let notes = render_performance(
            &c_then_g(),
            &[comp(CompPattern::Quarters, 2.0, 4.0)],
            &[],
            VoicingOptions::default(),
        );
        let mut starts: Vec<f64> = notes.iter().map(|n| n.start).collect();
        starts.dedup();
        assert_eq!(starts, vec![2.0, 3.0, 4.0, 5.0]);
    }

    #[test]
    fn uncovered_chords_are_silent() {
        let notes = render_performance(
            &c_then_g(),
            &[comp(CompPattern::Sustain, 0.0, 4.0)],
            &[],
            VoicingOptions::default(),
        );
        assert!(notes.iter().all(|n| n.start < 4.0));
    }

    #[test]
    fn a_walking_bass_leads_into_the_next_chord() {
        let notes = render_performance(
            &c_then_g(),
            &[],
            &[bass(BassPattern::Walking, 0.0, 8.0)],
            VoicingOptions::default(),
        );
        let first_bar: Vec<i32> = notes
            .iter()
            .filter(|n| n.start < 4.0)
            .map(|n| n.pitch as i32)
            .collect();
        // C E G then a half step from G (43): F# (42) or G# (44).
        assert_eq!(&first_bar[..3], &[36, 40, 43]);
        assert!((first_bar[3] - 43).abs() == 1, "{first_bar:?}");
        assert!(notes.iter().all(|n| n.pitch < 60));
    }

    #[test]
    fn the_bass_follows_a_slash_chord() {
        let mut c_over_e = Chord::new(0, ChordQuality::Major);
        c_over_e.bass = Some(4);
        let chords = [ChordSpan {
            chord: c_over_e,
            start: 0.0,
            length: 4.0,
        }];
        let notes = render_performance(
            &chords,
            &[],
            &[bass(BassPattern::Root, 0.0, 4.0)],
            VoicingOptions::default(),
        );
        assert_eq!(notes[0].pitch % 12, 4);
    }
}

//! Applying a generated performance to a MIDI clip.
//!
//! The FBMX Performer predicts *how a score is played* — where each note lands,
//! how long it is held, how loud it is, and how its pitch moves inside it. What
//! arrives here is that prediction as a performance document, and what leaves is
//! ordinary clip data: notes with the start, duration, and velocity they were
//! given, and a [`PitchCurve`] on each one.
//!
//! That is the whole point. There is no separate AI-owned performance state and
//! nothing about a generated curve marks it as generated: the Pitch editor draws
//! it because it draws every curve, a drag on one of its points edits it the way
//! it edits a hand-drawn one, and deleting the curve returns the note to its
//! notated pitch. A user who never runs the Performer has a project that behaves
//! exactly as it did before.
//!
//! The document is the same JSON the offline renderer already reads, so a
//! performance can be rendered to a file and applied to a clip without two
//! descriptions of the same thing drifting apart.

use serde::Deserialize;

use crate::components::timeline::timeline_state::{MidiNoteState, PitchCurve, PitchSegmentShape};

/// Format tag written by the generator. Checked rather than assumed so a file
/// from a later, incompatible version is refused instead of half-applied.
pub const PERFORMANCE_FORMAT: &str = "solfage-performance-1";

/// Points closer together than this in beats collapse into one. The generator
/// emits at 100 Hz, which at 120 bpm is a point every 0.005 beats — far more
/// than the curve needs to reproduce the shape, and more than a person wants to
/// drag around.
const MERGE_BEATS: f32 = 0.004;

/// Largest deviation, in cents, the decimator may introduce. Matches the
/// tolerance the engine snapshot builder uses when it decimates the other way,
/// so a curve survives the round trip through the engine unchanged.
const DECIMATE_CENTS: f32 = 1.0;

#[derive(Debug, Clone, Deserialize)]
pub struct PerformanceDocument {
    #[serde(default)]
    pub format: String,
    #[serde(default)]
    pub generated_by: String,
    #[serde(default)]
    pub tempo_bpm: Option<f32>,
    #[serde(default)]
    pub notes: Vec<PerformanceNote>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PerformanceNote {
    pub start: f32,
    pub duration: f32,
    pub note: i32,
    #[serde(default)]
    pub velocity: Option<f32>,
    #[serde(default)]
    pub articulation: Option<String>,
    #[serde(default)]
    pub pitch: Vec<PerformancePitchPoint>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct PerformancePitchPoint {
    pub t: f32,
    pub cents: f32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PerformanceError {
    Parse(String),
    UnknownFormat(String),
    NoteCountMismatch { document: usize, clip: usize },
}

impl std::fmt::Display for PerformanceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PerformanceError::Parse(why) => write!(f, "not a performance document: {why}"),
            PerformanceError::UnknownFormat(found) => write!(
                f,
                "performance format {found:?} is not {PERFORMANCE_FORMAT}"
            ),
            PerformanceError::NoteCountMismatch { document, clip } => write!(
                f,
                "performance has {document} notes but the clip has {clip}; \
                 the performance was generated from a different score"
            ),
        }
    }
}

/// What one application changed, for the undo entry and the status line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PerformanceApplied {
    pub notes: usize,
    pub curves: usize,
    pub points: usize,
}

pub fn parse(text: &str) -> Result<PerformanceDocument, PerformanceError> {
    let document: PerformanceDocument =
        serde_json::from_str(text).map_err(|error| PerformanceError::Parse(error.to_string()))?;
    if document.format != PERFORMANCE_FORMAT {
        return Err(PerformanceError::UnknownFormat(document.format));
    }
    Ok(document)
}

/// Write a performance onto `notes`, in place.
///
/// The document and the clip are matched by position: the generator was handed
/// this clip's notes in score order and returned one entry per note in that same
/// order. A count mismatch therefore means the two are not describing the same
/// music, and applying it anyway would silently put one note's expression on
/// another — so it is refused rather than best-guessed.
///
/// `seconds_per_beat` converts the document's seconds into the clip's beats.
/// The document carries the tempo it was generated at; passing the *project's*
/// tempo is what makes a performance generated at 90 bpm play as written when
/// dropped into a 120 bpm project, rather than arriving stretched.
pub fn apply(
    document: &PerformanceDocument,
    notes: &mut [MidiNoteState],
    seconds_per_beat: f32,
    next_point_id: &mut u64,
) -> Result<PerformanceApplied, PerformanceError> {
    if document.notes.len() != notes.len() {
        return Err(PerformanceError::NoteCountMismatch {
            document: document.notes.len(),
            clip: notes.len(),
        });
    }
    let seconds_per_beat = if seconds_per_beat.is_finite() && seconds_per_beat > 1e-6 {
        seconds_per_beat
    } else {
        0.5
    };

    let mut applied = PerformanceApplied::default();
    for (note, performed) in notes.iter_mut().zip(&document.notes) {
        note.start = performed.start / seconds_per_beat;
        note.duration = (performed.duration / seconds_per_beat).max(1e-4);
        if let Some(velocity) = performed.velocity {
            // The document works in 0..1; the note stores MIDI 1..=127.
            note.velocity = (velocity.clamp(0.0, 1.0) * 127.0).round().clamp(1.0, 127.0) as u8;
        }
        applied.notes += 1;

        if performed.pitch.is_empty() {
            // No curve means "sounds as written", and that is expressed by
            // having no curve rather than by a flat one — otherwise every
            // generated note would carry points a user has to delete to get
            // back to the notated pitch.
            note.pitch_curve = None;
            continue;
        }

        let mut curve = PitchCurve::default();
        for point in decimate(&performed.pitch) {
            curve.set_point(
                point.t / seconds_per_beat,
                point.cents,
                PitchSegmentShape::Smooth,
                MERGE_BEATS,
            );
        }
        if curve.is_empty() {
            note.pitch_curve = None;
            continue;
        }
        // `set_point` mints ids of its own; re-stamping them from the caller's
        // counter keeps every point id unique across the clip, which selection
        // and undo both rely on.
        for point in &mut curve.points {
            point.id = *next_point_id;
            *next_point_id += 1;
        }
        applied.points += curve.points.len();
        applied.curves += 1;
        note.pitch_curve = Some(curve);
    }
    Ok(applied)
}

/// Drop points a straight line between their neighbours already reproduces.
///
/// A 100 Hz curve on a two-second note is two hundred points describing a shape
/// a dozen would carry. Keeping all of them costs nothing to play but makes the
/// curve unusable by hand: the editor draws a solid bar of control points and
/// dragging one moves the line imperceptibly.
fn decimate(points: &[PerformancePitchPoint]) -> Vec<PerformancePitchPoint> {
    if points.len() <= 2 {
        return points.to_vec();
    }
    let mut kept = vec![points[0]];
    let mut anchor = 0usize;
    for index in 1..points.len() - 1 {
        let next = index + 1;
        let (t0, c0) = (points[anchor].t, points[anchor].cents);
        let (t1, c1) = (points[next].t, points[next].cents);
        let span = t1 - t0;
        // Would every point between `anchor` and `next` be reproduced by the
        // straight line joining them? Checking only the immediate neighbour is
        // the tempting shortcut and it flattens a peak that sits in the middle
        // of a long run.
        let reproducible = span.abs() > f32::EPSILON
            && (anchor + 1..=index).all(|probe| {
                let t = (points[probe].t - t0) / span;
                let line = c0 + (c1 - c0) * t;
                (points[probe].cents - line).abs() <= DECIMATE_CENTS
            });
        if !reproducible {
            kept.push(points[index]);
            anchor = index;
        }
    }
    kept.push(points[points.len() - 1]);
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(json: &str) -> PerformanceDocument {
        parse(json).expect("valid document")
    }

    #[test]
    fn a_document_from_another_format_is_refused() {
        let error = parse(r#"{"format":"something-else","notes":[]}"#).unwrap_err();
        assert!(matches!(error, PerformanceError::UnknownFormat(_)));
    }

    #[test]
    fn a_performance_for_a_different_score_is_refused_rather_than_misapplied() {
        let doc = document(
            r#"{"format":"solfage-performance-1","notes":[
                {"start":0.0,"duration":1.0,"note":60}
            ]}"#,
        );
        let mut notes = vec![
            MidiNoteState::new(60, 0.0, 1.0, 96),
            MidiNoteState::new(62, 1.0, 1.0, 96),
        ];
        let mut ids = 1;
        let error = apply(&doc, &mut notes, 0.5, &mut ids).unwrap_err();
        assert_eq!(
            error,
            PerformanceError::NoteCountMismatch {
                document: 1,
                clip: 2
            }
        );
        // And nothing was written.
        assert_eq!(notes[0].start, 0.0);
        assert!(notes[0].pitch_curve.is_none());
    }

    #[test]
    fn a_generated_curve_lands_as_ordinary_editable_pitch_data() {
        let doc = document(
            r#"{"format":"solfage-performance-1","notes":[
                {"start":1.0,"duration":2.0,"note":69,"velocity":0.8,
                 "pitch":[{"t":0.0,"cents":-30},{"t":0.5,"cents":0},
                          {"t":1.0,"cents":20},{"t":2.0,"cents":0}]}
            ]}"#,
        );
        let mut notes = vec![MidiNoteState::new(69, 0.0, 1.0, 64)];
        let mut ids = 100;
        let applied = apply(&doc, &mut notes, 0.5, &mut ids).expect("applies");

        assert_eq!(applied.notes, 1);
        assert_eq!(applied.curves, 1);
        // Seconds became beats at 0.5 s per beat.
        assert!((notes[0].start - 2.0).abs() < 1e-5);
        assert!((notes[0].duration - 4.0).abs() < 1e-5);
        assert_eq!(notes[0].velocity, 102);

        let curve = notes[0].pitch_curve.as_ref().expect("a curve");
        assert!(curve.points.len() >= 3, "the shape survived decimation");
        // Ids are unique and came from the caller's counter.
        let mut seen = std::collections::HashSet::new();
        for point in &curve.points {
            assert!(point.id >= 100, "point ids come from the shared counter");
            assert!(seen.insert(point.id), "point ids are unique");
        }
        // The drawn shape is preserved: the curve still reads -30 cents at the
        // note start and reaches its peak near the middle.
        assert!((curve.cents_at(0.0) + 30.0).abs() < 2.0);
        assert!(curve.cents_at(2.0) > 10.0);
    }

    #[test]
    fn a_note_with_no_expression_is_left_without_a_curve() {
        let doc = document(
            r#"{"format":"solfage-performance-1","notes":[
                {"start":0.0,"duration":1.0,"note":60,"velocity":0.5}
            ]}"#,
        );
        let mut notes = vec![MidiNoteState::new(60, 0.0, 1.0, 64)];
        notes[0].pitch_curve = Some(PitchCurve {
            points: vec![
                crate::components::timeline::timeline_state::PitchPoint::new(
                    0.0,
                    50.0,
                    PitchSegmentShape::Smooth,
                ),
            ],
        });
        let mut ids = 1;
        let applied = apply(&doc, &mut notes, 0.5, &mut ids).expect("applies");
        assert_eq!(applied.curves, 0);
        assert!(
            notes[0].pitch_curve.is_none(),
            "a note that sounds as written carries no curve, so Reset means \
             something and the editor is not littered with flat lines"
        );
    }

    /// The real generator's output, not a hand-written fixture.
    ///
    /// A format both sides *say* they agree on is worth checking against a file
    /// one of them actually produced: this is a phrase from
    /// `13_Hark_vn_vn_va`, generated by the URMP-trained Performer and rendered
    /// by the offline engine, trimmed to its first notes.
    #[test]
    fn a_file_from_the_generator_applies() {
        let doc = document(include_str!("performance_fixture.json"));
        assert_eq!(doc.generated_by, "fbmx-performer");
        let mut notes: Vec<MidiNoteState> = doc
            .notes
            .iter()
            .map(|n| MidiNoteState::new(n.note as u8, 0.0, 1.0, 64))
            .collect();
        let mut ids = 1;
        let applied = apply(&doc, &mut notes, 0.5, &mut ids).expect("applies");

        assert_eq!(applied.notes, doc.notes.len());
        assert!(applied.curves > 0, "the generator emitted pitch expression");
        // Decimation must have done real work: the generator writes at 100 Hz.
        let raw: usize = doc.notes.iter().map(|n| n.pitch.len()).sum();
        assert!(
            applied.points < raw / 2,
            "kept {} of {raw} points; a curve this dense is not editable",
            applied.points
        );
        for note in &notes {
            assert!(note.duration > 0.0);
            if let Some(curve) = note.pitch_curve.as_ref() {
                assert!(curve.points.iter().all(|p| p.beat >= 0.0));
            }
        }
    }

    /// Decimation has to preserve the shape, not merely shorten the list.
    #[test]
    fn decimation_keeps_the_shape_within_tolerance() {
        // A full vibrato cycle at 100 Hz over one second.
        let dense: Vec<PerformancePitchPoint> = (0..=100)
            .map(|i| {
                let t = i as f32 / 100.0;
                PerformancePitchPoint {
                    t,
                    cents: 25.0 * (t * std::f32::consts::TAU * 5.0).sin(),
                }
            })
            .collect();
        let thinned = decimate(&dense);
        assert!(thinned.len() < dense.len(), "something was dropped");

        // Every original point is reproduced by the thinned polyline.
        for point in &dense {
            let after = thinned
                .iter()
                .position(|k| k.t >= point.t)
                .unwrap_or(thinned.len() - 1);
            let before = after.saturating_sub(1);
            let (a, b) = (thinned[before], thinned[after]);
            let value = if (b.t - a.t).abs() < f32::EPSILON {
                b.cents
            } else {
                a.cents + (b.cents - a.cents) * ((point.t - a.t) / (b.t - a.t))
            };
            assert!(
                (value - point.cents).abs() <= DECIMATE_CENTS * 2.0,
                "at t={} the thinned curve reads {value} but the original was {}",
                point.t,
                point.cents
            );
        }
    }
}

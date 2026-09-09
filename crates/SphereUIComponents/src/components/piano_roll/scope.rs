//! What the MIDI editor is looking at, in project beats.
//!
//! The roll was built around one clip, and its whole coordinate system said so:
//! x was a beat measured from that clip's start, a note's `start` was the same
//! number the grid drew, and anything outside the clip did not exist. That is a
//! fine model for editing one part and a poor one for reading a song — the clip
//! you are editing has no visible relationship to the ones before and after it,
//! so a fill written to land on the downbeat of the next clip is written blind.
//!
//! This module is the piece that makes the editor project-wide: a list of the
//! clips on screen, each with its place on the timeline, and the arithmetic for
//! moving between the two frames of reference.
//!
//! Two frames, named consistently everywhere:
//!
//! * **project beat** — measured from the start of the song. What the ruler
//!   shows, what the transport reports, what `beat_to_x` now takes.
//! * **local beat** — measured from a clip's own start. What a
//!   [`MidiNoteState`] stores, and what the edit commands still speak, because
//!   a note belongs to a clip and moving the clip must move the note with it.
//!
//! Everything here is pure. That is deliberate: this is where a coordinate bug
//! would live, and pure functions are the part of a 6,000-line editor that can
//! actually be tested.

use crate::components::timeline::timeline_state::{ClipType, TimelineState};

/// One clip, placed on the project timeline.
#[derive(Debug, Clone, PartialEq)]
pub struct ClipSpan {
    pub clip_id: String,
    pub track_id: String,
    /// What the arrangement calls this clip, for labelling a neighbour.
    pub name: String,
    /// Where the clip starts in project beats.
    pub start_beat: f32,
    /// How long it runs. Never negative — a clip with a nonsense duration is
    /// clamped rather than allowed to invert its own bounds, because a
    /// backwards span makes `contains` answer nonsense for every beat.
    pub duration_beats: f32,
    /// The track's colour, so a clip can be told apart from its neighbours
    /// without reading the name.
    pub color: gpui::Rgba,
    /// The clip the user is editing. The others are drawn, and are context.
    pub editable: bool,
}

impl ClipSpan {
    pub fn end_beat(&self) -> f32 {
        self.start_beat + self.duration_beats
    }

    /// Whether `project_beat` falls inside this clip.
    ///
    /// Half-open: the start belongs to this clip and the end belongs to the
    /// next. Two clips laid end to end share a boundary beat, and a closed
    /// interval would give it to both — which is how a note drawn exactly on a
    /// downbeat ends up in the clip that just finished.
    pub fn contains(&self, project_beat: f32) -> bool {
        project_beat >= self.start_beat && project_beat < self.end_beat()
    }

    /// Project beat → this clip's own frame.
    pub fn to_local(&self, project_beat: f32) -> f32 {
        project_beat - self.start_beat
    }

    /// This clip's frame → project beat.
    pub fn to_project(&self, local_beat: f32) -> f32 {
        self.start_beat + local_beat
    }
}

/// The clips the editor is showing, in timeline order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EditorScope {
    spans: Vec<ClipSpan>,
}

impl EditorScope {
    /// The MIDI clips on the track that owns `editing_clip_id`.
    ///
    /// Scoped to one track rather than the whole project on purpose: a piano
    /// roll has one pitch axis, and stacking two instruments' notes on it makes
    /// a chord out of two parts that are not one. The song-wide view a user
    /// wants from "see everything" is the arrangement; what is wanted *here* is
    /// this instrument, over the whole song.
    ///
    /// Returns an empty scope when the clip cannot be found, which is the
    /// honest answer — the caller then draws nothing rather than drawing a
    /// four-beat clip at bar one that does not exist.
    pub fn for_editing_clip(state: &TimelineState, editing_clip_id: &str) -> Self {
        let Some(track) = state
            .tracks
            .iter()
            .find(|track| track.clips.iter().any(|clip| clip.id == editing_clip_id))
        else {
            return Self::default();
        };

        let mut spans: Vec<ClipSpan> = track
            .clips
            .iter()
            .filter(|clip| matches!(clip.clip_type, ClipType::Midi { .. }))
            .map(|clip| ClipSpan {
                clip_id: clip.id.clone(),
                track_id: track.id.clone(),
                name: clip.name.clone(),
                start_beat: clip.start_beat,
                duration_beats: clip.duration_beats.max(0.0),
                color: track.color,
                editable: clip.id == editing_clip_id,
            })
            .collect();

        // Timeline order, so drawing and hit-testing walk the song left to
        // right and `owner_at` can stop at the first match.
        spans.sort_by(|a, b| {
            a.start_beat
                .partial_cmp(&b.start_beat)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Self { spans }
    }

    pub fn spans(&self) -> &[ClipSpan] {
        &self.spans
    }

    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    /// The clip being edited.
    pub fn editing(&self) -> Option<&ClipSpan> {
        self.spans.iter().find(|span| span.editable)
    }

    pub fn span(&self, clip_id: &str) -> Option<&ClipSpan> {
        self.spans.iter().find(|span| span.clip_id == clip_id)
    }

    /// Which clip owns `project_beat`.
    ///
    /// The clip being edited wins where clips overlap. Overlap is legal on a
    /// timeline and the alternative — first or last in order — would silently
    /// move the user's edits into a neighbour the moment two clips touched.
    pub fn owner_at(&self, project_beat: f32) -> Option<&ClipSpan> {
        self.spans
            .iter()
            .find(|span| span.editable && span.contains(project_beat))
            .or_else(|| self.spans.iter().find(|span| span.contains(project_beat)))
    }

    /// First and last beat covered by any clip, for scroll bounds.
    ///
    /// `None` for an empty scope: there is no extent to scroll over, and
    /// answering `(0, 0)` would be a claim about a song that has no clips.
    pub fn extent(&self) -> Option<(f32, f32)> {
        let first = self.spans.first()?;
        let start = first.start_beat;
        let end = self
            .spans
            .iter()
            .map(ClipSpan::end_beat)
            .fold(f32::NEG_INFINITY, f32::max);
        Some((start, end))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(id: &str, start: f32, len: f32, editable: bool) -> ClipSpan {
        ClipSpan {
            clip_id: id.to_string(),
            track_id: "t1".to_string(),
            name: id.to_string(),
            start_beat: start,
            duration_beats: len,
            color: gpui::Rgba {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 1.0,
            },
            editable,
        }
    }

    fn scope(spans: Vec<ClipSpan>) -> EditorScope {
        EditorScope { spans }
    }

    #[test]
    fn local_and_project_beats_round_trip() {
        let clip = span("a", 16.0, 8.0, true);
        assert_eq!(clip.to_project(0.0), 16.0);
        assert_eq!(clip.to_local(16.0), 0.0);
        assert_eq!(clip.to_local(clip.to_project(3.5)), 3.5);
        assert_eq!(clip.end_beat(), 24.0);
    }

    /// The boundary belongs to the clip that is starting, not the one that just
    /// ended. Adjacent clips share the beat, and giving it to both is how a
    /// note drawn on a downbeat lands in the previous bar's clip.
    #[test]
    fn a_shared_boundary_belongs_to_the_later_clip() {
        let first = span("a", 0.0, 16.0, false);
        let second = span("b", 16.0, 16.0, false);
        assert!(first.contains(15.999));
        assert!(!first.contains(16.0));
        assert!(second.contains(16.0));

        let scope = scope(vec![first, second]);
        assert_eq!(scope.owner_at(16.0).map(|s| s.clip_id.as_str()), Some("b"));
    }

    #[test]
    fn a_beat_in_no_clip_has_no_owner() {
        let scope = scope(vec![span("a", 0.0, 4.0, true), span("b", 16.0, 4.0, false)]);
        assert!(scope.owner_at(8.0).is_none());
        assert!(scope.owner_at(-1.0).is_none());
        assert!(scope.owner_at(64.0).is_none());
    }

    /// Overlap is legal on a timeline. Where it happens the edited clip keeps
    /// the beat, so a user's notes never quietly move into a neighbour.
    #[test]
    fn the_edited_clip_wins_an_overlap() {
        let scope = scope(vec![
            span("under", 0.0, 32.0, false),
            span("editing", 8.0, 8.0, true),
        ]);
        assert_eq!(
            scope.owner_at(10.0).map(|s| s.clip_id.as_str()),
            Some("editing")
        );
        // Outside the edited clip the other one still answers.
        assert_eq!(
            scope.owner_at(20.0).map(|s| s.clip_id.as_str()),
            Some("under")
        );
    }

    #[test]
    fn extent_spans_every_clip() {
        let scope = scope(vec![
            span("a", 8.0, 4.0, true),
            span("b", 32.0, 16.0, false),
        ]);
        assert_eq!(scope.extent(), Some((8.0, 48.0)));
    }

    /// A clip that ends after a later-starting one still sets the extent — the
    /// end is a max, not the last clip's end.
    #[test]
    fn extent_uses_the_furthest_end_not_the_last_start() {
        let scope = scope(vec![
            span("long", 0.0, 64.0, true),
            span("short", 8.0, 4.0, false),
        ]);
        assert_eq!(scope.extent(), Some((0.0, 64.0)));
    }

    #[test]
    fn an_empty_scope_claims_nothing() {
        let scope = EditorScope::default();
        assert!(scope.is_empty());
        assert!(scope.extent().is_none());
        assert!(scope.owner_at(0.0).is_none());
        assert!(scope.editing().is_none());
    }

    /// Rebasing a note from one clip to another must keep it where the user
    /// dropped it on the timeline. This is the arithmetic the cross-clip drag
    /// performs, stated on its own so a sign error cannot hide inside a drag.
    #[test]
    fn a_note_rebased_between_clips_keeps_its_place_in_the_song() {
        let from = span("a", 16.0, 16.0, true);
        let to = span("b", 32.0, 16.0, false);

        // A note 14 beats into the first clip is at project beat 30.
        let local_in_from = 14.0_f32;
        let on_timeline = from.to_project(local_in_from);
        assert_eq!(on_timeline, 30.0);

        // Dragged four beats right it lands at 34, inside the second clip.
        let dropped_at = on_timeline + 4.0;
        assert!(to.contains(dropped_at));
        let local_in_to = to.to_local(dropped_at);
        assert_eq!(local_in_to, 2.0);
        // And converting back gives the same place in the song.
        assert_eq!(to.to_project(local_in_to), dropped_at);
    }

    /// The gap between two clips owns nothing, so a note dropped there has no
    /// destination — and the editor leaves it alone rather than deleting it.
    #[test]
    fn the_gap_between_clips_owns_no_note() {
        let scope = scope(vec![span("a", 0.0, 8.0, true), span("b", 16.0, 8.0, false)]);
        assert!(scope.owner_at(12.0).is_none());
    }

    /// A note dragged left, out of the front of its clip, migrates the same way
    /// as one dragged right. Worth its own case: the subtraction changes sign.
    #[test]
    fn migration_works_in_both_directions() {
        let earlier = span("a", 0.0, 16.0, false);
        let editing = span("b", 16.0, 16.0, true);
        let scope = scope(vec![earlier.clone(), editing.clone()]);

        // Two beats into the edited clip, dragged five beats left.
        let dropped_at = editing.to_project(2.0) - 5.0;
        assert_eq!(dropped_at, 13.0);
        let owner = scope.owner_at(dropped_at).expect("a clip owns beat 13");
        assert_eq!(owner.clip_id, "a");
        assert_eq!(owner.to_local(dropped_at), 13.0);
    }

    /// A zero-length clip owns no beat at all, rather than owning its start
    /// forever. `contains` is half-open, so start == end is empty by
    /// construction — this pins that, because the alternative is a clip that
    /// swallows every edit on its downbeat.
    #[test]
    fn a_zero_length_clip_owns_nothing() {
        let clip = span("empty", 4.0, 0.0, true);
        assert!(!clip.contains(4.0));
        assert!(!clip.contains(3.999));
        assert!(!clip.contains(4.001));
    }
}

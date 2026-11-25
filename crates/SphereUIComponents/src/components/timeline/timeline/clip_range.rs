//! Time ranges inside clips (`TimelineState::arrangement_range`).
//!
//! With the Pointer, a press in an audio or MIDI clip's body — below the top
//! band, which still moves the clip — and a drag selects a time range,
//! snapped to the grid, on the track pressed and every track the pointer
//! crosses. The clips it overlaps are selected with it. A press inside the
//! range's span on one of those clips and a drag moves what the range holds
//! instead: the clips are split at the range's edges and the pieces inside
//! go with the ordinary clip move — same snap, same track rules — and the
//! split and the move are one undo step.
//!
//! While a range is set, Delete, Cut and Copy act on it rather than on whole
//! clips: Delete removes what lies inside it and leaves a gap, Copy puts the
//! pieces inside it on the clip clipboard without changing the project, and
//! Cut is both, as one undo step.
//!
//! The drag follows the pointer through the root's mouse handlers and the
//! window-wide tracker the marquee uses, not a GPUI drag, so it does not
//! refresh the whole window on every move.

use super::*;
use crate::components::timeline::audio_clip::ClipBodyPress;
use crate::components::timeline::timeline_state::{
    next_clip_id_number, ClipSelectOp, PendingTrackEdit, TimelineRangeSelection, TrackEditScope,
    RANGE_EDGE_BEATS,
};

/// A Pointer press in a clip's body, from the press to the release.
#[derive(Clone, Debug)]
pub(super) struct ClipBodyDrag {
    clip_id: String,
    track_id: String,
    /// Window position of the press; the drag threshold is measured from it.
    press_window: (f32, f32),
    /// The beat under the press, unsnapped: the range's fixed edge.
    anchor_beat: f32,
    /// Past the drag threshold.
    dragging: bool,
    /// The press landed inside the range on a clip it holds: a drag moves the
    /// range's content. Taken from the press, because the click that a press
    /// without a drag becomes clears the range.
    move_range: Option<TimelineRangeSelection>,
    /// Once the range's clips are split: the piece under the press, which
    /// the clip move resolves the others from.
    move_anchor: Option<ClipDragItem>,
}

/// One clip a range edit split: where it was and the piece inside the range.
#[derive(Clone, Debug)]
pub(crate) struct RangeSplit {
    pub source_clip_id: String,
    pub track_id: String,
    pub inside: ClipState,
}

/// Replace every clip `range` covers with its pieces
/// ([`TimelineState::plan_clip_range_pieces`]), as the split command does —
/// a take's pieces stay takes. With `keep_inside` false the piece inside the
/// range is left out: a range delete. Returns each clip's inside piece, kept
/// or not. Records nothing; the caller wraps it in one track edit.
pub(crate) fn apply_range_split(
    state: &mut TimelineState,
    range: &TimelineRangeSelection,
    keep_inside: bool,
) -> Vec<RangeSplit> {
    let (lo, hi) = range.as_f32_range();
    let mut next_id = next_clip_id_number(&state.tracks);
    let mut splits = Vec::new();
    for target in state.range_clip_targets(range) {
        let Some(snapshot) = ClipSnapshot::capture(state, &target.clip_id) else {
            continue;
        };
        let Some(pieces) = state.plan_clip_range_pieces(&snapshot.clip, lo, hi, &mut next_id)
        else {
            continue;
        };
        let track_id = snapshot.track_id.clone();
        let mut clips = Vec::with_capacity(3);
        clips.extend(pieces.left.clone().map(|clip| (track_id.clone(), clip)));
        if keep_inside {
            clips.push((track_id.clone(), pieces.inside.clone()));
        }
        clips.extend(pieces.right.clone().map(|clip| (track_id.clone(), clip)));
        EditCommand::ReplaceClipWithClips { snapshot, clips }.execute(state);
        splits.push(RangeSplit {
            source_clip_id: target.clip_id,
            track_id,
            inside: pieces.inside,
        });
    }
    splits
}

/// The range set in the arrangement, when it spans any time.
fn active_range(state: &TimelineState) -> Option<TimelineRangeSelection> {
    state
        .arrangement_range
        .clone()
        .filter(|range| range.end_beat - range.start_beat > f64::from(RANGE_EDGE_BEATS))
}

impl Timeline {
    // ── Range edits ──────────────────────────────────────────────────────

    /// The clipboard a Copy of the range makes: each covered clip's piece
    /// inside it. `None` when no range is set; the project is not changed.
    pub fn copy_arrangement_range(&self) -> Option<Vec<ClipSnapshot>> {
        let range = active_range(&self.state)?;
        Some(
            self.state
                .plan_range_copy(&range)
                .into_iter()
                .map(|(track_id, index, clip)| ClipSnapshot {
                    track_id,
                    clip,
                    index,
                    take: None,
                })
                .collect(),
        )
    }

    /// Delete what the range holds from every clip it covers, leaving a gap:
    /// one undo step under `label`. `None` when no range is set, so the
    /// caller falls back to whole clips; `Some(false)` when the range covers
    /// nothing. The range stays set.
    pub fn delete_arrangement_range(
        &mut self,
        label: &'static str,
        cx: &mut gpui::Context<Self>,
    ) -> Option<bool> {
        let range = active_range(&self.state)?;
        let edit = self.begin_track_edit(TrackEditScope::tracks(
            self.state.range_scope_track_ids(&range),
        ));
        let removed = apply_range_split(&mut self.state, &range, false);
        if removed.is_empty() {
            return Some(false);
        }
        let audio = removed
            .iter()
            .any(|split| matches!(split.inside.clip_type, ClipType::Audio { .. }));
        self.state.selection.selected_clip_ids.clear();
        self.state.arrangement_range = Some(range);
        self.commit_track_edit(label, edit, false, cx);
        self.mark_project_changed(cx);
        if audio {
            self.mark_media_changed(cx);
        }
        cx.notify();
        Some(true)
    }

    // ── The clip body press ──────────────────────────────────────────────

    /// A Pointer press in a clip's body. Selects as a press on the clip
    /// does, unless it lands inside the range on a clip the range holds:
    /// then the range and its selection stay for a drag to move, and a click
    /// selects just this clip on the release.
    pub(super) fn begin_clip_body_press(
        &mut self,
        press: &ClipBodyPress,
        cx: &mut gpui::Context<Self>,
    ) {
        // A body drag whose release never reached the timeline ends here,
        // as a cancel would: a range move it split goes back whole.
        if self.clip_body_drag.take().is_some() {
            self.revert_clip_move();
            self.state.follow_playhead_suspended = false;
        }
        if self.state.active_tool != TimelineTool::Pointer {
            return;
        }
        let beat = self.state.beats_from_window_x(press.window_x);
        let move_range = active_range(&self.state).filter(|range| {
            let (lo, hi) = range.as_f32_range();
            beat >= lo
                && beat <= hi
                && self
                    .state
                    .range_clip_targets(range)
                    .iter()
                    .any(|target| target.clip_id == press.clip_id)
        });
        // The move measures from the press, as the clip move does.
        self.clip_drag_origin = Some(gpui::point(px(press.window_x), px(press.window_y)));
        self.clip_clone_drag_id = None;
        self.clip_clone_hint = None;
        if move_range.is_some() {
            self.pending_clip_click = Some(PendingClipClick {
                clip_id: press.clip_id.clone(),
                op: if press.additive {
                    ClipSelectOp::Toggle
                } else {
                    ClipSelectOp::Replace
                },
            });
        } else {
            self.press_clip_selection(&press.clip_id, press.additive);
        }
        self.clip_body_drag = Some(ClipBodyDrag {
            clip_id: press.clip_id.clone(),
            track_id: press.track_id.clone(),
            press_window: (press.window_x, press.window_y),
            anchor_beat: beat,
            dragging: false,
            move_range,
            move_anchor: None,
        });
        cx.notify();
    }

    /// The pointer moved with the button down after a clip body press: past
    /// the drag threshold it draws the range, or moves the range's content.
    pub(super) fn update_clip_body_drag(
        &mut self,
        window_pos: (f32, f32),
        bypass_snap: bool,
        window: &Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(drag) = self.clip_body_drag.as_mut() else {
            return;
        };
        if !drag.dragging {
            if !crate::components::edit::marquee_drag_started(drag.press_window, window_pos) {
                return;
            }
            drag.dragging = true;
            let moving = drag.move_range.is_some();
            // A drag, not a click: the press's selection change is off.
            self.pending_clip_click = None;
            self.state.follow_playhead_suspended = true;
            if moving && !self.start_range_move() {
                // Nothing to move after all: this drag draws a new range.
                if let Some(drag) = self.clip_body_drag.as_mut() {
                    drag.move_range = None;
                }
            }
        }
        let Some(anchor) = self
            .clip_body_drag
            .as_ref()
            .map(|drag| drag.move_anchor.clone())
        else {
            return;
        };
        match anchor {
            Some(anchor) => {
                self.move_dragged_clip_to_position_with_bypass(
                    &anchor,
                    gpui::point(px(window_pos.0), px(window_pos.1)),
                    window,
                    bypass_snap,
                );
                cx.notify();
            }
            None => self.update_clip_range(window_pos, bypass_snap, cx),
        }
    }

    /// The range a body drag draws: from the press to the pointer, both
    /// snapped (Shift bypasses), over the tracks from the pressed one to the
    /// one under the pointer. Selects the clips it overlaps; notifies only
    /// when the range changed.
    fn update_clip_range(
        &mut self,
        window_pos: (f32, f32),
        bypass_snap: bool,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(drag) = self.clip_body_drag.as_ref() else {
            return;
        };
        let start = self
            .state
            .snap_beats_with_bypass(drag.anchor_beat, bypass_snap)
            .max(0.0);
        let end = self
            .state
            .snap_beats_with_bypass(self.state.beats_from_window_x(window_pos.0), bypass_snap)
            .max(0.0);
        let track_id = drag.track_id.clone();
        let current_track = self
            .state
            .track_index_at_y(
                self.track_area_y_from_window(gpui::point(px(window_pos.0), px(window_pos.1))),
            )
            .and_then(|index| self.state.tracks.get(index))
            .map(|track| track.id.clone())
            .unwrap_or_else(|| track_id.clone());
        let tracks = self.state.track_ids_between(&track_id, &current_track);
        let range = ((end - start).abs() > RANGE_EDGE_BEATS)
            .then(|| TimelineRangeSelection::new(start.into(), end.into(), tracks.clone()));
        if range == self.state.arrangement_range {
            return;
        }
        let clip_ids = match range.as_ref() {
            Some(range) => self
                .state
                .range_clip_targets(range)
                .into_iter()
                .map(|target| target.clip_id)
                .collect(),
            None => vec![drag.clip_id.clone()],
        };
        self.state
            .apply_marquee_selection(&tracks, clip_ids, &track_id, false);
        self.state.arrangement_range = range;
        cx.notify();
    }

    /// The release of a clip body press. A click leaves it to the root's
    /// release, which applies the press's held-back selection; a drag keeps
    /// the range it drew, or commits the range move.
    pub(super) fn finish_clip_body_drag(
        &mut self,
        window_pos: Option<(f32, f32)>,
        bypass_snap: bool,
        window: &Window,
        cx: &mut gpui::Context<Self>,
    ) {
        if let Some(position) = window_pos {
            self.update_clip_body_drag(position, bypass_snap, window, cx);
        }
        let Some(drag) = self.clip_body_drag.take() else {
            return;
        };
        self.state.follow_playhead_suspended = false;
        if let (true, Some(anchor)) = (drag.dragging, drag.move_anchor) {
            self.commit_range_move(&anchor, drag.move_range, cx);
        }
        cx.notify();
    }

    // ── Moving a range ───────────────────────────────────────────────────

    /// Split the range's clips at its edges and hand the pieces inside to
    /// the clip move, anchored on the piece under the press. The split is
    /// not recorded yet: the release records it together with the move, and
    /// a cancel puts the clips back whole. Returns whether there is
    /// something to move.
    fn start_range_move(&mut self) -> bool {
        let Some((range, pressed)) = self
            .clip_body_drag
            .as_ref()
            .and_then(|drag| Some((drag.move_range.clone()?, drag.clip_id.clone())))
        else {
            return false;
        };
        let edit = self.begin_track_edit(TrackEditScope::tracks(
            self.state.range_scope_track_ids(&range),
        ));
        let splits = apply_range_split(&mut self.state, &range, true);
        let Some(anchor) = splits
            .iter()
            .find(|split| split.source_clip_id == pressed)
            .map(|split| ClipDragItem {
                clip_id: split.inside.id.clone(),
                source_track_id: split.track_id.clone(),
                start_beat: split.inside.start_beat,
            })
        else {
            self.state.restore_track_edit(&edit.before);
            return false;
        };
        // What moves: every piece inside the range. The range itself is
        // drawn again where the release puts it.
        self.state.selection.selected_clip_ids =
            splits.iter().map(|split| split.inside.id.clone()).collect();
        self.state.selection.selected_track_id = Some(anchor.source_track_id.clone());
        self.state.arrangement_range = None;
        self.range_move_edit = Some(edit);
        if let Some(drag) = self.clip_body_drag.as_mut() {
            drag.move_anchor = Some(anchor);
        }
        true
    }

    /// Commit a range move: the clip move's own commit, recorded with the
    /// split as one step (see `commit_clip_move`), and the range redrawn
    /// over what it moved.
    fn commit_range_move(
        &mut self,
        anchor: &ClipDragItem,
        range: Option<TimelineRangeSelection>,
        cx: &mut gpui::Context<Self>,
    ) {
        let target_index = self.clip_drag_target_track_index;
        let origin_start = self
            .clip_move_origin
            .as_ref()
            .filter(|origin| origin.anchor_clip_id == anchor.clip_id)
            .map(|origin| origin.anchor_start);
        let source_index = self.track_index(&anchor.source_track_id);
        self.extend_range_move_edit(target_index);
        self.commit_clip_move(&anchor.clip_id, target_index, cx);
        // A move that never got past its split still records the split's
        // undo step: the clips are cut either way.
        if let Some(edit) = self.range_move_edit.take() {
            self.record_range_move_edit(edit, cx);
        }
        if let (Some(range), Some(origin_start), Some(source_index)) =
            (range, origin_start, source_index)
        {
            if let Some((track_id, start)) = self
                .state
                .find_clip(&anchor.clip_id)
                .map(|(track, clip)| (track.id.clone(), clip.start_beat))
            {
                let delta = f64::from(start - origin_start);
                let track_delta = self
                    .track_index(&track_id)
                    .map_or(0, |index| index as isize - source_index as isize);
                let max_index = self.state.tracks.len().saturating_sub(1) as isize;
                let mut tracks: Vec<String> = Vec::with_capacity(range.track_ids.len());
                for id in &range.track_ids {
                    let Some(index) = self.track_index(id) else {
                        continue;
                    };
                    let moved = (index as isize + track_delta).clamp(0, max_index) as usize;
                    if let Some(track) = self.state.tracks.get(moved) {
                        if !tracks.contains(&track.id) {
                            tracks.push(track.id.clone());
                        }
                    }
                }
                self.state.arrangement_range = Some(TimelineRangeSelection::new(
                    range.start_beat + delta,
                    range.end_beat + delta,
                    tracks,
                ));
            }
        }
        self.clip_drag_origin = None;
        self.clip_drag_target_track_index = None;
        self.clip_move_hint_from = None;
    }

    /// A range move about to commit across tracks lands its pieces on
    /// tracks its split did not touch. Their "before" is still as it was —
    /// the preview only moves clips along their own track — so it joins the
    /// split's now, and the one undo step covers every track the move
    /// changes.
    fn extend_range_move_edit(&mut self, target_index: Option<usize>) {
        let (Some(target_index), Some(origin)) = (target_index, self.clip_move_origin.as_ref())
        else {
            return;
        };
        let Some(source_index) = origin
            .clips
            .iter()
            .find(|snapshot| snapshot.clip.id == origin.anchor_clip_id)
            .and_then(|snapshot| self.track_index(&snapshot.track_id))
        else {
            return;
        };
        let track_delta = target_index as isize - source_index as isize;
        if track_delta == 0 {
            return;
        }
        let max_index = self.state.tracks.len().saturating_sub(1) as isize;
        let destinations: Vec<String> = origin
            .clips
            .iter()
            .filter_map(|snapshot| self.track_index(&snapshot.track_id))
            .filter_map(|index| {
                let moved = (index as isize + track_delta).clamp(0, max_index) as usize;
                self.state.tracks.get(moved).map(|track| track.id.clone())
            })
            .collect();
        let Some(edit) = self.range_move_edit.as_mut() else {
            return;
        };
        let mut added: Vec<String> = Vec::new();
        for id in destinations {
            if !edit.scope.tracks.contains(&id) && !added.contains(&id) {
                added.push(id);
            }
        }
        if added.is_empty() {
            return;
        }
        let extra = self
            .state
            .capture_track_edit(&TrackEditScope::tracks(added.clone()));
        edit.before.tracks.extend(extra.tracks);
        edit.before.heights.extend(extra.heights);
        edit.scope.tracks.extend(added);
    }

    /// Record a range move — its split and the move — as one undo step.
    pub(super) fn record_range_move_edit(
        &mut self,
        edit: PendingTrackEdit,
        cx: &mut gpui::Context<Self>,
    ) {
        if self.commit_track_edit("Move Range", edit, false, cx) {
            self.mark_project_changed(cx);
            self.mark_media_changed(cx);
        }
    }

    /// Put back the clips a range move split, whole, when the move is
    /// cancelled. Returns whether a range move was in flight.
    pub(super) fn revert_range_move_split(&mut self) -> bool {
        let Some(edit) = self.range_move_edit.take() else {
            return false;
        };
        self.state.restore_track_edit(&edit.before);
        true
    }

    fn track_index(&self, track_id: &str) -> Option<usize> {
        self.state
            .tracks
            .iter()
            .position(|track| track.id == track_id)
    }

    // ── The window-wide pointer tracker ─────────────────────────────────

    /// A move the root's window-wide tracker caught: the marquee's or a clip
    /// body drag's, whichever is in flight.
    pub(super) fn track_pointer_drag(
        &mut self,
        window_pos: (f32, f32),
        bypass_snap: bool,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        if self.range_select_drag.is_some() {
            self.update_marquee(window_pos, window, cx);
        } else {
            self.update_clip_body_drag(window_pos, bypass_snap, window, cx);
        }
    }

    /// A release the root's window-wide tracker caught (`None`: it went to
    /// somebody else, seen as a move with no button down).
    pub(super) fn release_pointer_drag(
        &mut self,
        window_pos: Option<(f32, f32)>,
        bypass_snap: bool,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        if self.range_select_drag.is_some() {
            self.finish_marquee(window_pos, window, cx);
        } else {
            self.finish_clip_body_drag(window_pos, bypass_snap, window, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::timeline::timeline_state::{
        AudioClipStretchState, AudioImportState, MidiNoteState,
    };

    /// An audio clip `beats` long at `start` over a 10 s 48 kHz file.
    fn audio_clip(id: &str, start: f32, beats: f32) -> ClipState {
        let mut clip = ClipState {
            id: id.into(),
            name: "Vox".into(),
            start_beat: start,
            duration_beats: beats,
            source_duration_seconds: Some(10.0),
            offset_beats: 0.0,
            gain: 1.0,
            clip_type: ClipType::Audio {
                file_id: "file-1".into(),
                source_path: Some("vox.wav".into()),
            },
            muted: false,
            audio_import: AudioImportState::Ready,
            stretch: AudioClipStretchState::default(),
        };
        clip.stretch.original_sample_rate = 48_000;
        clip.stretch.project_sample_rate = 48_000;
        clip.stretch.original_duration_samples = 480_000;
        clip.stretch.source_end_samples = 480_000;
        clip
    }

    /// An audio track with a clip at beats 0..8 and a MIDI track with a clip
    /// at 0..8 holding notes at clip beats 1, 3 (crossing beat 4) and 6.
    fn state_with_clips() -> (TimelineState, String, String) {
        let mut state = TimelineState::default();
        let audio = state.create_audio_track();
        let midi = state.create_midi_track();
        let mut midi_clip = state.build_midi_clip(&midi, 0.0, 8.0).expect("clip");
        midi_clip.id = "clip-2".into();
        if let ClipType::Midi { notes, .. } = &mut midi_clip.clip_type {
            notes.push(MidiNoteState::new(60, 1.0, 1.0, 100));
            notes.push(MidiNoteState::new(62, 3.0, 2.0, 100));
            notes.push(MidiNoteState::new(64, 6.0, 1.0, 100));
        }
        for track in &mut state.tracks {
            if track.id == audio {
                track.clips.push(audio_clip("clip-1", 0.0, 8.0));
            } else if track.id == midi {
                track.clips.push(midi_clip.clone());
            }
        }
        (state, audio, midi)
    }

    fn spans(state: &TimelineState, track_id: &str) -> Vec<(f32, f32)> {
        let mut spans: Vec<(f32, f32)> = state
            .find_track(track_id)
            .expect("track")
            .clips
            .iter()
            .map(|clip| (clip.start_beat, clip.start_beat + clip.duration_beats))
            .collect();
        spans.sort_by(|a, b| a.0.total_cmp(&b.0));
        spans
    }

    fn pitches(state: &TimelineState, track_id: &str) -> Vec<Vec<(u8, f32, f32)>> {
        let mut clips: Vec<&ClipState> = state.find_track(track_id).unwrap().clips.iter().collect();
        clips.sort_by(|a, b| a.start_beat.total_cmp(&b.start_beat));
        clips
            .iter()
            .map(|clip| match &clip.clip_type {
                ClipType::Midi { notes, .. } => notes
                    .iter()
                    .map(|note| (note.pitch, note.start, note.duration))
                    .collect(),
                _ => Vec::new(),
            })
            .collect()
    }

    #[test]
    fn a_range_delete_leaves_a_gap_in_every_covered_clip() {
        let (mut state, audio, midi) = state_with_clips();
        let range = TimelineRangeSelection::new(2.0, 4.0, vec![audio.clone(), midi.clone()]);
        let removed = apply_range_split(&mut state, &range, false);

        assert_eq!(removed.len(), 2);
        assert_eq!(spans(&state, &audio), vec![(0.0, 2.0), (4.0, 8.0)]);
        assert_eq!(spans(&state, &midi), vec![(0.0, 2.0), (4.0, 8.0)]);
        // No ripple, and the audio after the gap is the audio that was there.
        let after = state
            .find_track(&audio)
            .unwrap()
            .clips
            .iter()
            .find(|clip| clip.start_beat == 4.0)
            .unwrap();
        assert_eq!(after.stretch.source_start_samples, 96_000);
        assert_eq!(after.stretch.source_end_samples, 480_000);
        // A note that starts inside the range goes with it, even the one that
        // sounds on past its end; the one after it keeps its time.
        assert_eq!(
            pitches(&state, &midi),
            vec![vec![(60, 1.0, 1.0)], vec![(64, 2.0, 1.0)]]
        );
    }

    #[test]
    fn a_range_on_one_track_leaves_the_others_alone() {
        let (mut state, audio, midi) = state_with_clips();
        let before = state.find_track(&midi).unwrap().clips.clone();
        let range = TimelineRangeSelection::new(2.0, 4.0, vec![audio.clone()]);
        apply_range_split(&mut state, &range, false);
        assert_eq!(spans(&state, &audio), vec![(0.0, 2.0), (4.0, 8.0)]);
        assert_eq!(state.find_track(&midi).unwrap().clips, before);
    }

    #[test]
    fn a_range_over_a_whole_clip_deletes_it() {
        let (mut state, audio, _) = state_with_clips();
        let range = TimelineRangeSelection::new(0.0, 12.0, vec![audio.clone()]);
        apply_range_split(&mut state, &range, false);
        assert!(state.find_track(&audio).unwrap().clips.is_empty());
    }

    #[test]
    fn a_range_copy_takes_the_inside_pieces_and_changes_nothing() {
        let (state, audio, midi) = state_with_clips();
        let tracks_before = state.tracks.clone();
        let range = TimelineRangeSelection::new(2.0, 4.0, vec![audio.clone(), midi.clone()]);
        let copy = state.plan_range_copy(&range);

        assert_eq!(state.tracks, tracks_before, "a copy changes nothing");
        assert_eq!(copy.len(), 2);
        for (track_id, _, clip) in &copy {
            assert_eq!((clip.start_beat, clip.duration_beats), (2.0, 2.0));
            assert!(*track_id == audio || *track_id == midi);
        }
        let (_, _, audio_piece) = copy.iter().find(|(id, _, _)| *id == audio).unwrap();
        assert_eq!(audio_piece.stretch.source_start_samples, 48_000);
        assert_eq!(audio_piece.stretch.source_end_samples, 96_000);
        let (_, _, midi_piece) = copy.iter().find(|(id, _, _)| *id == midi).unwrap();
        let ClipType::Midi { notes, .. } = &midi_piece.clip_type else {
            panic!("MIDI piece");
        };
        // The note starting before the range is not in the copy; the one
        // starting inside it is, from the range's start.
        let notes: Vec<_> = notes.iter().map(|n| (n.pitch, n.start)).collect();
        assert_eq!(notes, vec![(62, 1.0)]);
    }

    #[test]
    fn the_range_move_split_keeps_the_inside_pieces() {
        let (mut state, audio, _) = state_with_clips();
        let range = TimelineRangeSelection::new(2.0, 4.0, vec![audio.clone()]);
        let splits = apply_range_split(&mut state, &range, true);
        assert_eq!(splits.len(), 1);
        assert_eq!(splits[0].source_clip_id, "clip-1");
        assert_eq!(
            spans(&state, &audio),
            vec![(0.0, 2.0), (2.0, 4.0), (4.0, 8.0)]
        );
        assert!(state.find_clip(&splits[0].inside.id).is_some());
    }

    #[test]
    fn a_reverted_range_move_puts_the_clips_back_whole() {
        let (state, audio, midi) = state_with_clips();
        let mut timeline = Timeline::new();
        timeline.state = state;
        let before = timeline.state.tracks.clone();
        timeline.state.arrangement_range = Some(TimelineRangeSelection::new(
            2.0,
            4.0,
            vec![audio.clone(), midi],
        ));
        timeline.clip_body_drag = Some(ClipBodyDrag {
            clip_id: "clip-1".into(),
            track_id: audio.clone(),
            press_window: (0.0, 0.0),
            anchor_beat: 3.0,
            dragging: true,
            move_range: timeline.state.arrangement_range.clone(),
            move_anchor: None,
        });
        assert!(timeline.start_range_move());
        assert_eq!(spans(&timeline.state, &audio).len(), 3);
        assert_eq!(timeline.state.selection.selected_clip_ids.len(), 2);

        // Escape: the clip move reverts and the split goes with it.
        timeline.reset_input_state();
        assert_eq!(timeline.state.tracks, before);
        assert!(timeline.range_move_edit.is_none());
        assert!(timeline.clip_body_drag.is_none());
    }

    #[test]
    fn a_release_keeps_the_range_and_escape_clears_it() {
        let mut timeline = Timeline::new();
        timeline.state.arrangement_range = Some(TimelineRangeSelection::new(1.0, 2.0, vec![]));
        timeline.end_pointer_gestures();
        assert!(timeline.state.arrangement_range.is_some());
        timeline.reset_input_state();
        assert!(timeline.state.arrangement_range.is_none());
    }
}

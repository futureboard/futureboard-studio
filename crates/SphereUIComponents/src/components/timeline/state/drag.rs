use super::*;

#[derive(Debug, Clone)]
pub struct ClipDragItem {
    pub clip_id: String,
    pub source_track_id: String,
    pub start_beat: f32,
}

/// In-flight clip edge-resize drag payload (mirrors [`ClipDragItem`]). Carries
/// the clip identity, which edge is dragged, and the original bounds so the
/// handler can resolve the new length from the live cursor position.
///
/// Deliberately identity-only: the pre-gesture clip snapshot that the undo step
/// needs is captured by the timeline root on the first drag-move, before it
/// mutates anything (`Timeline::clip_resize_origin`). Carrying the whole
/// [`ClipState`] here instead meant cloning every note in the clip on each
/// repaint — the payload is built during element construction — and again on
/// each drag-move event.
#[derive(Debug, Clone)]
pub struct ClipResizeDrag {
    pub clip_id: String,
    pub edge: ClipEdge,
    pub start_beat: f32,
    pub duration_beats: f32,
}

#[derive(Debug, Clone)]
pub struct TrackDragItem {
    pub track_id: String,
    pub origin_index: usize,
    pub name: String,
    pub color: gpui::Rgba,
    pub is_group: bool,
}

/// In-flight track row height resize. Heights are resolved live from
/// [`TimelineState::update_track_height_resize`]; this payload only
/// carries identity + the gesture anchor.
#[derive(Debug, Clone)]
pub struct TrackHeightResizeDrag {
    pub anchor_track_id: String,
}

/// In-flight global (conductor) lane height resize. Identity only, for the
/// same reason as [`TrackHeightResizeDrag`]: the height is resolved live from
/// [`TimelineState::update_global_lane_resize`].
#[derive(Debug, Clone)]
pub struct GlobalLaneResizeDrag {
    pub kind: GlobalLaneKind,
}

/// How far the pointer must travel, in lane pixels, before a press on a
/// conductor-lane object becomes a move.
///
/// Without it every click on a flag is also a one-pixel nudge: the lanes seek
/// the playhead on press, so the pointer is already moving when the button
/// comes up.
pub const CONDUCTOR_DRAG_THRESHOLD_PX: f32 = 3.0;

/// In-flight arrangement-marker move on the global Marker lane.
///
/// This is a gesture *session* owned by the timeline root, not a GPUI
/// drag-and-drop payload. Nothing is being transferred to a drop target — the
/// flag follows the pointer inside its own lane — and the root's mouse-move
/// listener is the one path every other in-place timeline gesture already
/// shares (automation, range select, pen, tempo, meter). Markers used to be
/// the odd one out on `on_drag`, which is also the only conductor lane whose
/// move never worked.
#[derive(Debug, Clone, PartialEq)]
pub struct TimelineMarkerDrag {
    pub marker_id: String,
    /// Pointer x in lane pixels at mouse-down — the threshold is measured from
    /// here, not from the previous frame, so slow drags still arm.
    pub press_lane_x: f32,
    /// Beats between the marker's own beat and the grab point, so the flag
    /// keeps its offset under the cursor instead of snapping its left edge to
    /// it on the first move.
    pub grab_offset_beats: f64,
    /// Set once the marker has actually moved, so a plain click never marks the
    /// project dirty or writes an undo entry.
    pub moved: bool,
}

/// Where every clip of a move lands: one delta, from the grabbed clip, added
/// to every clip's start *at the origin of the gesture*.
///
/// `origin_starts` are the moving clips' starts when the drag began,
/// `anchor_origin` the grabbed clip's, and `anchor_target` where the grabbed
/// clip should now start (already snapped, or Shift-bypassed). Resolving from
/// the origin on every move, rather than nudging the current positions, means
/// a long drag cannot drift and a drop commits exactly what the last preview
/// showed. The delta is held so the earliest clip stops at beat 0, which
/// keeps the group's spacing intact instead of piling clips up at the start.
pub fn group_move_starts(origin_starts: &[f32], anchor_origin: f32, anchor_target: f32) -> Vec<f32> {
    let earliest = origin_starts
        .iter()
        .copied()
        .fold(anchor_origin, f32::min)
        .max(0.0);
    let delta = (anchor_target - anchor_origin).max(-earliest);
    origin_starts
        .iter()
        .map(|start| (start + delta).max(0.0))
        .collect()
}

impl TimelineState {
    /// Move a clip to `start_beat` on its own track, in place: its index in the
    /// track, the selection and every other field are left alone. The live
    /// preview of a clip move; the drop records the undo step.
    pub fn set_clip_start_in_place(&mut self, clip_id: &str, start_beat: f32) -> bool {
        let Some(clip) = self
            .tracks
            .iter_mut()
            .flat_map(|track| track.clips.iter_mut())
            .find(|clip| clip.id == clip_id)
        else {
            return false;
        };
        let start_beat = start_beat.max(0.0);
        if clip.start_beat == start_beat {
            return false;
        }
        clip.start_beat = start_beat;
        true
    }

    /// Move a clip onto another track at `start_beat`, appended to that track's
    /// clips as a drop always has. No snapping and no selection change: the
    /// caller resolved the position and restores the selection. Returns
    /// `false` when the clip or the track is missing.
    pub fn move_clip_to_track_unsnapped(
        &mut self,
        clip_id: &str,
        target_track_id: &str,
        start_beat: f32,
    ) -> bool {
        if !self.tracks.iter().any(|track| track.id == target_track_id) {
            return false;
        }
        let Some(source) = self
            .tracks
            .iter()
            .position(|track| track.clips.iter().any(|clip| clip.id == clip_id))
        else {
            return false;
        };
        if self.tracks[source].id == target_track_id {
            self.set_clip_start_in_place(clip_id, start_beat);
            return true;
        }
        let Some(index) = self.tracks[source]
            .clips
            .iter()
            .position(|clip| clip.id == clip_id)
        else {
            return false;
        };
        let mut clip = self.tracks[source].clips.remove(index);
        clip.start_beat = start_beat.max(0.0);
        if let Some(track) = self
            .tracks
            .iter_mut()
            .find(|track| track.id == target_track_id)
        {
            track.clips.push(clip);
        }
        true
    }
}

#[cfg(test)]
mod group_move_tests {
    use super::group_move_starts;

    #[test]
    fn every_clip_moves_by_the_anchor_delta_from_the_origin() {
        let starts = group_move_starts(&[4.0, 6.5, 9.0], 6.5, 8.0);
        assert_eq!(starts, vec![5.5, 8.0, 10.5]);
        // Resolving the same target again from the origin gives the same
        // answer: nothing accumulates between moves.
        assert_eq!(group_move_starts(&[4.0, 6.5, 9.0], 6.5, 8.0), starts);
        // Back to where it started is exactly where it started.
        assert_eq!(
            group_move_starts(&[4.0, 6.5, 9.0], 6.5, 6.5),
            vec![4.0, 6.5, 9.0]
        );
    }

    /// Dragging a group hard left stops it with its earliest clip at 0 and its
    /// spacing intact; it used to squash every clip that hit 0 onto the others.
    #[test]
    fn a_group_stops_at_beat_zero_with_its_spacing_intact() {
        let starts = group_move_starts(&[2.0, 5.0, 7.0], 5.0, 0.0);
        assert_eq!(starts, vec![0.0, 3.0, 5.0]);
        // A move right after that still resolves from the origin.
        assert_eq!(
            group_move_starts(&[2.0, 5.0, 7.0], 5.0, 6.0),
            vec![3.0, 6.0, 8.0]
        );
    }

    #[test]
    fn a_single_clip_follows_the_target() {
        assert_eq!(group_move_starts(&[3.0], 3.0, 7.25), vec![7.25]);
        assert_eq!(group_move_starts(&[3.0], 3.0, -2.0), vec![0.0]);
    }
}

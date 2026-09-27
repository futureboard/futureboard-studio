//! Cancelling a clip move or Option-drag clone mid-drag.
//!
//! A clip move is a GPUI drag (`ClipDragItem`): each move previews the clips
//! from their origin, and the drop records one `UpdateClips`. Escape and a
//! tool change end it without a commit — the clips go back where the press
//! found them — but GPUI keeps delivering that drag's moves, its 16 ms
//! repeats and its drop until the button comes up. Those are refused until the
//! next press, so no later move captures a fresh origin and resnaps a clip,
//! and no drop records an undo step for a move the user cancelled. With a
//! window at hand the drag is stopped at once; without one (a menu command)
//! its next move stops it.

use super::*;

impl Timeline {
    /// Put the clips of an uncommitted clip move back where the gesture found
    /// them and forget the move. The preview only ever moves clips along
    /// their own track, in place. Returns whether a move was in flight.
    pub(super) fn revert_clip_move(&mut self) -> bool {
        let Some(origin) = self.clip_move_origin.take() else {
            return false;
        };
        for snapshot in &origin.clips {
            self.state
                .set_clip_start_in_place(&snapshot.clip.id, snapshot.clip.start_beat);
        }
        true
    }

    /// What a cancel does to the clip drag, without a window or context:
    /// revert the move, forget the press, and refuse the rest of the drag.
    ///
    /// The drag is refused when a clip gesture shows (a captured move, an
    /// Option-drag clone, a press waiting for its click) or when any platform
    /// drag is active (`platform_drag_active`), since a press on an unselected
    /// clip leaves no trace until its drag's first move. Refusing a drag that
    /// is not a clip's costs nothing: only `ClipDragItem` events check it, and
    /// the next press lifts it. Returns whether a clip gesture showed, which
    /// is when the caller may stop the platform drag as the clip's own.
    pub(super) fn cancel_clip_drag_state(&mut self, platform_drag_active: bool) -> bool {
        let clip_gesture = self.clip_move_origin.is_some()
            || self.clip_clone_drag_id.is_some()
            || self.clip_drag_origin.is_some()
            || self.pending_clip_click.is_some();
        self.revert_clip_move();
        self.clip_drag_origin = None;
        self.clip_clone_drag_id = None;
        self.clip_clone_hint = None;
        self.clip_drag_target_track_index = None;
        self.pending_clip_click = None;
        if clip_gesture || platform_drag_active {
            self.clip_drag_cancelled = true;
        }
        clip_gesture
    }

    /// Escape or a tool change during a clip move or Option-drag clone: the
    /// clips go back, nothing is recorded, and the drag stops — at once with a
    /// window, else at its next move. Call it before `reset_input_state`.
    /// Returns whether a clip gesture was in flight.
    pub fn cancel_clip_drag(
        &mut self,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) -> bool {
        let clip_gesture = self.cancel_clip_drag_state(cx.has_active_drag());
        if clip_gesture {
            if let Some(window) = window {
                cx.stop_active_drag(window);
            }
            cx.notify();
        }
        clip_gesture
    }

    /// Whether a `ClipDragItem` move or drop belongs to a live gesture:
    /// `false` from a cancel until the next press.
    pub(super) fn clip_drag_accepted(&self) -> bool {
        !self.clip_drag_cancelled
    }

    /// A press in the arrangement starts a new gesture, and any clip drag that
    /// follows is its own. Called from the root's capture phase, before a clip
    /// sees the press.
    pub(super) fn note_arrangement_press(&mut self) {
        self.clip_drag_cancelled = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One MIDI track with one clip at `start`; returns the timeline and the
    /// clip id.
    fn timeline_with_clip(start: f32) -> (Timeline, String) {
        let mut timeline = Timeline::new();
        let track_id = timeline.state.create_midi_track();
        let clip = timeline
            .state
            .build_midi_clip(&track_id, start, 4.0)
            .expect("clip");
        let clip_id = clip.id.clone();
        timeline
            .state
            .tracks
            .iter_mut()
            .find(|track| track.id == track_id)
            .expect("track")
            .clips
            .push(clip);
        (timeline, clip_id)
    }

    fn clip_start(timeline: &Timeline, clip_id: &str) -> f32 {
        timeline
            .state
            .find_clip(clip_id)
            .map(|(_, clip)| clip.start_beat)
            .expect("clip")
    }

    /// The review's case: a clip at beat 4.3 is dragged, Escape is pressed
    /// and the button comes up without a move. The clip must be back at 4.3,
    /// and the drag's later moves and its drop refused, so nothing resnaps it
    /// to 4.25 and no `UpdateClips` is recorded.
    #[test]
    fn a_cancelled_clip_move_goes_back_and_refuses_the_rest_of_its_drag() {
        let (mut timeline, clip_id) = timeline_with_clip(4.3);
        let snapshot = ClipSnapshot::capture(&timeline.state, &clip_id).expect("snapshot");
        timeline.clip_move_origin = Some(ClipMoveOrigin {
            anchor_clip_id: clip_id.clone(),
            anchor_start: 4.3,
            clips: vec![snapshot],
        });
        timeline.state.set_clip_start_in_place(&clip_id, 6.0);
        assert!(timeline.clip_drag_accepted());

        assert!(timeline.cancel_clip_drag_state(true));

        assert_eq!(clip_start(&timeline, &clip_id), 4.3);
        assert!(timeline.clip_move_origin.is_none());
        assert!(!timeline.clip_drag_accepted());
        // The mouse-up reset that follows does not lift the refusal: only a
        // new press does.
        timeline.reset_input_state();
        assert!(!timeline.clip_drag_accepted());
        timeline.note_arrangement_press();
        assert!(timeline.clip_drag_accepted());
    }

    /// An Option-drag clone cancelled before its drop must not turn into a
    /// move of the original: the clone is forgotten and the drag refused.
    #[test]
    fn a_cancelled_clone_drag_is_refused_too() {
        let (mut timeline, clip_id) = timeline_with_clip(2.0);
        timeline.clip_clone_drag_id = Some(clip_id.clone());

        assert!(timeline.cancel_clip_drag_state(false));

        assert!(timeline.clip_clone_drag_id.is_none());
        assert!(!timeline.clip_drag_accepted());
        assert_eq!(clip_start(&timeline, &clip_id), 2.0);
    }

    /// A press on an unselected clip leaves no trace until the drag's first
    /// move, so an active platform drag alone refuses the rest of it — but is
    /// not claimed as the clip's to stop.
    #[test]
    fn a_platform_drag_without_a_clip_trace_is_refused_but_not_claimed() {
        let (mut timeline, _) = timeline_with_clip(2.0);
        assert!(!timeline.cancel_clip_drag_state(true));
        assert!(!timeline.clip_drag_accepted());
    }

    /// With no clip gesture and no drag, a cancel leaves clip drags alone.
    #[test]
    fn a_cancel_with_no_drag_refuses_nothing() {
        let (mut timeline, _) = timeline_with_clip(2.0);
        assert!(!timeline.cancel_clip_drag_state(false));
        assert!(timeline.clip_drag_accepted());
    }
}

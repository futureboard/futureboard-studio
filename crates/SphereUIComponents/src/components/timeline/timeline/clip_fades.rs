//! Clip gain, fade and crossfade gestures on the arrangement.
//!
//! The clip elements forward raw presses and window x; everything is resolved
//! here, through the timeline's one transform and grid, and committed once
//! when the button comes up (`finish_clip_handle_gestures`, called from the
//! timeline root's mouse-up in both of its forms). Escape, a tool change and
//! focus loss cancel through [`Timeline::cancel_clip_handle_gesture`], which
//! restores the clips and stops the drag — never through `reset_input_state`,
//! which runs on every mouse-up and, for a release outside the timeline, before
//! the commit.
//!
//! What each update and a cancel do to the arrangement is decided without a
//! GPUI context ([`Timeline::clip_process_update_state`],
//! [`Timeline::cancel_clip_handle_state`]), so it is unit-tested; the
//! context-taking wrappers only repaint and stop the platform drag.

use super::*;
use crate::components::timeline::audio_clip::{
    clip_fade_handle_layout, AudioClipProcessUpdate, CLIP_BORDER,
};
use crate::components::timeline::crossfade_overlay::CrossfadeGesture;
use crate::components::timeline::fade_handle_overlay::{
    FadeHandleCell, FadeHandleFrame, FadeHandleMark, FadeHandleOverlay,
};
use crate::components::timeline::timeline_state::{
    clip_edit_is_noop, clip_press_plan, ClipSelectOp, CrossfadeBlock, FadeEdge, TrackRowLayout,
    DEFAULT_CROSSFADE_SECONDS, MIN_CROSSFADE_SECONDS,
};

/// What a clip-process update asks of the view.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct ClipProcessEffect {
    /// The arrangement changed (a clip, or the selection): re-render it.
    pub redraw: bool,
    /// The hovered or dragged clip's fade handles may have moved: re-resolve
    /// the overlay's frame (a redraw does that too).
    pub handles: bool,
}

/// A fade handle being dragged.
#[derive(Debug, Clone)]
struct FadeGesture {
    clip_id: String,
    edge: FadeEdge,
    /// Window px between the press and the fade's end, so the end follows the
    /// pointer's motion instead of jumping under it.
    grab_dx: f32,
}

/// A crossfade handle being dragged: both clips as the press found them. Each
/// move re-plans from these, so the preview never accumulates rounding.
#[derive(Debug, Clone)]
struct CrossfadeDrag {
    left: ClipSnapshot,
    right: ClipSnapshot,
    press_window_x: f32,
    center_seconds: f64,
    origin_seconds: f64,
}

/// The arrangement's fade and crossfade handle state.
#[derive(Default)]
pub(super) struct ClipFadeUi {
    fade: Option<FadeGesture>,
    crossfade: Option<CrossfadeDrag>,
    /// Set by a cancel. The cancelled gesture's later drag events are
    /// ignored until the next press starts a new one.
    cancelled: bool,
    /// The audio clip under the pointer, whose fade handles the overlay shows.
    hover: Option<String>,
    frame: FadeHandleCell,
    overlay: Option<gpui::Entity<FadeHandleOverlay>>,
}

impl Timeline {
    /// The fade handle overlay, built on first render like the razor line's.
    pub(super) fn fade_handle_overlay(
        &mut self,
        cx: &mut Context<Self>,
    ) -> gpui::Entity<FadeHandleOverlay> {
        if let Some(overlay) = self.clip_fades.overlay.as_ref() {
            return overlay.clone();
        }
        let frame = self.clip_fades.frame.clone();
        let overlay = cx.new(|_| FadeHandleOverlay::new(frame));
        self.clip_fades.overlay = Some(overlay.clone());
        overlay
    }

    /// A clip-gain, fade or crossfade gesture is in flight.
    pub fn clip_handle_gesture_active(&self) -> bool {
        self.clip_process_origin.is_some()
            || self.clip_fades.fade.is_some()
            || self.clip_fades.crossfade.is_some()
    }

    /// Where the overlay draws the fade handles this frame, placed in `rows`
    /// (the frame's track rows): those of the clip whose fade is being
    /// dragged, drawn active over the clip's own squares, else of the hovered
    /// one — unless it is selected (it draws its own) or off the visible
    /// track area.
    fn fade_handle_frame(&self, rows: &TrackRowLayout) -> Option<FadeHandleFrame> {
        if self.state.active_tool != TimelineTool::Pointer {
            return None;
        }
        let (clip_id, active) = match (&self.clip_fades.fade, &self.clip_fades.hover) {
            (Some(gesture), _) => (gesture.clip_id.as_str(), true),
            (None, Some(hover)) => (hover.as_str(), false),
            (None, None) => return None,
        };
        if !active
            && self
                .state
                .display_selection()
                .selected_clip_ids
                .iter()
                .any(|id| id == clip_id)
        {
            return None;
        }
        let index = self
            .state
            .tracks
            .iter()
            .position(|track| track.clips.iter().any(|clip| clip.id == clip_id))?;
        let track = &self.state.tracks[index];
        let clip = track.clips.iter().find(|clip| clip.id == clip_id)?;
        if !matches!(clip.clip_type, ClipType::Audio { .. }) {
            return None;
        }
        let row = rows.row_for_index(index)?;
        if row.height <= 0.0 {
            return None;
        }
        let crossfades = self.state.audio_crossfades(track);
        let fades = self.state.effective_clip_fades(clip, &crossfades);
        let rect = self.state.clip_lane_rect(clip, row.height);
        let handles = clip_fade_handle_layout(
            &self.state.clip_time_axis(clip),
            &fades,
            rect.left,
            rect.width,
            rect.height,
            false,
        );
        // The clip's inner box in window coordinates: the rows' transform.
        let lane_left = self.state.lane_origin_x();
        let track_top = self.state.timeline_origin_y() + self.state.arrangement_content_top();
        let origin_x = lane_left + rect.left + CLIP_BORDER;
        let origin_y = track_top + row.y - self.state.viewport.scroll_y + rect.top + CLIP_BORDER;
        let lane_right = lane_left + self.state.viewport.viewport_width;
        let track_bottom = track_top + self.state.viewport.viewport_height;
        let mark = |rect: crate::components::timeline::audio_clip::FadeHandleRect| {
            let left = origin_x + rect.mark.left;
            let top = origin_y + rect.mark.top;
            let size = rect.mark.width;
            (left >= lane_left
                && left + size <= lane_right
                && top >= track_top
                && top + size <= track_bottom)
                .then_some(FadeHandleMark { left, top, size })
        };
        let frame = FadeHandleFrame {
            fade_in: handles.fade_in.and_then(mark),
            fade_out: handles.fade_out.and_then(mark),
            active,
            disabled: track.ara.is_some(),
        };
        (frame.fade_in.is_some() || frame.fade_out.is_some()).then_some(frame)
    }

    /// Bring the overlay's frame in line with the arrangement, laid out in
    /// this frame's `rows`. Called by `render`, which the overlay renders
    /// with, so no notify is needed there.
    pub(super) fn refresh_fade_handle_frame(&self, rows: &TrackRowLayout) {
        self.clip_fades.frame.set(self.fade_handle_frame(rows));
    }

    /// Re-resolve the overlay's frame outside a render (a hover) and repaint
    /// the overlay alone if it moved. Placed in the rows the last frame drew,
    /// which are the ones under the pointer.
    fn publish_fade_handle_frame(&self, cx: &mut gpui::App) {
        let next = match self.frame_lane_ctx.as_ref() {
            Some(ctx) => self.fade_handle_frame(&ctx.row_layout),
            None => self.fade_handle_frame(&self.state.track_row_layout()),
        };
        if self.clip_fades.frame.get() == next {
            return;
        }
        self.clip_fades.frame.set(next);
        if let Some(overlay) = self.clip_fades.overlay.as_ref() {
            overlay.update(cx, |_, cx| cx.notify());
        }
    }

    /// Window x of where `clip_id`'s fade on `edge` ends (fade-in) or starts
    /// (fade-out), through the transform the handle is drawn with.
    fn fade_end_window_x(&self, clip_id: &str, edge: FadeEdge) -> Option<f32> {
        let (track, clip) = self.state.find_clip(clip_id)?;
        let crossfades = self.state.audio_crossfades(track);
        let fades = self.state.effective_clip_fades(clip, &crossfades);
        let seconds = match edge {
            FadeEdge::In => fades.in_seconds,
            FadeEdge::Out => fades.played_seconds - fades.out_seconds,
        };
        Some(
            self.state.lane_origin_x()
                + self
                    .state
                    .clip_time_axis(clip)
                    .lane_x_at_local_seconds(seconds),
        )
    }

    /// Apply a left press on `clip_id` to the selection exactly as a press on
    /// the clip's body does ([`clip_press_plan`]): an unselected clip is
    /// selected now (Cmd/Ctrl adds it), a selected one keeps the selection
    /// until the release, which the pending click then narrows or toggles —
    /// unless the press became a drag. Returns whether the selection changed
    /// now.
    pub(super) fn press_clip_selection(&mut self, clip_id: &str, additive: bool) -> bool {
        let already_selected = self
            .state
            .selection
            .selected_clip_ids
            .iter()
            .any(|id| id == clip_id);
        let plan = clip_press_plan(already_selected, additive);
        self.pending_clip_click = plan.on_click.map(|op| PendingClipClick {
            clip_id: clip_id.to_string(),
            op,
        });
        match plan.on_press {
            Some(ClipSelectOp::Replace) => self.state.select_clip(clip_id),
            Some(ClipSelectOp::Toggle) => self.state.select_clip_additive(clip_id),
            None => return false,
        }
        true
    }

    /// The first change of a gain or fade gesture remembers the clip as it
    /// was, for the one undo entry its release records.
    fn capture_clip_process_origin(&mut self, clip_id: &str) {
        if self
            .clip_process_origin
            .as_ref()
            .is_none_or(|origin| origin.id != clip_id)
        {
            self.clip_process_origin = self.state.find_clip(clip_id).map(|(_, clip)| clip.clone());
        }
    }

    /// One update from an audio clip's gain control, fade handles or hover.
    pub(super) fn apply_clip_process_update(
        &mut self,
        clip_id: &str,
        update: AudioClipProcessUpdate,
        cx: &mut Context<Self>,
    ) {
        let effect = self.clip_process_update_state(clip_id, update);
        if effect.redraw {
            // The render re-resolves the overlay's frame.
            cx.notify();
        } else if effect.handles {
            self.publish_fade_handle_frame(cx);
        }
    }

    /// What one clip-process update does to the arrangement, without a GPUI
    /// context.
    ///
    /// A press on a fade handle selects the clip as a press on its body does
    /// ([`Self::press_clip_selection`]), so a plain click there still selects
    /// it; the fade only changes once the press becomes a drag, which drops
    /// the click's pending selection change. A double-click resets the fade.
    /// After a cancel the gesture's later updates are ignored until the next
    /// press.
    pub(super) fn clip_process_update_state(
        &mut self,
        clip_id: &str,
        update: AudioClipProcessUpdate,
    ) -> ClipProcessEffect {
        let redraw = |changed: bool| ClipProcessEffect {
            redraw: changed,
            handles: false,
        };
        match update {
            AudioClipProcessUpdate::Hover(hovered) => {
                if hovered {
                    self.clip_fades.hover = Some(clip_id.to_string());
                } else if self.clip_fades.hover.as_deref() == Some(clip_id) {
                    self.clip_fades.hover = None;
                }
                ClipProcessEffect {
                    redraw: false,
                    handles: true,
                }
            }
            AudioClipProcessUpdate::GainPress => {
                self.clip_fades.cancelled = false;
                ClipProcessEffect::default()
            }
            AudioClipProcessUpdate::Gain(gain) => {
                if !self.clip_handle_drag_accepted() {
                    return ClipProcessEffect::default();
                }
                self.capture_clip_process_origin(clip_id);
                redraw(self.state.set_clip_gain(clip_id, gain))
            }
            AudioClipProcessUpdate::FadePress {
                edge,
                window_x,
                additive,
            } => {
                self.clip_fades.cancelled = false;
                if self.state.find_clip(clip_id).is_none() {
                    return ClipProcessEffect::default();
                }
                let selected = self.press_clip_selection(clip_id, additive);
                let effect = ClipProcessEffect {
                    redraw: selected,
                    handles: true,
                };
                let Some((track, clip)) = self.state.find_clip(clip_id) else {
                    return effect;
                };
                if track.ara.is_some() {
                    return effect;
                }
                let crossfades = self.state.audio_crossfades(track);
                if self
                    .state
                    .effective_clip_fades(clip, &crossfades)
                    .crossfaded(edge)
                {
                    return effect;
                }
                let Some(fade_x) = self.fade_end_window_x(clip_id, edge) else {
                    return effect;
                };
                self.capture_clip_process_origin(clip_id);
                self.clip_fades.fade = Some(FadeGesture {
                    clip_id: clip_id.to_string(),
                    edge,
                    grab_dx: window_x - fade_x,
                });
                effect
            }
            AudioClipProcessUpdate::FadeDrag {
                edge,
                window_x,
                bypass_snap,
            } => {
                if !self.clip_handle_drag_accepted() {
                    return ClipProcessEffect::default();
                }
                let Some(grab_dx) = self
                    .clip_fades
                    .fade
                    .as_ref()
                    .filter(|gesture| gesture.clip_id == clip_id && gesture.edge == edge)
                    .map(|gesture| gesture.grab_dx)
                else {
                    return ClipProcessEffect::default();
                };
                // A drag, not a click: the release leaves the selection as the
                // press made it.
                self.pending_clip_click = None;
                // The fade end follows the pointer through the arrangement's
                // transform and grid (Shift for free), then into the clip's
                // own time.
                let beat = self.beat_from_window_x(window_x - grab_dx);
                let snapped = self.snap_beat_with_bypass(beat, bypass_snap);
                redraw(
                    self.state
                        .set_clip_fade_at_beat(clip_id, edge, snapped as f64),
                )
            }
            AudioClipProcessUpdate::FadeReset { edge, additive } => {
                self.clip_fades.cancelled = false;
                self.clip_fades.fade = None;
                let Some((track, _)) = self.state.find_clip(clip_id) else {
                    return ClipProcessEffect::default();
                };
                let ara = track.ara.is_some();
                let selected = self.press_clip_selection(clip_id, additive);
                if ara {
                    return redraw(selected);
                }
                self.capture_clip_process_origin(clip_id);
                let changed = self.state.set_clip_fade_seconds(clip_id, edge, 0.0);
                redraw(selected || changed)
            }
        }
    }

    /// Record the gain or fade gesture on `clip_id` (any clip when `None`) as
    /// one `UpdateClip`, unless it ended where it started.
    pub(super) fn commit_clip_process_gesture(
        &mut self,
        clip_id: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        // Every other clip's gain control reports its release too.
        let Some(original) = self
            .clip_process_origin
            .take_if(|origin| clip_id.is_none_or(|id| origin.id == id))
        else {
            return;
        };
        self.clip_fades.fade = None;
        if let Some(next) = ClipSnapshot::capture(&self.state, &original.id) {
            if !clip_edit_is_noop(&original, &next.clip) {
                let previous = ClipSnapshot {
                    track_id: next.track_id.clone(),
                    clip: original,
                    index: next.index,
                };
                self.record_executed_command(EditCommand::UpdateClip { previous, next }, cx);
                self.mark_project_changed(cx);
                self.mark_media_changed(cx);
            }
        }
        self.publish_fade_handle_frame(cx);
        cx.notify();
    }

    /// One gesture on a crossfade handle.
    pub(super) fn apply_crossfade_gesture(
        &mut self,
        gesture: &CrossfadeGesture,
        cx: &mut Context<Self>,
    ) {
        let CrossfadeGesture::Reset { left_id, right_id } = gesture else {
            if self.crossfade_drag_state(gesture) {
                cx.notify();
            }
            return;
        };
        self.clip_fades.cancelled = false;
        self.clip_fades.crossfade = None;
        let (Some(left), Some(right)) = (
            ClipSnapshot::capture(&self.state, left_id),
            ClipSnapshot::capture(&self.state, right_id),
        ) else {
            return;
        };
        let center = self.state.crossfade_center_seconds(&left.clip, &right.clip);
        if let Some(plan) =
            self.state
                .plan_crossfade(&left.clip, &right.clip, center, DEFAULT_CROSSFADE_SECONDS)
        {
            self.run_clip_pair_edit(left, right, plan.left, plan.right, cx);
        }
    }

    /// A press or drag on a crossfade handle, without a GPUI context: the
    /// press remembers both clips, each move re-plans the pair from them.
    /// Returns whether the clips changed. A pair the planner cannot resize
    /// ([`TimelineState::can_adjust_crossfade`]) starts no drag, since its
    /// handle is drawn disabled. After a cancel, the drag's later moves are
    /// ignored until the next press.
    pub(super) fn crossfade_drag_state(&mut self, gesture: &CrossfadeGesture) -> bool {
        match gesture {
            CrossfadeGesture::Press {
                left_id,
                right_id,
                window_x,
            } => {
                self.clip_fades.cancelled = false;
                let (Some(left), Some(right)) = (
                    ClipSnapshot::capture(&self.state, left_id),
                    ClipSnapshot::capture(&self.state, right_id),
                ) else {
                    return false;
                };
                let crossfade = self
                    .state
                    .tracks
                    .iter()
                    .find(|track| track.id == left.track_id)
                    .filter(|track| track.ara.is_none())
                    .and_then(|track| {
                        self.state
                            .audio_crossfades(track)
                            .crossfades
                            .into_iter()
                            .find(|xf| xf.left_id == *left_id && xf.right_id == *right_id)
                    });
                let Some(crossfade) = crossfade else {
                    return false;
                };
                if !self.state.can_adjust_crossfade(&left.clip, &right.clip) {
                    return false;
                }
                let center_seconds = self.state.crossfade_center_seconds(&left.clip, &right.clip);
                self.clip_fades.crossfade = Some(CrossfadeDrag {
                    left,
                    right,
                    press_window_x: *window_x,
                    center_seconds,
                    origin_seconds: crossfade.seconds,
                });
                false
            }
            CrossfadeGesture::Drag {
                left_id,
                right_id,
                window_x,
            } => {
                if !self.clip_handle_drag_accepted() {
                    return false;
                }
                let Some(drag) = self.clip_fades.crossfade.as_ref().filter(|drag| {
                    drag.left.clip.id == *left_id && drag.right.clip.id == *right_id
                }) else {
                    return false;
                };
                // Right lengthens, left shortens, by twice the pointer's travel
                // in real time: both edges move, about the same centre.
                let from = self.beat_from_window_x(drag.press_window_x) as f64;
                let to = self.beat_from_window_x(*window_x) as f64;
                let delta = self.state.seconds_at_beat(to) - self.state.seconds_at_beat(from);
                let length = (drag.origin_seconds + 2.0 * delta).max(MIN_CROSSFADE_SECONDS);
                let (left, right) = if (length - drag.origin_seconds).abs() < 1.0e-6 {
                    // Back where it started: the clips exactly as they were,
                    // not a re-plan, so the release records nothing.
                    (drag.left.clip.clone(), drag.right.clip.clone())
                } else {
                    let Some(plan) = self.state.plan_crossfade(
                        &drag.left.clip,
                        &drag.right.clip,
                        drag.center_seconds,
                        length,
                    ) else {
                        return false;
                    };
                    (plan.left, plan.right)
                };
                let left_changed = self.state.replace_clip_in_place(&left);
                let right_changed = self.state.replace_clip_in_place(&right);
                left_changed || right_changed
            }
            CrossfadeGesture::Reset { .. } => false,
        }
    }

    /// Record a two-clip edit already planned, as one `UpdateClips`.
    fn run_clip_pair_edit(
        &mut self,
        left: ClipSnapshot,
        right: ClipSnapshot,
        next_left: ClipState,
        next_right: ClipState,
        cx: &mut Context<Self>,
    ) -> bool {
        if clip_edit_is_noop(&left.clip, &next_left) && clip_edit_is_noop(&right.clip, &next_right)
        {
            return false;
        }
        let next = vec![
            ClipSnapshot {
                track_id: left.track_id.clone(),
                clip: next_left,
                index: left.index,
            },
            ClipSnapshot {
                track_id: right.track_id.clone(),
                clip: next_right,
                index: right.index,
            },
        ];
        self.run_edit_command(
            EditCommand::UpdateClips {
                previous: vec![left, right],
                next,
            },
            cx,
        );
        self.mark_media_changed(cx);
        true
    }

    /// Record the crossfade drag in flight as one `UpdateClips`, unless it
    /// ended where it started.
    fn commit_crossfade_drag(&mut self, cx: &mut Context<Self>) {
        let Some(drag) = self.clip_fades.crossfade.take() else {
            return;
        };
        let (Some(left), Some(right)) = (
            ClipSnapshot::capture(&self.state, &drag.left.clip.id),
            ClipSnapshot::capture(&self.state, &drag.right.clip.id),
        ) else {
            return;
        };
        if clip_edit_is_noop(&drag.left.clip, &left.clip)
            && clip_edit_is_noop(&drag.right.clip, &right.clip)
        {
            return;
        }
        self.record_executed_command(
            EditCommand::UpdateClips {
                previous: vec![drag.left, drag.right],
                next: vec![left, right],
            },
            cx,
        );
        self.mark_media_changed(cx);
    }

    /// The button came up: commit whatever handle gesture is in flight. Called
    /// from the timeline root's mouse-up, inside and outside, so a release
    /// anywhere ends the gesture exactly once.
    pub(super) fn finish_clip_handle_gestures(&mut self, cx: &mut Context<Self>) {
        self.commit_clip_process_gesture(None, cx);
        self.commit_crossfade_drag(cx);
        if self.clip_fades.fade.take().is_some() {
            self.publish_fade_handle_frame(cx);
        }
    }

    /// Cancel the clip-gain, fade or crossfade gesture in flight: put the clips
    /// back as the gesture found them and end the drag, recording nothing.
    /// Returns whether there was one. With a window the GPUI drag is stopped
    /// outright; without one (a menu command) its remaining moves are ignored.
    pub fn cancel_clip_handle_gesture(
        &mut self,
        window: Option<&mut gpui::Window>,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.cancel_clip_handle_state() {
            return false;
        }
        if let Some(window) = window {
            cx.stop_active_drag(window);
        }
        cx.notify();
        true
    }

    /// What a cancel does, without a GPUI context: every clip the gesture
    /// touched goes back exactly as its press found it (the gain or fade
    /// clip, or both crossfade clips), nothing is recorded, and the rest of
    /// the gesture's drag is refused ([`Self::clip_handle_drag_accepted`])
    /// until the next press. Returns whether a gesture was in flight.
    pub(super) fn cancel_clip_handle_state(&mut self) -> bool {
        if !self.clip_handle_gesture_active() {
            return false;
        }
        if let Some(origin) = self.clip_process_origin.take() {
            self.state.replace_clip_in_place(&origin);
        }
        if let Some(drag) = self.clip_fades.crossfade.take() {
            self.state.replace_clip_in_place(&drag.left.clip);
            self.state.replace_clip_in_place(&drag.right.clip);
        }
        self.clip_fades.fade = None;
        self.clip_fades.cancelled = true;
        true
    }

    /// Whether a gain, fade or crossfade drag update belongs to a live
    /// gesture: `false` from a cancel until the next press.
    pub(super) fn clip_handle_drag_accepted(&self) -> bool {
        !self.clip_fades.cancelled
    }

    /// `audio:create-crossfade`: crossfade the two selected touching audio
    /// clips (or the boundary a range spans) as one undo step. The error is a
    /// short reason for the status bar when there is nothing to crossfade.
    pub fn create_crossfade_at_selection(&mut self, cx: &mut Context<Self>) -> Result<(), String> {
        let request = self.state.crossfade_request()?;
        let (Some(left), Some(right)) = (
            ClipSnapshot::capture(&self.state, &request.left_id),
            ClipSnapshot::capture(&self.state, &request.right_id),
        ) else {
            return Err("The clips to crossfade are gone".to_string());
        };
        let plan = self
            .state
            .plan_crossfade(
                &left.clip,
                &right.clip,
                request.center_seconds,
                request.length_seconds,
            )
            .ok_or_else(|| CrossfadeBlock::NoRoom.command_message().to_string())?;
        self.run_clip_pair_edit(left, right, plan.left, plan.right, cx);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::timeline::timeline_state::{AudioClipStretchState, AudioImportState};

    /// A decoded audio clip `seconds` long at `start_beat`, playing the
    /// middle of a file with a second of audio on each side of it.
    fn audio_clip(id: &str, start_beat: f32, seconds: f64) -> ClipState {
        let rate = 48_000.0;
        let mut clip = ClipState {
            id: id.to_string(),
            name: id.to_string(),
            start_beat,
            duration_beats: (seconds * 2.0) as f32,
            source_duration_seconds: Some(seconds + 2.0),
            offset_beats: 0.0,
            gain: 1.0,
            clip_type: ClipType::Audio {
                file_id: id.to_string(),
                source_path: Some(format!("{id}.wav")),
            },
            muted: false,
            audio_import: AudioImportState::Ready,
            stretch: AudioClipStretchState::default(),
        };
        clip.stretch.original_sample_rate = 48_000;
        clip.stretch.project_sample_rate = 48_000;
        clip.stretch.original_duration_samples = ((seconds + 2.0) * rate).round() as u64;
        clip.stretch.source_start_samples = rate as u64;
        clip.stretch.source_end_samples = ((seconds + 1.0) * rate).round() as u64;
        clip
    }

    /// One audio track at 120 BPM (two beats a second), nothing selected.
    fn timeline_with(clips: Vec<ClipState>) -> Timeline {
        let mut timeline = Timeline::new();
        timeline.state.bpm = 120.0;
        timeline.state.tracks.clear();
        let track_id = timeline.state.create_audio_track();
        timeline
            .state
            .tracks
            .iter_mut()
            .find(|track| track.id == track_id)
            .expect("track")
            .clips = clips;
        timeline.state.reconcile_audio_clip_lengths();
        timeline.state.viewport.pixels_per_second = 100.0;
        timeline.state.sync_pixels_per_beat();
        timeline.state.selection.selected_clip_ids.clear();
        timeline
    }

    fn clip(timeline: &Timeline, id: &str) -> ClipState {
        timeline.state.find_clip(id).expect("clip").1.clone()
    }

    fn selected(timeline: &Timeline) -> Vec<&str> {
        timeline
            .state
            .selection
            .selected_clip_ids
            .iter()
            .map(String::as_str)
            .collect()
    }

    fn window_x_at_beat(timeline: &Timeline, beat: f32) -> f32 {
        timeline.state.lane_origin_x() + timeline.state.beats_to_x(beat)
    }

    fn press(edge: FadeEdge, window_x: f32, additive: bool) -> AudioClipProcessUpdate {
        AudioClipProcessUpdate::FadePress {
            edge,
            window_x,
            additive,
        }
    }

    fn drag(edge: FadeEdge, window_x: f32) -> AudioClipProcessUpdate {
        AudioClipProcessUpdate::FadeDrag {
            edge,
            window_x,
            bypass_snap: true,
        }
    }

    /// A plain click on a corner fade handle selects the clip, as a click on
    /// its body does; Cmd/Ctrl adds to the selection; a click on a clip that
    /// is already selected narrows the selection to it on the release. The
    /// press changes no fade.
    #[test]
    fn a_click_on_a_fade_handle_selects_its_clip() {
        let mut timeline =
            timeline_with(vec![audio_clip("a", 0.0, 2.0), audio_clip("b", 6.0, 2.0)]);
        let a_before = clip(&timeline, "a");
        let x = timeline.fade_end_window_x("a", FadeEdge::In).unwrap();

        let effect = timeline.clip_process_update_state("a", press(FadeEdge::In, x, false));
        assert!(effect.redraw, "the selection changed at the press");
        assert_eq!(selected(&timeline), ["a"]);
        assert!(timeline.clip_fades.fade.is_some(), "a drag would adjust it");
        // The release of a click: no fade changed, nothing to record.
        timeline.apply_pending_clip_click();
        assert!(clip_edit_is_noop(&a_before, &clip(&timeline, "a")));
        timeline.reset_input_state();

        let x = timeline.fade_end_window_x("b", FadeEdge::Out).unwrap();
        timeline.clip_process_update_state("b", press(FadeEdge::Out, x, true));
        assert_eq!(selected(&timeline), ["a", "b"]);
        timeline.apply_pending_clip_click();
        timeline.reset_input_state();

        // A click on a clip already selected narrows the selection on release.
        let x = timeline.fade_end_window_x("a", FadeEdge::In).unwrap();
        let effect = timeline.clip_process_update_state("a", press(FadeEdge::In, x, false));
        assert!(!effect.redraw && effect.handles);
        assert_eq!(selected(&timeline), ["a", "b"], "held until the release");
        timeline.apply_pending_clip_click();
        assert_eq!(selected(&timeline), ["a"]);
    }

    /// A press that becomes a drag adjusts the fade and leaves the selection
    /// as the press made it.
    #[test]
    fn a_drag_from_a_fade_handle_adjusts_the_fade_not_the_selection() {
        let mut timeline =
            timeline_with(vec![audio_clip("a", 0.0, 2.0), audio_clip("b", 6.0, 2.0)]);
        timeline.state.selection.selected_clip_ids = vec!["a".into(), "b".into()];
        let x = timeline.fade_end_window_x("a", FadeEdge::In).unwrap();
        timeline.clip_process_update_state("a", press(FadeEdge::In, x, false));
        // Half a second into the clip: beat 1.
        let effect = timeline
            .clip_process_update_state("a", drag(FadeEdge::In, window_x_at_beat(&timeline, 1.0)));
        assert!(effect.redraw);
        let fade_in = clip(&timeline, "a").stretch.fade_in_ms;
        assert!((fade_in - 500.0).abs() < 1.0, "fade-in is {fade_in} ms");
        timeline.apply_pending_clip_click();
        assert_eq!(selected(&timeline), ["a", "b"], "a drag is not a click");
    }

    /// A double-click resets the fade and still selects the clip.
    #[test]
    fn a_double_click_on_a_fade_handle_resets_it_and_selects_the_clip() {
        let mut faded = audio_clip("a", 0.0, 2.0);
        faded.stretch.fade_out_ms = 400.0;
        let mut timeline = timeline_with(vec![faded, audio_clip("b", 6.0, 2.0)]);
        timeline.state.selection.selected_clip_ids = vec!["b".into()];
        let effect = timeline.clip_process_update_state(
            "a",
            AudioClipProcessUpdate::FadeReset {
                edge: FadeEdge::Out,
                additive: false,
            },
        );
        assert!(effect.redraw);
        assert_eq!(clip(&timeline, "a").stretch.fade_out_ms, 0.0);
        assert_eq!(selected(&timeline), ["a"]);
    }

    /// Escape mid fade drag: the clip goes back exactly, nothing is left to
    /// record, and the drag's later moves are refused until the next press.
    #[test]
    fn a_cancelled_fade_drag_goes_back_and_refuses_the_rest_of_its_drag() {
        let mut timeline = timeline_with(vec![audio_clip("a", 0.0, 2.0)]);
        let before = clip(&timeline, "a");
        let x = timeline.fade_end_window_x("a", FadeEdge::In).unwrap();
        timeline.clip_process_update_state("a", press(FadeEdge::In, x, false));
        timeline
            .clip_process_update_state("a", drag(FadeEdge::In, window_x_at_beat(&timeline, 1.0)));
        assert_ne!(clip(&timeline, "a"), before);

        assert!(timeline.cancel_clip_handle_state());
        assert_eq!(clip(&timeline, "a"), before);
        assert!(!timeline.clip_handle_gesture_active(), "nothing to commit");
        assert!(!timeline.clip_handle_drag_accepted());
        let effect = timeline
            .clip_process_update_state("a", drag(FadeEdge::In, window_x_at_beat(&timeline, 2.0)));
        assert_eq!(effect, ClipProcessEffect::default());
        assert_eq!(clip(&timeline, "a"), before);
        // The mouse-up reset does not lift the refusal; a new press does.
        timeline.reset_input_state();
        assert!(!timeline.clip_handle_drag_accepted());
        timeline.clip_process_update_state("a", press(FadeEdge::In, x, false));
        assert!(timeline.clip_handle_drag_accepted());
        // With nothing in flight a cancel does nothing.
        timeline.clip_fades.fade = None;
        timeline.clip_process_origin = None;
        assert!(!timeline.cancel_clip_handle_state());
        assert!(timeline.clip_handle_drag_accepted());
    }

    /// Two clips overlapping by 0.2 s, each with audio to spare.
    fn crossfaded_pair() -> Timeline {
        let timeline = timeline_with(vec![audio_clip("l", 0.0, 3.0), audio_clip("r", 6.0, 3.0)]);
        let (left, right) = (clip(&timeline, "l"), clip(&timeline, "r"));
        let center = timeline.state.crossfade_center_seconds(&left, &right);
        let plan = timeline
            .state
            .plan_crossfade(&left, &right, center, 0.2)
            .expect("both have audio beyond the cut");
        let mut timeline = timeline;
        timeline.state.replace_clip_in_place(&plan.left);
        timeline.state.replace_clip_in_place(&plan.right);
        timeline
    }

    fn crossfade_press(timeline: &Timeline) -> CrossfadeGesture {
        CrossfadeGesture::Press {
            left_id: "l".into(),
            right_id: "r".into(),
            window_x: window_x_at_beat(timeline, 6.0),
        }
    }

    fn crossfade_drag(timeline: &Timeline, beat: f32) -> CrossfadeGesture {
        CrossfadeGesture::Drag {
            left_id: "l".into(),
            right_id: "r".into(),
            window_x: window_x_at_beat(timeline, beat),
        }
    }

    /// Escape mid crossfade drag puts *both* clips back and refuses the rest
    /// of the drag.
    #[test]
    fn a_cancelled_crossfade_drag_puts_both_clips_back() {
        let mut timeline = crossfaded_pair();
        let before = timeline.state.tracks[0].clips.clone();
        timeline.crossfade_drag_state(&crossfade_press(&timeline));
        assert!(timeline.crossfade_drag_state(&crossfade_drag(&timeline, 6.2)));
        assert_ne!(clip(&timeline, "l"), before[0]);
        assert_ne!(clip(&timeline, "r"), before[1]);

        assert!(timeline.cancel_clip_handle_state());
        assert_eq!(timeline.state.tracks[0].clips, before);
        assert!(timeline.clip_fades.crossfade.is_none(), "nothing to commit");
        assert!(!timeline.crossfade_drag_state(&crossfade_drag(&timeline, 6.4)));
        assert_eq!(timeline.state.tracks[0].clips, before);
        // A new press starts a new drag.
        timeline.crossfade_drag_state(&crossfade_press(&timeline));
        assert!(timeline.crossfade_drag_state(&crossfade_drag(&timeline, 6.2)));
    }

    /// A pair the planner cannot re-trim starts no drag: its handle is drawn
    /// disabled, and a press that reaches it anyway moves nothing.
    #[test]
    fn a_crossfade_the_planner_refuses_starts_no_drag() {
        let mut timeline = crossfaded_pair();
        let mut reversed = clip(&timeline, "r");
        reversed.stretch.reverse = true;
        timeline.state.replace_clip_in_place(&reversed);
        let before = timeline.state.tracks[0].clips.clone();
        timeline.crossfade_drag_state(&crossfade_press(&timeline));
        assert!(timeline.clip_fades.crossfade.is_none());
        assert!(!timeline.crossfade_drag_state(&crossfade_drag(&timeline, 6.4)));
        assert_eq!(timeline.state.tracks[0].clips, before);
    }

    /// Two clips that are their whole files and overlap by 0.7 ms: nothing
    /// blocks a trim, but no crossfade of the 1 ms minimum fits, so every
    /// plan is refused. The press starts no drag, as on a blocked pair.
    #[test]
    fn a_crossfade_with_no_room_starts_no_drag() {
        let whole_file = |id: &str, start_beat: f32, seconds: f64| {
            let mut clip = audio_clip(id, start_beat, seconds);
            let frames = (seconds * 48_000.0).round() as u64;
            clip.source_duration_seconds = Some(seconds);
            clip.stretch.original_duration_samples = frames;
            clip.stretch.source_start_samples = 0;
            clip.stretch.source_end_samples = frames;
            clip
        };
        // 0.0014 beats is 0.7 ms at 120 BPM.
        let mut timeline = timeline_with(vec![
            whole_file("l", 0.0, 3.0),
            whole_file("r", 6.0 - 0.0014, 3.0),
        ]);
        assert_eq!(
            timeline
                .state
                .audio_crossfades(&timeline.state.tracks[0])
                .crossfades
                .len(),
            1
        );
        let (left, right) = (clip(&timeline, "l"), clip(&timeline, "r"));
        assert!(!timeline.state.can_adjust_crossfade(&left, &right));
        let before = timeline.state.tracks[0].clips.clone();
        timeline.crossfade_drag_state(&crossfade_press(&timeline));
        assert!(timeline.clip_fades.crossfade.is_none());
        assert!(!timeline.crossfade_drag_state(&crossfade_drag(&timeline, 6.4)));
        assert_eq!(timeline.state.tracks[0].clips, before);
    }
}

//! Point editing on automation lanes beyond drawing and dragging: the curve a
//! segment takes, and copy / cut / paste of points.
//!
//! Each acts on the lane that holds the selection — the right-clicked lane for
//! a lane menu command, else the selected track's lane with selected points,
//! then the master's, or the selected track's active lane for a paste — and is
//! one undo step (`EditCommand::SetTrackAutomationLanes`, like every other lane
//! edit).

use gpui::Context;

use crate::components::timeline::timeline_state::{
    AutomationClipboard, AutomationCurveShape, MASTER_TRACK_ID,
};

use super::StudioLayout;

/// What the last Copy or Cut took, so Paste knows which clipboard to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum LastCopied {
    #[default]
    Clips,
    Automation,
}

/// The automation clipboard and which clipboard Paste reads.
#[derive(Debug, Clone, Default)]
pub(crate) struct AutomationClipboardState {
    pub points: Option<AutomationClipboard>,
    pub last: LastCopied,
    /// `(track, lane)` whose context menu is open: what its commands act on.
    pub menu_lane: Option<(String, String)>,
}

impl StudioLayout {
    /// `(track, lane)` holding the selected points, or — with `for_paste` — the
    /// selected track's active lane when none are selected.
    fn automation_edit_lane(
        &self,
        for_paste: bool,
        cx: &Context<Self>,
    ) -> Option<(String, String)> {
        let state = &self.timeline.read(cx).state;
        let with_selection = |owner: &str| {
            state
                .automation_lanes_of(owner)?
                .iter()
                .find(|lane| lane.points.iter().any(|p| p.selected))
                .map(|lane| (owner.to_string(), lane.id.clone()))
        };
        let track_id = state.selection.selected_track_id.clone();
        if let Some(found) = track_id.as_deref().and_then(with_selection) {
            return Some(found);
        }
        // The master is never the selected track, so its selection is looked
        // for on its own.
        if let Some(found) = with_selection(MASTER_TRACK_ID) {
            return Some(found);
        }
        if !for_paste {
            return None;
        }
        let track_id = track_id?;
        let lane_id = state.active_automation_lane_id(&track_id)?;
        Some((track_id, lane_id))
    }

    /// The lane whose context menu issued the command being run, once: the
    /// menu is gone after it, and the keyboard goes back to the selection.
    pub(super) fn take_menu_automation_lane(
        &mut self,
        cx: &Context<Self>,
    ) -> Option<(String, String)> {
        let (track_id, lane_id) = self.automation_clipboard.menu_lane.take()?;
        self.timeline
            .read(cx)
            .state
            .automation_lane(&track_id, &lane_id)
            .is_some()
            .then_some((track_id, lane_id))
    }

    /// One recorded lane edit on `track_id`, then the engine resync.
    fn edit_automation_lanes(
        &mut self,
        track_id: &str,
        cx: &mut Context<Self>,
        edit: impl FnOnce(&mut crate::components::timeline::timeline_state::TimelineState) -> usize,
    ) -> usize {
        let changed = self.timeline.update(cx, |timeline, cx| {
            let before = timeline.state.capture_automation_lanes(track_id);
            let changed = edit(&mut timeline.state);
            if changed > 0 {
                timeline.record_automation_lanes_edit(track_id, before, cx);
                cx.notify();
            }
            changed
        });
        if changed > 0 {
            self.mark_dirty();
            self.audio_bridge.project_dirty = true;
            self.schedule_audio_project_sync(cx, false, "automation_point_edit");
        }
        changed
    }

    /// Give the selected points a Curve-menu shape (curve kind and tension).
    /// One undo step. Returns whether any changed.
    pub(super) fn set_selected_automation_shape(
        &mut self,
        lane: Option<(String, String)>,
        shape: AutomationCurveShape,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some((track_id, lane_id)) = lane.or_else(|| self.automation_edit_lane(false, cx))
        else {
            return false;
        };
        self.edit_automation_lanes(&track_id, cx, |state| {
            state.set_selected_automation_shape(&track_id, &lane_id, shape)
        }) > 0
    }

    /// Copy (or cut) the selected automation points. Returns whether there were
    /// any, so a Copy with none falls through to the clip clipboard.
    pub(super) fn copy_automation_points(
        &mut self,
        lane: Option<(String, String)>,
        cut: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some((track_id, lane_id)) = lane.or_else(|| self.automation_edit_lane(false, cx))
        else {
            return false;
        };
        let Some(copied) = self
            .timeline
            .read(cx)
            .state
            .copy_selected_automation(&track_id, &lane_id)
        else {
            return false;
        };
        self.automation_clipboard.points = Some(copied);
        self.automation_clipboard.last = LastCopied::Automation;
        if cut {
            // Only the lane that was copied: a selection in another lane is
            // not on the clipboard, so deleting it would lose it for good.
            self.edit_automation_lanes(&track_id, cx, |state| {
                state.delete_selected_automation_points_in_lane(&track_id, &lane_id)
            });
        }
        true
    }

    /// Paste the copied points at `at_beat` (the playhead when `None`) into the
    /// lane being edited. Returns whether automation was pasted; `false` leaves
    /// Paste to the clip clipboard.
    pub(super) fn paste_automation_points(
        &mut self,
        lane: Option<(String, String)>,
        at_beat: Option<f32>,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.automation_clipboard.last != LastCopied::Automation {
            return false;
        }
        let Some(copied) = self.automation_clipboard.points.clone() else {
            return false;
        };
        let Some((track_id, lane_id)) = lane.or_else(|| self.automation_edit_lane(true, cx)) else {
            return false;
        };
        let beat = at_beat.unwrap_or_else(|| self.timeline.read(cx).state.transport.playhead_beats);
        self.edit_automation_lanes(&track_id, cx, |state| {
            state.paste_automation(&track_id, &lane_id, beat, &copied)
        }) > 0
    }

    /// Note that clips were copied last, so Paste pastes them.
    pub(super) fn note_clips_copied(&mut self) {
        self.automation_clipboard.last = LastCopied::Clips;
    }

    /// Whether Paste would paste automation points here.
    pub(super) fn can_paste_automation(&self) -> bool {
        self.automation_clipboard.last == LastCopied::Automation
            && self.automation_clipboard.points.is_some()
    }
}

impl StudioLayout {
    /// Automation Arm on or off. Disarming mid-run ends the run's recording
    /// the way stopping does: open lanes close and the run is one undo step.
    pub(super) fn toggle_automation_arm(&mut self, cx: &mut Context<Self>) {
        let armed = self.timeline.update(cx, |timeline, cx| {
            timeline.state.automation_arm = !timeline.state.automation_arm;
            cx.notify();
            timeline.state.automation_arm
        });
        if !armed {
            self.finish_automation_recording(cx);
        }
        cx.notify();
    }

    /// Re-Enable Automation: every lane overridden by hand plays again.
    pub(super) fn reenable_automation(&mut self, cx: &mut Context<Self>) {
        let changed = self.timeline.update(cx, |timeline, cx| {
            let changed = timeline.state.reenable_automation();
            if changed {
                cx.notify();
            }
            changed
        });
        if changed {
            self.sync_automation_overrides(cx);
            self.push_mixer_snapshot_to_window(cx);
        }
        cx.notify();
    }

    /// Send the engine the lanes again when some were overridden or put back
    /// in play since it last heard. The engine applies a lane change live
    /// (`SetTrackAutomation`), so this costs no graph rebuild.
    pub(super) fn sync_automation_overrides(&mut self, cx: &mut Context<Self>) {
        let changed = self.timeline.update(cx, |timeline, _| {
            timeline.state.take_automation_override_change()
        });
        if changed {
            self.audio_bridge.project_dirty = true;
            self.schedule_audio_project_sync(cx, false, "automation_override");
            cx.notify();
        }
    }

    /// One playback tick of armed automation recording at `beat`.
    pub(super) fn tick_automation_recording(&mut self, beat: f32, cx: &mut Context<Self>) {
        let now = self.automation_record_clock.elapsed().as_secs_f64();
        let mut recorder = std::mem::take(&mut self.automation_recorder);
        let tick = self.timeline.update(cx, |timeline, cx| {
            let tick = timeline
                .state
                .automation_record_tick(&mut recorder, beat, now);
            if tick.wrote {
                cx.notify();
            }
            tick
        });
        self.automation_recorder = recorder;
        // A lane started or stopped being written: the engine stops or starts
        // playing it, so the control being moved is what is heard.
        if tick.engine_changed {
            self.audio_bridge.project_dirty = true;
            self.schedule_audio_project_sync(cx, false, "automation_record");
        }
    }

    /// End the run's recording: close the writing lanes and record each
    /// written track's lanes as one undo step.
    pub(super) fn finish_automation_recording(&mut self, cx: &mut Context<Self>) {
        if self.automation_recorder.is_idle() {
            return;
        }
        let mut recorder = std::mem::take(&mut self.automation_recorder);
        let (written, engine_changed) = self.timeline.update(cx, |timeline, cx| {
            let beat = timeline.state.transport.playhead_beats;
            let (written, engine_changed) = recorder.finish(&mut timeline.state, beat);
            for (track_id, before) in &written {
                timeline.record_automation_lanes_edit(track_id, before.clone(), cx);
            }
            cx.notify();
            (written, engine_changed)
        });
        if !written.is_empty() {
            self.mark_dirty();
        }
        if engine_changed || !written.is_empty() {
            self.audio_bridge.project_dirty = true;
            self.schedule_audio_project_sync(cx, false, "automation_record_finish");
        }
    }
}

impl StudioLayout {
    /// The master's automation in a menu: show or hide its block in the
    /// arrangement, then a lane for each control it can automate.
    pub(super) fn master_automation_menu_entries(
        &self,
        cx: &Context<Self>,
    ) -> Vec<crate::components::context_menu::ContextMenuEntry> {
        use crate::components::context_menu::ContextMenuEntry;
        use crate::components::timeline::timeline_state::automation_target_menu_command;
        let state = &self.timeline.read(cx).state;
        let mut entries = vec![
            ContextMenuEntry::Separator,
            ContextMenuEntry::Header("Master Automation".into()),
            ContextMenuEntry::checked_item(
                "Show Master Automation",
                "master:automation-toggle",
                state.master_automation_shown(),
            ),
        ];
        for target in state.master_automation_targets() {
            let shown = state.master_automation_shown()
                && state
                    .master
                    .automation_lanes
                    .iter()
                    .any(|lane| lane.target == target && lane.visible);
            let label = state.automation_target_label(MASTER_TRACK_ID, &target);
            let command = automation_target_menu_command(MASTER_TRACK_ID, &target);
            entries.push(if shown {
                ContextMenuEntry::disabled_item(label, command)
            } else {
                ContextMenuEntry::item(label, command)
            });
        }
        entries
    }

    /// Show or hide the master's automation block. Saved with the project, but
    /// the engine never sees it.
    pub(super) fn toggle_master_automation(&mut self, cx: &mut Context<Self>) {
        use crate::components::timeline::timeline_state::AutomationViewChange;
        let change = self.timeline.update(cx, |timeline, cx| {
            let lanes_before = timeline.state.capture_automation_lanes(MASTER_TRACK_ID);
            let change = timeline
                .state
                .edit_automation_view(MASTER_TRACK_ID, |state| {
                    state.toggle_track_lane_mode(MASTER_TRACK_ID);
                });
            if change == AutomationViewChange::Lanes {
                // Opening it the first time makes its Volume lane: one undo step.
                timeline.record_automation_lanes_edit(MASTER_TRACK_ID, lanes_before, cx);
            }
            cx.notify();
            change
        });
        self.mark_automation_view_change(change);
        if change == AutomationViewChange::Lanes {
            self.audio_bridge.project_dirty = true;
            self.schedule_audio_project_sync(cx, false, "master_automation_toggle");
        }
        cx.notify();
    }
}

//! Dual pan on a channel: switching between Balance and Dual, and moving a
//! Dual position. Each is one `EditCommand::SetTrackDualPan`, carrying the
//! whole setting, so undo restores the mode and both positions together.
//!
//! These change only `TimelineState`; the caller pushes the result down the
//! realtime control path (`panMode` / `panLeft` / `panRight`), as it does for
//! the balance knob.

use super::*;
use crate::components::timeline::timeline_state::{DualPan, DualPanSide, PanMode};

impl Timeline {
    /// Switch a channel between Balance and Dual pan as one undo step.
    /// Returns the new setting, or `None` when nothing changed: the track is
    /// gone, already in `mode`, or is mono (Dual needs two inputs).
    pub(crate) fn set_track_pan_mode(
        &mut self,
        track_id: &str,
        mode: PanMode,
        cx: &mut gpui::Context<Self>,
    ) -> Option<DualPan> {
        let track = self.state.find_track(track_id)?;
        if mode == PanMode::Dual && !track.supports_dual_pan() {
            return None;
        }
        let prev = track.dual_pan;
        if prev.mode == mode {
            return None;
        }
        let next = DualPan { mode, ..prev };
        self.run_edit_command(
            EditCommand::SetTrackDualPan {
                track_id: track_id.to_string(),
                prev,
                next,
            },
            cx,
        );
        Some(next)
    }

    /// One value from a Dual position knob, which reports every pointer
    /// sample and no release: applied at once, consecutive samples on the same
    /// channel folding into one undo step. Returns the new setting when it
    /// moved. Dual positions move only the channel they belong to — a gang
    /// follows the balance knob, not these.
    pub(crate) fn nudge_dual_pan(
        &mut self,
        track_id: &str,
        side: DualPanSide,
        value: f32,
        cx: &mut gpui::Context<Self>,
    ) -> Option<DualPan> {
        let prev = self.state.find_track(track_id)?.dual_pan;
        let mut next = prev;
        match side {
            DualPanSide::Left => next.left = value,
            DualPanSide::Right => next.right = value,
        }
        let next = next.sanitized();
        if !self.state.set_track_dual_pan(track_id, next) {
            return None;
        }
        self.record_mixer_value(
            EditCommand::SetTrackDualPan {
                track_id: track_id.to_string(),
                prev,
                next,
            },
            cx,
        );
        cx.notify();
        Some(next)
    }
}

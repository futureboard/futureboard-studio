//! Typing an automation point's value.
//!
//! A double-click on a point, a click on a selected point's value tag, or the
//! lane menu's Edit Value opens a small field in place of the tag, holding the
//! value in the parameter's own unit (`-6.0` dB, `L32`, `75` %, `On`) with the
//! number selected. Enter commits, Escape cancels, a press anywhere else
//! commits. A commit is one undo step on the track's lanes; text that is not a
//! value, or the value the point already has, records nothing.
//!
//! Keys reach the field the way they reach the transport's inline editors: the
//! studio's key handler routes them here first while the field is open, so no
//! focus grab is needed and no arrangement shortcut sees what is typed.

use super::*;
use crate::components::text_input::{TextInputAction, TextInputState};
use gpui::KeyDownEvent;

/// Height of the value field, the same as the value tag it replaces.
pub(crate) const AUTOMATION_VALUE_FIELD_HEIGHT: f32 = 18.0;

/// An open value field on one automation point.
pub(crate) struct AutomationValueEdit {
    pub(crate) track_id: String,
    pub(crate) lane_id: String,
    pub(crate) point_id: u64,
    pub(crate) input: TextInputState,
}

impl Timeline {
    /// The open value field, if any, for the lane renderer.
    pub(crate) fn automation_value_edit(&self) -> Option<&AutomationValueEdit> {
        self.automation_value_edit.as_ref()
    }

    pub(crate) fn is_editing_automation_value(&self) -> bool {
        self.automation_value_edit.is_some()
    }

    /// Open the value field on one point, closing (committing) any other one
    /// first. Selects the point alone. No-op when the point is gone.
    pub(crate) fn open_automation_value_edit(
        &mut self,
        track_id: &str,
        lane_id: &str,
        point_id: u64,
        cx: &mut Context<Self>,
    ) {
        self.commit_automation_value_edit(cx);
        let Some(text) = self
            .state
            .automation_lane(track_id, lane_id)
            .and_then(|lane| {
                let point = lane.points.iter().find(|p| p.id == point_id)?;
                Some(lane.target.edit_text(point.value))
            })
        else {
            return;
        };
        self.state
            .select_automation_point(track_id, lane_id, point_id, false);
        let mut input = TextInputState::new("automation-point-value", cx.focus_handle())
            .with_accessible_label("Automation point value")
            .with_field_height(AUTOMATION_VALUE_FIELD_HEIGHT);
        input.set_value(text);
        input.select_all();
        self.automation_value_edit = Some(AutomationValueEdit {
            track_id: track_id.to_string(),
            lane_id: lane_id.to_string(),
            point_id,
            input,
        });
        cx.notify();
    }

    /// Open the field on the lane's single selected point (the lane menu's
    /// Edit Value). Returns whether it opened.
    pub(crate) fn open_selected_automation_value_edit(
        &mut self,
        track_id: &str,
        lane_id: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let selected = self.state.selected_automation_point_ids(track_id, lane_id);
        let [point_id] = selected.as_slice() else {
            return false;
        };
        self.open_automation_value_edit(track_id, lane_id, *point_id, cx);
        self.is_editing_automation_value()
    }

    /// Close the field and apply what it holds. One undo step when the value
    /// changed; nothing when the text is not a value or the point is gone.
    pub(crate) fn commit_automation_value_edit(&mut self, cx: &mut Context<Self>) {
        let Some(edit) = self.automation_value_edit.take() else {
            return;
        };
        let parsed = self
            .state
            .automation_lane(&edit.track_id, &edit.lane_id)
            .and_then(|lane| lane.target.parse_value(&edit.input.value));
        if let Some(value) = parsed {
            let before = self.state.capture_automation_lanes(&edit.track_id);
            if self.state.set_automation_point_value(
                &edit.track_id,
                &edit.lane_id,
                edit.point_id,
                value,
            ) {
                self.record_automation_lanes_edit(&edit.track_id, before, cx);
            }
        }
        cx.notify();
    }

    /// Close the field without applying it.
    pub(crate) fn cancel_automation_value_edit(&mut self, cx: &mut Context<Self>) {
        if self.automation_value_edit.take().is_some() {
            cx.notify();
        }
    }

    /// Route one key to the open field. Enter commits, Escape cancels, the
    /// rest edits the text. Returns `false` (key not taken) when no field is
    /// open.
    pub(crate) fn handle_automation_value_key(
        &mut self,
        event: &KeyDownEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(edit) = self.automation_value_edit.as_mut() else {
            return false;
        };
        match edit.input.handle_key_with_clipboard(event, Some(cx)) {
            TextInputAction::Submit => self.commit_automation_value_edit(cx),
            TextInputAction::Cancel => self.cancel_automation_value_edit(cx),
            TextInputAction::Consumed | TextInputAction::Pass => cx.notify(),
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use crate::components::timeline::timeline_state::{volume, AutomationTarget};

    fn parse(target: &AutomationTarget, text: &str) -> Option<f32> {
        target.parse_value(text)
    }

    #[test]
    fn volume_reads_db_with_or_without_the_unit() {
        let t = AutomationTarget::TrackVolume;
        let minus6 = volume::db_to_norm(-6.0);
        for text in ["-6", "-6 dB", " -6.0db ", "\u{2212}6"] {
            assert!((parse(&t, text).unwrap() - minus6).abs() < 1e-6, "{text}");
        }
        assert_eq!(parse(&t, "+3"), Some(volume::db_to_norm(3.0)));
        assert_eq!(parse(&t, "-inf"), Some(0.0));
        assert_eq!(parse(&t, "-∞"), Some(0.0));
        // Out of range clamps to the fader's ends.
        assert_eq!(parse(&t, "40"), Some(1.0));
        assert_eq!(parse(&t, "loud"), None);
        assert_eq!(parse(&t, ""), None);
    }

    #[test]
    fn pan_reads_sides_signed_numbers_and_centre() {
        let t = AutomationTarget::TrackPan;
        assert_eq!(parse(&t, "C"), Some(0.5));
        assert_eq!(parse(&t, "0"), Some(0.5));
        assert!((parse(&t, "L50").unwrap() - 0.25).abs() < 1e-6);
        assert!((parse(&t, "50L").unwrap() - 0.25).abs() < 1e-6);
        assert!((parse(&t, "r100").unwrap() - 1.0).abs() < 1e-6);
        assert!((parse(&t, "-50").unwrap() - 0.25).abs() < 1e-6);
        assert!((parse(&t, "L250").unwrap() - 0.0).abs() < 1e-6);
        assert_eq!(parse(&t, "left"), None);
    }

    #[test]
    fn switches_and_percentages() {
        assert_eq!(parse(&AutomationTarget::TrackMute, "Muted"), Some(1.0));
        assert_eq!(parse(&AutomationTarget::TrackMute, "off"), Some(0.0));
        let bypass = AutomationTarget::InsertBypass {
            insert_id: "i".into(),
        };
        assert_eq!(parse(&bypass, "active"), Some(0.0));
        assert_eq!(parse(&bypass, "1"), Some(1.0));
        let param = AutomationTarget::PluginParameter {
            insert_id: "i".into(),
            parameter_id: "p".into(),
            parameter_name: "Cutoff".into(),
        };
        assert!((parse(&param, "75").unwrap() - 0.75).abs() < 1e-6);
        assert!((parse(&param, "12.5 %").unwrap() - 0.125).abs() < 1e-6);
        assert_eq!(parse(&param, "150"), Some(1.0));
    }

    /// What the field opens with reads back as the same value, so opening and
    /// pressing Enter never moves a point.
    #[test]
    fn the_opening_text_reads_back_as_the_same_value() {
        let param = AutomationTarget::PluginParameter {
            insert_id: "i".into(),
            parameter_id: "p".into(),
            parameter_name: "Cutoff".into(),
        };
        for target in [
            AutomationTarget::TrackVolume,
            AutomationTarget::TrackPan,
            AutomationTarget::TrackMute,
            param,
        ] {
            for norm in [0.0f32, 0.25, 0.5, 0.75, 1.0] {
                let back = target.parse_value(&target.edit_text(norm)).unwrap();
                let tolerance = match target {
                    // Shown to 0.1 dB over a 66 dB range.
                    AutomationTarget::TrackVolume => 0.1 / 66.0 + 1e-6,
                    // Shown to whole percent of full width.
                    AutomationTarget::TrackPan => 0.005 + 1e-6,
                    _ => 1e-3,
                };
                let expect = if matches!(target, AutomationTarget::TrackMute) {
                    if norm >= 0.5 {
                        1.0
                    } else {
                        0.0
                    }
                } else if matches!(target, AutomationTarget::TrackVolume) && norm <= 0.001 {
                    0.0
                } else {
                    norm
                };
                assert!(
                    (back - expect).abs() <= tolerance,
                    "{target:?} {norm} -> {:?} -> {back}",
                    target.edit_text(norm)
                );
            }
        }
    }
}

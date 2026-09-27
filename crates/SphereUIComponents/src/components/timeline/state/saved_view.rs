//! Writers of the arrangement view that the project file saves (the v54 view
//! section, `project::view::ProjectViewState`): the snap switch and grid, and
//! which tracks show their automation and on which lane.
//!
//! Each writer says whether it changed what the file would save, so its owner
//! can mark the session "Unsaved changes" through the view-only path. That path
//! gets a close prompt and autosave, both of which only see a dirty session.
//! It never marks a click that changed nothing, and never rebuilds the engine
//! graph, which none of this reaches. None of it is in the undo history either.
//!
//! The conductor lanes' show and hide helpers (`show_tempo_track_lane` and the
//! rest) report changes the same way, from their own modules.

use super::*;

impl TimelineState {
    /// The ruler magnet and `timeline:toggle-snap`. Always a change.
    pub fn toggle_snap_to_grid(&mut self) {
        self.snap_to_grid = !self.snap_to_grid;
    }

    /// The ruler's grid menu. Choosing a grid is asking to snap to it, so it
    /// also turns the magnet on (and `Off` turns it off), as the piano roll's
    /// grid menu does. Returns whether the saved snap changed.
    pub fn choose_grid_division(&mut self, division: SnapDivision) -> bool {
        let snap = division != SnapDivision::Off;
        let changed = self.grid_division != division || self.snap_to_grid != snap;
        self.grid_division = division;
        self.snap_to_grid = snap;
        changed
    }

    /// The grid menu's straight, dotted or triplet choice. Returns whether the
    /// saved shape changed.
    pub fn choose_snap_shape(&mut self, shape: SnapShape) -> bool {
        std::mem::replace(&mut self.snap_shape, shape) != shape
    }

    /// `track_id`'s automation view as the project file sees it. Compare two
    /// with [`TrackAutomationView::change_to`], or use
    /// [`Self::edit_automation_view`].
    pub fn track_automation_view(&self, track_id: &str) -> TrackAutomationView {
        let Some(track) = self.find_track(track_id) else {
            return TrackAutomationView::default();
        };
        TrackAutomationView {
            focus: (track.lane_mode == TrackLaneMode::Automation)
                .then(|| self.active_automation_target(track_id)),
            lane_count: track.automation_lanes.len(),
        }
    }

    /// Runs `edit` and says what it changed of `track_id`'s automation view.
    pub fn edit_automation_view(
        &mut self,
        track_id: &str,
        edit: impl FnOnce(&mut Self),
    ) -> AutomationViewChange {
        let before = self.track_automation_view(track_id);
        edit(self);
        before.change_to(&self.track_automation_view(track_id))
    }
}

/// One track's automation view, reduced to what decides how a change to it is
/// marked. See [`TimelineState::track_automation_view`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TrackAutomationView {
    /// `Some` while the track's lane shows automation, holding the lane its
    /// editor is on: the selected lane, else the first, which is also how a
    /// reopened project resolves it. `None` in clip mode, whose focus the file
    /// does not keep.
    focus: Option<AutomationTarget>,
    lane_count: usize,
}

impl TrackAutomationView {
    pub fn change_to(&self, after: &Self) -> AutomationViewChange {
        if self.lane_count != after.lane_count {
            AutomationViewChange::Lanes
        } else if self.focus != after.focus {
            AutomationViewChange::View
        } else {
            AutomationViewChange::Unchanged
        }
    }
}

/// What an automation view command changed of a track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutomationViewChange {
    /// Nothing the project file saves.
    Unchanged,
    /// The saved view only: the expansion or the focused lane. The owner marks
    /// the session view-only dirty.
    View,
    /// An automation lane was added. The lane is project content that the
    /// engine snapshot carries, so a command whose job is to add one keeps its
    /// full project path. A mode toggle that seeds an empty lane only needs
    /// the session saved, since an empty lane plays nothing.
    Lanes,
}

impl AutomationViewChange {
    pub fn is_change(self) -> bool {
        self != Self::Unchanged
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::view::ProjectViewState;

    /// What the project file would save of this timeline's view.
    fn saved(state: &TimelineState) -> ProjectViewState {
        ProjectViewState::capture(state)
    }

    #[test]
    fn a_grid_choice_reports_only_a_real_change() {
        let mut state = TimelineState::default();
        state.snap_to_grid = true;
        state.grid_division = SnapDivision::Div1_16;

        let before = saved(&state);
        assert!(!state.choose_grid_division(SnapDivision::Div1_16));
        assert_eq!(saved(&state), before);

        assert!(state.choose_grid_division(SnapDivision::Div1_8));
        assert_eq!(state.grid_division, SnapDivision::Div1_8);
        assert!(state.snap_to_grid);
        assert_ne!(saved(&state), before);

        // Same grid with the magnet off: choosing it turns the magnet on.
        state.toggle_snap_to_grid();
        assert!(!state.snap_to_grid);
        assert!(state.choose_grid_division(SnapDivision::Div1_8));
        assert!(state.snap_to_grid);

        assert!(state.choose_grid_division(SnapDivision::Off));
        assert!(!state.snap_to_grid);
        let off = saved(&state);
        assert!(!state.choose_grid_division(SnapDivision::Off));
        assert_eq!(saved(&state), off);
    }

    #[test]
    fn a_shape_choice_reports_only_a_real_change() {
        let mut state = TimelineState::default();
        state.snap_shape = SnapShape::Straight;
        let before = saved(&state);
        assert!(!state.choose_snap_shape(SnapShape::Straight));
        assert_eq!(saved(&state), before);
        assert!(state.choose_snap_shape(SnapShape::Triplet));
        assert_eq!(state.snap_shape, SnapShape::Triplet);
        assert_ne!(saved(&state), before);
        assert!(!state.choose_snap_shape(SnapShape::Triplet));
    }

    #[test]
    fn the_magnet_always_changes_the_saved_snap() {
        let mut state = TimelineState::default();
        let before = saved(&state);
        state.toggle_snap_to_grid();
        assert_ne!(saved(&state), before);
        state.toggle_snap_to_grid();
        assert_eq!(saved(&state), before);
    }

    /// Every conductor lane's show and hide helper reports a change exactly
    /// when the saved lane visibility changed.
    #[test]
    fn conductor_lanes_report_only_a_real_visibility_change() {
        type Toggle = fn(&mut TimelineState) -> bool;
        let lanes: [(&str, Toggle, Toggle); 6] = [
            (
                "tempo",
                TimelineState::show_tempo_track_lane,
                TimelineState::hide_tempo_track_lane,
            ),
            (
                "time signature",
                TimelineState::show_time_signature_track_lane,
                TimelineState::hide_time_signature_track_lane,
            ),
            (
                "marker",
                TimelineState::show_marker_track_lane,
                TimelineState::hide_marker_track_lane,
            ),
            (
                "region",
                TimelineState::show_region_track_lane,
                TimelineState::hide_region_track_lane,
            ),
            (
                "song text",
                TimelineState::show_song_text_track_lane,
                TimelineState::hide_song_text_track_lane,
            ),
            (
                "chord",
                TimelineState::show_chord_track_lane,
                TimelineState::hide_chord_track_lane,
            ),
        ];
        let mut state = TimelineState::default();
        // Seed the tempo and meter lanes' anchors first, so that below only the
        // visibility can change.
        state.show_tempo_track_lane();
        state.show_time_signature_track_lane();
        for (name, show, hide) in lanes {
            show(&mut state);
            let shown = saved(&state);

            assert!(!show(&mut state), "{name}: showing a shown lane");
            assert_eq!(saved(&state), shown, "{name}");

            assert!(hide(&mut state), "{name}: hiding a shown lane");
            let hidden = saved(&state);
            assert_ne!(hidden, shown, "{name}");

            assert!(!hide(&mut state), "{name}: hiding a hidden lane");
            assert_eq!(saved(&state), hidden, "{name}");

            assert!(show(&mut state), "{name}: showing a hidden lane");
            assert_eq!(saved(&state), shown, "{name}");
        }
    }

    /// Showing a shown Tempo or Time Signature lane still changes the project
    /// when it has to seed the lane's first point.
    #[test]
    fn showing_a_lane_that_seeds_its_first_point_is_a_change() {
        let mut state = TimelineState::default();
        state.show_tempo_track = true;
        state.tempo_map.points.clear();
        assert!(state.show_tempo_track_lane());
        assert!(!state.tempo_map.points.is_empty());
        assert!(!state.show_tempo_track_lane());

        state.show_time_signature_track = true;
        state.time_signature_map.points.clear();
        assert!(state.show_time_signature_track_lane());
        assert!(!state.time_signature_map.points.is_empty());
        assert!(!state.show_time_signature_track_lane());
    }

    #[test]
    fn expanding_and_collapsing_automation_are_view_changes() {
        let mut state = TimelineState::default();
        let id = state.create_audio_track();

        // The first expansion also seeds the lane its editor draws.
        let expand = state.edit_automation_view(&id, |s| {
            s.toggle_track_lane_mode(&id);
        });
        assert_eq!(expand, AutomationViewChange::Lanes);
        let expanded = saved(&state);

        let collapse = state.edit_automation_view(&id, |s| {
            s.toggle_track_lane_mode(&id);
        });
        assert_eq!(collapse, AutomationViewChange::View);
        assert_ne!(saved(&state), expanded);

        let again = state.edit_automation_view(&id, |s| {
            s.toggle_track_lane_mode(&id);
        });
        assert_eq!(again, AutomationViewChange::View);
        assert_eq!(saved(&state), expanded);
    }

    #[test]
    fn focusing_another_lane_is_a_view_change_only_when_it_moves() {
        let mut state = TimelineState::default();
        let id = state.create_audio_track();
        state.toggle_track_lane_mode(&id);
        let volume_lane = state.active_automation_lane_id(&id).unwrap();

        // Adding a lane is content, whatever else it does.
        let add = state.edit_automation_view(&id, |s| {
            s.set_track_automation_target(&id, AutomationTarget::TrackPan);
        });
        assert_eq!(add, AutomationViewChange::Lanes);
        let on_pan = saved(&state);

        let focus = state.edit_automation_view(&id, |s| {
            s.activate_automation_lane(&id, &volume_lane);
        });
        assert_eq!(focus, AutomationViewChange::View);
        let on_volume = saved(&state);
        assert_ne!(on_volume, on_pan);

        let same = state.edit_automation_view(&id, |s| {
            s.activate_automation_lane(&id, &volume_lane);
        });
        assert_eq!(same, AutomationViewChange::Unchanged);
        assert_eq!(saved(&state), on_volume);

        // Cycling onto a lane that already exists only moves the focus.
        let cycle = state.edit_automation_view(&id, |s| {
            s.cycle_automation_target(&id);
        });
        assert_eq!(cycle, AutomationViewChange::View);
        assert_eq!(saved(&state), on_pan);
    }

    /// Selecting the first lane when nothing was selected leaves the editor
    /// where it was, and where a reopened project would put it.
    #[test]
    fn selecting_the_lane_already_in_focus_is_no_change() {
        let mut state = TimelineState::default();
        let id = state.create_audio_track();
        state.toggle_track_lane_mode(&id);
        let first = state.active_automation_lane_id(&id).unwrap();
        state
            .tracks
            .iter_mut()
            .find(|t| t.id == id)
            .unwrap()
            .selected_automation_target = None;

        let change = state.edit_automation_view(&id, |s| {
            s.activate_automation_lane(&id, &first);
        });
        assert_eq!(change, AutomationViewChange::Unchanged);
    }

    /// A track in clip mode keeps its lane focus for the next expansion, but
    /// the file saves no focus for it, so moving it is no change.
    #[test]
    fn the_focus_of_a_track_in_clip_mode_is_not_saved() {
        let mut state = TimelineState::default();
        let id = state.create_audio_track();
        state.toggle_track_lane_mode(&id);
        let volume_lane = state.active_automation_lane_id(&id).unwrap();
        state.set_track_automation_target(&id, AutomationTarget::TrackPan);
        state.toggle_track_lane_mode(&id);
        let before = saved(&state);

        let change = state.edit_automation_view(&id, |s| {
            s.activate_automation_lane(&id, &volume_lane);
        });
        assert_eq!(change, AutomationViewChange::Unchanged);
        assert_eq!(saved(&state), before);
    }

    #[test]
    fn a_missing_track_changes_nothing() {
        let mut state = TimelineState::default();
        let change = state.edit_automation_view("no-such-track", |s| {
            s.toggle_track_lane_mode("no-such-track");
        });
        assert_eq!(change, AutomationViewChange::Unchanged);
        assert!(!change.is_change());
    }
}

//! Folder commands: group the selected tracks into a new folder, take a track
//! out of its folder, dissolve a folder, fold it open or shut.
//!
//! The model does the moves (`timeline_state::group_tree`); this is the
//! studio's half: which tracks an action touches, so it is one undo step that
//! puts every one of them back, and the engine sync the new routing needs —
//! a folder's members play through it.

use gpui::Context;

use crate::components::context_menu::ContextMenuEntry;
use crate::components::timeline::timeline_state::{TrackEditScope, TrackType};

use super::StudioLayout;

impl StudioLayout {
    /// The track context menu's folder section for `track_id`.
    pub(super) fn folder_menu_entries(
        &self,
        track_id: &str,
        cx: &gpui::App,
    ) -> Vec<ContextMenuEntry> {
        let state = &self.timeline.read(cx).state;
        let Some(track) = state.find_track(track_id) else {
            return Vec::new();
        };
        if track.track_type == TrackType::Master {
            return Vec::new();
        }
        let mut entries = vec![
            ContextMenuEntry::Separator,
            ContextMenuEntry::Header("Folder".to_string()),
            ContextMenuEntry::item("Group Selected Tracks", "track:group-selected"),
        ];
        if track.parent_group_id.is_some() {
            entries.push(ContextMenuEntry::item(
                "Remove from Folder",
                "track:remove-from-folder",
            ));
        }
        if track.track_type == TrackType::Group {
            entries.push(ContextMenuEntry::item(
                if track.group_collapsed {
                    "Expand Folder"
                } else {
                    "Collapse Folder"
                },
                "track:toggle-folder",
            ));
            entries.push(ContextMenuEntry::item("Ungroup Folder", "track:ungroup"));
        }
        entries
    }

    /// Puts the selected tracks (or the context track) into a new folder,
    /// where the first of them was.
    pub(super) fn group_selected_tracks(&mut self, cx: &mut Context<Self>) {
        let context = self.context_track_id_or_selected(cx);
        let changed = self.timeline.update(cx, |timeline, cx| {
            let state = &mut timeline.state;
            // The context track joins the selection it was clicked in, or
            // stands for itself when it is not part of it.
            if let Some(context) = context.as_deref() {
                if !state
                    .selected_range_track_ids()
                    .iter()
                    .any(|id| id == context)
                {
                    state.select_track(context);
                }
            }
            let scope = TrackEditScope::tracks(state.selected_range_track_ids());
            let edit = timeline.begin_track_edit(scope);
            let folders = timeline
                .state
                .tracks
                .iter()
                .filter(|track| track.track_type == TrackType::Group)
                .count();
            let name = format!("Folder {}", folders + 1);
            let grouped = timeline.state.group_selected_tracks(name).is_some();
            timeline.commit_track_edit("Group Tracks", edit, false, cx);
            cx.notify();
            grouped
        });
        if changed {
            self.mark_dirty();
            cx.notify();
        }
    }

    /// Takes the context track out of its folder, one level up.
    pub(super) fn remove_context_track_from_folder(&mut self, cx: &mut Context<Self>) {
        let Some(track_id) = self.context_track_id_or_selected(cx) else {
            return;
        };
        let changed = self.timeline.update(cx, |timeline, cx| {
            let edit = timeline.begin_track_edit(TrackEditScope::tracks([track_id.clone()]));
            let moved = timeline.state.remove_track_from_group(&track_id);
            timeline.commit_track_edit("Remove from Folder", edit, false, cx);
            cx.notify();
            moved
        });
        if changed {
            self.mark_dirty();
            cx.notify();
        }
    }

    /// Dissolves the context folder: its members move up a level, in place.
    pub(super) fn ungroup_context_folder(&mut self, cx: &mut Context<Self>) {
        let Some(group_id) = self.context_track_id_or_selected(cx) else {
            return;
        };
        let changed = self.timeline.update(cx, |timeline, cx| {
            let state = &timeline.state;
            if !state
                .find_track(&group_id)
                .is_some_and(|track| track.track_type == TrackType::Group)
            {
                return false;
            }
            // The folder and every track directly inside it: their folder and
            // output change, and undo has to put all of them back.
            let mut scope = vec![group_id.clone()];
            scope.extend(
                state
                    .tracks
                    .iter()
                    .filter(|track| track.parent_group_id.as_deref() == Some(group_id.as_str()))
                    .map(|track| track.id.clone()),
            );
            let edit = timeline.begin_track_edit(TrackEditScope::tracks(scope));
            let ungrouped = timeline.state.ungroup(&group_id);
            timeline.commit_track_edit("Ungroup Folder", edit, false, cx);
            cx.notify();
            ungrouped
        });
        if changed {
            self.mark_dirty();
            cx.notify();
        }
    }

    /// Folds the context folder open or shut.
    pub(super) fn toggle_context_folder(&mut self, cx: &mut Context<Self>) {
        if let Some(group_id) = self.context_track_id_or_selected(cx) {
            self.toggle_folder(&group_id, cx);
        }
    }

    /// Folds the folder `group_id` open or shut, in the arrangement and the
    /// mixer alike: one fold, one undo step.
    pub(crate) fn toggle_folder(&mut self, group_id: &str, cx: &mut Context<Self>) {
        let group_id = group_id.to_string();
        self.timeline.update(cx, |timeline, cx| {
            let edit = timeline.begin_track_edit(TrackEditScope::tracks([group_id.clone()]));
            if timeline.state.toggle_group_collapsed(&group_id).is_some() {
                timeline.commit_track_edit("Collapse Folder", edit, false, cx);
                cx.notify();
            }
        });
        self.mark_dirty_view_only();
        self.push_mixer_snapshot_to_window(cx);
        cx.notify();
    }
}

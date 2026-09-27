//! Cached mixer tree rows — rebuilt only when dirty flags require it.

use crate::assets;
use crate::components::mixer_tree_model::{MixerTreeModel, MixerTreeNodeKind, MixerTreeRow};
use crate::components::timeline::timeline_state::MixerTreeViewState;

/// Lightweight row for sidebar paint — no nested node hierarchy.
#[derive(Clone, Debug)]
pub struct MixerTreeVisibleRow {
    pub node_id: String,
    pub depth: u8,
    pub label: String,
    pub kind: MixerTreeNodeKind,
    pub channel_id: Option<String>,
    pub icon_path: Option<&'static str>,
    pub accent: gpui::Rgba,
    pub track_color: Option<gpui::Rgba>,
    pub expanded: bool,
    pub has_children: bool,
    pub visible_in_mixer: bool,
    pub pinned: bool,
    pub selected: bool,
    pub muted: bool,
    pub solo: bool,
    /// VSTi multi-out channel sounding because its parent instrument is soloed,
    /// not because its own Solo is engaged. Mirrors the strip's S button state.
    pub solo_implied: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct MixerTreeDirty {
    pub routing: bool,
    pub filter: bool,
    pub expansion: bool,
    pub selection: bool,
    pub hover: bool,
}

impl MixerTreeDirty {
    pub fn any(&self) -> bool {
        self.routing || self.filter || self.expansion || self.selection || self.hover
    }

    pub fn needs_visible_rows(&self) -> bool {
        self.routing || self.filter || self.expansion || self.selection
    }

    pub fn clear_after_visible_rebuild(&mut self) {
        self.routing = false;
        self.filter = false;
        self.expansion = false;
        self.selection = false;
    }
}

#[derive(Clone, Debug, Default)]
pub struct MixerTreeRenderCache {
    pub model: Option<MixerTreeModel>,
    pub visible_rows: Vec<MixerTreeVisibleRow>,
    pub dirty: MixerTreeDirty,
    pub routing_gen: u64,
    /// The timeline's `track_names_revision`. Rows copy track names, and a
    /// rename never advances the routing version.
    pub names_revision: u64,
    pub output_channels: u32,
    pub filter: String,
    pub show_only_selected_group: bool,
    pub selected_channel_id: Option<String>,
    pub hovered_row: Option<usize>,
    pub model_rebuild_count: u64,
    pub visible_rows_rebuild_count: u64,
}

impl MixerTreeRenderCache {
    pub fn mark_routing_dirty(&mut self) {
        self.dirty.routing = true;
    }

    pub fn mark_filter_dirty(&mut self) {
        self.dirty.filter = true;
    }

    pub fn mark_expansion_dirty(&mut self) {
        self.dirty.expansion = true;
    }

    pub fn mark_selection_dirty(&mut self) {
        self.dirty.selection = true;
    }

    pub fn set_hovered_row(&mut self, row: Option<usize>) -> bool {
        if self.hovered_row == row {
            return false;
        }
        self.hovered_row = row;
        self.dirty.hover = true;
        true
    }

    pub fn clear_hover_dirty(&mut self) {
        self.dirty.hover = false;
    }

    pub fn sync_routing_key(
        &mut self,
        routing_gen: u64,
        names_revision: u64,
        output_channels: u32,
        filter: &str,
        show_only: bool,
        selected_id: Option<&str>,
    ) {
        let selected = selected_id.map(str::to_string);
        // `self.model.is_none()` forces the very first sync to build even when the
        // routing version still matches the cache default (0 == 0) — i.e. the graph
        // became ready without the version advancing past what this cache last saw.
        // The spec's "if local version is missing or older, schedule one rebuild".
        if self.model.is_none()
            || self.routing_gen != routing_gen
            || self.names_revision != names_revision
            || self.output_channels != output_channels
            || self.show_only_selected_group != show_only
            || (show_only && self.selected_channel_id != selected)
        {
            self.routing_gen = routing_gen;
            self.names_revision = names_revision;
            self.output_channels = output_channels;
            self.show_only_selected_group = show_only;
            self.selected_channel_id = selected;
            self.dirty.routing = true;
        }
        if self.filter != filter {
            self.filter = filter.to_string();
            self.dirty.filter = true;
        }
        if self.selected_channel_id.as_deref() != selected_id {
            self.selected_channel_id = selected_id.map(str::to_string);
            self.dirty.selection = true;
        }
    }

    pub fn rebuild_model_if_needed(
        &mut self,
        tracks: &[crate::components::timeline::timeline_state::TrackState],
        view: &MixerTreeViewState,
    ) {
        if !self.dirty.routing && !self.dirty.filter {
            return;
        }
        let model = MixerTreeModel::build(
            tracks,
            self.output_channels,
            view,
            &self.filter,
            self.show_only_selected_group,
            self.selected_channel_id.as_deref(),
        );
        self.model = Some(model);
        self.dirty.expansion = true;
        self.dirty.selection = true;
        self.model_rebuild_count = self.model_rebuild_count.saturating_add(1);
        crate::perf::count("mixer_tree_model_rebuild_count", self.model_rebuild_count);
        if crate::perf::mixer_tree_debug_enabled() {
            eprintln!(
                "[mixer-tree] model rebuild routing_gen={} filter={:?}",
                self.routing_gen, self.filter
            );
        }
    }

    pub fn rebuild_visible_rows_if_needed(&mut self, view: &MixerTreeViewState) {
        if !self.dirty.needs_visible_rows() {
            return;
        }
        let Some(model) = self.model.as_ref() else {
            self.visible_rows.clear();
            self.dirty.clear_after_visible_rebuild();
            return;
        };
        let flat = model.flatten(view, self.selected_channel_id.as_deref());
        self.visible_rows = flat.into_iter().map(visible_row_from_flat).collect();
        self.visible_rows_rebuild_count = self.visible_rows_rebuild_count.saturating_add(1);
        crate::perf::count(
            "visible_rows_rebuild_count",
            self.visible_rows_rebuild_count,
        );
        if crate::perf::mixer_tree_debug_enabled() {
            eprintln!(
                "[mixer-tree] visible rows rebuild count={}",
                self.visible_rows.len()
            );
        }
        self.dirty.clear_after_visible_rebuild();
    }

    pub fn recompute(
        &mut self,
        tracks: &[crate::components::timeline::timeline_state::TrackState],
        view: &MixerTreeViewState,
    ) {
        if !self.dirty.any() {
            return;
        }
        self.rebuild_model_if_needed(tracks, view);
        self.rebuild_visible_rows_if_needed(view);
        for row in &mut self.visible_rows {
            let Some(channel_id) = row.channel_id.as_deref() else {
                continue;
            };
            if let Some(track) = tracks.iter().find(|track| track.id == channel_id) {
                row.muted = track.muted;
                row.solo = track.solo;
            }
            // A VSTi multi-out channel is sounded by its parent instrument's
            // solo too, so the sidebar S must not read as off while the channel
            // is audible. Cheap: only `vsti-out:` ids reach the track scan.
            row.solo_implied = !row.solo
                && crate::components::timeline::timeline_state::is_vsti_output_solo_implied(
                    channel_id, tracks,
                );
        }
    }
}

fn visible_row_from_flat(row: MixerTreeRow) -> MixerTreeVisibleRow {
    let node = row.node;
    MixerTreeVisibleRow {
        node_id: node.id,
        depth: row.depth.min(255) as u8,
        label: node.display_name,
        kind: node.kind,
        channel_id: node.channel_id,
        icon_path: icon_for_kind(node.kind),
        accent: node.kind.accent_color(),
        track_color: node.track_color,
        expanded: row.expanded,
        has_children: row.has_children,
        visible_in_mixer: row.visible_in_mixer,
        pinned: row.pinned,
        selected: row.selected,
        muted: row.muted,
        solo: row.solo,
        // Resolved against the live track list in `recompute`.
        solo_implied: false,
    }
}

fn icon_for_kind(kind: MixerTreeNodeKind) -> Option<&'static str> {
    match kind {
        MixerTreeNodeKind::AudioTrack => Some(assets::ICON_VOLUME_2_PATH),
        MixerTreeNodeKind::InstrumentTrack | MixerTreeNodeKind::Plugin => {
            Some(assets::ICON_SLIDERS_HORIZONTAL_PATH)
        }
        MixerTreeNodeKind::Bus => Some(assets::ICON_SLIDERS_HORIZONTAL_PATH),
        MixerTreeNodeKind::FxReturn => Some(assets::ICON_VOLUME_2_PATH),
        MixerTreeNodeKind::HardwareOutput | MixerTreeNodeKind::BusOutput => {
            Some(assets::ICON_VOLUME_2_PATH)
        }
        MixerTreeNodeKind::MidiTrack => Some(assets::ICON_SLIDERS_HORIZONTAL_PATH),
        MixerTreeNodeKind::Root | MixerTreeNodeKind::Group => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::timeline::timeline_state::{MixerTreeViewState, TrackState};

    #[test]
    fn first_sync_builds_model_then_only_rebuilds_on_version_change() {
        let mut cache = MixerTreeRenderCache::default();
        let view = MixerTreeViewState::default();
        let tracks: Vec<TrackState> = Vec::new();

        // First Studio open: the routing version still matches the cache default
        // (0 == 0), but no model exists yet. This must still build (the first-open
        // bug was that the equal versions skipped the build → blank sidebar).
        cache.sync_routing_key(0, 0, 2, "", false, None);
        assert!(cache.dirty.routing, "first sync must mark routing dirty");
        cache.recompute(&tracks, &view);
        assert!(cache.model.is_some(), "tree model must build on first sync");
        assert_eq!(cache.model_rebuild_count, 1);

        // A no-op re-sync (the shape of a meter / fader repaint): nothing changed,
        // so the tree must NOT rebuild.
        cache.sync_routing_key(0, 0, 2, "", false, None);
        cache.recompute(&tracks, &view);
        assert_eq!(
            cache.model_rebuild_count, 1,
            "no-op sync must not rebuild the tree"
        );

        // Routing graph advances (tracks ready / added / renamed): rebuild once.
        cache.sync_routing_key(1, 0, 2, "", false, None);
        cache.recompute(&tracks, &view);
        assert_eq!(cache.model_rebuild_count, 2);
    }

    /// A rename changes no routing, so only the names revision can tell the
    /// cache that the rows it copied a track name into are stale.
    #[test]
    fn a_rename_rebuilds_the_rows_on_the_same_routing_version() {
        use crate::components::mixer_tree_model::ensure_timeline_mixer_tree_defaults;
        use crate::components::timeline::timeline_state::TimelineState;

        let mut state = TimelineState::default();
        let id = state.create_midi_track();
        assert!(state.rename_track(&id, "Before"));
        ensure_timeline_mixer_tree_defaults(&mut state, 2);
        let labels = |cache: &MixerTreeRenderCache| -> Vec<String> {
            cache
                .visible_rows
                .iter()
                .filter(|row| row.channel_id.as_deref() == Some(id.as_str()))
                .map(|row| row.label.clone())
                .collect()
        };

        let mut cache = MixerTreeRenderCache::default();
        cache.sync_routing_key(3, state.track_names_revision, 2, "", false, None);
        cache.recompute(&state.tracks, &state.mixer_tree);
        assert_eq!(labels(&cache), vec!["Before".to_string()]);
        let builds = cache.model_rebuild_count;

        assert!(state.rename_track(&id, "After"));
        cache.sync_routing_key(3, state.track_names_revision, 2, "", false, None);
        cache.recompute(&state.tracks, &state.mixer_tree);
        assert_eq!(cache.model_rebuild_count, builds + 1);
        assert_eq!(labels(&cache), vec!["After".to_string()]);

        // Nothing renamed since: the next sync is a no-op again.
        cache.sync_routing_key(3, state.track_names_revision, 2, "", false, None);
        cache.recompute(&state.tracks, &state.mixer_tree);
        assert_eq!(cache.model_rebuild_count, builds + 1);
    }
}

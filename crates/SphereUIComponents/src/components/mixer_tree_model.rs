//! Mixer channel tree — mirrors the live mixer routing graph for the left sidebar.

use std::collections::HashSet;

use crate::audio_routing::build_output_channel_options;
use crate::components::mixer_panel::vsti_output_bus_label;
use crate::components::timeline::timeline_state::{
    is_vsti_output_child_track_id, vsti_output_child_insert_id, InsertSlotState,
    MixerTreeViewState, TimelineState, TrackState, TrackType,
};

pub const MIXER_TREE_ROOT_ID: &str = "mixer-tree:root";
pub const MIXER_TREE_GROUP_AUDIO: &str = "mixer-tree:group:audio";
pub const MIXER_TREE_GROUP_INSTRUMENT: &str = "mixer-tree:group:instrument";
pub const MIXER_TREE_GROUP_MIDI: &str = "mixer-tree:group:midi";
pub const MIXER_TREE_GROUP_BUS: &str = "mixer-tree:group:bus";
pub const MIXER_TREE_GROUP_RETURN: &str = "mixer-tree:group:return";
pub const MIXER_TREE_GROUP_OUTPUT: &str = "mixer-tree:group:output";

/// Prefix for hardware output nodes that do not map to mixer strips.
pub const MIXER_HW_OUTPUT_PREFIX: &str = "mixer-hw-out:";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MixerTreeNodeKind {
    Root,
    Group,
    AudioTrack,
    InstrumentTrack,
    MidiTrack,
    Bus,
    FxReturn,
    Plugin,
    BusOutput,
    HardwareOutput,
}

impl MixerTreeNodeKind {
    pub fn accent_color(self) -> gpui::Rgba {
        use crate::theme::Colors;
        match self {
            Self::AudioTrack => gpui::Rgba {
                r: 0.35,
                g: 0.78,
                b: 0.95,
                a: 1.0,
            },
            Self::InstrumentTrack | Self::Plugin => gpui::Rgba {
                r: 0.42,
                g: 0.82,
                b: 0.48,
                a: 1.0,
            },
            Self::MidiTrack => gpui::Rgba {
                r: 0.55,
                g: 0.72,
                b: 0.95,
                a: 1.0,
            },
            Self::Bus => gpui::Rgba {
                r: 0.95,
                g: 0.72,
                b: 0.28,
                a: 1.0,
            },
            Self::FxReturn => Colors::accent_primary(),
            Self::HardwareOutput | Self::BusOutput => gpui::Rgba {
                r: 0.55,
                g: 0.58,
                b: 0.62,
                a: 1.0,
            },
            Self::Root | Self::Group => Colors::text_faint(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct MixerTreeNode {
    pub id: String,
    pub display_name: String,
    pub kind: MixerTreeNodeKind,
    pub channel_id: Option<String>,
    pub plugin_instance_id: Option<String>,
    pub bus_index: Option<u8>,
    pub parent_id: Option<String>,
    pub track_color: Option<gpui::Rgba>,
    pub children: Vec<MixerTreeNode>,
}

/// One flattened row for virtualized tree rendering.
#[derive(Debug, Clone)]
pub struct MixerTreeRow {
    pub node: MixerTreeNode,
    pub depth: usize,
    pub expanded: bool,
    pub has_children: bool,
    pub visible_in_mixer: bool,
    pub pinned: bool,
    pub selected: bool,
    pub muted: bool,
    pub solo: bool,
}

#[derive(Debug, Clone)]
pub struct MixerTreeModel {
    pub root: MixerTreeNode,
    pub all_expandable_ids: Vec<String>,
}

impl MixerTreeModel {
    pub fn build(
        tracks: &[TrackState],
        output_device_channels: u32,
        view: &MixerTreeViewState,
        filter: &str,
        show_only_selected_group: bool,
        selected_channel_id: Option<&str>,
    ) -> Self {
        let filter = filter.trim().to_lowercase();
        let mut audio_children = Vec::new();
        let mut instrument_children = Vec::new();
        let mut midi_children = Vec::new();
        let mut bus_children = Vec::new();
        let mut return_children = Vec::new();

        for track in tracks {
            if is_vsti_output_child_track_id(&track.id) {
                continue;
            }
            match track.track_type {
                TrackType::Audio => {
                    audio_children.push(track_node(track, track.track_type));
                }
                TrackType::Instrument => {
                    instrument_children.push(instrument_track_node(track, tracks));
                }
                TrackType::Midi => {
                    midi_children.push(track_node(track, track.track_type));
                }
                TrackType::Bus => bus_children.push(track_node(track, TrackType::Bus)),
                TrackType::Group => bus_children.push(track_node(track, TrackType::Group)),
                TrackType::Return => return_children.push(track_node(track, TrackType::Return)),
                TrackType::Master => {}
                // Video carries no audio, so it has no mixer channel.
                TrackType::Video => {}
            }
        }

        let output_children = build_hardware_output_nodes(output_device_channels);

        let groups = [
            (MIXER_TREE_GROUP_AUDIO, "Audio Tracks", audio_children),
            (
                MIXER_TREE_GROUP_INSTRUMENT,
                "Instrument Tracks",
                instrument_children,
            ),
            (MIXER_TREE_GROUP_MIDI, "MIDI Tracks", midi_children),
            (MIXER_TREE_GROUP_BUS, "Buses", bus_children),
            (MIXER_TREE_GROUP_RETURN, "FX Returns", return_children),
            (MIXER_TREE_GROUP_OUTPUT, "Outputs", output_children),
        ];

        let selected_group_id = selected_channel_id.and_then(|ch| group_id_for_channel(tracks, ch));

        let mut group_nodes = Vec::new();
        for (id, label, children) in groups {
            if children.is_empty() {
                continue;
            }
            if show_only_selected_group && selected_group_id != Some(id) {
                continue;
            }
            group_nodes.push(MixerTreeNode {
                id: id.to_string(),
                display_name: label.to_string(),
                kind: MixerTreeNodeKind::Group,
                channel_id: None,
                plugin_instance_id: None,
                bus_index: None,
                parent_id: Some(MIXER_TREE_ROOT_ID.to_string()),
                track_color: None,
                children,
            });
        }

        let root = MixerTreeNode {
            id: MIXER_TREE_ROOT_ID.to_string(),
            display_name: "Project Mix".to_string(),
            kind: MixerTreeNodeKind::Root,
            channel_id: None,
            plugin_instance_id: None,
            bus_index: None,
            parent_id: None,
            track_color: None,
            children: group_nodes,
        };

        let mut model = Self {
            root,
            all_expandable_ids: Vec::new(),
        };
        model.collect_expandable_ids(&model.root.clone());
        if !filter.is_empty() {
            model = model.filtered(&filter);
        }
        // Default-expand root and non-empty groups on first open.
        let _ = view;
        model
    }

    fn collect_expandable_ids(&mut self, node: &MixerTreeNode) {
        if !node.children.is_empty() {
            self.all_expandable_ids.push(node.id.clone());
        }
        for child in &node.children {
            self.collect_expandable_ids(child);
        }
    }

    fn filtered(self, query: &str) -> Self {
        fn filter_node(node: MixerTreeNode, query: &str) -> Option<MixerTreeNode> {
            let name_match = node.display_name.to_lowercase().contains(query);
            let mut kept_children = Vec::new();
            for child in node.children {
                if let Some(filtered) = filter_node(child, query) {
                    kept_children.push(filtered);
                }
            }
            if name_match || !kept_children.is_empty() {
                Some(MixerTreeNode {
                    children: kept_children,
                    ..node
                })
            } else {
                None
            }
        }
        if let Some(root) = filter_node(self.root, query) {
            let mut model = Self {
                root,
                all_expandable_ids: Vec::new(),
            };
            model.collect_expandable_ids(&model.root.clone());
            model
        } else {
            Self {
                root: MixerTreeNode {
                    id: MIXER_TREE_ROOT_ID.to_string(),
                    display_name: "Project Mix".to_string(),
                    kind: MixerTreeNodeKind::Root,
                    channel_id: None,
                    plugin_instance_id: None,
                    bus_index: None,
                    parent_id: None,
                    track_color: None,
                    children: Vec::new(),
                },
                all_expandable_ids: vec![MIXER_TREE_ROOT_ID.to_string()],
            }
        }
    }

    pub fn flatten(
        &self,
        view: &MixerTreeViewState,
        selected_channel_id: Option<&str>,
    ) -> Vec<MixerTreeRow> {
        let mut rows = Vec::new();
        self.flatten_node(&self.root, 0, view, selected_channel_id, &mut rows);
        rows
    }

    fn flatten_node(
        &self,
        node: &MixerTreeNode,
        depth: usize,
        view: &MixerTreeViewState,
        selected_channel_id: Option<&str>,
        out: &mut Vec<MixerTreeRow>,
    ) {
        let has_children = !node.children.is_empty();
        let expanded = view.is_expanded(&node.id);
        let channel_id = node.channel_id.as_deref();
        let visible_in_mixer = channel_id.is_none_or(|id| !view.is_channel_hidden(id));
        let pinned = channel_id.is_some_and(|id| view.is_pinned(id));
        let selected = channel_id.is_some_and(|id| selected_channel_id == Some(id));
        out.push(MixerTreeRow {
            node: node.clone(),
            depth,
            expanded,
            has_children,
            visible_in_mixer,
            pinned,
            selected,
            muted: false,
            solo: false,
        });
        if has_children && expanded {
            for child in &node.children {
                self.flatten_node(child, depth + 1, view, selected_channel_id, out);
            }
        }
    }

    pub fn ancestor_path_to_channel(&self, channel_id: &str) -> Vec<String> {
        let mut path = Vec::new();
        if find_path(&self.root, channel_id, &mut path) {
            path
        } else {
            Vec::new()
        }
    }
}

fn find_path(node: &MixerTreeNode, channel_id: &str, path: &mut Vec<String>) -> bool {
    path.push(node.id.clone());
    if node.channel_id.as_deref() == Some(channel_id) {
        return true;
    }
    for child in &node.children {
        if find_path(child, channel_id, path) {
            return true;
        }
    }
    path.pop();
    false
}

fn group_id_for_channel(tracks: &[TrackState], channel_id: &str) -> Option<&'static str> {
    if channel_id.starts_with(MIXER_HW_OUTPUT_PREFIX) {
        return Some(MIXER_TREE_GROUP_OUTPUT);
    }
    if let Some(insert_id) = vsti_output_child_insert_id(channel_id) {
        let parent = tracks.iter().find(|t| {
            t.instrument_insert()
                .is_some_and(|slot| slot.id == insert_id)
        });
        return parent.map(|_| MIXER_TREE_GROUP_INSTRUMENT);
    }
    tracks
        .iter()
        .find(|t| t.id == channel_id)
        .map(|t| match t.track_type {
            TrackType::Audio => MIXER_TREE_GROUP_AUDIO,
            TrackType::Instrument => MIXER_TREE_GROUP_INSTRUMENT,
            TrackType::Midi => MIXER_TREE_GROUP_MIDI,
            TrackType::Bus => MIXER_TREE_GROUP_BUS,
            TrackType::Group => MIXER_TREE_GROUP_BUS,
            TrackType::Return => MIXER_TREE_GROUP_RETURN,
            TrackType::Master => MIXER_TREE_GROUP_OUTPUT,
            // Never reached: Video tracks are not added to the mixer tree.
            TrackType::Video => MIXER_TREE_GROUP_OUTPUT,
        })
}

fn track_kind_for_type(ty: TrackType) -> MixerTreeNodeKind {
    match ty {
        TrackType::Audio => MixerTreeNodeKind::AudioTrack,
        TrackType::Instrument => MixerTreeNodeKind::InstrumentTrack,
        TrackType::Midi => MixerTreeNodeKind::MidiTrack,
        TrackType::Bus => MixerTreeNodeKind::Bus,
        TrackType::Group => MixerTreeNodeKind::Group,
        TrackType::Return => MixerTreeNodeKind::FxReturn,
        TrackType::Master => MixerTreeNodeKind::Group,
        // Never reached: Video tracks are not added to the mixer tree.
        TrackType::Video => MixerTreeNodeKind::Group,
    }
}

fn track_node(track: &TrackState, ty: TrackType) -> MixerTreeNode {
    MixerTreeNode {
        id: track.id.clone(),
        display_name: track.name.clone(),
        kind: track_kind_for_type(ty),
        channel_id: Some(track.id.clone()),
        plugin_instance_id: None,
        bus_index: None,
        parent_id: None,
        track_color: Some(track.color),
        children: Vec::new(),
    }
}

fn instrument_track_node(track: &TrackState, tracks: &[TrackState]) -> MixerTreeNode {
    let mut node = track_node(track, TrackType::Instrument);
    let Some(slot) = track.instrument_insert().filter(|s| !s.is_empty()) else {
        return node;
    };
    let plugin_node = plugin_node(track, slot, tracks);
    node.children.push(plugin_node);
    node.parent_id = Some(MIXER_TREE_GROUP_INSTRUMENT.to_string());
    node
}

fn plugin_node(
    parent_track: &TrackState,
    slot: &InsertSlotState,
    tracks: &[TrackState],
) -> MixerTreeNode {
    let mut bus_children = Vec::new();
    let bus_counts = &slot.output_bus_channel_counts;
    let mut child_buses: Vec<(u8, &TrackState)> = tracks
        .iter()
        .filter_map(|t| {
            if vsti_output_child_insert_id(&t.id) != Some(slot.id.as_str()) {
                return None;
            }
            let bus_index =
                t.id.rsplit_once(":bus:")
                    .and_then(|(_, bus)| bus.parse::<u8>().ok())?;
            Some((bus_index, t))
        })
        .collect();
    child_buses.sort_by_key(|(idx, _)| *idx);
    for (bus_index, child_track) in child_buses {
        let label = if bus_counts.is_empty() {
            child_track.name.clone()
        } else {
            vsti_output_bus_label(bus_counts, bus_index)
        };
        bus_children.push(MixerTreeNode {
            id: child_track.id.clone(),
            display_name: label,
            kind: MixerTreeNodeKind::BusOutput,
            channel_id: Some(child_track.id.clone()),
            plugin_instance_id: Some(slot.id.clone()),
            bus_index: Some(bus_index),
            parent_id: Some(slot.id.clone()),
            track_color: Some(parent_track.color),
            children: Vec::new(),
        });
    }

    MixerTreeNode {
        id: slot.id.clone(),
        display_name: slot.display_name.clone(),
        kind: MixerTreeNodeKind::Plugin,
        channel_id: None,
        plugin_instance_id: Some(slot.id.clone()),
        bus_index: None,
        parent_id: Some(parent_track.id.clone()),
        track_color: Some(parent_track.color),
        children: bus_children,
    }
}

fn build_hardware_output_nodes(output_device_channels: u32) -> Vec<MixerTreeNode> {
    build_output_channel_options(output_device_channels)
        .into_iter()
        .filter(|opt| opt.channels.len() == 2)
        .map(|opt| {
            let id = format!("{MIXER_HW_OUTPUT_PREFIX}{}", opt.id);
            MixerTreeNode {
                id: id.clone(),
                display_name: opt
                    .label
                    .replace("Output ", "Out ")
                    .replace(" (Stereo)", " Stereo"),
                kind: MixerTreeNodeKind::HardwareOutput,
                channel_id: Some(id),
                plugin_instance_id: None,
                bus_index: None,
                parent_id: Some(MIXER_TREE_GROUP_OUTPUT.to_string()),
                track_color: None,
                children: Vec::new(),
            }
        })
        .collect()
}

/// Expand ancestor nodes so `channel_id` is visible in the flattened tree.
pub fn expand_ancestors_for_channel(
    view: &mut MixerTreeViewState,
    model: &MixerTreeModel,
    channel_id: &str,
) {
    for id in model.ancestor_path_to_channel(channel_id) {
        view.set_expanded(id, true);
    }
    view.set_expanded(MIXER_TREE_ROOT_ID, true);
}

/// Seed default expanded groups during session install (not per-frame render).
///
/// Once per project: the persisted `mixer_tree_initialized` latch marks the
/// tree as set up, so a project saved with everything collapsed stays
/// collapsed instead of looking like one that never had defaults.
pub fn ensure_timeline_mixer_tree_defaults(state: &mut TimelineState, output_device_channels: u32) {
    if state.mixer_tree_initialized || !state.mixer_tree.expanded_node_ids.is_empty() {
        state.mixer_tree_initialized = true;
        return;
    }
    state.mixer_tree_initialized = true;
    let model = MixerTreeModel::build(
        &state.tracks,
        output_device_channels,
        &state.mixer_tree,
        "",
        false,
        None,
    );
    state.mixer_tree.expanded_node_ids = default_expanded_nodes(&model);
}

/// Default expanded nodes for a fresh project.
pub fn default_expanded_nodes(model: &MixerTreeModel) -> HashSet<String> {
    let mut ids = HashSet::new();
    ids.insert(MIXER_TREE_ROOT_ID.to_string());
    for group_id in model.all_expandable_ids.iter() {
        if group_id.starts_with("mixer-tree:group:") {
            ids.insert(group_id.clone());
        }
    }
    ids
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::timeline::timeline_state::{
        CreateTrackOptions, InsertPluginFormat, TimelineState, TrackType,
    };

    fn drum_scenario(output_bus_layout: &[u32]) -> (TimelineState, String, String) {
        let mut state = TimelineState::default();
        let track_id = state.create_track(CreateTrackOptions {
            track_type: TrackType::Instrument,
            name: "Drums".into(),
            color: crate::color::auto_color_for_index(0),
            volume: 0.8,
            pan: 0.0,
            armed: false,
            input_monitor: crate::project::InputMonitorMode::Off,
        });
        let slot = state.ensure_insert_slot_at(&track_id, 0).expect("slot");
        state.set_insert_plugin(
            &track_id,
            &slot,
            "drums".to_string(),
            Some(std::path::PathBuf::from("C:/p/drums.vst3")),
            InsertPluginFormat::Vst3,
            None,
            "Drums".to_string(),
        );
        state.set_insert_output_bus_layout(&track_id, &slot, output_bus_layout);
        let output_channels = output_bus_layout.iter().copied().sum::<u32>().max(2);
        state.auto_enable_detected_insert_outputs(&track_id, &slot, output_channels);
        state.ensure_vsti_output_child_tracks(&track_id, &slot, output_channels, "Drums", true);
        (state, track_id, slot)
    }

    #[test]
    fn instrument_multiout_tree_has_plugin_and_bus_children() {
        let (state, _track_id, _slot) = drum_scenario(&[2, 2, 2, 2]);

        let model = MixerTreeModel::build(
            &state.tracks,
            2,
            &MixerTreeViewState::default(),
            "",
            false,
            None,
        );
        let instrument_group = model
            .root
            .children
            .iter()
            .find(|n| n.id == MIXER_TREE_GROUP_INSTRUMENT)
            .expect("instrument group");
        assert_eq!(instrument_group.children.len(), 1);
        let track_node = &instrument_group.children[0];
        assert_eq!(track_node.kind, MixerTreeNodeKind::InstrumentTrack);
        assert_eq!(track_node.children.len(), 1);
        let plugin_node = &track_node.children[0];
        assert_eq!(plugin_node.kind, MixerTreeNodeKind::Plugin);
        assert_eq!(plugin_node.children.len(), 4);
        assert!(plugin_node
            .children
            .iter()
            .all(|c| c.kind == MixerTreeNodeKind::BusOutput));
    }

    /// Solo on the main VSTi track sounds every one of its output channels
    /// (engine: `has_soloed_vsti_output_parent`), so the mixer must show those
    /// channels as soloed-by-parent instead of leaving their S buttons dark
    /// while they are plainly audible.
    #[test]
    fn parent_instrument_solo_implies_solo_on_its_output_channels() {
        use crate::components::timeline::timeline_state::{
            is_vsti_output_solo_implied, vsti_output_child_track_id,
        };

        let (mut state, track_id, slot) = drum_scenario(&[2, 2]);
        let child_id = vsti_output_child_track_id(&slot, 1);

        assert!(
            !is_vsti_output_solo_implied(&child_id, &state.tracks),
            "nothing is soloed yet"
        );

        state.set_track_solo(&track_id, true);
        assert!(
            is_vsti_output_solo_implied(&child_id, &state.tracks),
            "the instrument's solo must reach its output channels"
        );

        // A channel soloed on its own is a real solo, not an inherited one: the
        // parent is clear, so the strip shows the engaged state, not the
        // implied one.
        state.set_track_solo(&track_id, false);
        state.set_track_solo(&child_id, true);
        assert!(!is_vsti_output_solo_implied(&child_id, &state.tracks));

        // Ordinary tracks never inherit: only `vsti-out:` ids have a parent.
        assert!(!is_vsti_output_solo_implied(&track_id, &state.tracks));
    }
}

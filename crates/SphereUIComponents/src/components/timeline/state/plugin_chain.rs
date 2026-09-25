use super::*;

pub const VSTI_OUTPUT_CHILD_TRACK_PREFIX: &str = "vsti-out:";

pub fn vsti_output_child_track_id(plugin_instance_id: &str, bus_index: u8) -> String {
    format!("{VSTI_OUTPUT_CHILD_TRACK_PREFIX}{plugin_instance_id}:bus:{bus_index}")
}

pub fn is_vsti_output_child_track_id(track_id: &str) -> bool {
    track_id.starts_with(VSTI_OUTPUT_CHILD_TRACK_PREFIX)
}

/// Tracks that exist for mixer/engine routing but must not appear as arrangement
/// lanes (Bus/Return, plus VSTi multi-out child channels).
pub fn is_arrangement_hidden_track(track: &TrackState) -> bool {
    track.track_type.is_mixer_only() || is_vsti_output_child_track_id(&track.id)
}

/// Parent plugin instance (insert) id embedded in a child mixer-channel track id
/// (`vsti-out:{insert}:bus:{n}`), or `None` if `track_id` is not a child id.
pub fn vsti_output_child_insert_id(track_id: &str) -> Option<&str> {
    track_id
        .strip_prefix(VSTI_OUTPUT_CHILD_TRACK_PREFIX)?
        .split_once(":bus:")
        .map(|(insert, _)| insert)
}

/// The instrument track that owns a VSTi multi-out child strip — the "main
/// track" whose transport-facing state the channel follows. `None` for any
/// other track id, or when the owning instrument is gone.
pub fn vsti_output_parent_track<'a>(
    child_track_id: &str,
    tracks: &'a [TrackState],
) -> Option<&'a TrackState> {
    let insert_id = vsti_output_child_insert_id(child_track_id)?;
    tracks.iter().find(|track| {
        track
            .instrument_insert()
            .is_some_and(|slot| slot.id == insert_id)
    })
}

/// True when this channel is audible only because its parent instrument is
/// soloed, not because its own Solo is engaged. Mirrors the engine rule in
/// `has_soloed_vsti_output_parent`: soloing the main VSTi track solos every one
/// of its output channels, so those channels must *look* soloed too.
pub fn is_vsti_output_solo_implied(child_track_id: &str, tracks: &[TrackState]) -> bool {
    vsti_output_parent_track(child_track_id, tracks).is_some_and(|parent| parent.solo)
}

/// The mixer group key (`track_id:insert_id`) used by [`vsti_output_group_key`]
/// in the mixer view, for a parent track + instrument insert.
pub fn vsti_output_group_key(track_id: &str, insert_id: &str) -> String {
    format!("{track_id}:{insert_id}")
}

/// Group keys of every instrument whose VSTi multi-out group is collapsed, read
/// from the persisted per-insert `multiout_collapsed` flag (the single source of
/// truth). Both the docked and floating mixer derive their hidden-strip set from
/// this so they can never drift.
pub fn collapsed_vsti_output_group_keys_from_tracks(
    tracks: &[TrackState],
) -> std::collections::HashSet<String> {
    let mut keys = std::collections::HashSet::new();
    for track in tracks {
        if let Some(slot) = track.instrument_insert() {
            if slot.multiout_collapsed {
                keys.insert(vsti_output_group_key(&track.id, &slot.id));
            }
        }
    }
    keys
}

pub fn vsti_output_bus_index_for_channel(channel: u8) -> Option<u8> {
    (channel > 0).then_some((channel - 1) / 2)
}

pub fn vsti_output_child_channels_for_bus(bus_index: u8) -> (u8, u8) {
    (
        bus_index.saturating_mul(2).saturating_add(1),
        bus_index.saturating_mul(2).saturating_add(2),
    )
}

/// Maximum flat output channels the bridge carries (mirrors the engine's
/// `MAX_CHANNELS` / C++ `kMaxBridgeChannels`). Output buses whose flat channels
/// fall entirely past this cap are dropped by the bridge and cannot be heard, so
/// the host does not create silent strips for them.
pub const VSTI_MAX_BRIDGE_CHANNELS: u8 = 16;

/// Given the real per-bus output channel counts (bus-by-bus order, as the bridge
/// flattens them), return the `(start_channel_1based, channel_count)` of
/// `bus_index`, or `None` if out of range / zero-width.
pub fn vsti_output_bus_flat_range(bus_counts: &[u8], bus_index: usize) -> Option<(u8, u8)> {
    let mut start: u32 = 1;
    for (i, &count) in bus_counts.iter().enumerate() {
        if i == bus_index {
            if count == 0 || start > u8::MAX as u32 {
                return None;
            }
            return Some((start as u8, count));
        }
        start += count as u32;
    }
    None
}

/// The `(channel_l, channel_r)` 1-based flat channels a child stereo strip reads
/// for `bus_index`. With a real per-bus layout: a **mono** bus maps to
/// `(ch, ch)` so the engine duplicates it to both L and R (never paired with the
/// next bus); a **stereo+** bus maps to `(start, start+1)` preserving L/R (first
/// two channels of a multichannel bus). Some VST3 drum instruments expose
/// multi-out as one multichannel output bus instead of many buses; in that case
/// the flat channels are split into stereo child strips. With an empty layout
/// (unknown) it falls back to the legacy "every bus is a consecutive stereo
/// pair" assumption.
pub fn vsti_output_child_channels_for_bus_layout(
    bus_counts: &[u8],
    bus_index: u8,
) -> Option<(u8, u8)> {
    if bus_counts.is_empty() {
        return Some(vsti_output_child_channels_for_bus(bus_index));
    }
    if bus_counts.len() == 1 && bus_counts[0] > 2 {
        let left = bus_index.saturating_mul(2).saturating_add(1);
        if left > bus_counts[0] || left > VSTI_MAX_BRIDGE_CHANNELS {
            return None;
        }
        let right = left
            .saturating_add(1)
            .min(bus_counts[0])
            .min(VSTI_MAX_BRIDGE_CHANNELS);
        return Some((left, right));
    }
    let (start, count) = vsti_output_bus_flat_range(bus_counts, bus_index as usize)?;
    let r = if count >= 2 {
        start.saturating_add(1)
    } else {
        start
    };
    Some((start, r))
}

/// Output indices that should become child mixer strips, given the reported
/// output layout. Multi-bus plugins get one strip per bus. Single-bus
/// multichannel plugins get one strip per stereo flat-channel pair because
/// several drum VST3s expose outputs that way. A normal mono/stereo single-bus
/// instrument keeps playing on its parent instrument track. Returns empty for
/// mono/stereo single-bus or unknown layouts.
pub fn vsti_output_bus_strip_indices(bus_counts: &[u8]) -> Vec<u8> {
    if bus_counts.is_empty() {
        return Vec::new();
    }
    if bus_counts.len() == 1 {
        let channels = bus_counts[0].min(VSTI_MAX_BRIDGE_CHANNELS);
        if channels <= 2 {
            return Vec::new();
        }
        let pair_count = channels.div_ceil(2);
        return (0..pair_count).collect();
    }
    let mut indices = Vec::new();
    for bus_index in 0..bus_counts.len() {
        let Some((start, count)) = vsti_output_bus_flat_range(bus_counts, bus_index) else {
            continue;
        };
        // Drop buses the bridge can't carry (start past the channel cap).
        if start > VSTI_MAX_BRIDGE_CHANNELS {
            continue;
        }
        let _ = count;
        if bus_index <= u8::MAX as usize {
            indices.push(bus_index as u8);
        }
    }
    indices
}

/// Plugin format identifier mirrored from `project::PluginFormat`. Kept
/// in the UI state so we can render an icon/badge without depending on
/// the project crate from render code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertPluginFormat {
    Vst3,
    Vst2,
    Clap,
    Au,
    Lv2,
    Unknown,
}

impl InsertPluginFormat {
    pub fn label(self) -> &'static str {
        match self {
            InsertPluginFormat::Vst3 => "VST3",
            InsertPluginFormat::Vst2 => "VST2",
            InsertPluginFormat::Clap => "CLAP",
            InsertPluginFormat::Au => "AU",
            InsertPluginFormat::Lv2 => "LV2",
            InsertPluginFormat::Unknown => "?",
        }
    }

    /// Whether this format's plug-in is a file on disk. An Audio Unit is
    /// addressed by component id (`au:<type>:<subtype>:<manufacturer>`) with no
    /// module to stat, so `plugin_path` holds that identifier and a
    /// missing-file check would report every AU as broken. Mirrors
    /// `SpherePluginHost::registry::PluginFormat::has_module_file`.
    pub fn has_module_file(self) -> bool {
        !matches!(self, InsertPluginFormat::Au)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginRuntimeBackend {
    InProcess,
    ExternalBridge,
}

impl PluginRuntimeBackend {
    pub fn label(self) -> &'static str {
        match self {
            PluginRuntimeBackend::InProcess => "in_process",
            PluginRuntimeBackend::ExternalBridge => "external_bridge",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginRuntimeState {
    /// Persisted insert restored; runtime instance not created yet.
    NotLoaded,
    Loading,
    /// DSP instance loaded in host; not yet in audio graph.
    Loaded,
    /// DSP active in audio graph (processing).
    Active,
    /// Legacy alias — treated as Active in UI status.
    Ready,
    EditorOpening,
    EditorOpen,
    /// Editor UI closed; DSP instance still loaded and processing.
    EditorClosed,
    Bypassed,
    /// Plugin binary path no longer resolves.
    Missing(String),
    Failed(String),
    Crashed,
    Unloaded,
}

/// Load progress of an insert slot. Drives the chip color / label.
/// `Loading` is reserved for Phase 2 when actual plugin instantiation
/// runs on a worker thread. Phase 1 transitions Empty → Ready directly
/// because the runtime doesn't yet instantiate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InsertLoadStatus {
    Empty,
    Loading,
    Ready,
    /// Persisted plugin metadata present but binary not found on disk.
    Missing(String),
    Failed(String),
    Disabled,
}

impl Default for InsertLoadStatus {
    fn default() -> Self {
        InsertLoadStatus::Empty
    }
}

/// Read-only parameter snapshot populated from the plugin host on load.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginParameterState {
    pub id: u32,
    pub name: String,
    pub value_normalized: f32,
    pub automatable: bool,
    pub hidden: bool,
    pub read_only: bool,
    pub unit: String,
}

/// UI-side mirror of `project::ProjectInsert`. The runtime owns the
/// actual plugin processor; this struct only stores descriptor +
/// transient UI state (bypass, load status, last-seen parameters).
#[derive(Debug, Clone, PartialEq)]
pub struct InsertSlotState {
    pub id: String,
    /// Stable plugin identifier (`plugin_uid` / classId) — primary key
    /// against the plugin registry. `None` while the slot is empty.
    pub plugin_id: Option<String>,
    pub plugin_path: Option<std::path::PathBuf>,
    pub plugin_format: Option<InsertPluginFormat>,
    /// Registry-resolved plug-in role. `None` is retained only for legacy
    /// projects, where the old track/slot heuristic remains the fallback.
    pub plugin_is_instrument: Option<bool>,
    /// Plugin vendor from the registry, if available.
    pub vendor: Option<String>,
    /// Display label shown on the mixer strip. "Empty" when no plugin
    /// is loaded; the plugin's `display_name` otherwise.
    pub display_name: String,
    pub enabled: bool,
    pub bypassed: bool,
    pub load_status: InsertLoadStatus,
    pub runtime_backend: PluginRuntimeBackend,
    pub runtime_state: PluginRuntimeState,
    pub host_pid: Option<u32>,
    pub parameters: Vec<PluginParameterState>,
    /// 1-based VSTi output channels enabled for the engine-side stereo downmix.
    /// Empty means the default main output pair (1/2).
    pub enabled_audio_output_channels: Vec<u8>,
    /// Real per-bus output channel counts reported by the plugin, in the
    /// bus-by-bus order the bridge flattens them (bus0 channels, then bus1…).
    /// Drives one mixer strip per real output bus with correct mono→stereo
    /// duplication. Empty = unknown (falls back to legacy stereo pairing). Not
    /// persisted — re-detected from the host on every load via `ProcessingPrepared`.
    pub output_bus_channel_counts: Vec<u8>,
    /// Mixer-only view flag for this instrument's VSTi multi-out group: when
    /// `true` the child bus strips are hidden from the mixer (collapsed). This is
    /// a pure VIEW concern — it never changes audio routing, never removes child
    /// mixer channels or route nodes, and is not sent to the engine. Persisted so
    /// the collapsed/expanded state survives save/restore. Default `false`
    /// (expanded). Only meaningful on the instrument insert (`inserts[0]`).
    pub multiout_collapsed: bool,
    /// When true, open the plugin editor once runtime reaches Active/Loaded.
    pub pending_open_editor: bool,
    /// Opaque per-plugin state bytes for project persistence. Despite the
    /// name, this is not VST3-specific: `timeline_insert_to_project` /
    /// `project_insert_to_timeline` round-trip it for *any* `plugin_id`,
    /// keying only on presence, not format. Built-in plugins (e.g.
    /// rodharerist) use it too — their bytes are a UTF-8 JSON
    /// `RodhareistState` blob (see that crate's `state` module), not a VST3
    /// packed state; this field carries whichever byte format the plugin_id
    /// in question actually produces. For VST3 specifically: packed via
    /// `Vst3PluginState::to_packed_bytes`, loaded from the project file on
    /// open (then pushed to the plugin host after `LoadPlugin`), refreshed
    /// from the host on save. `Arc` because plugin states can be megabytes
    /// and slots are cloned freely. `None` = no captured state (fresh insert
    /// / stateless plugin).
    pub vst3_state: Option<std::sync::Arc<Vec<u8>>>,
}

impl InsertSlotState {
    pub fn empty(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            plugin_id: None,
            plugin_path: None,
            plugin_format: None,
            plugin_is_instrument: None,
            vendor: None,
            display_name: "Empty".to_string(),
            enabled: true,
            bypassed: false,
            load_status: InsertLoadStatus::Empty,
            runtime_backend: PluginRuntimeBackend::InProcess,
            runtime_state: PluginRuntimeState::Unloaded,
            host_pid: None,
            parameters: Vec::new(),
            enabled_audio_output_channels: Vec::new(),
            output_bus_channel_counts: Vec::new(),
            multiout_collapsed: false,
            pending_open_editor: false,
            vst3_state: None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.plugin_id.is_none()
    }

    /// Whether the plug-in host process can load this insert as an external
    /// module over the shared-audio bridge — the question the engine-sink wiring
    /// and the restore batch both ask before touching a slot.
    ///
    /// A VST3 needs a module path on disk. An Audio Unit has none: it is
    /// addressed by the component id in `plugin_id`, so requiring a path would
    /// silently skip every AU insert.
    ///
    /// Built-ins are deliberately excluded. They are identified by a
    /// `plugin_id` prefix rather than by format, and callers that host them
    /// handle them on their own branch.
    pub fn is_bridge_hosted_external_module(&self) -> bool {
        if self.plugin_id.as_deref() == Some(crate::components::plugin_picker::STUB_PLUGIN_ID) {
            return false;
        }
        // Listed exhaustively on purpose. This gate decides whether an insert is
        // ever handed to the host process, so a format missing from it presents
        // as "the plug-in is in the project but its editor never opens" — which
        // is exactly how VST2 and CLAP first shipped.
        match self.plugin_format {
            // Module formats the native bridge can instantiate: addressed by a
            // file on disk.
            Some(
                InsertPluginFormat::Vst3 | InsertPluginFormat::Vst2 | InsertPluginFormat::Clap,
            ) => self
                .plugin_path
                .as_ref()
                .is_some_and(|path| !path.as_os_str().is_empty()),
            // An Audio Unit has no module file — it is addressed by component id.
            Some(InsertPluginFormat::Au) => self
                .plugin_id
                .as_deref()
                .is_some_and(|component_id| !component_id.trim().is_empty()),
            // LV2 is scanned but not hosted; `Unknown` is a built-in or an
            // undeclared plug-in, and built-ins take the built-in path instead.
            Some(InsertPluginFormat::Lv2) | Some(InsertPluginFormat::Unknown) | None => false,
        }
    }
}

pub const MASTER_TRACK_ID: &str = "master";

/// Most slots one channel's insert chain holds, the instrument slot included.
pub const MAX_INSERT_SLOTS: usize = 8;

/// A planned move of one effect from one channel's chain to another's — what
/// [`TimelineState::apply_insert_move`] does and everything
/// [`TimelineState::revert_insert_move`] needs to undo it exactly. Built by
/// [`TimelineState::plan_insert_move`].
#[derive(Debug, Clone, PartialEq)]
pub struct InsertMove {
    pub insert_id: String,
    pub from_track: String,
    pub from_index: usize,
    pub to_track: String,
    /// Index in the destination chain after the move, placeholder included.
    pub to_index: usize,
    /// Empty slot 0 created when the destination is an Instrument track with
    /// no inserts at all, so the effect never lands in the instrument's place.
    /// Recorded so redo recreates the same id.
    pub placeholder_id: Option<String>,
    /// Where the insert's plug-in parameter lanes sat in the source track's
    /// lane list, ascending. They travel with the insert.
    pub lane_positions: Vec<usize>,
    /// The source track's automation focus, when it was on one of the moved
    /// insert's parameters. Cleared by the move, restored by its undo.
    pub cleared_selected_target: Option<AutomationTarget>,
}

fn target_is_insert_param(target: &AutomationTarget, insert_id: &str) -> bool {
    matches!(target, AutomationTarget::PluginParameter { insert_id: id, .. } if id == insert_id)
}

/// Whether `lane` automates one of `insert_id`'s parameters.
pub fn is_insert_parameter_lane(lane: &AutomationLaneState, insert_id: &str) -> bool {
    target_is_insert_param(&lane.target, insert_id)
}

impl TrackState {
    /// First chain index an effect may occupy.
    ///
    /// Which slot is the instrument is decided by position in several places
    /// (`instrument_insert`, MIDI routing, the engine's legacy slot-zero role),
    /// so slot 0 of a chain that has one is off limits to effects:
    ///
    /// * an Instrument track always reserves slot 0, even while it is an empty
    ///   placeholder or the track plays a built-in instrument;
    /// * a MIDI track reserves it when a plug-in there is the instrument — by
    ///   its registry role, or, for a legacy slot with no recorded role, by the
    ///   same slot-zero rule the engine applies;
    /// * every other channel carries audio only and reserves nothing.
    pub fn fx_chain_floor(&self) -> usize {
        match self.track_type {
            TrackType::Instrument => 1,
            TrackType::Midi => {
                let Some(first) = self.inserts.first() else {
                    return 0;
                };
                let is_instrument = match first.plugin_is_instrument {
                    Some(role) => role,
                    None => {
                        !first.is_empty()
                            || self.instrument_plugin_instance_id.as_deref()
                                == Some(first.id.as_str())
                    }
                };
                usize::from(is_instrument)
            }
            _ => 0,
        }
    }

    /// Whether an effect from another channel may be moved onto this one.
    ///
    /// A video track has no effect chain, a full chain has no room (counting
    /// the placeholder an empty Instrument track needs), and a MIDI track only
    /// processes audio once an instrument makes some — an effect dropped on a
    /// bare MIDI track would do nothing, or, with no recorded role, be taken
    /// for the instrument.
    pub fn accepts_moved_effect(&self) -> bool {
        let needed = 1 + usize::from(self.needs_instrument_placeholder());
        match self.track_type {
            TrackType::Video => false,
            TrackType::Midi if self.fx_chain_floor() == 0 => false,
            _ => self.inserts.len() + needed <= MAX_INSERT_SLOTS,
        }
    }

    /// An Instrument track with no inserts must get an empty slot 0 before an
    /// effect can go in after it.
    pub fn needs_instrument_placeholder(&self) -> bool {
        self.track_type == TrackType::Instrument && self.inserts.is_empty()
    }
}

impl TimelineState {
    pub fn insert_slots(&self, track_id: &str) -> Option<&Vec<InsertSlotState>> {
        if track_id == MASTER_TRACK_ID {
            Some(&self.master.inserts)
        } else {
            self.tracks
                .iter()
                .find(|track| track.id == track_id)
                .map(|track| &track.inserts)
        }
    }

    pub fn insert_slots_mut(&mut self, track_id: &str) -> Option<&mut Vec<InsertSlotState>> {
        if track_id == MASTER_TRACK_ID {
            Some(&mut self.master.inserts)
        } else {
            self.tracks
                .iter_mut()
                .find(|track| track.id == track_id)
                .map(|track| &mut track.inserts)
        }
    }

    pub fn insert_slot_at(&self, track_id: &str, slot_index: usize) -> Option<&InsertSlotState> {
        self.insert_slots(track_id)
            .and_then(|slots| slots.get(slot_index))
    }

    pub fn find_insert_slot(&self, track_id: &str, insert_id: &str) -> Option<&InsertSlotState> {
        self.insert_slots(track_id)?
            .iter()
            .find(|slot| slot.id == insert_id)
    }

    pub fn insert_owner_ids_containing(&self, insert_id: &str) -> Vec<String> {
        let mut owners: Vec<String> = self
            .tracks
            .iter()
            .filter(|track| track.inserts.iter().any(|slot| slot.id == insert_id))
            .map(|track| track.id.clone())
            .collect();
        if self.master.inserts.iter().any(|slot| slot.id == insert_id) {
            owners.push(MASTER_TRACK_ID.to_string());
        }
        owners
    }

    /// Append an empty insert slot to a track and return the slot id.
    /// Phase 1 — purely UI state; runtime is updated on the next project
    /// sync (the engine ignores unknown plugin descriptors gracefully).
    pub fn add_insert(&mut self, track_id: &str) -> Option<String> {
        self.insert_slots(track_id)?;
        let slot_id = self.next_insert_slot_id_for(track_id);
        let slot = InsertSlotState::empty(&slot_id);
        if plugin_debug_enabled() {
            eprintln!("[plugin] add_insert track={} slot_id={}", track_id, slot_id);
        }
        self.insert_slots_mut(track_id)?.push(slot);
        crate::forensic_trace::log_trace_plugin(track_id, &slot_id);
        Some(slot_id)
    }

    pub fn ensure_insert_slot_at(&mut self, track_id: &str, slot_index: usize) -> Option<String> {
        while self.insert_slots(track_id)?.len() <= slot_index {
            let slot_id = self.next_insert_slot_id_for(track_id);
            if plugin_debug_enabled() {
                eprintln!("[plugin] add_insert track={} slot_id={}", track_id, slot_id);
            }
            self.insert_slots_mut(track_id)?
                .push(InsertSlotState::empty(&slot_id));
        }
        self.insert_slot_at(track_id, slot_index)
            .map(|slot| slot.id.clone())
    }

    /// Whether any channel — every track and the master — holds `insert_id`.
    fn insert_id_in_use(&self, insert_id: &str) -> bool {
        self.master.inserts.iter().any(|slot| slot.id == insert_id)
            || self
                .tracks
                .iter()
                .any(|track| track.inserts.iter().any(|slot| slot.id == insert_id))
    }

    fn next_insert_slot_id_for(&self, owner_id: &str) -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_INSERT_SLOT_SEQ: AtomicU64 = AtomicU64::new(1);
        // Session-monotonic so a removed slot's id is NEVER regenerated. The
        // audio engine reconciles live VST3 instances by insert id (plugin
        // path/class_id are only an extra reuse guard), so handing a fresh slot
        // the id of a just-removed one resurrects the old instance — the exact
        // "old VSTi is still there" bug. The counter is process-global and
        // restarts with the process, so every candidate is also checked against
        // every channel, not just the owner: an insert keeps its id when it is
        // moved to another track, and after a reopen its old owner would
        // otherwise regenerate an id that now lives somewhere else. The engine
        // keys processors and bridge sinks by id alone, so two inserts sharing
        // one would share one instance.
        loop {
            let seq = NEXT_INSERT_SLOT_SEQ.fetch_add(1, Ordering::Relaxed);
            let candidate = format!("insert-{}-{}", owner_id, seq);
            if !self.insert_id_in_use(&candidate) {
                return candidate;
            }
        }
    }

    /// Record the registry-resolved role after assigning a plug-in. Keeping this
    /// separate preserves the broad test/helper API while production insertion
    /// paths can replace the old slot-position heuristic with authoritative data.
    pub fn set_insert_plugin_role(&mut self, track_id: &str, insert_id: &str, is_instrument: bool) {
        if track_id == MASTER_TRACK_ID {
            if let Some(slot) = self
                .master
                .inserts
                .iter_mut()
                .find(|slot| slot.id == insert_id)
            {
                slot.plugin_is_instrument = Some(is_instrument);
            }
            return;
        }
        let Some(track) = self.tracks.iter_mut().find(|track| track.id == track_id) else {
            return;
        };
        let Some(slot) = track.inserts.iter_mut().find(|slot| slot.id == insert_id) else {
            return;
        };
        slot.plugin_is_instrument = Some(is_instrument);
        if is_instrument {
            track.instrument_plugin_instance_id = Some(insert_id.to_string());
        } else if track.instrument_plugin_instance_id.as_deref() == Some(insert_id) {
            track.instrument_plugin_instance_id = None;
        }
    }

    /// Assign a plugin to an insert slot. The caller resolves the
    /// `plugin_id` → display metadata before calling so the UI doesn't
    /// have to know about the plugin registry directly.
    pub fn set_insert_plugin(
        &mut self,
        track_id: &str,
        insert_id: &str,
        plugin_id: String,
        plugin_path: Option<std::path::PathBuf>,
        plugin_format: InsertPluginFormat,
        vendor: Option<String>,
        display_name: String,
    ) {
        if track_id == MASTER_TRACK_ID {
            let Some(slot) = self.master.inserts.iter_mut().find(|i| i.id == insert_id) else {
                return;
            };
            slot.plugin_id = Some(plugin_id);
            slot.plugin_path = plugin_path;
            slot.plugin_format = Some(plugin_format);
            slot.vendor = vendor;
            slot.display_name = display_name;
            slot.load_status = InsertLoadStatus::Ready;
            slot.runtime_backend = PluginRuntimeBackend::InProcess;
            slot.runtime_state = PluginRuntimeState::Ready;
            slot.host_pid = None;
            slot.bypassed = false;
            slot.parameters.clear();
            crate::forensic_trace::log_trace_plugin(track_id, insert_id);
            if plugin_debug_enabled() {
                eprintln!(
                    "[plugin] set_insert_plugin track={} slot={} -> {}",
                    track_id, insert_id, slot.display_name
                );
            }
            return;
        }

        let Some(track) = self.tracks.iter_mut().find(|t| t.id == track_id) else {
            return;
        };
        let is_instrument_slot =
            matches!(track.track_type, TrackType::Instrument | TrackType::Midi)
                && track
                    .inserts
                    .first()
                    .map(|first| first.id == insert_id)
                    .unwrap_or(false);
        let Some(slot) = track.inserts.iter_mut().find(|i| i.id == insert_id) else {
            return;
        };
        slot.plugin_id = Some(plugin_id);
        slot.plugin_path = plugin_path;
        slot.plugin_format = Some(plugin_format);
        slot.vendor = vendor;
        slot.display_name = display_name;
        slot.load_status = InsertLoadStatus::Ready;
        slot.runtime_backend = PluginRuntimeBackend::InProcess;
        slot.runtime_state = PluginRuntimeState::Ready;
        slot.host_pid = None;
        slot.bypassed = false;
        slot.parameters.clear();
        crate::forensic_trace::log_trace_plugin(track_id, insert_id);
        if is_instrument_slot {
            track.instrument_plugin_instance_id = Some(insert_id.to_string());
            eprintln!("[instrument-route] track={track_id} instrument_instance={insert_id}");
            eprintln!("[instrument-route] plugin_instance_id={insert_id}");
            eprintln!("[instrument-route] route_ok=true");
        }
        if plugin_debug_enabled() {
            eprintln!(
                "[plugin] set_insert_plugin track={} slot={} -> {}",
                track_id, insert_id, slot.display_name
            );
        }
    }

    pub fn set_insert_runtime(
        &mut self,
        track_id: &str,
        insert_id: &str,
        backend: PluginRuntimeBackend,
        state: PluginRuntimeState,
        host_pid: Option<u32>,
    ) -> bool {
        let Some(slots) = self.insert_slots_mut(track_id) else {
            return false;
        };
        let Some(slot) = slots.iter_mut().find(|i| i.id == insert_id) else {
            return false;
        };
        let status = match &state {
            PluginRuntimeState::NotLoaded | PluginRuntimeState::Unloaded => {
                InsertLoadStatus::Loading
            }
            PluginRuntimeState::Loading => InsertLoadStatus::Loading,
            // EditorOpening is an EDITOR-state, not a plugin-load state: the
            // plugin itself is already loaded. Conflating it with Loading made
            // the inspector show "Loading" forever whenever an editor open
            // stalled. The plugin-load status here reflects the plugin only.
            PluginRuntimeState::Loaded
            | PluginRuntimeState::Active
            | PluginRuntimeState::Ready
            | PluginRuntimeState::EditorOpening
            | PluginRuntimeState::EditorOpen
            | PluginRuntimeState::EditorClosed
            | PluginRuntimeState::Bypassed => InsertLoadStatus::Ready,
            PluginRuntimeState::Missing(message) => InsertLoadStatus::Missing(message.clone()),
            PluginRuntimeState::Failed(message) => InsertLoadStatus::Failed(message.clone()),
            PluginRuntimeState::Crashed => {
                InsertLoadStatus::Failed("Plugin host crashed".to_string())
            }
        };
        let changed = slot.runtime_backend != backend
            || slot.runtime_state != state
            || slot.host_pid != host_pid
            || slot.load_status != status;
        slot.runtime_backend = backend;
        slot.runtime_state = state;
        slot.host_pid = host_pid;
        slot.load_status = status;
        changed
    }

    pub fn remove_insert(&mut self, track_id: &str, insert_id: &str) {
        if track_id == MASTER_TRACK_ID {
            let was_present = self.master.inserts.iter().any(|i| i.id == insert_id);
            self.master.inserts.retain(|i| i.id != insert_id);
            if was_present {
                eprintln!("[PluginUnload] model remove_insert track={track_id} slot={insert_id}");
            }
            if plugin_debug_enabled() {
                eprintln!(
                    "[plugin] remove_insert track={} slot={}",
                    track_id, insert_id
                );
            }
            return;
        }

        let Some(track) = self.tracks.iter_mut().find(|t| t.id == track_id) else {
            return;
        };
        let was_present = track.inserts.iter().any(|i| i.id == insert_id);
        track.inserts.retain(|i| i.id != insert_id);
        // Drop automation lanes bound to the removed instance's parameters — they
        // would otherwise reference a destroyed PluginInstanceId, and a re-add
        // gets a fresh id so they can never re-bind. (Automation bindings don't
        // keep the plugin alive, but stale lanes are dead weight + confusing.)
        track.automation_lanes.retain(|lane| {
            !matches!(&lane.target, AutomationTarget::PluginParameter { insert_id: id, .. } if id == insert_id)
        });
        // If the removed slot was this track's canonical MIDI/instrument
        // destination, drop the dangling pointer. The instrument insert is
        // always `inserts[0]` on an Instrument/MIDI track, so re-point it at the
        // new first non-empty slot, or clear it when none remains. Without this
        // the track keeps routing MIDI to a destroyed PluginInstanceId.
        if track.instrument_plugin_instance_id.as_deref() == Some(insert_id) {
            track.instrument_plugin_instance_id = track
                .inserts
                .first()
                .filter(|slot| !slot.is_empty())
                .map(|slot| slot.id.clone());
        }
        if was_present {
            eprintln!(
                "[PluginUnload] model remove_insert track={} slot={} instrument_instance={:?}",
                track_id, insert_id, track.instrument_plugin_instance_id
            );
        }
        let child_prefix = format!("{VSTI_OUTPUT_CHILD_TRACK_PREFIX}{insert_id}:bus:");
        self.tracks
            .retain(|track| !track.id.starts_with(&child_prefix));
        if plugin_debug_enabled() {
            eprintln!(
                "[plugin] remove_insert track={} slot={}",
                track_id, insert_id
            );
        }
    }

    /// Replace the insert slot identified by `old_insert_id` with a brand-new
    /// empty slot at the SAME index, returning the fresh slot id. The replace
    /// flow uses this so a replaced plugin never inherits the previous instance
    /// id — the engine reconciles live VST3 instances by insert id, so reusing
    /// the id would resurrect the old instance (and reloading the same plugin
    /// file must produce an independent instance). Returns `None` if the slot is
    /// not found. The caller is responsible for tearing the OLD instance down
    /// (editor / bridge host / engine sink) before calling this.
    pub fn replace_insert_with_fresh_slot(
        &mut self,
        track_id: &str,
        old_insert_id: &str,
    ) -> Option<String> {
        if track_id == MASTER_TRACK_ID {
            let idx = self
                .master
                .inserts
                .iter()
                .position(|s| s.id == old_insert_id)?;
            let fresh_id = self.next_insert_slot_id_for(track_id);
            self.master.inserts[idx] = InsertSlotState::empty(&fresh_id);
            eprintln!(
                "[PluginAdd] replace_insert_with_fresh_slot track={track_id} old={old_insert_id} new={fresh_id}"
            );
            return Some(fresh_id);
        }

        self.find_insert_slot(track_id, old_insert_id)?;
        let fresh_id = self.next_insert_slot_id_for(track_id);
        let track = self.tracks.iter_mut().find(|t| t.id == track_id)?;
        let idx = track.inserts.iter().position(|s| s.id == old_insert_id)?;
        track.inserts[idx] = InsertSlotState::empty(&fresh_id);
        // Drop automation lanes bound to the OLD instance so no old state leaks
        // into the fresh instance (which gets a brand-new id below).
        track.automation_lanes.retain(|lane| {
            !matches!(&lane.target, AutomationTarget::PluginParameter { insert_id: id, .. } if id == old_insert_id)
        });
        // Drop the dangling instrument pointer; `set_insert_plugin` re-points it
        // at the fresh id when the new plugin binds.
        if track.instrument_plugin_instance_id.as_deref() == Some(old_insert_id) {
            track.instrument_plugin_instance_id = None;
        }
        let child_prefix = format!("{VSTI_OUTPUT_CHILD_TRACK_PREFIX}{old_insert_id}:bus:");
        self.tracks
            .retain(|track| !track.id.starts_with(&child_prefix));
        eprintln!(
            "[PluginAdd] replace_insert_with_fresh_slot track={track_id} old={old_insert_id} new={fresh_id}"
        );
        Some(fresh_id)
    }

    /// First chain index an effect may occupy on `track_id`: 1 where slot 0 is
    /// the instrument, 0 everywhere else (the master included). See
    /// [`TrackState::fx_chain_floor`].
    pub fn fx_chain_floor(&self, track_id: &str) -> usize {
        if track_id == MASTER_TRACK_ID {
            return 0;
        }
        self.tracks
            .iter()
            .find(|track| track.id == track_id)
            .map(TrackState::fx_chain_floor)
            .unwrap_or(0)
    }

    /// Plan moving the effect `insert_id` from `from_track` to `to_track`,
    /// landing in the gap `gap` (0..=len) of the destination chain as it is
    /// now. `None` when the move is refused:
    ///
    /// * the insert is not on `from_track`, or both tracks are the same;
    /// * the insert sits below its chain's floor (it is the instrument) or is
    ///   an instrument plug-in — its identity is positional and it drives MIDI
    ///   routing, so it never changes channels;
    /// * the destination cannot take an effect (see
    ///   [`TrackState::accepts_moved_effect`]);
    /// * the destination is the master and the insert has plug-in parameter
    ///   automation, which the master has no lanes to hold.
    ///
    /// The gap is clamped to the destination's floor, so an effect can never
    /// land in front of an instrument.
    pub fn plan_insert_move(
        &self,
        from_track: &str,
        insert_id: &str,
        to_track: &str,
        gap: usize,
    ) -> Option<InsertMove> {
        if from_track == to_track {
            return None;
        }
        let from_slots = self.insert_slots(from_track)?;
        let from_index = from_slots.iter().position(|slot| slot.id == insert_id)?;
        if from_index < self.fx_chain_floor(from_track)
            || from_slots[from_index].plugin_is_instrument == Some(true)
        {
            return None;
        }
        let (to_len, placeholder) = if to_track == MASTER_TRACK_ID {
            if self.master.inserts.len() >= MAX_INSERT_SLOTS {
                return None;
            }
            (self.master.inserts.len(), false)
        } else {
            let to = self.tracks.iter().find(|track| track.id == to_track)?;
            if !to.accepts_moved_effect() {
                return None;
            }
            (to.inserts.len(), to.needs_instrument_placeholder())
        };
        let (lane_positions, cleared_selected_target) =
            match self.tracks.iter().find(|track| track.id == from_track) {
                Some(track) => (
                    track
                        .automation_lanes
                        .iter()
                        .enumerate()
                        .filter(|(_, lane)| is_insert_parameter_lane(lane, insert_id))
                        .map(|(position, _)| position)
                        .collect::<Vec<_>>(),
                    track
                        .selected_automation_target
                        .clone()
                        .filter(|target| target_is_insert_param(target, insert_id)),
                ),
                None => (Vec::new(), None),
            };
        if to_track == MASTER_TRACK_ID && !lane_positions.is_empty() {
            return None;
        }
        let to_len = to_len + usize::from(placeholder);
        let to_index = gap.max(self.fx_chain_floor(to_track)).min(to_len);
        Some(InsertMove {
            insert_id: insert_id.to_string(),
            from_track: from_track.to_string(),
            from_index,
            to_track: to_track.to_string(),
            to_index,
            placeholder_id: placeholder.then(|| self.next_insert_slot_id_for(to_track)),
            lane_positions,
            cleared_selected_target,
        })
    }

    /// Carry out a planned [`InsertMove`]: the SAME [`InsertSlotState`] leaves
    /// the source chain and enters the destination chain — never cloned or
    /// recreated, so its id, opaque plug-in state, parameters, bypass/enabled
    /// flags, output layout and runtime state all go with it, and the engine
    /// reuses the live instance by id on the next sync. Its plug-in parameter
    /// automation lanes move with it. Removal and insertion happen in this one
    /// call, so the id is never on two channels at once. Returns `false`, and
    /// changes nothing, when either side no longer matches the plan.
    pub fn apply_insert_move(&mut self, plan: &InsertMove) -> bool {
        self.transfer_insert(
            &plan.insert_id,
            &plan.from_track,
            &plan.to_track,
            plan.to_index,
            plan.placeholder_id.as_deref(),
            None,
            &[],
        )
    }

    /// Exact inverse of [`Self::apply_insert_move`]: the insert goes back to
    /// `from_index`, its lanes back to their old places in the source lane
    /// list, the source's cleared automation focus is restored, and the
    /// placeholder slot 0 is dropped again — unless something was loaded into
    /// it since, in which case it is no longer a placeholder and stays.
    pub fn revert_insert_move(&mut self, plan: &InsertMove) -> bool {
        let moved = self.transfer_insert(
            &plan.insert_id,
            &plan.to_track,
            &plan.from_track,
            plan.from_index,
            None,
            plan.placeholder_id.as_deref(),
            &plan.lane_positions,
        );
        if !moved {
            return false;
        }
        if let Some(target) = plan.cleared_selected_target.clone() {
            if let Some(track) = self.tracks.iter_mut().find(|t| t.id == plan.from_track) {
                track.selected_automation_target = Some(target);
            }
        }
        true
    }

    /// Move `insert_id` from one channel to another at `to_index`. Validates
    /// both sides before touching anything. `create_placeholder` makes an empty
    /// slot 0 with that id first when the destination chain is empty;
    /// `drop_placeholder` removes that slot from the source if it is still
    /// empty. Lanes land at `lane_positions` (ascending) when given, else at the
    /// end of the destination's lane list.
    #[allow(clippy::too_many_arguments)]
    fn transfer_insert(
        &mut self,
        insert_id: &str,
        from_track: &str,
        to_track: &str,
        to_index: usize,
        create_placeholder: Option<&str>,
        drop_placeholder: Option<&str>,
        lane_positions: &[usize],
    ) -> bool {
        if from_track == to_track
            || self.find_insert_slot(from_track, insert_id).is_none()
            || self.insert_slots(to_track).is_none()
        {
            return false;
        }
        // The master has no lane list; lanes must never be dropped on the way.
        if to_track == MASTER_TRACK_ID
            && self.find_track(from_track).is_some_and(|track| {
                track
                    .automation_lanes
                    .iter()
                    .any(|lane| is_insert_parameter_lane(lane, insert_id))
            })
        {
            return false;
        }
        let Some(slots) = self.insert_slots_mut(from_track) else {
            return false;
        };
        let Some(position) = slots.iter().position(|slot| slot.id == insert_id) else {
            return false;
        };
        let slot = slots.remove(position);
        if let Some(placeholder) = drop_placeholder {
            if slots
                .first()
                .is_some_and(|s| s.id == placeholder && s.is_empty())
            {
                slots.remove(0);
            }
        }
        let mut lanes = Vec::new();
        if let Some(track) = self.tracks.iter_mut().find(|t| t.id == from_track) {
            let mut kept = Vec::with_capacity(track.automation_lanes.len());
            for lane in std::mem::take(&mut track.automation_lanes) {
                if is_insert_parameter_lane(&lane, insert_id) {
                    lanes.push(lane);
                } else {
                    kept.push(lane);
                }
            }
            track.automation_lanes = kept;
            if track
                .selected_automation_target
                .as_ref()
                .is_some_and(|target| target_is_insert_param(target, insert_id))
            {
                track.selected_automation_target = None;
            }
        }
        let Some(slots) = self.insert_slots_mut(to_track) else {
            return false;
        };
        if let Some(placeholder) = create_placeholder {
            if slots.is_empty() {
                slots.push(InsertSlotState::empty(placeholder));
            }
        }
        let index = to_index.min(slots.len());
        slots.insert(index, slot);
        if let Some(track) = self.tracks.iter_mut().find(|t| t.id == to_track) {
            if lane_positions.len() == lanes.len() {
                for (position, lane) in lane_positions.iter().zip(lanes) {
                    let at = (*position).min(track.automation_lanes.len());
                    track.automation_lanes.insert(at, lane);
                }
            } else {
                track.automation_lanes.extend(lanes);
            }
        }
        if let Some(touched) = self.last_touched_plugin_param.as_mut() {
            if touched.insert_id == insert_id {
                touched.track_id = to_track.to_string();
            }
        }
        if plugin_debug_enabled() {
            eprintln!(
                "[plugin] move_insert_between_tracks slot={insert_id} {from_track} -> {to_track}[{index}]"
            );
        }
        true
    }

    /// Current insert-chain order as a list of slot ids (DSP order == UI order).
    pub fn insert_order(&self, track_id: &str) -> Vec<String> {
        self.insert_slots(track_id)
            .map(|slots| slots.iter().map(|slot| slot.id.clone()).collect())
            .unwrap_or_default()
    }

    /// Reorder a track's insert chain so its slot ids match `ordered_ids`.
    /// Slots named in `ordered_ids` are placed in that order; any slot NOT named
    /// (defensive — should never happen for a complete order) is kept at the end
    /// in its current relative order, so no slot is ever dropped.
    ///
    /// The existing [`InsertSlotState`] structs are reordered **in place** —
    /// never recreated — so every per-instance field (bypass, enabled,
    /// `vst3_state`, parameters, runtime state, host pid) follows the plugin
    /// instance across the move. The engine reconciles live VST3 instances by
    /// insert id, so reordering the `Vec` is all the runtime needs: the next
    /// project sync carries the new chain order down without recreating any
    /// instance. Automation lanes target `PluginParameter { insert_id, .. }`, so
    /// they also follow the instance untouched. Returns `true` if order changed.
    pub fn set_insert_order(&mut self, track_id: &str, ordered_ids: &[String]) -> bool {
        let Some(slots) = self.insert_slots_mut(track_id) else {
            return false;
        };
        let before: Vec<String> = slots.iter().map(|slot| slot.id.clone()).collect();
        let mut remaining: Vec<InsertSlotState> = std::mem::take(slots);
        let mut reordered: Vec<InsertSlotState> = Vec::with_capacity(remaining.len());
        for id in ordered_ids {
            if let Some(pos) = remaining.iter().position(|slot| &slot.id == id) {
                reordered.push(remaining.remove(pos));
            }
        }
        // Defensive: keep any slot the caller did not name, in current order.
        reordered.append(&mut remaining);
        let after: Vec<String> = reordered.iter().map(|slot| slot.id.clone()).collect();
        *slots = reordered;
        let changed = before != after;
        if changed && plugin_debug_enabled() {
            eprintln!("[plugin] set_insert_order track={track_id} {before:?} -> {after:?}");
        }
        changed
    }

    /// Pure helper: the insert-id order produced by moving `insert_id` into the
    /// gap at `insertion_index` (0..=len, the position *between* items where the
    /// drop indicator sits). Handles the removal/reinsertion off-by-one so the
    /// item lands exactly at the visual gap regardless of drag direction. Used
    /// by the drag-reorder drop handler to build the `ReorderFxSlot` command's
    /// `after_order`. Returns `current` unchanged if `insert_id` is absent.
    pub fn reordered_insert_ids(
        current: &[String],
        insert_id: &str,
        insertion_index: usize,
    ) -> Vec<String> {
        let mut ids: Vec<String> = current.to_vec();
        let Some(from) = ids.iter().position(|id| id == insert_id) else {
            return ids;
        };
        let id = ids.remove(from);
        // The gap index is in pre-removal coordinates; if the item was before
        // the gap, removing it shifts the gap left by one.
        let mut to = insertion_index;
        if from < to {
            to -= 1;
        }
        let to = to.min(ids.len());
        ids.insert(to, id);
        ids
    }

    pub fn toggle_insert_bypass(&mut self, track_id: &str, insert_id: &str) -> Option<bool> {
        let slot = self
            .insert_slots_mut(track_id)?
            .iter_mut()
            .find(|i| i.id == insert_id)?;
        slot.bypassed = !slot.bypassed;
        if plugin_debug_enabled() {
            eprintln!(
                "[plugin] toggle_bypass track={} slot={} -> {}",
                track_id, insert_id, slot.bypassed
            );
        }
        Some(slot.bypassed)
    }

    pub fn toggle_insert_enabled(&mut self, track_id: &str, insert_id: &str) -> Option<bool> {
        let slot = self
            .insert_slots_mut(track_id)?
            .iter_mut()
            .find(|i| i.id == insert_id)?;
        slot.enabled = !slot.enabled;
        if plugin_debug_enabled() {
            eprintln!(
                "[plugin] toggle_enabled track={} slot={} -> {}",
                track_id, insert_id, slot.enabled
            );
        }
        Some(slot.enabled)
    }

    /// Set an insert slot's load status by id (Phase 2b engine readback).
    /// Returns `true` if the status actually changed, so callers can decide
    /// whether to repaint. Used by the audio sync completion handler to flip
    /// `Failed` when the engine reports a native plugin failed to instantiate.
    pub fn set_insert_load_status(
        &mut self,
        track_id: &str,
        insert_id: &str,
        status: InsertLoadStatus,
    ) -> bool {
        let Some(slots) = self.insert_slots_mut(track_id) else {
            return false;
        };
        let Some(slot) = slots.iter_mut().find(|i| i.id == insert_id) else {
            return false;
        };
        if slot.load_status == status {
            return false;
        }
        if plugin_debug_enabled() {
            eprintln!(
                "[plugin] set_load_status track={} slot={} -> {:?}",
                track_id, insert_id, status
            );
        }
        slot.load_status = status;
        true
    }

    /// Store the plugin's real per-bus output channel counts (reported by the
    /// host on `ProcessingPrepared`). Sanitizes counts but keeps the bus-by-bus
    /// order intact so the flat-channel ranges line up with the bridge. Returns
    /// `true` if the stored layout changed.
    pub fn set_insert_output_bus_layout(
        &mut self,
        track_id: &str,
        insert_id: &str,
        bus_channel_counts: &[u32],
    ) -> bool {
        let Some(slots) = self.insert_slots_mut(track_id) else {
            return false;
        };
        let Some(slot) = slots.iter_mut().find(|i| i.id == insert_id) else {
            return false;
        };
        let sanitized: Vec<u8> = bus_channel_counts
            .iter()
            .map(|c| (*c).clamp(0, u8::MAX as u32) as u8)
            .collect();
        if slot.output_bus_channel_counts == sanitized {
            return false;
        }
        if plugin_debug_enabled() {
            eprintln!(
                "[plugin] set_output_bus_layout track={} slot={} buses={:?}",
                track_id, insert_id, sanitized
            );
        }
        slot.output_bus_channel_counts = sanitized;
        true
    }

    /// Replace the cached VST3 parameter list for an insert. Returns `true` when
    /// the stored list changed.
    pub fn set_insert_parameters(
        &mut self,
        track_id: &str,
        insert_id: &str,
        parameters: Vec<PluginParameterState>,
    ) -> bool {
        let Some(slots) = self.insert_slots_mut(track_id) else {
            return false;
        };
        let Some(slot) = slots.iter_mut().find(|i| i.id == insert_id) else {
            return false;
        };
        if slot.parameters == parameters {
            return false;
        }
        if plugin_debug_enabled() {
            eprintln!(
                "[plugin] set_parameters track={} slot={} count={}",
                track_id,
                insert_id,
                parameters.len()
            );
        }
        slot.parameters = parameters;
        true
    }

    /// Update one cached normalized VST3 parameter value for a real insert.
    /// Returns `true` if the UI snapshot changed.
    pub fn set_insert_parameter_value(
        &mut self,
        track_id: &str,
        insert_id: &str,
        param_id: u32,
        value_normalized: f32,
    ) -> bool {
        let Some(slots) = self.insert_slots_mut(track_id) else {
            return false;
        };
        let Some(slot) = slots.iter_mut().find(|i| i.id == insert_id) else {
            return false;
        };
        let Some(param) = slot.parameters.iter_mut().find(|p| p.id == param_id) else {
            return false;
        };
        let value = value_normalized.clamp(0.0, 1.0);
        if (param.value_normalized - value).abs() <= f32::EPSILON {
            return false;
        }
        param.value_normalized = value;
        true
    }

    /// Set the VSTi multi-out group's collapsed (mixer-view) flag on an insert.
    /// Returns the new value (`true` = collapsed) so the caller can log/persist.
    /// Pure view state — does not touch routing, child channels, or the engine.
    pub fn set_insert_multiout_collapsed(
        &mut self,
        track_id: &str,
        insert_id: &str,
        collapsed: bool,
    ) -> bool {
        if let Some(slots) = self.insert_slots_mut(track_id) {
            if let Some(slot) = slots.iter_mut().find(|i| i.id == insert_id) {
                slot.multiout_collapsed = collapsed;
            }
        }
        collapsed
    }

    /// Flip the collapsed flag for an insert's multi-out group, returning the new
    /// value. `false` when the insert is not found (no-op).
    pub fn toggle_insert_multiout_collapsed(&mut self, track_id: &str, insert_id: &str) -> bool {
        let current = self
            .find_insert_slot(track_id, insert_id)
            .map(|slot| slot.multiout_collapsed)
            .unwrap_or(false);
        self.set_insert_multiout_collapsed(track_id, insert_id, !current)
    }

    /// Group keys (`track_id:insert_id`) of every instrument whose VSTi multi-out
    /// group is currently collapsed. Used to derive the mixer's hidden-strip set
    /// from the persisted model (the single source of truth).
    pub fn collapsed_vsti_output_group_keys(&self) -> std::collections::HashSet<String> {
        collapsed_vsti_output_group_keys_from_tracks(&self.tracks)
    }

    pub fn auto_enable_detected_insert_outputs(
        &mut self,
        track_id: &str,
        insert_id: &str,
        output_channels: u32,
    ) -> bool {
        let Some(slots) = self.insert_slots_mut(track_id) else {
            return false;
        };
        let Some(slot) = slots.iter_mut().find(|i| i.id == insert_id) else {
            return false;
        };
        let mut changed = false;
        if slot.enabled_audio_output_channels.is_empty() {
            let output_channels = output_channels.clamp(2, 32) as u8;
            slot.enabled_audio_output_channels = (1..=output_channels).collect();
            changed = true;
            if plugin_debug_enabled() {
                eprintln!(
                    "[plugin] auto_enable_outputs track={} slot={} channels={:?}",
                    track_id, insert_id, slot.enabled_audio_output_channels
                );
            }
        }
        let plugin_name = slot.display_name.clone();
        let can_create_child_strips =
            !vsti_output_bus_strip_indices(&slot.output_bus_channel_counts).is_empty();
        changed |= self.ensure_vsti_output_child_tracks(
            track_id,
            insert_id,
            output_channels,
            &plugin_name,
            can_create_child_strips,
        );
        changed
    }

    pub fn ensure_vsti_output_child_tracks(
        &mut self,
        parent_track_id: &str,
        insert_id: &str,
        _output_channels: u32,
        plugin_name: &str,
        user_multiout_enabled: bool,
    ) -> bool {
        let Some(parent_index) = self
            .tracks
            .iter()
            .position(|track| track.id == parent_track_id)
        else {
            return false;
        };
        let Some(slot) = self.tracks[parent_index]
            .inserts
            .iter()
            .find(|slot| slot.id == insert_id)
        else {
            return false;
        };
        let output_bus_channel_counts = slot.output_bus_channel_counts.clone();
        let bus_indices_for_layout = vsti_output_bus_strip_indices(&output_bus_channel_counts);
        let multiout_capable = !bus_indices_for_layout.is_empty();
        let selected_path = if multiout_capable && user_multiout_enabled {
            "multiout_child_channels"
        } else {
            "parent_stereo"
        };
        eprintln!(
            "[PLUGIN CAPABILITY_DECISION]\nplugin_instance_id={insert_id}\nplugin_name={plugin_name}\nvendor=(unknown)\nis_instrument=true\nis_effect=false\naudio_input_bus_count=(unknown)\naudio_output_bus_count={}\nevent_input_bus_count=(unknown)\nmain_output_bus_count={}\naux_output_bus_count={}\nmultiout_capable={multiout_capable}\nuser_multiout_enabled={user_multiout_enabled}\nselected_path={selected_path}\ndecision_reason_from_capabilities_only=true",
            output_bus_channel_counts.len(),
            usize::from(!output_bus_channel_counts.is_empty()),
            output_bus_channel_counts.len().saturating_sub(1)
        );
        let mut bus_indices: Vec<u8> = if multiout_capable && user_multiout_enabled {
            // Real per-bus layout: one strip per genuine output bus. A mono bus
            // becomes its own stereo strip (duplicated L/R) instead of being
            // paired with the next bus's channel. Single-bus multichannel VST3s
            // are split into flat stereo pairs.
            bus_indices_for_layout
        } else {
            Vec::new()
        };
        bus_indices.sort_unstable();
        bus_indices.dedup();

        let parent_color = self.tracks[parent_index].color;
        let parent_id = self.tracks[parent_index].id.clone();
        let mut changed = false;
        let expected_ids: std::collections::HashSet<String> = bus_indices
            .iter()
            .map(|bus| vsti_output_child_track_id(insert_id, *bus))
            .collect();

        let before_len = self.tracks.len();
        self.tracks.retain(|track| {
            !track
                .id
                .strip_prefix(VSTI_OUTPUT_CHILD_TRACK_PREFIX)
                .is_some_and(|rest| {
                    rest.strip_prefix(insert_id)
                        .is_some_and(|suffix| suffix.starts_with(":bus:"))
                        && !expected_ids.contains(&track.id)
                })
        });
        changed |= self.tracks.len() != before_len;

        let mut insert_at = self
            .tracks
            .iter()
            .position(|track| track.id == parent_id)
            .map(|idx| idx + 1)
            .unwrap_or(self.tracks.len());
        while insert_at < self.tracks.len()
            && self.tracks[insert_at]
                .id
                .strip_prefix(VSTI_OUTPUT_CHILD_TRACK_PREFIX)
                .is_some_and(|rest| rest.starts_with(insert_id))
        {
            insert_at += 1;
        }

        if !bus_indices.is_empty() {
            let mut map_log = format!(
                "[VSTI BUS TO MIXER CHANNEL MAP]\nplugin_instance_id={insert_id}\nplugin_name={plugin_name}\nparent_track_id={parent_track_id}\n"
            );
            for bus_index in &bus_indices {
                let child_id = vsti_output_child_track_id(insert_id, *bus_index);
                let bus_number = bus_index.saturating_add(1);
                let bus_name = format!("Out Ch {bus_number}");
                let strip_label = format!("{plugin_name} {bus_name}");
                let (channel_l, channel_r) = vsti_output_child_channels_for_bus_layout(
                    &output_bus_channel_counts,
                    *bus_index,
                )
                .unwrap_or_else(|| vsti_output_child_channels_for_bus(*bus_index));
                let source_kind =
                    if output_bus_channel_counts.len() == 1 && output_bus_channel_counts[0] > 2 {
                        if channel_l == channel_r {
                            "flat_mono_tail"
                        } else {
                            "flat_stereo_pair"
                        }
                    } else {
                        output_bus_channel_counts
                            .get(*bus_index as usize)
                            .map(|count| match *count {
                                0 | 1 => "mono",
                                2 => "stereo",
                                _ => "multichannel",
                            })
                            .unwrap_or("stereo")
                    };
                let normalization = match source_kind {
                    "mono" => "mono_duplicate",
                    "stereo" | "flat_stereo_pair" => "preserve_stereo",
                    "flat_mono_tail" => "mono_duplicate",
                    _ => "downmix_to_stereo",
                };
                eprintln!(
                    "[BUS_TO_MIXER_GENERIC_MAP]\nplugin_instance_id={insert_id}\nsource_index={bus_index}\nsource_bus_index={bus_index}\nsource_channel_indices={channel_l},{channel_r}\nsource_kind={source_kind}\ntarget_mixer_channel_id={child_id}\ntarget_is_stereo=true\nnormalization={normalization}\nused_vendor_logic=false"
                );
                map_log.push_str(&format!(
                    "active_output_bus bus_index={bus_index} vst3_bus_name=\"{bus_name}\" channel_count=2 speaker_arrangement=stereo mixer_channel_id={child_id} route_node_id={child_id} strip_view_id={child_id} strip_label=\"{strip_label}\" meter_source_id={child_id} mute_solo_target_id={child_id}\n"
                ));
            }
            eprint!("{map_log}");
        }

        for bus_index in bus_indices {
            let child_id = vsti_output_child_track_id(insert_id, bus_index);
            if self.tracks.iter().any(|track| track.id == child_id) {
                continue;
            }
            let bus_number = bus_index.saturating_add(1);
            let bus_name = format!("Out Ch {bus_number}");
            let name = format!("{plugin_name} {bus_name}");
            let subscription_key = child_id.clone();
            eprintln!(
                "[MIXER MULTIOUT STRIP CREATED]\nplugin_instance_id={insert_id}\nplugin_name={plugin_name}\nbus_index={bus_index}\nbus_name=\"{bus_name}\"\nchannel_count=2\nmixer_channel_id={child_id}\nroute_node_id={child_id}\nparent_track_id={parent_track_id}\nsubscription_key={subscription_key}\nmeter_source_id={child_id}\nsolo_mute_target_id={child_id}"
            );
            eprintln!(
                "[CHILD MIXER INIT]\nplugin_instance_id={insert_id}\nbus_index={bus_index}\nmixer_channel_id={child_id}\nroute_node_id={child_id}\ninitial_gain=1.000000\ninitial_pan=0.000000\ninitial_mute=false\ninitial_solo=false\nroute_enabled=true\ndefault_destination=master\nroute_to_master_exists=true\nmeter_source_id={child_id}\nstate_inserted_in_central_store=true\naudio_thread_route_published=false"
            );
            self.tracks.insert(
                insert_at,
                TrackState {
                    listen: ListenMode::Off,
                    id: child_id,
                    name,
                    track_type: TrackType::Bus,
                    ara: None,
                    timebase: TrackTimebase::default(),
                    parent_group_id: None,
                    group_collapsed: false,
                    color: parent_color,
                    volume: volume::db_to_norm(0.0),
                    volume_effective: volume::db_to_norm(0.0),
                    volume_automation_read: true,
                    pan: 0.0,
                    muted: false,
                    solo: false,
                    armed: false,
                    input_monitor: InputMonitorMode::Off,
                    meter_level_l: 0.0,
                    meter_level_r: 0.0,
                    meter_peak_hold_l: 0.0,
                    meter_peak_hold_r: 0.0,
                    meter_clip: false,
                    clips: Vec::new(),
                    automation_lanes: Vec::new(),
                    lane_mode: TrackLaneMode::Clips,
                    selected_automation_target: None,
                    inserts: Vec::new(),
                    instrument_plugin_instance_id: None,
                    builtin_soundfont_player: false,
                    soundfont_path: None,
                    soundfont_preset: None,
                    soundfont_volume: 1.0,
                    soundfont_reverb_chorus: true,
                    soundfont_polyphony: 64,
                    soundfont_envelope: Default::default(),
                    soundfont_quality: Default::default(),
                    solfege: None,
                    sends: Vec::new(),
                    routing: TrackRoutingState::for_track_type(TrackType::Bus),
                    takes: Vec::new(),
                    takes_expanded: false,
                },
            );
            insert_at += 1;
            changed = true;
        }
        changed
    }

    pub fn set_insert_pending_editor_open(
        &mut self,
        track_id: &str,
        insert_id: &str,
        pending: bool,
    ) -> bool {
        let Some(slots) = self.insert_slots_mut(track_id) else {
            return false;
        };
        let Some(slot) = slots.iter_mut().find(|i| i.id == insert_id) else {
            return false;
        };
        if slot.pending_open_editor == pending {
            return false;
        }
        slot.pending_open_editor = pending;
        true
    }

    pub fn take_pending_insert_editor_opens(&mut self) -> Vec<(String, usize, String)> {
        let mut pending = Vec::new();
        for (index, slot) in self.master.inserts.iter_mut().enumerate() {
            if slot.pending_open_editor {
                slot.pending_open_editor = false;
                pending.push((MASTER_TRACK_ID.to_string(), index, slot.id.clone()));
            }
        }
        for track in &mut self.tracks {
            for (index, slot) in track.inserts.iter_mut().enumerate() {
                if slot.pending_open_editor {
                    slot.pending_open_editor = false;
                    pending.push((track.id.clone(), index, slot.id.clone()));
                }
            }
        }
        pending
    }
}

#[cfg(test)]
mod bridge_hosted_module_tests {
    use super::{InsertPluginFormat, InsertSlotState};

    fn slot_with(format: InsertPluginFormat, path: Option<&str>) -> InsertSlotState {
        let mut slot = InsertSlotState::empty("slot-1");
        slot.plugin_id = Some("plugin.id".to_string());
        slot.plugin_path = path.map(std::path::PathBuf::from);
        slot.plugin_format = Some(format);
        slot
    }

    /// Regression guard: this gate decides whether an insert is handed to the
    /// host process at all. When VST2 and CLAP were missing from it, both loaded
    /// nowhere and their editors silently never opened.
    #[test]
    fn every_hosted_module_format_is_bridge_hosted() {
        for format in [
            InsertPluginFormat::Vst3,
            InsertPluginFormat::Vst2,
            InsertPluginFormat::Clap,
        ] {
            let slot = slot_with(format, Some("C:/plugins/Thing.dll"));
            assert!(
                slot.is_bridge_hosted_external_module(),
                "{format:?} must be recognised as a bridge-hosted module"
            );
            // No module path means nothing to load, whatever the format says.
            assert!(!slot_with(format, None).is_bridge_hosted_external_module());
        }
    }

    #[test]
    fn audio_unit_is_bridge_hosted_by_component_id_not_path() {
        let mut slot = slot_with(InsertPluginFormat::Au, None);
        slot.plugin_id = Some("au:aufx:dely:appl".to_string());
        assert!(slot.is_bridge_hosted_external_module());
    }

    #[test]
    fn unhosted_formats_are_not_bridge_hosted() {
        // LV2 is scanned but has no runtime bridge; `Unknown` is a built-in or
        // an undeclared plug-in and takes a different path.
        for format in [InsertPluginFormat::Lv2, InsertPluginFormat::Unknown] {
            assert!(
                !slot_with(format, Some("C:/plugins/Thing.so")).is_bridge_hosted_external_module(),
                "{format:?} must not be routed to the native bridge"
            );
        }
    }
}

#[cfg(test)]
mod vsti_output_bus_layout_tests {
    use super::{
        vsti_output_bus_flat_range, vsti_output_bus_strip_indices,
        vsti_output_child_channels_for_bus_layout,
    };

    #[test]
    fn mono_buses_each_become_an_independent_stereo_strip() {
        // 4 mono output buses routed to separate mono outs.
        let counts = [1u8, 1, 1, 1];
        assert_eq!(vsti_output_bus_strip_indices(&counts), vec![0, 1, 2, 3]);
        // Each mono bus duplicates its single flat channel to both L and R,
        // never paired with the next bus.
        assert_eq!(
            vsti_output_child_channels_for_bus_layout(&counts, 0),
            Some((1, 1))
        );
        assert_eq!(
            vsti_output_child_channels_for_bus_layout(&counts, 1),
            Some((2, 2))
        );
        assert_eq!(
            vsti_output_child_channels_for_bus_layout(&counts, 2),
            Some((3, 3))
        );
        assert_eq!(
            vsti_output_child_channels_for_bus_layout(&counts, 3),
            Some((4, 4))
        );
    }

    #[test]
    fn stereo_buses_preserve_left_right() {
        let counts = [2u8, 2, 2];
        assert_eq!(vsti_output_bus_strip_indices(&counts), vec![0, 1, 2]);
        assert_eq!(
            vsti_output_child_channels_for_bus_layout(&counts, 0),
            Some((1, 2))
        );
        assert_eq!(
            vsti_output_child_channels_for_bus_layout(&counts, 1),
            Some((3, 4))
        );
        assert_eq!(
            vsti_output_child_channels_for_bus_layout(&counts, 2),
            Some((5, 6))
        );
    }

    #[test]
    fn mixed_mono_and_stereo_buses_map_to_real_boundaries() {
        // bus0 stereo (ch1,2), bus1 mono (ch3), bus2 mono (ch4).
        let counts = [2u8, 1, 1];
        assert_eq!(
            vsti_output_child_channels_for_bus_layout(&counts, 0),
            Some((1, 2))
        );
        assert_eq!(
            vsti_output_child_channels_for_bus_layout(&counts, 1),
            Some((3, 3))
        );
        assert_eq!(
            vsti_output_child_channels_for_bus_layout(&counts, 2),
            Some((4, 4))
        );
        assert_eq!(vsti_output_bus_flat_range(&counts, 1), Some((3, 1)));
    }

    #[test]
    fn single_bus_plugin_creates_no_child_strips() {
        // A normal stereo VSTi keeps playing on its instrument track (criteria #28).
        assert!(vsti_output_bus_strip_indices(&[2u8]).is_empty());
        assert!(vsti_output_bus_strip_indices(&[1u8]).is_empty());
    }

    #[test]
    fn single_multichannel_bus_splits_into_flat_stereo_pairs() {
        // Some drum VST3s expose multi-out as one 8-channel bus instead of four
        // stereo buses. The mixer still needs one child strip per audible pair.
        assert_eq!(vsti_output_bus_strip_indices(&[8u8]), vec![0, 1, 2, 3]);
        assert_eq!(
            vsti_output_child_channels_for_bus_layout(&[8u8], 0),
            Some((1, 2))
        );
        assert_eq!(
            vsti_output_child_channels_for_bus_layout(&[8u8], 1),
            Some((3, 4))
        );
        assert_eq!(
            vsti_output_child_channels_for_bus_layout(&[8u8], 2),
            Some((5, 6))
        );
        assert_eq!(
            vsti_output_child_channels_for_bus_layout(&[8u8], 3),
            Some((7, 8))
        );
    }

    #[test]
    fn odd_single_multichannel_bus_duplicates_tail_channel() {
        assert_eq!(vsti_output_bus_strip_indices(&[7u8]), vec![0, 1, 2, 3]);
        assert_eq!(
            vsti_output_child_channels_for_bus_layout(&[7u8], 3),
            Some((7, 7))
        );
    }

    #[test]
    fn buses_past_the_bridge_channel_cap_are_dropped() {
        // 18 mono buses → only the first 16 flat channels can be carried.
        let counts = [1u8; 18];
        let indices = vsti_output_bus_strip_indices(&counts);
        assert_eq!(indices.len(), 16);
        assert_eq!(*indices.last().unwrap(), 15);
    }

    #[test]
    fn unknown_layout_falls_back_to_legacy_stereo_pairing() {
        // Empty layout (host hasn't reported yet) → legacy consecutive pairs.
        assert_eq!(
            vsti_output_child_channels_for_bus_layout(&[], 0),
            Some((1, 2))
        );
        assert_eq!(
            vsti_output_child_channels_for_bus_layout(&[], 1),
            Some((3, 4))
        );
    }
}

/// Cross-channel insert moves: the chain floor, the move itself (the same
/// slot struct, its lanes, exact undo), and project-wide id uniqueness.
#[cfg(test)]
mod insert_move_tests {
    use super::*;
    use crate::components::edit::edit_commands::{EditCommand, EditHistory};

    fn track(state: &mut TimelineState, track_type: TrackType, name: &str) -> String {
        state.create_track(CreateTrackOptions {
            track_type,
            name: name.to_string(),
            color: gpui::Rgba {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 1.0,
            },
            volume: 1.0,
            pan: 0.0,
            armed: false,
            input_monitor: InputMonitorMode::Off,
        })
    }

    /// Load a VST3 into slot `index` of `track_id` with a registry role.
    fn load(
        state: &mut TimelineState,
        track_id: &str,
        index: usize,
        name: &str,
        instrument: bool,
    ) -> String {
        let slot = state.ensure_insert_slot_at(track_id, index).expect("slot");
        state.set_insert_plugin(
            track_id,
            &slot,
            name.to_string(),
            Some(std::path::PathBuf::from(format!("C:/p/{name}.vst3"))),
            InsertPluginFormat::Vst3,
            None,
            name.to_string(),
        );
        state.set_insert_plugin_role(track_id, &slot, instrument);
        slot
    }

    fn param_lane(id: &str, insert_id: &str) -> AutomationLaneState {
        AutomationLaneState::new(
            id,
            AutomationTarget::PluginParameter {
                insert_id: insert_id.to_string(),
                parameter_id: "7".to_string(),
                parameter_name: "Cutoff".to_string(),
            },
        )
    }

    fn track_mut<'a>(state: &'a mut TimelineState, track_id: &str) -> &'a mut TrackState {
        state
            .tracks
            .iter_mut()
            .find(|track| track.id == track_id)
            .expect("track")
    }

    fn empty_state() -> TimelineState {
        let mut state = TimelineState::default();
        state.tracks.clear();
        state
    }

    #[test]
    fn fx_chain_floor_reserves_the_instrument_slot_only() {
        let mut state = empty_state();
        let inst = track(&mut state, TrackType::Instrument, "Inst");
        let midi = track(&mut state, TrackType::Midi, "MIDI");
        let bare_midi = track(&mut state, TrackType::Midi, "Bare MIDI");
        let legacy_midi = track(&mut state, TrackType::Midi, "Legacy MIDI");
        let audio = track(&mut state, TrackType::Audio, "Audio");
        let bus = track(&mut state, TrackType::Bus, "Bus");
        let ret = track(&mut state, TrackType::Return, "Return");

        // An Instrument track reserves slot 0 even with nothing in it — an
        // empty placeholder, or a built-in instrument that is no insert at all.
        assert_eq!(state.fx_chain_floor(&inst), 1);
        track_mut(&mut state, &inst).builtin_soundfont_player = true;
        assert_eq!(state.fx_chain_floor(&inst), 1);

        load(&mut state, &midi, 0, "synth", true);
        assert_eq!(state.fx_chain_floor(&midi), 1, "a MIDI track's VSTi");
        load(&mut state, &bare_midi, 0, "fx", false);
        assert_eq!(
            state.fx_chain_floor(&bare_midi),
            0,
            "an effect is no instrument"
        );
        // A legacy slot with no recorded role runs as the instrument at slot 0.
        let legacy = load(&mut state, &legacy_midi, 0, "old", false);
        state
            .insert_slots_mut(&legacy_midi)
            .unwrap()
            .iter_mut()
            .find(|slot| slot.id == legacy)
            .unwrap()
            .plugin_is_instrument = None;
        assert_eq!(state.fx_chain_floor(&legacy_midi), 1);

        for id in [&audio, &bus, &ret] {
            load(&mut state, id, 0, "fx", false);
            assert_eq!(state.fx_chain_floor(id), 0);
        }
        assert_eq!(state.fx_chain_floor(MASTER_TRACK_ID), 0);
    }

    /// The moved insert is the same struct: nothing is cloned or recreated, so
    /// its opaque state is the very same allocation, and its lanes go with it.
    #[test]
    fn move_insert_between_tracks_keeps_the_same_struct() {
        let mut state = empty_state();
        let a = track(&mut state, TrackType::Audio, "A");
        let b = track(&mut state, TrackType::Audio, "B");
        let fx1 = load(&mut state, &a, 0, "fx1", false);
        let fx2 = load(&mut state, &a, 1, "fx2", false);
        let other = load(&mut state, &b, 0, "other", false);

        let blob = std::sync::Arc::new(vec![1u8, 2, 3, 4]);
        {
            let slot = state
                .insert_slots_mut(&a)
                .unwrap()
                .iter_mut()
                .find(|slot| slot.id == fx2)
                .unwrap();
            slot.vst3_state = Some(blob.clone());
            slot.bypassed = true;
            slot.enabled = false;
            slot.output_bus_channel_counts = vec![2, 2];
            slot.parameters.push(PluginParameterState {
                id: 7,
                name: "Cutoff".to_string(),
                value_normalized: 0.25,
                automatable: true,
                hidden: false,
                read_only: false,
                unit: String::new(),
            });
        }
        let before = state.find_insert_slot(&a, &fx2).unwrap().clone();
        {
            let track_a = track_mut(&mut state, &a);
            track_a.automation_lanes.push(AutomationLaneState::new(
                "vol",
                AutomationTarget::TrackVolume,
            ));
            track_a.automation_lanes.push(param_lane("cut", &fx2));
            track_a.automation_lanes.push(param_lane("fx1", &fx1));
            track_a.selected_automation_target = Some(param_lane("cut", &fx2).target.clone());
        }
        state.last_touched_plugin_param = Some(LastTouchedPluginParam {
            track_id: a.clone(),
            insert_id: fx2.clone(),
            parameter_id: "7".to_string(),
            parameter_name: "Cutoff".to_string(),
            plugin_name: "fx2".to_string(),
            normalized_value: 0.25,
        });

        // Dropped in front of `other`.
        let plan = state.plan_insert_move(&a, &fx2, &b, 0).expect("movable");
        assert!(state.apply_insert_move(&plan));

        assert_eq!(state.insert_order(&a), vec![fx1.clone()]);
        assert_eq!(state.insert_order(&b), vec![fx2.clone(), other.clone()]);
        let moved = state.find_insert_slot(&b, &fx2).unwrap();
        assert_eq!(*moved, before, "every field travels unchanged");
        assert!(
            std::sync::Arc::ptr_eq(moved.vst3_state.as_ref().unwrap(), &blob),
            "the opaque state is the same allocation, not a copy"
        );
        let lanes_a: Vec<_> = state
            .find_track(&a)
            .unwrap()
            .automation_lanes
            .iter()
            .map(|l| l.id.clone())
            .collect();
        let lanes_b: Vec<_> = state
            .find_track(&b)
            .unwrap()
            .automation_lanes
            .iter()
            .map(|l| l.id.clone())
            .collect();
        assert_eq!(lanes_a, vec!["vol", "fx1"]);
        assert_eq!(lanes_b, vec!["cut"]);
        assert_eq!(
            state.find_track(&a).unwrap().selected_automation_target,
            None,
            "the source's focus on a moved lane is cleared"
        );
        assert_eq!(
            state.last_touched_plugin_param.as_ref().unwrap().track_id,
            b
        );
        assert_eq!(state.insert_owner_ids_containing(&fx2), vec![b.clone()]);

        // Undo puts everything back where it was.
        assert!(state.revert_insert_move(&plan));
        assert_eq!(state.insert_order(&a), vec![fx1.clone(), fx2.clone()]);
        assert_eq!(state.insert_order(&b), vec![other]);
        let lanes_a: Vec<_> = state
            .find_track(&a)
            .unwrap()
            .automation_lanes
            .iter()
            .map(|l| l.id.clone())
            .collect();
        assert_eq!(lanes_a, vec!["vol", "cut", "fx1"]);
        assert!(state.find_track(&b).unwrap().automation_lanes.is_empty());
        assert_eq!(
            state.find_track(&a).unwrap().selected_automation_target,
            Some(param_lane("cut", &fx2).target)
        );
        assert_eq!(
            state.last_touched_plugin_param.as_ref().unwrap().track_id,
            a
        );
    }

    #[test]
    fn moves_the_chain_cannot_take_are_refused() {
        let mut state = empty_state();
        let inst = track(&mut state, TrackType::Instrument, "Inst");
        let audio = track(&mut state, TrackType::Audio, "Audio");
        let video = track(&mut state, TrackType::Video, "Video");
        let bare_midi = track(&mut state, TrackType::Midi, "MIDI");
        let full = track(&mut state, TrackType::Audio, "Full");
        let synth = load(&mut state, &inst, 0, "synth", true);
        let fx = load(&mut state, &inst, 1, "fx", false);
        for index in 0..MAX_INSERT_SLOTS {
            load(&mut state, &full, index, "filler", false);
        }
        // A VSTi used as an effect on an audio track is still an instrument
        // plug-in: it does not change channels.
        let loose_synth = load(&mut state, &audio, 0, "synth2", true);

        assert!(
            state.plan_insert_move(&inst, &synth, &audio, 0).is_none(),
            "the instrument"
        );
        assert!(state
            .plan_insert_move(&audio, &loose_synth, &inst, 1)
            .is_none());
        assert!(
            state.plan_insert_move(&inst, &fx, &video, 0).is_none(),
            "no effect chain"
        );
        assert!(
            state.plan_insert_move(&inst, &fx, &full, 0).is_none(),
            "no room"
        );
        assert!(
            state.plan_insert_move(&inst, &fx, &bare_midi, 0).is_none(),
            "no instrument"
        );
        assert!(
            state.plan_insert_move(&inst, &fx, &inst, 0).is_none(),
            "same channel"
        );
        assert!(
            state.plan_insert_move(&inst, "nope", &audio, 0).is_none(),
            "unknown id"
        );
        assert!(
            state.plan_insert_move(&inst, &fx, "no-track", 0).is_none(),
            "unknown track"
        );
        assert!(state.plan_insert_move(&inst, &fx, &audio, 0).is_some());
    }

    /// The master keeps no automation lanes, so an insert with parameter
    /// automation cannot go there — its lanes would be lost.
    #[test]
    fn a_move_onto_the_master_is_refused_while_the_insert_has_lanes() {
        let mut state = empty_state();
        let a = track(&mut state, TrackType::Audio, "A");
        let fx = load(&mut state, &a, 0, "fx", false);
        track_mut(&mut state, &a)
            .automation_lanes
            .push(param_lane("cut", &fx));
        assert!(state
            .plan_insert_move(&a, &fx, MASTER_TRACK_ID, 0)
            .is_none());

        track_mut(&mut state, &a).automation_lanes.clear();
        let plan = state
            .plan_insert_move(&a, &fx, MASTER_TRACK_ID, 0)
            .expect("no lanes, no problem");
        assert!(state.apply_insert_move(&plan));
        assert_eq!(state.insert_order(MASTER_TRACK_ID), vec![fx]);
    }

    /// One drop onto an Instrument track with no inserts creates the reserved
    /// slot 0 first; undo removes it again and puts the effect back where it
    /// was, redo recreates the same slot id. One history entry throughout.
    #[test]
    fn move_insert_slot_undo_redo_is_exact_including_the_placeholder() {
        let mut state = empty_state();
        let a = track(&mut state, TrackType::Audio, "A");
        let inst = track(&mut state, TrackType::Instrument, "Inst");
        let fx1 = load(&mut state, &a, 0, "fx1", false);
        let fx2 = load(&mut state, &a, 1, "fx2", false);
        let fx3 = load(&mut state, &a, 2, "fx3", false);

        // Asked for the very front; the floor puts it after the reserved slot.
        let plan = state.plan_insert_move(&a, &fx2, &inst, 0).expect("movable");
        assert_eq!(plan.from_index, 1);
        assert_eq!(plan.to_index, 1);
        let placeholder = plan.placeholder_id.clone().expect("empty instrument track");

        let mut history = EditHistory::new(16);
        let cmd = EditCommand::MoveInsertSlot { plan };
        assert!(cmd.is_channel_chain_edit());
        cmd.execute(&mut state);
        history.push(cmd);
        assert_eq!(
            state.insert_order(&inst),
            vec![placeholder.clone(), fx2.clone()]
        );
        assert!(state
            .find_insert_slot(&inst, &placeholder)
            .unwrap()
            .is_empty());
        assert_eq!(state.insert_order(&a), vec![fx1.clone(), fx3.clone()]);

        assert!(history.undo(&mut state));
        assert_eq!(
            state.insert_order(&a),
            vec![fx1.clone(), fx2.clone(), fx3.clone()]
        );
        assert!(
            state.insert_order(&inst).is_empty(),
            "the placeholder goes too"
        );

        assert!(history.redo(&mut state));
        assert_eq!(state.insert_order(&inst), vec![placeholder, fx2.clone()]);
        assert_eq!(state.insert_order(&a), vec![fx1, fx3]);
        assert!(!history.redo(&mut state), "one drop is one entry");
    }

    /// An insert keeps its id when it moves, so after a reopen its old owner
    /// could otherwise regenerate an id that now lives on another channel.
    #[test]
    fn fresh_insert_ids_are_unique_across_every_channel() {
        let mut state = empty_state();
        let a = track(&mut state, TrackType::Audio, "A");
        let b = track(&mut state, TrackType::Audio, "B");
        let first = state.add_insert(&a).expect("slot");
        let seq: u64 = first.rsplit('-').next().unwrap().parse().unwrap();
        // Track B holds the next ids the counter would hand track A, as if they
        // had been moved there and the project reopened.
        {
            let slots = state.insert_slots_mut(&b).unwrap();
            for next in seq + 1..seq + 400 {
                slots.push(InsertSlotState::empty(format!("insert-{a}-{next}")));
            }
        }
        for _ in 0..8 {
            let fresh = state.add_insert(&a).expect("slot");
            assert_eq!(
                state.insert_owner_ids_containing(&fresh),
                vec![a.clone()],
                "{fresh} must exist on exactly one channel"
            );
        }
    }
}

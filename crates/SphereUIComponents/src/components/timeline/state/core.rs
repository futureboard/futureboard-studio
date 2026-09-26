use super::*;

#[derive(Debug, Clone, PartialEq)]
pub struct TransportState {
    pub playing: bool,
    pub recording: bool,
    pub metronome_enabled: bool,
    pub playhead_beats: f32,
    pub loop_enabled: bool,
    pub loop_start_beats: f32,
    pub loop_end_beats: f32,
    pub last_engine_frame: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MasterBusState {
    pub volume: f32,
    /// Master-bus insert chain. Uses the same slot model as track inserts,
    /// owned by the synthetic `"master"` route in the engine graph.
    pub inserts: Vec<InsertSlotState>,
    pub meter_level_l: f32,
    pub meter_level_r: f32,
    /// Held peak levels (slow release) for the master peak-hold tick. UI-only.
    pub meter_peak_hold_l: f32,
    pub meter_peak_hold_r: f32,
    /// Latched master clip indicator. UI-only.
    pub meter_clip: bool,
    /// Display label for the assigned Master Output connection, resolved from
    /// the registry by [`TimelineState::refresh_output_labels`]. UI-only: the
    /// routing itself is the id in `master_output_connection_id`, so a rename
    /// changes this string and nothing else.
    pub output_label: String,
}

/// Which signal the Control Room monitors. Mirrors the engine's
/// `MonitorSource`; the engine remains authoritative for routing.
///
/// The default is [`MonitorSourceKind::MasterBus`] — the complete internal mix
/// (audio tracks, instruments, aux/returns, group buses, master processing).
/// A hardware input is selectable but never the default, and selecting it is
/// what makes the engine read a capture device at all.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum MonitorSourceKind {
    #[default]
    MasterBus,
    Bus(String),
    TrackPreFader(String),
    TrackAfterFader(String),
    HardwareInput(String),
}

impl MonitorSourceKind {
    /// Stable tag shared with the engine.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::MasterBus => "master",
            Self::Bus(_) => "bus",
            Self::TrackPreFader(_) => "track-pfl",
            Self::TrackAfterFader(_) => "track-afl",
            Self::HardwareInput(_) => "hardware-input",
        }
    }

    pub fn target_id(&self) -> Option<&str> {
        match self {
            Self::MasterBus => None,
            Self::Bus(id)
            | Self::TrackPreFader(id)
            | Self::TrackAfterFader(id)
            | Self::HardwareInput(id) => Some(id.as_str()),
        }
    }
}

/// Control Room bus — the monitoring path fed from the master bus, shown as its
/// own pinned mixer strip.
///
/// Everything here affects playback monitoring only. None of it reaches the
/// master mix, an offline export, a stem export, or recorded audio: the engine
/// applies the Control Room inside the device callback, which export never
/// enters. This is session/hardware state, so it is deliberately not persisted
/// with the project and never marks it dirty.
#[derive(Debug, Clone, PartialEq)]
pub struct MonitorBusState {
    /// What the Control Room listens to when no channel PFL/AFL is engaged.
    pub source: MonitorSourceKind,
    /// Human-readable name of [`Self::source`], resolved against the project
    /// when the selection changes.
    pub source_display: String,
    /// Display label for the Control Room's destination, resolved from the
    /// registry by [`TimelineState::refresh_output_labels`]. UI-only: the
    /// routing is `monitor_output_connection_id` (or Master, when that is
    /// `None`), never a hardware pair held here.
    pub output_name: String,
    /// Monitor level as a normalized fader position.
    pub volume: f32,
    pub mute: bool,
    /// -20 dB monitoring reference cut.
    pub dim: bool,
    /// Fold to mono for mono-compatibility checks.
    pub mono: bool,
    /// Post-monitor-processing level actually leaving for the monitoring
    /// output — after monitor inserts and the control processor. UI-only.
    pub meter_level_l: f32,
    pub meter_level_r: f32,
    pub meter_peak_hold_l: f32,
    pub meter_peak_hold_r: f32,
    pub meter_clip: bool,
    /// True while any channel has PFL/AFL engaged, so the strip can show that
    /// the Control Room is on a Listen tap rather than its selected source.
    pub listen_active: bool,
    /// Whether the Control Room sits in the playback path.
    ///
    /// Compile input for hardware ownership, not a control: while true the
    /// Control Room is the only stage writing hardware, and Master must not
    /// also write its output directly. Studio keeps the Control Room in the
    /// path, so this is `true`; the false branch exists because ownership is a
    /// property of the routing, not an assumption baked into the callback.
    pub control_room_enabled: bool,
}

impl MonitorBusState {
    /// Short label for the current source, shown in the Source selector.
    /// Resolved from the project when the source is set, so the chip shows a
    /// bus name rather than an internal track id.
    pub fn source_label(&self) -> String {
        self.source_display.clone()
    }
}

/// A saved scroll position waiting to be placed.
///
/// Scroll cannot be placed when a session is installed: the viewport has no
/// size yet (so it would be clamped to nothing), and the rows of restored
/// automation lanes are only measured once the arrangement lays out. The
/// Timeline consumes it on the first frame with a real viewport, through
/// [`TimelineState::apply_pending_view_restore`], right before it clamps the
/// scroll to the content. Set from the per-user view sidecar.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PendingViewRestore {
    /// Beat at the left edge; converted to pixels through the time warp.
    pub left_edge_beat: f64,
    pub scroll_y: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TimelineState {
    pub bpm: f32,
    /// Nominal sample rate stored with this project. The audio device may be
    /// running at a different rate while a requested reopen is deferred or when
    /// hardware falls back; runtime code reads the active engine rate separately.
    pub project_sample_rate: u32,
    /// Project-level tempo automation. Always active and owned by the project;
    /// the TempoTrack (when shown) is only a view/editor over this. When empty
    /// the project plays at the static `bpm`.
    pub tempo_map: TempoMap,
    /// The engine tempo map resolved from `tempo_map` + `bpm`. Derived cache —
    /// see [`ResolvedTempo`]; read through
    /// [`TimelineState::resolved_tempo_map`], refreshed by
    /// [`TimelineState::refresh_tempo_cache`].
    pub(crate) resolved_tempo: ResolvedTempo,
    /// Global time signature markers (authoritative for bar/beat layout).
    pub time_signature_map: TimeSignatureMap,
    /// Unit the ruler and every position readout are expressed in.
    ///
    /// Display only: the arrangement's coordinate model stays musical whatever
    /// this is set to, so changing it can never move a clip or a note.
    pub time_display_format: TimeDisplayFormat,
    /// Frame rate used when [`Self::time_display_format`] is
    /// [`TimeDisplayFormat::Timecode`]. Ignored by every other format.
    pub timecode_rate: TimecodeRate,
    /// The project's key: root and scale. `None` until someone sets one — a
    /// project has no key by default, and "C major" would be a guess.
    pub project_key: Option<MidiScale>,
    /// Timeline markers shown on the arrangement ruler.
    pub markers: Vec<TimelineMarkerState>,
    /// Named timeline regions spanning a beat range.
    pub regions: Vec<TimelineRegionState>,
    /// Project-owned chord, lyric, and section events in canonical beat space.
    pub song_text_events: Vec<SongTextEvent>,
    /// UI lookup accelerator rebuilt whenever `song_text_events` changes.
    pub song_text_index: SongTextIndex,
    /// Non-persisted mutation revision used by virtualized Song Text views.
    pub song_text_revision: u64,
    /// Non-persisted revision that takes a fresh value whenever a track's
    /// name changes (rename, its undo and redo) and for every new arrangement
    /// ([`TimelineState::touch_track_names`]). Views that copy names out — the
    /// mixer tree, the pop-out mixer, built-in editor sidebars — key their
    /// caches on it, since a rename never touches the routing graph they
    /// otherwise follow. Compare it for equality only: values are unique in
    /// the process, not consecutive.
    pub track_names_revision: u64,
    /// Legacy single signature — kept in sync with the marker at beat 0 for
    /// templates and engine fallbacks.
    pub time_signature_num: u32,
    pub time_signature_den: u32,
    pub viewport: TimelineViewport,
    pub transport: TransportState,
    pub tracks: Vec<TrackState>,
    pub master: MasterBusState,
    /// Project Audio Connections — the single source of truth for logical
    /// audio input/output buses. Tracks reference entries by stable id; nothing
    /// outside this registry maps a bus to physical ports.
    pub audio_connections: crate::audio_connections::AudioConnectionRegistry,
    /// The project's main logical output. `None` means Master has no hardware
    /// destination — never "the default device". Stores only the stable id, so
    /// renaming or re-patching that bus flows through untouched.
    pub master_output_connection_id: Option<crate::audio_connections::AudioConnectionId>,
    /// Optional Monitor / Control Room output override. `None` means **Follow
    /// Master Output**, which is a real selection rather than an empty one — see
    /// [`crate::output_routing::effective_monitor_output`].
    pub monitor_output_connection_id: Option<crate::audio_connections::AudioConnectionId>,
    /// Latch for the one-time output-routing bootstrap. Persisted so a
    /// deliberately deleted Master output is never recreated on the next launch.
    pub output_routing_initialized: bool,
    /// Input-monitoring bus rendered as the pinned Monitor strip. Session
    /// state — see [`MonitorBusState`].
    pub monitor: MonitorBusState,
    pub selection: TimelineSelection,
    /// What an arrangement marquee in flight would select. The arrangement
    /// draws clips and tracks from it (see [`Self::display_selection`]) while
    /// the rectangle is out; `selection` — what the Inspector, the editors and
    /// every command follow — changes once, when the release commits it.
    /// UI-only, never saved.
    pub marquee_selection_preview: Option<TimelineSelection>,
    pub active_tool: TimelineTool,
    pub snap_to_grid: bool,
    pub grid_division: SnapDivision,
    /// Straight / dotted / triplet shaping applied on top of [`Self::grid_division`].
    pub snap_shape: SnapShape,
    pub dragging_track_id: Option<TrackId>,
    pub drag_origin_index: Option<usize>,
    pub drag_current_y: f32,
    pub drag_target_index: Option<usize>,
    /// True when the timeline viewport should follow the playhead during
    /// playback. Toggled off temporarily when the user manually scrolls or
    /// drags the viewport; can be re-enabled from the Follow button.
    pub follow_playhead: bool,
    /// Follow-playhead is held off for the length of a gesture — a marquee,
    /// so playback cannot page the arrangement from under the rectangle —
    /// without touching the user's `follow_playhead`, which the transport
    /// shows and Settings saves. UI-only, never saved.
    pub follow_playhead_suspended: bool,
    pub auto_scroll_mode: AutoScrollMode,
    /// Arrangement time-range selection in beats. UI-only; never marks the
    /// project or engine dirty by itself.
    pub arrangement_range: Option<TimelineRangeSelection>,
    /// When true, the global Tempo Track lane is shown below the ruler.
    pub show_tempo_track: bool,
    /// Compact collapsed height for the Tempo Track lane header/curve.
    pub tempo_track_collapsed: bool,
    /// Selected tempo marker on the Tempo Track (stable persisted id).
    pub selected_tempo_point_id: Option<String>,
    pub show_time_signature_track: bool,
    pub time_signature_track_collapsed: bool,
    pub selected_time_signature_point_id: Option<String>,
    /// When true, the global Song Text lane is shown below the ruler.
    ///
    /// This lane used to be unconditional — rendered and measured whether or not
    /// the project had any lyrics in it. Tempo and meter apply to every project;
    /// song text does not, so it is now opt-in like the other conductor lanes.
    pub show_song_text_track: bool,
    /// When true, the global Region (arranger) lane is shown below the ruler.
    ///
    /// Regions used to exist only as translucent bars inside the ruler, which
    /// left no room to read a section name, no grab target that was not also
    /// the playhead scrub, and no place to put per-region commands.
    pub show_region_track: bool,
    pub region_track_collapsed: bool,
    /// Selected arrangement region (stable id).
    pub selected_region_id: Option<String>,
    /// Chord Track events, ordered by start beat and never overlapping.
    pub chord_events: Vec<ChordTrackEvent>,
    /// When true, the global Chord Track is shown below the ruler.
    pub show_chord_track: bool,
    pub chord_track_collapsed: bool,
    pub selected_chord_event_id: Option<u64>,
    /// Ghost of a progression being dragged in from the Chord Generator.
    pub chord_drop_preview: Option<ChordDropPreview>,
    /// When true, the global Marker lane is shown below the ruler.
    pub show_marker_track: bool,
    pub marker_track_collapsed: bool,
    /// Selected arrangement marker (stable id).
    pub selected_marker_id: Option<String>,
    /// User-set heights for the global lanes (view state, same class as the
    /// visibility and collapse flags above).
    pub global_lane_heights: GlobalLaneHeights,
    /// Active global-lane height resize gesture, if any.
    pub global_lane_resize: Option<GlobalLaneResizeSession>,
    /// Armed at pointer-down on a global lane's resize handle; promoted to
    /// [`Self::global_lane_resize`] on the first drag-move delta.
    pub global_lane_resize_arm: Option<(GlobalLaneKind, f32)>,
    /// Per-track arrangement row heights (layout/view state, persisted in project).
    pub track_view_layout: TrackViewLayout,
    /// Active track-height resize gesture, if any.
    pub track_height_resize: Option<TrackHeightResizeSession>,
    /// Armed at pointer-down on a resize handle; promoted to
    /// [`Self::track_height_resize`] on the first drag-move delta.
    pub track_height_resize_arm: Option<(String, f32, bool, bool)>,
    /// Last VST3 parameter touched inside a plugin editor (UI-only, not saved).
    pub last_touched_plugin_param: Option<LastTouchedPluginParam>,
    /// Mixer tree sidebar — expanded nodes, pins, hidden channels (persisted).
    pub mixer_tree: MixerTreeViewState,
    /// The mixer tree's default groups were expanded once (persisted latch,
    /// v54), so an empty expanded set is a deliberate collapse-all rather than
    /// a tree that was never set up. See `ensure_timeline_mixer_tree_defaults`.
    pub mixer_tree_initialized: bool,
    /// A reopened project's saved scroll, waiting for the first frame with a
    /// real viewport. See [`PendingViewRestore`].
    pub pending_view_restore: Option<PendingViewRestore>,
    /// Transient fader values while the user drags. UI-only: these do not mark the
    /// project dirty, enter undo history, or trigger engine graph sync. Both the
    /// arrangement track headers and mixer strips render from this cache so they
    /// stay visually locked during a drag.
    pub track_volume_previews: std::collections::HashMap<TrackId, f32>,
    /// Base volume captured at fader pointer-down so commit can emit one undo entry.
    pub track_volume_gesture_origin: std::collections::HashMap<TrackId, f32>,
    pub master_volume_preview: Option<f32>,
    /// Base master volume captured at fader pointer-down for one undo entry.
    pub master_volume_gesture_origin: Option<f32>,
    /// Base pan captured at pointer-down so one completed scrub becomes one
    /// undo entry. Preview updates remain transient until release.
    pub track_pan_gesture_origin: std::collections::HashMap<TrackId, f32>,
}

impl Default for TimelineState {
    /// Clean, empty project. No tracks, no clips, no MIDI — the real runtime
    /// startup state. Use [`TimelineState::demo_project`] when you explicitly
    /// want the seeded demo content (development / screenshots).
    fn default() -> Self {
        Self {
            bpm: 120.0,
            project_sample_rate: 48_000,
            tempo_map: TempoMap::new(),
            resolved_tempo: ResolvedTempo::default(),
            time_signature_map: TimeSignatureMap::with_default_4_4(),
            time_display_format: TimeDisplayFormat::default(),
            timecode_rate: TimecodeRate::default(),
            project_key: None,
            markers: Vec::new(),
            regions: Vec::new(),
            song_text_events: Vec::new(),
            song_text_index: SongTextIndex::default(),
            song_text_revision: 0,
            track_names_revision: fresh_track_names_revision(),
            time_signature_num: 4,
            time_signature_den: 4,
            viewport: TimelineViewport {
                scroll_x: 0.0,
                scroll_y: 0.0,
                target_scroll_x: 0.0,
                target_scroll_y: 0.0,
                pixels_per_second: 150.0,
                pixels_per_beat: 75.0,
                viewport_width: 0.0,
                viewport_height: 500.0,
                track_area_height: 500.0,
                panel_origin_x: 0.0,
                lane_origin_x_measured: None,
                timeline_origin_y_measured: None,
                time_warp: TimeWarp::default(),
            },
            transport: TransportState {
                playing: false,
                recording: false,
                metronome_enabled: false,
                playhead_beats: 0.0,
                loop_enabled: false,
                loop_start_beats: 0.0,
                loop_end_beats: 16.0,
                last_engine_frame: 0,
            },
            tracks: Vec::new(),
            audio_connections: crate::audio_connections::AudioConnectionRegistry::new(),
            master_output_connection_id: None,
            monitor_output_connection_id: None,
            output_routing_initialized: false,
            master: MasterBusState {
                volume: volume::db_to_norm(0.0),
                inserts: Vec::new(),
                meter_level_l: 0.0,
                meter_level_r: 0.0,
                meter_peak_hold_l: 0.0,
                meter_peak_hold_r: 0.0,
                meter_clip: false,
                output_label: crate::output_routing::NO_OUTPUT_LABEL.to_string(),
            },
            monitor: MonitorBusState {
                source: MonitorSourceKind::MasterBus,
                source_display: "Master Bus".to_string(),
                output_name: "Out 1-2".to_string(),
                volume: volume::db_to_norm(0.0),
                mute: false,
                dim: false,
                mono: false,
                meter_level_l: 0.0,
                meter_level_r: 0.0,
                meter_peak_hold_l: 0.0,
                meter_peak_hold_r: 0.0,
                meter_clip: false,
                listen_active: false,
                control_room_enabled: true,
            },
            selection: TimelineSelection {
                selected_track_id: None,
                selected_track_ids: Vec::new(),
                track_selection_anchor_id: None,
                selected_clip_ids: Vec::new(),
                selected_song_text_event_ids: Vec::new(),
            },
            marquee_selection_preview: None,
            active_tool: TimelineTool::Pointer,
            snap_to_grid: true,
            grid_division: SnapDivision::Div1_16,
            snap_shape: SnapShape::Straight,
            dragging_track_id: None,
            drag_origin_index: None,
            drag_current_y: 0.0,
            drag_target_index: None,
            follow_playhead: true,
            follow_playhead_suspended: false,
            auto_scroll_mode: AutoScrollMode::Page,
            arrangement_range: None,
            // Tempo and meter are properties of every project, so both
            // conductor lanes are on by default. Neither seeds a point into its
            // map: the lanes render the *effective* value as an implicit marker
            // instead, because writing an anchor point would make
            // `tempo_has_automation()` true and light the AUTO badge on a
            // project whose tempo is a constant.
            show_tempo_track: true,
            tempo_track_collapsed: false,
            selected_tempo_point_id: None,
            show_time_signature_track: true,
            time_signature_track_collapsed: false,
            selected_time_signature_point_id: None,
            show_song_text_track: false,
            // Structure lanes are on by default for the same reason tempo and
            // meter are: every arrangement has sections and cue points, and a
            // lane that has to be found in a menu before it can be used is a
            // lane most projects never get.
            show_region_track: true,
            region_track_collapsed: false,
            selected_region_id: None,
            chord_events: Vec::new(),
            // Off until a project has chords: an empty harmony lane is noise
            // above every arrangement that never uses it. Dropping chords or
            // opening it from the ruler menu shows it.
            show_chord_track: false,
            chord_track_collapsed: false,
            selected_chord_event_id: None,
            chord_drop_preview: None,
            show_marker_track: true,
            marker_track_collapsed: false,
            selected_marker_id: None,
            global_lane_heights: GlobalLaneHeights::default(),
            global_lane_resize: None,
            global_lane_resize_arm: None,
            track_view_layout: TrackViewLayout::default(),
            track_height_resize: None,
            track_height_resize_arm: None,
            last_touched_plugin_param: None,
            mixer_tree: MixerTreeViewState::default(),
            mixer_tree_initialized: false,
            pending_view_restore: None,
            track_volume_previews: std::collections::HashMap::new(),
            track_volume_gesture_origin: std::collections::HashMap::new(),
            master_volume_preview: None,
            master_volume_gesture_origin: None,
            track_pan_gesture_origin: std::collections::HashMap::new(),
        }
    }
}

// ── Time conversions and coordinate helpers ───────────────────────────────────────

pub const HEADER_WIDTH: f32 = 320.0; // Keep it slightly wider for native controls

pub const RULER_HEIGHT: f32 = 30.0;

pub type TrackId = String;

impl TimelineState {
    pub fn seconds_per_beat(&self) -> f32 {
        60.0 / self.bpm.max(1.0)
    }

    pub fn seconds_to_beats(&self, seconds: f64) -> f32 {
        (seconds * self.bpm.max(1.0) as f64 / 60.0) as f32
    }

    pub fn beats_to_seconds(&self, beats: f32) -> f32 {
        beats * self.seconds_per_beat()
    }

    /// Y offset from the timeline top to the track-list content area.
    pub fn arrangement_content_top(&self) -> f32 {
        RULER_HEIGHT + self.global_lanes_height()
    }

    /// Place a [`PendingViewRestore`] once the viewport has a width: the left
    /// edge beat through the same warp the ruler draws with, and the smooth
    /// scroll targets with it, or the next frame would animate back to where
    /// the view was. The caller clamps to the content right after. Returns
    /// `true` when one was applied.
    pub fn apply_pending_view_restore(&mut self) -> bool {
        if self.viewport.viewport_width <= 0.0 {
            return false;
        }
        let Some(pending) = self.pending_view_restore.take() else {
            return false;
        };
        let viewport = &mut self.viewport;
        let x = viewport.time_warp.content_x(
            pending.left_edge_beat.max(0.0),
            viewport.pixels_per_second,
            viewport.pixels_per_beat,
        ) as f32;
        let y = pending.scroll_y.max(0.0);
        viewport.scroll_x = x;
        viewport.target_scroll_x = x;
        viewport.scroll_y = y;
        viewport.target_scroll_y = y;
        true
    }
}

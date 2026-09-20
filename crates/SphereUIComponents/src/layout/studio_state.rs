use std::path::PathBuf;

use crate::overlay::OverlayAnchor;

/// Top-menu open state. `open_menu_id` is the manifest menu id currently
/// showing its dropdown; `anchor` is the label rect used to position the panel.
#[derive(Debug, Clone, Default)]
pub struct MenuBarUiState {
    pub open_menu_id: Option<String>,
    pub anchor: OverlayAnchor,
    /// Nested submenu ids open underneath the root dropdown. `path[0]` is
    /// the submenu open in the root panel, `path[1]` in *that* submenu's
    /// panel, etc.
    pub submenu_path: Vec<String>,
}

use super::context_menu_ops::ContextMenuRequest;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WorkspaceActivePanel {
    Arrangement,
    Mixer,
    Editor,
    Browser,
    Inspector,
    PianoRoll,
    Automation,
    EffectEditor,
    ChordDisplay,
    LyricDisplay,
    LyricEditor,
    Solfege,
}

impl WorkspaceActivePanel {
    pub fn label(self) -> &'static str {
        match self {
            WorkspaceActivePanel::Arrangement => "Arrangement",
            WorkspaceActivePanel::Mixer => "Mixer",
            WorkspaceActivePanel::Editor => "Editor",
            WorkspaceActivePanel::Browser => "Browser",
            WorkspaceActivePanel::Inspector => "Inspector",
            WorkspaceActivePanel::PianoRoll => "Piano Roll",
            WorkspaceActivePanel::Automation => "Automation",
            WorkspaceActivePanel::EffectEditor => "Effect Editor",
            WorkspaceActivePanel::ChordDisplay => "Chord Display",
            WorkspaceActivePanel::LyricDisplay => "Lyric Display",
            WorkspaceActivePanel::LyricEditor => "Lyric Editor",
            WorkspaceActivePanel::Solfege => "Solfege",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RightDockTab {
    Inspector,
    ChordDisplay,
    LyricDisplay,
    LyricEditor,
    Solfege,
}

impl RightDockTab {
    pub fn label(self) -> &'static str {
        match self {
            Self::Inspector => "Inspector",
            Self::ChordDisplay => "Chords",
            Self::LyricDisplay => "Lyrics",
            Self::LyricEditor => "Lyric Editor",
            Self::Solfege => "Solfege",
        }
    }

    pub fn active_panel(self) -> WorkspaceActivePanel {
        match self {
            Self::Inspector => WorkspaceActivePanel::Inspector,
            Self::ChordDisplay => WorkspaceActivePanel::ChordDisplay,
            Self::LyricDisplay => WorkspaceActivePanel::LyricDisplay,
            Self::LyricEditor => WorkspaceActivePanel::LyricEditor,
            Self::Solfege => WorkspaceActivePanel::Solfege,
        }
    }
}

#[derive(Debug, Clone)]
pub enum OpenPopover {
    Context {
        request: ContextMenuRequest,
    },
    /// Searchable automation target picker (instrument → effects → track).
    AutomationTargetPicker {
        track_id: String,
        x: f32,
        y: f32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TextMenuTarget {
    CommandPalette,
    ProjectSwitcherSearch,
    BrowserSearch,
    PluginPickerSearch,
    AutomationPickerSearch,
    /// The Inspector's track-name edit field.
    InspectorName,
    /// The Inspector's clip-name edit field.
    InspectorClipName,
    /// The Inspector's hex colour edit field.
    InspectorColorHex,
    /// The transport readout's inline BPM editor.
    TransportBpm,
    /// The transport readout's inline time-signature numerator.
    TransportTimeSigNum,
    /// The transport readout's inline time-signature denominator.
    TransportTimeSigDen,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct TextContextMenu {
    pub(super) target: TextMenuTarget,
    pub(super) x: f32,
    pub(super) y: f32,
}

/// Transient overlay state — the text-field context menu, the open popover, and
/// the inspector routing combo (dropdown + its screen anchor). `StudioLayout`
/// decomposition slice (all Option → derived `Default`).
#[derive(Default)]
pub(super) struct OverlayState {
    /// Open text-field context menu (cut/copy/paste), if any.
    pub(super) text_context_menu: Option<TextContextMenu>,
    /// Open popover (context menu / dropdown), if any.
    pub(super) open_popover: Option<OpenPopover>,
    /// Open inspector routing-combo dropdown, if any.
    pub(super) inspector_routing_combo: Option<crate::components::panel::InspectorRoutingCombo>,
    /// Screen anchor for the open inspector routing combo.
    pub(super) inspector_routing_combo_anchor: Option<crate::overlay::OverlayAnchor>,
    /// Status-bar performance metrics detail popover.
    pub(super) perf_metrics_popover_open: bool,
    /// Text field to focus after the next render pass mounts the Inspector.
    pub(super) pending_text_focus: Option<TextMenuTarget>,
}

#[derive(Debug, Clone)]
pub enum ContextTarget {
    TimelineEmpty,
    TrackLane {
        track_id: String,
        beat: f64,
    },
    Track(String),
    Clip(String),
    TimelineMarker {
        marker_id: String,
        beat: f64,
    },
    SongTextMarker {
        event_id: String,
        beat: f64,
    },
    AutomationLane {
        track_id: String,
        lane_id: String,
        beat: f64,
    },
    Browser(Option<PathBuf>),
    Mixer(String),
    SendPicker {
        track_id: String,
    },
    /// Output-routing picker opened from a mixer strip's OUT dropdown button.
    /// Lists Main plus the project's Bus/Return targets, wired to
    /// `set_track_output_routing` (the same mutation the Inspector drives).
    OutputPicker {
        track_id: String,
    },
    /// Master Output picker opened from the Master strip. Lists Output Audio
    /// Connections only; selecting one stores nothing but its stable id.
    MasterOutputPicker,
    /// Monitor / Control Room Output picker. Leads with Follow Master Output,
    /// which is a real selection rather than an empty one.
    MonitorOutputPicker,
    /// Automation target picker opened from a track's automation control lane.
    AutomationTargetPicker {
        track_id: String,
    },
    /// The compact tempo menu opened from the transport BPM display.
    Tempo,
    /// Tap Tempo control menu (apply / add marker / reset).
    TapTempo,
    /// Record count-in duration dropdown from the transport control.
    CountIn,
    /// The compact time signature menu from the transport display.
    TimeSignature,
    /// Right-click on a time signature marker on the ruler or lane.
    TimeSignaturePoint {
        point_id: String,
        beat: f64,
    },
    /// Right-click on the timeline ruler. Carries the beat under the cursor so
    /// tempo/time-signature actions are position-aware.
    TimelineRuler {
        beat: f64,
    },
    /// Right-click on the global Marker lane. `marker_id` is `None` on empty
    /// lane, which is what splits "act on this marker" from "create one here".
    MarkerTrack {
        beat: f64,
        marker_id: Option<String>,
    },
    /// Marker lane header menu — lane-level actions only.
    MarkerLane,
    /// Right-click on the global Region (arranger) lane.
    RegionTrack {
        beat: f64,
        region_id: Option<String>,
    },
    /// Region lane header menu — lane-level actions only.
    RegionLane,
    /// Right-click on the global Tempo Track lane.
    TempoTrack {
        beat: f64,
        bpm: f64,
        point_id: Option<String>,
    },
    /// Right-click on the global Time Signature Track lane.
    TimeSignatureTrack {
        beat: f64,
        point_id: Option<String>,
    },
}

/// Which docked studio panels are visible in the main window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StudioPanelVisibility {
    pub browser: bool,
    pub inspector: bool,
    /// The bottom dock, whichever tab it is showing. It was called
    /// `mixer_docked`, which is how the chrome ended up treating the dock as
    /// the Mixer's own panel and left the Editor tab with no toggle of its own.
    pub bottom_docked: bool,
}

impl Default for StudioPanelVisibility {
    fn default() -> Self {
        Self {
            browser: true,
            inspector: true,
            bottom_docked: true,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) enum TransportCommand {
    PlayPause,
    Stop,
    ReturnToStart,
    ToggleLoop,
    ToggleMetronome,
    ToggleFollowPlayhead,
    /// Switch the timeline auto-scroll behavior between paged and continuous.
    ToggleAutoScrollMode,
    Record,
}

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
    /// Metronome volume / settings menu from the transport control.
    Metronome,
    /// The compact time signature menu from the transport display.
    TimeSignature,
    /// Key root picker from the transport's key readout.
    ProjectKeyRoot,
    /// Scale picker from the transport's key readout.
    ProjectKeyScale,
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
    /// The arrangement grid dropdown opened from the ruler's grid button.
    SnapGrid,
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
    /// Right-click on the empty Chord Track (a chord is deleted outright).
    ChordTrack {
        beat: f64,
        event_id: Option<u64>,
    },
    /// Chord Track header menu — lane-level actions only.
    ChordLane,
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

/// Widths of the two side docks in logical pixels: the browser on the left and
/// the right dock (Inspector and its sibling tabs). Each is resized by
/// dragging its inner edge and saved with the workspace layout.
///
/// The browser width is load-bearing beyond its own column: it is the
/// arrangement's `panel_origin_x`, so every change goes through
/// `sync_timeline_chrome_metrics` before the next hit test.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DockWidths {
    pub browser: f32,
    pub inspector: f32,
}

impl DockWidths {
    /// Narrowest browser that still fits its six-tab strip.
    pub const BROWSER_MIN: f32 = 240.0;
    pub const BROWSER_MAX: f32 = 640.0;
    /// The Inspector's rows are laid out for its original width, so the dock
    /// can grow from there but not shrink below it.
    pub const INSPECTOR_MIN: f32 = crate::components::panel::INSPECTOR_WIDTH;
    pub const INSPECTOR_MAX: f32 = 560.0;
    /// Room the arrangement always keeps when either dock grows.
    pub const ARRANGEMENT_MIN: f32 = 320.0;

    pub fn clamp_browser(width: f32) -> f32 {
        if width.is_finite() {
            width.clamp(Self::BROWSER_MIN, Self::BROWSER_MAX)
        } else {
            crate::components::SIDEBAR_WIDTH
        }
    }

    pub fn clamp_inspector(width: f32) -> f32 {
        if width.is_finite() {
            width.clamp(Self::INSPECTOR_MIN, Self::INSPECTOR_MAX)
        } else {
            crate::components::panel::INSPECTOR_WIDTH
        }
    }
}

impl Default for DockWidths {
    fn default() -> Self {
        Self {
            browser: crate::components::SIDEBAR_WIDTH,
            inspector: crate::components::panel::INSPECTOR_WIDTH,
        }
    }
}

/// Which dock edge is being dragged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DockSide {
    Browser,
    Inspector,
}

/// A dock-edge drag in progress: where it started and from what width.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DockResize {
    pub side: DockSide,
    pub start_x: f32,
    pub start_width: f32,
}

/// Drag payload for a dock edge. Carries nothing: the layout holds the state.
#[derive(Debug, Clone, Copy, Default)]
pub struct DockResizeDrag;

impl gpui::Render for DockResizeDrag {
    fn render(
        &mut self,
        _window: &mut gpui::Window,
        _cx: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        gpui::Empty
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

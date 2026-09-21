mod methods;
mod render;
pub(crate) use render::*;

use crate::assets;
use crate::components::edit::{
    ClipSnapshot, EditCommand, EditHistory, EditImpact, TempoStateSnapshot,
    TimeSignatureStateSnapshot, normalize_range,
};
use crate::components::sidebar::BrowserDragItem;
use crate::components::timeline::floating_tools_bar::floating_tools_bar;
use crate::components::timeline::global_lane_header::{
    GlobalLaneResizeArmCb, GlobalLaneResizeResetCb,
};
use crate::components::timeline::marker_track::marker_track_lane;
use crate::components::timeline::region_track::region_track_lane;
use crate::components::timeline::song_text_track::{
    SongTextDragPreview, SongTextDragSession, SongTextMarkerDown, song_text_drag_positions,
    song_text_track_lane,
};
use crate::components::timeline::tempo_track::tempo_track_lane;
use crate::components::timeline::time_signature_track::time_signature_track_lane;
use crate::components::timeline::timeline_ruler::{
    LaneOriginProbe, TimelineLoopDragUpdate, TimelineRegionDrag, TimelineRegionDragUpdate,
    timeline_ruler,
};
use crate::components::timeline::timeline_state::{
    ArrangementCoordinateContext, ArrangementHitTarget, ClipDragItem, ClipResizeDrag, ClipState,
    ClipType, DEFAULT_TRACK_HEIGHT, GlobalLaneKind, GlobalLaneResizeDrag, HEADER_WIDTH,
    RULER_HEIGHT, SnapDivision, TempoPointDrag, TimeSignaturePointDrag, TimelineMarkerDrag,
    TimelineMarkerState, TimelineRangeSelection, TimelineRegionState, TimelineState, TimelineTool,
    TrackDragItem, TrackHeightResizeDrag, TrackType, hit_test_arrangement,
};
use crate::components::timeline::track_list::track_list;
use crate::theme::Colors;
use gpui::prelude::FluentBuilder;
use gpui::{
    Animation, AnimationExt, AppContext, Context, Empty, ExternalPaths, InteractiveElement,
    IntoElement, ParentElement, PinchEvent, Render, ScrollDelta, StatefulInteractiveElement,
    Styled, Subscription, Window, div, pulsating_between, px, svg,
};
use std::time::Duration;

/// Top chrome (titlebar band + transport bar) — converts window-space y into
/// the timeline track area. Single definition in `shell_metrics`; this used to
/// be one of three hand-mirrored copies.
use crate::shell_metrics::APP_CHROME_HEIGHT;
const MARQUEE_DRAG_THRESHOLD: f32 = 4.0;

/// Sizes of the surrounding chrome panels that the timeline's scroll/grid
/// math has to subtract from the window to know the actual timeline body
/// rect. Pushed by `StudioLayout` each render so resizing the bottom
/// panel, toggling browser/inspector, and maximizing the window all stay
/// in sync — no hardcoded constants.
#[derive(Clone, Copy, Debug, Default)]
pub struct TimelineChromeMetrics {
    pub browser_width: f32,
    pub inspector_width: f32,
    pub bottom_panel_height: f32,
    pub status_bar_height: f32,
}

/// Live pen-tool MIDI clip draw. Held only while the gesture is in flight
/// (mouse-down → mouse-up); the real clip is created once on release. `start_beat`
/// is snapped at mouse-down; `current_beat` tracks the snapped cursor while
/// dragging so the ghost preview and the committed clip share one set of bounds.
#[derive(Clone, Debug)]
struct ClipDrawPreview {
    track_id: String,
    start_beat: f32,
    current_beat: f32,
    /// `true` once the cursor has moved past the start — distinguishes a plain
    /// click (default-length clip) from a drag (sized clip).
    dragging: bool,
    /// Whether a plain click (no drag) should still commit a default-length
    /// clip. Always `true` for the Pen tool. For the Pointer tool's
    /// empty-lane-creates-a-clip gesture, a plain single click is a no-op
    /// (matches modern DAW marquee-vs-create conventions) — only a drag or a
    /// double-click (`click_count >= 2`) commits.
    commit_on_click: bool,
}

#[derive(Clone, Debug)]
struct RangeSelectDrag {
    start_beat: f32,
    current_beat: f32,
    start_track_id: String,
    additive: bool,
    dragging: bool,
}

#[derive(Clone, Debug)]
struct FileDropHint {
    position: gpui::Point<gpui::Pixels>,
    label: &'static str,
}

/// UI-only ghost for Alt-drag clip cloning. The real duplicate is still
/// created only on drop; this state exists solely to show its exact target
/// track and snapped start before committing the edit command.
#[derive(Clone, Debug)]
struct ClipCloneHint {
    clip_id: String,
    target_track_index: usize,
    start_beat: f32,
}

fn is_supported_audio_ext(path: &std::path::Path) -> bool {
    matches!(
        path.extension()
            .and_then(|s| s.to_str())
            .map(|s| s.to_ascii_lowercase())
            .as_deref(),
        Some("wav")
            | Some("wave")
            | Some("mp3")
            | Some("flac")
            | Some("ogg")
            | Some("oga")
            | Some("m4a")
            | Some("aiff")
            | Some("aif")
    )
}

fn is_supported_midi_ext(path: &std::path::Path) -> bool {
    matches!(
        path.extension()
            .and_then(|s| s.to_str())
            .map(|s| s.to_ascii_lowercase())
            .as_deref(),
        Some("mid") | Some("midi")
    )
}

use std::collections::HashSet;

pub struct Timeline {
    pub state: TimelineState,
    edit_history: EditHistory,
    /// Exact clip snapshot captured at Inspector scrub start. Preview changes
    /// stay out of history; release records one `UpdateClip` command.
    inspector_clip_gesture_origin: Option<ClipSnapshot>,
    on_seek_beats:
        Option<std::sync::Arc<dyn Fn(f32, f32, crate::layout::SeekReason) + Send + Sync + 'static>>,
    on_track_param_change:
        Option<std::sync::Arc<dyn Fn(String, String, f32) + Send + Sync + 'static>>,
    on_track_input_state_change: Option<
        std::sync::Arc<dyn Fn(String, bool, bool) -> Result<(), String> + Send + Sync + 'static>,
    >,
    on_project_changed: Option<TimelineProjectChangedCb>,
    /// Fired for persisted MIDI edits so the owner can publish the new note
    /// schedule immediately rather than waiting for gesture throttling.
    on_midi_changed: Option<TimelineProjectChangedCb>,
    /// Fired for live mixer-control edits (track mute/solo from the header)
    /// that are persisted in the project but reach the engine through the
    /// realtime command path — the owner marks view-only dirty instead of the
    /// full engine-graph dirty that `on_project_changed` implies.
    on_control_state_changed: Option<TimelineProjectChangedCb>,
    on_loop_changed: Option<TimelineProjectChangedCb>,
    on_tempo_map_changed: Option<TimelineProjectChangedCb>,
    on_time_signature_map_changed: Option<TimelineProjectChangedCb>,
    on_media_changed: Option<TimelineProjectChangedCb>,
    on_add_track: Option<TimelineAddTrackCb>,
    on_plugin_preset_drop: Option<TimelinePluginPresetDropCb>,
    on_plugin_drag_drop: Option<TimelinePluginDragDropCb>,

    /// Asks the owner to confirm what a dropped MIDI file should bring in
    /// besides its notes. Unset (tests, embedded editors) imports everything,
    /// which is what a drop did before the dialog existed.
    on_midi_import_prompt: Option<TimelineMidiImportPromptCb>,
    /// Window-space position of the last drag-move event while files are
    /// being dragged. We need this because `on_drop::<ExternalPaths>` does
    /// not carry the drop position itself — gpui translates the submit into
    /// a synthetic MouseUp, so we have to remember the last cursor position
    /// observed during the drag.
    last_drag_position: Option<gpui::Point<gpui::Pixels>>,
    file_drop_hint: Option<FileDropHint>,
    clip_clone_hint: Option<ClipCloneHint>,
    /// UI-only Song Text positions calculated from the drag-start snapshot.
    song_text_drag_preview: Option<SongTextDragPreview>,
    /// Blocks further drag-move/drop events after Escape or focus-loss cancellation.
    song_text_drag_cancelled: bool,
    clip_drag_origin: Option<gpui::Point<gpui::Pixels>>,
    /// Pre-gesture clip snapshot for the in-flight edge-resize, captured on the
    /// first drag-move (before any mutation) so the drop can record one exact
    /// undo step. Kept here rather than inside [`ClipResizeDrag`] so the drag
    /// payload — rebuilt for every clip on every repaint — stays identity-only.
    clip_resize_origin: Option<ClipSnapshot>,
    clip_drag_target_track_index: Option<usize>,
    clip_clone_drag_id: Option<String>,
    /// Pen-tool click-drag MIDI clip preview, live until mouse-up creates the clip.
    pen_clip_draw: Option<ClipDrawPreview>,
    /// Pointer-tool empty-lane marquee. Rule: Pointer + empty lane drag starts
    /// replace-marquee; Ctrl/Cmd + Pointer + empty lane drag starts additive
    /// marquee. Ctrl/Cmd wins over a MIDI or instrument lane's instant-create
    /// gesture, so those tracks can rubber-band too. Clips, rulers, toolbar
    /// controls, and non-pointer tools never start this gesture.
    range_select_drag: Option<RangeSelectDrag>,
    /// Right-drag erase: clip ids already queued for deletion this gesture.
    erase_clip_drag: Option<HashSet<String>>,
    /// Live preview of clip ids marked for erase (mirrors `erase_clip_drag`).
    erase_preview_ids: HashSet<String>,
    /// In-flight automation point move. Mutated live; committed once on release.
    automation_drag: Option<crate::components::timeline::timeline_state::AutomationPointDrag>,
    /// In-flight automation curve-tension drag (Alt+drag on a segment). Mutated
    /// live; committed once on release. Never moves the points themselves.
    automation_curve_drag: Option<crate::components::timeline::timeline_state::AutomationCurveDrag>,
    /// In-flight automation marquee (rubber-band) selection. UI-only.
    automation_marquee: Option<crate::components::timeline::timeline_state::AutomationMarquee>,
    /// Hovered automation point / curve segment under the cursor. UI-only; drives
    /// the per-segment highlight + hover cursor. Self-corrects on mouse-move and
    /// is cleared on hover-out, so it is never persisted or reset on gesture end.
    automation_hover: Option<crate::components::timeline::timeline_state::AutomationHover>,
    /// Automation control-lane actions that need studio-level handling (picker,
    /// clear-all confirmation, last-touched). Set by `StudioLayout`.
    on_automation_control:
        Option<crate::components::timeline::automation_control_lane::AutomationControlCallback>,
    /// In-flight tempo-point drag on the global Tempo Track lane.
    tempo_drag: Option<TempoPointDrag>,
    /// Pre-gesture tempo snapshot for the in-flight Tempo lane drag, plus the
    /// history label the release should use (a double-click *creates* a marker
    /// and then drags it — one gesture, but not an "edit"). Kept here rather
    /// than inside [`TempoPointDrag`] so the drag payload stays identity-only.
    tempo_gesture_origin: Option<(&'static str, TempoStateSnapshot)>,
    /// Wall-clock positions of the Linear-timebase clips as they stood when the
    /// Tempo lane drag began. A Linear track holds its place on the clock while
    /// the marker moves under it, and that can only be read *before* the map
    /// changes — see [`crate::components::timeline::timeline_state::TimelineState::capture_linear_clip_anchors`].
    tempo_gesture_linear_anchors:
        Vec<crate::components::timeline::timeline_state::LinearClipAnchor>,
    /// In-flight time-signature marker drag on the global Time Signature lane.
    ts_drag: Option<TimeSignaturePointDrag>,
    /// In-flight marker move on the global Marker lane. A root-driven gesture
    /// session like `tempo_drag` and `ts_drag`, not a GPUI drag payload — see
    /// [`TimelineMarkerDrag`].
    marker_drag: Option<TimelineMarkerDrag>,
    /// Pre-gesture meter snapshot for the in-flight Time Signature lane drag,
    /// with its history label. Mirrors `tempo_gesture_origin`.
    ts_gesture_origin: Option<(&'static str, TimeSignatureStateSnapshot)>,
    /// Pre-gesture region snapshot, captured on the first region drag-move
    /// (before any mutation) so the drop records one undo step for the whole
    /// drag instead of one per mouse-move.
    region_gesture_origin: Option<Vec<TimelineRegionState>>,
    /// Pre-gesture marker snapshot, for the Marker lane's flag drag. Mirrors
    /// `region_gesture_origin`.
    marker_gesture_origin: Option<Vec<TimelineMarkerState>>,
    pan_last_position: Option<gpui::Point<gpui::Pixels>>,
    /// View-only floating-toolbar placement. It deliberately never enters the
    /// project snapshot: moving tools must not make a session dirty.
    floating_toolbar_position: Option<(f32, f32)>,
    floating_toolbar_drag_anchor: Option<(gpui::Point<gpui::Pixels>, (f32, f32))>,
    on_context_menu: Option<TimelineContextMenuCb>,
    on_playhead_scrub_begin:
        Option<std::sync::Arc<dyn Fn(&mut gpui::Window, &mut gpui::App) + Send + Sync + 'static>>,
    on_playhead_scrub_end:
        Option<std::sync::Arc<dyn Fn(&mut gpui::Window, &mut gpui::App) + Send + Sync + 'static>>,
    /// Invoked when the user double-clicks a MIDI clip — `StudioLayout` uses it
    /// to switch the bottom panel to the piano-roll Editor tab.
    on_open_editor: Option<TimelineOpenEditorCb>,
    /// Invoked when the user double-clicks an existing Tempo Track marker —
    /// `StudioLayout` uses it to open the inline BPM editor targeting that
    /// marker, instead of the playhead-effective tempo.
    on_tempo_point_edit: Option<TempoPointEditCb>,
    /// Invoked when the user double-clicks a Song Text marker.
    on_open_song_text_editor: Option<TimelineOpenEditorCb>,
    /// Opens the selected instrument track's plugin editor (or the picker).
    on_open_track_instrument: Option<TimelineOpenTrackInstrumentCb>,
    chrome_metrics: TimelineChromeMetrics,
    /// Window x of the lane content column, measured from the ruler each frame
    /// and folded into the viewport at the top of the next render. Pointer math
    /// then resolves through the same origin the pixels were drawn at, instead
    /// of through chrome constants that go stale whenever a panel moves.
    lane_origin_probe: LaneOriginProbe,
    /// The ruler press in flight (scrub / loop edit / loop create). Owned here,
    /// not rebuilt per render, so the classification made at mouse-down survives
    /// the re-render GPUI triggers before the first drag-move — see
    /// [`timeline_ruler`] and `RulerGesture`.
    ruler_gesture:
        std::rc::Rc<std::cell::Cell<crate::components::timeline::timeline_ruler::RulerGesture>>,
    /// Where the playhead is this frame. Written by `render` (which is the only
    /// place that knows the current scroll and zoom) and by the playback poll
    /// between renders; read by the overlay entity below.
    playhead_frame: crate::components::timeline::playhead::PlayheadFrameCell,
    /// The playhead's own entity, built on first render.
    ///
    /// The line has to move at the display rate while the transport runs, and
    /// notifying `Timeline` for it rebuilt the whole arrangement per frame.
    /// Isolated here, a playback tick repaints one line.
    playhead_overlay: Option<gpui::Entity<crate::components::timeline::playhead::PlayheadOverlay>>,
    /// One meter entity per track header, so a level change repaints a bar
    /// instead of the arrangement. Built and pruned by `render`, fed by the
    /// audio poll.
    track_meters: crate::components::timeline::vu_meter::TrackMeterViews,
    /// The arrangement grid's own entity, built on first render and rendered
    /// through `AnyView::cached`. Keeps the grid out of playhead and meter
    /// frames; see `timeline_surface::TimelineSurfaceView`.
    arrangement_surface:
        Option<gpui::Entity<crate::components::timeline::timeline_surface::TimelineSurfaceView>>,
    /// One cached view per track clip lane. Built and pruned by `render` next
    /// to the meters; see `track_lane_view`.
    track_lanes: crate::components::timeline::track_lane_view::TrackLaneViews,
    /// This frame's lane inputs, published by `render` for the lane views to
    /// render through. `None` before the first render.
    frame_lane_ctx:
        Option<std::rc::Rc<crate::components::timeline::track_lane_view::LaneFrameContext>>,
    /// Absolute root folder of the saved project, pushed by `StudioLayout` each
    /// render. `None` for an Untitled (unsaved) project. Used to eagerly copy
    /// dropped audio into the project's `Assets/Audio` folder.
    project_root: Option<std::path::PathBuf>,
    focus_lost_subscription: Option<Subscription>,
}

pub type TimelineOpenEditorCb = std::sync::Arc<dyn Fn(&mut gpui::Window, &mut gpui::App) + 'static>;

pub type TimelineOpenTrackInstrumentCb =
    std::sync::Arc<dyn Fn(&String, &mut gpui::Window, &mut gpui::App) + 'static>;

pub type TempoPointEditCb =
    std::sync::Arc<dyn Fn(&str, &mut gpui::Window, &mut gpui::App) + 'static>;

#[derive(Clone, Debug)]
pub enum TimelineContextTarget {
    TimelineEmpty,
    TrackLane {
        track_id: String,
        beat: f64,
    },
    TrackHeader(String),
    AudioClip {
        track_id: String,
        clip_id: String,
        beat: f64,
        local_beat: f64,
    },
    MidiClip {
        track_id: String,
        clip_id: String,
        beat: f64,
        local_beat: f64,
    },
    Clip(String),
    Marker {
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
    /// Right-click on the arrangement ruler. Carries the beat under the cursor.
    Ruler(f64),
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
    /// Right-click on the global Marker lane. `marker_id` is `None` on empty
    /// lane, which is what splits "act on this marker" from "create one here".
    MarkerTrack {
        beat: f64,
        marker_id: Option<String>,
    },
    /// Right-click on the global Region (arranger) lane.
    RegionTrack {
        beat: f64,
        region_id: Option<String>,
    },
    /// Lane header menu button on the Tempo track.
    TempoLaneHeader,
    /// Lane header menu button on the Time Signature track.
    TimeSignatureLaneHeader,
    /// Lane header menu button on the Marker track.
    MarkerLaneHeader,
    /// Lane header menu button on the Region track.
    RegionLaneHeader,
    /// Automation target picker opened from the control lane "+ Add" button.
    AutomationTargetPicker {
        track_id: String,
    },
}

pub type TimelineContextMenuCb = std::sync::Arc<
    dyn Fn(&(TimelineContextTarget, f32, f32), &mut gpui::Window, &mut gpui::App) + 'static,
>;

#[derive(Clone, Copy, Debug)]
pub struct TimelineAddTrackRequest {
    pub track_count: usize,
    pub has_master_track: bool,
}

pub type TimelineAddTrackCb =
    std::sync::Arc<dyn Fn(&TimelineAddTrackRequest, &mut gpui::Window, &mut gpui::App) + 'static>;

pub type TimelinePluginPresetDropCb = std::sync::Arc<
    dyn Fn(&(std::path::PathBuf, String), &mut gpui::Window, &mut gpui::App) + 'static,
>;
pub type TimelinePluginDragDropCb = std::sync::Arc<
    dyn Fn(
            &crate::components::plugin_picker::PluginDragItem,
            &str,
            &mut gpui::Window,
            &mut gpui::App,
        ) + 'static,
>;

/// A dropped MIDI file that carries optional payload (markers, controller
/// lanes, SysEx), parked until the user says what to bring in.
///
/// Carries the resolved lane coordinates rather than relying on
/// `last_drag_position`, which is cleared as soon as the drop is handled — the
/// clip still has to land where it was dropped when the dialog closes.
#[derive(Debug, Clone, PartialEq)]
pub struct TimelineMidiImportPrompt {
    pub path: std::path::PathBuf,
    pub file_name: String,
    pub summary: crate::components::timeline::midi_import::MidiImportSummary,
    pub drop_x: f32,
    pub drop_y: f32,
}

pub type TimelineMidiImportPromptCb =
    std::sync::Arc<dyn Fn(&TimelineMidiImportPrompt, &mut gpui::Window, &mut gpui::App) + 'static>;

pub type TimelineProjectChangedCb = std::sync::Arc<dyn Fn(&mut gpui::App) + 'static>;

#[derive(Clone, Debug)]
struct ScrollbarDrag {
    axis: ScrollAxis,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ScrollAxis {
    Horizontal,
    Vertical,
}

impl Render for ScrollbarDrag {
    fn render(&mut self, _w: &mut Window, _cx: &mut gpui::Context<Self>) -> impl IntoElement {
        Empty
    }
}

// Clip edge-resize uses GPUI's drag system with no visible drag image, so the
// payload renders as `Empty` (same as the scrollbar thumb drag).
impl Render for ClipResizeDrag {
    fn render(&mut self, _w: &mut Window, _cx: &mut gpui::Context<Self>) -> impl IntoElement {
        Empty
    }
}

impl Render for TrackHeightResizeDrag {
    fn render(&mut self, _w: &mut Window, _cx: &mut gpui::Context<Self>) -> impl IntoElement {
        Empty
    }
}

impl Render for GlobalLaneResizeDrag {
    fn render(&mut self, _w: &mut Window, _cx: &mut gpui::Context<Self>) -> impl IntoElement {
        Empty
    }
}

// ── Timeline scrollbars ─────────────────────────────────────────────────
//
// Both scrollbars are rendered as absolute overlays on top of the
// arrangement area. The thumb is sized by `viewport / content` and
// positioned by `scroll / max_scroll`. Mouse-down on the track jumps
// the scroll position so the click point becomes the new thumb top
// (vertical) or thumb left (horizontal). The wheel handler on the
// Timeline div continues to handle smooth scrolling and zoom; the
// scrollbar is the visible indicator + a coarse jump target.

const SCROLLBAR_THICKNESS: f32 = 8.0;
const SCROLLBAR_MIN_THUMB: f32 = 24.0;

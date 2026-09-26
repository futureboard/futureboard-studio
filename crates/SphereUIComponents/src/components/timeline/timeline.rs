mod clip_fades;
mod clip_move_cancel;
mod methods;
mod plugin_drop;
mod render;
mod track_rename;
pub use plugin_drop::{resolve_plugin_drop, NewTrackKind, PluginDropTarget};
pub(crate) use render::*;
pub(crate) use track_rename::{
    track_rename_command_policy, TrackRenameChord, TrackRenameCommandPolicy, TrackRenameKeyOutcome,
    TrackRenameSession,
};

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
use crate::components::timeline::chord_track::{
    chord_track_lane, ChordEventDrag, ChordEventDragUpdate,
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
    ArrangementCoordinateContext, ArrangementHitTarget, ClipDragItem, ClipEdge, ClipResizeDrag,
    ClipState, ClipType, DEFAULT_TRACK_HEIGHT, GlobalLaneKind, GlobalLaneResizeDrag, HEADER_WIDTH,
    RULER_HEIGHT, TempoLaneDrag, TimeSignaturePointDrag, TimelineMarkerDrag, TimelineMarkerState,
    TimelineRegionState, TimelineState, TimelineTool, TrackDragItem, TrackHeightResizeDrag,
    TrackType, hit_test_arrangement,
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
    /// click (default-length clip) from a drag (sized clip). Only the Pen
    /// draws; the Pointer's empty-lane press is a marquee, and its
    /// double-click clip is created by the marquee's release (see
    /// `RangeSelectDrag::create_clip_on_click`).
    dragging: bool,
}

/// The arrangement marquee in flight: a free rectangle of unsnapped pixels.
#[derive(Clone, Debug)]
struct RangeSelectDrag {
    /// Where the press landed, in arrangement space: an unsnapped beat and a
    /// content y. Anchored there rather than in window pixels so the rectangle
    /// stays pinned to the arrangement while it scrolls or zooms in time under
    /// it. Track zoom waits for the marquee to end, since a height change
    /// would move this content y onto another track.
    anchor_beat: f32,
    anchor_content_y: f32,
    /// Window position of the press; the drag threshold is measured from it.
    press_window: (f32, f32),
    /// Latest pointer position, in window pixels. Auto-scroll and the wheel
    /// re-resolve the moving corner from it while the pointer is held still.
    last_window: (f32, f32),
    /// The anchor track: the one pressed, or the last one drawn when the press
    /// was below the tracks.
    start_track_id: String,
    additive: bool,
    /// The press was on `start_track_id`'s lane (not below the tracks).
    on_lane: bool,
    /// A double-click on an empty MIDI / Instrument lane: a release without a
    /// drag creates a default-length clip.
    create_clip_on_click: bool,
    /// Shift was held at the press: that clip starts off the grid.
    bypass_snap: bool,
    /// Past the drag threshold: the rectangle is out. Follow-playhead is
    /// suspended from here until the marquee ends
    /// (`TimelineState::follow_playhead_suspended`).
    dragging: bool,
    /// The selection before the press: the base an additive marquee adds to,
    /// and what Escape puts back.
    selection_before: crate::components::timeline::timeline_state::TimelineSelection,
    /// What the rectangle enclosed when the preview was last computed, so the
    /// arrangement is only notified when that changes. `None` until the drag
    /// starts.
    hits: Option<crate::components::timeline::timeline_state::MarqueeHits>,
}

/// A clip move in flight, captured on its first drag move before anything
/// changes: every moving clip as it was, so each move resolves from the origin
/// and the drop records one exact undo step.
#[derive(Clone, Debug)]
struct ClipMoveOrigin {
    anchor_clip_id: String,
    anchor_start: f32,
    clips: Vec<ClipSnapshot>,
}

/// A press on a clip that was already selected. Its selection change waits
/// for the release: a click applies it, a drag drops it, so dragging a
/// selected clip moves the whole selection.
#[derive(Clone, Debug)]
struct PendingClipClick {
    clip_id: String,
    op: crate::components::timeline::timeline_state::ClipSelectOp,
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
    /// Fired for project-key edits: persisted, never an engine-graph change,
    /// but live ARA documents have to be told.
    on_musical_context_changed: Option<TimelineProjectChangedCb>,
    on_media_changed: Option<TimelineProjectChangedCb>,
    on_add_track: Option<TimelineAddTrackCb>,
    on_plugin_preset_drop: Option<TimelinePluginPresetDropCb>,
    on_plugin_drag_drop: Option<TimelinePluginDragDropCb>,
    /// What releasing the plug-in being dragged over the arrangement would do.
    plugin_drop_hint: Option<plugin_drop::PluginDropHint>,

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
    /// Beats between the grabbed edge and the pointer when the edge-resize
    /// gesture began. Edge handles are up to 10 px wide; without this the edge
    /// jumped to the pointer on the first move.
    clip_resize_grab_beats: f32,
    clip_drag_target_track_index: Option<usize>,
    clip_clone_drag_id: Option<String>,
    /// Pen-tool click-drag MIDI clip preview, live until mouse-up creates the clip.
    pen_clip_draw: Option<ClipDrawPreview>,
    /// Pointer-tool arrangement marquee. A Pointer press on any lane space no
    /// clip owns starts it — every lane type, the pad bands around clips and
    /// the area below the last track — and the additive modifier (Cmd on
    /// macOS, where GPUI turns Ctrl + left into a right press; Ctrl elsewhere)
    /// keeps the existing selection; see `lane_press_intent`. Clips, rulers,
    /// toolbar controls and the other tools never start it.
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
    tempo_drag: Option<TempoLaneDrag>,
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
    /// Chord Track events at the start of a chord block drag; the drop turns
    /// the whole gesture into one undo entry.
    chord_gesture_origin: Option<Vec<crate::components::timeline::timeline_state::ChordTrackEvent>>,
    /// Named app commands the timeline asks the Studio to run (e.g. opening
    /// the Chord Generator from the Chord Track header).
    on_command: Option<TimelineCommandCb>,
    /// Pre-gesture marker snapshot, for the Marker lane's flag drag. Mirrors
    /// `region_gesture_origin`.
    marker_gesture_origin: Option<Vec<TimelineMarkerState>>,
    pan_last_position: Option<gpui::Point<gpui::Pixels>>,
    /// The Ctrl/Cmd+Alt+wheel track-zoom burst in flight: the heights it
    /// scales from. Ends on wheel idle or when anything else changes a height.
    track_zoom_session: Option<crate::components::timeline::timeline_state::TrackHeightZoomSession>,
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
    /// The clip as it was before the clip gain / fade gesture now in flight.
    ///
    /// Taken from state on the gesture's first preview. The handles used to
    /// capture it at render time, but every preview re-renders the lane, so by
    /// release the "before" was the last preview and the undo entry recorded
    /// nothing (or a one-pixel step) — undo and redo appeared to do nothing.
    clip_process_origin: Option<ClipState>,
    /// The ruler's grid-resolution dropdown is open.
    snap_menu_open: bool,
    /// Where the Smart Tool's razor line is, shared with its overlay.
    cut_guide: crate::components::timeline::cut_guide::CutGuideCell,
    /// The razor line's own entity, built on first render, so a hover moves
    /// one line instead of rebuilding the lanes.
    cut_guide_overlay:
        Option<gpui::Entity<crate::components::timeline::cut_guide::CutGuideOverlay>>,
    /// The last Smart Tool cut-zone hover, kept while the pointer is in a zone
    /// so pressing or releasing Option shows or hides the razor line without
    /// a mouse move.
    cut_hover: Option<crate::components::timeline::audio_clip::ClipCutGesture>,
    /// Window y of the timeline's top edge, measured by the root's probe each
    /// frame and folded into the viewport at the top of the next render — the
    /// vertical twin of `lane_origin_probe`.
    timeline_origin_probe: std::rc::Rc<std::cell::Cell<Option<f32>>>,
    /// Where the arrangement marquee's rectangle is, shared with its overlay.
    marquee_frame: crate::components::timeline::marquee_overlay::MarqueeFrameCell,
    /// The marquee rectangle's own entity, built on first render, so a pointer
    /// move does not rebuild the lanes (see `marquee_overlay`).
    marquee_overlay:
        Option<gpui::Entity<crate::components::timeline::marquee_overlay::MarqueeOverlay>>,
    /// Scrolls the arrangement while a marquee is dragged past the track
    /// area's edge. Dropped on every way a marquee ends.
    marquee_autoscroll: Option<gpui::Task<()>>,
    /// The clip move in flight; see [`ClipMoveOrigin`].
    clip_move_origin: Option<ClipMoveOrigin>,
    /// Escape or a tool change cancelled the clip move or Option-drag clone in
    /// flight. GPUI may still deliver that drag's moves and its drop; they are
    /// ignored until the next press. See `clip_move_cancel`.
    clip_drag_cancelled: bool,
    /// A clip selection change waiting for its release; see
    /// [`PendingClipClick`].
    pending_clip_click: Option<PendingClipClick>,
    /// The inline track-name edit in a header, if one is open; see
    /// [`TrackRenameSession`].
    track_rename: Option<TrackRenameSession>,
    /// The studio's keyboard anchor, set by the owner. Focus goes back here
    /// when an editor the arrangement owns (the track rename) closes, so the
    /// shortcuts never wait on a field that is no longer drawn.
    focus_return: Option<gpui::FocusHandle>,
    /// Fade and crossfade handle gestures, and the hovered clip's fade
    /// handles; see `clip_fades`.
    clip_fades: clip_fades::ClipFadeUi,
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
    /// The ruler's grid-resolution dropdown.
    SnapGrid,
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
    /// Right-click on the global Chord Track. `event_id` is `None` on the
    /// empty lane.
    ChordTrack {
        beat: f64,
        event_id: Option<u64>,
    },
    /// Lane header menu button on the Chord Track.
    ChordLaneHeader,
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

/// A plug-in preset (`.pst`) dropped on the arrangement: the preset path and
/// the track it landed on, or `None` for empty space below the last track.
pub type TimelinePluginPresetDropCb = std::sync::Arc<
    dyn Fn(&(std::path::PathBuf, Option<String>), &mut gpui::Window, &mut gpui::App) + 'static,
>;
pub type TimelinePluginDragDropCb = std::sync::Arc<
    dyn Fn(
            &crate::components::plugin_picker::PluginDragItem,
            &PluginDropTarget,
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

/// A named Studio command requested from inside the timeline.
pub type TimelineCommandCb = std::sync::Arc<dyn Fn(&'static str, &mut gpui::App) + 'static>;

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

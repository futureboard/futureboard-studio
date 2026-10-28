//! Solfege Voxtune — the vocal note editor, as a native GPUI view.
//!
//! One editor for every desktop front end: Studio's tuner panel, the
//! standalone desktop app and the VST3/ARA plug-in. It edits a
//! [`VoxSession`] (the shared `sphere_voxtune` engine's document, undo
//! history and engine sync) and asks a [`VoxtuneHost`] for the transport.
//!
//! ```text
//! [tools] [transport] [key · scale · correct] [A/B] [undo redo] [zoom]
//! ┌ keys ┬ ruler ───────────────────────────────┬ inspector ┐
//! │      │ notes: envelope blob + played pitch  │ note      │
//! │      │                                      │ expression│
//! │      │                                      │ macros    │
//! └──────┴──────────────────────────────────────┴ history ──┘
//! ```
//!
//! Drawing and hit-testing share [`viewport::VoxViewport`]; every edit is
//! one undo step through the session; nothing here touches the audio
//! thread except through the engine's own lock-free publish path.
//!
//! Mountable outside Studio (the plug-in compiles these files with
//! `#[path]`): it uses only `crate::theme`, `crate::assets` and
//! `crate::components::{controls, fader, slider}`.

pub mod gpu;
pub mod host;
pub mod icons;
pub mod paint;
pub mod scene;
mod view;
pub mod viewport;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    Bounds, Context, FocusHandle, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    Pixels, ScrollWheelEvent, SharedString, WeakEntity, Window,
};
use sphere_voxtune::edit::{Macro, TimingEdge, DEFAULT_ORN_DEPTH, DEFAULT_ORN_SPEED};
use sphere_voxtune::engine::{
    solfege_for_midi, CantoAnalysisResult, CantoNote, ORN_FALL, ORN_NONE, ORN_SHAKE, ORN_TURN_DOWN,
    ORN_TURN_UP,
};
use sphere_voxtune::load_job::LoadJob;
use sphere_voxtune::session::{VoxDocument, VoxSession};
use sphere_voxtune::shared::Shared;

use gpu::GpuCanvas;
pub use host::{EngineTransport, VoxTransport, VoxtuneHost};
use paint::{note_canvas, CanvasData, Peaks};
use view::PanelTab;
use viewport::{grid_lines, VoxViewport};

/// Width of the pitch-key lane left of the notes.
const KEYS_W: f32 = 52.0;
/// Height of the time ruler.
const RULER_H: f32 = 22.0;
/// Toolbar height.
const TOOLBAR_H: f32 = 40.0;
/// A timing drag grabs an edge within this many px of it.
const EDGE_GRAB_PX: f32 = 6.0;
/// Wheel line → px.
const WHEEL_LINE_PX: f32 = 40.0;
/// Transport poll while playing / loading, and while idle.
const TICK_ACTIVE: Duration = Duration::from_millis(33);
const TICK_IDLE: Duration = Duration::from_millis(150);

/// What a drag on a note does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoxTool {
    /// Vertical: the note's pitch centre (snaps to semitones; Shift = free).
    Pitch,
    /// Vertical: how much of the sung drift/vibrato is kept.
    Drift,
    /// Horizontal: move the note, or its start / end at the edges.
    Time,
    /// Vertical: formant shift.
    Formant,
    /// Vertical: note level.
    Level,
    /// Click: cut the note there.
    Cut,
}

impl VoxTool {
    pub const ALL: [VoxTool; 6] = [
        VoxTool::Pitch,
        VoxTool::Drift,
        VoxTool::Time,
        VoxTool::Formant,
        VoxTool::Level,
        VoxTool::Cut,
    ];

    pub fn label(self) -> &'static str {
        match self {
            VoxTool::Pitch => "Pitch",
            VoxTool::Drift => "Drift",
            VoxTool::Time => "Time",
            VoxTool::Formant => "Formant",
            VoxTool::Level => "Level",
            VoxTool::Cut => "Cut",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            VoxTool::Pitch => icons::TOOL_PITCH,
            VoxTool::Drift => icons::TOOL_DRIFT,
            VoxTool::Time => icons::TOOL_TIME,
            VoxTool::Formant => icons::TOOL_FORMANT,
            VoxTool::Level => icons::TOOL_LEVEL,
            VoxTool::Cut => icons::TOOL_CUT,
        }
    }

    fn tooltip(self) -> &'static str {
        match self {
            VoxTool::Pitch => "Pitch (1) — drag a note up or down; Shift for cents",
            VoxTool::Drift => "Drift (2) — drag down to flatten the sung wobble",
            VoxTool::Time => "Time (3) — drag a note, or its start or end",
            VoxTool::Formant => "Formant (4) — drag to change the voice colour",
            VoxTool::Level => "Level (5) — drag to change a note's level",
            VoxTool::Cut => "Cut (6) — click inside a note to split it",
        }
    }

    /// Undo label, as the phone writes it.
    fn undo_label(self) -> &'static str {
        match self {
            VoxTool::Pitch => "tool:pitch",
            VoxTool::Drift => "tool:drift",
            VoxTool::Time => "tool:time",
            VoxTool::Formant => "tool:formant",
            VoxTool::Level => "tool:level",
            VoxTool::Cut => "cut",
        }
    }
}

/// A slider in the inspector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Param {
    Pitch,
    Drift,
    /// The note's start, across the take.
    Time,
    Formant,
    Level,
    OrnDepth,
    OrnSpeed,
    Accent,
    Correct,
    MacroAmount,
}

impl Param {
    fn label(self) -> &'static str {
        match self {
            Param::Pitch => "Pitch",
            Param::Drift => "Drift",
            Param::Time => "Time",
            Param::Formant => "Formant",
            Param::Level => "Level",
            Param::OrnDepth => "Width",
            Param::OrnSpeed => "Speed",
            Param::Accent => "Accent",
            Param::Correct => "Correct",
            Param::MacroAmount => "Amount",
        }
    }

    fn undo_label(self) -> &'static str {
        match self {
            Param::Pitch => "tool:pitch",
            Param::Drift => "tool:drift",
            Param::Time => "tool:time",
            Param::Formant => "tool:formant",
            Param::Level => "tool:level",
            Param::OrnDepth | Param::OrnSpeed => "orn",
            Param::Accent => "accent",
            Param::Correct => "correct",
            Param::MacroAmount => "",
        }
    }

    fn bipolar(self) -> bool {
        matches!(self, Param::Pitch | Param::Formant)
    }

    fn per_note(self) -> bool {
        !matches!(self, Param::Correct | Param::MacroAmount)
    }

    fn slider_id(self) -> &'static str {
        match self {
            Param::Pitch => "voxtune-param-pitch",
            Param::Drift => "voxtune-param-drift",
            Param::Time => "voxtune-param-time",
            Param::Formant => "voxtune-param-formant",
            Param::Level => "voxtune-param-level",
            Param::OrnDepth => "voxtune-param-orn-width",
            Param::OrnSpeed => "voxtune-param-orn-speed",
            Param::Accent => "voxtune-param-accent",
            Param::Correct => "voxtune-param-correct",
            Param::MacroAmount => "voxtune-param-macro-amount",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Drag {
    /// Pitch / drift / formant / level: vertical travel from `anchor_y`.
    Value {
        index: usize,
        tool: VoxTool,
        anchor_y: f32,
        start: f32,
    },
    /// Timing: horizontal travel from `anchor_t`.
    Time {
        index: usize,
        edge: TimingEdge,
        anchor_t: f32,
        start: f32,
        dur: f32,
    },
    /// Middle-button pan.
    Pan {
        anchor: (f32, f32),
        view: VoxViewport,
    },
    /// Ruler scrub.
    Seek,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Menu {
    /// Key and scale in one popover.
    KeyScale,
}

/// Where a pitch drag lands (the phone's Free / Semi / Scale).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Snap {
    Free,
    Semi,
    #[default]
    Scale,
}

impl Snap {
    pub const ALL: [Snap; 3] = [Snap::Free, Snap::Semi, Snap::Scale];

    pub fn label(self) -> &'static str {
        match self {
            Snap::Free => "Free",
            Snap::Semi => "Semi",
            Snap::Scale => "Scale",
        }
    }
}

/// A load in progress: the job and the document to restore when it ends.
struct Loading {
    job: LoadJob,
    title: String,
    saved: Option<VoxDocument>,
}

pub struct VoxtuneEditor {
    session: VoxSession,
    host: Box<dyn VoxtuneHost>,
    focus: FocusHandle,
    view: VoxViewport,
    tool: VoxTool,
    selected: Option<usize>,
    hovered: Option<usize>,
    drag: Option<Drag>,
    /// A note preview is sounding (pressed note).
    auditioning: bool,
    grid_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    ruler_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    overview_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// Pitch drags snap to this.
    snap: Snap,
    /// The tool to go back to after a cut.
    tool_before_cut: Option<VoxTool>,
    peaks: Rc<Peaks>,
    notes_cache: (u64, Rc<Vec<CantoNote>>),
    loading: Option<Loading>,
    /// Last load's failure, shown until the next load.
    error: Option<String>,
    title: String,
    empty_hint: SharedString,
    bypass: bool,
    menu: Option<Menu>,
    /// Frame the notes once the note area has a size.
    fit_pending: bool,
    /// Follow the playhead while playing.
    follow: bool,
    /// Pitch slider anchor (the semitone it measures cents from).
    pitch_anchor: f32,
    macro_amount: f32,
    macro_grid: f32,
    macro_selected_only: bool,
    /// Feedback line after a macro ("3 notes changed").
    status: Option<String>,
    /// Inspector tab.
    panel: PanelTab,
    /// wgpu renderer of the note canvas.
    gpu: Rc<RefCell<GpuCanvas>>,
    last_transport: VoxTransport,
    last_progress: u32,
    /// Pointer over the note area (local px), for tool overlays.
    pointer: Option<(f32, f32)>,
    /// Shift held during the current drag (free pitch, fine values).
    drag_fine: bool,
    /// Preview scenes: a gesture to stage once the view is fitted.
    #[cfg(feature = "ui-preview")]
    preview_pending: Option<(VoxTool, f32)>,
}

impl VoxtuneEditor {
    pub fn new(shared: Arc<Shared>, host: Box<dyn VoxtuneHost>, cx: &mut Context<Self>) -> Self {
        let mut editor = Self {
            session: VoxSession::new(shared),
            host,
            focus: cx.focus_handle(),
            view: VoxViewport::default(),
            tool: VoxTool::Pitch,
            selected: None,
            hovered: None,
            drag: None,
            auditioning: false,
            grid_bounds: Rc::new(Cell::new(None)),
            ruler_bounds: Rc::new(Cell::new(None)),
            overview_bounds: Rc::new(Cell::new(None)),
            snap: Snap::default(),
            tool_before_cut: None,
            peaks: Rc::new(Peaks::default()),
            notes_cache: (u64::MAX, Rc::new(Vec::new())),
            loading: None,
            error: None,
            title: String::new(),
            empty_hint: "Open a vocal take to start.".into(),
            bypass: false,
            menu: None,
            fit_pending: false,
            follow: true,
            pitch_anchor: 60.0,
            macro_amount: 1.0,
            macro_grid: 0.5,
            macro_selected_only: false,
            status: None,
            panel: PanelTab::default(),
            gpu: Rc::new(RefCell::new(GpuCanvas::default())),
            last_transport: VoxTransport::default(),
            last_progress: 0,
            pointer: None,
            drag_fine: false,
            #[cfg(feature = "ui-preview")]
            preview_pending: None,
        };
        editor.start_ticker(cx);
        editor
    }

    // ---- Host API -----------------------------------------------------

    pub fn session(&self) -> &VoxSession {
        &self.session
    }

    pub fn document(&self) -> &VoxDocument {
        self.session.doc()
    }

    pub fn host_mut(&mut self) -> &mut dyn VoxtuneHost {
        self.host.as_mut()
    }

    pub fn has_audio(&self) -> bool {
        !self.peaks.is_empty()
    }

    pub fn is_loading(&self) -> bool {
        self.loading.is_some()
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    /// Swaps in another engine (a take at another sample rate). The editor
    /// shows nothing until the next load or [`Self::adopt`].
    pub fn set_engine(&mut self, shared: Arc<Shared>, cx: &mut Context<Self>) {
        self.cancel_load(cx);
        self.stop_audition();
        self.session = VoxSession::new(shared);
        self.peaks = Rc::new(Peaks::default());
        self.notes_cache = (u64::MAX, Rc::new(Vec::new()));
        self.selected = None;
        self.hovered = None;
        self.drag = None;
        self.bypass = false;
        self.title.clear();
        self.error = None;
        cx.notify();
    }

    /// Runs an editor command by id, as a menu would. Returns false for an
    /// unknown id. Ids: `undo`, `redo`, `play`, `home`, `loop`, `fit`,
    /// `zoom-in`, `zoom-out`, `history`, `tune-all`, `tune-note`, `join`,
    /// `mute`, `reset-note`, `original`, `panel-note` / `-shape` / `-take` /
    /// `-history`.
    pub fn run_command(&mut self, command: &str, cx: &mut Context<Self>) -> bool {
        match command {
            "undo" => self.undo(cx),
            "redo" => self.redo(cx),
            "play" => self.toggle_play(cx),
            "home" => {
                self.host.seek(0.0, cx);
                self.view.t0 = 0.0;
            }
            "loop" => {
                if let Some(on) = self.host.looping(cx) {
                    self.host.set_looping(!on, cx);
                }
            }
            "fit" => self.fit_view(),
            "panel-note" | "panel-history" => self.panel = PanelTab::Main,
            "panel-shape" => self.panel = PanelTab::Expression,
            "panel-take" => self.panel = PanelTab::Advanced,
            "next-off" => self.select_next_off(cx),
            "cut-mode" => self.toggle_cut_mode(),
            "zoom-in" => self.zoom(1.25, cx),
            "zoom-out" => self.zoom(0.8, cx),
            "history" => self.panel = PanelTab::Main,
            "tune-all" if self.editable() && !self.session.notes().is_empty() => {
                self.session.set_correct(1.0);
                self.changed(cx);
            }
            "tune-note" => {
                if let Some(i) = self.selected.filter(|_| self.editable()) {
                    self.session.tune_note(i);
                    self.changed(cx);
                }
            }
            "join" if self.editable() => self.join_selected(cx),
            "mute" => {
                if let Some(i) = self.selected.filter(|_| self.editable()) {
                    self.session.toggle_mute(i);
                    self.changed(cx);
                }
            }
            "reset-note" => {
                if let Some(i) = self.selected.filter(|_| self.editable()) {
                    self.session.reset_note(i);
                    self.changed(cx);
                }
            }
            "original" if self.has_audio() => {
                self.bypass = !self.bypass;
                self.session.shared().lock().bypass = self.bypass;
                self.end_gesture(cx);
            }
            "tune-all" | "join" | "original" => {}
            _ => return false,
        }
        cx.notify();
        true
    }

    /// Preview scenes only: puts the editor mid-gesture on the selected note
    /// — `tool` active, the pointer over the note, and for the value tools a
    /// drag that moved it by `delta` (semitones for Pitch) — so the overlays
    /// can be rendered without a mouse.
    #[cfg(feature = "ui-preview")]
    pub fn preview_gesture(&mut self, tool: VoxTool, delta: f32, cx: &mut Context<Self>) {
        // The pointer is placed in view coordinates, so wait for the first
        // fit (it needs the note area's size).
        if self.fit_pending || self.grid_bounds.get().is_none() {
            self.preview_pending = Some((tool, delta));
            cx.notify();
            return;
        }
        self.tool = tool;
        let Some(i) = self.selected else { return };
        let n = self.session.notes()[i].clone();
        let mid_x = self.view.time_to_x(n.start + n.dur * 0.6);
        self.pointer = Some((mid_x, self.view.pitch_to_y(n.pitch)));
        match tool {
            VoxTool::Pitch => {
                self.session.begin(tool.undo_label());
                if let Some(m) = self.session.note_mut(i) {
                    m.pitch = (n.pitch + delta).round();
                }
                self.session.sync_note(i);
                self.drag = Some(Drag::Value {
                    index: i,
                    tool,
                    anchor_y: 0.0,
                    start: n.pitch,
                });
            }
            VoxTool::Cut => {
                // Hovering the next note with the blade.
                self.hovered = Some(i);
            }
            _ => {}
        }
        cx.notify();
    }

    /// Selects a note (or none), as a click would.
    pub fn select_note(&mut self, index: Option<usize>, cx: &mut Context<Self>) {
        let index = index.filter(|&i| i < self.session.notes().len());
        self.select(index, cx);
    }

    /// Text shown while nothing is loaded.
    pub fn set_empty_hint(&mut self, hint: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.empty_hint = hint.into();
        cx.notify();
    }

    /// Follows `job` (decode + analysis on its own thread) with live
    /// progress; when it ends the take is shown with `saved` edits if they
    /// fit it, else the fresh analysis. Replaces any load in progress.
    pub fn start_load(
        &mut self,
        job: LoadJob,
        title: impl Into<String>,
        saved: Option<VoxDocument>,
        cx: &mut Context<Self>,
    ) {
        self.end_gesture(cx);
        self.stop_audition();
        self.error = None;
        self.status = None;
        self.loading = Some(Loading {
            job,
            title: title.into(),
            saved,
        });
        self.last_progress = 0;
        cx.notify();
    }

    /// Cancels a load in progress; the previous take (if any) stays.
    pub fn cancel_load(&mut self, cx: &mut Context<Self>) {
        if let Some(load) = self.loading.take() {
            load.job.cancel();
            cx.notify();
        }
    }

    /// Shows the take already installed in the engine, edited as `doc`
    /// (a restored session, or a host that analysed it itself).
    pub fn adopt(&mut self, doc: VoxDocument, title: impl Into<String>, cx: &mut Context<Self>) {
        self.title = title.into();
        self.install(doc, cx);
    }

    fn install(&mut self, doc: VoxDocument, cx: &mut Context<Self>) {
        let (peaks, duration) = {
            let engine = self.session.shared().lock();
            (
                Peaks::build(&engine.audio_samples, engine.sample_rate()),
                engine.duration_sec as f32,
            )
        };
        self.peaks = Rc::new(peaks);
        self.session.reset(doc, duration);
        self.selected = None;
        self.hovered = None;
        self.drag = None;
        self.fit_pending = true;
        self.host.document_changed(self.session.doc(), cx);
        cx.notify();
    }

    fn poll_load(&mut self, cx: &mut Context<Self>) {
        let Some(load) = self.loading.as_ref() else {
            return;
        };
        let Some(result) = load.job.state.take_result() else {
            let p = load
                .job
                .state
                .progress
                .permille
                .load(std::sync::atomic::Ordering::Relaxed);
            if p != self.last_progress {
                self.last_progress = p;
                cx.notify();
            }
            return;
        };
        let load = self.loading.take().unwrap();
        match result {
            Ok(analysis) => {
                let duration = analysis.duration_sec as f32;
                let doc = load
                    .saved
                    .filter(|d| fits_take(d, duration))
                    .unwrap_or_else(|| VoxDocument::from_analysis(&analysis));
                self.host.analysis_ready(&analysis, cx);
                self.title = load.title;
                self.install(doc, cx);
            }
            Err(message) => {
                self.error = Some(message);
                cx.notify();
            }
        }
    }

    // ---- Ticker: transport and load progress --------------------------

    fn start_ticker(&mut self, cx: &mut Context<Self>) {
        let executor = cx.background_executor().clone();
        cx.spawn(async move |this: WeakEntity<Self>, cx| loop {
            let active = match this.update(cx, |ed, cx| ed.tick(cx)) {
                Ok(active) => active,
                Err(_) => break,
            };
            executor
                .timer(if active { TICK_ACTIVE } else { TICK_IDLE })
                .await;
        })
        .detach();
    }

    /// One poll. Returns whether something is moving (faster polling).
    fn tick(&mut self, cx: &mut Context<Self>) -> bool {
        self.poll_load(cx);
        let transport = self.host.transport(cx);
        let audition = self.session.shared().is_auditioning();
        if self.auditioning && !audition {
            self.auditioning = false;
            cx.notify();
        }
        if transport != self.last_transport {
            self.last_transport = transport;
            if transport.playing && self.follow && self.drag.is_none() {
                self.follow_playhead(transport.position as f32);
            }
            cx.notify();
        }
        transport.playing || self.loading.is_some() || audition
    }

    /// Pages the view when the playhead leaves it.
    fn follow_playhead(&mut self, t: f32) {
        let Some(b) = self.grid_bounds.get() else {
            return;
        };
        let w = f32::from(b.size.width);
        let x = self.view.time_to_x(t);
        if x < 0.0 || x > w * 0.92 {
            self.view.t0 = t - w * 0.08 / self.view.pps;
        }
    }

    // ---- Edits ----------------------------------------------------------

    /// After a committed change: tell the host, redraw.
    fn changed(&mut self, cx: &mut Context<Self>) {
        if !self.session.is_open() {
            self.host.document_changed(self.session.doc(), cx);
        }
        if let Some(i) = self.selected {
            if i >= self.session.notes().len() {
                self.selected = None;
            }
        }
        cx.notify();
    }

    fn end_gesture(&mut self, cx: &mut Context<Self>) {
        if self.session.is_open() {
            self.session.commit();
            self.changed(cx);
        }
        self.drag = None;
    }

    fn undo(&mut self, cx: &mut Context<Self>) {
        self.drag = None;
        if self.session.undo() {
            self.status = None;
            self.changed(cx);
        }
    }

    fn redo(&mut self, cx: &mut Context<Self>) {
        self.drag = None;
        if self.session.redo() {
            self.status = None;
            self.changed(cx);
        }
    }

    fn selected_note(&self) -> Option<&CantoNote> {
        self.selected.and_then(|i| self.session.notes().get(i))
    }

    fn editable(&self) -> bool {
        !self.bypass && self.loading.is_none() && self.has_audio()
    }

    fn select(&mut self, index: Option<usize>, cx: &mut Context<Self>) {
        if self.selected != index {
            self.selected = index;
            cx.notify();
        }
    }

    fn cut_selected_at(&mut self, t: f32, cx: &mut Context<Self>) {
        let Some(i) = self.selected else { return };
        if let Some(second) = self.session.cut(i, t) {
            self.selected = Some(second);
            // A cut is one action: back to the tool before it.
            if let Some(tool) = self.tool_before_cut.take() {
                self.tool = tool;
            }
            self.changed(cx);
        }
    }

    /// Cut mode on (remembering the tool) or off.
    fn toggle_cut_mode(&mut self) {
        if self.tool == VoxTool::Cut {
            self.tool = self.tool_before_cut.take().unwrap_or(VoxTool::Pitch);
        } else {
            self.tool_before_cut = Some(self.tool);
            self.tool = VoxTool::Cut;
        }
    }

    /// Notes off the scale, in time order.
    fn off_notes(&self) -> Vec<usize> {
        let doc = self.session.doc();
        let mut off: Vec<usize> = (0..doc.notes.len())
            .filter(|&i| scene::off_key(&doc.notes[i].present(self.bypass), doc.key, doc.scale))
            .collect();
        off.sort_by(|&a, &b| doc.notes[a].start.total_cmp(&doc.notes[b].start));
        off
    }

    pub fn off_count(&self) -> usize {
        self.off_notes().len()
    }

    /// Selects the next off-key note after the selection (wrapping) and
    /// brings it into view.
    fn select_next_off(&mut self, cx: &mut Context<Self>) {
        let off = self.off_notes();
        let notes = self.session.notes();
        let after = self.selected.map(|i| notes[i].start).unwrap_or(f32::MIN);
        let Some(&next) = off
            .iter()
            .find(|&&i| notes[i].start > after + 1e-4)
            .or(off.first())
        else {
            return;
        };
        let n = notes[next].clone();
        self.selected = Some(next);
        let (w, h) = self.grid_size();
        let x0 = self.view.time_to_x(n.start);
        let x1 = self.view.time_to_x(n.start + n.dur);
        if x0 < 0.0 || x1 > w {
            self.view.t0 = n.start - w * 0.3 / self.view.pps;
        }
        let y = self.view.pitch_to_y(n.pitch);
        if y < 0.0 || y > h {
            self.view.top_pitch = n.pitch + h * 0.5 / self.view.row_h;
        }
        self.view.clamp(self.session.duration(), w, h);
        cx.notify();
    }

    fn join_selected(&mut self, cx: &mut Context<Self>) {
        let Some(i) = self.selected else { return };
        if let Some(joined) = self.session.join_next(i) {
            self.selected = Some(joined);
            self.changed(cx);
        }
    }

    fn nudge_pitch(&mut self, semis: f32, cx: &mut Context<Self>) {
        let Some(i) = self.selected else { return };
        if !self.editable() || self.session.notes()[i].unpitched {
            return;
        }
        let key = self.session.doc().key;
        self.session.edit("tool:pitch", |doc| {
            let n = &mut doc.notes[i];
            n.pitch += semis;
            n.pinned = true;
            n.lyric = solfege_for_midi(n.pitch, key);
        });
        self.changed(cx);
    }

    fn run_macro(&mut self, op: Macro, cx: &mut Context<Self>) {
        if !self.editable() {
            return;
        }
        let only = if self.macro_selected_only {
            match self.selected {
                Some(i) => Some(i),
                None => return,
            }
        } else {
            None
        };
        let count = self.session.apply_macro(only, op);
        self.status = Some(match count {
            0 => "No notes needed a change".to_string(),
            1 => "1 note changed".to_string(),
            n => format!("{n} notes changed"),
        });
        self.changed(cx);
    }

    // ---- Inspector parameters ---------------------------------------------

    fn param_value(&self, p: Param) -> Option<f32> {
        if p == Param::Correct {
            return Some(self.session.doc().correct);
        }
        if p == Param::MacroAmount {
            return Some(self.macro_amount);
        }
        let n = self.selected_note()?;
        if n.unpitched {
            return None;
        }
        Some(match p {
            Param::Pitch => (n.pitch - n.pitch.round()).clamp(-1.0, 1.0),
            Param::Drift => n.modulation / 1.45,
            Param::Time => n.start / self.time_span(n),
            Param::Formant => n.formant / 12.0,
            Param::Level => (n.gain_db + 18.0) / 24.0,
            Param::OrnDepth => (n.orn_depth.max(0.25) - 0.25) / 2.75,
            Param::OrnSpeed => n.orn_speed,
            Param::Accent => n.accent,
            Param::Correct | Param::MacroAmount => unreachable!(),
        })
    }

    /// The range a note's start can take: the take less the note.
    fn time_span(&self, n: &CantoNote) -> f32 {
        (self.session.duration() - n.dur).max(0.01)
    }

    fn param_text(&self, p: Param) -> String {
        if p == Param::Correct {
            return format!("{}%", (self.session.doc().correct * 100.0).round());
        }
        if p == Param::MacroAmount {
            return format!("{}%", (self.macro_amount * 100.0).round());
        }
        let Some(n) = self.selected_note().filter(|n| !n.unpitched) else {
            return "—".into();
        };
        match p {
            Param::Pitch => format_cents((n.pitch - n.pitch.round()) * 100.0),
            Param::Drift => format!("{}%", (n.modulation * 100.0).round()),
            Param::Time => format_position(n.start, self.session.doc().bpm),
            // `+ 0.0` turns a -0.0 into 0.0 so it never reads "-0.0".
            Param::Formant => format!("{:+.1} st", n.formant + 0.0),
            Param::Level => format!("{:+.1} dB", (n.gain_db * 10.0).round() / 10.0 + 0.0),
            Param::OrnDepth => format!("{:.2} st", n.orn_depth.max(0.25)),
            Param::OrnSpeed => format!("{}%", (n.orn_speed * 100.0).round()),
            Param::Accent => format!("+{:.1} dB", n.accent * 6.0),
            Param::Correct | Param::MacroAmount => unreachable!(),
        }
    }

    fn param_enabled(&self, p: Param) -> bool {
        match p {
            Param::MacroAmount => true,
            Param::Correct => self.editable() && !self.session.notes().is_empty(),
            Param::OrnDepth | Param::OrnSpeed => {
                self.editable()
                    && self
                        .selected_note()
                        .is_some_and(|n| !n.unpitched && n.orn_style != ORN_NONE)
            }
            _ => self.editable() && self.selected_note().is_some_and(|n| !n.unpitched),
        }
    }

    fn begin_param(&mut self, p: Param, cx: &mut Context<Self>) {
        if !self.param_enabled(p) || p == Param::MacroAmount {
            return;
        }
        if p == Param::Pitch {
            if let Some(n) = self.selected_note() {
                self.pitch_anchor = n.pitch.round();
            }
        }
        self.session.begin(p.undo_label());
        cx.notify();
    }

    fn set_param(&mut self, p: Param, v: f32, cx: &mut Context<Self>) {
        if p == Param::MacroAmount {
            self.macro_amount = v.clamp(0.0, 1.0);
            cx.notify();
            return;
        }
        if !self.param_enabled(p) {
            return;
        }
        if p == Param::Correct {
            self.session.set_correct(v);
            self.changed(cx);
            return;
        }
        let Some(i) = self.selected else { return };
        let one_shot = !self.session.is_open();
        if one_shot {
            self.begin_param(p, cx);
        }
        let key = self.session.doc().key;
        let anchor = self.pitch_anchor;
        let span = self
            .selected_note()
            .map(|n| self.time_span(n))
            .unwrap_or(1.0);
        if let Some(n) = self.session.note_mut(i) {
            match p {
                Param::Pitch => {
                    n.pitch = anchor + v.clamp(-1.0, 1.0);
                    n.pinned = true;
                    n.lyric = solfege_for_midi(n.pitch, key);
                }
                Param::Drift => n.modulation = (v * 1.45).clamp(0.0, 1.45),
                Param::Time => n.start = (v * span * 100.0).round() / 100.0,
                Param::Formant => n.formant = (v * 12.0 * 10.0).round() / 10.0,
                Param::Level => n.gain_db = ((v * 24.0 - 18.0) * 10.0).round() / 10.0,
                Param::OrnDepth => n.orn_depth = ((0.25 + v * 2.75) * 20.0).round() / 20.0,
                Param::OrnSpeed => n.orn_speed = (v * 100.0).round() / 100.0,
                Param::Accent => n.accent = (v * 100.0).round() / 100.0,
                Param::Correct | Param::MacroAmount => {}
            }
        }
        if p == Param::Time {
            self.session.clamp_timing(i, TimingEdge::Body);
        }
        self.session.sync_note(i);
        if one_shot {
            self.end_gesture(cx);
        }
        cx.notify();
    }

    /// Double-click reset to the neutral value.
    fn reset_param(&mut self, p: Param, cx: &mut Context<Self>) {
        let neutral = match p {
            Param::Pitch => 0.0,
            Param::Drift => 1.0 / 1.45,
            Param::Time => {
                let Some(n) = self.selected_note() else {
                    return;
                };
                n.detected_start / self.time_span(n)
            }
            Param::Formant => 0.0,
            Param::Level => {
                let Some(n) = self.selected_note() else {
                    return;
                };
                (n.detected_gain + 18.0) / 24.0
            }
            Param::OrnDepth => (DEFAULT_ORN_DEPTH - 0.25) / 2.75,
            Param::OrnSpeed => DEFAULT_ORN_SPEED,
            Param::Accent | Param::Correct => 0.0,
            Param::MacroAmount => 1.0,
        };
        if p == Param::Pitch {
            if let Some(n) = self.selected_note() {
                self.pitch_anchor = n.pitch.round();
            }
        }
        self.end_gesture(cx);
        self.session.begin(p.undo_label());
        self.set_param(p, neutral, cx);
        self.end_gesture(cx);
    }

    // ---- Canvas geometry and hit-testing -----------------------------------

    fn grid_local(&self, pos: gpui::Point<Pixels>) -> Option<(f32, f32)> {
        let b = self.grid_bounds.get()?;
        Some((f32::from(pos.x - b.origin.x), f32::from(pos.y - b.origin.y)))
    }

    fn grid_size(&self) -> (f32, f32) {
        self.grid_bounds
            .get()
            .map(|b| (f32::from(b.size.width), f32::from(b.size.height)))
            .unwrap_or((800.0, 400.0))
    }

    /// The note under `local`, preferring the one whose centre or played
    /// pitch is nearest.
    fn note_at(&self, local: (f32, f32)) -> Option<usize> {
        let t = self.view.x_to_time(local.0);
        let slop = (self.view.row_h * 0.6).max(7.0);
        self.session
            .notes()
            .iter()
            .enumerate()
            .filter(|(_, n)| {
                let n = n.present(self.bypass);
                t >= n.start && t <= n.start + n.dur
            })
            .filter_map(|(i, raw)| {
                let n = raw.present(self.bypass);
                let u = ((t - n.start) / n.dur.max(1e-3)).clamp(0.0, 1.0);
                let centre = (self.view.pitch_to_y(n.pitch) - local.1).abs();
                let curve = (self
                    .view
                    .pitch_to_y(sphere_voxtune::engine::pitch_at(&n, u))
                    - local.1)
                    .abs();
                let cap = scene::Capsule::of(&n, &self.view);
                let d = if cap.contains(local.0, local.1) {
                    0.0
                } else {
                    centre.min(curve)
                };
                (d <= slop.max(cap.half + 3.0)).then_some((i, d))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(i, _)| i)
    }

    fn timing_edge(&self, index: usize, x: f32) -> TimingEdge {
        let n = &self.session.notes()[index];
        let x0 = self.view.time_to_x(n.start);
        let x1 = self.view.time_to_x(n.start + n.dur);
        let grab = EDGE_GRAB_PX.min((x1 - x0) * 0.25);
        if x - x0 <= grab {
            TimingEdge::Start
        } else if x1 - x <= grab {
            TimingEdge::End
        } else {
            TimingEdge::Body
        }
    }

    fn fit_view(&mut self) {
        let (w, h) = self.grid_size();
        let notes = self.session.notes();
        let pitched: Vec<&CantoNote> = notes.iter().filter(|n| !n.unpitched).collect();
        let (lo, hi) = if pitched.is_empty() {
            (55.0, 72.0)
        } else {
            pitched.iter().fold((f32::MAX, f32::MIN), |(lo, hi), n| {
                (
                    lo.min(n.pitch.min(n.detected)),
                    hi.max(n.pitch.max(n.detected)),
                )
            })
        };
        let duration = self.session.duration().max(1.0);
        self.view.fit(0.0, duration, lo, hi, w, h);
        self.view.clamp(duration, w, h);
    }

    // ---- Pointer gestures ------------------------------------------------------

    fn on_grid_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window, cx);
        self.menu = None;
        let Some(local) = self.grid_local(ev.position) else {
            return;
        };
        self.pointer = Some(local);
        self.drag_fine = ev.modifiers.shift;
        if ev.button == MouseButton::Middle {
            self.drag = Some(Drag::Pan {
                anchor: local,
                view: self.view,
            });
            return;
        }
        if ev.button != MouseButton::Left || !self.has_audio() || self.loading.is_some() {
            return;
        }
        let hit = self.note_at(local);
        self.select(hit, cx);
        let Some(i) = hit else {
            // Empty space: move the playhead there.
            let t = self.view.x_to_time(local.0).max(0.0) as f64;
            self.host.seek(t, cx);
            cx.notify();
            return;
        };
        if self.tool == VoxTool::Cut {
            if self.editable() {
                let t = self.view.x_to_time(local.0);
                self.cut_selected_at(t, cx);
            }
            return;
        }
        // Press to hear the note while the transport is stopped.
        if self.host.can_audition() && !self.host.transport(cx).playing {
            let mut engine = self.session.shared().lock();
            engine.trigger_note_audition(i);
            self.session.shared().publish(&engine);
            self.auditioning = true;
        }
        if !self.editable() || self.session.notes()[i].unpitched && self.tool != VoxTool::Time {
            return;
        }
        let n = self.session.notes()[i].clone();
        self.session.begin(self.tool.undo_label());
        self.drag = Some(match self.tool {
            VoxTool::Time => Drag::Time {
                index: i,
                edge: self.timing_edge(i, local.0),
                anchor_t: self.view.x_to_time(local.0),
                start: n.start,
                dur: n.dur,
            },
            tool => Drag::Value {
                index: i,
                tool,
                anchor_y: local.1,
                start: match tool {
                    VoxTool::Pitch => n.pitch,
                    VoxTool::Drift => n.modulation,
                    VoxTool::Formant => n.formant,
                    VoxTool::Level => n.gain_db,
                    VoxTool::Time | VoxTool::Cut => 0.0,
                },
            },
        });
        cx.notify();
    }

    fn on_grid_move(&mut self, ev: &MouseMoveEvent, cx: &mut Context<Self>) {
        let Some(local) = self.grid_local(ev.position) else {
            return;
        };
        let pressed = ev.pressed_button.is_some();
        self.drag_fine = ev.modifiers.shift;
        let moved = self.pointer != Some(local);
        self.pointer = Some(local);
        match self.drag {
            Some(Drag::Pan { anchor, view }) if pressed => {
                let (w, h) = self.grid_size();
                self.view.t0 = view.t0 - (local.0 - anchor.0) / view.pps;
                self.view.top_pitch = view.top_pitch + (local.1 - anchor.1) / view.row_h;
                self.view.clamp(self.session.duration(), w, h);
                cx.notify();
            }
            Some(Drag::Value {
                index,
                tool,
                anchor_y,
                start,
            }) if pressed => {
                let dy = anchor_y - local.1;
                let fine = ev.modifiers.shift;
                let key = self.session.doc().key;
                let scale = self.session.doc().scale;
                let row = self.view.row_h.max(1.0);
                if let Some(n) = self.session.note_mut(index) {
                    match tool {
                        VoxTool::Pitch => {
                            let raw = start + dy / row;
                            // Snap the centre to a semitone; Shift moves freely.
                            // Shift moves freely whatever the snap.
                            n.pitch = match (fine, self.snap) {
                                (true, _) | (false, Snap::Free) => raw,
                                (false, Snap::Semi) => raw.round(),
                                (false, Snap::Scale) => {
                                    sphere_voxtune::edit::nearest_scale_note(raw, key, scale) as f32
                                }
                            };
                            n.pinned = true;
                            n.lyric = solfege_for_midi(n.pitch, key);
                        }
                        VoxTool::Drift => {
                            let k = if fine { 0.001 } else { 0.005 };
                            n.modulation = (start + dy * k).clamp(0.0, 1.45);
                        }
                        VoxTool::Formant => {
                            let k = if fine { 0.01 } else { 0.05 };
                            n.formant =
                                ((start + dy * k) * 10.0).round().clamp(-120.0, 120.0) / 10.0;
                        }
                        VoxTool::Level => {
                            let k = if fine { 0.02 } else { 0.1 };
                            n.gain_db =
                                ((start + dy * k) * 10.0).round().clamp(-180.0, 60.0) / 10.0;
                        }
                        VoxTool::Time | VoxTool::Cut => {}
                    }
                }
                self.session.sync_note(index);
                cx.notify();
            }
            Some(Drag::Time {
                index,
                edge,
                anchor_t,
                start,
                dur,
            }) if pressed => {
                let dt = self.view.x_to_time(local.0) - anchor_t;
                if let Some(n) = self.session.note_mut(index) {
                    match edge {
                        TimingEdge::Body => n.start = start + dt,
                        TimingEdge::Start => {
                            n.start = start + dt;
                            n.dur = dur - dt;
                        }
                        TimingEdge::End => n.dur = dur + dt,
                    }
                }
                self.session.clamp_timing(index, edge);
                self.session.sync_note(index);
                cx.notify();
            }
            Some(Drag::Seek) if pressed => self.seek_to_x(local.0, cx),
            Some(_) => self.finish_pointer(cx),
            None => {
                let hovered = self.note_at(local);
                // The Cut tool's line follows the pointer over a note.
                let cut_line = self.tool == VoxTool::Cut && hovered.is_some() && moved;
                if hovered != self.hovered || cut_line {
                    self.hovered = hovered;
                    cx.notify();
                }
            }
        }
    }

    /// The pointer left the note area: hover and tool previews end.
    fn leave_grid(&mut self, cx: &mut Context<Self>) {
        if self.drag.is_none() && (self.pointer.is_some() || self.hovered.is_some()) {
            self.pointer = None;
            self.hovered = None;
            cx.notify();
        }
    }

    fn finish_pointer(&mut self, cx: &mut Context<Self>) {
        if self.auditioning {
            let mut engine = self.session.shared().lock();
            engine.release_audition();
        }
        self.end_gesture(cx);
        cx.notify();
    }

    fn stop_audition(&mut self) {
        if self.auditioning {
            self.session.shared().lock().stop_audition();
            self.auditioning = false;
        }
    }

    fn on_wheel(&mut self, ev: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let (dx, dy) = match ev.delta {
            gpui::ScrollDelta::Pixels(p) => (f32::from(p.x), f32::from(p.y)),
            gpui::ScrollDelta::Lines(p) => (p.x * WHEEL_LINE_PX, p.y * WHEEL_LINE_PX),
        };
        let (w, h) = self.grid_size();
        let local = self.grid_local(ev.position).unwrap_or((w * 0.5, h * 0.5));
        let m = ev.modifiers;
        if m.control || m.platform {
            self.view.zoom_time(1.0015f32.powf(dy), local.0);
        } else if m.alt {
            self.view.zoom_pitch(1.0015f32.powf(dy), local.1);
        } else if m.shift {
            self.view.t0 -= (dy + dx) / self.view.pps;
        } else {
            self.view.top_pitch += dy / self.view.row_h;
            self.view.t0 -= dx / self.view.pps;
        }
        self.follow = false;
        self.view.clamp(self.session.duration(), w, h);
        cx.notify();
    }

    fn seek_to_x(&mut self, x: f32, cx: &mut Context<Self>) {
        let t = self.view.x_to_time(x).clamp(0.0, self.session.duration()) as f64;
        self.host.seek(t, cx);
        cx.notify();
    }

    fn toggle_play(&mut self, cx: &mut Context<Self>) {
        if !self.host.controls_transport() || !self.has_audio() {
            return;
        }
        let playing = self.host.transport(cx).playing;
        if !playing {
            self.stop_audition();
            self.follow = true;
        }
        self.host.set_playing(!playing, cx);
        cx.notify();
    }

    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        let cmd = k.modifiers.control || k.modifiers.platform;
        match k.key.as_str() {
            "z" if cmd && k.modifiers.shift => self.redo(cx),
            "z" if cmd => self.undo(cx),
            "y" if cmd => self.redo(cx),
            "space" => self.toggle_play(cx),
            "home" => {
                self.host.seek(0.0, cx);
                self.view.t0 = 0.0;
                cx.notify();
            }
            "1" | "2" | "3" | "4" | "5" | "6" if !cmd => {
                let i = k.key.parse::<usize>().unwrap_or(1) - 1;
                self.tool = VoxTool::ALL[i];
                cx.notify();
            }
            "up" => self.nudge_pitch(if k.modifiers.shift { 0.1 } else { 1.0 }, cx),
            "down" => self.nudge_pitch(if k.modifiers.shift { -0.1 } else { -1.0 }, cx),
            "left" | "right" => {
                let len = self.session.notes().len();
                if len > 0 {
                    let next = match (self.selected, k.key.as_str()) {
                        (None, _) => 0,
                        (Some(i), "left") => i.saturating_sub(1),
                        (Some(i), _) => (i + 1).min(len - 1),
                    };
                    self.select(Some(next), cx);
                }
            }
            "delete" | "backspace" => {
                if let Some(i) = self.selected.filter(|_| self.editable()) {
                    self.session.toggle_mute(i);
                    self.changed(cx);
                }
            }
            "j" if !cmd && self.editable() => self.join_selected(cx),
            "f" if !cmd => {
                self.fit_view();
                cx.notify();
            }
            "escape" => {
                if self.session.is_open() {
                    self.session.cancel();
                    self.drag = None;
                    self.changed(cx);
                } else if self.menu.is_some() {
                    self.menu = None;
                    cx.notify();
                } else {
                    self.select(None, cx);
                }
            }
            "=" | "+" if cmd => self.zoom(1.25, cx),
            "-" if cmd => self.zoom(0.8, cx),
            _ => return,
        }
        let _ = window;
        cx.stop_propagation();
    }

    fn zoom(&mut self, factor: f32, cx: &mut Context<Self>) {
        let (w, h) = self.grid_size();
        self.view.zoom_time(factor, w * 0.5);
        self.view.clamp(self.session.duration(), w, h);
        cx.notify();
    }

    fn notes_snapshot(&mut self) -> Rc<Vec<CantoNote>> {
        let rev = self.session.revision();
        if self.notes_cache.0 != rev {
            self.notes_cache = (rev, Rc::new(self.session.notes().to_vec()));
        }
        self.notes_cache.1.clone()
    }
}

/// Saved edits fit a take when every note lies inside it.
fn fits_take(doc: &VoxDocument, duration: f32) -> bool {
    !doc.notes.is_empty()
        && doc
            .notes
            .iter()
            .all(|n| n.detected_start + n.detected_dur <= duration + 0.1)
}

fn format_cents(cents: f32) -> String {
    let c = cents.round() as i32;
    if c == 0 {
        "0 c".into()
    } else {
        format!("{c:+} c")
    }
}

/// Scientific note name, `A4`.
pub fn note_name(midi: i32) -> String {
    const NAMES: [&str; 12] = [
        "C", "C#", "D", "Eb", "E", "F", "F#", "G", "Ab", "A", "Bb", "B",
    ];
    format!(
        "{}{}",
        NAMES[midi.rem_euclid(12) as usize],
        midi.div_euclid(12) - 1
    )
}

/// Bars.beats readout at `bpm`, 1-based.
fn format_position(sec: f32, bpm: f32) -> String {
    let beats = sec.max(0.0) / (60.0 / bpm.max(1.0));
    let bar = (beats / 4.0).floor() as i32 + 1;
    let beat = (beats % 4.0).floor() as i32 + 1;
    format!("{bar}.{beat}")
}

fn format_clock(sec: f64) -> String {
    let s = sec.max(0.0);
    let m = (s / 60.0).floor() as i32;
    format!("{m}:{:05.2}", s - m as f64 * 60.0)
}

/// Human label for a history code (the phone's codes).
pub fn history_label(code: &str) -> String {
    if code.is_empty() {
        return "Opened take".into();
    }
    if let Some(key) = code.strip_prefix("key:") {
        return format!("Key {key}");
    }
    if let Some(mode) = code.strip_prefix("mode:") {
        return format!("Scale {mode}");
    }
    if let Some(n) = code.strip_prefix("adv:transpose:") {
        return format!(
            "Transpose {}",
            n.parse::<i32>()
                .map(|v| format!("{v:+}"))
                .unwrap_or_default()
        );
    }
    match code {
        "cut" => "Cut note",
        "join" => "Join notes",
        "mute" => "Mute note",
        "unmute" => "Unmute note",
        "reset" => "Reset note",
        "tune" => "Tune note",
        "correct" => "Correction",
        "tool:pitch" => "Pitch",
        "tool:drift" => "Drift",
        "tool:time" => "Timing",
        "tool:formant" => "Formant",
        "tool:level" => "Level",
        "orn" => "Ornament",
        "accent" => "Accent",
        "exprClear" => "Clear expression",
        "adv:center" => "Center pitch",
        "adv:drift" => "Flatten drift",
        "adv:quantize" => "Quantize timing",
        "adv:level" => "Even out level",
        "adv:accentBeats" => "Accent on beats",
        other => other,
    }
    .into()
}

pub fn ornament_name(style: u8) -> &'static str {
    match style {
        ORN_TURN_UP => "Flick up",
        ORN_TURN_DOWN => "Flick down",
        ORN_SHAKE => "Shake",
        ORN_FALL => "Fall",
        _ => "None",
    }
}

impl VoxtuneEditor {
    /// The current committed analysis-free document as JSON (plug-in state,
    /// session files).
    pub fn document_json(&self) -> String {
        self.session.doc().to_json()
    }

    /// Replaces the take's analysis result shown, e.g. after the host
    /// re-analysed. Resets history.
    pub fn adopt_analysis(&mut self, result: &CantoAnalysisResult, cx: &mut Context<Self>) {
        self.install(VoxDocument::from_analysis(result), cx);
    }
}

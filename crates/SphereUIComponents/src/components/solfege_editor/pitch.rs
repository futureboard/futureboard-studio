//! The Pitch tab: a continuous musical pitch-performance editor.
//!
//! ```text
//! Select Draw Line Erase │ Smooth Transition Vibrato Reset │ − + Fit  +17 ct
//! ────────────────────────────────────────────────────────────────────────
//!        │      3.1        3.2        3.3         4
//! ───────┼────────────────────────────────────────────────────────────────
//!  C5    │                 ┌────────────────┐
//!        │            _____╯~~~~~~~╲________╰──
//!  B4    │      _____/
//!  A4    │ ┌──────────────┐
//!        │─╯~~~~╲_________╰────────────────────
//! ───────┴────────────────────────────────────────────────────────────────
//!  Wave  │ ▂▄▆▃▂▅▆▄▂▃▅▇▃▂▄▆
//! ───────┼────────────────────────────────────────────────────────────────
//!  ff    │ Dynamics
//!  mp    │ ────●─────────●────
//!  ppp   │
//! ```
//!
//! What is on screen is the **evaluated trajectory**, not the stored points.
//! [`PitchTrajectory`] composes the note baseline, note-to-note transitions and
//! the manual [`PitchCurve`] into one continuous line per voice, so:
//!
//! - a note nobody has touched still draws a line at its notated pitch;
//! - connected notes share one unbroken line through the transition;
//! - control points are editing affordances shown only where the user is
//!   working, never the primary visual.
//!
//! Every surface here — ruler, grid, keyboard, curve, waveform, dynamics —
//! renders through the piano roll's [`PianoRollViewport`]. That is the whole
//! synchronization story: one transform, on both axes, so the MIDI and Pitch
//! tabs cannot drift apart and scrolling either scrolls both.

use std::cell::Cell;
use std::collections::HashSet;
use std::rc::Rc;

use gpui::prelude::FluentBuilder;
use gpui::AppContext as _;
use gpui::{
    canvas, div, fill, point, px, Bounds, Context, FocusHandle, InteractiveElement, IntoElement,
    KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, ParentElement, Pixels,
    ScrollWheelEvent, StatefulInteractiveElement, Styled, Window,
};

use crate::components::edit::edit_commands::EditCommand;
use crate::components::piano_roll::{is_black, note_name, EditorMeter, PianoRollViewport};
use crate::components::timeline::timeline_state::{
    midi_edit_revision, ArticulationId, MidiNoteState, PitchCurve, PitchPoint, PitchSegmentShape,
    PitchTrajectory, ABUT_EPS_BEATS, LEGATO_BRIDGE_BEATS, PITCH_CURVE_MAX_CENTS,
};
use crate::components::timeline::waveform_cache;
use crate::solfege::{LaneSource, SolfegeLaneVisibility};
use crate::theme::Colors;

use super::{SolfegeEditContext, SolfegeEditorPanel};

/// Compact toolbar height.
const PITCH_TOOLBAR_H: f32 = 28.0;
/// Bar/beat ruler height.
const PITCH_RULER_H: f32 = 22.0;
/// Waveform strip height — timing context, not a second audio editor.
const WAVEFORM_H: f32 = 64.0;
/// Merge tolerance while drawing, in beats. Keeps one stroke from leaving a
/// breakpoint per pixel column.
const PITCH_MERGE_BEATS: f32 = 1.0 / 96.0;
/// Reconstruction error a finished stroke may be simplified by, in cents.
///
/// Under the ~5 cent just-noticeable difference for a sustained tone, so
/// collapsing a per-sample stroke to a handful of breakpoints is inaudible —
/// and it is what turns a drawn line into something a human can afterwards drag
/// point by point instead of a wall of coincident handles.
const PITCH_SIMPLIFY_CENTS: f32 = 2.5;
/// Pointer distance that counts as "on" a control point. Deliberately far wider
/// than the 2.5–3.5 px dot that is drawn: the handle stays visually compact
/// while remaining grabbable at workstation density.
const POINT_HIT_PX: f32 = 9.0;
/// Extra vertical reach outside a note's own row that still targets the note.
const NOTE_HIT_SLOP_PX: f32 = 4.0;
/// Grab band around the drawn pitch line. Without it a curve that scoops out of
/// its note's row would be plainly visible yet unclickable, because the row band
/// is the only other way in.
const CURVE_HIT_PX: f32 = 9.0;
/// Wheel lines to pixels, and the continuous zoom base. Both match
/// `PianoRoll::on_wheel` so one notch does the same thing in either tab.
const WHEEL_LINE_PX: f32 = 36.0;
const WHEEL_ZOOM_BASE: f32 = 1.0022;
/// Vibrato defaults, in cents and beats. Deliberately gentle: an instantly wide
/// vibrato reads as synthetic on every instrument family.
const VIBRATO_DEPTH_CENTS: f32 = 26.0;
const VIBRATO_CYCLE_BEATS: f32 = 0.25;
/// Fraction of the range the vibrato waits before starting.
const VIBRATO_ONSET_FRACTION: f32 = 0.25;

/// Pointer mode. The one-shot operations ([`PitchAction`]) are buttons, not
/// modes, so the pointer never silently changes meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PitchTool {
    Select,
    #[default]
    Draw,
    /// Drag a straight pitch ramp between two positions.
    Line,
    Erase,
}

impl PitchTool {
    const ALL: [PitchTool; 4] = [
        PitchTool::Select,
        PitchTool::Draw,
        PitchTool::Line,
        PitchTool::Erase,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Select => "Select",
            Self::Draw => "Draw",
            Self::Line => "Line",
            Self::Erase => "Erase",
        }
    }

    fn tooltip(self) -> &'static str {
        match self {
            Self::Select => "Select and move pitch points",
            Self::Draw => "Draw pitch freehand",
            Self::Line => "Drag a straight pitch ramp",
            Self::Erase => "Erase pitch points",
        }
    }
}

/// One-shot operations on the selected note, or on the selected point range.
///
/// Each writes ordinary editable breakpoints — nothing here is a hidden
/// modulator, and every result can be dragged point by point afterwards. A
/// future FBMX generator becomes another entry producing the same data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PitchAction {
    Smooth,
    Transition,
    Vibrato,
    Reset,
}

impl PitchAction {
    const ALL: [PitchAction; 4] = [
        PitchAction::Smooth,
        PitchAction::Transition,
        PitchAction::Vibrato,
        PitchAction::Reset,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Smooth => "Smooth",
            Self::Transition => "Transition",
            Self::Vibrato => "Vibrato",
            Self::Reset => "Reset",
        }
    }

    fn tooltip(self) -> &'static str {
        match self {
            Self::Smooth => "Smooth the selected range without flattening it",
            Self::Transition => "Connect this note to the next so their pitch glides",
            Self::Vibrato => "Generate an editable vibrato over the note",
            Self::Reset => "Clear manual pitch and return to the performance baseline",
        }
    }
}

/// A live pointer gesture. `prev` is the whole note as it stood before the
/// first sample, so release records one `EditMidiNotes` entry per gesture.
#[derive(Debug, Clone)]
enum PitchDrag {
    /// Freehand draw or erase on one note.
    Paint {
        clip_id: String,
        note_id: u64,
        prev: MidiNoteState,
    },
    /// Straight ramp between the press and the current pointer.
    Line {
        clip_id: String,
        note_id: u64,
        prev: MidiNoteState,
        from_beat: f32,
        from_cents: f32,
    },
    /// Move the selected points of one note in time and pitch.
    Move {
        clip_id: String,
        note_id: u64,
        prev: MidiNoteState,
        /// `(point id, original beat, original cents)` for each moved point.
        origin: Vec<(u64, f32, f32)>,
        anchor_beat: f32,
        anchor_cents: f32,
    },
    /// Rubber-band point selection.
    Marquee {
        origin: (f32, f32),
        current: (f32, f32),
        base: HashSet<u64>,
    },
}

/// Notes and evaluated trajectory as the canvas last built them.
///
/// The canvas closure outlives the render pass, so it needs owned data; and
/// [`PitchTrajectory::build`] does the whole voice assignment and span
/// construction. Without this, both ran on every frame — including every
/// mouse-move of a drag.
///
/// Validity is one integer compare against [`midi_edit_revision`]. It was a
/// field-by-field comparison of the cached `Vec<MidiNoteState>` against the
/// live one, which sounds cheap and is not: it walks every note *and every
/// pitch point of every note*, and on a clip carrying a few hundred drawn
/// points it measured at **five times the cost of the rebuild it was avoiding**
/// — paid on every frame, including the ones where nothing had changed. A
/// cache whose miss is cheaper than its hit test is not a cache.
struct PitchCanvasCache {
    clip_id: String,
    /// The MIDI edit revision this was built from.
    revision: u64,
    notes: Rc<Vec<MidiNoteState>>,
    trajectory: Rc<PitchTrajectory>,
}

pub struct PitchEditorState {
    tool: PitchTool,
    /// The note the tools act on. Its control points are the ones shown.
    selected_note: Option<u64>,
    /// Selected pitch points, scoped to `selected_note`.
    selected_points: HashSet<u64>,
    /// Note under the pointer — its points are revealed on hover.
    hovered_note: Option<u64>,
    /// Control point under the pointer. Only drives the grab cursor; the press
    /// re-resolves it against the same radius.
    hovered_point: Option<u64>,
    /// Compact readout, e.g. `+17 ct`, for the hovered or dragged point.
    readout: Option<String>,
    drag: Option<PitchDrag>,
    /// Note-relative beat range a freehand stroke has written into. Drives the
    /// single simplification pass on release.
    stroke_range: Option<(f32, f32)>,
    canvas: Option<PitchCanvasCache>,
    grid_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// Claimed on click so Delete reaches this surface. Never taken during
    /// render, so opening the tab does not steal focus from the rest of the app.
    focus: FocusHandle,
}

impl PitchEditorState {
    pub fn new(cx: &mut Context<SolfegeEditorPanel>) -> Self {
        Self {
            tool: PitchTool::default(),
            selected_note: None,
            selected_points: HashSet::new(),
            hovered_note: None,
            hovered_point: None,
            readout: None,
            drag: None,
            stroke_range: None,
            canvas: None,
            grid_bounds: Rc::new(Cell::new(None)),
            focus: cx.focus_handle(),
        }
    }

    /// The note the pitch tools act on, for the Inspector.
    pub(super) fn selected_note(&self) -> Option<u64> {
        self.selected_note
    }

    /// `true` while the pitch canvas holds keyboard focus. The studio's
    /// capture-phase shortcut router consults this so Delete edits pitch points
    /// here instead of deleting the arrangement clip.
    pub(super) fn is_focused(&self, window: &Window) -> bool {
        self.focus.is_focused(window)
    }

    fn local(&self, position: gpui::Point<Pixels>) -> Option<(f32, f32)> {
        let bounds = self.grid_bounds.get()?;
        Some((
            f32::from(position.x - bounds.origin.x),
            f32::from(position.y - bounds.origin.y),
        ))
    }

    fn grid_size(&self) -> (f32, f32) {
        match self.grid_bounds.get() {
            Some(b) => (
                f32::from(b.size.width).max(1.0),
                f32::from(b.size.height).max(1.0),
            ),
            None => (600.0, 240.0),
        }
    }
}

fn format_cents(cents: f32) -> String {
    format!("{cents:+.0} ct")
}

/// Multiplicative step for one Zoom +/- button press. Matches the wheel's own
/// feel — a few clicks to double, not one jarring jump.
const PITCH_ZOOM_BUTTON_FACTOR: f32 = 1.25;
/// Semitones of headroom the Fit button leaves above and below the range it
/// centers, so the trajectory never sits flush against the grid edge.
const PITCH_FIT_PAD_SEMITONES: f32 = 2.0;
/// Fallback pitch range for Fit when the clip has no notes yet: one octave
/// around middle C, the same neutral center the piano roll itself defaults to.
const PITCH_FIT_DEFAULT_RANGE: (f32, f32) = (54.0, 66.0);

impl SolfegeEditorPanel {
    pub(super) fn render_pitch_tab(
        &mut self,
        context: Option<SolfegeEditContext>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let toolbar = self.render_pitch_toolbar(context.as_ref(), cx);
        let viewport = self.viewport(cx);
        // The project's meter, so this tab's bars sit where the piano roll's do.
        let meter = EditorMeter::from_map(&self.timeline.read(cx).state.time_signature_map);
        let Some(ctx) = context else {
            return div()
                .flex()
                .flex_col()
                .size_full()
                .min_h(px(0.0))
                .child(toolbar)
                .child(self.render_pitch_ruler(None, viewport, &meter, cx))
                .child(pitch_empty_canvas(viewport))
                .into_any_element();
        };

        let ruler = self.render_pitch_ruler(Some(&ctx), viewport, &meter, cx);
        let grid = self.render_pitch_grid(&ctx, viewport, &meter, cx);
        let waveform = self.render_pitch_waveform(&ctx, viewport, cx);
        let support = self.render_pitch_support_lane(&ctx, cx);

        div()
            .flex()
            .flex_col()
            .size_full()
            .min_h(px(0.0))
            .child(toolbar)
            .child(ruler)
            .child(grid)
            .children(waveform)
            .children(support)
            .into_any_element()
    }

    // ── Toolbar ──────────────────────────────────────────────────────────

    fn pitch_button(
        &self,
        id: (&'static str, usize),
        label: &'static str,
        tooltip: &'static str,
        active: bool,
        enabled: bool,
        on_click: impl Fn(&mut Self, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        div()
            .id(id)
            .flex()
            .items_center()
            .h(px(18.0))
            .px(px(7.0))
            .rounded(px(crate::theme::radius::CONTROL))
            // Active is accent background *and* accent border; hover is only a
            // weaker graphite wash, and only on a tool that is not already
            // armed. Painting both with `surface_hover` made a hovered tool
            // indistinguishable from the selected one.
            .bg(if active {
                Colors::accent_muted()
            } else {
                Colors::surface_input()
            })
            .border(px(1.0))
            .border_color(if active {
                Colors::accent_primary()
            } else {
                Colors::with_alpha(Colors::text_primary(), 0.0)
            })
            .text_size(px(9.5))
            .text_color(if !enabled {
                Colors::text_disabled()
            } else if active {
                Colors::text_primary()
            } else {
                Colors::text_secondary()
            })
            .when(enabled, |style| {
                style.cursor(gpui::CursorStyle::PointingHand)
            })
            .when(enabled && !active, |style| {
                style.hover(|style| style.bg(Colors::surface_hover()))
            })
            .tooltip(pitch_tooltip(tooltip))
            .on_click(cx.listener(move |this, _event, _window, cx| {
                if enabled {
                    on_click(this, cx);
                }
            }))
            .child(label)
            .into_any_element()
    }

    fn render_pitch_toolbar(
        &self,
        ctx: Option<&SolfegeEditContext>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let active_tool = self.pitch.tool;
        let has_note = ctx.is_some() && self.pitch.selected_note.is_some();
        let has_points = has_note && !self.pitch.selected_points.is_empty();

        let tools: Vec<gpui::AnyElement> = PitchTool::ALL
            .into_iter()
            .enumerate()
            .map(|(index, tool)| {
                self.pitch_button(
                    ("solfege-pitch-tool", index),
                    tool.label(),
                    tool.tooltip(),
                    tool == active_tool,
                    true,
                    move |this, cx| {
                        this.pitch.tool = tool;
                        cx.notify();
                    },
                    cx,
                )
            })
            .collect();

        let actions: Vec<gpui::AnyElement> = PitchAction::ALL
            .into_iter()
            .enumerate()
            .map(|(index, action)| {
                let ctx_click = ctx.cloned();
                self.pitch_button(
                    ("solfege-pitch-action", index),
                    action.label(),
                    action.tooltip(),
                    false,
                    has_note,
                    move |this, cx| {
                        if let Some(ctx) = ctx_click.clone() {
                            this.apply_pitch_action(&ctx, action, cx);
                        }
                    },
                    cx,
                )
            })
            .collect();

        let delete = has_points.then(|| {
            let ctx_click = ctx.cloned();
            self.pitch_button(
                ("solfege-pitch-action", 100),
                "Delete",
                "Delete the selected pitch points",
                false,
                true,
                move |this, cx| {
                    if let Some(ctx) = ctx_click.clone() {
                        this.delete_selected_pitch_points(&ctx, cx);
                    }
                },
                cx,
            )
        });

        let fit_ctx = ctx.cloned();
        let zoom: Vec<gpui::AnyElement> = [
            self.pitch_button(
                ("solfege-pitch-zoom", 0),
                "\u{2212}",
                "Zoom out on pitch",
                false,
                true,
                |this, cx| this.zoom_pitch_view(1.0 / PITCH_ZOOM_BUTTON_FACTOR, cx),
                cx,
            ),
            self.pitch_button(
                ("solfege-pitch-zoom", 1),
                "+",
                "Zoom in on pitch",
                false,
                true,
                |this, cx| this.zoom_pitch_view(PITCH_ZOOM_BUTTON_FACTOR, cx),
                cx,
            ),
            self.pitch_button(
                ("solfege-pitch-zoom", 2),
                "Fit",
                "Fit pitch zoom to the selected note, or the whole clip",
                false,
                fit_ctx.is_some(),
                move |this, cx| {
                    if let Some(ctx) = fit_ctx.clone() {
                        this.fit_pitch_view(&ctx, cx);
                    }
                },
                cx,
            ),
        ]
        .into();

        let readout = self.pitch.readout.clone().map(|text| {
            div()
                .ml_auto()
                .px(px(6.0))
                .text_size(px(9.5))
                .text_color(Colors::accent_primary())
                .child(text)
        });

        div()
            .id("solfege-pitch-toolbar")
            .flex()
            .flex_row()
            .items_center()
            .flex_none()
            .w_full()
            .h(px(PITCH_TOOLBAR_H))
            .px(px(6.0))
            .gap(px(4.0))
            .border_b(px(1.0))
            .border_color(Colors::border_subtle())
            .bg(Colors::surface_titlebar())
            .children(tools)
            .child(
                div()
                    .w(px(1.0))
                    .h(px(12.0))
                    .mx(px(3.0))
                    .bg(Colors::border_subtle()),
            )
            .children(actions)
            .children(delete)
            .child(
                div()
                    .w(px(1.0))
                    .h(px(12.0))
                    .mx(px(3.0))
                    .bg(Colors::border_subtle()),
            )
            .children(zoom)
            .children(readout)
    }

    /// Zoom Y around the pitch canvas's own vertical center. A button press
    /// has no cursor position to anchor on, unlike the Alt+scroll gesture the
    /// grid also supports, so the view center is the next best fixed point.
    fn zoom_pitch_view(&mut self, factor: f32, cx: &mut Context<Self>) {
        let (_, view_h) = self.pitch.grid_size();
        self.piano_roll.update(cx, |roll, rcx| {
            roll.zoom_viewport_vertically(factor, view_h, view_h * 0.5);
            rcx.notify();
        });
        cx.notify();
    }

    /// Zoom and scroll the pitch axis to fit the selected note, or the whole
    /// clip when nothing is selected.
    fn fit_pitch_view(&mut self, ctx: &SolfegeEditContext, cx: &mut Context<Self>) {
        let (_, view_h) = self.pitch.grid_size();
        let (min_pitch, max_pitch) = self.pitch_fit_range(&ctx.clip_id, cx);
        self.piano_roll.update(cx, |roll, rcx| {
            roll.fit_viewport_vertically(view_h, min_pitch, max_pitch);
            rcx.notify();
        });
        cx.notify();
    }

    /// Pitch span the Fit button should bring into view.
    ///
    /// Uses the selected note's range when one is selected, otherwise every
    /// note in the clip, and in both cases the drawn curve's extremes rather
    /// than just the notated pitch — a scoop or fall is exactly what a
    /// performer wants centered, not clipped at the row's edge.
    fn pitch_fit_range(&self, clip_id: &str, cx: &Context<Self>) -> (f32, f32) {
        let notes = self
            .timeline
            .read(cx)
            .state
            .midi_clip_notes(clip_id)
            .cloned()
            .unwrap_or_default();
        let target: Vec<&MidiNoteState> = match self.pitch.selected_note {
            Some(id) => notes.iter().filter(|n| n.id == id).collect(),
            None => notes.iter().collect(),
        };
        if target.is_empty() {
            return PITCH_FIT_DEFAULT_RANGE;
        }
        let (mut lo, mut hi) = (f32::INFINITY, f32::NEG_INFINITY);
        for note in target {
            let base = note.pitch as f32;
            let (curve_lo, curve_hi) = note
                .pitch_curve
                .as_ref()
                .map(|curve| {
                    curve.points.iter().fold((0.0f32, 0.0f32), |(lo, hi), p| {
                        (lo.min(p.cents), hi.max(p.cents))
                    })
                })
                .unwrap_or((0.0, 0.0));
            lo = lo.min(base + curve_lo / 100.0);
            hi = hi.max(base + curve_hi / 100.0);
        }
        (lo - PITCH_FIT_PAD_SEMITONES, hi + PITCH_FIT_PAD_SEMITONES)
    }

    // ── Ruler ────────────────────────────────────────────────────────────

    /// The bar/beat ruler, aligned to the shared viewport.
    ///
    /// Mark geometry comes from [`PianoRollViewport::ruler_marks`] — the same
    /// call the piano roll's own ruler makes — so both tabs label the same
    /// beats at the same pixels. Clicking seeks the transport.
    fn render_pitch_ruler(
        &self,
        ctx: Option<&SolfegeEditContext>,
        viewport: PianoRollViewport,
        meter: &EditorMeter,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (view_w, _) = self.pitch.grid_size();
        let (start_beat, end_beat) = viewport.visible_beats(view_w);
        let labels: Vec<gpui::AnyElement> = viewport
            .ruler_marks_in(start_beat, end_beat, meter)
            .into_iter()
            .flat_map(|mark| {
                [
                    div()
                        .absolute()
                        .top_0()
                        .left(px(mark.x + 2.0))
                        .text_size(px(8.5))
                        .text_color(if mark.on_bar {
                            Colors::text_secondary()
                        } else {
                            Colors::text_muted()
                        })
                        .child(mark.label)
                        .into_any_element(),
                    div()
                        .absolute()
                        .left(px(mark.x))
                        .bottom_0()
                        .w(px(1.0))
                        .h(px(if mark.on_bar { 6.0 } else { 4.0 }))
                        .bg(Colors::with_alpha(
                            Colors::text_primary(),
                            if mark.on_bar { 0.26 } else { 0.13 },
                        ))
                        .into_any_element(),
                ]
            })
            .collect();

        let clip_start = ctx.map(|c| c.clip_start_beat).unwrap_or(0.0);
        let seekable = ctx.is_some();

        div()
            .id("solfege-pitch-ruler")
            .flex()
            .flex_row()
            .flex_none()
            .w_full()
            .h(px(PITCH_RULER_H))
            .border_b(px(1.0))
            .border_color(Colors::panel_border())
            .bg(Colors::surface_panel())
            .child(
                // Spacer that keeps the ruler's beat 0 above the grid's beat 0.
                div()
                    .flex_none()
                    .h_full()
                    .w(px(viewport.key_lane_width.max(48.0)))
                    .border_r(px(1.0))
                    .border_color(Colors::border_subtle()),
            )
            .child(
                div()
                    .id("solfege-pitch-ruler-body")
                    .flex_1()
                    .h_full()
                    .relative()
                    .overflow_hidden()
                    .when(seekable, |style| {
                        style.cursor(gpui::CursorStyle::PointingHand)
                    })
                    .children(labels)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseDownEvent, _window, cx| {
                            if !seekable {
                                return;
                            }
                            cx.stop_propagation();
                            // The ruler shares the grid's horizontal origin, so
                            // the grid's captured bounds resolve the beat.
                            let Some((local_x, _)) = this.pitch.local(event.position) else {
                                return;
                            };
                            let viewport = this.viewport(cx);
                            let beat = viewport.snap(viewport.x_to_beat(local_x)).max(0.0);
                            this.timeline.update(cx, |tl, tcx| {
                                tl.seek_to_beat(clip_start + beat, tcx);
                            });
                        }),
                    ),
            )
    }

    // ── Pitch canvas ─────────────────────────────────────────────────────

    fn render_pitch_grid(
        &mut self,
        ctx: &SolfegeEditContext,
        viewport: PianoRollViewport,
        meter: &EditorMeter,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (playhead_rel, playing) = {
            let state = &self.timeline.read(cx).state;
            (
                state.transport.playhead_beats - ctx.clip_start_beat,
                state.transport.playing,
            )
        };
        let (notes, trajectory) = self.pitch_canvas_data(&ctx.clip_id, cx);
        let empty = notes.is_empty();

        let keys = self.render_pitch_keys(viewport);
        let painter = self.build_pitch_canvas(notes, trajectory, viewport, meter.clone());
        let marquee = match &self.pitch.drag {
            Some(PitchDrag::Marquee {
                origin, current, ..
            }) => Some(marquee_overlay(*origin, *current)),
            _ => None,
        };
        let playhead = (playhead_rel >= 0.0 && playhead_rel <= ctx.clip_beats).then(|| {
            div()
                .absolute()
                .top_0()
                .left(px(viewport.beat_to_x(playhead_rel)))
                .w(px(1.0))
                .h_full()
                .bg(Colors::with_alpha(
                    Colors::status_warning(),
                    if playing { 0.9 } else { 0.45 },
                ))
        });
        let hint = empty.then(|| {
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(10.0))
                .text_color(Colors::text_faint())
                .child("Add notes in MIDI to edit pitch.")
        });

        let ctx_down = ctx.clone();
        let ctx_move = ctx.clone();
        let ctx_key = ctx.clone();
        let focus = self.pitch.focus.clone();
        let cursor = self.pitch_cursor_style();

        div()
            .id("solfege-pitch-canvas")
            .flex()
            .flex_row()
            .flex_1()
            .min_h(px(0.0))
            .w_full()
            .overflow_hidden()
            .child(keys)
            .child(
                div()
                    .id("solfege-pitch-grid")
                    .flex_1()
                    .h_full()
                    .relative()
                    .overflow_hidden()
                    .bg(Colors::surface_base())
                    .track_focus(&focus)
                    .cursor(cursor)
                    .child(painter)
                    .children(playhead)
                    .children(marquee)
                    .children(hint)
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _window, cx| {
                        match event.keystroke.key.as_str() {
                            "delete" | "backspace" => {
                                this.delete_selected_pitch_points(&ctx_key, cx)
                            }
                            "escape" => {
                                this.pitch.selected_points.clear();
                                cx.notify();
                            }
                            _ => {}
                        }
                    }))
                    .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _window, cx| {
                        // Same modifier map, line multiplier, zoom base and
                        // natural-scroll lookup as `PianoRoll::on_wheel`: one
                        // notch must mean the same thing in either tab, because
                        // both drive the one shared viewport. Alt = Zoom Y,
                        // Shift = horizontal scroll, plain = both scrolls.
                        let (dx, dy) = match event.delta {
                            gpui::ScrollDelta::Pixels(p) => (f32::from(p.x), f32::from(p.y)),
                            gpui::ScrollDelta::Lines(p) => {
                                (p.x * WHEEL_LINE_PX, p.y * WHEEL_LINE_PX)
                            }
                        };
                        // Ctrl/Cmd is Zoom X in the MIDI tab. `PianoRoll` owns
                        // `ppb` privately and exposes no horizontal-zoom entry
                        // point, so this surface cannot perform it; it must not
                        // silently zoom the *other* axis under the same
                        // gesture, so the modifier is left to the shell.
                        if event.modifiers.control || event.modifiers.platform {
                            return;
                        }
                        // The piano roll owns the viewport both tabs share, so
                        // scrolling here moves the same state the MIDI tab
                        // reads — the two can never drift apart. The limits,
                        // though, depend on THIS surface's height: the piano
                        // roll is not mounted while the Pitch tab is showing,
                        // so its own captured bounds are stale.
                        let (_, view_h) = this.pitch.grid_size();
                        let anchor_y = this
                            .pitch
                            .local(event.position)
                            .map(|(_, y)| y)
                            .unwrap_or(view_h * 0.5);
                        if event.modifiers.alt {
                            this.piano_roll.update(cx, |roll, rcx| {
                                roll.zoom_viewport_vertically(
                                    WHEEL_ZOOM_BASE.powf(dy),
                                    view_h,
                                    anchor_y,
                                );
                                rcx.notify();
                            });
                            cx.notify();
                            return;
                        }
                        let natural = cx
                            .try_global::<crate::settings::GlobalSettingsModel>()
                            .map(|g| g.0.read(cx).current.editing.mouse.natural_scroll)
                            .unwrap_or(false);
                        let (dx, dy) = if natural { (-dx, -dy) } else { (dx, dy) };
                        let shift = event.modifiers.shift;
                        this.piano_roll.update(cx, |roll, rcx| {
                            if shift {
                                roll.scroll_viewport_by(dy + dx, rcx);
                            } else {
                                roll.scroll_viewport_vertically_by(dy, view_h);
                                if dx != 0.0 {
                                    roll.scroll_viewport_by(dx, rcx);
                                }
                            }
                            rcx.notify();
                        });
                        cx.notify();
                    }))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            this.pitch.focus.clone().focus(window, cx);
                            let Some(local) = this.pitch.local(event.position) else {
                                return;
                            };
                            this.begin_pitch_gesture(&ctx_down, local, &event.modifiers, cx);
                        }),
                    )
                    .on_mouse_move(
                        cx.listener(move |this, event: &MouseMoveEvent, _window, cx| {
                            let Some(local) = this.pitch.local(event.position) else {
                                return;
                            };
                            if event.pressed_button == Some(MouseButton::Left) {
                                this.drag_pitch_gesture(&ctx_move, local, cx);
                            } else {
                                if this.pitch.drag.is_some() {
                                    this.commit_pitch_gesture(cx);
                                }
                                this.update_pitch_hover(&ctx_move, local, cx);
                            }
                        }),
                    )
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _event, _window, cx| this.commit_pitch_gesture(cx)),
                    )
                    .on_mouse_up_out(
                        MouseButton::Left,
                        cx.listener(|this, _event, _window, cx| this.commit_pitch_gesture(cx)),
                    ),
            )
    }

    /// The left pitch keyboard. Uses the piano roll's own key colouring and
    /// note naming so both tabs read as one instrument.
    fn render_pitch_keys(&self, viewport: PianoRollViewport) -> impl IntoElement {
        let (_, view_h) = self.pitch.grid_size();
        let (low, high) = viewport.visible_pitches(view_h);
        let row_h = viewport.row_h.max(1.0);
        // Label every row when there is room, otherwise only the C of each
        // octave — the same policy the piano roll's key lane uses.
        let show_all = row_h >= 14.0;
        let keys: Vec<gpui::AnyElement> = (low..=high)
            .map(|pitch| {
                let black = is_black(pitch);
                let is_c = pitch.rem_euclid(12) == 0;
                div()
                    .absolute()
                    .top(px(viewport.pitch_row_top(pitch as f32)))
                    .left_0()
                    .w_full()
                    .h(px(row_h))
                    .bg(if black {
                        Colors::surface_base()
                    } else {
                        Colors::surface_raised()
                    })
                    .border_b(px(1.0))
                    .border_color(Colors::border_subtle())
                    .flex()
                    .items_center()
                    .justify_end()
                    .pr(px(5.0))
                    .when(is_c || show_all, |style| {
                        style.child(
                            div()
                                .text_size(px(8.0))
                                .text_color(if is_c {
                                    Colors::text_primary()
                                } else if black {
                                    Colors::text_muted()
                                } else {
                                    Colors::text_secondary()
                                })
                                .child(note_name(pitch)),
                        )
                    })
                    .into_any_element()
            })
            .collect();

        div()
            .id("solfege-pitch-keys")
            .flex_none()
            .h_full()
            .w(px(viewport.key_lane_width.max(48.0)))
            .relative()
            .overflow_hidden()
            .border_r(px(1.0))
            .border_color(Colors::border_subtle())
            .bg(Colors::surface_panel())
            .children(keys)
    }

    /// The pointer's meaning, made visible. A drag in flight outranks the tool:
    /// while points are moving the cursor must say "holding", not "ready to
    /// draw".
    fn pitch_cursor_style(&self) -> gpui::CursorStyle {
        if matches!(self.pitch.drag, Some(PitchDrag::Move { .. })) {
            return gpui::CursorStyle::ClosedHand;
        }
        match self.pitch.tool {
            // Open hand over a grabbable handle is the same affordance the
            // arrangement uses for a movable clip.
            PitchTool::Select if self.pitch.hovered_point.is_some() => gpui::CursorStyle::OpenHand,
            PitchTool::Select => gpui::CursorStyle::Arrow,
            PitchTool::Draw | PitchTool::Line => gpui::CursorStyle::Crosshair,
            // The nearest thing to an eraser in gpui's cursor set, and the only
            // option that stays distinct from Draw on every platform.
            PitchTool::Erase => gpui::CursorStyle::OperationNotAllowed,
        }
    }

    /// Notes and evaluated trajectory for the canvas, reusing the previous
    /// frame's build while the clip's note and articulation data are unchanged.
    fn pitch_canvas_data(
        &mut self,
        clip_id: &str,
        cx: &mut Context<Self>,
    ) -> (Rc<Vec<MidiNoteState>>, Rc<PitchTrajectory>) {
        let revision = midi_edit_revision();
        if let Some(cache) = &self.pitch.canvas {
            if cache.clip_id == clip_id && cache.revision == revision {
                return (cache.notes.clone(), cache.trajectory.clone());
            }
        }
        let (notes, directions) = {
            let state = &self.timeline.read(cx).state;
            (
                state.midi_clip_notes(clip_id).cloned().unwrap_or_default(),
                state
                    .midi_clip_articulations(clip_id)
                    .cloned()
                    .unwrap_or_default(),
            )
        };
        let trajectory = Rc::new(PitchTrajectory::build(&notes, &directions));
        let notes = Rc::new(notes);
        self.pitch.canvas = Some(PitchCanvasCache {
            clip_id: clip_id.to_string(),
            revision,
            notes: notes.clone(),
            trajectory: trajectory.clone(),
        });
        (notes, trajectory)
    }

    /// Grid, note regions, evaluated trajectory, and control points — one
    /// canvas, painted back to front so the trajectory reads on top.
    fn build_pitch_canvas(
        &self,
        notes: Rc<Vec<MidiNoteState>>,
        trajectory: Rc<PitchTrajectory>,
        viewport: PianoRollViewport,
        meter: EditorMeter,
    ) -> gpui::AnyElement {
        let capture = self.pitch.grid_bounds.clone();
        let selected_note = self.pitch.selected_note;
        let hovered_note = self.pitch.hovered_note;
        let selected_points = self.pitch.selected_points.clone();

        let row_line = Colors::with_alpha(Colors::text_primary(), 0.035);
        let row_line_c = Colors::with_alpha(Colors::text_primary(), 0.14);
        let row_line_f = Colors::with_alpha(Colors::text_primary(), 0.07);
        let black_row = Colors::with_alpha(Colors::surface_base(), 0.45);
        let note_fill = Colors::with_alpha(Colors::accent_primary(), 0.14);
        let note_fill_selected = Colors::with_alpha(Colors::accent_primary(), 0.26);
        let note_edge = Colors::with_alpha(Colors::accent_primary(), 0.4);
        let curve_color = Colors::accent_cyan();
        let curve_selected = Colors::accent_primary();
        let point_color = Colors::accent_primary();
        let point_selected = Colors::text_primary();

        canvas(
            move |bounds, _window, _cx| capture.set(Some(bounds)),
            move |bounds: Bounds<Pixels>, (), window, _cx| {
                let origin = bounds.origin;
                let view_w = f32::from(bounds.size.width).max(1.0);
                let view_h = f32::from(bounds.size.height).max(1.0);
                let row_h = viewport.row_h.max(1.0);
                let quad = |x: f32, y: f32, w: f32, h: f32, color| {
                    fill(
                        Bounds::new(
                            origin + point(px(x), px(y)),
                            gpui::size(px(w.max(0.0)), px(h.max(0.0))),
                        ),
                        color,
                    )
                };

                // ── Pitch rows ────────────────────────────────────────────
                let (low, high) = viewport.visible_pitches(view_h);
                for pitch in low..=high {
                    let top = viewport.pitch_row_top(pitch as f32);
                    if top > view_h || top + row_h < 0.0 {
                        continue;
                    }
                    if is_black(pitch) {
                        window.paint_quad(quad(0.0, top, view_w, row_h, black_row));
                    }
                    // Octave and F separators read stronger, so the eye can
                    // find a pitch without counting rows.
                    let color = match pitch.rem_euclid(12) {
                        0 => row_line_c,
                        5 => row_line_f,
                        _ => row_line,
                    };
                    window.paint_quad(quad(0.0, top + row_h - 1.0, view_w, 1.0, color));
                }

                // ── Timing grid ───────────────────────────────────────────
                let (start_beat, end_beat) = viewport.visible_beats(view_w);
                for (x, kind) in viewport.grid_lines_in(start_beat, end_beat, &meter) {
                    if x < 0.0 || x > view_w {
                        continue;
                    }
                    window.paint_quad(quad(x, 0.0, 1.0, view_h, kind.color()));
                }

                // ── Note regions (secondary to the curve) ─────────────────
                for note in notes.iter() {
                    let x = viewport.beat_to_x(note.start);
                    let w = note.duration * viewport.ppb;
                    if x + w < 0.0 || x > view_w {
                        continue;
                    }
                    let top = viewport.pitch_row_top(note.pitch as f32);
                    if top > view_h || top + row_h < 0.0 {
                        continue;
                    }
                    let left = x.max(0.0);
                    let right = (x + w).min(view_w);
                    if right <= left {
                        continue;
                    }
                    let is_selected = selected_note == Some(note.id);
                    window.paint_quad(quad(
                        left,
                        top,
                        right - left,
                        row_h - 1.0,
                        if is_selected {
                            note_fill_selected
                        } else {
                            note_fill
                        },
                    ));
                    // Thin top/bottom edges give the region a readable shape
                    // without competing with the trajectory.
                    window.paint_quad(quad(left, top, right - left, 1.0, note_edge));
                    window.paint_quad(quad(left, top + row_h - 2.0, right - left, 1.0, note_edge));
                }

                // ── The evaluated trajectory ──────────────────────────────
                // One stroked path per continuous run. This is the dominant
                // visual: every note contributes a line whether or not anyone
                // has edited its pitch.
                let columns = view_w.ceil() as usize;
                let beats_per_column = 1.0 / viewport.ppb.max(0.0001);
                let mut samples: Vec<Option<f32>> = Vec::with_capacity(columns + 1);
                for (voice_index, voice) in trajectory.voices().iter().enumerate() {
                    trajectory.sample_columns(
                        notes.as_slice(),
                        voice_index,
                        start_beat,
                        beats_per_column,
                        columns + 1,
                        &mut samples,
                    );
                    let voice_selected = selected_note
                        .is_some_and(|id| voice.notes.iter().any(|&i| notes[i].id == id));
                    let color = if voice_selected {
                        curve_selected
                    } else {
                        curve_color
                    };
                    let mut run: Option<gpui::PathBuilder> = None;
                    for (column, sample) in samples.iter().enumerate() {
                        match sample {
                            Some(pitch) => {
                                let y = viewport.pitch_center_y(*pitch);
                                let at = origin + point(px(column as f32), px(y));
                                match run.as_mut() {
                                    Some(path) => path.line_to(at),
                                    None => {
                                        let mut path = gpui::PathBuilder::stroke(px(1.75));
                                        path.move_to(at);
                                        run = Some(path);
                                    }
                                }
                            }
                            None => {
                                // Pen up: close this run so silence is a gap,
                                // not a straight line across the rest.
                                if let Some(path) = run.take() {
                                    if let Ok(path) = path.build() {
                                        window.paint_path(path, color);
                                    }
                                }
                            }
                        }
                    }
                    if let Some(path) = run.take() {
                        if let Ok(path) = path.build() {
                            window.paint_path(path, color);
                        }
                    }
                }

                // ── Control points, only where the user is working ────────
                for note in notes.iter() {
                    if selected_note != Some(note.id) && hovered_note != Some(note.id) {
                        continue;
                    }
                    let Some(curve) = &note.pitch_curve else {
                        continue;
                    };
                    for p in &curve.points {
                        let x = viewport.beat_to_x(note.start + p.beat);
                        if x < -4.0 || x > view_w + 4.0 {
                            continue;
                        }
                        let y = viewport.pitch_center_y(note.pitch as f32 + p.cents / 100.0);
                        let picked = selected_points.contains(&p.id);
                        let r = if picked { 3.5 } else { 2.5 };
                        window.paint_quad(quad(
                            x - r,
                            y - r,
                            r * 2.0,
                            r * 2.0,
                            if picked { point_selected } else { point_color },
                        ));
                    }
                }
            },
        )
        .absolute()
        .inset_0()
        .into_any_element()
    }

    // ── Hit testing ──────────────────────────────────────────────────────

    /// Note under the pointer, or `None` when the pointer is on neither.
    ///
    /// A note is reachable through two vertical bands, both measured in screen
    /// pixels through the shared transform: its own row (plus a little slop, so
    /// a dense zoom stays clickable) and the pitch line it draws, which may run
    /// far outside that row when the performance scoops or bends. Time
    /// containment alone used to be the whole test, so a click two octaves below
    /// a note still targeted it — and with Draw armed that immediately wrote a
    /// breakpoint clamped to the ±2400 cent limit.
    fn note_at(
        &self,
        clip_id: &str,
        local: (f32, f32),
        viewport: PianoRollViewport,
        cx: &Context<Self>,
    ) -> Option<u64> {
        let beat = viewport.x_to_beat(local.0);
        let half_row = viewport.row_h.max(1.0) * 0.5;
        let notes = self.timeline.read(cx).state.midi_clip_notes(clip_id)?;
        notes
            .iter()
            .filter(|note| beat >= note.start && beat <= note.start + note.duration)
            .filter_map(|note| {
                // Negative inside the row, so an overlap resolves to the note
                // the pointer is actually standing on.
                let row_gap =
                    (viewport.pitch_center_y(note.pitch as f32) - local.1).abs() - half_row;
                let cents = note
                    .pitch_curve
                    .as_ref()
                    .map(|curve| curve.cents_at(beat - note.start))
                    .unwrap_or(0.0);
                let curve_gap =
                    (viewport.pitch_center_y(note.pitch as f32 + cents / 100.0) - local.1).abs();
                (row_gap <= NOTE_HIT_SLOP_PX || curve_gap <= CURVE_HIT_PX)
                    .then_some((note.id, row_gap.min(curve_gap)))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(id, _)| id)
    }

    /// Control point of `note_id` under the pointer, in screen pixels.
    fn point_at(
        &self,
        clip_id: &str,
        note_id: u64,
        local: (f32, f32),
        viewport: PianoRollViewport,
        cx: &Context<Self>,
    ) -> Option<u64> {
        let note = self.timeline.read(cx).state.midi_note(clip_id, note_id)?;
        let curve = note.pitch_curve.as_ref()?;
        curve
            .points
            .iter()
            .map(|p| {
                let x = viewport.beat_to_x(note.start + p.beat);
                let y = viewport.pitch_center_y(note.pitch as f32 + p.cents / 100.0);
                (p.id, (x - local.0).hypot(y - local.1))
            })
            .filter(|(_, distance)| *distance <= POINT_HIT_PX)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(id, _)| id)
    }

    /// Pointer position as `(clip-local beat, fractional pitch)`.
    fn pitch_cursor(&self, local: (f32, f32), viewport: PianoRollViewport) -> (f32, f32) {
        (
            viewport.x_to_beat(local.0),
            viewport.y_to_pitch_continuous(local.1),
        )
    }

    fn update_pitch_hover(
        &mut self,
        ctx: &SolfegeEditContext,
        local: (f32, f32),
        cx: &mut Context<Self>,
    ) {
        let viewport = self.viewport(cx);
        let (beat, _) = self.pitch_cursor(local, viewport);
        let hovered = self.note_at(&ctx.clip_id, local, viewport, cx);
        // Handles are drawn for the selected *or* hovered note, so the grab
        // cursor has to consult both — the same pair the press resolves.
        let hovered_point = self
            .pitch
            .selected_note
            .into_iter()
            .chain(hovered)
            .find_map(|note_id| self.point_at(&ctx.clip_id, note_id, local, viewport, cx));
        let readout = hovered.and_then(|note_id| {
            let note = self
                .timeline
                .read(cx)
                .state
                .midi_note(&ctx.clip_id, note_id)?;
            let cents = note
                .pitch_curve
                .as_ref()
                .map(|curve| curve.cents_at(beat - note.start))
                .unwrap_or(0.0);
            Some(format!(
                "{} {}",
                note_name(note.pitch as i32),
                format_cents(cents)
            ))
        });
        if self.pitch.hovered_note != hovered
            || self.pitch.hovered_point != hovered_point
            || self.pitch.readout != readout
        {
            self.pitch.hovered_note = hovered;
            self.pitch.hovered_point = hovered_point;
            self.pitch.readout = readout;
            cx.notify();
        }
    }

    // ── Gestures ─────────────────────────────────────────────────────────

    fn begin_pitch_gesture(
        &mut self,
        ctx: &SolfegeEditContext,
        local: (f32, f32),
        modifiers: &gpui::Modifiers,
        cx: &mut Context<Self>,
    ) {
        let viewport = self.viewport(cx);
        let (beat, pitch) = self.pitch_cursor(local, viewport);

        // A visible handle is a grabbable handle. Points are drawn for the
        // selected *or* hovered note, so both are candidates: pressing one on a
        // note that is merely hovered selects that note and starts the move in
        // the same gesture instead of costing a second click. A point still wins
        // over the note under the cursor, so grabbing a handle never re-targets.
        if self.pitch.tool == PitchTool::Select {
            let candidates = self.pitch.selected_note.into_iter().chain(
                self.pitch
                    .hovered_note
                    .filter(|id| Some(*id) != self.pitch.selected_note),
            );
            let grabbed = candidates
                .filter_map(|note_id| {
                    self.point_at(&ctx.clip_id, note_id, local, viewport, cx)
                        .map(|point_id| (note_id, point_id))
                })
                .next();
            if let Some((note_id, point_id)) = grabbed {
                if self.pitch.selected_note != Some(note_id) {
                    self.pitch.selected_note = Some(note_id);
                    self.pitch.selected_points.clear();
                }
                self.begin_point_move(ctx, note_id, point_id, beat, pitch, modifiers, cx);
                return;
            }
        }

        let Some(note_id) = self.note_at(&ctx.clip_id, local, viewport, cx) else {
            if self.pitch.tool == PitchTool::Select {
                // Empty canvas: start a rubber band rather than clearing
                // immediately, so a drag can still add to the selection.
                // `selected_note` deliberately survives — `points_in_rect`
                // resolves handles through it, so clearing it here would make
                // a marquee begun off-note incapable of selecting anything.
                self.pitch.drag = Some(PitchDrag::Marquee {
                    origin: local,
                    current: local,
                    base: if keeps_selection(modifiers) {
                        self.pitch.selected_points.clone()
                    } else {
                        HashSet::new()
                    },
                });
                if !keeps_selection(modifiers) {
                    self.pitch.selected_points.clear();
                }
            } else if !keeps_selection(modifiers) {
                self.pitch.selected_note = None;
                self.pitch.selected_points.clear();
            }
            cx.notify();
            return;
        };

        if self.pitch.selected_note != Some(note_id) {
            self.pitch.selected_note = Some(note_id);
            self.pitch.selected_points.clear();
        }

        let Some(prev) = self
            .timeline
            .read(cx)
            .state
            .midi_note_snapshot(&ctx.clip_id, note_id)
        else {
            return;
        };
        let cents = cents_for(&prev, pitch);
        let beat_in_note = beat_in_note(&prev, beat);

        match self.pitch.tool {
            PitchTool::Select => {
                self.pitch.drag = Some(PitchDrag::Marquee {
                    origin: local,
                    current: local,
                    base: if keeps_selection(modifiers) {
                        self.pitch.selected_points.clone()
                    } else {
                        HashSet::new()
                    },
                });
            }
            PitchTool::Draw | PitchTool::Erase => {
                self.pitch.stroke_range = None;
                self.pitch.drag = Some(PitchDrag::Paint {
                    clip_id: ctx.clip_id.clone(),
                    note_id,
                    prev,
                });
                self.drag_pitch_gesture(ctx, local, cx);
            }
            PitchTool::Line => {
                self.pitch.drag = Some(PitchDrag::Line {
                    clip_id: ctx.clip_id.clone(),
                    note_id,
                    prev,
                    from_beat: beat_in_note,
                    from_cents: cents,
                });
            }
        }
        cx.notify();
    }

    fn begin_point_move(
        &mut self,
        ctx: &SolfegeEditContext,
        note_id: u64,
        point_id: u64,
        beat: f32,
        pitch: f32,
        modifiers: &gpui::Modifiers,
        cx: &mut Context<Self>,
    ) {
        if keeps_selection(modifiers) {
            if !self.pitch.selected_points.insert(point_id) {
                // Toggling a point off is a selection gesture, not a move: arming
                // a drag here would let the same click nudge every point that
                // stayed selected.
                self.pitch.selected_points.remove(&point_id);
                cx.notify();
                return;
            }
        } else if !self.pitch.selected_points.contains(&point_id) {
            self.pitch.selected_points.clear();
            self.pitch.selected_points.insert(point_id);
        }
        let Some(prev) = self
            .timeline
            .read(cx)
            .state
            .midi_note_snapshot(&ctx.clip_id, note_id)
        else {
            return;
        };
        let Some(curve) = prev.pitch_curve.as_ref() else {
            return;
        };
        let origin: Vec<(u64, f32, f32)> = curve
            .points
            .iter()
            .filter(|p| self.pitch.selected_points.contains(&p.id))
            .map(|p| (p.id, p.beat, p.cents))
            .collect();
        if origin.is_empty() {
            cx.notify();
            return;
        }
        let anchor_beat = beat - prev.start;
        let anchor_cents = cents_for(&prev, pitch);
        self.pitch.drag = Some(PitchDrag::Move {
            clip_id: ctx.clip_id.clone(),
            note_id,
            prev,
            origin,
            anchor_beat,
            anchor_cents,
        });
        cx.notify();
    }

    fn drag_pitch_gesture(
        &mut self,
        ctx: &SolfegeEditContext,
        local: (f32, f32),
        cx: &mut Context<Self>,
    ) {
        let Some(drag) = self.pitch.drag.clone() else {
            return;
        };
        let viewport = self.viewport(cx);
        let (beat, pitch) = self.pitch_cursor(local, viewport);

        match drag {
            PitchDrag::Marquee { origin, base, .. } => {
                let selected = self.points_in_rect(ctx, origin, local, viewport, cx);
                self.pitch.selected_points = base.union(&selected).copied().collect();
                self.pitch.readout = Some(format!("{} pts", self.pitch.selected_points.len()));
                self.pitch.drag = Some(PitchDrag::Marquee {
                    origin,
                    current: local,
                    base,
                });
                cx.notify();
            }
            PitchDrag::Paint {
                clip_id,
                note_id,
                prev,
            } => {
                let beat_in_note = beat_in_note(&prev, beat);
                let cents = cents_for(&prev, pitch);
                let erasing = self.pitch.tool == PitchTool::Erase;
                let half = PITCH_MERGE_BEATS.max(viewport.snap_step_beats * 0.5);

                // Edited in place. Reading the curve out, changing it and
                // writing it back copies every point already drawn, once per
                // mouse-move sample — which makes one stroke quadratic in its
                // own length for the second time in this file's history. The
                // note is found once and the point goes straight in.
                let changed = self.edit_note_pitch_curve(&clip_id, note_id, cx, |curve| {
                    if erasing {
                        curve.erase_range(beat_in_note - half, beat_in_note + half) > 0
                    } else {
                        // The live curve tracks the pointer exactly — a stroke
                        // that lagged its own cursor would be unusable. The
                        // per-sample density this leaves behind is collapsed
                        // once, on release, by `simplify_stroke`.
                        curve.set_point(
                            beat_in_note,
                            cents,
                            PitchSegmentShape::Smooth,
                            PITCH_MERGE_BEATS,
                        );
                        true
                    }
                });
                if !changed {
                    return;
                }
                if !erasing {
                    self.pitch.stroke_range = Some(match self.pitch.stroke_range {
                        Some((lo, hi)) => (lo.min(beat_in_note), hi.max(beat_in_note)),
                        None => (beat_in_note, beat_in_note),
                    });
                    self.pitch.readout = Some(format_cents(cents));
                }
            }
            PitchDrag::Line {
                clip_id,
                note_id,
                prev,
                from_beat,
                from_cents,
            } => {
                let to_beat = beat_in_note(&prev, beat);
                let to_cents = cents_for(&prev, pitch);
                let (lo, hi) = if from_beat <= to_beat {
                    (from_beat, to_beat)
                } else {
                    (to_beat, from_beat)
                };
                // Rebuild from the pre-gesture curve on every move, so dragging
                // back and forth replaces the ramp instead of layering ramps.
                let mut curve = prev.pitch_curve.clone().unwrap_or_default();
                curve.replace_range(
                    lo,
                    hi,
                    PitchCurve::line(from_beat, to_beat, from_cents, to_cents),
                );
                self.pitch.readout = Some(format!(
                    "{} \u{2192} {}",
                    format_cents(from_cents),
                    format_cents(to_cents)
                ));
                self.write_pitch_curve(&clip_id, note_id, curve, cx);
            }
            PitchDrag::Move {
                clip_id,
                note_id,
                prev,
                origin,
                anchor_beat,
                anchor_cents,
            } => {
                let delta_beat = (beat - prev.start) - anchor_beat;
                let delta_cents = cents_for(&prev, pitch) - anchor_cents;
                let mut curve = prev.pitch_curve.clone().unwrap_or_default();
                for (id, beat0, cents0) in &origin {
                    if let Some(p) = curve.point_mut(*id) {
                        // Same span clamp the drawing tools use: a handle
                        // dragged past the note end would vanish from the
                        // trajectory while still bending its tail.
                        p.beat = (beat0 + delta_beat).clamp(0.0, prev.duration.max(0.0));
                        p.cents = (cents0 + delta_cents)
                            .clamp(-PITCH_CURVE_MAX_CENTS, PITCH_CURVE_MAX_CENTS);
                    }
                }
                curve.sort();
                self.pitch.readout = Some(format_cents(anchor_cents + delta_cents));
                self.write_pitch_curve(&clip_id, note_id, curve, cx);
            }
        }
    }

    fn points_in_rect(
        &self,
        ctx: &SolfegeEditContext,
        a: (f32, f32),
        b: (f32, f32),
        viewport: PianoRollViewport,
        cx: &Context<Self>,
    ) -> HashSet<u64> {
        let (x0, x1) = (a.0.min(b.0), a.0.max(b.0));
        let (y0, y1) = (a.1.min(b.1), a.1.max(b.1));
        let Some(note_id) = self.pitch.selected_note else {
            return HashSet::new();
        };
        let Some(note) = self
            .timeline
            .read(cx)
            .state
            .midi_note(&ctx.clip_id, note_id)
        else {
            return HashSet::new();
        };
        let Some(curve) = note.pitch_curve.as_ref() else {
            return HashSet::new();
        };
        curve
            .points
            .iter()
            .filter(|p| {
                let x = viewport.beat_to_x(note.start + p.beat);
                let y = viewport.pitch_center_y(note.pitch as f32 + p.cents / 100.0);
                x >= x0 && x <= x1 && y >= y0 && y <= y1
            })
            .map(|p| p.id)
            .collect()
    }

    /// Write a curve into live timeline state without touching history. The
    /// undo entry is recorded once, on gesture end.
    /// Mutate one note's pitch curve in place.
    ///
    /// `edit` returns whether it changed anything; when it did not, nothing is
    /// marked dirty and no redraw is requested, so a sample that lands on top
    /// of the previous one costs a lookup and stops.
    ///
    /// This exists because the obvious shape — read the curve, change it, write
    /// it back — copies the whole curve twice per call, and the drawing tools
    /// call it once per mouse-move sample. A curve is a `Vec` of every point
    /// drawn so far, so that is a stroke whose cost grows with its own length.
    fn edit_note_pitch_curve(
        &mut self,
        clip_id: &str,
        note_id: u64,
        cx: &mut Context<Self>,
        edit: impl FnOnce(&mut PitchCurve) -> bool,
    ) -> bool {
        let clip_id = clip_id.to_string();
        let changed = self.timeline.update(cx, |tl, tcx| {
            let Some(notes) = tl.state.midi_clip_notes_mut(&clip_id) else {
                return false;
            };
            let Some(note) = notes.iter_mut().find(|note| note.id == note_id) else {
                return false;
            };
            let mut curve = note.pitch_curve.take().unwrap_or_default();
            let changed = edit(&mut curve);
            note.pitch_curve = (!curve.is_empty()).then_some(curve);
            if changed {
                // The same unforced dirty mark `write_pitch_curve` explains:
                // the engine's poll coalesces a stroke's worth of these into a
                // handful of snapshots rather than one per sample.
                tl.mark_project_changed(tcx);
                tcx.notify();
            }
            changed
        });
        if changed {
            cx.notify();
        }
        changed
    }

    fn write_pitch_curve(
        &mut self,
        clip_id: &str,
        note_id: u64,
        curve: PitchCurve,
        cx: &mut Context<Self>,
    ) {
        let clip_id = clip_id.to_string();
        self.timeline.update(cx, |tl, tcx| {
            if let Some(notes) = tl.state.midi_clip_notes_mut(&clip_id) {
                if let Some(note) = notes.iter_mut().find(|note| note.id == note_id) {
                    note.pitch_curve = (!curve.is_empty()).then_some(curve);
                }
            }
            // Make the stroke audible while it is still being drawn. This is
            // the *unforced* dirty mark deliberately: the engine's own poll
            // coalesces it to a control-rate sync (~75 ms), which is inside
            // interactive latency but far below the per-mouse-move rate, so a
            // long stroke publishes a handful of snapshots rather than one per
            // sample. The forced sync that must not be missed happens once on
            // release, in `record_note_edit`.
            tl.mark_project_changed(tcx);
            tcx.notify();
        });
        cx.notify();
    }

    fn commit_pitch_gesture(&mut self, cx: &mut Context<Self>) {
        let Some(drag) = self.pitch.drag.take() else {
            return;
        };
        let stroke = self.pitch.stroke_range.take();
        let (clip_id, note_id, prev) = match drag {
            PitchDrag::Marquee {
                origin, current, ..
            } => {
                // A click that neither hit a note nor swept up any handle is
                // the gesture that means "deselect".
                let clicked =
                    (origin.0 - current.0).abs() < 2.0 && (origin.1 - current.1).abs() < 2.0;
                if clicked && self.pitch.selected_points.is_empty() {
                    self.pitch.selected_note = None;
                }
                self.pitch.readout = None;
                cx.notify();
                return;
            }
            PitchDrag::Paint {
                clip_id,
                note_id,
                prev,
            }
            | PitchDrag::Line {
                clip_id,
                note_id,
                prev,
                ..
            }
            | PitchDrag::Move {
                clip_id,
                note_id,
                prev,
                ..
            } => (clip_id, note_id, prev),
        };
        // Strictly before `record_note_edit` captures `next`, so undo restores
        // the curve the user is left holding rather than the dense one.
        if let Some((from, to)) = stroke {
            self.simplify_stroke(&clip_id, note_id, from, to, cx);
        }
        self.record_note_edit(clip_id, note_id, prev, cx);
    }

    /// Collapse a finished freehand stroke to the fewest breakpoints that still
    /// reproduce it within [`PITCH_SIMPLIFY_CENTS`].
    ///
    /// Only the beats the stroke actually wrote are touched; points either side
    /// of it keep their identities and values, so drawing over one phrase never
    /// rewrites another.
    fn simplify_stroke(
        &mut self,
        clip_id: &str,
        note_id: u64,
        from: f32,
        to: f32,
        cx: &mut Context<Self>,
    ) {
        let curve = self
            .timeline
            .read(cx)
            .state
            .note_pitch_curve(clip_id, note_id);
        let simplified = simplify_curve_range(&curve, from, to, PITCH_SIMPLIFY_CENTS);
        if simplified == curve {
            return;
        }
        // Handles that no longer exist must not stay selected.
        self.pitch
            .selected_points
            .retain(|id| simplified.point(*id).is_some());
        self.write_pitch_curve(clip_id, note_id, simplified, cx);
    }

    /// Record one `EditMidiNotes` entry for a finished edit of `note_id`.
    /// Pitch expression lives on the note, so it rides the DAW's existing MIDI
    /// history rather than a Solfege-local stack.
    fn record_note_edit(
        &mut self,
        clip_id: String,
        note_id: u64,
        prev: MidiNoteState,
        cx: &mut Context<Self>,
    ) {
        let Some(next) = self
            .timeline
            .read(cx)
            .state
            .midi_note_snapshot(&clip_id, note_id)
        else {
            return;
        };
        if prev == next {
            cx.notify();
            return;
        }
        self.timeline.update(cx, |tl, tcx| {
            tl.record_executed_command(
                EditCommand::EditMidiNotes {
                    clip_id,
                    prev: vec![prev],
                    next: vec![next],
                },
                tcx,
            );
        });
        cx.notify();
    }

    fn delete_selected_pitch_points(&mut self, ctx: &SolfegeEditContext, cx: &mut Context<Self>) {
        let Some(note_id) = self.pitch.selected_note else {
            return;
        };
        if self.pitch.selected_points.is_empty() {
            return;
        }
        let Some(prev) = self
            .timeline
            .read(cx)
            .state
            .midi_note_snapshot(&ctx.clip_id, note_id)
        else {
            return;
        };
        let mut curve = prev.pitch_curve.clone().unwrap_or_default();
        for id in &self.pitch.selected_points {
            curve.remove_point(*id);
        }
        self.pitch.selected_points.clear();
        self.write_pitch_curve(&ctx.clip_id, note_id, curve, cx);
        self.record_note_edit(ctx.clip_id.clone(), note_id, prev, cx);
    }

    // ── One-shot operations ──────────────────────────────────────────────

    /// The beat range an operation applies to: the selected points' span when
    /// there is a point selection, otherwise the whole note. This is what keeps
    /// Smooth from flattening a phrase the user only wanted tidied in one spot.
    fn action_range(&self, note: &MidiNoteState) -> (f32, f32) {
        let selected: Vec<f32> = note
            .pitch_curve
            .as_ref()
            .map(|curve| {
                curve
                    .points
                    .iter()
                    .filter(|p| self.pitch.selected_points.contains(&p.id))
                    .map(|p| p.beat)
                    .collect()
            })
            .unwrap_or_default();
        if selected.len() < 2 {
            return (0.0, note.duration.max(0.01));
        }
        let lo = selected.iter().copied().fold(f32::MAX, f32::min);
        let hi = selected.iter().copied().fold(f32::MIN, f32::max);
        (lo, hi)
    }

    fn apply_pitch_action(
        &mut self,
        ctx: &SolfegeEditContext,
        action: PitchAction,
        cx: &mut Context<Self>,
    ) {
        let Some(note_id) = self.pitch.selected_note else {
            return;
        };
        let Some(prev) = self
            .timeline
            .read(cx)
            .state
            .midi_note_snapshot(&ctx.clip_id, note_id)
        else {
            return;
        };

        if action == PitchAction::Transition {
            self.connect_note_to_next(ctx, note_id, prev, cx);
            return;
        }

        let (from, to) = self.action_range(&prev);
        let mut curve = prev.pitch_curve.clone().unwrap_or_default();
        match action {
            PitchAction::Smooth => curve.smooth_range(from, to),
            PitchAction::Vibrato => {
                let onset = from + (to - from) * VIBRATO_ONSET_FRACTION;
                let center = curve.cents_at(onset);
                curve.replace_range(
                    onset,
                    to,
                    PitchCurve::vibrato(
                        onset,
                        to,
                        center,
                        VIBRATO_DEPTH_CENTS,
                        VIBRATO_CYCLE_BEATS,
                    ),
                );
            }
            PitchAction::Reset => {
                // Clear manual pitch only. The note stays, and the trajectory
                // falls back to the performance baseline — today the notated
                // pitch plus any note-to-note transitions. When an FBMX
                // performer supplies a generated baseline, this returns to that
                // instead, with no change needed here.
                //
                // With a point selection this removes exactly those points, by
                // id. Going through `action_range` would widen a single-point
                // selection to the whole note and wipe the curve.
                if self.pitch.selected_points.is_empty() {
                    curve = PitchCurve::default();
                } else {
                    for id in &self.pitch.selected_points {
                        curve.remove_point(*id);
                    }
                }
                self.pitch.selected_points.clear();
            }
            PitchAction::Transition => unreachable!("handled above"),
        }
        self.write_pitch_curve(&ctx.clip_id, note_id, curve, cx);
        self.record_note_edit(ctx.clip_id.clone(), note_id, prev, cx);
    }

    /// Make `note_id` glide into the note that follows it.
    ///
    /// The trajectory bridges notes whose articulation connects them, so the
    /// honest way to "create a transition" is to set that articulation: the
    /// continuous line then falls out of the same evaluation everything else
    /// uses, and it is one ordinary undoable note edit. A future FBMX
    /// transition generator replaces the transition *shape* inside
    /// [`PitchTrajectory`], not this command.
    fn connect_note_to_next(
        &mut self,
        ctx: &SolfegeEditContext,
        note_id: u64,
        prev: MidiNoteState,
        cx: &mut Context<Self>,
    ) {
        // The trajectory only bridges a gap up to `LEGATO_BRIDGE_BEATS`, so a
        // successor further away than that would leave the button claiming a
        // glide the user would never see. Require one inside the window.
        let note_end = prev.start + prev.duration;
        let reachable = self
            .timeline
            .read(cx)
            .state
            .midi_clip_notes(&ctx.clip_id)
            .is_some_and(|notes| {
                notes.iter().any(|n| {
                    n.id != note_id
                        && n.start >= note_end - ABUT_EPS_BEATS
                        && n.start - note_end <= LEGATO_BRIDGE_BEATS
                })
            });
        if !reachable {
            self.pitch.readout = Some("No note close enough to glide into".to_string());
            cx.notify();
            return;
        }
        let clip_id = ctx.clip_id.clone();
        self.timeline.update(cx, |tl, tcx| {
            if let Some(notes) = tl.state.midi_clip_notes_mut(&clip_id) {
                if let Some(note) = notes.iter_mut().find(|n| n.id == note_id) {
                    note.articulation = Some(ArticulationId::Legato);
                }
            }
            tcx.notify();
        });
        self.pitch.readout = Some("Legato".to_string());
        self.record_note_edit(ctx.clip_id.clone(), note_id, prev, cx);
    }

    // ── Waveform ─────────────────────────────────────────────────────────

    /// Compact rendered-audio strip for timing context.
    ///
    /// Peaks come from the shared waveform cache, which the render/import
    /// pipeline fills off the UI thread. This is a pure cache read: if nothing
    /// has been published for the clip the strip is hidden rather than faked,
    /// and no decode or file IO can happen inside the render pass.
    fn render_pitch_waveform(
        &self,
        ctx: &SolfegeEditContext,
        viewport: PianoRollViewport,
        cx: &Context<Self>,
    ) -> Option<impl IntoElement> {
        let preview = waveform_cache::recording_preview(&ctx.clip_id)?;
        let lod = preview.lods.first()?.clone();
        if lod.peaks.is_empty() || preview.sample_rate == 0 {
            return None;
        }
        let seconds_per_beat = self.timeline.read(cx).state.seconds_per_beat() as f32;
        let peaks_per_second = preview.sample_rate as f32 / lod.samples_per_peak.max(1) as f32;
        let bar_color = Colors::with_alpha(Colors::accent_cyan(), 0.6);
        let mid_color = Colors::with_alpha(Colors::border_subtle(), 0.9);

        Some(
            div()
                .id("solfege-pitch-waveform")
                .flex()
                .flex_row()
                .flex_none()
                .w_full()
                .h(px(WAVEFORM_H))
                .border_t(px(1.0))
                .border_color(Colors::border_subtle())
                .child(
                    div()
                        .flex_none()
                        .h_full()
                        .w(px(viewport.key_lane_width.max(48.0)))
                        .px(px(6.0))
                        .py(px(3.0))
                        .border_r(px(1.0))
                        .border_color(Colors::border_subtle())
                        .bg(Colors::surface_panel())
                        .text_size(px(8.0))
                        .text_color(Colors::text_faint())
                        .child("Wave"),
                )
                .child(
                    div()
                        .flex_1()
                        .h_full()
                        .relative()
                        .overflow_hidden()
                        .bg(Colors::surface_panel_alt())
                        .child(
                            canvas(
                                |_bounds, _window, _cx| {},
                                move |bounds: Bounds<Pixels>, (), window, _cx| {
                                    let origin = bounds.origin;
                                    let view_w = f32::from(bounds.size.width).max(1.0);
                                    let view_h = f32::from(bounds.size.height).max(1.0);
                                    let mid = view_h * 0.5;
                                    window.paint_quad(fill(
                                        Bounds::new(
                                            origin + point(px(0.0), px(mid)),
                                            gpui::size(px(view_w), px(1.0)),
                                        ),
                                        mid_color,
                                    ));
                                    for col in 0..view_w as usize {
                                        // Same horizontal transform as the pitch
                                        // canvas above, so the waveform lines up
                                        // with the notes.
                                        let beat = viewport.x_to_beat(col as f32);
                                        let seconds = beat * seconds_per_beat;
                                        let index = (seconds * peaks_per_second) as usize;
                                        let Some(peak) = lod.peaks.get(index) else {
                                            break;
                                        };
                                        let top = mid - peak.max.clamp(-1.0, 1.0) * mid;
                                        let bottom = mid - peak.min.clamp(-1.0, 1.0) * mid;
                                        window.paint_quad(fill(
                                            Bounds::new(
                                                origin + point(px(col as f32), px(top.min(bottom))),
                                                gpui::size(
                                                    px(1.0),
                                                    px((bottom - top).abs().max(1.0)),
                                                ),
                                            ),
                                            bar_color,
                                        ));
                                    }
                                },
                            )
                            .absolute()
                            .inset_0(),
                        ),
                ),
        )
    }

    // ── Supporting lane ──────────────────────────────────────────────────

    /// Exactly one supporting performance lane (Dynamics), full width, on the
    /// shared timeline, reusing the MIDI tab's lane renderer. Deliberately not
    /// the whole `+ Lane` stack — detailed non-pitch editing stays in MIDI.
    fn render_pitch_support_lane(
        &mut self,
        ctx: &SolfegeEditContext,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let spec = *ctx
            .capabilities
            .lanes
            .iter()
            .find(|lane| lane.id == "dynamics" && lane.source != LaneSource::NoteVelocity)?;
        let row = SolfegeLaneVisibility {
            lane_id: spec.id.to_string(),
            height: super::lanes::SUPPORT_LANE_HEIGHT,
        };
        Some(self.render_lane_rows(ctx, vec![(row, spec)], false, cx))
    }
}

/// Toolbar tooltip body. Matches the compact tooltip styling used elsewhere in
/// the app rather than introducing a second tooltip look.
struct PitchTooltip(&'static str);

impl gpui::Render for PitchTooltip {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(8.0))
            .py(px(4.0))
            .rounded(px(crate::theme::radius::CONTROL))
            .bg(Colors::surface_raised())
            .border(px(1.0))
            .border_color(Colors::border_subtle())
            .text_size(px(10.0))
            .text_color(Colors::text_secondary())
            .child(self.0)
    }
}

fn pitch_tooltip(
    text: &'static str,
) -> impl Fn(&mut Window, &mut gpui::App) -> gpui::AnyView + 'static {
    move |_window, cx| cx.new(|_| PitchTooltip(text)).into()
}

/// Cent deviation of a fractional `pitch` relative to `note`'s notated pitch.
#[inline]
fn cents_for(note: &MidiNoteState, pitch: f32) -> f32 {
    ((pitch - note.pitch as f32) * 100.0).clamp(-PITCH_CURVE_MAX_CENTS, PITCH_CURVE_MAX_CENTS)
}

/// Clip-local `beat` expressed relative to `note`, clamped to the note's own
/// span.
///
/// The trajectory only evaluates a note up to its end, so a breakpoint written
/// past it draws nothing — while `PitchCurve::cents_at` still interpolates
/// toward it and skews the audible tail. A point the user cannot see must not
/// exist.
#[inline]
fn beat_in_note(note: &MidiNoteState, beat: f32) -> f32 {
    (beat - note.start).clamp(0.0, note.duration.max(0.0))
}

/// Ramer–Douglas–Peucker over the points inside `[from, to]`, everything
/// outside preserved untouched.
///
/// The error of a candidate segment is measured against the curve as
/// [`PitchCurve::cents_at`] will actually evaluate it — including the cosine
/// ease of a `Smooth` segment — so `tolerance_cents` bounds the real
/// reconstruction error and not a straight-line approximation of it.
fn simplify_curve_range(
    curve: &PitchCurve,
    from: f32,
    to: f32,
    tolerance_cents: f32,
) -> PitchCurve {
    let (lo, hi) = if from <= to { (from, to) } else { (to, from) };
    let inside: Vec<PitchPoint> = curve
        .points
        .iter()
        .filter(|p| p.beat >= lo && p.beat <= hi)
        .cloned()
        .collect();
    if inside.len() <= 2 {
        return curve.clone();
    }
    let kept = simplify_points(&inside, tolerance_cents);
    if kept.len() == inside.len() {
        return curve.clone();
    }
    let mut points: Vec<PitchPoint> = curve
        .points
        .iter()
        .filter(|p| p.beat < lo || p.beat > hi)
        .cloned()
        .collect();
    points.extend(kept);
    PitchCurve::from_points(points)
}

/// The RDP recursion itself, iterative so a long stroke cannot blow the stack.
/// Retained points keep their ids, values and shapes, so the result is the same
/// curve with fewer handles — not a resampled copy.
fn simplify_points(points: &[PitchPoint], tolerance_cents: f32) -> Vec<PitchPoint> {
    if points.len() <= 2 {
        return points.to_vec();
    }
    let last = points.len() - 1;
    let mut keep = vec![false; points.len()];
    keep[0] = true;
    keep[last] = true;
    let mut pending = vec![(0usize, last)];
    while let Some((a, b)) = pending.pop() {
        if b <= a + 1 {
            continue;
        }
        // The segment exactly as the finished curve will hold it: `a`'s shape
        // decides how it travels to `b`.
        let chord = PitchCurve {
            points: vec![points[a].clone(), points[b].clone()],
        };
        let mut worst = (a, 0.0f32);
        for (index, point) in points.iter().enumerate().take(b).skip(a + 1) {
            let error = (point.cents - chord.cents_at(point.beat)).abs();
            if error > worst.1 {
                worst = (index, error);
            }
        }
        if worst.1 > tolerance_cents {
            keep[worst.0] = true;
            pending.push((a, worst.0));
            pending.push((worst.0, b));
        }
    }
    points
        .iter()
        .zip(keep)
        .filter(|(_, kept)| *kept)
        .map(|(point, _)| point.clone())
        .collect()
}

/// `true` when a modifier says "extend the current selection".
fn keeps_selection(modifiers: &gpui::Modifiers) -> bool {
    modifiers.shift || modifiers.control || modifiers.platform
}

fn marquee_overlay(origin: (f32, f32), current: (f32, f32)) -> impl IntoElement {
    div()
        .absolute()
        .left(px(origin.0.min(current.0)))
        .top(px(origin.1.min(current.1)))
        .w(px((origin.0 - current.0).abs()))
        .h(px((origin.1 - current.1).abs()))
        .border(px(1.0))
        .border_color(Colors::accent_primary())
        .bg(Colors::with_alpha(Colors::accent_primary(), 0.12))
}

/// The plain musical canvas shown when no Solfege clip is selected. The grid
/// and the pitch keyboard stay visible — never a counter or an empty card.
fn pitch_empty_canvas(viewport: PianoRollViewport) -> impl IntoElement {
    let row_line = Colors::with_alpha(Colors::text_primary(), 0.035);
    let row_line_c = Colors::with_alpha(Colors::text_primary(), 0.14);
    let black_row = Colors::with_alpha(Colors::surface_base(), 0.45);
    div()
        .flex()
        .flex_row()
        .flex_1()
        .min_h(px(0.0))
        .w_full()
        .overflow_hidden()
        .child(
            div()
                .flex_none()
                .h_full()
                .w(px(viewport.key_lane_width.max(48.0)))
                .border_r(px(1.0))
                .border_color(Colors::border_subtle())
                .bg(Colors::surface_panel()),
        )
        .child(
            div()
                .flex_1()
                .h_full()
                .relative()
                .overflow_hidden()
                .bg(Colors::surface_base())
                .child(
                    canvas(
                        |_bounds, _window, _cx| {},
                        move |bounds: Bounds<Pixels>, (), window, _cx| {
                            let origin = bounds.origin;
                            let view_w = f32::from(bounds.size.width).max(1.0);
                            let view_h = f32::from(bounds.size.height).max(1.0);
                            let row_h = viewport.row_h.max(1.0);
                            let (low, high) = viewport.visible_pitches(view_h);
                            for pitch in low..=high {
                                let top = viewport.pitch_row_top(pitch as f32);
                                if top > view_h || top + row_h < 0.0 {
                                    continue;
                                }
                                if is_black(pitch) {
                                    window.paint_quad(fill(
                                        Bounds::new(
                                            origin + point(px(0.0), px(top)),
                                            gpui::size(px(view_w), px(row_h)),
                                        ),
                                        black_row,
                                    ));
                                }
                                window.paint_quad(fill(
                                    Bounds::new(
                                        origin + point(px(0.0), px(top + row_h - 1.0)),
                                        gpui::size(px(view_w), px(1.0)),
                                    ),
                                    if pitch.rem_euclid(12) == 0 {
                                        row_line_c
                                    } else {
                                        row_line
                                    },
                                ));
                            }
                        },
                    )
                    .absolute()
                    .inset_0(),
                )
                .child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(10.0))
                        .text_color(Colors::text_faint())
                        .child("Select a Solfege clip to edit its pitch"),
                ),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn viewport() -> PianoRollViewport {
        PianoRollViewport {
            ppb: 60.0,
            scroll_x: 0.0,
            row_h: 14.0,
            scroll_y: 0.0,
            key_lane_width: 72.0,
            snap_step_beats: 0.25,
            sub_step_beats: 0.25,
            grid_width: 600.0,
            grid_height: 240.0,
        }
    }

    #[test]
    fn a_flat_note_sits_on_its_own_row_centre() {
        let v = viewport();
        assert!((v.pitch_center_y(60.0) - (v.pitch_row_top(60.0) + v.row_h * 0.5)).abs() < 0.001);
    }

    #[test]
    fn the_vertical_axis_round_trips_at_fractional_pitches() {
        let v = viewport();
        for pitch in [48.0f32, 60.37, 71.5, 84.0] {
            let y = v.pitch_center_y(pitch);
            assert!(
                (v.y_to_pitch_continuous(y) - pitch).abs() < 0.001,
                "{pitch}"
            );
        }
    }

    #[test]
    fn scrolling_shifts_the_axis_without_changing_its_scale() {
        let mut v = viewport();
        let before = v.pitch_center_y(60.0);
        v.scroll_y += 100.0;
        assert!((v.pitch_center_y(60.0) - (before - 100.0)).abs() < 0.001);
        assert!((v.y_to_pitch_continuous(v.pitch_center_y(60.0)) - 60.0).abs() < 0.001);
    }

    #[test]
    fn a_higher_pitch_is_higher_on_screen() {
        let v = viewport();
        assert!(v.pitch_center_y(72.0) < v.pitch_center_y(60.0));
    }

    #[test]
    fn cents_are_measured_from_the_notes_own_pitch() {
        let note = MidiNoteState::new(60, 0.0, 1.0, 100);
        assert!((cents_for(&note, 60.5) - 50.0).abs() < 0.001);
        assert!((cents_for(&note, 59.0) + 100.0).abs() < 0.001);
        // Clamped, never wrapped.
        assert_eq!(cents_for(&note, 200.0), PITCH_CURVE_MAX_CENTS);
    }

    #[test]
    fn ruler_labels_bars_when_zoomed_out_and_beats_when_zoomed_in() {
        let mut v = viewport();
        v.ppb = 8.0;
        assert!(v
            .ruler_marks(0.0, 32.0, 4.0)
            .iter()
            .all(|m| !m.label.contains('.')));
        v.ppb = 60.0;
        let fine = v.ruler_marks(0.0, 8.0, 4.0);
        assert!(fine.iter().any(|m| m.label == "1.2"));
        assert!(fine.iter().any(|m| m.label == "2.1" && m.on_bar));
    }

    #[test]
    fn the_grid_thins_out_as_it_zooms_out() {
        let mut v = viewport();
        v.ppb = 60.0;
        let dense = v.grid_lines(0.0, 8.0, 4.0).len();
        v.ppb = 4.0;
        let sparse = v.grid_lines(0.0, 8.0, 4.0).len();
        assert!(sparse < dense, "expected fewer lines when zoomed out");
    }

    #[test]
    fn visible_pitches_stay_inside_the_midi_range() {
        let v = viewport();
        let (low, high) = v.visible_pitches(240.0);
        assert!(low >= 0 && high <= 127 && low <= high);
    }

    #[test]
    fn cents_readout_is_signed() {
        assert_eq!(format_cents(17.0), "+17 ct");
        assert_eq!(format_cents(-8.4), "-8 ct");
    }

    #[test]
    fn a_drawn_beat_never_lands_outside_its_note() {
        let note = MidiNoteState::new(60, 4.0, 1.5, 100);
        assert_eq!(beat_in_note(&note, 3.0), 0.0);
        assert!((beat_in_note(&note, 4.75) - 0.75).abs() < 0.001);
        // Dragging past the note end pins to the end, not past it.
        assert!((beat_in_note(&note, 9.0) - 1.5).abs() < 0.001);
    }

    /// One pointer sample per pixel column across two beats, tracing a scoop
    /// with a vibrato tail — the shape a real freehand stroke leaves behind.
    fn synthetic_stroke(samples: usize) -> PitchCurve {
        PitchCurve::from_points(
            (0..samples)
                .map(|i| {
                    let t = i as f32 / (samples - 1) as f32;
                    let cents =
                        -180.0 * (1.0 - t).powi(3) + 30.0 * (t * std::f32::consts::TAU * 3.0).sin();
                    PitchPoint::new(t * 2.0, cents, PitchSegmentShape::Smooth)
                })
                .collect(),
        )
    }

    #[test]
    fn a_freehand_stroke_collapses_to_editable_breakpoints() {
        let dense = synthetic_stroke(500);
        let simplified = simplify_curve_range(&dense, 0.0, 2.0, PITCH_SIMPLIFY_CENTS);
        assert_eq!(dense.len(), 500);
        assert!(
            simplified.len() < 60,
            "expected a hand-editable curve, got {} points",
            simplified.len()
        );
        assert!(simplified.len() >= 2);
        // Every original sample is still reproduced within the tolerance.
        for point in &dense.points {
            let error = (simplified.cents_at(point.beat) - point.cents).abs();
            assert!(
                error <= PITCH_SIMPLIFY_CENTS + 0.001,
                "beat {} drifted by {error} cents",
                point.beat
            );
        }
    }

    #[test]
    fn simplification_leaves_points_outside_the_stroke_alone() {
        let mut curve = synthetic_stroke(200);
        let outside = PitchPoint::new(5.0, 400.0, PitchSegmentShape::Linear);
        let outside_id = outside.id;
        curve.points.push(outside);
        curve.sort();

        let simplified = simplify_curve_range(&curve, 0.0, 2.0, PITCH_SIMPLIFY_CENTS);
        let survivor = simplified
            .point(outside_id)
            .expect("a point the stroke never touched must survive verbatim");
        assert!((survivor.beat - 5.0).abs() < 0.001);
        assert!((survivor.cents - 400.0).abs() < 0.001);
        assert_eq!(survivor.shape, PitchSegmentShape::Linear);
    }

    #[test]
    fn simplification_keeps_the_surviving_points_identities() {
        let dense = synthetic_stroke(300);
        let simplified = simplify_curve_range(&dense, 0.0, 2.0, PITCH_SIMPLIFY_CENTS);
        for point in &simplified.points {
            let original = dense
                .point(point.id)
                .expect("simplification must retain points, never mint new ones");
            assert!((original.beat - point.beat).abs() < 0.001);
            assert!((original.cents - point.cents).abs() < 0.001);
        }
    }

    #[test]
    fn a_curve_that_is_already_sparse_is_left_as_it_is() {
        let curve = PitchCurve::from_points(PitchCurve::line(0.0, 1.0, -50.0, 50.0));
        let simplified = simplify_curve_range(&curve, 0.0, 1.0, PITCH_SIMPLIFY_CENTS);
        assert_eq!(simplified, curve);
    }
}

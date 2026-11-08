//! Compact performance lanes under the piano roll, and the `+ Lane` menu.
//!
//! One reusable lane system serves every parameter. A lane is described by the
//! loaded instrument's [`LaneSpec`] and is backed by data the DAW already
//! owns:
//!
//! | Lane source              | Storage                          | Undo entry             |
//! |--------------------------|----------------------------------|------------------------|
//! | [`LaneSource::NoteVelocity`] | `MidiNoteState::velocity`     | `EditMidiNotes`        |
//! | [`LaneSource::NoteAccent`]   | `MidiNoteState::accent`       | `EditMidiNotes`        |
//! | [`LaneSource::Controller`]   | `MidiControllerLane` on the clip | `SetControllerPoints` |
//!
//! Nothing about violins (or any other instrument) is compiled into this file
//! — the lane list comes from [`InstrumentCapabilities`].
//!
//! Every lane renders through the piano roll's [`PianoRollViewport`], so zoom,
//! scroll, snap, and clip offset are shared with the notes above by
//! construction rather than by synchronization.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;

use gpui::prelude::FluentBuilder;
use gpui::{
    canvas, div, fill, point, px, Bounds, Context, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, MouseMoveEvent, ParentElement, Pixels, ScrollWheelEvent,
    StatefulInteractiveElement, Styled, Window,
};

use crate::components::edit::edit_commands::EditCommand;
use crate::components::piano_roll::{EditorMeter, PianoRollViewport};
use crate::components::timeline::timeline_state::{
    AccentState, MidiControllerKind, MidiControllerPoint, MidiNoteState,
};
use crate::solfege::{
    AccentReplacePolicy, LaneGroup, LaneSource, LaneSpec, SolfegeLaneVisibility,
    DEFAULT_LANE_HEIGHT, MAX_LANE_HEIGHT, MIN_LANE_HEIGHT,
};
use crate::theme::Colors;

use super::{SolfegeEditContext, SolfegeEditorPanel};

/// Height of the strip holding the `+ Lane` button.
const LANE_TOOLBAR_H: f32 = 22.0;
/// Grab zone on a lane's bottom edge that starts a resize drag. Wide enough to
/// hit without hunting; the strip carries the resize cursor so the affordance
/// is visible before the press.
const LANE_RESIZE_ZONE: f32 = 7.0;
/// Points closer together than this collapse while drawing, so one stroke does
/// not leave a breakpoint per pixel column.
const LANE_MERGE_BEATS: f32 = 1.0 / 128.0;

/// An in-progress lane gesture. Values are written to timeline state live so
/// the lane tracks the pointer; the single undo entry is recorded on release
/// from the `prev` snapshot taken here.
#[derive(Debug, Clone)]
enum LaneDrag {
    Controller {
        clip_id: String,
        kind: MidiControllerKind,
        prev: Vec<MidiControllerPoint>,
        erase: bool,
    },
    Velocity {
        clip_id: String,
        prev: Vec<MidiNoteState>,
    },
    /// Dragging accent. Snapshots the same `Vec<MidiNoteState>` a velocity drag
    /// does, because both write note fields and both close as one
    /// `EditCommand::EditMidiNotes` -- there is no accent-specific undo entry
    /// to write, and adding one would give accent a second history to keep in
    /// step with the notes it lives on.
    Accent {
        clip_id: String,
        prev: Vec<MidiNoteState>,
    },
    Resize {
        /// Needed on release to read back the net height without the render
        /// context the gesture started from.
        track_id: String,
        lane_id: String,
        start_y: f32,
        start_height: f32,
    },
}

/// Transient UI state for the performance lane stack. Lane *visibility* is not
/// here — it is project state on the track (`SolfegeTrackState::visible_lanes`)
/// so it survives save/load.
#[derive(Default)]
pub struct SolfegeLaneStack {
    /// `+ Lane` popup open state.
    menu_open: bool,
    drag: Option<LaneDrag>,
    /// Lane body bounds captured at paint, keyed by lane id, for pointer →
    /// (beat, value) mapping.
    bounds: HashMap<String, Rc<Cell<Option<Bounds<Pixels>>>>>,
}

impl SolfegeLaneStack {
    pub fn close_menus(&mut self) {
        self.menu_open = false;
    }

    fn bounds_cell(&mut self, lane_id: &str) -> Rc<Cell<Option<Bounds<Pixels>>>> {
        self.bounds
            .entry(lane_id.to_string())
            .or_insert_with(|| Rc::new(Cell::new(None)))
            .clone()
    }

    fn local(&self, lane_id: &str, position: gpui::Point<Pixels>) -> Option<(f32, f32)> {
        let bounds = self.bounds.get(lane_id)?.get()?;
        Some((
            f32::from(position.x - bounds.origin.x),
            f32::from(position.y - bounds.origin.y),
        ))
    }

    fn lane_height(&self, lane_id: &str) -> Option<f32> {
        self.bounds
            .get(lane_id)
            .and_then(|cell| cell.get())
            .map(|bounds| f32::from(bounds.size.height))
    }
}

/// Normalized value (`0.0..=1.0`) for a pointer `y` inside a lane of `height`.
fn value_from_y(local_y: f32, height: f32) -> f32 {
    let usable = (height - 8.0).max(1.0);
    (1.0 - (local_y - 4.0) / usable).clamp(0.0, 1.0)
}

/// Inverse of [`value_from_y`].
fn y_from_value(value: f32, height: f32) -> f32 {
    let usable = (height - 8.0).max(1.0);
    (1.0 - value.clamp(0.0, 1.0)) * usable + 4.0
}

impl SolfegeEditorPanel {
    /// Visible lanes for the current track, resolved against the instrument's
    /// capability table. Ids the instrument no longer exposes are skipped for
    /// rendering but kept in project state, so switching the instrument back
    /// restores the layout.
    fn visible_lanes(&self, ctx: &SolfegeEditContext) -> Vec<(SolfegeLaneVisibility, LaneSpec)> {
        ctx.solfege
            .visible_lanes
            .iter()
            .filter_map(|row| {
                ctx.capabilities
                    .lane(&row.lane_id)
                    .map(|spec| (row.clone(), *spec))
            })
            .collect()
    }

    /// Show or hide a lane. Lane layout is track state, so this goes through
    /// the timeline and marks the project dirty like any other project edit.
    fn toggle_lane(&mut self, ctx: &SolfegeEditContext, lane_id: &str, cx: &mut Context<Self>) {
        let track_id = ctx.track_id.clone();
        let mut next = ctx.solfege.clone();
        if let Some(index) = next
            .visible_lanes
            .iter()
            .position(|row| row.lane_id == lane_id)
        {
            next.visible_lanes.remove(index);
        } else {
            next.visible_lanes.push(SolfegeLaneVisibility::new(lane_id));
        }
        self.timeline.update(cx, |tl, tcx| {
            if tl.state.set_track_solfege_engine(&track_id, Some(next)) {
                tl.mark_project_changed(tcx);
            }
            tcx.notify();
        });
        cx.notify();
    }

    /// Write a lane height into track state.
    ///
    /// `mark_dirty` is `false` while a resize drag is live: the drag samples the
    /// pointer many times a second, and marking the project changed on each
    /// sample made one gesture look like hundreds of edits. The release marks it
    /// once for the net change, matching every other lane gesture in this file.
    fn set_lane_height(
        &mut self,
        ctx: &SolfegeEditContext,
        lane_id: &str,
        height: f32,
        mark_dirty: bool,
        cx: &mut Context<Self>,
    ) {
        let track_id = ctx.track_id.clone();
        let mut next = ctx.solfege.clone();
        let Some(row) = next
            .visible_lanes
            .iter_mut()
            .find(|row| row.lane_id == lane_id)
        else {
            return;
        };
        let clamped = height.clamp(MIN_LANE_HEIGHT, MAX_LANE_HEIGHT);
        if (row.height - clamped).abs() < 0.5 {
            return;
        }
        row.height = clamped;
        self.timeline.update(cx, |tl, tcx| {
            if tl.state.set_track_solfege_engine(&track_id, Some(next)) && mark_dirty {
                tl.mark_project_changed(tcx);
            }
            tcx.notify();
        });
        cx.notify();
    }

    /// Height stored for `lane_id` on `track_id` right now, so a finished resize
    /// can compare against the height captured on press.
    fn stored_lane_height(&self, track_id: &str, lane_id: &str, cx: &Context<Self>) -> Option<f32> {
        self.timeline
            .read(cx)
            .state
            .find_track(track_id)?
            .solfege
            .as_ref()?
            .visible_lanes
            .iter()
            .find(|row| row.lane_id == lane_id)
            .map(|row| row.height)
    }

    /// `true` when a lane has nothing to draw — no notes for a velocity lane,
    /// no breakpoints for a controller lane.
    fn lane_is_empty(&self, ctx: &SolfegeEditContext, spec: LaneSpec, cx: &Context<Self>) -> bool {
        let state = &self.timeline.read(cx).state;
        match spec.source {
            LaneSource::NoteVelocity => state
                .midi_clip_notes(&ctx.clip_id)
                .is_none_or(|notes| notes.is_empty()),
            LaneSource::NoteAccent => state
                .midi_clip_notes(&ctx.clip_id)
                .is_none_or(|notes| notes.iter().all(|note| note.accent.is_none())),
            LaneSource::Controller(kind) => state
                .controller_points_snapshot(&ctx.clip_id, kind)
                .is_empty(),
        }
    }

    // ── Gestures ─────────────────────────────────────────────────────────

    fn begin_lane_gesture(
        &mut self,
        ctx: &SolfegeEditContext,
        spec: LaneSpec,
        local: (f32, f32),
        erase: bool,
        cx: &mut Context<Self>,
    ) {
        let clip_id = ctx.clip_id.clone();
        let state = &self.timeline.read(cx).state;
        self.lanes.drag = Some(match spec.source {
            LaneSource::Controller(kind) => LaneDrag::Controller {
                prev: state.controller_points_snapshot(&clip_id, kind),
                clip_id,
                kind,
                erase,
            },
            LaneSource::NoteVelocity => LaneDrag::Velocity {
                prev: state.midi_clip_notes(&clip_id).cloned().unwrap_or_default(),
                clip_id,
            },
            LaneSource::NoteAccent => LaneDrag::Accent {
                prev: state.midi_clip_notes(&clip_id).cloned().unwrap_or_default(),
                clip_id,
            },
        });
        self.apply_lane_gesture(ctx, spec, local, cx);
    }

    /// Write the pointer's current (beat, value) into timeline state. Called on
    /// press and on every move while the gesture is live.
    fn apply_lane_gesture(
        &mut self,
        ctx: &SolfegeEditContext,
        spec: LaneSpec,
        local: (f32, f32),
        cx: &mut Context<Self>,
    ) {
        let Some(height) = self.lanes.lane_height(spec.id) else {
            return;
        };
        let viewport = self.viewport(cx);
        // The project's meter, so "1 Bar" lands on the bar lines the notes do.
        let meter = EditorMeter::from_map(&self.timeline.read(cx).state.time_signature_map);
        let beat = viewport
            .snap_in_clip(viewport.x_to_beat(local.0), &meter, ctx.clip_start_beat)
            .clamp(0.0, ctx.clip_beats);
        let value = value_from_y(local.1, height);
        let clip_id = ctx.clip_id.clone();

        match (&self.lanes.drag, spec.source) {
            (Some(LaneDrag::Controller { erase: true, .. }), LaneSource::Controller(kind)) => {
                // Erase sweeps a snap-step-wide window so a fast drag does not
                // leave points between samples.
                let half = viewport.snap_step_beats.max(LANE_MERGE_BEATS) * 0.5;
                self.timeline.update(cx, |tl, tcx| {
                    let mut points = tl.state.controller_points_snapshot(&clip_id, kind);
                    points.retain(|p| (p.beat - beat).abs() > half);
                    tl.state.set_controller_lane_points(&clip_id, kind, points);
                    tcx.notify();
                });
            }
            (Some(LaneDrag::Controller { .. }), LaneSource::Controller(kind)) => {
                self.timeline.update(cx, |tl, tcx| {
                    tl.state.put_controller_point(&clip_id, kind, beat, value);
                    tcx.notify();
                });
            }
            (Some(LaneDrag::Velocity { .. }), LaneSource::NoteVelocity) => {
                // A velocity bar belongs to a note, so the pointer picks the
                // note whose span contains this beat rather than a free point.
                let velocity = (value * 126.0).round().clamp(0.0, 126.0) as u8 + 1;
                let raw_beat = viewport.x_to_beat(local.0);
                let ids: Vec<u64> = self
                    .timeline
                    .read(cx)
                    .state
                    .midi_clip_notes(&clip_id)
                    .map(|notes| {
                        notes
                            .iter()
                            .filter(|note| {
                                raw_beat >= note.start && raw_beat <= note.start + note.duration
                            })
                            .map(|note| note.id)
                            .collect()
                    })
                    .unwrap_or_default();
                if ids.is_empty() {
                    return;
                }
                self.timeline.update(cx, |tl, tcx| {
                    tl.state
                        .set_midi_notes_velocity_bulk(&clip_id, &ids, velocity);
                    tcx.notify();
                });
            }
            (Some(LaneDrag::Accent { .. }), LaneSource::NoteAccent) => {
                // Accent belongs to a note, so the pointer picks the note whose
                // span contains this beat -- the same rule the velocity lane
                // uses, and the reason neither lane can leave a value with no
                // note under it.
                let raw_beat = viewport.x_to_beat(local.0);
                let ids: Vec<u64> = self
                    .timeline
                    .read(cx)
                    .state
                    .midi_clip_notes(&clip_id)
                    .map(|notes| {
                        notes
                            .iter()
                            .filter(|note| {
                                raw_beat >= note.start && raw_beat <= note.start + note.duration
                            })
                            .map(|note| note.id)
                            .collect()
                    })
                    .unwrap_or_default();
                if ids.is_empty() {
                    return;
                }
                self.timeline.update(cx, |tl, tcx| {
                    tl.state.set_midi_notes_accent_bulk(&clip_id, &ids, value);
                    tcx.notify();
                });
            }
            _ => {}
        }
        cx.notify();
    }

    /// Record the finished gesture as exactly one entry in the DAW's shared
    /// history. No-op gestures are dropped so undo never gains empty steps.
    fn commit_lane_gesture(&mut self, cx: &mut Context<Self>) {
        let Some(drag) = self.lanes.drag.take() else {
            return;
        };
        match drag {
            LaneDrag::Resize {
                track_id,
                lane_id,
                start_height,
                ..
            } => {
                // Lane layout has no `EditCommand` variant, so the net change
                // is recorded as one project-dirty mark rather than one per
                // pointer sample.
                let changed = self
                    .stored_lane_height(&track_id, &lane_id, cx)
                    .is_some_and(|height| (height - start_height).abs() >= 0.5);
                if changed {
                    self.timeline
                        .update(cx, |tl, tcx| tl.mark_project_changed(tcx));
                }
            }
            LaneDrag::Controller {
                clip_id,
                kind,
                prev,
                ..
            } => {
                let next = self
                    .timeline
                    .read(cx)
                    .state
                    .controller_points_snapshot(&clip_id, kind);
                if prev == next {
                    return;
                }
                self.timeline.update(cx, |tl, tcx| {
                    tl.record_executed_command(
                        EditCommand::SetControllerPoints {
                            clip_id,
                            kind,
                            prev,
                            next,
                        },
                        tcx,
                    );
                });
            }
            LaneDrag::Accent { clip_id, prev } | LaneDrag::Velocity { clip_id, prev } => {
                let next: Vec<MidiNoteState> = self
                    .timeline
                    .read(cx)
                    .state
                    .midi_clip_notes(&clip_id)
                    .cloned()
                    .unwrap_or_default();
                if prev == next {
                    return;
                }
                self.timeline.update(cx, |tl, tcx| {
                    tl.record_executed_command(
                        EditCommand::EditMidiNotes {
                            clip_id,
                            prev,
                            next,
                        },
                        tcx,
                    );
                });
            }
        }
        cx.notify();
    }

    // ── Rendering ────────────────────────────────────────────────────────

    /// The MIDI tab's full lane stack: the track's visible lanes plus the
    /// `+ Lane` strip.
    pub(super) fn render_lane_stack(
        &mut self,
        ctx: &SolfegeEditContext,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let rows = self.visible_lanes(ctx);
        self.render_lane_rows(ctx, rows, true, cx)
    }

    /// Render an explicit lane list. Shared by the MIDI tab (all visible lanes,
    /// with the `+ Lane` menu) and the Pitch tab (one supporting lane, no
    /// menu), so both use the same painter, gestures, and undo entries.
    pub(super) fn render_lane_rows(
        &mut self,
        ctx: &SolfegeEditContext,
        rows: Vec<(SolfegeLaneVisibility, LaneSpec)>,
        show_toolbar: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let viewport = self.viewport(cx);
        let toolbar = show_toolbar.then(|| self.render_lane_toolbar(ctx, cx));
        let lane_elements: Vec<gpui::AnyElement> = rows
            .iter()
            .enumerate()
            .map(|(index, (row, spec))| {
                // A lane the user added from `+ Lane` can be removed again; the
                // Pitch tab's fixed supporting lane cannot.
                self.render_lane(ctx, index, row, *spec, viewport, show_toolbar, cx)
                    .into_any_element()
            })
            .collect();
        let ctx_for_move = ctx.clone();
        let specs: HashMap<String, LaneSpec> = rows
            .iter()
            .map(|(row, spec)| (row.lane_id.clone(), *spec))
            .collect();

        // Wheel forwarding belongs to the MIDI tab only: there the piano roll
        // is mounted and its captured grid bounds are current. The Pitch tab's
        // support lane renders while the roll is unmounted, so clamping a
        // scroll against those stale bounds would fight the Pitch canvas —
        // that tab owns its own wheel handler.
        let forward_wheel = show_toolbar;
        let stack = div()
            .id("solfege-lane-stack")
            .flex()
            .flex_col()
            .flex_none()
            .w_full()
            .relative()
            .children(lane_elements)
            .children(toolbar)
            .on_mouse_move(
                cx.listener(move |this, event: &MouseMoveEvent, _window, cx| {
                    if event.pressed_button != Some(MouseButton::Left) {
                        // The button came up outside a lane — close the gesture so
                        // the next press starts a fresh, correctly-scoped undo entry.
                        if this.lanes.drag.is_some() {
                            this.commit_lane_gesture(cx);
                        }
                        return;
                    }
                    match this.lanes.drag.clone() {
                        Some(LaneDrag::Resize {
                            lane_id,
                            start_y,
                            start_height,
                            ..
                        }) => {
                            let delta = f32::from(event.position.y) - start_y;
                            this.set_lane_height(
                                &ctx_for_move,
                                &lane_id,
                                start_height + delta,
                                false,
                                cx,
                            );
                        }
                        Some(_) => {
                            // Route the move to whichever lane the gesture started
                            // on: `lanes.local` resolves against that lane's own
                            // captured bounds, so leaving the lane vertically keeps
                            // editing it instead of jumping to a neighbour.
                            for (lane_id, spec) in &specs {
                                if this.lanes.drag_targets(lane_id, *spec) {
                                    if let Some(local) = this.lanes.local(lane_id, event.position) {
                                        this.apply_lane_gesture(&ctx_for_move, *spec, local, cx);
                                    }
                                    break;
                                }
                            }
                        }
                        None => {}
                    }
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _event, _window, cx| this.commit_lane_gesture(cx)),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _event, _window, cx| this.commit_lane_gesture(cx)),
            );
        if !forward_wheel {
            return stack.into_any_element();
        }
        // The lane stack is a sibling of the piano roll, not a descendant, so
        // the roll's own wheel handler never sees events landing here. Forward
        // them so scroll and zoom mean the same thing over a lane as over the
        // notes it is aligned to.
        stack
            .on_scroll_wheel(cx.listener(
                |this, event: &ScrollWheelEvent, window: &mut Window, cx| {
                    cx.stop_propagation();
                    this.piano_roll.update(cx, |roll, roll_cx| {
                        roll.forward_wheel(event, window, roll_cx);
                    });
                    cx.notify();
                },
            ))
            .into_any_element()
    }

    /// `index` is the row's position in the rendered stack. It is what the
    /// element id is built from: an id derived from the lane name would collide
    /// (e.g. "velocity" and "dynamics" are both eight characters).
    fn render_lane(
        &mut self,
        ctx: &SolfegeEditContext,
        index: usize,
        row: &SolfegeLaneVisibility,
        spec: LaneSpec,
        viewport: PianoRollViewport,
        removable: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // A lane the user added from `+ Lane` owns a stored height and can be
        // removed; the Pitch tab's fixed support lane is neither.
        let resizable = removable;
        let height = row.height.clamp(MIN_LANE_HEIGHT, MAX_LANE_HEIGHT);
        let bounds_cell = self.lanes.bounds_cell(spec.id);
        let capture = bounds_cell.clone();
        let body_canvas = canvas(
            move |bounds, _window, _cx| capture.set(Some(bounds)),
            |_bounds, _result, _window, _cx| {},
        )
        .absolute()
        .inset_0();

        let content = match spec.source {
            LaneSource::NoteVelocity => self.render_velocity_lane(ctx, viewport, height, cx),
            LaneSource::NoteAccent => self.render_accent_lane(ctx, viewport, height, cx),
            LaneSource::Controller(kind) => {
                self.render_controller_lane(ctx, kind, viewport, height, cx)
            }
        };

        let ctx_down = ctx.clone();
        let track_id = ctx.track_id.clone();
        let lane_id = spec.id.to_string();
        let scale = self.render_lane_scale(index, spec, viewport);
        let name = self.render_lane_name(ctx, index, spec, removable, cx);
        // An empty lane otherwise shows only its baseline, which reads as a
        // broken surface rather than an empty one.
        let empty_hint = self.lane_is_empty(ctx, spec, cx).then(|| {
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(9.0))
                .text_color(Colors::text_faint())
                .child(match spec.source {
                    LaneSource::NoteVelocity => "No notes — draw notes above to shape velocity",
                    LaneSource::NoteAccent => {
                        "No accents — run Analyze Accent, or drag here to set them by hand"
                    }
                    LaneSource::Controller(_) => "Drag here to draw this lane",
                })
        });
        // Visible grab affordance for the resize edge. It carries no listener:
        // the lane body's own mouse-down resolves the resize from the same
        // `LANE_RESIZE_ZONE`, so there is one owner of the gesture.
        let resize_strip = resizable.then(|| {
            div()
                .absolute()
                .left_0()
                .right_0()
                .bottom_0()
                .h(px(LANE_RESIZE_ZONE))
                .cursor(gpui::CursorStyle::ResizeUpDown)
        });

        div()
            .id(("solfege-lane", index))
            .flex()
            .flex_row()
            .flex_none()
            .w_full()
            .h(px(height))
            .border_t(px(1.0))
            .border_color(Colors::border_subtle())
            .child(scale)
            .child(
                div()
                    .flex_1()
                    .h_full()
                    .relative()
                    .overflow_hidden()
                    .bg(Colors::surface_panel_alt())
                    .cursor(gpui::CursorStyle::Crosshair)
                    .child(body_canvas)
                    .child(content)
                    .children(empty_hint)
                    .children(resize_strip)
                    .child(name)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseDownEvent, _window, cx| {
                            cx.stop_propagation();
                            this.lanes.menu_open = false;
                            let Some(local) = this.lanes.local(&lane_id, event.position) else {
                                return;
                            };
                            let Some(height) = this.lanes.lane_height(&lane_id) else {
                                return;
                            };
                            // The bottom edge resizes instead of drawing —
                            // but only for a lane whose height is really stored.
                            // The Pitch tab's support lane is synthesized at a
                            // fixed height, so a resize there would silently
                            // rewrite the MIDI tab's lane and show nothing.
                            if resizable && local.1 >= height - LANE_RESIZE_ZONE {
                                this.lanes.drag = Some(LaneDrag::Resize {
                                    track_id: track_id.clone(),
                                    lane_id: lane_id.clone(),
                                    start_y: f32::from(event.position.y),
                                    start_height: height,
                                });
                                return;
                            }
                            let erase = event.modifiers.alt;
                            this.begin_lane_gesture(&ctx_down, spec, local, erase, cx);
                        }),
                    ),
            )
    }

    /// The lane's left gutter.
    ///
    /// It carries the lane's **value scale**, not its name: the gutter is the
    /// same narrow width as the pitch keyboard above it so every surface in the
    /// editor stays column-aligned, and a name would not fit there legibly. The
    /// name is an overlay inside the lane body instead (see
    /// [`Self::render_lane_name`]), which is also where a lane name is easiest
    /// to read against the curve.
    fn render_lane_scale(
        &self,
        index: usize,
        spec: LaneSpec,
        viewport: PianoRollViewport,
    ) -> impl IntoElement {
        let [top, mid, bottom] = spec.scale.marks();
        let mark = |text: &'static str| {
            div()
                .text_size(px(8.0))
                .text_color(Colors::text_faint())
                .child(text)
        };
        div()
            .id(("solfege-lane-scale", index))
            .flex()
            .flex_col()
            .items_end()
            .justify_between()
            .flex_none()
            .h_full()
            .w(px(viewport.key_lane_width.max(48.0)))
            .px(px(6.0))
            .py(px(3.0))
            .border_r(px(1.0))
            .border_color(Colors::border_subtle())
            .bg(Colors::surface_panel())
            .child(mark(top))
            .child(mark(mid))
            .child(mark(bottom))
    }

    /// The lane's name, overlaid on the top-left of its body. `on_remove` is
    /// `None` for a fixed lane (the Pitch tab's supporting lane), which then
    /// renders the name without a close affordance.
    fn render_lane_name(
        &self,
        ctx: &SolfegeEditContext,
        index: usize,
        spec: LaneSpec,
        removable: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let ctx_click = ctx.clone();
        let lane_id = spec.id.to_string();
        let close = removable.then(|| {
            div()
                .id(("solfege-lane-close", index))
                .flex()
                .items_center()
                .justify_center()
                .size(px(12.0))
                .rounded(px(crate::theme::radius::CONTROL))
                .text_size(px(8.0))
                .text_color(Colors::text_faint())
                .cursor(gpui::CursorStyle::PointingHand)
                .hover(|style| {
                    style
                        .bg(Colors::surface_hover())
                        .text_color(Colors::text_primary())
                })
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    this.toggle_lane(&ctx_click, &lane_id, cx);
                }))
                .child("\u{2715}")
        });
        div()
            .absolute()
            .top(px(3.0))
            .left(px(6.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .px(px(4.0))
            .rounded(px(crate::theme::radius::CONTROL))
            // The overlay sits inside the lane body, which owns the draw
            // gesture. Without occluding, clicking the close button would also
            // paint a value into the lane on the way out.
            .occlude()
            .bg(Colors::with_alpha(Colors::surface_panel(), 0.72))
            .text_size(px(9.0))
            .text_color(Colors::text_secondary())
            .child(spec.label)
            .children(close)
    }

    /// One bar per note, aligned to the note above it in the piano roll.
    fn render_velocity_lane(
        &self,
        ctx: &SolfegeEditContext,
        viewport: PianoRollViewport,
        height: f32,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let bars: Vec<(f32, f32, f32)> = self
            .timeline
            .read(cx)
            .state
            .midi_clip_notes(&ctx.clip_id)
            .map(|notes| {
                notes
                    .iter()
                    .map(|note| {
                        let x = viewport.beat_to_x(note.start);
                        let w = (note.duration * viewport.ppb).max(2.0);
                        let value = f32::from(note.velocity) / 127.0;
                        (x, w, value)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let fill_color = Colors::accent_primary();
        let baseline_color = Colors::with_alpha(Colors::border_subtle(), 0.8);
        canvas(
            |_bounds, _window, _cx| {},
            move |bounds: Bounds<Pixels>, (), window, _cx| {
                let origin = bounds.origin;
                let view_w = f32::from(bounds.size.width);
                let baseline_y = height - 4.0;
                window.paint_quad(fill(
                    Bounds::new(
                        origin + point(px(0.0), px(baseline_y)),
                        gpui::size(px(view_w), px(1.0)),
                    ),
                    baseline_color,
                ));
                for (x, w, value) in &bars {
                    if x + w < 0.0 || *x > view_w {
                        continue;
                    }
                    let top = y_from_value(*value, height);
                    let left = x.max(0.0);
                    let right = (x + w).min(view_w);
                    if right <= left {
                        continue;
                    }
                    window.paint_quad(fill(
                        Bounds::new(
                            origin + point(px(left), px(top)),
                            gpui::size(px(right - left), px((baseline_y - top).max(1.0))),
                        ),
                        fill_color,
                    ));
                }
            },
        )
        .absolute()
        .inset_0()
        .into_any_element()
    }

    /// One bar per note, drawn from the note's [`AccentState`].
    ///
    /// Bars rather than a curve, and rising from the *middle* of the lane
    /// rather than from the floor. Both choices follow from what an accent
    /// value means: it is an event-level judgement about one note, and its
    /// neutral reading is 0.5 rather than zero — the analyser's scale is
    /// relative to a note's neighbourhood, so a phrase of evenly played notes
    /// is a phrase of 0.5s and a note *below* the midline is one the player
    /// would lean away from. A floor-anchored bar chart would draw that phrase
    /// as half-full bars and make "de-emphasised" indistinguishable from
    /// "slightly emphasised".
    ///
    /// Hand-edited notes are drawn in a second colour. Without it, re-running
    /// the analysis with "keep manual edits" would leave some bars untouched
    /// for no visible reason.
    fn render_accent_lane(
        &self,
        ctx: &SolfegeEditContext,
        viewport: PianoRollViewport,
        height: f32,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        struct Bar {
            x: f32,
            width: f32,
            accent: AccentState,
        }
        let bars: Vec<Bar> = self
            .timeline
            .read(cx)
            .state
            .midi_clip_notes(&ctx.clip_id)
            .map(|notes| {
                notes
                    .iter()
                    .filter_map(|note| {
                        note.accent.map(|accent| Bar {
                            x: viewport.beat_to_x(note.start),
                            width: (note.duration * viewport.ppb).max(2.0),
                            accent,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        let generated = Colors::accent_primary();
        let manual = Colors::accent_cyan();
        let midline_color = Colors::with_alpha(Colors::border_default(), 0.9);
        canvas(
            |_bounds, _window, _cx| {},
            move |bounds: Bounds<Pixels>, (), window, _cx| {
                let origin = bounds.origin;
                let view_w = f32::from(bounds.size.width);
                let neutral_y = y_from_value(0.5, height);
                window.paint_quad(fill(
                    Bounds::new(
                        origin + point(px(0.0), px(neutral_y)),
                        gpui::size(px(view_w), px(1.0)),
                    ),
                    midline_color,
                ));
                for bar in &bars {
                    if bar.x + bar.width < 0.0 || bar.x > view_w {
                        continue;
                    }
                    let left = bar.x.max(0.0);
                    let right = (bar.x + bar.width).min(view_w);
                    if right <= left {
                        continue;
                    }
                    let value_y = y_from_value(bar.accent.prominence, height);
                    let top = value_y.min(neutral_y);
                    let bottom = value_y.max(neutral_y);
                    let base = if bar.accent.source
                        == crate::components::timeline::timeline_state::AccentSource::Manual
                    {
                        manual
                    } else {
                        generated
                    };
                    // A low-confidence reading is drawn fainter. The analyser
                    // reports how much its training data agreed about notes
                    // like this one, and a bar that is a guess should not look
                    // like a measurement. A hand-drawn accent carries no
                    // confidence and is drawn at full strength: the user is not
                    // making a probabilistic claim.
                    let strength = if bar.accent.source
                        == crate::components::timeline::timeline_state::AccentSource::Manual
                    {
                        1.0
                    } else {
                        0.45 + 0.55 * bar.accent.confidence.clamp(0.0, 1.0)
                    };
                    window.paint_quad(fill(
                        Bounds::new(
                            origin + point(px(left), px(top)),
                            gpui::size(px(right - left), px((bottom - top).max(1.0))),
                        ),
                        Colors::with_alpha(base, strength),
                    ));
                    // A cap at the value, so a note sitting exactly on neutral
                    // still shows where it is rather than vanishing into the
                    // midline.
                    window.paint_quad(fill(
                        Bounds::new(
                            origin + point(px(left), px(value_y - 1.0)),
                            gpui::size(px(right - left), px(2.0)),
                        ),
                        base,
                    ));
                }
            },
        )
        .absolute()
        .inset_0()
        .into_any_element()
    }

    /// A continuous controller lane, sampled once per column so cost is
    /// `O(width + points)` regardless of how dense the lane gets.
    fn render_controller_lane(
        &self,
        ctx: &SolfegeEditContext,
        kind: MidiControllerKind,
        viewport: PianoRollViewport,
        height: f32,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let points = self
            .timeline
            .read(cx)
            .state
            .controller_points_snapshot(&ctx.clip_id, kind);
        let line_color = Colors::accent_cyan();
        let handle_color = Colors::accent_primary();
        let baseline_color = Colors::with_alpha(Colors::border_subtle(), 0.8);
        let ppb = viewport.ppb;
        let scroll_x = viewport.scroll_x;
        canvas(
            |_bounds, _window, _cx| {},
            move |bounds: Bounds<Pixels>, (), window, _cx| {
                let origin = bounds.origin;
                let view_w = f32::from(bounds.size.width).max(1.0);
                window.paint_quad(fill(
                    Bounds::new(
                        origin + point(px(0.0), px(height * 0.5)),
                        gpui::size(px(view_w), px(1.0)),
                    ),
                    baseline_color,
                ));
                if points.is_empty() {
                    return;
                }
                let value_at = |beat: f32| -> f32 {
                    match points.binary_search_by(|p| p.beat.total_cmp(&beat)) {
                        Ok(hit) => points[hit].value,
                        Err(0) => points[0].value,
                        Err(insert) if insert >= points.len() => points[points.len() - 1].value,
                        Err(insert) => {
                            let a = &points[insert - 1];
                            let b = &points[insert];
                            let span = b.beat - a.beat;
                            if span <= f32::EPSILON {
                                b.value
                            } else {
                                a.value + (b.value - a.value) * ((beat - a.beat) / span)
                            }
                        }
                    }
                };
                let columns = view_w.ceil() as usize;
                let mut path = gpui::PathBuilder::stroke(px(1.4));
                for col in 0..=columns {
                    let beat = ((col as f32 + scroll_x) / ppb).max(0.0);
                    let y = y_from_value(value_at(beat), height);
                    let at = origin + point(px(col as f32), px(y));
                    if col == 0 {
                        path.move_to(at);
                    } else {
                        path.line_to(at);
                    }
                }
                if let Ok(path) = path.build() {
                    window.paint_path(path, line_color);
                }
                for p in &points {
                    let x = p.beat * ppb - scroll_x;
                    if x < -3.0 || x > view_w + 3.0 {
                        continue;
                    }
                    let y = y_from_value(p.value, height);
                    window.paint_quad(fill(
                        Bounds::new(
                            origin + point(px(x - 2.0), px(y - 2.0)),
                            gpui::size(px(4.0), px(4.0)),
                        ),
                        handle_color,
                    ));
                }
            },
        )
        .absolute()
        .inset_0()
        .into_any_element()
    }

    /// The `+ Lane` strip. Lanes are opt-in: nothing is shown by default beyond
    /// the instrument's small default set, so the piano roll keeps the viewport.
    fn render_lane_toolbar(
        &mut self,
        ctx: &SolfegeEditContext,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let menu = self.lanes.menu_open.then(|| self.render_lane_menu(ctx, cx));
        div()
            .id("solfege-lane-toolbar")
            .flex()
            .flex_row()
            .items_center()
            .flex_none()
            .w_full()
            .h(px(LANE_TOOLBAR_H))
            .px(px(6.0))
            .gap(px(6.0))
            .relative()
            .border_t(px(1.0))
            .border_color(Colors::border_subtle())
            .bg(Colors::surface_titlebar())
            .child(
                div()
                    .id("solfege-add-lane")
                    .flex()
                    .items_center()
                    .h(px(16.0))
                    .px(px(7.0))
                    .rounded(px(crate::theme::radius::CONTROL))
                    .bg(if self.lanes.menu_open {
                        Colors::accent_muted()
                    } else {
                        Colors::surface_input()
                    })
                    .text_size(px(9.5))
                    .text_color(Colors::text_secondary())
                    .cursor(gpui::CursorStyle::PointingHand)
                    .hover(|style| style.bg(Colors::surface_hover()))
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.lanes.menu_open = !this.lanes.menu_open;
                        cx.notify();
                    }))
                    .child("+ Lane"),
            )
            .children(self.render_analyze_accent(ctx, cx))
            .child(
                div()
                    .text_size(px(9.0))
                    .text_color(Colors::text_faint())
                    .child(ctx.capabilities.family.label()),
            )
            .children(menu)
    }

    /// The Analyze Accent control, and what the last pass reported.
    ///
    /// It lives in the lane strip rather than in a menu because that is where
    /// the lane it fills is: the button, the lane, and the result are one
    /// surface. The main press runs the analysis with the safe policy; the
    /// caret offers the destructive one, which is never the default.
    ///
    /// While a pass is running the button is disabled and names its stage. It
    /// does not animate: on a clip of a few hundred notes the whole analysis is
    /// under a millisecond of arithmetic plus whatever the first model read
    /// costs, and a spinner for something that is usually already finished is a
    /// loading state imitating work.
    fn render_analyze_accent(
        &mut self,
        ctx: &SolfegeEditContext,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        // Only offered where it can do something: an instrument with no accent
        // lane in its table has no place to put the result.
        ctx.capabilities.lane("accent")?;

        use crate::components::solfege_editor::AccentAnalysisState;

        let running = matches!(self.accent_analysis(), AccentAnalysisState::Running { .. });
        let (label, tone) = match self.accent_analysis() {
            AccentAnalysisState::Running { notes } => (
                format!("Analyzing {notes} notes\u{2026}"),
                Colors::text_muted(),
            ),
            AccentAnalysisState::Done { summary } => (summary.clone(), Colors::text_faint()),
            AccentAnalysisState::Failed { message } => (message.clone(), Colors::status_error()),
            AccentAnalysisState::Idle => (String::new(), Colors::text_faint()),
        };

        let button = div()
            .id("solfege-analyze-accent")
            .flex()
            .items_center()
            .h(px(16.0))
            .px(px(7.0))
            .rounded(px(crate::theme::radius::CONTROL))
            .bg(if running {
                Colors::surface_panel()
            } else {
                Colors::surface_input()
            })
            .text_size(px(9.5))
            .text_color(if running {
                Colors::text_faint()
            } else {
                Colors::text_secondary()
            })
            .when(!running, |style| {
                style
                    .cursor(gpui::CursorStyle::PointingHand)
                    .hover(|style| style.bg(Colors::surface_hover()))
            })
            .child("Analyze Accent");
        let button = if running {
            button
        } else {
            button.on_click(cx.listener(|this, _event, _window, cx| {
                this.analyze_accent(AccentReplacePolicy::KeepManual, cx);
            }))
        };

        let caret = div()
            .id("solfege-analyze-accent-options")
            .flex()
            .items_center()
            .justify_center()
            .h(px(16.0))
            .w(px(14.0))
            .rounded(px(crate::theme::radius::CONTROL))
            .bg(if self.accent_menu_open {
                Colors::accent_muted()
            } else {
                Colors::surface_input()
            })
            .text_size(px(8.0))
            .text_color(Colors::text_faint())
            .cursor(gpui::CursorStyle::PointingHand)
            .hover(|style| style.bg(Colors::surface_hover()))
            .on_click(cx.listener(|this, _event, _window, cx| {
                this.accent_menu_open = !this.accent_menu_open;
                this.lanes.close_menus();
                cx.notify();
            }))
            .child("\u{25BE}");

        let options = self.accent_menu_open.then(|| {
            let mut menu = div()
                .id("solfege-accent-menu")
                .absolute()
                .bottom(px(LANE_TOOLBAR_H + 2.0))
                .left(px(96.0))
                .flex()
                .flex_col()
                .w(px(200.0))
                .py(px(3.0))
                .rounded(px(crate::theme::radius::CONTROL))
                .border(px(1.0))
                .border_color(Colors::border_default())
                .bg(Colors::surface_overlay())
                .occlude();
            for (index, policy) in [
                AccentReplacePolicy::KeepManual,
                AccentReplacePolicy::ReplaceAll,
            ]
            .into_iter()
            .enumerate()
            {
                menu = menu.child(
                    div()
                        .id(("solfege-accent-policy", index))
                        .flex()
                        .items_center()
                        .h(px(19.0))
                        .px(px(9.0))
                        .text_size(px(10.0))
                        .text_color(Colors::text_primary())
                        .cursor(gpui::CursorStyle::PointingHand)
                        .hover(|style| style.bg(Colors::surface_hover()))
                        .on_click(cx.listener(move |this, _event, _window, cx| {
                            this.accent_menu_open = false;
                            this.analyze_accent(policy, cx);
                        }))
                        .child(policy.label()),
                );
            }
            menu.into_any_element()
        });

        // The second half of the workflow, and a separate press. Analysis
        // produces a reading; this acts on it. Disabled until there is
        // something to act on, so the button never promises a change it cannot
        // make.
        let has_accents = self
            .timeline
            .read(cx)
            .state
            .midi_clip_notes(&ctx.clip_id)
            .is_some_and(|notes| notes.iter().any(|note| note.accent.is_some()));
        let apply = div()
            .id("solfege-apply-accent")
            .flex()
            .items_center()
            .h(px(16.0))
            .px(px(7.0))
            .rounded(px(crate::theme::radius::CONTROL))
            .bg(Colors::surface_input())
            .text_size(px(9.5))
            .text_color(if has_accents {
                Colors::text_secondary()
            } else {
                Colors::text_faint()
            })
            .when(has_accents, |style| {
                style
                    .cursor(gpui::CursorStyle::PointingHand)
                    .hover(|style| style.bg(Colors::surface_hover()))
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.apply_accent_to_performance(cx);
                    }))
            })
            .child("Apply");

        Some(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(3.0))
                .child(button)
                .child(caret)
                .child(apply)
                .children(options)
                .child(
                    div()
                        .text_size(px(9.0))
                        .text_color(tone)
                        .overflow_hidden()
                        .child(label),
                )
                .into_any_element(),
        )
    }

    fn render_lane_menu(
        &self,
        ctx: &SolfegeEditContext,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let visible: Vec<String> = ctx
            .solfege
            .visible_lanes
            .iter()
            .map(|row| row.lane_id.clone())
            .collect();
        let mut menu = div()
            .id("solfege-lane-menu")
            .absolute()
            .bottom(px(LANE_TOOLBAR_H + 2.0))
            .left(px(6.0))
            .flex()
            .flex_col()
            .w(px(180.0))
            .py(px(3.0))
            .rounded(px(crate::theme::radius::CONTROL))
            .border(px(1.0))
            .border_color(Colors::border_default())
            .bg(Colors::surface_overlay());

        for group in [LaneGroup::Performance, LaneGroup::Instrument] {
            let mut any = false;
            let mut rows: Vec<gpui::AnyElement> = Vec::new();
            for (index, spec) in ctx.capabilities.lanes_in_group(group).enumerate() {
                any = true;
                let checked = visible.iter().any(|id| id == spec.id);
                let ctx_click = ctx.clone();
                let lane_id = spec.id.to_string();
                rows.push(
                    div()
                        .id((
                            if group == LaneGroup::Performance {
                                "solfege-lane-menu-perf"
                            } else {
                                "solfege-lane-menu-inst"
                            },
                            index,
                        ))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .h(px(19.0))
                        .px(px(9.0))
                        .text_size(px(10.0))
                        .text_color(Colors::text_primary())
                        .cursor(gpui::CursorStyle::PointingHand)
                        .hover(|style| style.bg(Colors::surface_hover()))
                        .on_click(cx.listener(move |this, _event, _window, cx| {
                            this.toggle_lane(&ctx_click, &lane_id, cx);
                        }))
                        .child(
                            div()
                                .w(px(8.0))
                                .text_size(px(9.0))
                                .text_color(Colors::accent_primary())
                                .child(if checked { "\u{2713}" } else { "" }),
                        )
                        .child(spec.label)
                        .into_any_element(),
                );
            }
            if !any {
                continue;
            }
            menu = menu
                .child(
                    div()
                        .px(px(9.0))
                        .pt(px(4.0))
                        .pb(px(2.0))
                        .text_size(px(8.5))
                        .text_color(Colors::text_faint())
                        .child(group.label()),
                )
                .children(rows);
        }
        menu.into_any_element()
    }
}

impl SolfegeLaneStack {
    /// Whether a live gesture belongs to `lane_id`.
    fn drag_targets(&self, lane_id: &str, spec: LaneSpec) -> bool {
        match &self.drag {
            Some(LaneDrag::Controller { kind, .. }) => {
                spec.id == lane_id && matches!(spec.source, LaneSource::Controller(k) if k == *kind)
            }
            Some(LaneDrag::Velocity { .. }) => {
                spec.id == lane_id && spec.source == LaneSource::NoteVelocity
            }
            Some(LaneDrag::Accent { .. }) => {
                spec.id == lane_id && spec.source == LaneSource::NoteAccent
            }
            _ => false,
        }
    }
}

/// Default lane height, re-exported so the Pitch tab's supporting lane matches
/// the MIDI tab's density.
pub(super) const SUPPORT_LANE_HEIGHT: f32 = DEFAULT_LANE_HEIGHT;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_mapping_round_trips() {
        for height in [40.0f32, 72.0, 160.0] {
            for value in [0.0f32, 0.25, 0.5, 1.0] {
                let y = y_from_value(value, height);
                assert!((value_from_y(y, height) - value).abs() < 0.001);
            }
        }
    }

    #[test]
    fn value_from_y_clamps_outside_the_lane() {
        assert_eq!(value_from_y(-50.0, 72.0), 1.0);
        assert_eq!(value_from_y(500.0, 72.0), 0.0);
    }

    /// The resize strip lives on the lane's bottom edge and swallows presses
    /// there. At the smallest allowed lane height it must still leave a usable
    /// draw area above it, or a short lane becomes resize-only.
    #[test]
    fn resize_zone_leaves_a_draw_area_at_the_minimum_lane_height() {
        assert!(LANE_RESIZE_ZONE * 4.0 < MIN_LANE_HEIGHT);
    }

    /// A default lane stack must not fill the panel. Every instrument family
    /// opens with a small default set, so the piano roll keeps the viewport.
    #[test]
    fn default_lane_stacks_stay_small() {
        use crate::solfege::{capabilities_for_family, InstrumentFamily};
        for family in [
            InstrumentFamily::BowedString,
            InstrumentFamily::Wind,
            InstrumentFamily::ThaiBowed,
            InstrumentFamily::Generic,
        ] {
            let caps = capabilities_for_family(family);
            assert!(
                caps.default_visible_lanes.len() <= 2,
                "{family:?} opens with {} lanes",
                caps.default_visible_lanes.len()
            );
            let stack_h =
                caps.default_visible_lanes.len() as f32 * DEFAULT_LANE_HEIGHT + LANE_TOOLBAR_H;
            assert!(stack_h <= 220.0, "{family:?} default stack is {stack_h}px");
        }
    }
}

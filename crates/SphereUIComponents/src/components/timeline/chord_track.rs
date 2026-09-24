//! Global Chord Track lane — the project's harmony as a chord chart.
//!
//! Blocks follow the Region lane's gesture model: the body moves, the edges
//! trim, one undo entry per gesture, all through the same beat→x transform as
//! the ruler and grid. A progression dragged in from the Chord Generator shows
//! as a ghost here before it lands, so the drop is never a surprise.

use crate::components::timeline::global_lane_header::{
    global_lane_header, global_lane_resize_handle, GlobalLaneHeaderActions, GlobalLaneResizeArmCb,
    GlobalLaneResizeResetCb,
};
use crate::components::timeline::region_track::{GlobalLaneMenuCallback, GlobalLaneVoidCallback};
use crate::components::timeline::timeline_grid::timeline_grid;
use crate::components::timeline::timeline_state::{
    ChordDropTarget, GlobalLaneKind, TimelineState, GLOBAL_LANE_RESIZE_HANDLE_HITBOX,
};
use crate::theme::{radius, Colors};
use gpui::{
    div, px, AppContext, Empty, InteractiveElement, IntoElement, ParentElement, Render,
    StatefulInteractiveElement, Styled, Window,
};

/// Chord lane mouse-down: `(beat, event_id, click_count)`.
pub type ChordTrackDownCallback =
    std::sync::Arc<dyn Fn(&(f64, Option<u64>, u32), &mut gpui::Window, &mut gpui::App) + 'static>;

/// Chord lane right-click: `(beat, event_id, window_x, window_y)`.
pub type ChordTrackContextCallback = std::sync::Arc<
    dyn Fn(&(f64, Option<u64>, f32, f32), &mut gpui::Window, &mut gpui::App) + 'static,
>;

pub type ChordTrackDragCallback =
    std::sync::Arc<dyn Fn(&ChordEventDragUpdate, &mut gpui::Window, &mut gpui::App) + 'static>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChordEventDragMode {
    Move,
    Start,
    End,
}

/// Drag payload for one chord block.
#[derive(Clone, Debug)]
pub struct ChordEventDrag {
    pub event_id: u64,
    pub mode: ChordEventDragMode,
    pub start_beat: f64,
    pub end_beat: f64,
    pub pointer_offset_x: f32,
}

impl Render for ChordEventDrag {
    fn render(&mut self, _w: &mut Window, _cx: &mut gpui::Context<Self>) -> impl IntoElement {
        Empty
    }
}

#[derive(Clone, Debug)]
pub struct ChordEventDragUpdate {
    pub event_id: u64,
    pub start_beat: f64,
    pub end_beat: f64,
}

const EDGE_W: f32 = 5.0;
const BLOCK_INSET_Y: f32 = 3.0;

#[allow(clippy::too_many_arguments)]
pub fn chord_track_lane(
    state: &TimelineState,
    lane_height: f32,
    on_down: Option<ChordTrackDownCallback>,
    on_context: Option<ChordTrackContextCallback>,
    on_drag: Option<ChordTrackDragCallback>,
    on_open_generator: Option<GlobalLaneVoidCallback>,
    on_header_menu: Option<GlobalLaneMenuCallback>,
    on_hide: Option<GlobalLaneVoidCallback>,
    on_toggle_collapsed: Option<GlobalLaneVoidCallback>,
    on_resize_arm: Option<GlobalLaneResizeArmCb>,
    on_resize_reset: Option<GlobalLaneResizeResetCb>,
) -> impl IntoElement {
    let lane_w = state.viewport.viewport_width.max(1.0);
    let selected = state.selected_chord_event_id;
    let block_h = (lane_height - 2.0 * BLOCK_INSET_Y - GLOBAL_LANE_RESIZE_HANDLE_HITBOX).max(12.0);
    let collapsed = state.chord_track_collapsed;
    let accent = Colors::accent_primary();

    let blocks: Vec<gpui::AnyElement> = state
        .chord_events
        .iter()
        .filter_map(|event| {
            let x = state.beats_to_x(event.start_beat as f32);
            let width = (state.beats_to_x(event.end_beat() as f32) - x).max(2.0);
            if x > lane_w + 32.0 || x + width < -32.0 {
                return None;
            }
            let is_selected = selected == Some(event.id);
            let body = ChordEventDrag {
                event_id: event.id,
                mode: ChordEventDragMode::Move,
                start_beat: event.start_beat,
                end_beat: event.end_beat(),
                pointer_offset_x: 0.0,
            };
            let start_drag = ChordEventDrag {
                mode: ChordEventDragMode::Start,
                ..body.clone()
            };
            let end_drag = ChordEventDrag {
                mode: ChordEventDragMode::End,
                ..body.clone()
            };
            let rest = Colors::surface_raised();
            Some(
                div()
                    .absolute()
                    .left(px(x))
                    .top(px(BLOCK_INSET_Y))
                    .w(px(width))
                    .h(px(block_h))
                    .id(("chord-lane-block", event.id))
                    .rounded(px(radius::clamped(radius::CONTROL_SM, width, block_h)))
                    .overflow_hidden()
                    .bg(if is_selected {
                        Colors::composite(rest, Colors::state_selected())
                    } else {
                        rest
                    })
                    .border(px(1.0))
                    .border_color(if is_selected {
                        Colors::with_alpha(accent, 0.9)
                    } else {
                        Colors::border_normal()
                    })
                    .cursor(gpui::CursorStyle::PointingHand)
                    .on_drag(body, move |drag, offset, _window, cx| {
                        cx.new(|_| ChordEventDrag {
                            pointer_offset_x: offset.x.into(),
                            ..drag.clone()
                        })
                    })
                    .child(
                        div()
                            .absolute()
                            .left(px(EDGE_W + 1.0))
                            .top_0()
                            .bottom_0()
                            .right(px(EDGE_W + 1.0))
                            .flex()
                            .items_center()
                            .text_size(px(if collapsed { 10.0 } else { 12.0 }))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(Colors::text_primary())
                            .whitespace_nowrap()
                            .truncate()
                            .child(event.name()),
                    )
                    .child(
                        div()
                            .absolute()
                            .left_0()
                            .top_0()
                            .bottom_0()
                            .w(px(EDGE_W))
                            .id(("chord-lane-start", event.id))
                            .cursor(gpui::CursorStyle::ResizeLeftRight)
                            .on_drag(start_drag, |drag, _offset, _window, cx| {
                                cx.new(|_| drag.clone())
                            }),
                    )
                    .child(
                        div()
                            .absolute()
                            .right_0()
                            .top_0()
                            .bottom_0()
                            .w(px(EDGE_W))
                            .id(("chord-lane-end", event.id))
                            .cursor(gpui::CursorStyle::ResizeLeftRight)
                            .on_drag(end_drag, |drag, _offset, _window, cx| {
                                cx.new(|_| drag.clone())
                            }),
                    )
                    .into_any_element(),
            )
        })
        .collect();

    // Ghost of a progression being dragged in from the Chord Generator.
    let ghosts: Vec<gpui::AnyElement> = state
        .chord_drop_preview
        .as_ref()
        .filter(|preview| preview.target == ChordDropTarget::ChordTrack)
        .map(|preview| {
            let mut beat = preview.start_beat;
            preview
                .chords
                .iter()
                .map(|placement| {
                    let x = state.beats_to_x(beat as f32);
                    let width =
                        (state.beats_to_x((beat + placement.length_beats) as f32) - x).max(2.0);
                    beat += placement.length_beats;
                    div()
                        .absolute()
                        .left(px(x))
                        .top(px(BLOCK_INSET_Y))
                        .w(px(width))
                        .h(px(block_h))
                        .rounded(px(radius::clamped(radius::CONTROL_SM, width, block_h)))
                        .bg(Colors::with_alpha(accent, 0.18))
                        .border(px(1.0))
                        .border_color(Colors::with_alpha(accent, 0.8))
                        .flex()
                        .items_center()
                        .px(px(EDGE_W + 1.0))
                        .text_size(px(12.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(accent)
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .child(placement.chord.name(placement.flats))
                        .into_any_element()
                })
                .collect()
        })
        .unwrap_or_default();

    let interaction = on_down.map(|cb| {
        let state_hit = state.clone();
        let mut layer = div()
            .absolute()
            .inset_0()
            .id("chord-track-hit")
            .on_mouse_down(
                gpui::MouseButton::Left,
                move |event: &gpui::MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    let wx: f32 = event.position.x.into();
                    let lane_x = state_hit.lane_x_from_window_x(wx);
                    let beat = state_hit.x_to_beat(lane_x).max(0.0);
                    let id = state_hit.chord_event_at(beat);
                    let snapped = state_hit.snap_beats(beat as f32).max(0.0) as f64;
                    cb(&(snapped, id, event.click_count as u32), window, cx);
                },
            );
        if let Some(ctx_cb) = on_context {
            let state_ctx = state.clone();
            layer = layer.on_mouse_down(
                gpui::MouseButton::Right,
                move |event: &gpui::MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    let wx: f32 = event.position.x.into();
                    let wy: f32 = event.position.y.into();
                    let lane_x = state_ctx.lane_x_from_window_x(wx);
                    let beat = state_ctx.x_to_beat(lane_x).max(0.0);
                    let id = state_ctx.chord_event_at(beat);
                    ctx_cb(&(beat, id, wx, wy), window, cx);
                },
            );
        }
        layer
    });

    let header = global_lane_header(
        "chord",
        "Chords",
        state.chord_lane_header_subtitle(),
        collapsed,
        "Hide Chord Track",
        GlobalLaneHeaderActions {
            on_add: on_open_generator,
            on_menu: on_header_menu,
            on_hide,
            on_toggle_collapsed,
        },
    );

    let resize_handle = on_resize_arm
        .zip(on_resize_reset)
        .map(|(arm, reset)| global_lane_resize_handle(GlobalLaneKind::Chord, arm, reset));

    let gesture = std::rc::Rc::new(
        crate::components::timeline::timeline_state::TimelineGestureContext::from_state(state),
    );

    let mut content = div()
        .flex_1()
        .h_full()
        .relative()
        .overflow_hidden()
        .bg(Colors::timeline_content_background())
        .child(timeline_grid(state, lane_w, lane_height))
        .children(interaction)
        .children(blocks)
        .children(ghosts);

    if let Some(drag_cb) = on_drag {
        content = content.on_drag_move::<ChordEventDrag>(
            move |event: &gpui::DragMoveEvent<ChordEventDrag>, window, cx| {
                let drag = event.drag(cx);
                let x: f32 = event.event.position.x.into();
                let ox: f32 = event.bounds.origin.x.into();
                let local_x = (x - ox).max(0.0);
                let beat_at_x = |x: f32| gesture.snap_beats(gesture.x_to_beats(x)).max(0.0) as f64;
                let (start_beat, end_beat) = match drag.mode {
                    ChordEventDragMode::Move => {
                        let length = (drag.end_beat - drag.start_beat).max(1.0e-3);
                        let start = beat_at_x(local_x - drag.pointer_offset_x);
                        (start, start + length)
                    }
                    ChordEventDragMode::Start => (beat_at_x(local_x), drag.end_beat),
                    ChordEventDragMode::End => (drag.start_beat, beat_at_x(local_x)),
                };
                drag_cb(
                    &ChordEventDragUpdate {
                        event_id: drag.event_id,
                        start_beat,
                        end_beat,
                    },
                    window,
                    cx,
                );
                window.prevent_default();
                cx.stop_propagation();
            },
        );
    }

    div()
        .flex()
        .flex_row()
        .relative()
        .h(px(lane_height))
        .w_full()
        .bg(Colors::surface_panel_alt())
        .border_b(px(1.0))
        .border_color(Colors::border_subtle())
        .child(header)
        .child(content)
        .children(resize_handle)
}

/// Ghost of a progression about to become a MIDI clip: a clip-shaped outline
/// on the target track row (or below the last track for a new one), with the
/// chord names laid out where their notes will land. Drawn in lane space
/// (x from the lane origin, y from the top of the track area).
pub(crate) fn chord_drop_clip_overlay(state: &TimelineState) -> Option<gpui::AnyElement> {
    let preview = state.chord_drop_preview.as_ref()?;
    let accent = Colors::accent_primary();
    let (top, height, label) = match &preview.target {
        ChordDropTarget::ChordTrack => return None,
        ChordDropTarget::Track { track_id } => {
            let index = state.tracks.iter().position(|t| &t.id == track_id)?;
            let layout = state.track_row_layout();
            let row = layout.row_for_index(index)?;
            (
                row.y - state.viewport.scroll_y,
                row.height,
                format!("MIDI clip on {}", state.tracks[index].name),
            )
        }
        ChordDropTarget::NewTrack => (
            state.total_track_rows_height() - state.viewport.scroll_y,
            crate::components::timeline::timeline_state::DEFAULT_TRACK_HEIGHT,
            "New MIDI track".to_string(),
        ),
    };
    let x = state.beats_to_x(preview.start_beat as f32);
    let width =
        (state.beats_to_x((preview.start_beat + preview.total_beats()) as f32) - x).max(4.0);
    let inset = 3.0;
    let mut beat = preview.start_beat;
    let names = preview.chords.iter().map(|placement| {
        let cx0 = state.beats_to_x(beat as f32) - x;
        beat += placement.length_beats;
        div()
            .absolute()
            .left(px(cx0 + 6.0))
            .top(px(inset + 16.0))
            .text_size(px(11.0))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(accent)
            .whitespace_nowrap()
            .child(placement.chord.name(placement.flats))
    });
    Some(
        div()
            .absolute()
            .left(px(x))
            .top(px(top.max(0.0) + inset))
            .w(px(width))
            .h(px((height - 2.0 * inset).max(20.0)))
            .rounded(px(radius::clamped(radius::CONTROL_SM, width, height)))
            .bg(Colors::with_alpha(accent, 0.12))
            .border(px(1.0))
            .border_color(Colors::with_alpha(accent, 0.75))
            .overflow_hidden()
            .child(
                div()
                    .absolute()
                    .left(px(6.0))
                    .top(px(inset))
                    .text_size(px(10.0))
                    .text_color(Colors::text_secondary())
                    .whitespace_nowrap()
                    .child(label),
            )
            .children(names)
            .into_any_element(),
    )
}

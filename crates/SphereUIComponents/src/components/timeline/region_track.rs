//! Global Region lane — the arrangement's sections (intro, verse, chorus).
//!
//! A region is a *span*, not a point, so unlike the Marker lane this one draws
//! real blocks: a filled body carrying the section name, plus two edge handles
//! for trimming. The ruler used to paint regions as translucent bars behind the
//! playhead scrub, which meant the only way to resize one was to hit a 4 px
//! strip that also seeks. Here the block owns its own hit area.
//!
//! Drag payloads are shared with the ruler (`TimelineRegionDrag`), so a region
//! dragged from either surface travels the same path and produces one undo
//! entry on release.

use crate::components::timeline::global_lane_header::{
    global_lane_header, global_lane_resize_handle, GlobalLaneHeaderActions, GlobalLaneResizeArmCb,
    GlobalLaneResizeResetCb,
};
use crate::components::timeline::timeline_grid::timeline_grid;
use crate::components::timeline::timeline_ruler::{
    TimelineRegionDrag, TimelineRegionDragMode, TimelineRegionDragUpdate,
};
use crate::components::timeline::timeline_state::{GlobalLaneKind, TimelineState};
use crate::theme::Colors;
use gpui::{
    div, px, AppContext, InteractiveElement, IntoElement, ParentElement,
    StatefulInteractiveElement, Styled,
};

/// Region lane mouse-down: `(beat, region_id, click_count)`.
pub type RegionTrackDownCallback = std::sync::Arc<
    dyn Fn(&(f64, Option<String>, u32), &mut gpui::Window, &mut gpui::App) + 'static,
>;

/// Region lane right-click: `(beat, region_id, screen_x, screen_y)`.
pub type RegionTrackContextCallback = std::sync::Arc<
    dyn Fn(&(f64, Option<String>, f32, f32), &mut gpui::Window, &mut gpui::App) + 'static,
>;

pub type RegionTrackDragCallback =
    std::sync::Arc<dyn Fn(&TimelineRegionDragUpdate, &mut gpui::Window, &mut gpui::App) + 'static>;

pub type GlobalLaneVoidCallback =
    std::sync::Arc<dyn Fn(&(), &mut gpui::Window, &mut gpui::App) + 'static>;
pub type GlobalLaneMenuCallback =
    std::sync::Arc<dyn Fn(&(f32, f32), &mut gpui::Window, &mut gpui::App) + 'static>;

/// Width of each trim handle. Wide enough to hit without aiming, narrow enough
/// that a short region still has a draggable middle.
const REGION_EDGE_W: f32 = 5.0;

/// Vertical inset of the block inside the lane, leaving the resize strip at the
/// lane's bottom edge clear.
const REGION_BLOCK_INSET_Y: f32 = 3.0;

/// Global Region (arranger) lane — named section blocks over the timeline.
#[allow(clippy::too_many_arguments)]
pub fn region_track_lane(
    state: &TimelineState,
    lane_height: f32,
    on_down: Option<RegionTrackDownCallback>,
    on_context: Option<RegionTrackContextCallback>,
    on_drag: Option<RegionTrackDragCallback>,
    on_add: Option<GlobalLaneVoidCallback>,
    on_header_menu: Option<GlobalLaneMenuCallback>,
    on_hide: Option<GlobalLaneVoidCallback>,
    on_toggle_collapsed: Option<GlobalLaneVoidCallback>,
    on_resize_arm: Option<GlobalLaneResizeArmCb>,
    on_resize_reset: Option<GlobalLaneResizeResetCb>,
) -> impl IntoElement {
    let lane_w = state.viewport.viewport_width.max(1.0);
    let selected = state.selected_region_id.clone();
    // The block sits above the lane's own bottom resize strip so the two
    // gestures never compete for the same pixels.
    let block_h = (lane_height
        - 2.0 * REGION_BLOCK_INSET_Y
        - crate::components::timeline::timeline_state::GLOBAL_LANE_RESIZE_HANDLE_HITBOX)
        .max(12.0);

    let blocks: Vec<gpui::AnyElement> = state
        .regions
        .iter()
        .enumerate()
        .filter_map(|(index, region)| {
            let (start, end) = region.normalized_range();
            let x = state.beats_to_x(start as f32);
            let width = (state.beats_to_x(end as f32) - x).max(2.0);
            if x > lane_w + 32.0 || x + width < -32.0 {
                return None;
            }
            let color = crate::color::parse_hex_color(&region.color_hex)
                .unwrap_or_else(|_| Colors::accent_success());
            let is_selected = selected.as_deref() == Some(region.id.as_str());

            let body_drag = TimelineRegionDrag {
                region_id: region.id.clone(),
                mode: TimelineRegionDragMode::Move,
                start_beat: start,
                end_beat: end,
                pointer_offset_x: 0.0,
            };
            let start_drag = TimelineRegionDrag {
                mode: TimelineRegionDragMode::Start,
                ..body_drag.clone()
            };
            let end_drag = TimelineRegionDrag {
                mode: TimelineRegionDragMode::End,
                ..body_drag.clone()
            };

            Some(
                div()
                    .absolute()
                    .left(px(x))
                    .top(px(REGION_BLOCK_INSET_Y))
                    .w(px(width))
                    .h(px(block_h))
                    .id(("region-lane-block", index))
                    .rounded(px(crate::theme::radius::CONTROL_SM))
                    .overflow_hidden()
                    // Selection reads as a solid, confident block; an unselected
                    // section stays a tinted plate so a long arrangement is a
                    // readable colour map rather than a wall of saturation.
                    .bg(Colors::with_alpha(
                        color,
                        if is_selected { 0.34 } else { 0.18 },
                    ))
                    .border(px(1.0))
                    .border_color(Colors::with_alpha(
                        color,
                        if is_selected { 0.95 } else { 0.5 },
                    ))
                    .cursor(gpui::CursorStyle::PointingHand)
                    .on_drag(body_drag, move |drag, offset, _window, cx| {
                        cx.new(|_| TimelineRegionDrag {
                            pointer_offset_x: offset.x.into(),
                            ..drag.clone()
                        })
                    })
                    .child(
                        div()
                            .absolute()
                            .left(px(REGION_EDGE_W + 1.0))
                            .top_0()
                            .bottom_0()
                            .right(px(REGION_EDGE_W + 1.0))
                            .flex()
                            .items_center()
                            .text_size(px(10.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(if is_selected {
                                Colors::text_primary()
                            } else {
                                color
                            })
                            .whitespace_nowrap()
                            .truncate()
                            .child(region.name.clone()),
                    )
                    .child(
                        div()
                            .absolute()
                            .left_0()
                            .top_0()
                            .bottom_0()
                            .w(px(REGION_EDGE_W))
                            .id(("region-lane-start", index))
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
                            .w(px(REGION_EDGE_W))
                            .id(("region-lane-end", index))
                            .cursor(gpui::CursorStyle::ResizeLeftRight)
                            .on_drag(end_drag, |drag, _offset, _window, cx| {
                                cx.new(|_| drag.clone())
                            }),
                    )
                    .into_any_element(),
            )
        })
        .collect();

    // Handlers own only the gesture geometry and the region spans — never a
    // clone of the whole project (see `TimelineGestureContext`).
    let gesture = std::rc::Rc::new(state.gesture_context());
    let region_spans: std::rc::Rc<Vec<(String, f64, f64)>> = std::rc::Rc::new(
        state
            .regions
            .iter()
            .map(|region| {
                let (start, end) = region.normalized_range();
                (region.id.clone(), start, end)
            })
            .collect(),
    );
    // Same rule as `TimelineState::region_at`: the last region drawn wins.
    let region_at = |spans: &[(String, f64, f64)], beat: f64| {
        spans
            .iter()
            .rev()
            .find(|(_, start, end)| beat >= *start && beat <= *end)
            .map(|(id, _, _)| id.clone())
    };
    let interaction = on_down.map(|cb| {
        let state_hit = gesture.clone();
        let spans_hit = region_spans.clone();
        let mut layer = div()
            .absolute()
            .inset_0()
            .id("region-track-hit")
            .on_mouse_down(
                gpui::MouseButton::Left,
                move |event: &gpui::MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    let wx: f32 = event.position.x.into();
                    let lane_x = state_hit.lane_x_from_window_x(wx);
                    let beat = state_hit.x_to_beat(lane_x).max(0.0);
                    let region_id = region_at(&spans_hit, beat);
                    let snapped = state_hit.snap_beats(beat as f32).max(0.0) as f64;
                    cb(&(snapped, region_id, event.click_count as u32), window, cx);
                },
            );
        if let Some(ctx_cb) = on_context {
            let state_ctx = gesture.clone();
            let spans_ctx = region_spans.clone();
            layer = layer.on_mouse_down(
                gpui::MouseButton::Right,
                move |event: &gpui::MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    let wx: f32 = event.position.x.into();
                    let sx: f32 = event.position.x.into();
                    let sy: f32 = event.position.y.into();
                    let lane_x = state_ctx.lane_x_from_window_x(wx);
                    let beat = state_ctx.x_to_beat(lane_x).max(0.0);
                    let region_id = region_at(&spans_ctx, beat);
                    ctx_cb(&(beat, region_id, sx, sy), window, cx);
                },
            );
        }
        layer
    });

    let header = global_lane_header(
        "region",
        "Regions",
        state.region_lane_header_subtitle(),
        state.region_track_collapsed,
        "Hide Region Track",
        GlobalLaneHeaderActions {
            on_add,
            on_menu: on_header_menu,
            on_hide,
            on_toggle_collapsed,
        },
    );

    let resize_handle = on_resize_arm
        .zip(on_resize_reset)
        .map(|(arm, reset)| global_lane_resize_handle(GlobalLaneKind::Arranger, arm, reset));

    // Drag-move is handled here rather than only at the surface because the
    // content div's own bounds give the lane origin directly — the same
    // conversion the ruler does with its markings area.
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
        .children(crate::perf::debug_clip_outline());

    if let Some(drag_cb) = on_drag {
        content = content.on_drag_move::<TimelineRegionDrag>(
            move |event: &gpui::DragMoveEvent<TimelineRegionDrag>, window, cx| {
                let drag = event.drag(cx);
                let x: f32 = event.event.position.x.into();
                let ox: f32 = event.bounds.origin.x.into();
                let local_x = (x - ox).max(0.0);
                let beat_at_x = |x: f32| gesture.snap_beats(gesture.x_to_beats(x)).max(0.0) as f64;
                let (start_beat, end_beat) = match drag.mode {
                    TimelineRegionDragMode::Move => {
                        let length = (drag.end_beat - drag.start_beat).max(1.0e-3);
                        let start = beat_at_x(local_x - drag.pointer_offset_x);
                        (start, start + length)
                    }
                    TimelineRegionDragMode::Start => (beat_at_x(local_x), drag.end_beat),
                    TimelineRegionDragMode::End => (drag.start_beat, beat_at_x(local_x)),
                };
                drag_cb(
                    &TimelineRegionDragUpdate {
                        region_id: drag.region_id.clone(),
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

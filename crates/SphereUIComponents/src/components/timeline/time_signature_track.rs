use crate::components::timeline::global_lane_header::{
    global_lane_header, global_lane_resize_handle, GlobalLaneHeaderActions, GlobalLaneResizeArmCb,
    GlobalLaneResizeResetCb,
};
use crate::components::timeline::marker_flag::{
    flag_hit_index, marker_flag_layer, MarkerFlag, MARKER_FLAG_HIT_SLOP,
};
use crate::components::timeline::timeline_grid::timeline_grid;
use crate::components::timeline::timeline_state::{GlobalLaneKind, TimelineState};
use crate::theme::Colors;
use gpui::prelude::FluentBuilder;
use gpui::{div, px, InteractiveElement, IntoElement, ParentElement, Styled};

pub type TimeSignatureTrackDownCallback = std::sync::Arc<
    dyn Fn(&(f64, Option<String>, bool, u32), &mut gpui::Window, &mut gpui::App) + 'static,
>;

pub type TimeSignatureTrackContextCallback = std::sync::Arc<
    dyn Fn(&(f64, Option<String>, f32, f32), &mut gpui::Window, &mut gpui::App) + 'static,
>;

pub type GlobalLaneVoidCallback =
    std::sync::Arc<dyn Fn(&(), &mut gpui::Window, &mut gpui::App) + 'static>;
pub type GlobalLaneMenuCallback =
    std::sync::Arc<dyn Fn(&(f32, f32), &mut gpui::Window, &mut gpui::App) + 'static>;

/// Global Time Signature lane — compact marker blocks over the project map.
pub fn time_signature_track_lane(
    state: &TimelineState,
    lane_height: f32,
    on_down: Option<TimeSignatureTrackDownCallback>,
    on_context: Option<TimeSignatureTrackContextCallback>,
    on_add: Option<GlobalLaneVoidCallback>,
    on_header_menu: Option<GlobalLaneMenuCallback>,
    on_hide: Option<GlobalLaneVoidCallback>,
    on_toggle_collapsed: Option<GlobalLaneVoidCallback>,
    on_resize_arm: Option<GlobalLaneResizeArmCb>,
    on_resize_reset: Option<GlobalLaneResizeResetCb>,
) -> impl IntoElement {
    let lane_w = state.viewport.viewport_width.max(1.0);
    let points = state.time_signature_map.points.clone();
    let selected = state.selected_time_signature_point_id.clone();

    // Anchored flags, not centred pills: a signature change takes effect *at*
    // its beat, and the old centred pill put half the chip in the bar before it.
    //
    // `hit_ids` is filled in the same pass, so a span and the marker it belongs
    // to stay index-aligned even though off-screen points are culled.
    let mut flags: Vec<MarkerFlag> = Vec::with_capacity(points.len() + 1);
    let mut hit_ids: Vec<String> = Vec::with_capacity(points.len());
    for p in &points {
        let x = state.beats_to_x(p.beat as f32);
        if x < -64.0 || x > lane_w + 64.0 {
            continue;
        }
        flags.push(MarkerFlag {
            x,
            label: p.label(),
            selected: selected.as_deref() == Some(p.id.as_str()),
        });
        hit_ids.push(p.id.clone());
    }
    // Spans cover the real points only — the implicit flag added below has no
    // marker behind it and must not be pickable as one.
    let hit_spans: Vec<(f32, f32)> = flags.iter().map(|f| (f.x, f.width())).collect();
    // `time_signature_at_beat` already resolves an implicit 4/4 for an empty
    // map; surface that rather than leaving the lane blank.
    if points.is_empty() {
        let implicit = state.time_signature_map.time_signature_at_beat(0.0);
        flags.push(MarkerFlag {
            x: state.beats_to_x(0.0),
            label: implicit.label(),
            selected: false,
        });
    }
    // A transparent pad over each real flag, so the pointer says "grabbable"
    // before the click. It carries no listener: the lane's single hit layer
    // still resolves every press, and a second one here could disagree with it.
    let hover_pads: Vec<gpui::Div> = hit_spans
        .iter()
        .map(|(x, width)| {
            div()
                .absolute()
                .left(px(*x - MARKER_FLAG_HIT_SLOP))
                .top(px(0.0))
                .bottom_0()
                .w(px(width + MARKER_FLAG_HIT_SLOP))
                .cursor(gpui::CursorStyle::PointingHand)
        })
        .collect();
    let (flag_layer, flag_labels) = marker_flag_layer(flags, lane_w, lane_height);

    let subtitle = state.time_signature_lane_header_subtitle();

    // The hit test resolves against the drawn flag bodies rather than a beat
    // tolerance. Snapping the pointer beat *before* looking for a marker was
    // the bug: at high zoom the snap step is wider than the tolerance, so a
    // marker off the grid could not be picked at all.
    // Handlers own only the gesture geometry — never a clone of the whole
    // project (see `TimelineGestureContext`).
    let gesture = std::rc::Rc::new(state.gesture_context());
    let interaction = on_down.map(|cb| {
        let state_hit = gesture.clone();
        let spans_hit = hit_spans.clone();
        let ids_hit = hit_ids.clone();
        div()
            .absolute()
            .inset_0()
            .id("time-signature-track-hit")
            .on_mouse_down(
                gpui::MouseButton::Left,
                move |event: &gpui::MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    let wx: f32 = event.position.x.into();
                    let lane_x = state_hit.lane_x_from_window_x(wx);
                    let beat = state_hit.x_to_beat(lane_x).max(0.0);
                    let snapped = state_hit.snap_beats(beat as f32) as f64;
                    let point_id = flag_hit_index(&spans_hit, lane_x, MARKER_FLAG_HIT_SLOP)
                        .and_then(|index| ids_hit.get(index).cloned());
                    cb(
                        &(snapped, point_id, false, event.click_count as u32),
                        window,
                        cx,
                    );
                },
            )
            .when_some(on_context, |layer, ctx_cb| {
                let state_ctx = gesture.clone();
                let spans_ctx = hit_spans.clone();
                let ids_ctx = hit_ids.clone();
                layer.on_mouse_down(
                    gpui::MouseButton::Right,
                    move |event: &gpui::MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        let wx: f32 = event.position.x.into();
                        let sx: f32 = event.position.x.into();
                        let sy: f32 = event.position.y.into();
                        let lane_x = state_ctx.lane_x_from_window_x(wx);
                        let beat = state_ctx.x_to_beat(lane_x).max(0.0);
                        let point_id = flag_hit_index(&spans_ctx, lane_x, MARKER_FLAG_HIT_SLOP)
                            .and_then(|index| ids_ctx.get(index).cloned());
                        ctx_cb(&(beat, point_id, sx, sy), window, cx);
                    },
                )
            })
    });

    let header = global_lane_header(
        "time-signature",
        "Time Signature",
        subtitle,
        state.time_signature_track_collapsed,
        "Hide Time Signature Track",
        GlobalLaneHeaderActions {
            on_add,
            on_menu: on_header_menu,
            on_hide,
            on_toggle_collapsed,
        },
    );

    let resize_handle = on_resize_arm
        .zip(on_resize_reset)
        .map(|(arm, reset)| global_lane_resize_handle(GlobalLaneKind::TimeSignature, arm, reset));

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
        .child(
            div()
                .flex_1()
                .h_full()
                .relative()
                .overflow_hidden()
                .bg(Colors::timeline_content_background())
                .child(timeline_grid(state, lane_w, lane_height))
                .child(flag_layer)
                .children(flag_labels)
                .children(interaction)
                .children(hover_pads)
                // Debug: outline time_signature_lane_content_rect (FUTUREBOARD_UI_DEBUG_CLIPS=1).
                .children(crate::perf::debug_clip_outline()),
        )
        .children(resize_handle)
}

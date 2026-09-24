//! Global Marker lane — the arrangement's cue points, given a row of their own.
//!
//! Markers used to live only as 9 px chips crammed into the ruler, sharing every
//! pixel with the playhead scrub: there was nowhere to read a long name, no grab
//! target that was not also a seek, and no place to hang per-marker commands. A
//! marker is a *named point in the arrangement*, so it gets the same treatment
//! as the other conductor data — its own lane, its own header, its own hit area.
//!
//! Geometry is shared with the Tempo and Time Signature lanes: the same
//! `marker_flag_layer` shape (left edge on the beat, tapering right) so a
//! marker, a tempo change, and a meter change all read as "starts here".

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
use gpui::{div, px, InteractiveElement, IntoElement, ParentElement, Styled};

/// One mouse-down on the Marker lane.
///
/// Both beats are here on purpose. `snapped_beat` is where a *new* marker
/// would be created, and `pointer_beat` is the raw grab point a move measures
/// its offset from — resolving a move against the snapped beat would jump the
/// flag by up to half a grid step the instant it was grabbed.
#[derive(Debug, Clone, PartialEq)]
pub struct MarkerLaneDown {
    pub snapped_beat: f64,
    pub pointer_beat: f64,
    /// Lane-local pointer x, the anchor the drag threshold is measured from.
    pub lane_x: f32,
    /// `None` when the press landed on empty lane, which is what separates
    /// "select this marker" from "seek / create here".
    pub marker_id: Option<String>,
    pub click_count: u32,
}

/// Marker lane mouse-down.
pub type MarkerTrackDownCallback =
    std::sync::Arc<dyn Fn(&MarkerLaneDown, &mut gpui::Window, &mut gpui::App) + 'static>;

/// Marker lane right-click: `(beat, marker_id, screen_x, screen_y)`.
pub type MarkerTrackContextCallback = std::sync::Arc<
    dyn Fn(&(f64, Option<String>, f32, f32), &mut gpui::Window, &mut gpui::App) + 'static,
>;

pub type GlobalLaneVoidCallback =
    std::sync::Arc<dyn Fn(&(), &mut gpui::Window, &mut gpui::App) + 'static>;
pub type GlobalLaneMenuCallback =
    std::sync::Arc<dyn Fn(&(f32, f32), &mut gpui::Window, &mut gpui::App) + 'static>;

/// Global Marker lane — named cue points over the arrangement timeline.
#[allow(clippy::too_many_arguments)]
pub fn marker_track_lane(
    state: &TimelineState,
    lane_height: f32,
    on_down: Option<MarkerTrackDownCallback>,
    on_context: Option<MarkerTrackContextCallback>,
    on_add: Option<GlobalLaneVoidCallback>,
    on_header_menu: Option<GlobalLaneMenuCallback>,
    on_hide: Option<GlobalLaneVoidCallback>,
    on_toggle_collapsed: Option<GlobalLaneVoidCallback>,
    on_resize_arm: Option<GlobalLaneResizeArmCb>,
    on_resize_reset: Option<GlobalLaneResizeResetCb>,
) -> impl IntoElement {
    let lane_w = state.viewport.viewport_width.max(1.0);
    let selected = state.selected_marker_id.clone();

    let flags: Vec<MarkerFlag> = state
        .markers
        .iter()
        .filter_map(|marker| {
            let x = state.beats_to_x(marker.beat as f32);
            // Overscan by a flag's worth so a marker scrolling in from either
            // edge does not pop.
            if x < -160.0 || x > lane_w + 160.0 {
                return None;
            }
            Some(MarkerFlag {
                x,
                label: marker.name.clone(),
                selected: selected.as_deref() == Some(marker.id.as_str()),
            })
        })
        .collect();
    // The hit test runs against the *drawn* bodies, so build the spans from the
    // same list that paints them — an id and its shape can then never disagree.
    let hit_ids: Vec<String> = state
        .markers
        .iter()
        .filter(|marker| {
            let x = state.beats_to_x(marker.beat as f32);
            x >= -160.0 && x <= lane_w + 160.0
        })
        .map(|marker| marker.id.clone())
        .collect();
    let hit_spans: Vec<(f32, f32)> = flags.iter().map(|f| (f.x, f.width())).collect();
    let (flag_layer, flag_labels) = marker_flag_layer(flags, lane_w, lane_height);

    // A transparent pad over each flag so the pointer says "grabbable" before
    // the press. It carries the cursor and nothing else: the move itself is a
    // gesture session owned by the timeline root, armed by the lane's single
    // hit layer below, so there is no second hit test here that could disagree
    // with it about which marker was meant.
    let hover_pads: Vec<gpui::Div> = hit_spans
        .iter()
        .map(|(x, width)| {
            div()
                .absolute()
                .left(px(*x - MARKER_FLAG_HIT_SLOP))
                .top(px(0.0))
                .h(px(lane_height))
                .w(px(width + MARKER_FLAG_HIT_SLOP))
                .cursor(gpui::CursorStyle::PointingHand)
        })
        .collect();

    // One hit layer under the flags resolves both cases: it maps the pointer to
    // a beat, then asks the state whether a marker is close enough. Keeping the
    // hit test in one place is why a click on a flag and a click 3 px beside it
    // cannot disagree about which marker was meant.
    // Handlers own only the gesture geometry — never a clone of the whole
    // project (see `TimelineGestureContext`).
    let gesture = std::rc::Rc::new(state.gesture_context());
    let interaction = on_down.map(|cb| {
        let state_hit = gesture.clone();
        let ids_hit = hit_ids.clone();
        let spans_hit = hit_spans.clone();
        let mut layer = div()
            .absolute()
            .inset_0()
            .id("marker-track-hit")
            .on_mouse_down(
                gpui::MouseButton::Left,
                move |event: &gpui::MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    let wx: f32 = event.position.x.into();
                    let lane_x = state_hit.lane_x_from_window_x(wx);
                    let beat = state_hit.x_to_beat(lane_x).max(0.0);
                    let marker_id = flag_hit_index(&spans_hit, lane_x, MARKER_FLAG_HIT_SLOP)
                        .and_then(|index| ids_hit.get(index).cloned());
                    // Creating snaps; only the hit test and the grab offset
                    // read the raw pointer, so a marker just left of a grid
                    // line is still grabbable and does not jump when grabbed.
                    let snapped = state_hit.snap_beats(beat as f32).max(0.0) as f64;
                    cb(
                        &MarkerLaneDown {
                            snapped_beat: snapped,
                            pointer_beat: beat,
                            lane_x,
                            marker_id,
                            click_count: event.click_count as u32,
                        },
                        window,
                        cx,
                    );
                },
            );
        if let Some(ctx_cb) = on_context {
            let state_ctx = gesture.clone();
            let ids_ctx = hit_ids.clone();
            let spans_ctx = hit_spans.clone();
            layer = layer.on_mouse_down(
                gpui::MouseButton::Right,
                move |event: &gpui::MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    let wx: f32 = event.position.x.into();
                    let sx: f32 = event.position.x.into();
                    let sy: f32 = event.position.y.into();
                    let lane_x = state_ctx.lane_x_from_window_x(wx);
                    let beat = state_ctx.x_to_beat(lane_x).max(0.0);
                    let marker_id = flag_hit_index(&spans_ctx, lane_x, MARKER_FLAG_HIT_SLOP)
                        .and_then(|index| ids_ctx.get(index).cloned());
                    ctx_cb(&(beat, marker_id, sx, sy), window, cx);
                },
            );
        }
        layer
    });

    let header = global_lane_header(
        "marker",
        "Markers",
        state.marker_lane_header_subtitle(),
        state.marker_track_collapsed,
        "Hide Marker Track",
        GlobalLaneHeaderActions {
            on_add,
            on_menu: on_header_menu,
            on_hide,
            on_toggle_collapsed,
        },
    );

    let resize_handle = on_resize_arm
        .zip(on_resize_reset)
        .map(|(arm, reset)| global_lane_resize_handle(GlobalLaneKind::Marker, arm, reset));

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
                .children(interaction)
                .child(flag_layer)
                .children(flag_labels)
                .children(hover_pads)
                .children(crate::perf::debug_clip_outline()),
        )
        .children(resize_handle)
}

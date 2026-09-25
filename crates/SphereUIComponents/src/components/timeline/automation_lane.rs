use crate::components::timeline::timeline_state::{
    automation_value_to_y, automation_y_to_value, evaluate_automation, AutomationHover,
    AutomationLaneState, AutomationMarquee, AutomationTarget, TimelineGestureContext,
    TimelineState, HEADER_WIDTH,
};
use crate::theme::Colors;
use gpui::{
    canvas, div, fill, point, px, size, AnyView, App, AppContext, Background, Bounds, Context,
    InteractiveElement, IntoElement, ParentElement, PathBuilder, PathStyle, Pixels, Point, Render,
    StatefulInteractiveElement, StrokeOptions, Styled, Window,
};

/// Tiny tooltip surface for sub-lane control buttons. Matches the global lane
/// header tooltip styling so hover hints read consistently across the timeline.
struct LaneTooltipText(&'static str);

impl Render for LaneTooltipText {
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

fn lane_tooltip(text: &'static str) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    move |_window, cx| cx.new(|_| LaneTooltipText(text)).into()
}

/// Left inset for automation sub-lane header content. Keeps lane titles visually
/// nested under the parent track without shifting the timeline grid.
const AUTOMATION_SUBLANE_HEADER_INDENT: f32 = 28.0;

/// X-position of the vertical child-lane guide inside the header column.
const AUTOMATION_SUBLANE_RAIL_X: f32 = 18.0;

/// Action fired from a sub-lane header control. One callback handles them all so
/// new lane controls land without re-threading the whole call stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutomationLaneAction {
    /// Focus the editor on this lane (header body click).
    Activate,
    /// Toggle the lane's automation mode between Read and Off (enabled flag).
    ToggleEnable,
    /// Remove every point but keep the lane.
    Clear,
    /// Remove the sub-lane (lane and its points).
    Hide,
    /// Open the parameter picker for this track: the lane's name is a
    /// selector, so choosing another parameter shows (or adds) its lane.
    PickTarget,
}

/// Sub-lane mouse-down payload:
/// `(track_id, lane_id, beat, value_norm, additive, alt, click_count)`.
/// `alt` enables the curve-tension edit; `click_count` distinguishes a double
/// click (Alt+double-click resets a segment to linear).
pub type AutomationDownCallback = std::sync::Arc<
    dyn Fn(&(String, String, f32, f32, bool, bool, u32), &mut gpui::Window, &mut gpui::App)
        + 'static,
>;

/// Sub-lane right-click payload: `(track_id, lane_id, beat, value_norm)`.
///
/// Right-click deletes the point under the cursor. It is a separate callback
/// from [`AutomationDownCallback`] because it is a different gesture, not a
/// modifier on the same one: nothing about it selects, drags, or adds.
pub type AutomationDeleteCallback = std::sync::Arc<
    dyn Fn(&(String, String, f32, f32), &mut gpui::Window, &mut gpui::App) -> bool + 'static,
>;

/// Sub-lane header action payload: `(track_id, lane_id, action, window_x,
/// window_y)`. The position anchors the parameter picker for
/// [`AutomationLaneAction::PickTarget`]; the other actions ignore it.
pub type AutomationLaneActionCallback = std::sync::Arc<
    dyn Fn(&(String, String, AutomationLaneAction, f32, f32), &mut gpui::Window, &mut gpui::App)
        + 'static,
>;

/// Sub-lane hover payload: `(track_id, lane_id, beat, value_norm)`. Fired on
/// mouse-move over a lane so the editor can resolve the hovered point/segment;
/// `beat` is snapped exactly like the mouse-down path so hover and click agree.
pub type AutomationHoverCallback = std::sync::Arc<
    dyn Fn(&(String, String, f32, f32), &mut gpui::Window, &mut gpui::App) + 'static,
>;

/// Human category shown under the lane name in the sub-lane header.
fn target_category(target: &AutomationTarget) -> &'static str {
    match target {
        AutomationTarget::TrackVolume
        | AutomationTarget::TrackPan
        | AutomationTarget::TrackMute => "Track",
        AutomationTarget::PluginParameter { .. } => "Plugin Parameter",
        AutomationTarget::SendLevel { .. } => "Send",
    }
}

/// One expanded automation sub-lane rendered directly below its parent track.
///
/// The left header is a compact lane strip: the parameter name is a selector
/// (opens the target picker), a Read/Off pill is the lane's automation mode, a
/// live readout says what the curve is worth (the hovered point, else the
/// value at the playhead, in the parameter's own unit), and the lane's own
/// controls sit at the right edge. The right area draws the envelope — a
/// filled body under the curve, the curve, the base value as a dashed guide,
/// the points, and a value tag on the hovered point — and captures point edits
/// scoped to this lane's own row bounds.
#[allow(clippy::too_many_arguments)]
pub fn automation_lane(
    track_id: &str,
    lane: &AutomationLaneState,
    track_color: gpui::Rgba,
    is_active: bool,
    lane_y_abs: f32,
    lane_height: f32,
    state: &TimelineState,
    gesture: &std::rc::Rc<TimelineGestureContext>,
    on_automation_down: Option<AutomationDownCallback>,
    on_lane_action: Option<AutomationLaneActionCallback>,
    on_automation_hover: Option<AutomationHoverCallback>,
    on_automation_delete: Option<AutomationDeleteCallback>,
    marquee: Option<&AutomationMarquee>,
    hover: Option<&AutomationHover>,
) -> impl IntoElement {
    let track_id = track_id.to_string();
    let lane_id = lane.id.clone();
    // Hover that targets THIS lane (drives the segment highlight + cursor).
    let lane_hover = hover
        .filter(|h| h.matches_lane(&track_id, &lane_id))
        .cloned();
    let id_num = {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        track_id.hash(&mut hasher);
        lane.id.hash(&mut hasher);
        hasher.finish() as usize
    };

    let category = target_category(&lane.target);
    // Source/category text stays in the muted ramp — never bright accent — so
    // the only saturated element in the lane is the envelope curve itself.
    let category_color = if is_active {
        Colors::text_secondary()
    } else {
        Colors::with_alpha(Colors::text_muted(), 0.62)
    };
    let mut accent = track_color;
    accent.a = if is_active { 0.92 } else { 0.55 };

    // ── Left header (indented child lane — timeline grid stays flush right) ───
    let activate_action = on_lane_action.clone();
    let mut header = div()
        .relative()
        .w(px(HEADER_WIDTH))
        .h_full()
        .flex_none()
        .border_r(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(if is_active {
            Colors::automation_lane_bg_selected()
        } else {
            Colors::automation_lane_header_bg()
        })
        .id(("automation-lane-header", id_num))
        .cursor(gpui::CursorStyle::PointingHand);
    if let Some(cb) = activate_action {
        let tid = track_id.clone();
        let lid = lane_id.clone();
        header = header.on_mouse_down(
            gpui::MouseButton::Left,
            move |event: &gpui::MouseDownEvent, window, cx| {
                cx.stop_propagation();
                let x: f32 = event.position.x.into();
                let y: f32 = event.position.y.into();
                cb(
                    &(
                        tid.clone(),
                        lid.clone(),
                        AutomationLaneAction::Activate,
                        x,
                        y,
                    ),
                    window,
                    cx,
                );
            },
        );
    }

    // Nesting gutter — slightly darker band in the indent column so child lanes
    // read as belonging to the parent, not as peer tracks.
    header = header.child(
        div()
            .absolute()
            .left_0()
            .top_0()
            .bottom_0()
            .w(px(AUTOMATION_SUBLANE_HEADER_INDENT))
            .bg(Colors::with_alpha(Colors::surface_muted(), 0.5)),
    );

    // Vertical child-lane guide shared by every automation sub-row. Active lanes
    // light the rail with the automation accent; idle lanes stay quiet graphite.
    header = header.child(
        div()
            .absolute()
            .left(px(AUTOMATION_SUBLANE_RAIL_X))
            .top(px(9.0))
            .bottom(px(9.0))
            .w(px(1.0))
            .bg(if is_active {
                Colors::automation_rail_active()
            } else {
                Colors::automation_rail()
            }),
    );

    // ── Row 1: parameter selector + live value readout ───────────────────────
    //
    // The name is a selector, not a label: clicking it opens the track's
    // parameter picker, exactly like choosing what a lane shows on a console
    // automation strip. The chevron is the one hint that it opens something.
    let pick_target = on_lane_action.clone();
    let mut name_button = div()
        .id(("automation-lane-target", id_num))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(5.0))
        .min_w(px(0.0))
        .h(px(20.0))
        .pl(px(5.0))
        .pr(px(5.0))
        .rounded(px(crate::theme::radius::CONTROL))
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(|s| s.bg(Colors::button_bg_hover()))
        .tooltip(lane_tooltip("Choose parameter"))
        .child(
            div()
                .flex_none()
                .w(px(2.0))
                .h(px(10.0))
                .rounded(px(crate::theme::radius::PILL))
                .bg(accent),
        )
        .child(
            // Parameter name must stay on a single line — `truncate` applies
            // nowrap + ellipsis so "Volume" can never wrap to "Volu / me".
            div()
                .min_w(px(0.0))
                .text_size(px(11.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(if lane.enabled {
                    Colors::text_primary()
                } else {
                    Colors::text_muted()
                })
                .truncate()
                .child(lane.name.clone()),
        )
        .child(
            div()
                .flex_none()
                .text_size(px(8.0))
                .text_color(Colors::text_muted())
                .child("▾"),
        );
    if let Some(cb) = pick_target {
        let tid = track_id.clone();
        let lid = lane_id.clone();
        name_button = name_button.on_mouse_down(
            gpui::MouseButton::Left,
            move |event: &gpui::MouseDownEvent, window, cx| {
                cx.stop_propagation();
                let x: f32 = event.position.x.into();
                let y: f32 = event.position.y.into();
                cb(
                    &(
                        tid.clone(),
                        lid.clone(),
                        AutomationLaneAction::PickTarget,
                        x,
                        y,
                    ),
                    window,
                    cx,
                );
            },
        );
    }

    // Readout: the hovered point's value, else what the lane plays at the
    // playhead — the header always says what the curve is worth right now, in
    // the parameter's own unit. Hover reading takes the curve hue so it is
    // recognisably "the point under the cursor", not the transport value.
    let hovered_point_value = lane_hover
        .as_ref()
        .and_then(|h| h.point_id)
        .and_then(|id| lane.points.iter().find(|p| p.id == id))
        .map(|p| p.value);
    let readout_value = hovered_point_value.unwrap_or_else(|| {
        evaluate_automation(
            &lane.points,
            state.transport.playhead_beats as f64,
            lane.target.default_value(),
        )
    });
    let readout = div()
        .flex_none()
        .px(px(6.0))
        .h(px(18.0))
        .flex()
        .items_center()
        .rounded(px(crate::theme::radius::CONTROL))
        .bg(Colors::with_alpha(Colors::surface_base(), 0.55))
        .text_size(px(10.0))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(if hovered_point_value.is_some() {
            Colors::automation_curve()
        } else if lane.enabled {
            Colors::text_secondary()
        } else {
            Colors::text_muted()
        })
        .child(lane.target.format_value(readout_value));

    let name_row = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(6.0))
        .min_w(px(0.0))
        .child(div().flex_1().min_w(px(0.0)).child(name_button))
        .child(readout);

    // ── Row 2: automation mode + category, lane controls at the right edge ──
    //
    // Two honest states: Read plays the lane back, Off bypasses it. Fill and
    // border both carry the state (latched language), never hue alone.
    let mode_action = on_lane_action.clone();
    let (mode_label, mode_tip, mode_fill, mode_border, mode_text) = if lane.enabled {
        let (fill, border) = Colors::latched(
            Colors::automation_lane_header_bg(),
            Colors::state_automation(),
        );
        (
            "READ",
            "Automation mode: Read — the lane drives the parameter. Click for Off.",
            fill,
            border,
            Colors::state_automation(),
        )
    } else {
        (
            "OFF",
            "Automation mode: Off — the lane is bypassed. Click for Read.",
            Colors::button_bg(),
            Colors::button_border(),
            Colors::button_text_muted(),
        )
    };
    let mut mode_pill = div()
        .id(("automation-lane-mode", id_num))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .h(px(16.0))
        .min_w(px(36.0))
        .px(px(6.0))
        .rounded(px(crate::theme::radius::CONTROL))
        .bg(mode_fill)
        .border(px(1.0))
        .border_color(mode_border)
        .text_size(px(8.0))
        .font_weight(gpui::FontWeight::BOLD)
        .text_color(mode_text)
        .cursor(gpui::CursorStyle::PointingHand)
        .tooltip(lane_tooltip(mode_tip))
        .child(mode_label);
    if let Some(cb) = mode_action {
        let tid = track_id.clone();
        let lid = lane_id.clone();
        mode_pill = mode_pill.on_mouse_down(
            gpui::MouseButton::Left,
            move |event: &gpui::MouseDownEvent, window, cx| {
                cx.stop_propagation();
                let x: f32 = event.position.x.into();
                let y: f32 = event.position.y.into();
                cb(
                    &(
                        tid.clone(),
                        lid.clone(),
                        AutomationLaneAction::ToggleEnable,
                        x,
                        y,
                    ),
                    window,
                    cx,
                );
            },
        );
    }

    let category_label = div()
        .min_w(px(0.0))
        .text_size(px(8.0))
        .text_color(category_color)
        .truncate()
        .child(category);

    // Lane controls stay flush to the right edge of the header column. Clear
    // is the destructive action here (removes every point), so it is the one
    // that reads danger on hover; Remove drops the lane itself.
    let control_buttons = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(4.0))
        .child(lane_button(
            ("automation-lane-clear", id_num).into(),
            "C",
            "Clear automation points",
            LaneButtonStyle::Danger,
            track_id.clone(),
            lane_id.clone(),
            AutomationLaneAction::Clear,
            on_lane_action.clone(),
        ))
        .child(lane_button(
            ("automation-lane-remove", id_num).into(),
            "×",
            "Remove lane",
            LaneButtonStyle::Neutral,
            track_id.clone(),
            lane_id.clone(),
            AutomationLaneAction::Hide,
            on_lane_action.clone(),
        ));

    let mode_row = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(6.0))
        .min_w(px(0.0))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .min_w(px(0.0))
                .pl(px(5.0))
                .child(mode_pill)
                .child(category_label),
        )
        .child(div().flex_none().child(control_buttons));

    let header = header.child(
        div()
            .flex()
            .flex_col()
            .justify_center()
            .gap(px(3.0))
            .w_full()
            .h_full()
            .pl(px(AUTOMATION_SUBLANE_HEADER_INDENT))
            .pr(px(8.0))
            .child(name_row)
            .child(mode_row),
    );

    // ── Right envelope + interaction area ────────────────────────────────────
    let envelope = lane_envelope(
        lane,
        state,
        lane_height,
        is_active,
        marquee,
        lane_hover.as_ref(),
    );

    // Cursor reflects what the hovered region edits: a point handle (pointer) or a
    // curve segment (vertical-resize = drag to shape tension). Built-in OS cursors
    // — no custom-PNG hotspot, so they stay correct at 125% / 150% / 200% DPI.
    // Empty lane is left untouched (keeps the active tool's cursor).
    let hover_cursor: Option<gpui::CursorStyle> = match lane_hover.as_ref() {
        Some(h) if h.point_id.is_some() => Some(gpui::CursorStyle::PointingHand),
        Some(h) if h.segment_left_id.is_some() => Some(gpui::CursorStyle::ResizeUpDown),
        _ => None,
    };

    let interaction = on_automation_down.clone().map(|cb| {
        // Per-frame geometry snapshot, not a full project clone — see
        // [`TimelineGestureContext`].
        let state_for = std::rc::Rc::clone(gesture);
        let tid = track_id.clone();
        let lid = lane_id.clone();
        let mut hit = div()
            .absolute()
            .inset_0()
            .id(("automation-lane-hit", id_num))
            .on_mouse_down(
                gpui::MouseButton::Left,
                move |event: &gpui::MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    let wx: f32 = event.position.x.into();
                    let wy: f32 = event.position.y.into();
                    let lane_x = state_for.lane_x_from_window_x(wx);
                    let raw_beat = state_for.x_to_beats(lane_x);
                    let snapped_sec = state_for.snap_time(raw_beat * state_for.seconds_per_beat());
                    let beat = (snapped_sec / state_for.seconds_per_beat()).max(0.0);
                    let content_y = state_for.content_y_from_window_y(wy);
                    let local_y = content_y - lane_y_abs;
                    let value = automation_y_to_value(local_y, lane_height);
                    let additive = event.modifiers.shift || event.modifiers.control;
                    let alt = event.modifiers.alt;
                    let click_count = event.click_count.max(1) as u32;
                    cb(
                        &(
                            tid.clone(),
                            lid.clone(),
                            beat,
                            value,
                            additive,
                            alt,
                            click_count,
                        ),
                        window,
                        cx,
                    );
                },
            );

        // Right-click deletes the point under the cursor.
        //
        // Only when there is one: on empty lane space the press is left alone
        // so the arrangement's own context menu still opens behind it. That is
        // why the callback answers whether it deleted anything — the decision
        // to swallow the event cannot be made here, where the points are not
        // known.
        if let Some(delete_cb) = on_automation_delete.clone() {
            let state_for = std::rc::Rc::clone(gesture);
            let tid = track_id.clone();
            let lid = lane_id.clone();
            hit = hit.on_mouse_down(
                gpui::MouseButton::Right,
                move |event: &gpui::MouseDownEvent, window, cx| {
                    let wx: f32 = event.position.x.into();
                    let wy: f32 = event.position.y.into();
                    let lane_x = state_for.lane_x_from_window_x(wx);
                    // Unsnapped: the click has to resolve to the point actually
                    // under the cursor, and snapping would move the probe onto
                    // a grid line the point is not on.
                    let beat = state_for.x_to_beats(lane_x).max(0.0);
                    let content_y = state_for.content_y_from_window_y(wy);
                    let local_y = content_y - lane_y_abs;
                    let value = automation_y_to_value(local_y, lane_height);
                    if delete_cb(&(tid.clone(), lid.clone(), beat, value), window, cx) {
                        cx.stop_propagation();
                    }
                },
            );
        }

        if let Some(cursor) = hover_cursor {
            hit = hit.cursor(cursor);
        }

        // Hover tracking: resolve the point/segment under the cursor on move, and
        // clear it when the cursor leaves the lane. Same snapped beat as the click
        // path so the hovered target matches what a click would grab.
        if let Some(hover_cb) = on_automation_hover.clone() {
            let state_for = std::rc::Rc::clone(gesture);
            let tid = track_id.clone();
            let lid = lane_id.clone();
            hit = hit.on_mouse_move(move |event: &gpui::MouseMoveEvent, window, cx| {
                // Only resolve hover when not dragging — a pressed-button move is a
                // gesture and is handled by the global timeline move handler.
                if event.pressed_button.is_some() {
                    return;
                }
                let wx: f32 = event.position.x.into();
                let wy: f32 = event.position.y.into();
                let lane_x = state_for.lane_x_from_window_x(wx);
                let raw_beat = state_for.x_to_beats(lane_x);
                let snapped_sec = state_for.snap_time(raw_beat * state_for.seconds_per_beat());
                let beat = (snapped_sec / state_for.seconds_per_beat()).max(0.0);
                let content_y = state_for.content_y_from_window_y(wy);
                let local_y = content_y - lane_y_abs;
                let value = automation_y_to_value(local_y, lane_height);
                hover_cb(&(tid.clone(), lid.clone(), beat, value), window, cx);
            });
        }
        if let Some(hover_cb) = on_automation_hover.clone() {
            let tid = track_id.clone();
            let lid = lane_id.clone();
            // Hover-out: an out-of-range beat/value signals "clear" to the handler
            // (it resolves no point/segment there and drops the highlight).
            hit = hit.on_hover(move |hovered, window, cx| {
                if !*hovered {
                    hover_cb(&(tid.clone(), lid.clone(), -1.0, -1.0), window, cx);
                }
            });
        }
        hit
    });

    // Right-side lane body: a TRANSLUCENT overlay so the timeline grid behind
    // the rows stays visible. The lane reads as a sublane overlay on the
    // arrangement canvas, never an opaque dark block. The selected lane only
    // gets a whisper of purple — the rail/curve/label carry the selection.
    let lane_area = div()
        .flex_1()
        .h_full()
        .relative()
        .overflow_hidden()
        .bg(if is_active {
            Colors::automation_canvas_bg_selected()
        } else {
            Colors::automation_canvas_bg()
        })
        .child(envelope)
        .children(interaction);

    div()
        .flex()
        .flex_row()
        .w_full()
        .h(px(lane_height))
        // No row-level fill — the header paints the left label, the lane_area
        // paints a translucent right body. Only a subtle separator hairline.
        .border_b(px(1.0))
        .border_color(Colors::with_alpha(Colors::automation_separator(), 0.7))
        .child(header)
        .child(lane_area)
}

/// Visual weight for a sub-lane control: the destructive action stays neutral
/// until hovered; everything else is a quiet neutral button.
#[derive(Clone, Copy)]
enum LaneButtonStyle {
    /// Always neutral with a quiet hover (Remove).
    Neutral,
    /// Neutral by default, danger/red only on hover (Clear).
    Danger,
}

/// Small square control button used in the sub-lane header.
#[allow(clippy::too_many_arguments)]
fn lane_button(
    id: gpui::ElementId,
    label: &'static str,
    tooltip: &'static str,
    style: LaneButtonStyle,
    track_id: String,
    lane_id: String,
    action: AutomationLaneAction,
    cb: Option<AutomationLaneActionCallback>,
) -> impl IntoElement {
    let mut btn = div()
        .flex()
        .items_center()
        .justify_center()
        .w(px(18.0))
        .h(px(18.0))
        .rounded(px(crate::theme::radius::CONTROL))
        .text_size(px(9.0))
        .font_weight(gpui::FontWeight::BOLD)
        .id(id)
        .cursor(gpui::CursorStyle::PointingHand)
        .tooltip(lane_tooltip(tooltip));
    match style {
        LaneButtonStyle::Danger => {
            btn = btn
                .bg(Colors::button_bg())
                .text_color(Colors::button_text_muted())
                .hover(|s| {
                    s.bg(Colors::with_alpha(Colors::status_error(), 0.18))
                        .text_color(Colors::status_error())
                });
        }
        LaneButtonStyle::Neutral => {
            btn = btn
                .bg(Colors::button_bg())
                .text_color(Colors::button_text_muted())
                .hover(|s| {
                    s.bg(Colors::button_bg_hover())
                        .text_color(Colors::button_text())
                });
        }
    }
    if let Some(cb) = cb {
        btn = btn.on_mouse_down(
            gpui::MouseButton::Left,
            move |event: &gpui::MouseDownEvent, window, cx| {
                cx.stop_propagation();
                let x: f32 = event.position.x.into();
                let y: f32 = event.position.y.into();
                cb(
                    &(track_id.clone(), lane_id.clone(), action, x, y),
                    window,
                    cx,
                );
            },
        );
    }
    btn.child(label)
}

/// Logical stroke widths for the automation envelope. Kept comfortably above
/// 1px so HiDPI scaling can never thin the line into subpixel shimmer —
/// `paint_path` applies the device scale exactly once, so these stay visually
/// stable at 125% / 150% / 175% / 200%.
const AUTOMATION_LINE_WIDTH: f32 = 1.6;
const AUTOMATION_LINE_WIDTH_HOVER: f32 = 2.2;
const AUTOMATION_LINE_WIDTH_ACTIVE: f32 = 2.6;

/// Paint a polyline as a single anti-aliased stroked path.
///
/// `pts` are lane-local (x/y in lane pixels) and `origin` is the canvas'
/// window-space top-left, so the path lands in the right place. Coordinates stay
/// in floating point on purpose: GPUI tessellates + anti-aliases the stroke and
/// `paint_path` applies the DPI scale once, so diagonals and curves come out
/// smooth at any zoom / HiDPI scale instead of the old pixel-stepped quads. The
/// whole curve is one continuous path (not per-segment quads), so there are no
/// gaps or unpainted pixels at segment boundaries.
fn paint_automation_stroke(
    window: &mut Window,
    origin: Point<Pixels>,
    pts: &[(f32, f32)],
    width: f32,
    color: impl Into<Background>,
) {
    if pts.len() < 2 {
        return;
    }
    // Tight miter limit: a continuous single-path stroke that bevels (rather than
    // spiking) at sharp peaks. No `lyon::LineJoin` import is needed — the default
    // join already produces gap-free joins; the limit just tames sharp corners.
    let options = StrokeOptions::default()
        .with_line_width(width)
        .with_miter_limit(2.0);
    let mut builder = PathBuilder::stroke(px(width)).with_style(PathStyle::Stroke(options));
    let (x0, y0) = pts[0];
    builder.move_to(origin + point(px(x0), px(y0)));
    for &(x, y) in &pts[1..] {
        builder.line_to(origin + point(px(x), px(y)));
    }
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

/// Draw the automation envelope for one lane inside its sub-lane area: the
/// filled body under the curve, the curve, the dashed base-value guide, the
/// points, and a value tag on the hovered (or, on the active lane, selected)
/// point. Pure render of state. The curve is sampled per visible column so
/// Hold steps and Linear ramps are both correct.
fn lane_envelope(
    lane: &AutomationLaneState,
    state: &TimelineState,
    lane_height: f32,
    is_active: bool,
    marquee: Option<&AutomationMarquee>,
    hover: Option<&AutomationHover>,
) -> impl IntoElement {
    let default_value = lane.target.default_value();
    let points = lane.points.clone();

    let lane_w = state.viewport.viewport_width.max(1.0);
    // Screen-space adaptive sampling: ~one sample per logical pixel of visible
    // width. That keeps the polyline continuous and smooth at any zoom (a wider
    // visible segment / higher zoom yields more samples), while a hard cap stops
    // an ultra-wide window from blowing up the per-frame stroke tessellation. A
    // 1px screen step is below what a thin AA stroke can resolve, so curves read
    // as smooth without oversampling tiny offscreen detail.
    const MAX_SAMPLES: usize = 4096;
    let sample_step = (lane_w / MAX_SAMPLES as f32).max(1.0);
    let sample_count = (lane_w / sample_step).ceil().max(1.0) as usize;

    // Hovered / actively-dragged segment → column range to emphasize. Uses the
    // SAME point geometry the curve is sampled from, so the highlight tracks the
    // visible curve at any zoom/scroll. `(c0, c1, active)`.
    let highlight: Option<(usize, usize, bool)> = hover
        .and_then(|h| h.segment_left_id.map(|id| (id, h.active)))
        .and_then(|(left_id, active)| {
            let i = points.iter().position(|p| p.id == left_id)?;
            if i + 1 >= points.len() {
                return None;
            }
            let x0 = state.beats_to_x(points[i].beat);
            let x1 = state.beats_to_x(points[i + 1].beat);
            let c0 = (x0 / sample_step).floor().max(0.0) as usize;
            let c1 = ((x1 / sample_step).ceil().max(0.0) as usize).min(sample_count);
            (c1 > c0).then_some((c0, c1, active))
        });

    let mut samples: Vec<(f32, f32)> = Vec::with_capacity(sample_count + 1);
    for sample in 0..=sample_count {
        let x = (sample as f32 * sample_step).min(lane_w);
        let beat = state.x_to_beat(x);
        let v = evaluate_automation(&points, beat, default_value);
        samples.push((x, automation_value_to_y(v, lane_height)));
    }
    let baseline_y = automation_value_to_y(default_value, lane_height);

    let enabled = lane.enabled;
    // Curve at ~0.85 so it stays clearly readable without the razor-sharp edge.
    let line_color = if enabled {
        Colors::with_alpha(Colors::automation_curve(), 0.85)
    } else {
        Colors::with_alpha(Colors::automation_curve(), 0.32)
    };
    // Hovered / dragged segment: same hue at full alpha, thicker line (width
    // carries the emphasis). No accent, glow or gloss — keeps the lane's "only
    // saturated element is the curve" rule. Active drag is thicker than hover.
    let highlight_color = if enabled {
        Colors::automation_curve()
    } else {
        Colors::with_alpha(Colors::automation_curve(), 0.5)
    };
    // Base-value guide: dashed so it reads as a reference, never as a second
    // curve. The filled body under the curve is what gives the lane its amount;
    // the active lane's body sits a step brighter so the focused lane is
    // findable across a tall arrangement without a louder curve.
    let baseline_color = Colors::automation_center_line();
    let body_alpha = match (enabled, is_active) {
        (true, true) => 0.16,
        (true, false) => 0.10,
        (false, _) => 0.05,
    };
    let body_color = Colors::with_alpha(Colors::automation_curve(), body_alpha);
    let body_floor = lane_height;

    let line = canvas(
        |_b, _w, _cx| {},
        move |bounds: Bounds<Pixels>, (), window, _cx| {
            // Filled body: the sampled curve closed down to the lane floor. One
            // fill path per lane, tessellated once — same cost class as the
            // stroke it sits under.
            if samples.len() >= 2 {
                let mut body = PathBuilder::fill();
                let (x0, y0) = samples[0];
                body.move_to(bounds.origin + point(px(x0), px(body_floor)));
                body.line_to(bounds.origin + point(px(x0), px(y0)));
                for &(x, y) in &samples[1..] {
                    body.line_to(bounds.origin + point(px(x), px(y)));
                }
                let (xn, _) = samples[samples.len() - 1];
                body.line_to(bounds.origin + point(px(xn), px(body_floor)));
                body.close();
                if let Ok(path) = body.build() {
                    window.paint_path(path, body_color);
                }
            }

            // Dashed base-value guide.
            const DASH: f32 = 6.0;
            const GAP: f32 = 4.0;
            let mut x = 0.0f32;
            while x < lane_w {
                let w = DASH.min(lane_w - x);
                let dash = Bounds::new(
                    bounds.origin + point(px(x), px(baseline_y)),
                    size(px(w), px(1.0)),
                );
                window.paint_quad(fill(dash, baseline_color));
                x += DASH + GAP;
            }

            // Base envelope: ONE continuous anti-aliased stroke for the whole
            // visible curve. Replaces the old per-column hard quads, so diagonals
            // and curved segments are smooth instead of stair-stepped.
            paint_automation_stroke(
                window,
                bounds.origin,
                &samples,
                AUTOMATION_LINE_WIDTH,
                line_color,
            );

            // Hovered / actively-dragged segment: the SAME sampled path, redrawn
            // thicker and at full alpha over the base. The emphasis is a clean
            // weight change with no second jagged 1px line and no doubled pixels.
            if let Some((c0, c1, active)) = highlight {
                if c1 > c0 && c1 <= sample_count {
                    let width = if active {
                        AUTOMATION_LINE_WIDTH_ACTIVE
                    } else {
                        AUTOMATION_LINE_WIDTH_HOVER
                    };
                    paint_automation_stroke(
                        window,
                        bounds.origin,
                        &samples[c0..=c1],
                        width,
                        highlight_color,
                    );
                }
            }
        },
    )
    .absolute()
    .inset_0();

    let hovered_point = hover.and_then(|h| h.point_id);
    let mut markers: Vec<gpui::Div> = Vec::new();
    let mut value_tags: Vec<gpui::Div> = Vec::new();
    for p in &points {
        let x = state.beats_to_x(p.beat);
        if x < -8.0 || x > lane_w + 8.0 {
            continue;
        }
        let y = automation_value_to_y(p.value, lane_height);
        let hovered = hovered_point == Some(p.id);
        let (fill_color, ring) = if p.selected {
            (Colors::text_primary(), Colors::automation_curve())
        } else {
            (Colors::automation_point(), Colors::automation_curve())
        };
        let size_px = if p.selected || hovered { 9.0 } else { 7.0 };
        markers.push(
            div()
                .absolute()
                .left(px(x - size_px / 2.0))
                .top(px(y - size_px / 2.0))
                .w(px(size_px))
                .h(px(size_px))
                .rounded(px(crate::theme::radius::PILL))
                .bg(fill_color)
                .border(px(1.0))
                .border_color(ring),
        );
        // Value tag: the hovered point always; selected points only on the
        // active lane, so a multi-lane track does not sprout a label per lane.
        if hovered || (p.selected && is_active) {
            const TAG_H: f32 = 16.0;
            // Above the point unless that would leave the lane; then below.
            let tag_top = if y - TAG_H - 6.0 >= 0.0 {
                y - TAG_H - 6.0
            } else {
                (y + 7.0).min(lane_height - TAG_H)
            };
            value_tags.push(
                div()
                    .absolute()
                    .left(px(x + 7.0))
                    .top(px(tag_top))
                    .h(px(TAG_H))
                    .px(px(5.0))
                    .flex()
                    .items_center()
                    .rounded(px(crate::theme::radius::CONTROL))
                    .bg(Colors::surface_raised())
                    .border(px(1.0))
                    .border_color(if hovered {
                        Colors::with_alpha(Colors::automation_curve(), 0.7)
                    } else {
                        Colors::border_subtle()
                    })
                    .text_size(px(9.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(Colors::text_primary())
                    .whitespace_nowrap()
                    .child(lane.target.format_value(p.value)),
            );
        }
    }

    let marquee_el = marquee.filter(|m| m.lane_id == lane.id).map(|m| {
        let x0 = state.beats_to_x(m.start_beat.min(m.cur_beat));
        let x1 = state.beats_to_x(m.start_beat.max(m.cur_beat));
        let y0 = automation_value_to_y(m.start_value.max(m.cur_value), lane_height);
        let y1 = automation_value_to_y(m.start_value.min(m.cur_value), lane_height);
        div()
            .absolute()
            .left(px(x0))
            .top(px(y0))
            .w(px((x1 - x0).max(1.0)))
            .h(px((y1 - y0).max(1.0)))
            .bg(Colors::with_alpha(Colors::accent_primary(), 0.14))
            .border(px(1.0))
            .border_color(Colors::with_alpha(Colors::accent_primary(), 0.7))
    });

    div()
        .absolute()
        .inset_0()
        .child(line)
        .children(markers)
        .children(value_tags)
        .children(marquee_el)
}

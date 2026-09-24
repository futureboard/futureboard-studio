use std::sync::Arc;

use gpui::{
    div, px, svg, AnyView, App, AppContext, Context, CursorStyle, ElementId, InteractiveElement,
    IntoElement, ParentElement, Render, StatefulInteractiveElement, Styled, Window,
};

use crate::assets;
use crate::components::timeline::timeline_state::{
    GlobalLaneKind, GlobalLaneResizeDrag, GLOBAL_LANE_RESIZE_HANDLE_HITBOX, HEADER_WIDTH,
};
use crate::theme::Colors;

const LANE_BTN: f32 = 20.0;
const LANE_ICON: f32 = 11.0;

pub type GlobalLaneVoidCb = Arc<dyn Fn(&(), &mut Window, &mut App) + 'static>;
pub type GlobalLaneMenuCb = Arc<dyn Fn(&(f32, f32), &mut Window, &mut App) + 'static>;
/// Pointer-down on a lane's resize handle: `(lane, window y)`.
pub type GlobalLaneResizeArmCb =
    Arc<dyn Fn(&(GlobalLaneKind, f32), &mut Window, &mut App) + 'static>;
/// Double-click on a lane's resize handle: back to the default height.
pub type GlobalLaneResizeResetCb = Arc<dyn Fn(&GlobalLaneKind, &mut Window, &mut App) + 'static>;

/// Stable element-id discriminator per lane, so the three handles never share
/// a GPUI id.
fn lane_id_index(kind: GlobalLaneKind) -> usize {
    match kind {
        GlobalLaneKind::Tempo => 0,
        GlobalLaneKind::TimeSignature => 1,
        GlobalLaneKind::SongText => 2,
        GlobalLaneKind::Marker => 3,
        GlobalLaneKind::Arranger => 4,
        GlobalLaneKind::Chord => 5,
    }
}

/// Drag strip on a global lane's bottom edge. Mirrors the arrangement track row
/// handle: pointer-down arms, the first drag-move promotes it to a live resize,
/// and a double-click resets the lane to its default height.
///
/// Spans the full lane width (header included) so the grab target is the same
/// edge the user sees, and sits above the lane content so the lane's own
/// mouse-down hit layer never swallows the gesture.
pub fn global_lane_resize_handle(
    kind: GlobalLaneKind,
    on_arm: GlobalLaneResizeArmCb,
    on_reset: GlobalLaneResizeResetCb,
) -> impl IntoElement {
    div()
        .absolute()
        .left_0()
        .right_0()
        .bottom(px(0.0))
        .h(px(GLOBAL_LANE_RESIZE_HANDLE_HITBOX))
        .id(("global-lane-resize", lane_id_index(kind)))
        .cursor(CursorStyle::ResizeUpDown)
        .on_mouse_down(gpui::MouseButton::Left, {
            let on_arm = on_arm.clone();
            let on_reset = on_reset.clone();
            move |event: &gpui::MouseDownEvent, window, cx| {
                cx.stop_propagation();
                if event.click_count >= 2 {
                    on_reset(&kind, window, cx);
                    return;
                }
                let y: f32 = event.position.y.into();
                on_arm(&(kind, y), window, cx);
            }
        })
        .on_drag(
            GlobalLaneResizeDrag { kind },
            move |_drag, _offset, _window, cx| {
                cx.stop_propagation();
                cx.new(|_| GlobalLaneResizeDrag { kind })
            },
        )
}

#[derive(Clone, Default)]
pub struct GlobalLaneHeaderActions {
    pub on_add: Option<GlobalLaneVoidCb>,
    pub on_menu: Option<GlobalLaneMenuCb>,
    pub on_hide: Option<GlobalLaneVoidCb>,
    pub on_toggle_collapsed: Option<GlobalLaneVoidCb>,
}

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

fn tooltip_view(text: &'static str) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    move |_window, cx| cx.new(|_| LaneTooltipText(text)).into()
}

fn lane_icon_button(
    id: impl Into<ElementId>,
    icon: &'static str,
    tooltip: &'static str,
    accent: bool,
    on_mouse_down: impl Fn(&gpui::MouseDownEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let color = if accent {
        Colors::accent_primary()
    } else {
        Colors::text_muted()
    };
    div()
        .id(id)
        .flex_shrink_0()
        .w(px(LANE_BTN))
        .h(px(LANE_BTN))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(crate::theme::radius::CONTROL_SM))
        .bg(Colors::with_alpha(Colors::surface_raised(), 0.0))
        .cursor(CursorStyle::PointingHand)
        .hover(|s| s.bg(Colors::surface_hover()))
        .tooltip(tooltip_view(tooltip))
        .on_mouse_down(gpui::MouseButton::Left, move |event, window, cx| {
            cx.stop_propagation();
            on_mouse_down(event, window, cx);
        })
        .child(
            svg()
                .path(icon)
                .w(px(LANE_ICON))
                .h(px(LANE_ICON))
                .text_color(color),
        )
}

/// Compact conductor-lane header shared by Tempo and Time Signature tracks.
///
/// Label first, controls on demand. The previous version washed the whole header
/// in accent, added a 2px accent left rule, stacked a title over a subtitle, and
/// showed four permanently-bordered icon buttons — so a lane whose entire job is
/// to say "Tempo" carried more visual weight than the musical content beside it.
/// The actions are still all there; they fade in on hover of the row, which is
/// where the eye already is when you reach for them.
pub fn global_lane_header(
    lane_id: &'static str,
    title: &'static str,
    subtitle: String,
    collapsed: bool,
    hide_tooltip: &'static str,
    actions: GlobalLaneHeaderActions,
) -> impl IntoElement {
    let collapse_icon = if collapsed {
        assets::ICON_CHEVRON_RIGHT_PATH
    } else {
        assets::ICON_CHEVRON_DOWN_PATH
    };
    // One group name per lane so hovering Tempo does not reveal Signature's
    // buttons. `SharedString` keeps the name alive for the element's lifetime.
    let group: gpui::SharedString = format!("global-lane-{lane_id}").into();

    let mut action_row = div()
        .flex()
        .items_center()
        .gap(px(crate::theme::space::HAIR))
        .flex_none()
        // Hidden until the row is hovered, and never by `display: none` — the
        // row must not reflow when the buttons appear, or the label jumps.
        .opacity(0.0)
        .group_hover(group.clone(), |s| s.opacity(1.0));

    if let Some(on_add) = actions.on_add {
        let add = on_add.clone();
        action_row = action_row.child(lane_icon_button(
            format!("global-lane-add-{lane_id}"),
            assets::ICON_PLUS_PATH,
            match lane_id {
                "tempo" => "Add tempo point at playhead",
                "region" => "Add region at playhead",
                "marker" => "Add marker at playhead",
                "chord" => "Open Chord Generator",
                _ => "Add time signature marker at playhead",
            },
            true,
            move |_event, window, cx| add(&(), window, cx),
        ));
    }

    if let Some(on_menu) = actions.on_menu {
        action_row = action_row.child(lane_icon_button(
            format!("global-lane-menu-{lane_id}"),
            assets::ICON_MENU_PATH,
            "Lane menu",
            false,
            move |event, window, cx| {
                let x: f32 = event.position.x.into();
                let y: f32 = event.position.y.into();
                on_menu(&(x, y), window, cx);
            },
        ));
    }

    if let Some(on_toggle) = actions.on_toggle_collapsed {
        let toggle = on_toggle.clone();
        action_row = action_row.child(lane_icon_button(
            format!("global-lane-collapse-{lane_id}"),
            collapse_icon,
            if collapsed {
                "Expand lane"
            } else {
                "Collapse lane"
            },
            false,
            move |_event, window, cx| toggle(&(), window, cx),
        ));
    }

    if let Some(on_hide) = actions.on_hide {
        let hide = on_hide.clone();
        action_row = action_row.child(lane_icon_button(
            format!("global-lane-hide-{lane_id}"),
            assets::ICON_X_PATH,
            hide_tooltip,
            false,
            move |_event, window, cx| hide(&(), window, cx),
        ));
    }

    div()
        .group(group)
        .flex_shrink_0()
        .w(px(HEADER_WIDTH))
        .h_full()
        .border_r(px(1.0))
        .border_color(Colors::border_normal())
        .bg(Colors::surface_panel_alt())
        .flex()
        .flex_row()
        .items_center()
        .gap(px(crate::theme::space::TIGHT))
        .px(px(crate::theme::space::BASE))
        .py(px(crate::theme::space::TIGHT))
        .child(
            div()
                .flex()
                .flex_col()
                .min_w_0()
                .flex_1()
                .child(
                    div()
                        .text_size(px(crate::theme::typography::UI_XS))
                        .text_color(Colors::text_muted())
                        .truncate()
                        .whitespace_nowrap()
                        .child(title),
                )
                // The subtitle carries real state (marker counts, "follows
                // project tempo"), so it stays — but only while the row is
                // hovered, so the resting lane is one clean label.
                .child(
                    div()
                        .text_size(px(crate::theme::typography::DENSE_CAPTION))
                        .text_color(Colors::text_faint())
                        .truncate()
                        .whitespace_nowrap()
                        .h(px(0.0))
                        .overflow_hidden()
                        .opacity(0.0)
                        .group_hover(format!("global-lane-{lane_id}"), |s| {
                            s.h(px(11.0)).opacity(1.0)
                        })
                        .child(subtitle),
                ),
        )
        .child(action_row)
}

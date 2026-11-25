//! Compact workspace chrome for audio tool windows.
//!
//! Controls stay `flex_none` and visualization occupies remaining space so a
//! resize grows the plot, not padding between knobs.

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, App, InteractiveElement, IntoElement, ParentElement, Role, StatefulInteractiveElement,
    Styled, Toggled, Window,
};

use crate::components::controls::{fb_segment, fb_segmented_track, FbSegment};
use crate::components::inspector::inspector_mini_button;
use crate::components::slider::compact_slider_with_reset;
use crate::theme::{radius, size, space, typography, Colors};

pub fn stage(viz: impl IntoElement, controls: impl IntoElement) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .size_full()
        .min_h(px(0.0))
        .child(div().flex_1().min_h(px(0.0)).min_w(px(0.0)).child(viz))
        .child(div().flex_none().child(controls))
}

pub fn viz_frame(plot: impl IntoElement, overlay: impl IntoElement) -> impl IntoElement {
    div()
        .size_full()
        .relative()
        .min_h(px(0.0))
        .bg(Colors::surface_canvas())
        .border_b(px(1.0))
        .border_color(Colors::border_subtle())
        .child(div().absolute().inset_0().child(plot))
        .child(
            div()
                .absolute()
                .top(px(space::TIGHT))
                .left(px(space::SNUG))
                .right(px(space::SNUG))
                .flex()
                .flex_row()
                .items_start()
                .justify_between()
                .child(overlay),
        )
}

pub fn overlay_stack() -> gpui::Div {
    div().flex().flex_col().gap(px(1.0))
}

pub fn overlay_line(text: impl Into<String>) -> impl IntoElement {
    div()
        .text_size(px(typography::DENSE_CAPTION))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(Colors::text_secondary())
        .child(text.into())
}

pub fn header(
    target: impl Into<String>,
    source: impl Into<String>,
    status: impl Into<String>,
    follow: impl IntoElement,
    pin: impl IntoElement,
) -> impl IntoElement {
    div()
        .flex_none()
        .h(px(size::DEFAULT))
        .px(px(space::SNUG))
        .gap(px(space::BASE))
        .flex()
        .flex_row()
        .items_center()
        .border_b(px(1.0))
        .border_color(Colors::border_subtle())
        .child(
            div()
                .flex_none()
                .text_size(px(typography::DENSE_LABEL))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(Colors::text_primary())
                .child(target.into()),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .text_size(px(typography::DENSE_CAPTION))
                .text_color(Colors::text_muted())
                .overflow_hidden()
                .child(source.into()),
        )
        .child(follow)
        .child(pin)
        .child(
            div()
                .flex_none()
                .text_size(px(typography::DENSE_CAPTION))
                .text_color(Colors::text_faint())
                .child(status.into()),
        )
}

pub fn nav_strip(child: impl IntoElement) -> impl IntoElement {
    div()
        .flex_none()
        .px(px(space::SNUG))
        .py(px(space::HAIR))
        .border_b(px(1.0))
        .border_color(Colors::border_subtle())
        .child(child)
}

pub fn control_strip(child: impl IntoElement) -> impl IntoElement {
    div()
        .flex_none()
        .px(px(space::SNUG))
        .py(px(space::TIGHT))
        .flex()
        .flex_col()
        .gap(px(space::TIGHT))
        .child(child)
}

pub fn workflow_bar(child: impl IntoElement) -> impl IntoElement {
    div()
        .flex_none()
        .h(px(size::COMFORTABLE))
        .px(px(space::SNUG))
        .flex()
        .flex_row()
        .items_center()
        .border_t(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_panel())
        .child(child)
}

pub fn workflow_row() -> gpui::Div {
    div()
        .flex()
        .flex_1()
        .flex_row()
        .items_center()
        .gap(px(space::TIGHT))
}

pub fn param_row(
    label: impl Into<String>,
    slider: impl IntoElement,
    value: impl Into<String>,
) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::SNUG))
        .h(px(size::DENSE))
        .child(
            div()
                .w(px(78.0))
                .flex_none()
                .text_size(px(typography::DENSE_LABEL))
                .text_color(Colors::text_muted())
                .child(label.into()),
        )
        .child(div().flex_1().min_w(px(0.0)).child(slider))
        .child(
            div()
                .w(px(52.0))
                .flex_none()
                .text_size(px(typography::DENSE_LABEL))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(Colors::text_primary())
                .child(value.into()),
        )
}

pub fn unipolar_slider(
    id: impl Into<gpui::SharedString>,
    value: f32,
    min: f32,
    max: f32,
    on_change: impl Fn(f32, &mut Window, &mut App) + 'static,
    on_reset: Option<f32>,
) -> impl IntoElement {
    let span = (max - min).max(1.0e-6);
    let norm = ((value - min) / span).clamp(0.0, 1.0);
    let on_change = std::sync::Arc::new(on_change);
    let on_drag = on_change.clone();
    let on_reset_fn = on_change;
    compact_slider_with_reset(
        id,
        norm,
        Colors::accent_primary(),
        move |next, window, cx| {
            on_drag(min + *next * span, window, cx);
        },
        Some(move |window: &mut Window, cx: &mut App| {
            if let Some(reset_at) = on_reset {
                on_reset_fn(reset_at, window, cx);
            }
        }),
    )
}

pub fn segment_position(index: usize, count: usize) -> FbSegment {
    if count <= 1 {
        FbSegment::Only
    } else if index == 0 {
        FbSegment::First
    } else if index + 1 == count {
        FbSegment::Last
    } else {
        FbSegment::Middle
    }
}

pub fn segment_track() -> gpui::Div {
    fb_segmented_track()
}

pub fn compact_segment(
    id: impl Into<gpui::ElementId>,
    label: impl Into<String>,
    active: bool,
    position: FbSegment,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    fb_segment(id, label, active, position, on_click)
}

pub fn latch(
    id: impl Into<gpui::ElementId>,
    label: impl Into<String>,
    active: bool,
    enabled: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let label: String = label.into();
    let rest = if active {
        Colors::accent_soft()
    } else {
        Colors::surface_input()
    };
    let hover = Colors::composite(rest, Colors::state_hover());
    div()
        .id(id)
        .role(Role::Button)
        .aria_label(label.clone())
        .aria_toggled(if active {
            Toggled::True
        } else {
            Toggled::False
        })
        .aria_disabled(!enabled)
        .h(px(size::DENSE))
        .px(px(space::SNUG))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(radius::CONTROL_SM))
        .bg(rest)
        .border(px(1.0))
        .border_color(if active {
            Colors::border_accent()
        } else {
            Colors::border_subtle()
        })
        .text_size(px(typography::DENSE_LABEL))
        .font_weight(if active {
            gpui::FontWeight::SEMIBOLD
        } else {
            gpui::FontWeight::MEDIUM
        })
        .text_color(if active {
            Colors::text_primary()
        } else {
            Colors::text_secondary()
        })
        .opacity(if enabled { 1.0 } else { 0.4 })
        .when(enabled, |this| {
            this.cursor(gpui::CursorStyle::PointingHand)
                .hover(move |s| s.bg(hover))
                .on_click(on_click)
        })
        .child(label)
}

pub fn apply_action(
    id: impl Into<gpui::ElementId>,
    label: impl Into<String>,
    enabled: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let label: String = label.into();
    div()
        .id(id)
        .role(Role::Button)
        .aria_label(label.clone())
        .aria_disabled(!enabled)
        .h(px(size::DENSE))
        .px(px(space::BASE))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(radius::CONTROL_SM))
        .bg(Colors::accent_primary())
        .text_size(px(typography::DENSE_LABEL))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(Colors::text_inverse())
        .opacity(if enabled { 1.0 } else { 0.4 })
        .when(enabled, |this| {
            this.cursor(gpui::CursorStyle::PointingHand)
                .hover(|s| s.bg(Colors::accent_primary_hover()))
                .on_click(on_click)
        })
        .child(label)
}

pub fn ghost_action(
    id: impl Into<gpui::ElementId>,
    label: impl Into<String>,
    enabled: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    inspector_mini_button(id, label, enabled, on_click)
}

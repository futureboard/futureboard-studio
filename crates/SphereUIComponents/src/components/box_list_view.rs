//! Compact DAW-style boxed list rows — reusable for MIDI / audio / plug-in / GPU device lists.

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, svg, App, Div, InteractiveElement, IntoElement, ParentElement, Role,
    StatefulInteractiveElement, Styled, Window,
};

use crate::theme::Colors;

/// Bordered list container for device / plug-in rows.
pub fn box_list_view() -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(1.0))
        .rounded(px(crate::theme::radius::CONTROL))
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_input())
        .overflow_hidden()
}

/// Group label above a [`box_list_view`].
pub fn box_list_group_label(title: impl Into<String>) -> impl IntoElement {
    div()
        .pb(px(3.0))
        .text_size(px(9.5))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(Colors::text_faint())
        .child(title.into())
}

/// Single list row shell — callers compose leading / content / trailing inside.
pub fn box_list_item() -> Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .min_h(px(36.0))
        .px(px(8.0))
        .py(px(5.0))
        .bg(Colors::surface_card())
}

pub fn box_list_item_leading_icon(path: &'static str) -> impl IntoElement {
    div()
        .flex_shrink_0()
        .w(px(24.0))
        .h(px(24.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(crate::theme::radius::CONTROL))
        .bg(Colors::surface_control_hover())
        .child(
            svg()
                .path(path)
                .w(px(12.0))
                .h(px(12.0))
                .text_color(Colors::text_secondary()),
        )
}

pub fn box_list_item_content() -> Div {
    div().flex_1().min_w_0().flex().flex_col().gap(px(1.0))
}

pub fn box_list_item_title(title: impl Into<String>) -> impl IntoElement {
    div()
        .truncate()
        .text_size(px(10.5))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(Colors::text_primary())
        .child(title.into())
}

pub fn box_list_item_subtitle(text: impl Into<String>) -> impl IntoElement {
    div()
        .truncate()
        .text_size(px(9.5))
        .text_color(Colors::text_muted())
        .child(text.into())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoxListBadgeTone {
    Neutral,
    Accent,
    Success,
    Warning,
    Error,
}

pub fn box_list_item_badge(label: impl Into<String>, tone: BoxListBadgeTone) -> impl IntoElement {
    let (bg, fg) = match tone {
        BoxListBadgeTone::Neutral => (Colors::surface_control_hover(), Colors::text_secondary()),
        BoxListBadgeTone::Accent => (Colors::accent_muted(), Colors::accent_primary()),
        BoxListBadgeTone::Success => (Colors::accent_muted(), Colors::status_success()),
        BoxListBadgeTone::Warning => (Colors::surface_control_hover(), Colors::status_warning()),
        BoxListBadgeTone::Error => (Colors::surface_control_hover(), Colors::status_error()),
    };
    div()
        .flex_shrink_0()
        .px(px(6.0))
        .py(px(1.0))
        .rounded(px(crate::theme::radius::CONTROL))
        .bg(bg)
        .text_size(px(9.0))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(fg)
        .child(label.into())
}

pub fn box_list_item_trailing() -> Div {
    div()
        .flex_shrink_0()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
}

/// Compact ON/OFF switch aligned at the trailing edge of list rows.
pub fn box_list_toggle(
    id: impl Into<gpui::ElementId>,
    enabled: bool,
    on_toggle: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    toggle_switch(id, enabled, true, on_toggle)
}

/// [`box_list_toggle`] without its ON/OFF word, for a table whose column
/// header already names what the switch does.
pub fn box_list_switch(
    id: impl Into<gpui::ElementId>,
    enabled: bool,
    on_toggle: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    toggle_switch(id, enabled, false, on_toggle)
}

fn toggle_switch(
    id: impl Into<gpui::ElementId>,
    enabled: bool,
    labelled: bool,
    on_toggle: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .role(Role::Switch)
        .aria_toggled(if enabled {
            gpui::Toggled::True
        } else {
            gpui::Toggled::False
        })
        .flex()
        .flex_row()
        .items_center()
        .gap(px(5.0))
        .cursor(gpui::CursorStyle::PointingHand)
        .on_click(on_toggle)
        .when(labelled, |row| {
            row.child(
                div()
                    .text_size(px(9.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(if enabled {
                        Colors::accent_primary()
                    } else {
                        Colors::text_faint()
                    })
                    .child(if enabled { "ON" } else { "OFF" }),
            )
        })
        .child(
            div()
                .relative()
                .w(px(30.0))
                .h(px(16.0))
                .rounded(px(crate::theme::radius::PILL))
                .bg(if enabled {
                    Colors::accent_primary()
                } else {
                    Colors::surface_control_hover()
                })
                .child(
                    div()
                        .absolute()
                        .top(px(1.0))
                        .left(if enabled { px(15.0) } else { px(1.0) })
                        .w(px(14.0))
                        .h(px(14.0))
                        .rounded(px(crate::theme::radius::PILL))
                        .bg(Colors::text_inverse())
                        .shadow(vec![gpui::BoxShadow {
                            color: Colors::with_alpha(Colors::surface_base(), 0.35).into(),
                            offset: gpui::point(px(0.0), px(1.0)),
                            blur_radius: px(2.0),
                            spread_radius: px(0.0),
                            inset: false,
                        }]),
                ),
        )
}

pub fn box_list_icon_button(
    id: impl Into<gpui::ElementId>,
    icon_path: &'static str,
    tooltip: &'static str,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .role(Role::Button)
        .aria_label(tooltip)
        .focusable()
        .tab_stop(true)
        .focus_visible(|style| style.border_color(Colors::border_focus()))
        .flex()
        .items_center()
        .justify_center()
        .w(px(22.0))
        .h(px(22.0))
        .rounded(px(crate::theme::radius::CONTROL))
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_input())
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(|s| {
            s.bg(Colors::surface_control_hover())
                .border_color(Colors::border_strong())
        })
        .on_click(on_click)
        .child(
            svg()
                .path(icon_path)
                .w(px(11.0))
                .h(px(11.0))
                .text_color(Colors::text_secondary()),
        )
}

pub fn box_list_empty_state(
    message: impl Into<String>,
    action_label: impl Into<String>,
    on_action: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(8.0))
        .py(px(16.0))
        .px(px(12.0))
        .rounded(px(crate::theme::radius::CONTROL))
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_input())
        .child(
            div()
                .text_size(px(10.5))
                .text_color(Colors::text_muted())
                .child(message.into()),
        )
        .child(
            div()
                .id("box-list-empty-action")
                .px(px(10.0))
                .py(px(5.0))
                .rounded(px(crate::theme::radius::CONTROL))
                .border(px(1.0))
                .border_color(Colors::border_subtle())
                .bg(Colors::surface_card())
                .text_size(px(10.0))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(Colors::text_secondary())
                .cursor(gpui::CursorStyle::PointingHand)
                .hover(|s| s.bg(Colors::surface_control_hover()))
                .on_click(on_action)
                .child(action_label.into()),
        )
}

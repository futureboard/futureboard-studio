//! DAW-style layout primitives for the Preferences window.

use gpui::{
    div, px, svg, App, Div, InteractiveElement, IntoElement, ParentElement,
    StatefulInteractiveElement, Styled, Window,
};

use crate::theme::{typography, Colors};

pub const SETTINGS_WINDOW_WIDTH: f32 = 880.0;
pub const SETTINGS_WINDOW_HEIGHT: f32 = 600.0;
pub const SETTINGS_SIDEBAR_WIDTH: f32 = 196.0;
pub const SETTINGS_CONTENT_PAD: f32 = 14.0;
pub const SETTINGS_LABEL_WIDTH: f32 = 132.0;
pub const SETTINGS_ROW_GAP: f32 = 12.0;
pub const SETTINGS_SECTION_GAP: f32 = 10.0;
/// Preferences page geometry: the content column's side padding, the padding
/// inside a group's plate, and the one width every dropdown has. Dropdown menus
/// are anchored from these (see [`crate::overlay::settings_form_column`]).
pub const PREFS_CONTENT_PAD_X: f32 = crate::theme::space::BLOCK;
pub const PREFS_ROW_PAD_X: f32 = crate::theme::space::LOOSE;
pub const PREFS_CONTROL_W: f32 = 240.0;

fn settings_icon(path: &'static str, size: f32, color: gpui::Rgba) -> impl IntoElement {
    svg().path(path).w(px(size)).h(px(size)).text_color(color)
}

/// Sidebar group label (GENERAL, STUDIO, …).
pub fn settings_nav_group_header(title: impl Into<String>) -> impl IntoElement {
    let title = title.into();
    div()
        .pt(px(12.0))
        .pb(px(5.0))
        .px(px(12.0))
        .text_size(px(typography::DENSE_CAPTION))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(Colors::text_faint())
        .child(title.to_uppercase())
}

/// Compact sidebar category row.
pub fn settings_nav_item(
    id: impl Into<gpui::ElementId>,
    label: impl Into<String>,
    icon_path: &'static str,
    active: bool,
    search_hit: bool,
    on_select: impl Fn(&mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let label = label.into();
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .h(px(30.0))
        .px(px(10.0))
        .mx(px(8.0))
        .rounded(px(crate::theme::radius::CONTROL))
        .bg(if active {
            Colors::accent_muted()
        } else {
            gpui::transparent_black().into()
        })
        .text_size(px(typography::DENSE_LABEL))
        .font_weight(if active {
            gpui::FontWeight::SEMIBOLD
        } else {
            gpui::FontWeight::MEDIUM
        })
        .text_color(if active {
            Colors::text_primary()
        } else if search_hit {
            Colors::accent_primary()
        } else {
            Colors::text_secondary()
        })
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(|s| {
            s.bg(if active {
                Colors::accent_muted()
            } else {
                Colors::surface_control_hover()
            })
        })
        .on_click(move |_, window, cx| on_select(window, cx))
        .child(settings_icon(
            icon_path,
            12.0,
            if active {
                Colors::accent_primary()
            } else {
                Colors::text_faint()
            },
        ))
        .child(label)
}

/// Content area page header (category title + optional description).
pub fn settings_page_header(
    title: impl Into<String>,
    description: impl Into<String>,
) -> impl IntoElement {
    let title = title.into();
    let description = description.into();
    div()
        .flex()
        .flex_col()
        .gap(px(3.0))
        .pb(px(2.0))
        .child(
            div()
                .text_size(px(14.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(Colors::text_primary())
                .child(title),
        )
        .child(
            div()
                .text_size(px(typography::UI_XS))
                .text_color(Colors::text_muted())
                .child(description),
        )
}

/// Bordered settings group card.
pub fn settings_section_card() -> Div {
    div()
        .rounded(px(crate::theme::radius::CONTROL))
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_card())
        .p(px(12.0))
        .flex()
        .flex_col()
        .gap(px(6.0))
}

/// Section title inside a card.
pub fn settings_section_title(title: impl Into<String>) -> impl IntoElement {
    let title = title.into();
    div()
        .pb(px(4.0))
        .border_b(px(1.0))
        .border_color(Colors::divider())
        .text_size(px(typography::UI_XS))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(Colors::text_primary())
        .child(title)
}

pub fn settings_section_hint(text: impl Into<String>) -> impl IntoElement {
    let text = text.into();
    div()
        .text_size(px(typography::DENSE_LABEL))
        .text_color(Colors::text_faint())
        .child(text)
}

pub fn settings_field_label(label: impl Into<String>) -> impl IntoElement {
    let label = label.into();
    div()
        .w(px(SETTINGS_LABEL_WIDTH))
        .flex_shrink_0()
        .text_size(px(typography::UI_XS))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(Colors::text_muted())
        .child(label)
}

/// DAW-aligned label + control row.
pub fn settings_daw_row(label: impl Into<String>, child: impl IntoElement) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(SETTINGS_ROW_GAP))
        .min_h(px(30.0))
        .child(settings_field_label(label))
        .child(div().flex_1().min_w_0().child(child))
}

/// DAW-aligned label + control row, with optional description under the label.
pub fn settings_daw_row_with_description(
    label: impl Into<String>,
    description: Option<String>,
    child: impl IntoElement,
) -> impl IntoElement {
    let label = label.into();
    div()
        .flex()
        .flex_row()
        .items_start()
        .gap(px(SETTINGS_ROW_GAP))
        .min_h(px(30.0))
        .child(
            div()
                .w(px(SETTINGS_LABEL_WIDTH))
                .flex_shrink_0()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .text_size(px(typography::UI_XS))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(Colors::text_muted())
                        .child(label),
                )
                .children(description.map(|d| {
                    div()
                        .text_size(px(typography::DENSE_CAPTION))
                        .text_color(Colors::text_faint())
                        .child(d)
                })),
        )
        .child(div().flex_1().min_w_0().child(child))
}

pub fn settings_value_readout(text: impl Into<String>) -> impl IntoElement {
    div()
        .text_size(px(typography::UI_XS))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(Colors::text_secondary())
        .child(text.into())
}

pub fn settings_status_badge(label: impl Into<String>, ok: bool) -> impl IntoElement {
    div()
        .px(px(7.0))
        .py(px(2.0))
        .rounded(px(crate::theme::radius::CONTROL))
        .bg(if ok {
            Colors::accent_muted()
        } else {
            Colors::surface_control_hover()
        })
        .text_size(px(typography::DENSE_LABEL))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(if ok {
            Colors::status_success()
        } else {
            Colors::status_warning()
        })
        .child(label.into())
}

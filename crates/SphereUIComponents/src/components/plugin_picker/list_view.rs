//! Plugin list row rendering.

use std::sync::Arc;

use gpui::prelude::FluentBuilder;
use gpui::{
    App, AppContext, Div, InteractiveElement, IntoElement, ParentElement,
    StatefulInteractiveElement, Styled, Window, div, px, svg,
};

use crate::assets;
use crate::components::plugin_format_badge::{plugin_format_badge, plugin_format_badge_for};
use crate::components::plugin_picker::category::normalized_category_label;
use crate::components::plugin_picker::insert::is_insertable;
use crate::theme::Colors;
use SpherePluginHost::{PluginFormat, PluginKind, PluginScanStatus, PluginStatus, RegistryPlugin};

pub const ROW_HEIGHT: f32 = 38.0;

/// Shared column metrics — header and body rows must stay in sync.
const ROW_GAP: f32 = 8.0;
const COL_ICON: f32 = 12.0;
const COL_FAVORITE: f32 = 18.0;
const COL_NAME_MIN: f32 = 160.0;
const COL_VENDOR_MIN: f32 = 100.0;
const COL_CATEGORY_MIN: f32 = 100.0;
/// Instrument / Audio Effect / Unknown, straight from `PluginKind`. Fixed width
/// because the three labels are the only values it ever shows.
const COL_TYPE: f32 = 86.0;
const COL_FORMAT: f32 = 80.0;

type StringCb = Arc<dyn Fn(&String, &mut Window, &mut App) + 'static>;

#[derive(Clone, Debug)]
pub struct PluginDragItem {
    pub plugin_id: String,
    pub label: String,
    pub kind: PluginKind,
}

fn col_name_cell(label: impl Into<String>) -> Div {
    div()
        .flex_1()
        .min_w(px(COL_NAME_MIN))
        .min_w_0()
        .overflow_hidden()
        .text_size(px(11.0))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(Colors::text_primary())
        .truncate()
        .child(label.into())
}

fn col_vendor_cell(label: impl Into<String>) -> Div {
    div()
        .flex_1()
        .min_w(px(COL_VENDOR_MIN))
        .min_w_0()
        .overflow_hidden()
        .text_size(px(10.5))
        .text_color(Colors::text_dim())
        .truncate()
        .child(label.into())
}

fn col_category_cell(label: impl Into<String>) -> Div {
    div()
        .flex_1()
        .min_w(px(COL_CATEGORY_MIN))
        .min_w_0()
        .overflow_hidden()
        .text_size(px(10.5))
        .text_color(Colors::text_dim())
        .truncate()
        .child(label.into())
}

fn col_type_cell(kind: PluginKind) -> Div {
    div()
        .w(px(COL_TYPE))
        .flex_shrink_0()
        .overflow_hidden()
        .text_size(px(10.5))
        .text_color(match kind {
            PluginKind::Instrument => Colors::accent_primary(),
            PluginKind::Effect => Colors::text_dim(),
            PluginKind::Unknown => Colors::text_faint(),
        })
        .truncate()
        .child(kind.label())
}

fn col_format_cell(plugin: &RegistryPlugin) -> Div {
    div()
        .w(px(COL_FORMAT))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_end()
        .child(plugin_format_badge_for(plugin))
}

/// Column header row — shares the same flex column layout as [`plugin_row`].
pub fn plugin_table_header() -> impl IntoElement {
    div()
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .h(px(26.0))
        .px(px(10.0))
        .border_b(px(1.0))
        .border_color(Colors::divider())
        .bg(Colors::surface_input())
        .gap(px(ROW_GAP))
        .text_size(px(9.5))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(Colors::text_faint())
        .child(div().w(px(COL_ICON)).flex_shrink_0())
        .child(div().w(px(COL_FAVORITE)).flex_shrink_0())
        .child(
            div()
                .flex_1()
                .min_w(px(COL_NAME_MIN))
                .min_w_0()
                .overflow_hidden()
                .truncate()
                .child("Plug-in"),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(COL_VENDOR_MIN))
                .min_w_0()
                .overflow_hidden()
                .truncate()
                .child("Vendor"),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(COL_CATEGORY_MIN))
                .min_w_0()
                .overflow_hidden()
                .truncate()
                .child("Category"),
        )
        .child(
            div()
                .w(px(COL_TYPE))
                .flex_shrink_0()
                .overflow_hidden()
                .truncate()
                .child("Type"),
        )
        .child(
            div()
                .w(px(COL_FORMAT))
                .flex_shrink_0()
                .text_align(gpui::TextAlign::Right)
                .child("Format"),
        )
}

pub fn plugin_row(
    list_index: usize,
    plugin: &RegistryPlugin,
    highlighted: bool,
    favorite: bool,
    on_select: StringCb,
    on_pick: StringCb,
    on_toggle_favorite: StringCb,
) -> impl IntoElement {
    let id_select = plugin.id.clone();
    let id_pick = plugin.id.clone();
    let id_fav = plugin.id.clone();
    let name = plugin.name.clone();
    let vendor = plugin.vendor.clone();
    let category = normalized_category_label(plugin);
    let insertable = is_insertable(plugin);
    let (kind_icon, kind_color) = kind_icon_for(plugin.kind);
    let status = scan_status_label(plugin);
    let drag_item = PluginDragItem {
        plugin_id: plugin.id.clone(),
        label: plugin.name.clone(),
        kind: plugin.kind,
    };

    div()
        .id(("plugin-picker-row", list_index))
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(ROW_GAP))
        .h(px(ROW_HEIGHT))
        .px(px(10.0))
        .border_b(px(1.0))
        .border_color(Colors::divider())
        .when(highlighted, |el| {
            el.bg(Colors::accent_muted())
                .border_l(px(2.0))
                .border_color(Colors::accent_primary())
        })
        .when(!highlighted, |el| {
            el.hover(|s| s.bg(Colors::surface_hover()))
        })
        .when(!insertable, |el| el.opacity(0.55))
        .when(insertable, |el| {
            el.on_drag(drag_item, move |drag, _offset, _window, cx| {
                cx.new(|_| crate::components::plugin_picker::PluginDragPreview {
                    label: drag.label.clone(),
                })
            })
        })
        .cursor(if insertable {
            gpui::CursorStyle::PointingHand
        } else {
            gpui::CursorStyle::Arrow
        })
        .on_click(move |event, window, cx| {
            if event.click_count() >= 2 {
                if insertable {
                    on_pick(&id_pick, window, cx);
                }
            } else {
                on_select(&id_select, window, cx);
            }
        })
        .child(
            div()
                .w(px(COL_ICON))
                .flex_shrink_0()
                .child(icon(kind_icon, 12.0, kind_color)),
        )
        .child(
            div()
                .w(px(COL_FAVORITE))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .cursor(gpui::CursorStyle::PointingHand)
                .child(icon(
                    assets::ICON_STAR_PATH,
                    11.0,
                    if favorite {
                        Colors::status_warning()
                    } else {
                        Colors::text_faint()
                    },
                ))
                .on_mouse_down(gpui::MouseButton::Left, move |_, window, cx| {
                    on_toggle_favorite(&id_fav, window, cx);
                }),
        )
        .child(col_name_cell(name))
        .child(col_vendor_cell(vendor))
        .child(col_category_cell(category))
        .child(col_type_cell(plugin.kind))
        .child(col_format_cell(plugin))
        .when_some(status, |el, label| {
            el.child(div().flex_shrink_0().child(status_badge(label, true)))
        })
}

fn kind_icon_for(kind: PluginKind) -> (&'static str, gpui::Rgba) {
    match kind {
        PluginKind::Instrument => (assets::ICON_MUSIC_PATH, Colors::accent_primary()),
        PluginKind::Effect => (
            assets::ICON_SLIDERS_HORIZONTAL_PATH,
            Colors::status_success(),
        ),
        // Same glyph as an effect — that is how it can be inserted — but muted,
        // so "we do not know" never reads as a confirmed classification.
        PluginKind::Unknown => (assets::ICON_SLIDERS_HORIZONTAL_PATH, Colors::text_faint()),
    }
}

fn icon(path: &'static str, size: f32, color: gpui::Rgba) -> impl IntoElement {
    svg().path(path).w(px(size)).h(px(size)).text_color(color)
}

pub fn format_badge(fmt: PluginFormat) -> impl IntoElement {
    plugin_format_badge(fmt)
}

pub fn format_badge_for(plugin: &RegistryPlugin) -> impl IntoElement {
    plugin_format_badge_for(plugin)
}

fn status_badge(label: &'static str, tone_warn: bool) -> impl IntoElement {
    let (fg, bg) = if tone_warn {
        (Colors::status_warning(), gpui::rgba(0xE5C07B14))
    } else {
        (Colors::status_success(), gpui::rgba(0x6FCF9714))
    };
    div()
        .px(px(5.0))
        .py(px(1.0))
        .rounded(px(crate::theme::radius::CONTROL))
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(bg)
        .text_size(px(9.0))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(fg)
        .child(label)
}

pub fn scan_status_label(plugin: &RegistryPlugin) -> Option<&'static str> {
    match plugin.scan_status {
        PluginScanStatus::Crashed => Some("Crashed"),
        PluginScanStatus::Failed | PluginScanStatus::MetadataOnly => Some("Failed"),
        PluginScanStatus::Skipped | PluginScanStatus::Disabled => Some("Disabled"),
        PluginScanStatus::Success | PluginScanStatus::Ok => {
            if plugin.status == PluginStatus::MissingPreset {
                Some("Missing")
            } else if !plugin.supports_insert() {
                Some("Unsupported")
            } else {
                None
            }
        }
        _ => None,
    }
}

pub fn skeleton_row(index: usize) -> impl IntoElement {
    let block = |w: f32, alpha: f32| {
        div()
            .h(px(10.0))
            .w(px(w))
            .rounded(px(crate::theme::radius::CONTROL))
            .bg(Colors::with_alpha(Colors::text_primary(), alpha))
    };
    let alpha = 0.06 + ((index % 3) as f32) * 0.015;
    div()
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(ROW_GAP))
        .h(px(ROW_HEIGHT))
        .px(px(10.0))
        .border_b(px(1.0))
        .border_color(Colors::divider())
        .child(
            div().w(px(COL_ICON)).flex_shrink_0().child(
                div()
                    .w(px(12.0))
                    .h(px(12.0))
                    .rounded(px(crate::theme::radius::CONTROL))
                    .bg(Colors::with_alpha(Colors::text_primary(), alpha)),
            ),
        )
        .child(div().w(px(COL_FAVORITE)).flex_shrink_0())
        .child(
            div()
                .flex_1()
                .min_w(px(COL_NAME_MIN))
                .min_w_0()
                .child(block(140.0 + (index % 4) as f32 * 20.0, alpha)),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(COL_VENDOR_MIN))
                .min_w_0()
                .child(block(110.0, alpha)),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(COL_CATEGORY_MIN))
                .min_w_0()
                .child(block(80.0, alpha)),
        )
        .child(
            div()
                .w(px(COL_TYPE))
                .flex_shrink_0()
                .child(block(58.0, alpha)),
        )
        .child(
            div()
                .w(px(COL_FORMAT))
                .flex_shrink_0()
                .flex()
                .justify_end()
                .child(block(36.0, alpha)),
        )
}

pub fn skeleton_body() -> impl IntoElement {
    let mut col = div().flex().flex_col().w_full();
    for i in 0..14 {
        col = col.child(skeleton_row(i));
    }
    col
}

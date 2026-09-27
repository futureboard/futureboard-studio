//! Plugin list rows and their column header.
//!
//! The list is the picker's main surface, so it stays calm: one line per
//! plug-in, the name leading, everything else quieter. Rows are full-bleed and
//! therefore square; selection is a fill plus a leading-edge marker drawn as an
//! overlay, so selecting a row never reflows it.

use std::sync::Arc;

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, svg, App, AppContext, Div, InteractiveElement, IntoElement, ParentElement, Rgba,
    StatefulInteractiveElement, Styled, Window,
};

use crate::assets;
use crate::components::plugin_format_badge::{plugin_format_badge, plugin_format_badge_for};
use crate::components::plugin_picker::category::normalized_category_label;
use crate::components::plugin_picker::insert::is_insertable;
use crate::theme::{radius, size, space, state, typography, Colors};
use SpherePluginHost::{PluginFormat, PluginKind, PluginScanStatus, PluginStatus, RegistryPlugin};

/// One list row. Compact on purpose: a large library is scanned, not read.
pub const ROW_HEIGHT: f32 = size::COMFORTABLE;

/// Shared column metrics — header and body rows must stay in sync.
const ROW_PAD_X: f32 = space::LOOSE;
const COL_GAP: f32 = space::BASE;
const COL_STAR: f32 = size::MICRO;
const COL_KIND: f32 = 14.0;
const COL_NAME_MIN: f32 = 160.0;
const COL_VENDOR_W: f32 = 150.0;
const COL_CATEGORY_W: f32 = 110.0;
const COL_FORMAT_W: f32 = 64.0;

type StringCb = Arc<dyn Fn(&String, &mut Window, &mut App) + 'static>;

#[derive(Clone, Debug)]
pub struct PluginDragItem {
    pub plugin_id: String,
    pub label: String,
    pub kind: PluginKind,
}

/// Row paint resolved once per render. `Colors::composite` is a control-path
/// helper, so the list builds this before materialising any row.
#[derive(Clone, Copy)]
pub struct RowPaint {
    rest: Rgba,
    hover: Rgba,
    selected: Rgba,
    selected_hover: Rgba,
}

impl RowPaint {
    pub fn resolve() -> Self {
        let rest = Colors::surface_base();
        Self {
            rest,
            hover: Colors::composite(rest, Colors::state_hover()),
            selected: Colors::composite(rest, Colors::state_selected()),
            selected_hover: Colors::composite(rest, Colors::state_selected_hover()),
        }
    }
}

/// Column header row — shares the column metrics of [`plugin_row`].
pub fn plugin_table_header() -> impl IntoElement {
    let caption = |label: &'static str| {
        div()
            .text_size(px(typography::DENSE_CAPTION))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(Colors::text_faint())
            .truncate()
            .child(label)
    };
    div()
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .flex_shrink_0()
        .h(px(size::ROW_DENSE))
        .px(px(ROW_PAD_X))
        .gap(px(COL_GAP))
        .border_b(px(1.0))
        .border_color(Colors::divider())
        .bg(Colors::surface_base())
        .child(div().w(px(COL_STAR)).flex_shrink_0())
        .child(div().w(px(COL_KIND)).flex_shrink_0())
        .child(
            div()
                .flex_1()
                .min_w(px(COL_NAME_MIN))
                .child(caption("Name")),
        )
        .child(
            div()
                .w(px(COL_VENDOR_W))
                .flex_shrink_0()
                .child(caption("Vendor")),
        )
        .child(
            div()
                .w(px(COL_CATEGORY_W))
                .flex_shrink_0()
                .child(caption("Category")),
        )
        .child(
            div()
                .w(px(COL_FORMAT_W))
                .flex_shrink_0()
                .flex()
                .justify_end()
                .child(caption("Format")),
        )
}

#[allow(clippy::too_many_arguments)]
pub fn plugin_row(
    list_index: usize,
    plugin: &RegistryPlugin,
    highlighted: bool,
    favorite: bool,
    paint: RowPaint,
    on_select: StringCb,
    on_pick: StringCb,
    on_toggle_favorite: StringCb,
) -> impl IntoElement {
    let id_select = plugin.id.clone();
    let id_pick = plugin.id.clone();
    let id_fav = plugin.id.clone();
    let insertable = is_insertable(plugin);
    let status = scan_status_label(plugin);
    let drag_item = PluginDragItem {
        plugin_id: plugin.id.clone(),
        label: plugin.name.clone(),
        kind: plugin.kind,
    };
    let (rest, hover) = if highlighted {
        (paint.selected, paint.selected_hover)
    } else {
        (paint.rest, paint.hover)
    };
    let dim = |color: Rgba| {
        if insertable {
            color
        } else {
            Colors::with_alpha(color, state::DISABLED_CONTENT + 0.2)
        }
    };

    div()
        .id(("plugin-picker-row", list_index))
        .relative()
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(COL_GAP))
        .h(px(ROW_HEIGHT))
        .px(px(ROW_PAD_X))
        .bg(rest)
        .hover(move |s| s.bg(hover))
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
        .when(highlighted, |el| el.child(selection_marker()))
        .child(favorite_star(
            list_index,
            favorite,
            paint.hover,
            id_fav,
            on_toggle_favorite,
        ))
        .child(
            div()
                .w(px(COL_KIND))
                .flex_shrink_0()
                .flex()
                .items_center()
                .child(kind_glyph(plugin.kind, insertable)),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(COL_NAME_MIN))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::SNUG))
                .overflow_hidden()
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(px(typography::UI_SM))
                        .font_weight(if highlighted {
                            gpui::FontWeight::SEMIBOLD
                        } else {
                            gpui::FontWeight::MEDIUM
                        })
                        .text_color(dim(Colors::text_primary()))
                        .child(plugin.name.clone()),
                )
                .when_some(status, |el, label| {
                    el.child(
                        div()
                            .flex_shrink_0()
                            .child(crate::components::controls::fb_badge(
                                label,
                                Colors::status_warning(),
                            )),
                    )
                }),
        )
        .child(
            div()
                .w(px(COL_VENDOR_W))
                .flex_shrink_0()
                .truncate()
                .text_size(px(typography::UI_XS))
                .text_color(dim(Colors::text_muted()))
                .child(plugin.vendor.clone()),
        )
        .child(
            div()
                .w(px(COL_CATEGORY_W))
                .flex_shrink_0()
                .truncate()
                .text_size(px(typography::UI_XS))
                .text_color(dim(Colors::text_faint()))
                .child(normalized_category_label(plugin)),
        )
        .child(
            div()
                .w(px(COL_FORMAT_W))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_end()
                .child(plugin_format_badge_for(plugin)),
        )
}

/// The selected row's leading-edge marker. An overlay, so the row's content
/// does not shift when it is selected.
fn selection_marker() -> impl IntoElement {
    div()
        .absolute()
        .left_0()
        .top(px(space::TIGHT))
        .bottom(px(space::TIGHT))
        .w(px(2.0))
        .rounded(px(radius::PILL))
        .bg(Colors::accent_primary())
}

fn favorite_star(
    list_index: usize,
    favorite: bool,
    hover: Rgba,
    plugin_id: String,
    on_toggle: StringCb,
) -> impl IntoElement {
    let rest = if favorite {
        Colors::status_warning()
    } else {
        Colors::with_alpha(Colors::text_faint(), 0.5)
    };
    div()
        .id(("plugin-picker-star", list_index))
        .w(px(COL_STAR))
        .h(px(COL_STAR))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(radius::CONTROL_SM))
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(move |s| s.bg(hover))
        .tooltip(crate::components::controls::fb_tooltip(if favorite {
            "Remove from Favorites"
        } else {
            "Add to Favorites"
        }))
        // Not a row click: starring a plug-in must neither select nor insert it.
        .on_click(move |_, window, cx| {
            cx.stop_propagation();
            on_toggle(&plugin_id, window, cx);
        })
        .child(
            svg()
                .path(assets::ICON_STAR_PATH)
                .w(px(11.0))
                .h(px(11.0))
                .text_color(rest),
        )
}

/// Kind glyph. Instruments and effects differ in shape, not only hue; an
/// undeclared plug-in shares the effect glyph (it inserts as one) but muted, so
/// "we do not know" never reads as a confirmed classification.
pub fn kind_glyph(kind: PluginKind, available: bool) -> impl IntoElement {
    let (path, color) = match kind {
        PluginKind::Instrument => (assets::ICON_MUSIC_PATH, Colors::accent_primary()),
        PluginKind::Effect => (assets::ICON_SLIDERS_HORIZONTAL_PATH, Colors::text_muted()),
        PluginKind::Unknown => (assets::ICON_SLIDERS_HORIZONTAL_PATH, Colors::text_faint()),
    };
    let color = if available {
        color
    } else {
        Colors::with_alpha(color, state::DISABLED_CONTENT)
    };
    svg().path(path).w(px(12.0)).h(px(12.0)).text_color(color)
}

pub fn format_badge(fmt: PluginFormat) -> impl IntoElement {
    plugin_format_badge(fmt)
}

pub fn format_badge_for(plugin: &RegistryPlugin) -> impl IntoElement {
    plugin_format_badge_for(plugin)
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

fn skeleton_row(index: usize) -> impl IntoElement {
    let alpha = 0.05 + ((index % 3) as f32) * 0.015;
    let block = move |w: f32| {
        div()
            .h(px(8.0))
            .w(px(w))
            .rounded(px(radius::MICRO))
            .bg(Colors::with_alpha(Colors::text_primary(), alpha))
    };
    div()
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(COL_GAP))
        .h(px(ROW_HEIGHT))
        .px(px(ROW_PAD_X))
        .child(div().w(px(COL_STAR)).flex_shrink_0())
        .child(div().w(px(COL_KIND)).flex_shrink_0().child(block(12.0)))
        .child(
            div()
                .flex_1()
                .min_w(px(COL_NAME_MIN))
                .child(block(120.0 + (index % 4) as f32 * 24.0)),
        )
        .child(div().w(px(COL_VENDOR_W)).flex_shrink_0().child(block(96.0)))
        .child(
            div()
                .w(px(COL_CATEGORY_W))
                .flex_shrink_0()
                .child(block(64.0)),
        )
        .child(
            div()
                .w(px(COL_FORMAT_W))
                .flex_shrink_0()
                .flex()
                .justify_end()
                .child(block(32.0)),
        )
}

pub fn skeleton_body() -> Div {
    let mut col = div().flex().flex_col().w_full();
    for i in 0..16 {
        col = col.child(skeleton_row(i));
    }
    col
}

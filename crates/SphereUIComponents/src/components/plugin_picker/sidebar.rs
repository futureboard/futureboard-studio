//! Plugin picker rail: collections, formats, categories, and (folded) vendors.
//!
//! Kind is not here — it is the tab row above the list, and it scopes every
//! count on this rail, so each entry says what clicking it would show.

use std::sync::Arc;

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, svg, App, InteractiveElement, IntoElement, ParentElement, Rgba, ScrollHandle,
    StatefulInteractiveElement, Styled, Window,
};

use crate::assets;
use crate::components::plugin_picker::filter::FilterCounts;
use crate::components::plugin_picker::state::PickerFilter;
use crate::components::scroll_thumb::vertical_scrollbar_thumb;
use crate::theme::{radius, size, space, typography, Colors};
use SpherePluginHost::PluginFormat;

pub const SIDEBAR_WIDTH: f32 = 196.0;

type FilterCb = Arc<dyn Fn(&PickerFilter, &mut Window, &mut App) + 'static>;
type VoidCb = Arc<dyn Fn(&(), &mut Window, &mut App) + 'static>;

/// Rail item paint, resolved once per render.
#[derive(Clone, Copy)]
struct RailPaint {
    hover: Rgba,
    selected: Rgba,
    selected_hover: Rgba,
}

impl RailPaint {
    fn resolve() -> Self {
        let base = Colors::surface_sidebar();
        Self {
            hover: Colors::composite(base, Colors::state_hover()),
            selected: Colors::composite(base, Colors::state_selected()),
            selected_hover: Colors::composite(base, Colors::state_selected_hover()),
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn plugin_filter_sidebar(
    active: &PickerFilter,
    counts: &FilterCounts,
    vendors: &[String],
    categories: &[String],
    vendors_expanded: bool,
    debug_mode: bool,
    au_available: bool,
    filter_cb: FilterCb,
    on_toggle_vendors: VoidCb,
    sidebar_scroll: &ScrollHandle,
) -> impl IntoElement {
    let paint = RailPaint::resolve();
    let item = |id: gpui::ElementId, label: String, count: usize, value: PickerFilter| {
        rail_item(
            id,
            label,
            count,
            active == &value,
            paint,
            filter_cb.clone(),
            value,
        )
    };

    let mut col = div()
        .flex()
        .flex_col()
        .w_full()
        .px(px(space::TIGHT))
        .pb(px(space::BASE));

    col = col
        .child(rail_heading("Collections"))
        .child(item(
            "pp-filter-all".into(),
            "All Plug-ins".into(),
            counts.all,
            PickerFilter::All,
        ))
        .child(item(
            "pp-filter-fav".into(),
            "Favorites".into(),
            counts.favorites,
            PickerFilter::Favorites,
        ))
        .child(item(
            "pp-filter-recent".into(),
            "Recently Used".into(),
            counts.recent,
            PickerFilter::RecentlyUsed,
        ))
        .child(item(
            "pp-filter-builtin".into(),
            "Built-in".into(),
            counts.builtin,
            PickerFilter::Builtin,
        ));

    col = col.child(rail_heading("Format"));
    for (id, label, count, format) in [
        ("pp-filter-vst3", "VST3", counts.vst3, PluginFormat::Vst3),
        ("pp-filter-clap", "CLAP", counts.clap, PluginFormat::Clap),
        ("pp-filter-vst2", "VST2", counts.vst2, PluginFormat::Vst2),
    ] {
        // A format with nothing installed is noise on a rail meant for
        // narrowing a real library. The one that is selected stays, so the
        // rail never loses the entry the list is showing.
        let value = PickerFilter::Format(format);
        if count > 0 || active == &value {
            col = col.child(item(id.into(), label.into(), count, value));
        }
    }
    if au_available {
        col = col.child(item(
            "pp-filter-au".into(),
            if cfg!(target_os = "macos") {
                "Audio Units".into()
            } else {
                "Audio Units (Unavailable)".into()
            },
            counts.au,
            PickerFilter::Format(PluginFormat::Au),
        ));
    }

    if !categories.is_empty() {
        col = col.child(rail_heading("Category"));
        for (i, category) in categories.iter().enumerate() {
            let count = counts.categories.get(i).copied().unwrap_or(0);
            let value = PickerFilter::Category(category.clone());
            let selected =
                matches!(active, PickerFilter::Category(c) if c.eq_ignore_ascii_case(category));
            if count == 0 && !selected {
                continue;
            }
            col = col.child(rail_item(
                ("pp-filter-cat", i).into(),
                category.clone(),
                count,
                selected,
                paint,
                filter_cb.clone(),
                value,
            ));
        }
    }

    if !vendors.is_empty() {
        // An active vendor keeps the section open: folding away the entry
        // that explains the list would leave nothing saying why it is short.
        let vendor_active = matches!(active, PickerFilter::Vendor(_));
        let open = vendors_expanded || vendor_active;
        let present = counts.vendors.iter().filter(|&&n| n > 0).count();
        col = col.child(fold_heading(
            "Vendor",
            present,
            open,
            paint.hover,
            on_toggle_vendors,
        ));
        if open {
            for (i, vendor) in vendors.iter().enumerate() {
                let count = counts.vendors.get(i).copied().unwrap_or(0);
                let selected =
                    matches!(active, PickerFilter::Vendor(v) if v.eq_ignore_ascii_case(vendor));
                if count == 0 && !selected {
                    continue;
                }
                col = col.child(rail_item(
                    ("pp-filter-vendor", i).into(),
                    vendor.clone(),
                    count,
                    selected,
                    paint,
                    filter_cb.clone(),
                    PickerFilter::Vendor(vendor.clone()),
                ));
            }
        }
    }

    if debug_mode && counts.failed > 0 {
        col = col.child(rail_heading("Debug")).child(item(
            "pp-filter-failed".into(),
            "Failed / Missing".into(),
            counts.failed,
            PickerFilter::Failed,
        ));
    }

    let thumb_scroll = sidebar_scroll.clone();

    div()
        .flex()
        .flex_col()
        .w(px(SIDEBAR_WIDTH))
        .h_full()
        .flex_shrink_0()
        .border_r(px(1.0))
        .border_color(Colors::divider())
        .bg(Colors::surface_sidebar())
        .child(
            div()
                .flex_1()
                .min_h(px(0.0))
                .relative()
                .child(
                    div()
                        .id("plugin-picker-sidebar-scroll")
                        .size_full()
                        .overflow_y_scroll()
                        .track_scroll(sidebar_scroll)
                        .child(col),
                )
                .child(vertical_scrollbar_thumb(thumb_scroll)),
        )
}

fn rail_item(
    id: gpui::ElementId,
    label: String,
    count: usize,
    active: bool,
    paint: RailPaint,
    cb: FilterCb,
    value: PickerFilter,
) -> impl IntoElement {
    let (rest, hover) = if active {
        (paint.selected, paint.selected_hover)
    } else {
        (Colors::with_alpha(paint.hover, 0.0), paint.hover)
    };
    div()
        .id(id)
        .relative()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::BASE))
        .w_full()
        .h(px(size::ROW))
        .px(px(space::BASE))
        .rounded(px(radius::CONTROL))
        .bg(rest)
        .hover(move |s| s.bg(hover))
        .cursor(gpui::CursorStyle::PointingHand)
        .on_click(move |_, window, cx| cb(&value, window, cx))
        .when(active, |el| {
            el.child(
                div()
                    .absolute()
                    .left_0()
                    .top(px(space::SNUG))
                    .bottom(px(space::SNUG))
                    .w(px(2.0))
                    .rounded(px(radius::PILL))
                    .bg(Colors::accent_primary()),
            )
        })
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .truncate()
                .text_size(px(typography::UI_XS))
                .font_weight(if active {
                    gpui::FontWeight::SEMIBOLD
                } else {
                    gpui::FontWeight::NORMAL
                })
                .text_color(if active {
                    Colors::text_primary()
                } else {
                    Colors::text_secondary()
                })
                .child(label),
        )
        .child(
            div()
                .flex_shrink_0()
                .text_size(px(typography::DENSE_LABEL))
                .text_color(if active {
                    Colors::text_secondary()
                } else {
                    Colors::text_faint()
                })
                .child(count.to_string()),
        )
}

fn rail_heading(label: &'static str) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .h(px(size::ROW))
        .px(px(space::BASE))
        .mt(px(space::SNUG))
        .text_size(px(typography::DENSE_CAPTION))
        .font_weight(gpui::FontWeight::BOLD)
        .text_color(Colors::text_faint())
        .child(label.to_uppercase())
}

/// A rail heading that folds its section. The count says how much is inside
/// while it is folded, so a closed section is not a mystery.
fn fold_heading(
    label: &'static str,
    count: usize,
    open: bool,
    hover: Rgba,
    on_toggle: VoidCb,
) -> impl IntoElement {
    div()
        .id(gpui::SharedString::from(format!("pp-fold-{label}")))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::TIGHT))
        .h(px(size::ROW))
        .px(px(space::BASE))
        .mt(px(space::SNUG))
        .rounded(px(radius::CONTROL))
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(move |s| s.bg(hover))
        .on_click(move |_, window, cx| on_toggle(&(), window, cx))
        .child(
            svg()
                .path(if open {
                    assets::ICON_CHEVRON_DOWN_PATH
                } else {
                    assets::ICON_CHEVRON_RIGHT_PATH
                })
                .w(px(10.0))
                .h(px(10.0))
                .text_color(Colors::text_faint()),
        )
        .child(
            div()
                .flex_1()
                .text_size(px(typography::DENSE_CAPTION))
                .font_weight(gpui::FontWeight::BOLD)
                .text_color(Colors::text_faint())
                .child(label.to_uppercase()),
        )
        .child(
            div()
                .text_size(px(typography::DENSE_LABEL))
                .text_color(Colors::text_faint())
                .child(count.to_string()),
        )
}

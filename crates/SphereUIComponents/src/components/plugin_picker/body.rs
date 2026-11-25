//! The insert picker body, shared by the Add Insert window and the overlay.
//!
//! ```txt
//! destination + kind tabs
//! search + source chips
//! category chips
//! rail │ list (recent tiles + rows) │ details
//! hints + Cancel / Add <name>
//! ```
//!
//! Rows are two lines — name over "vendor · format · category" — so nothing
//! needs a column of its own, and a built-in says "Built-in" once instead of
//! carrying a badge. Kind is the tab row, sources and categories are chips,
//! and the rail keeps the collections and vendors; all of them compose.

use std::sync::Arc;

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, svg, uniform_list, App, AppContext, ClickEvent, InteractiveElement, IntoElement,
    ParentElement, Rgba, ScrollStrategy, StatefulInteractiveElement, Styled, Window,
};

use super::category::{normalize_category, NormalizedCategory};
use super::filter::{compute_filter_result, picker_perf_debug, FilterCounts, FilterResult};
use super::insert::{is_insertable, validate_insert, InsertValidation, PluginInsertKind};
use super::list_view::{scan_status_label, skeleton_body, PluginDragItem};
use super::overlay::{empty_state_body, notice_banner};
use super::prefs::PluginPickerPrefs;
use super::search_index::PluginSearchIndex;
use super::state::{
    CatalogStatus, KindTab, PickerFacet, PickerFilter, PluginPickerScrollHandles,
    PluginPickerState, PluginSource,
};
use super::{PluginDragPreview, PluginPickerCallbacks};
use crate::assets;
use crate::components::controls::{
    fb_badge, fb_button, fb_section_header, fb_segment, fb_segmented_track, fb_tooltip,
    FbButtonKind, FbSegment,
};
use crate::components::scrollbar::scroll_handle_scrollbar;
use crate::components::text_input::{
    text_field_with_callbacks, TextInputCallbacks, TextInputState,
};
use crate::theme::{radius, size, space, state, typography, Colors};
use SpherePluginHost::{PluginFormat, PluginKind, RegistryPlugin};

const RAIL_W: f32 = 184.0;
const DETAILS_W: f32 = 264.0;
/// One list row: name over vendor · format · category.
pub const ROW_H: f32 = 40.0;
const TILE_H: f32 = 52.0;
const GLYPH_TILE: f32 = size::DEFAULT;
const RECENT_TILES: usize = 4;
const TILE_MIN_W: f32 = 136.0;

type StringCb = Arc<dyn Fn(&String, &mut Window, &mut App) + 'static>;
type FilterCb = Arc<dyn Fn(&PickerFilter, &mut Window, &mut App) + 'static>;
type FacetCb = Arc<dyn Fn(&PickerFacet, &mut Window, &mut App) + 'static>;

fn category_icon(category: NormalizedCategory) -> &'static str {
    use NormalizedCategory as C;
    match category {
        C::Eq => assets::ICON_SLIDERS_HORIZONTAL_PATH,
        C::Compressor | C::Dynamics => assets::ICON_GAUGE_PATH,
        C::Reverb => assets::ICON_SPARKLES_PATH,
        C::Delay => assets::ICON_REPEAT_PATH,
        C::Distortion => assets::ICON_AUDIO_LINES_PATH,
        C::Modulation => assets::ICON_ARROW_LEFT_RIGHT_PATH,
        C::Utility => assets::ICON_CPU_PATH,
        C::Analyzer => assets::ICON_SCAN_SEARCH_PATH,
        C::Instrument | C::Synth => assets::ICON_KEYBOARD_PATH,
        C::Sampler => assets::ICON_LAYERS_PATH,
        C::Drum => assets::ICON_DRUM_PATH,
        C::MidiFx => assets::ICON_LIST_MUSIC_PATH,
        C::Other => assets::ICON_PLUG_PATH,
    }
}

/// "Built-in" or "Vendor · VST3", then the category.
fn subtitle(plugin: &RegistryPlugin) -> String {
    let category = normalize_category(plugin).label();
    if plugin.is_builtin() {
        format!("Built-in · {category}")
    } else if plugin.vendor.is_empty() {
        format!("{} · {category}", plugin.format.label())
    } else {
        format!("{} · {} · {category}", plugin.vendor, plugin.format.label())
    }
}

fn track_label(state: &PluginPickerState) -> String {
    if state.insert_target.track_name.is_empty() {
        "Selected track".to_string()
    } else {
        state.insert_target.track_name.clone()
    }
}

fn slot_label(state: &PluginPickerState) -> String {
    match state.insert_target.desired_kind {
        PluginInsertKind::Instrument => "Instrument slot".to_string(),
        PluginInsertKind::Effect => format!("Insert {}", state.insert_target.next_slot_index + 1),
    }
}

/// The plug-in's category glyph on a raised tile. Instruments take the
/// keyboard family of glyphs, so kind still reads by shape.
fn glyph_tile(plugin: &RegistryPlugin, edge: f32, available: bool) -> impl IntoElement {
    let glyph = edge * 0.5;
    let tint = if !available {
        Colors::text_disabled()
    } else if plugin.kind == PluginKind::Instrument {
        Colors::text_primary()
    } else {
        Colors::text_secondary()
    };
    div()
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .w(px(edge))
        .h(px(edge))
        .rounded(px(if edge >= size::PROMINENT {
            radius::CONTROL
        } else {
            radius::CONTROL_SM
        }))
        .bg(Colors::surface_raised())
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .child(
            svg()
                .path(category_icon(normalize_category(plugin)))
                .w(px(glyph))
                .h(px(glyph))
                .text_color(tint),
        )
}

fn drag_item(plugin: &RegistryPlugin) -> PluginDragItem {
    PluginDragItem {
        plugin_id: plugin.id.clone(),
        label: plugin.name.clone(),
        kind: plugin.kind,
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn picker_body(
    state: &PluginPickerState,
    index: Option<Arc<PluginSearchIndex>>,
    prefs: &PluginPickerPrefs,
    catalog_status: CatalogStatus,
    search_input: &TextInputState,
    search_focused: bool,
    search_callbacks: TextInputCallbacks,
    callbacks: PluginPickerCallbacks,
    au_scan_error: Option<&str>,
    scroll: &PluginPickerScrollHandles,
) -> impl IntoElement {
    let debug = picker_perf_debug();
    let index = index.unwrap_or_else(|| Arc::new(PluginSearchIndex::from_plugins(Vec::new())));
    let result = compute_filter_result(&index, &state.query, &state.filters, prefs, debug);
    let visible = result.indices.len();
    let highlighted = state.highlighted_index.min(visible.saturating_sub(1));
    let selected = result
        .indices
        .get(highlighted)
        .copied()
        .and_then(|i| index.plugin_at(i));
    let validation = selected
        .map(|p| validate_insert(p, &state.insert_target))
        .unwrap_or(InsertValidation::NotInsertable);

    // Keep the keyboard highlight in view — only when it moves, so the wheel
    // can still scroll away from it.
    let reveal = (highlighted, visible);
    if visible > 0 && scroll.revealed_row.get() != Some(reveal) {
        scroll.revealed_row.set(Some(reveal));
        scroll
            .list
            .scroll_to_item(highlighted, ScrollStrategy::Nearest);
    }

    // Recent tiles only on the unfiltered view, where they are a shortcut
    // rather than a second copy of a narrowed list.
    let unfiltered = state.query.trim().is_empty()
        && state.filters.sidebar == PickerFilter::All
        && state.filters.source.is_none()
        && state.filters.category.is_none();
    let recents: Vec<&RegistryPlugin> = if unfiltered {
        prefs
            .recent
            .iter()
            .filter_map(|id| index.plugins().iter().find(|p| &p.id == id))
            .filter(|p| state.filters.kind.matches(p))
            .take(RECENT_TILES)
            .collect()
    } else {
        Vec::new()
    };

    div()
        .flex()
        .flex_col()
        .size_full()
        .min_h(px(0.0))
        .bg(Colors::surface_window())
        .child(header(
            state,
            &result.counts,
            callbacks.on_select_kind.clone(),
        ))
        .child(search_row(
            search_input,
            search_focused,
            search_callbacks,
            state,
            &result.counts,
            callbacks.on_select_facet.clone(),
        ))
        .child(category_chips(
            state,
            &result,
            callbacks.on_select_facet.clone(),
        ))
        .when_some(au_scan_error, |panel, message| {
            panel.child(notice_banner(format!(
                "Audio Unit scan failed — VST3 and CLAP plug-ins are still available. {message}"
            )))
        })
        .child(
            div()
                .flex()
                .flex_row()
                .flex_1()
                .min_h(px(0.0))
                .border_t(px(1.0))
                .border_color(Colors::divider())
                .child(rail(
                    state,
                    &result,
                    debug,
                    callbacks.on_select_filter.clone(),
                    scroll,
                ))
                .child(list_column(
                    &recents,
                    &index,
                    Arc::new(result.indices.clone()),
                    highlighted,
                    prefs,
                    &callbacks,
                    scroll,
                    state,
                    catalog_status,
                    au_scan_error,
                ))
                .child(details(
                    selected,
                    &validation,
                    state,
                    prefs,
                    callbacks.on_toggle_favorite.clone(),
                )),
        )
        .child(footer(
            selected,
            &validation,
            visible,
            callbacks.on_pick.clone(),
            callbacks.on_close.clone(),
        ))
}

fn header(
    state: &PluginPickerState,
    counts: &FilterCounts,
    on_select_kind: Arc<dyn Fn(&KindTab, &mut Window, &mut App) + 'static>,
) -> impl IntoElement {
    let active = state.filters.kind;
    let tab = |id: &'static str, label: &str, count: usize, kind: KindTab, pos: FbSegment| {
        let cb = on_select_kind.clone();
        fb_segment(
            id,
            format!("{label}  {count}"),
            active == kind,
            pos,
            move |_, w, cx| cb(&kind, w, cx),
        )
    };
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(space::LOOSE))
        .flex_shrink_0()
        .h(px(44.0))
        .px(px(space::SECTION))
        .bg(Colors::surface_panel())
        .child(
            div()
                .flex()
                .flex_row()
                .items_baseline()
                .gap(px(space::SNUG))
                .min_w(px(0.0))
                .child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(typography::UI_TITLE))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(Colors::text_primary())
                        .child("Add plug-in"),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(typography::UI_SM))
                        .text_color(Colors::text_muted())
                        .child("to"),
                )
                .child(
                    div()
                        .min_w(px(0.0))
                        .truncate()
                        .text_size(px(typography::UI_SM))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(Colors::text_secondary())
                        .child(track_label(state)),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(typography::UI_XS))
                        .text_color(Colors::text_faint())
                        .child(format!("· {}", slot_label(state))),
                ),
        )
        .child(
            div().w(px(320.0)).flex_shrink_0().child(
                fb_segmented_track()
                    .child(tab(
                        "pp-kind-fx",
                        "Effects",
                        counts.effects,
                        KindTab::Effects,
                        FbSegment::First,
                    ))
                    .child(tab(
                        "pp-kind-inst",
                        "Instruments",
                        counts.instruments,
                        KindTab::Instruments,
                        FbSegment::Middle,
                    ))
                    .child(tab(
                        "pp-kind-all",
                        "All",
                        counts.library,
                        KindTab::All,
                        FbSegment::Last,
                    )),
            ),
        )
}

/// A quiet toggle chip: rest is ghost with a hairline, on is the selected
/// state layer plus a leading check, so "on" is never hue alone.
fn chip(
    id: impl Into<gpui::ElementId>,
    label: String,
    count: Option<usize>,
    on: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let base = Colors::surface_panel();
    let rest = if on {
        Colors::composite(base, Colors::state_selected())
    } else {
        base
    };
    let hover = Colors::composite(rest, Colors::state_hover());
    div()
        .id(id)
        .flex()
        .flex_row()
        .flex_shrink_0()
        .items_center()
        .gap(px(space::TIGHT))
        .h(px(size::DENSE))
        .px(px(space::BASE))
        .rounded(px(radius::CONTROL_SM))
        .bg(rest)
        .border(px(1.0))
        .border_color(if on {
            Colors::with_alpha(Colors::accent_primary(), 0.55)
        } else {
            Colors::border_subtle()
        })
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(move |s| s.bg(hover))
        .on_click(on_click)
        .when(on, |el| {
            el.child(
                svg()
                    .path(assets::ICON_CHECK_PATH)
                    .w(px(10.0))
                    .h(px(10.0))
                    .text_color(Colors::accent_primary()),
            )
        })
        .child(
            div()
                .text_size(px(typography::UI_XS))
                .font_weight(if on {
                    gpui::FontWeight::SEMIBOLD
                } else {
                    gpui::FontWeight::NORMAL
                })
                .text_color(if on {
                    Colors::text_primary()
                } else {
                    Colors::text_secondary()
                })
                .child(label),
        )
        .when_some(count, |el, n| {
            el.child(
                div()
                    .text_size(px(typography::DENSE_LABEL))
                    .text_color(Colors::text_faint())
                    .child(n.to_string()),
            )
        })
}

/// The sources this library has under the current tab, plus the active one so
/// it can always be cleared.
fn offered_sources(counts: &FilterCounts, active: Option<PluginSource>) -> Vec<PluginSource> {
    [
        (PluginSource::Builtin, counts.builtin),
        (PluginSource::Format(PluginFormat::Vst3), counts.vst3),
        (PluginSource::Format(PluginFormat::Clap), counts.clap),
        (PluginSource::Format(PluginFormat::Vst2), counts.vst2),
        (PluginSource::Format(PluginFormat::Au), counts.au),
    ]
    .into_iter()
    .filter(|(source, n)| *n > 0 || active == Some(*source))
    .map(|(source, _)| source)
    .collect()
}

fn search_row(
    search_input: &TextInputState,
    search_focused: bool,
    search_callbacks: TextInputCallbacks,
    state: &PluginPickerState,
    counts: &FilterCounts,
    on_facet: FacetCb,
) -> impl IntoElement {
    let active = state.filters.source;
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::BASE))
        .flex_shrink_0()
        .px(px(space::SECTION))
        .pb(px(space::BASE))
        .bg(Colors::surface_panel())
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .child(text_field_with_callbacks(
                    search_input,
                    search_focused,
                    search_callbacks,
                )),
        )
        .children(
            offered_sources(counts, active)
                .into_iter()
                .enumerate()
                .map(|(i, source)| {
                    let on = active == Some(source);
                    let cb = on_facet.clone();
                    chip(
                        ("pp-source", i),
                        source.label().to_string(),
                        None,
                        on,
                        move |_, w, cx| cb(&PickerFacet::Source((!on).then_some(source)), w, cx),
                    )
                }),
        )
}

fn category_chips(
    state: &PluginPickerState,
    result: &FilterResult,
    on_facet: FacetCb,
) -> impl IntoElement {
    let active = state.filters.category.as_deref();
    let is_active = |label: &str| active.is_some_and(|a| a.eq_ignore_ascii_case(label));
    // Most-used categories lead; "Other" always trails. A chip that would
    // list nothing is left out unless it is the active one.
    let mut ordered: Vec<(&str, usize)> = result
        .categories
        .iter()
        .zip(result.counts.category_facets.iter())
        .map(|(label, n)| (label.as_str(), *n))
        .filter(|(label, n)| *n > 0 || is_active(label))
        .collect();
    ordered.sort_by(|a, b| {
        (a.0 == "Other")
            .cmp(&(b.0 == "Other"))
            .then(b.1.cmp(&a.1))
            .then(a.0.cmp(b.0))
    });
    let all_cb = on_facet.clone();
    div()
        .flex()
        .flex_row()
        .flex_wrap()
        .items_center()
        .gap(px(space::SNUG))
        .flex_shrink_0()
        .px(px(space::SECTION))
        .pb(px(space::LOOSE))
        .bg(Colors::surface_panel())
        .child(chip(
            "pp-cat-all",
            "All".into(),
            Some(result.counts.category_facet_total),
            active.is_none(),
            move |_, w, cx| all_cb(&PickerFacet::Category(None), w, cx),
        ))
        .children(ordered.into_iter().enumerate().map(|(i, (label, n))| {
            let on = is_active(label);
            let cb = on_facet.clone();
            let value = (!on).then(|| label.to_string());
            chip(
                ("pp-cat", i),
                label.to_string(),
                Some(n),
                on,
                move |_, w, cx| cb(&PickerFacet::Category(value.clone()), w, cx),
            )
        }))
}

#[allow(clippy::too_many_arguments)]
fn rail_row(
    id: impl Into<gpui::ElementId>,
    icon: &'static str,
    label: String,
    count: usize,
    on: bool,
    filter: PickerFilter,
    on_select: FilterCb,
) -> impl IntoElement {
    let base = Colors::surface_panel();
    let rest = if on {
        Colors::composite(base, Colors::state_selected())
    } else {
        base
    };
    let hover = Colors::composite(rest, Colors::state_hover());
    div()
        .id(id)
        .relative()
        .flex()
        .flex_row()
        .flex_shrink_0()
        .items_center()
        .gap(px(space::BASE))
        .h(px(size::DEFAULT))
        .px(px(space::BASE))
        .rounded(px(radius::CONTROL_SM))
        .bg(rest)
        .hover(move |s| s.bg(hover))
        .cursor(gpui::CursorStyle::PointingHand)
        .on_click(move |_, w, cx| on_select(&filter, w, cx))
        .when(on, |el| {
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
            svg()
                .path(icon)
                .w(px(12.0))
                .h(px(12.0))
                .flex_shrink_0()
                .text_color(if on {
                    Colors::text_primary()
                } else {
                    Colors::text_muted()
                }),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .truncate()
                .text_size(px(typography::UI_XS))
                .font_weight(if on {
                    gpui::FontWeight::SEMIBOLD
                } else {
                    gpui::FontWeight::NORMAL
                })
                .text_color(if on {
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
                .text_color(Colors::text_faint())
                .child(count.to_string()),
        )
}

fn rail(
    state: &PluginPickerState,
    result: &FilterResult,
    debug: bool,
    on_select: FilterCb,
    scroll: &PluginPickerScrollHandles,
) -> impl IntoElement {
    let active = &state.filters.sidebar;
    let counts = &result.counts;
    let row = |id: &'static str, icon, label: &str, count, filter: PickerFilter| {
        rail_row(
            id,
            icon,
            label.to_string(),
            count,
            *active == filter,
            filter,
            on_select.clone(),
        )
    };
    let mut col = div()
        .flex()
        .flex_col()
        .gap(px(space::HAIR))
        .px(px(space::BASE))
        .py(px(space::LOOSE))
        .child(row(
            "pp-rail-all",
            assets::ICON_LAYERS_PATH,
            "All plug-ins",
            counts.all,
            PickerFilter::All,
        ))
        .child(row(
            "pp-rail-fav",
            assets::ICON_STAR_PATH,
            "Favorites",
            counts.favorites,
            PickerFilter::Favorites,
        ))
        .child(row(
            "pp-rail-recent",
            assets::ICON_CLOCK_PATH,
            "Recently used",
            counts.recent,
            PickerFilter::RecentlyUsed,
        ))
        .when(debug && counts.failed > 0, |col| {
            col.child(row(
                "pp-rail-failed",
                assets::ICON_PLUG_PATH,
                "Failed / Missing",
                counts.failed,
                PickerFilter::Failed,
            ))
        });

    let vendors: Vec<(usize, &String, usize, bool)> = result
        .vendors
        .iter()
        .zip(counts.vendors.iter())
        .enumerate()
        .map(|(i, (vendor, n))| {
            let on = matches!(active, PickerFilter::Vendor(v) if v.eq_ignore_ascii_case(vendor));
            (i, vendor, *n, on)
        })
        .filter(|(_, _, n, on)| *n > 0 || *on)
        .collect();
    if !vendors.is_empty() {
        col = col
            .child(div().h(px(space::LOOSE)))
            .child(
                div()
                    .px(px(space::TIGHT))
                    .child(fb_section_header("VENDORS")),
            )
            .children(vendors.into_iter().map(|(i, vendor, n, on)| {
                rail_row(
                    ("pp-rail-vendor", i),
                    assets::ICON_USER_PATH,
                    vendor.clone(),
                    n,
                    on,
                    PickerFilter::Vendor(vendor.clone()),
                    on_select.clone(),
                )
            }));
    }

    div()
        .relative()
        .flex_shrink_0()
        .w(px(RAIL_W))
        .h_full()
        .bg(Colors::surface_panel())
        .border_r(px(1.0))
        .border_color(Colors::divider())
        .child(
            div()
                .id("pp-rail-scroll")
                .size_full()
                .overflow_y_scroll()
                .track_scroll(&scroll.sidebar)
                .child(col),
        )
        .children(scroll_handle_scrollbar(
            "pp-rail-scrollbar",
            &scroll.sidebar,
        ))
}

fn recent_tile(
    i: usize,
    plugin: &RegistryPlugin,
    on_select: StringCb,
    on_pick: StringCb,
) -> impl IntoElement {
    let base = Colors::surface_panel();
    let hover = Colors::composite(base, Colors::state_hover());
    let insertable = is_insertable(plugin);
    let id = plugin.id.clone();
    div()
        .id(("pp-recent-tile", i))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::BASE))
        .flex_1()
        .min_w(px(TILE_MIN_W))
        .h(px(TILE_H))
        .px(px(space::BASE))
        .rounded(px(radius::CONTROL))
        .bg(base)
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .hover(move |s| s.bg(hover))
        .cursor(gpui::CursorStyle::PointingHand)
        .when(insertable, |el| {
            el.on_drag(drag_item(plugin), |drag, _offset, _window, cx| {
                cx.new(|_| PluginDragPreview {
                    label: drag.label.clone(),
                })
            })
        })
        .on_click(move |e, w, cx| {
            if e.click_count() >= 2 {
                if insertable {
                    on_pick(&id, w, cx);
                }
            } else {
                on_select(&id, w, cx);
            }
        })
        .child(glyph_tile(plugin, size::PROMINENT, insertable))
        .child(
            div()
                .flex()
                .flex_col()
                .min_w(px(0.0))
                .child(
                    div()
                        .truncate()
                        .text_size(px(typography::UI_SM))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(Colors::text_primary())
                        .child(plugin.name.clone()),
                )
                .child(
                    div()
                        .truncate()
                        .text_size(px(typography::DENSE_LABEL))
                        .text_color(Colors::text_faint())
                        .child(normalize_category(plugin).label()),
                ),
        )
}

#[allow(clippy::too_many_arguments)]
fn list_column(
    recents: &[&RegistryPlugin],
    index: &Arc<PluginSearchIndex>,
    indices: Arc<Vec<usize>>,
    highlighted: usize,
    prefs: &PluginPickerPrefs,
    callbacks: &PluginPickerCallbacks,
    scroll: &PluginPickerScrollHandles,
    state: &PluginPickerState,
    catalog_status: CatalogStatus,
    au_scan_error: Option<&str>,
) -> impl IntoElement {
    let count = indices.len();
    let heading = match state.filters.kind {
        KindTab::Effects => "EFFECTS",
        KindTab::Instruments => "INSTRUMENTS",
        KindTab::All => "ALL PLUG-INS",
    };
    let body = if matches!(catalog_status, CatalogStatus::Loading) {
        skeleton_body().into_any_element()
    } else if count == 0 {
        empty_state_body(catalog_status, index.len(), state, au_scan_error, callbacks)
            .into_any_element()
    } else {
        let index = index.clone();
        let favorites = Arc::new(prefs.favorites.clone());
        let on_select = callbacks.on_select.clone();
        let on_pick = callbacks.on_pick.clone();
        let on_fav = callbacks.on_toggle_favorite.clone();
        let base = Colors::surface_base();
        let paint = RowPaint {
            rest: base,
            hover: Colors::composite(base, Colors::state_hover()),
            selected: Colors::composite(base, Colors::state_selected()),
        };
        let thumb = scroll.list.0.borrow().base_handle.clone();
        let list = uniform_list("pp-list", count, move |range, _w, _cx| {
            range
                .filter_map(|i| {
                    let plugin = index.plugin_at(*indices.get(i)?)?;
                    Some(
                        row(
                            i,
                            plugin,
                            i == highlighted,
                            favorites.contains(&plugin.id),
                            paint,
                            on_select.clone(),
                            on_pick.clone(),
                            on_fav.clone(),
                        )
                        .into_any_element(),
                    )
                })
                .collect::<Vec<_>>()
        })
        .track_scroll(&scroll.list)
        .size_full();
        div()
            .relative()
            .size_full()
            .child(list)
            .children(scroll_handle_scrollbar("pp-list-scrollbar", &thumb))
            .into_any_element()
    };

    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w(px(0.0))
        .bg(Colors::surface_base())
        .when(!recents.is_empty(), |col| {
            col.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(space::SNUG))
                    .flex_shrink_0()
                    .px(px(space::LOOSE))
                    .pt(px(space::LOOSE))
                    .pb(px(space::SNUG))
                    .child(fb_section_header("RECENTLY USED"))
                    // Tiles keep a readable width: the ones that do not fit
                    // wrap onto a second line, which is clipped away, so a
                    // narrow window shows fewer tiles instead of squeezed ones.
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .gap(px(space::SNUG))
                            .h(px(TILE_H))
                            .overflow_hidden()
                            .children(recents.iter().enumerate().map(|(i, p)| {
                                recent_tile(
                                    i,
                                    p,
                                    callbacks.on_select.clone(),
                                    callbacks.on_pick.clone(),
                                )
                            })),
                    ),
            )
        })
        .child(
            div()
                .flex_shrink_0()
                .px(px(space::LOOSE))
                .pt(px(space::SNUG))
                .pb(px(space::TIGHT))
                .child(fb_section_header(format!("{heading} · {count}"))),
        )
        .child(div().flex_1().min_h(px(0.0)).child(body))
}

/// Row paint resolved once per render: `Colors::composite` is a control-path
/// helper, so the list builds this before materialising any row.
#[derive(Clone, Copy)]
struct RowPaint {
    rest: Rgba,
    hover: Rgba,
    selected: Rgba,
}

#[allow(clippy::too_many_arguments)]
fn row(
    i: usize,
    plugin: &RegistryPlugin,
    highlighted: bool,
    favorite: bool,
    paint: RowPaint,
    on_select: StringCb,
    on_pick: StringCb,
    on_toggle_favorite: StringCb,
) -> impl IntoElement {
    let insertable = is_insertable(plugin);
    let (rest, hover) = if highlighted {
        (paint.selected, paint.selected)
    } else {
        (paint.rest, paint.hover)
    };
    let id_select = plugin.id.clone();
    let id_pick = plugin.id.clone();
    let dim = |c: Rgba| {
        if insertable {
            c
        } else {
            Colors::with_alpha(c, state::DISABLED_CONTENT + 0.2)
        }
    };
    div()
        .id(("pp-row", i))
        .relative()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::BASE))
        .w_full()
        .h(px(ROW_H))
        .px(px(space::LOOSE))
        .bg(rest)
        .hover(move |s| s.bg(hover))
        .cursor(if insertable {
            gpui::CursorStyle::PointingHand
        } else {
            gpui::CursorStyle::Arrow
        })
        .when(insertable, |el| {
            el.on_drag(drag_item(plugin), |drag, _offset, _window, cx| {
                cx.new(|_| PluginDragPreview {
                    label: drag.label.clone(),
                })
            })
        })
        .on_click(move |e, w, cx| {
            if e.click_count() >= 2 {
                if insertable {
                    on_pick(&id_pick, w, cx);
                }
            } else {
                on_select(&id_select, w, cx);
            }
        })
        .when(highlighted, |el| {
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
        .child(glyph_tile(plugin, GLYPH_TILE, insertable))
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(0.0))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(space::SNUG))
                        .min_w(px(0.0))
                        .child(
                            div()
                                .min_w(px(0.0))
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
                        .when_some(scan_status_label(plugin), |el, label| {
                            el.child(
                                div()
                                    .flex_shrink_0()
                                    .child(fb_badge(label, Colors::status_warning())),
                            )
                        }),
                )
                .child(
                    div()
                        .truncate()
                        .text_size(px(typography::DENSE_LABEL))
                        .text_color(dim(Colors::text_faint()))
                        .child(subtitle(plugin)),
                ),
        )
        .child(favorite_star(
            i,
            favorite,
            paint.hover,
            plugin.id.clone(),
            on_toggle_favorite,
        ))
}

fn favorite_star(
    i: usize,
    favorite: bool,
    hover: Rgba,
    plugin_id: String,
    on_toggle: StringCb,
) -> impl IntoElement {
    div()
        .id(("pp-star", i))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .w(px(size::DENSE))
        .h(px(size::DENSE))
        .rounded(px(radius::CONTROL_SM))
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(move |s| s.bg(hover))
        .tooltip(fb_tooltip(if favorite {
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
                .w(px(12.0))
                .h(px(12.0))
                .text_color(if favorite {
                    Colors::status_warning()
                } else {
                    Colors::with_alpha(Colors::text_faint(), 0.5)
                }),
        )
}

fn detail_line(label: &'static str, value: String) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(space::BASE))
        .h(px(size::DENSE))
        .child(
            div()
                .flex_shrink_0()
                .text_size(px(typography::UI_XS))
                .text_color(Colors::text_faint())
                .child(label),
        )
        .child(
            div()
                .min_w(px(0.0))
                .truncate()
                .text_size(px(typography::UI_XS))
                .text_color(Colors::text_secondary())
                .child(value),
        )
}

fn details(
    selected: Option<&RegistryPlugin>,
    validation: &InsertValidation,
    state: &PluginPickerState,
    prefs: &PluginPickerPrefs,
    on_toggle_favorite: StringCb,
) -> impl IntoElement {
    let pane = div()
        .flex()
        .flex_col()
        .flex_shrink_0()
        .w(px(DETAILS_W))
        .h_full()
        .gap(px(space::LOOSE))
        .p(px(space::SECTION))
        .bg(Colors::surface_panel())
        .border_l(px(1.0))
        .border_color(Colors::divider());
    let Some(plugin) = selected else {
        return pane.child(
            div()
                .text_size(px(typography::UI_XS))
                .text_color(Colors::text_faint())
                .child("Choose a plug-in to see where it goes."),
        );
    };
    let favorite = prefs.favorites.contains(&plugin.id);
    let vendor = if plugin.is_builtin() {
        "Futureboard · Built-in".to_string()
    } else {
        plugin.vendor.clone()
    };
    let plugin_id = plugin.id.clone();
    pane.child(
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(space::LOOSE))
            .child(glyph_tile(plugin, 44.0, is_insertable(plugin)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .min_w(px(0.0))
                    .child(
                        div()
                            .truncate()
                            .text_size(px(typography::UI_LG))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(Colors::text_primary())
                            .child(plugin.name.clone()),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(px(typography::UI_XS))
                            .text_color(Colors::text_muted())
                            .child(vendor),
                    ),
            ),
    )
    .child(
        div()
            .flex()
            .flex_col()
            .child(fb_section_header("PLUG-IN"))
            .child(detail_line("Kind", plugin.kind.label().to_string()))
            .child(detail_line(
                "Category",
                normalize_category(plugin).label().to_string(),
            ))
            .child(detail_line(
                "Format",
                if plugin.is_builtin() {
                    "Built-in".to_string()
                } else {
                    plugin.format.label().to_string()
                },
            ))
            .when_some(plugin.version.clone(), |el, v| {
                el.child(detail_line("Version", v))
            }),
    )
    .child(
        div()
            .flex()
            .flex_col()
            .child(fb_section_header("GOES TO"))
            .child(detail_line("Track", track_label(state)))
            .child(detail_line("Slot", slot_label(state))),
    )
    .when_some(validation.message(), |el, reason| {
        el.child(
            div()
                .px(px(space::BASE))
                .py(px(space::SNUG))
                .rounded(px(radius::CONTROL_SM))
                .bg(Colors::with_alpha(Colors::status_warning(), 0.10))
                .text_size(px(typography::UI_XS))
                .text_color(Colors::status_warning())
                .child(reason),
        )
    })
    .child(div().flex_1())
    .child(fb_button(
        "pp-favorite",
        if favorite {
            "★ Remove from Favorites"
        } else {
            "☆ Add to Favorites"
        },
        FbButtonKind::Default,
        true,
        move |_, w, cx| on_toggle_favorite(&plugin_id, w, cx),
    ))
}

fn footer(
    selected: Option<&RegistryPlugin>,
    validation: &InsertValidation,
    visible: usize,
    on_pick: StringCb,
    on_close: Arc<dyn Fn(&(), &mut Window, &mut App) + 'static>,
) -> impl IntoElement {
    let can_add = selected.is_some() && *validation == InsertValidation::Ok;
    let add_label = match selected {
        Some(p) => format!("Add {}", p.name),
        None => "Add".to_string(),
    };
    let selected_id = selected.map(|p| p.id.clone());
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(space::LOOSE))
        .flex_shrink_0()
        .h(px(52.0))
        .px(px(space::SECTION))
        .bg(Colors::surface_panel())
        .border_t(px(1.0))
        .border_color(Colors::divider())
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::LOOSE))
                .min_w(px(0.0))
                .text_size(px(typography::DENSE_LABEL))
                .text_color(Colors::text_faint())
                .child(if visible == 1 {
                    "1 shown".to_string()
                } else {
                    format!("{visible} shown")
                })
                .child("↑↓ browse")
                .child("Enter or double-click to add"),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::BASE))
                .flex_shrink_0()
                .child(fb_button(
                    "plugin-picker-cancel",
                    "Cancel",
                    FbButtonKind::Default,
                    true,
                    move |_, w, cx| on_close(&(), w, cx),
                ))
                .child(div().max_w(px(240.0)).child(fb_button(
                    "plugin-picker-add",
                    add_label,
                    FbButtonKind::Primary,
                    can_add,
                    move |_, w, cx| {
                        if let Some(id) = selected_id.as_ref() {
                            on_pick(id, w, cx);
                        }
                    },
                ))),
        )
}

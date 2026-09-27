//! Insert picker composition.
//!
//! One body, two hosts: the Add Insert window (`plugin_picker_panel`) and the
//! in-window fallback (`plugin_picker_overlay`). Top to bottom:
//!
//! ```txt
//! target + kind tabs      ← where the plug-in goes, and which kind
//! search                  ← focused on open; typing is the main way in
//! rail │ list             ← collections/format/category/vendor │ plug-ins
//! selection + actions     ← what Enter would insert, or why it cannot
//! ```

use std::sync::Arc;

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, svg, uniform_list, App, InteractiveElement, IntoElement, ParentElement,
    ScrollStrategy, Styled, UniformListScrollHandle, Window,
};

use crate::assets;
use crate::components::controls::{
    fb_button, fb_segment, fb_segmented_track, FbButtonKind, FbSegment,
};
use crate::components::plugin_format_badge::plugin_format_badge_for;
use crate::components::plugin_picker::filter::{compute_filter_result, FilterCounts};
use crate::components::plugin_picker::insert::{validate_insert, InsertValidation};
use crate::components::plugin_picker::list_view::{
    kind_glyph, plugin_row, plugin_table_header, skeleton_body, RowPaint, ROW_HEIGHT,
};
use crate::components::plugin_picker::prefs::PluginPickerPrefs;
use crate::components::plugin_picker::search_index::PluginSearchIndex;
use crate::components::plugin_picker::sidebar::plugin_filter_sidebar;
use crate::components::plugin_picker::state::{
    CatalogStatus, KindTab, PickerFilter, PluginPickerScrollHandles, PluginPickerState,
};
use crate::components::plugin_picker::PluginPickerCallbacks;
use crate::components::scroll_thumb::vertical_scrollbar_thumb;
use crate::components::text_input::{
    text_field_with_callbacks, TextInputCallbacks, TextInputState,
};
use crate::theme::{radius, size, space, typography, Colors};
use SpherePluginHost::RegistryPlugin;

type VoidCb = Arc<dyn Fn(&(), &mut Window, &mut App) + 'static>;

/// The picker drawn over the studio window, for hosts without the Add Insert
/// window. Same body; this only adds the modal frame and the click-away.
#[allow(clippy::too_many_arguments)]
pub fn plugin_picker_overlay(
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
    let close_backdrop = callbacks.on_close.clone();
    let modal_width = prefs.window_width.max(760.0);
    let modal_height = prefs.window_height.max(480.0);
    let body = picker_body(
        state,
        index,
        prefs,
        catalog_status,
        search_input,
        search_focused,
        search_callbacks,
        callbacks,
        au_scan_error,
        scroll,
    );

    div()
        .absolute()
        .top_0()
        .bottom_0()
        .left_0()
        .right_0()
        .flex()
        .items_start()
        .justify_center()
        .pt(px(space::PAGE + space::SECTION))
        .px(px(space::SECTION))
        .pb(px(space::BLOCK))
        .id("plugin-picker-overlay")
        .bg(Colors::state_scrim())
        .occlude()
        .on_mouse_down(gpui::MouseButton::Left, move |_, window, cx| {
            close_backdrop(&(), window, cx);
        })
        .child(
            div()
                .flex()
                .flex_col()
                .w(px(modal_width))
                .h(px(modal_height))
                .overflow_hidden()
                .rounded(px(radius::DIALOG))
                .border(px(1.0))
                .border_color(Colors::border_normal())
                .bg(Colors::surface_window())
                .shadow_xl()
                .on_mouse_down(gpui::MouseButton::Left, |_, _window, cx| {
                    cx.stop_propagation();
                })
                .child(body),
        )
}

/// The picker as the whole content of the Add Insert window.
#[allow(clippy::too_many_arguments)]
pub fn plugin_picker_panel(
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
    let on_drop_plugin = callbacks.on_drop_plugin.clone();
    div()
        .flex()
        .flex_col()
        .size_full()
        .overflow_hidden()
        .bg(Colors::surface_window())
        .on_drop::<crate::components::plugin_picker::PluginDragItem>(move |item, window, cx| {
            on_drop_plugin(item, window, cx)
        })
        .child(picker_body(
            state,
            index,
            prefs,
            catalog_status,
            search_input,
            search_focused,
            search_callbacks,
            callbacks,
            au_scan_error,
            scroll,
        ))
}

#[allow(clippy::too_many_arguments)]
fn picker_body(
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
    let debug = std::env::var_os("FUTUREBOARD_PLUGIN_PICKER_DEBUG").is_some();
    let index = index.unwrap_or_else(|| Arc::new(PluginSearchIndex::from_plugins(Vec::new())));
    let result = compute_filter_result(&index, &state.query, &state.filters, prefs, debug);
    let visible_count = result.indices.len();
    let total = index.len();

    let highlighted = state.highlighted_index.min(visible_count.saturating_sub(1));
    let selected_plugin = result
        .indices
        .get(highlighted)
        .copied()
        .and_then(|i| index.plugin_at(i));
    let validation = selected_plugin
        .map(|plugin| validate_insert(plugin, &state.insert_target))
        .unwrap_or(InsertValidation::NotInsertable);
    let selected_id = state
        .selected_id
        .clone()
        .or_else(|| selected_plugin.map(|p| p.id.clone()));

    // Keep the keyboard highlight in view — only when it moves, so the wheel
    // can still scroll away from it.
    let reveal = (highlighted, visible_count);
    if visible_count > 0 && scroll.revealed_row.get() != Some(reveal) {
        scroll.revealed_row.set(Some(reveal));
        scroll
            .list
            .scroll_to_item(highlighted, ScrollStrategy::Nearest);
    }

    let sidebar = plugin_filter_sidebar(
        &state.filters.sidebar,
        &result.counts,
        &result.vendors,
        &result.categories,
        state.vendors_expanded,
        debug,
        cfg!(target_os = "macos") || result.counts.au > 0,
        callbacks.on_select_filter.clone(),
        callbacks.on_toggle_vendors.clone(),
        &scroll.sidebar,
    );

    let list = build_list_body(
        catalog_status.clone(),
        total,
        state,
        index.clone(),
        Arc::new(result.indices),
        highlighted,
        &callbacks,
        prefs,
        au_scan_error,
        &scroll.list,
    );

    div()
        .flex()
        .flex_col()
        .size_full()
        .min_h(px(0.0))
        .child(target_bar(
            state,
            &result.counts,
            callbacks.on_select_kind.clone(),
        ))
        .child(search_bar(
            search_input,
            search_focused,
            search_callbacks,
            visible_count,
            &state.query,
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
                .w_full()
                .child(sidebar)
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w(px(0.0))
                        .bg(Colors::surface_base())
                        .child(plugin_table_header())
                        .child(div().flex_1().min_h(px(0.0)).w_full().child(list)),
                ),
        )
        .child(selection_bar(
            selected_plugin,
            &validation,
            selected_id,
            callbacks.on_pick.clone(),
            callbacks.on_close.clone(),
        ))
}

/// Where the plug-in is going, and the kind tabs.
fn target_bar(
    state: &PluginPickerState,
    counts: &FilterCounts,
    on_select_kind: Arc<dyn Fn(&KindTab, &mut Window, &mut App) + 'static>,
) -> impl IntoElement {
    let target = &state.insert_target;
    let slot = match target.desired_kind {
        crate::components::plugin_picker::PluginInsertKind::Instrument => {
            "Instrument slot".to_string()
        }
        crate::components::plugin_picker::PluginInsertKind::Effect => {
            format!("Insert slot {}", target.next_slot_index + 1)
        }
    };
    let active = state.filters.kind;
    let tab = |id: &'static str, label: &str, count: usize, kind: KindTab, position: FbSegment| {
        let cb = on_select_kind.clone();
        fb_segment(
            id,
            format!("{label}  {count}"),
            active == kind,
            position,
            move |_, window, cx| cb(&kind, window, cx),
        )
    };

    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(space::LOOSE))
        .flex_shrink_0()
        .h(px(size::PROMINENT + space::LOOSE))
        .px(px(space::LOOSE))
        .bg(Colors::surface_panel())
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::SNUG))
                .min_w(px(0.0))
                .child(
                    svg()
                        .path(assets::ICON_PLUG_PATH)
                        .w(px(13.0))
                        .h(px(13.0))
                        .text_color(Colors::text_muted()),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(typography::UI_XS))
                        .text_color(Colors::text_muted())
                        .child("Insert on"),
                )
                .child(
                    div()
                        .min_w(px(0.0))
                        .truncate()
                        .text_size(px(typography::UI_MD))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(Colors::text_primary())
                        .child(if target.track_name.is_empty() {
                            "Selected track".to_string()
                        } else {
                            target.track_name.clone()
                        }),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(typography::UI_XS))
                        .text_color(Colors::text_faint())
                        .child(format!("· {slot}")),
                ),
        )
        .child(
            div().w(px(340.0)).flex_shrink_0().child(
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

fn search_bar(
    search_input: &TextInputState,
    search_focused: bool,
    search_callbacks: TextInputCallbacks,
    visible_count: usize,
    query: &str,
) -> impl IntoElement {
    let result_label = if query.trim().is_empty() {
        format!("{visible_count} plug-ins")
    } else if visible_count == 1 {
        "1 match".to_string()
    } else {
        format!("{visible_count} matches")
    };
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::LOOSE))
        .flex_shrink_0()
        .px(px(space::LOOSE))
        .pb(px(space::LOOSE))
        .bg(Colors::surface_panel())
        .border_b(px(1.0))
        .border_color(Colors::divider())
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
        .child(
            div()
                .flex_shrink_0()
                .min_w(px(72.0))
                .text_size(px(typography::UI_XS))
                .text_color(Colors::text_faint())
                .text_align(gpui::TextAlign::Right)
                .child(result_label),
        )
}

fn notice_banner(message: String) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::SNUG))
        .flex_shrink_0()
        .px(px(space::LOOSE))
        .py(px(space::SNUG))
        .border_b(px(1.0))
        .border_color(Colors::divider())
        .bg(Colors::with_alpha(Colors::status_warning(), 0.08))
        .text_size(px(typography::DENSE_LABEL))
        .text_color(Colors::status_warning())
        .child(message)
}

#[allow(clippy::too_many_arguments)]
fn build_list_body(
    catalog_status: CatalogStatus,
    total: usize,
    state: &PluginPickerState,
    index: Arc<PluginSearchIndex>,
    indices: Arc<Vec<usize>>,
    highlighted: usize,
    callbacks: &PluginPickerCallbacks,
    prefs: &PluginPickerPrefs,
    au_scan_error: Option<&str>,
    list_scroll: &UniformListScrollHandle,
) -> gpui::AnyElement {
    if matches!(catalog_status, CatalogStatus::Loading) {
        return skeleton_body().into_any_element();
    }
    let visible_count = indices.len();
    if visible_count == 0 {
        return empty_state_body(catalog_status, total, state, au_scan_error, callbacks)
            .into_any_element();
    }
    let on_select = callbacks.on_select.clone();
    let on_pick = callbacks.on_pick.clone();
    let on_fav = callbacks.on_toggle_favorite.clone();
    let favorites = Arc::new(prefs.favorites.clone());
    let paint = RowPaint::resolve();
    let scroll_for_thumb = list_scroll.0.borrow().base_handle.clone();
    let list = uniform_list(
        "plugin-picker-list",
        visible_count,
        move |range, _window, _cx| {
            // Only the visible range is materialised, and each row resolves
            // its plug-in from the shared index — no per-row catalog clone.
            range
                .filter_map(|i| {
                    let plugin = index.plugin_at(*indices.get(i)?)?;
                    Some(
                        plugin_row(
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
        },
    )
    .track_scroll(list_scroll)
    .size_full();
    div()
        .relative()
        .size_full()
        .child(list)
        .child(vertical_scrollbar_thumb(scroll_for_thumb))
        .into_any_element()
}

fn empty_state_body(
    catalog_status: CatalogStatus,
    total: usize,
    state: &PluginPickerState,
    au_scan_error: Option<&str>,
    callbacks: &PluginPickerCallbacks,
) -> impl IntoElement {
    let (title, hint) = match &catalog_status {
        CatalogStatus::Loading => ("Loading plug-ins…".to_string(), None),
        CatalogStatus::MissingDatabase => (
            "No plug-in library yet".to_string(),
            Some("Open the Plugin Manager and run a scan.".to_string()),
        ),
        CatalogStatus::Error(err) => (
            "The plug-in library could not be read".to_string(),
            Some(err.clone()),
        ),
        CatalogStatus::Ready if total == 0 => (
            "No plug-ins found".to_string(),
            Some("Scan for plug-ins in the Plugin Manager.".to_string()),
        ),
        CatalogStatus::Ready if !state.query.trim().is_empty() => (
            format!("Nothing matches “{}”", state.query.trim()),
            Some("Try a shorter name, a vendor, or another kind tab.".to_string()),
        ),
        CatalogStatus::Ready if state.filters.sidebar == PickerFilter::Favorites => (
            "No favorites here yet".to_string(),
            Some("Star a plug-in to keep it one click away.".to_string()),
        ),
        CatalogStatus::Ready if state.filters.sidebar == PickerFilter::RecentlyUsed => (
            "Nothing used recently".to_string(),
            Some("Plug-ins you insert will show up here.".to_string()),
        ),
        CatalogStatus::Ready if au_scan_error.is_some() => (
            "Audio Units unavailable".to_string(),
            Some("VST3 and CLAP plug-ins are still available.".to_string()),
        ),
        CatalogStatus::Ready => (
            "Nothing in this view".to_string(),
            Some("Pick another entry on the left or another kind tab.".to_string()),
        ),
    };
    let offer_manager = matches!(
        catalog_status,
        CatalogStatus::MissingDatabase | CatalogStatus::Error(_)
    ) || (matches!(catalog_status, CatalogStatus::Ready) && total == 0);

    div()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(space::SNUG))
        .size_full()
        .px(px(space::BLOCK))
        .child(
            svg()
                .path(assets::ICON_SEARCH_PATH)
                .w(px(20.0))
                .h(px(20.0))
                .text_color(Colors::text_faint()),
        )
        .child(
            div()
                .text_size(px(typography::UI_SM))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(Colors::text_secondary())
                .child(title),
        )
        .when_some(hint, |el, h| {
            el.child(
                div()
                    .text_size(px(typography::UI_XS))
                    .text_color(Colors::text_faint())
                    .child(h),
            )
        })
        .when(offer_manager, |el| {
            el.child(recovery_actions(
                matches!(catalog_status, CatalogStatus::Error(_)),
                callbacks.on_retry_load.clone(),
                callbacks.on_open_plugin_manager.clone(),
                callbacks.on_rebuild_database.clone(),
            ))
        })
}

fn recovery_actions(
    failed: bool,
    on_retry: VoidCb,
    on_open_manager: VoidCb,
    on_rebuild: VoidCb,
) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .gap(px(space::BASE))
        .pt(px(space::LOOSE))
        .child(fb_button(
            "plugin-picker-open-mgr",
            "Open Plugin Manager",
            FbButtonKind::Primary,
            true,
            move |_, window, cx| on_open_manager(&(), window, cx),
        ))
        .when(failed, |row| {
            row.child(fb_button(
                "plugin-picker-retry",
                "Retry",
                FbButtonKind::Default,
                true,
                move |_, window, cx| on_retry(&(), window, cx),
            ))
            .child(fb_button(
                "plugin-picker-rebuild",
                "Rebuild Library",
                FbButtonKind::Default,
                true,
                move |_, window, cx| on_rebuild(&(), window, cx),
            ))
        })
}

/// What Enter would insert — or why it cannot — and the actions.
fn selection_bar(
    selected: Option<&RegistryPlugin>,
    validation: &InsertValidation,
    selected_id: Option<String>,
    on_pick: Arc<dyn Fn(&String, &mut Window, &mut App) + 'static>,
    on_close: VoidCb,
) -> impl IntoElement {
    let can_add = selected.is_some() && *validation == InsertValidation::Ok;
    let summary = match selected {
        Some(plugin) => {
            let detail = match validation.message() {
                Some(reason) => div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(space::TIGHT))
                    .text_color(Colors::status_warning())
                    .child(reason)
                    .into_any_element(),
                None => div()
                    .text_color(Colors::text_muted())
                    .truncate()
                    .child(if plugin.vendor.is_empty() {
                        plugin.kind.label().to_string()
                    } else {
                        format!("{} · {}", plugin.vendor, plugin.kind.label())
                    })
                    .into_any_element(),
            };
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::BASE))
                .min_w(px(0.0))
                .child(kind_glyph(plugin.kind, can_add))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .min_w(px(0.0))
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(space::SNUG))
                                .child(
                                    div()
                                        .min_w(px(0.0))
                                        .truncate()
                                        .text_size(px(typography::UI_SM))
                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                        .text_color(Colors::text_primary())
                                        .child(plugin.name.clone()),
                                )
                                .child(plugin_format_badge_for(plugin)),
                        )
                        .child(div().text_size(px(typography::UI_XS)).child(detail)),
                )
                .into_any_element()
        }
        None => div()
            .text_size(px(typography::UI_XS))
            .text_color(Colors::text_faint())
            .child("Choose a plug-in")
            .into_any_element(),
    };

    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(space::LOOSE))
        .flex_shrink_0()
        .h(px(size::PROMINENT + space::SECTION))
        .px(px(space::LOOSE))
        .border_t(px(1.0))
        .border_color(Colors::divider())
        .bg(Colors::surface_panel())
        .child(div().flex_1().min_w(px(0.0)).child(summary))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::BASE))
                .flex_shrink_0()
                .child(
                    div()
                        .text_size(px(typography::DENSE_LABEL))
                        .text_color(Colors::text_faint())
                        .child("↑↓ to browse · Enter to add"),
                )
                .child(fb_button(
                    "plugin-picker-cancel",
                    "Cancel",
                    FbButtonKind::Default,
                    true,
                    move |_, window, cx| on_close(&(), window, cx),
                ))
                .child(fb_button(
                    "plugin-picker-add",
                    "Add",
                    FbButtonKind::Primary,
                    can_add,
                    move |_, window, cx| {
                        if let Some(id) = selected_id.clone() {
                            on_pick(&id, window, cx);
                        }
                    },
                )),
        )
}

pub fn page_size_for_height(height: f32) -> usize {
    (height / ROW_HEIGHT).max(1.0) as usize
}

pub fn visible_plugin_id_at(
    state: &PluginPickerState,
    index: &PluginSearchIndex,
    prefs: &PluginPickerPrefs,
) -> Option<String> {
    let result = compute_filter_result(index, &state.query, &state.filters, prefs, false);
    let plugin_index = result.indices.get(state.highlighted_index)?;
    index.plugin_at(*plugin_index).map(|p| p.id.clone())
}

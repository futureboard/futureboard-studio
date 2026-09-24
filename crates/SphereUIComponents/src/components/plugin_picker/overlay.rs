//! Main plugin picker overlay composition.

use std::sync::Arc;

use gpui::prelude::FluentBuilder;
use gpui::{
    App, InteractiveElement, IntoElement, ParentElement, StatefulInteractiveElement, Styled,
    UniformListScrollHandle, Window, div, px, svg, uniform_list,
};

use crate::assets;
use crate::components::controls::{FbButtonKind, fb_button};
use crate::components::plugin_picker::PluginPickerCallbacks;
use crate::components::plugin_picker::details::plugin_details_panel;
use crate::components::plugin_picker::filter::{FilterResult, compute_filter_result};
use crate::components::plugin_picker::insert::{InsertValidation, validate_insert};
use crate::components::plugin_picker::list_view::{
    ROW_HEIGHT, plugin_row, plugin_table_header, skeleton_body,
};
use crate::components::plugin_picker::prefs::PluginPickerPrefs;
use crate::components::plugin_picker::search_index::PluginSearchIndex;
use crate::components::plugin_picker::sidebar::plugin_filter_sidebar;
use crate::components::plugin_picker::state::{
    CatalogStatus, PluginPickerScrollHandles, PluginPickerState,
};
use crate::components::scroll_thumb::vertical_scrollbar_thumb;
use crate::components::text_input::{
    TextInputCallbacks, TextInputState, text_field_with_callbacks,
};
use crate::theme::Colors;
use SpherePluginHost::RegistryPlugin;

type VoidCb = Arc<dyn Fn(&(), &mut Window, &mut App) + 'static>;

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
    let close_button = callbacks.on_close.clone();
    let on_pick_add = callbacks.on_pick.clone();
    let on_pick_stub = callbacks.on_pick.clone();
    let debug = std::env::var_os("FUTUREBOARD_PLUGIN_PICKER_DEBUG").is_some();
    let stub_enabled = false;

    let index_arc = index.unwrap_or_else(|| Arc::new(PluginSearchIndex::from_plugins(Vec::new())));
    let index_ref: &PluginSearchIndex = &index_arc;
    let filter_result =
        compute_filter_result(index_ref, &state.query, &state.filters, prefs, debug);
    let FilterResult {
        indices,
        counts,
        vendors,
        categories,
    } = filter_result;
    let visible_count = indices.len();
    let total = index_ref.len();

    let highlighted = state.highlighted_index.min(visible_count.saturating_sub(1));
    // Rows are resolved by index on demand inside the virtualized list — no
    // per-render deep clone of every matching plugin.
    let selected_plugin = indices
        .get(highlighted)
        .copied()
        .and_then(|i| index_ref.plugin_at(i));
    let validation = selected_plugin
        .map(|plugin| validate_insert(plugin, &state.insert_target))
        .unwrap_or(InsertValidation::NotInsertable);
    let can_add = validation == InsertValidation::Ok;
    let row_indices = Arc::new(indices);

    let modal_width = prefs.window_width.max(760.0);
    let modal_height = prefs.window_height.max(480.0);

    let list_body = build_list_body(
        catalog_status.clone(),
        visible_count,
        total,
        state,
        index_arc.clone(),
        row_indices,
        highlighted,
        &callbacks,
        prefs,
        au_scan_error,
        &scroll.list,
    );

    let sidebar = plugin_filter_sidebar(
        &state.filters.sidebar,
        &counts,
        &vendors,
        &categories,
        debug,
        cfg!(target_os = "macos") || counts.au > 0,
        callbacks.on_select_filter.clone(),
        &scroll.sidebar,
    );

    let list_section = div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w(px(0.0))
        .overflow_hidden()
        .child(plugin_table_header())
        .child(
            div()
                .flex_1()
                .min_h(px(0.0))
                .w_full()
                .overflow_hidden()
                .child(list_body),
        );

    let footer_label = footer_label_for(
        selected_plugin,
        &validation,
        visible_count,
        total,
        &catalog_status,
        state,
    );

    let footer = build_footer(
        footer_label,
        can_add,
        stub_enabled,
        state
            .selected_id
            .clone()
            .or_else(|| selected_plugin.map(|p| p.id.clone())),
        on_pick_add,
        on_pick_stub,
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
        .pt(px(48.0))
        .px(px(18.0))
        .pb(px(24.0))
        .id("plugin-picker-overlay")
        .bg(gpui::transparent_black())
        .occlude()
        .on_mouse_down(gpui::MouseButton::Left, move |_, window, cx| {
            close_backdrop(&(), window, cx);
        })
        .child(
            div()
                .flex()
                .flex_col()
                .w(px(modal_width))
                .max_w(px(modal_width))
                .h(px(modal_height))
                .max_h(px(modal_height))
                .overflow_hidden()
                .rounded(px(crate::theme::radius::DIALOG))
                .border(px(1.0))
                .border_color(Colors::border_default())
                .bg(Colors::surface_window())
                .shadow_xl()
                .on_mouse_down(gpui::MouseButton::Left, |_, _window, cx| {
                    cx.stop_propagation();
                })
                .child(build_header(close_button))
                .child(
                    div()
                        .px(px(10.0))
                        .py(px(6.0))
                        .border_b(px(1.0))
                        .border_color(Colors::divider())
                        .text_size(px(10.0))
                        .text_color(Colors::text_faint())
                        .child(state.insert_target.label()),
                )
                .when_some(au_scan_error, |panel, message| {
                    panel.child(
                        div()
                            .px(px(10.0))
                            .py(px(6.0))
                            .border_b(px(1.0))
                            .border_color(Colors::divider())
                            .bg(Colors::surface_input())
                            .text_size(px(10.0))
                            .text_color(Colors::status_warning())
                            .child(format!(
                                "AudioUnit scan failed. VST3/CLAP results are still available. {message}"
                            )),
                    )
                })
                .child(
                    div()
                        .border_b(px(1.0))
                        .border_color(Colors::divider())
                        .px(px(10.0))
                        .py(px(7.0))
                        .child(text_field_with_callbacks(
                            search_input,
                            search_focused,
                            search_callbacks,
                        )),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_1()
                        .min_h(px(0.0))
                        .w_full()
                        .child(div().flex_shrink_0().h_full().child(sidebar))
                        .child(list_section)
                        .when(state.show_details, |panel| {
                            panel.when_some(selected_plugin, |panel, plugin| {
                                panel.child(
                                    div()
                                        .flex_shrink_0()
                                        .child(plugin_details_panel(plugin, &state.insert_target)),
                                )
                            })
                        }),
                )
                .child(footer),
        )
}

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
    let on_pick_add = callbacks.on_pick.clone();
    let on_pick_stub = callbacks.on_pick.clone();
    let debug = std::env::var_os("FUTUREBOARD_PLUGIN_PICKER_DEBUG").is_some();
    let stub_enabled = false;

    let index_arc = index.unwrap_or_else(|| Arc::new(PluginSearchIndex::from_plugins(Vec::new())));
    let index_ref: &PluginSearchIndex = &index_arc;
    let filter_result =
        compute_filter_result(index_ref, &state.query, &state.filters, prefs, debug);
    let FilterResult {
        indices,
        counts,
        vendors,
        categories,
    } = filter_result;
    let visible_count = indices.len();
    let total = index_ref.len();

    let highlighted = state.highlighted_index.min(visible_count.saturating_sub(1));
    let selected_plugin = indices
        .get(highlighted)
        .copied()
        .and_then(|i| index_ref.plugin_at(i));
    let validation = selected_plugin
        .map(|plugin| validate_insert(plugin, &state.insert_target))
        .unwrap_or(InsertValidation::NotInsertable);
    let can_add = validation == InsertValidation::Ok;
    let row_indices = Arc::new(indices);

    let list_body = build_list_body(
        catalog_status.clone(),
        visible_count,
        total,
        state,
        index_arc.clone(),
        row_indices,
        highlighted,
        &callbacks,
        prefs,
        au_scan_error,
        &scroll.list,
    );

    let sidebar = plugin_filter_sidebar(
        &state.filters.sidebar,
        &counts,
        &vendors,
        &categories,
        debug,
        cfg!(target_os = "macos") || counts.au > 0,
        callbacks.on_select_filter.clone(),
        &scroll.sidebar,
    );

    let list_section = div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w(px(0.0))
        .overflow_hidden()
        .child(plugin_table_header())
        .child(
            div()
                .flex_1()
                .min_h(px(0.0))
                .w_full()
                .overflow_hidden()
                .child(list_body),
        );

    let footer_label = footer_label_for(
        selected_plugin,
        &validation,
        visible_count,
        total,
        &catalog_status,
        state,
    );

    let footer = build_footer(
        footer_label,
        can_add,
        stub_enabled,
        state
            .selected_id
            .clone()
            .or_else(|| selected_plugin.map(|p| p.id.clone())),
        on_pick_add,
        on_pick_stub,
    );

    div()
        .flex()
        .flex_col()
        .size_full()
        .overflow_hidden()
        .bg(Colors::surface_window())
        .on_drop::<crate::components::plugin_picker::PluginDragItem>({
            let on_drop_plugin = callbacks.on_drop_plugin.clone();
            move |item, window, cx| on_drop_plugin(item, window, cx)
        })
        .child(
            div()
                .px(px(10.0))
                .py(px(6.0))
                .border_b(px(1.0))
                .border_color(Colors::divider())
                .text_size(px(10.0))
                .text_color(Colors::text_faint())
                .child(state.insert_target.label()),
        )
        .when_some(au_scan_error, |panel, message| {
            panel.child(
                div()
                    .px(px(10.0))
                    .py(px(6.0))
                    .border_b(px(1.0))
                    .border_color(Colors::divider())
                    .bg(Colors::surface_input())
                    .text_size(px(10.0))
                    .text_color(Colors::status_warning())
                    .child(format!(
                        "AudioUnit scan failed. VST3/CLAP results are still available. {message}"
                    )),
            )
        })
        .child(
            div()
                .border_b(px(1.0))
                .border_color(Colors::divider())
                .px(px(10.0))
                .py(px(7.0))
                .child(text_field_with_callbacks(
                    search_input,
                    search_focused,
                    search_callbacks,
                )),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .flex_1()
                .min_h(px(0.0))
                .w_full()
                .child(div().flex_shrink_0().h_full().child(sidebar))
                .child(list_section)
                .when(state.show_details, |panel| {
                    panel.when_some(selected_plugin, |panel, plugin| {
                        panel.child(
                            div()
                                .flex_shrink_0()
                                .child(plugin_details_panel(plugin, &state.insert_target)),
                        )
                    })
                }),
        )
        .child(footer)
}

fn build_header(close_button: VoidCb) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .h(px(36.0))
        .px(px(14.0))
        .border_b(px(1.0))
        .border_color(Colors::divider())
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .child(icon(assets::ICON_CPU_PATH, 13.0, Colors::accent_primary()))
                .child(
                    div()
                        .text_size(px(12.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(Colors::text_primary())
                        .child("Add Insert"),
                ),
        )
        .child(
            div()
                .flex()
                .items_center()
                .justify_center()
                .w(px(22.0))
                .h(px(22.0))
                .rounded(px(crate::theme::radius::CONTROL))
                .id("plugin-picker-close")
                .cursor(gpui::CursorStyle::PointingHand)
                .hover(|s| s.bg(Colors::surface_control_hover()))
                .on_click(move |_, window, cx| close_button(&(), window, cx))
                .child(icon(assets::ICON_X_PATH, 12.0, Colors::text_faint())),
        )
}

#[allow(clippy::too_many_arguments)]
fn build_list_body(
    catalog_status: CatalogStatus,
    visible_count: usize,
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
    if visible_count > 0 {
        let on_select_cb = callbacks.on_select.clone();
        let on_pick_cb = callbacks.on_pick.clone();
        let on_fav_cb = callbacks.on_toggle_favorite.clone();
        let favorites = Arc::new(prefs.favorites.clone());
        let highlighted_row = highlighted;
        let index_for_rows = index;
        let indices_for_rows = indices;
        let scroll_for_thumb = list_scroll.0.borrow().base_handle.clone();
        let list = uniform_list(
            "plugin-picker-list",
            visible_count,
            move |range, _window, _cx| {
                let on_select = on_select_cb.clone();
                let on_pick = on_pick_cb.clone();
                let on_fav = on_fav_cb.clone();
                let favorites = favorites.clone();
                let index = index_for_rows.clone();
                let indices = indices_for_rows.clone();
                // Only the visible range is materialized, and each row resolves
                // its plugin from the shared index by id — no per-row clone of
                // the whole catalog.
                range
                    .filter_map(|i| {
                        let plugin_idx = *indices.get(i)?;
                        let p = index.plugin_at(plugin_idx)?;
                        Some(
                            plugin_row(
                                i,
                                p,
                                i == highlighted_row,
                                favorites.contains(&p.id),
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
        return div()
            .relative()
            .size_full()
            .child(list)
            .child(vertical_scrollbar_thumb(scroll_for_thumb))
            .into_any_element();
    }

    empty_state_body(catalog_status, total, state, au_scan_error, callbacks).into_any_element()
}

fn empty_state_body(
    catalog_status: CatalogStatus,
    total: usize,
    state: &PluginPickerState,
    au_scan_error: Option<&str>,
    callbacks: &PluginPickerCallbacks,
) -> impl IntoElement {
    let (title, hint) = match &catalog_status {
        CatalogStatus::Loading => ("Loading plug-in index…".to_string(), None),
        CatalogStatus::MissingDatabase => (
            "No plugin database found.".to_string(),
            Some("Open Plugin Manager and click Scan Now.".to_string()),
        ),
        CatalogStatus::Error(err) => (
            "Failed to load plugin database.".to_string(),
            Some(err.clone()),
        ),
        CatalogStatus::Ready if total == 0 => (
            "No plugins found.".to_string(),
            Some("Scan plugins in Plugin Manager.".to_string()),
        ),
        CatalogStatus::Ready
            if matches!(
                state.filters.sidebar,
                crate::components::plugin_picker::state::PickerFilter::Favorites
            ) =>
        {
            (
                "No favorites yet.".to_string(),
                Some("Star a plug-in to add it here.".to_string()),
            )
        }
        CatalogStatus::Ready
            if matches!(
                state.filters.sidebar,
                crate::components::plugin_picker::state::PickerFilter::RecentlyUsed
            ) =>
        {
            (
                "No recently used plug-ins yet.".to_string(),
                Some("Inserted plug-ins will appear here.".to_string()),
            )
        }
        CatalogStatus::Ready if !state.query.is_empty() => {
            ("No plugins match this search.".to_string(), None)
        }
        CatalogStatus::Ready if au_scan_error.is_some() => (
            "Audio Units unavailable.".to_string(),
            Some("VST3 and CLAP plug-ins are still available.".to_string()),
        ),
        CatalogStatus::Ready => (
            "No plugins in this filter.".to_string(),
            Some("Pick a different sidebar entry.".to_string()),
        ),
    };

    div()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(6.0))
        .py(px(40.0))
        .child(
            div()
                .text_size(px(11.5))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(Colors::text_secondary())
                .child(title),
        )
        .when_some(hint, |el, h| {
            el.child(
                div()
                    .text_size(px(10.5))
                    .text_color(Colors::text_faint())
                    .child(h),
            )
        })
        .when(matches!(catalog_status, CatalogStatus::Error(_)), |el| {
            el.child(recovery_actions(
                callbacks.on_retry_load.clone(),
                callbacks.on_open_plugin_manager.clone(),
                callbacks.on_rebuild_database.clone(),
            ))
        })
}

fn recovery_actions(
    on_retry: VoidCb,
    on_open_manager: VoidCb,
    on_rebuild: VoidCb,
) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .gap(px(8.0))
        .pt(px(12.0))
        .child(fb_button(
            "plugin-picker-retry",
            "Retry",
            FbButtonKind::Primary,
            true,
            move |_, window, cx| on_retry(&(), window, cx),
        ))
        .child(fb_button(
            "plugin-picker-open-mgr",
            "Open Plugin Manager",
            FbButtonKind::Default,
            true,
            move |_, window, cx| on_open_manager(&(), window, cx),
        ))
        .child(fb_button(
            "plugin-picker-rebuild",
            "Rebuild Database",
            FbButtonKind::Default,
            true,
            move |_, window, cx| on_rebuild(&(), window, cx),
        ))
}

fn footer_label_for(
    selected: Option<&RegistryPlugin>,
    validation: &InsertValidation,
    visible_count: usize,
    total: usize,
    catalog_status: &CatalogStatus,
    state: &PluginPickerState,
) -> String {
    if let Some(message) = validation.message() {
        if selected.is_some() {
            return message.to_string();
        }
    }
    if let Some(p) = selected {
        return format!("{} · {} · {}", p.name, p.vendor, p.format.label());
    }
    if matches!(catalog_status, CatalogStatus::Loading) {
        return "Loading plugin index…".to_string();
    }
    if visible_count == 0 {
        return match catalog_status {
            CatalogStatus::MissingDatabase => "Open Plugin Manager → Scan Now".to_string(),
            CatalogStatus::Error(_) => "Database error".to_string(),
            CatalogStatus::Ready if total == 0 => "Catalog is empty".to_string(),
            CatalogStatus::Ready => "Adjust filter or search".to_string(),
            CatalogStatus::Loading => "Loading…".to_string(),
        };
    }
    if !state.query.is_empty() {
        return format!("{visible_count} match(es) · {total} total");
    }
    format!("{visible_count} plug-in(s)")
}

fn build_footer(
    footer_label: String,
    can_add: bool,
    stub_enabled: bool,
    selected_id: Option<String>,
    on_pick_add: Arc<dyn Fn(&String, &mut Window, &mut App) + 'static>,
    on_pick_stub: Arc<dyn Fn(&String, &mut Window, &mut App) + 'static>,
) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .h(px(40.0))
        .px(px(12.0))
        .border_t(px(1.0))
        .border_color(Colors::divider())
        .bg(Colors::surface_panel_alt())
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .text_size(px(10.5))
                .text_color(Colors::text_dim())
                .truncate()
                .child(footer_label),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .when(stub_enabled, |row| {
                    row.child(fb_button(
                        "plugin-picker-stub",
                        "Insert Stub (Dev)",
                        FbButtonKind::Default,
                        true,
                        move |_, window, cx| {
                            on_pick_stub(
                                &crate::components::plugin_picker::STUB_PLUGIN_ID.to_string(),
                                window,
                                cx,
                            )
                        },
                    ))
                })
                .child(fb_button(
                    "plugin-picker-add",
                    "Add",
                    FbButtonKind::Primary,
                    can_add,
                    move |_, window, cx| {
                        if let Some(id) = selected_id.clone() {
                            on_pick_add(&id, window, cx);
                        }
                    },
                )),
        )
}

fn icon(path: &'static str, size: f32, color: gpui::Rgba) -> impl IntoElement {
    svg().path(path).w(px(size)).h(px(size)).text_color(color)
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

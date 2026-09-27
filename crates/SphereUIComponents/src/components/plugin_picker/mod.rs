//! DAW-grade insert plugin browser overlay.
//!
//! Reads from the cached SQLite catalog only — never scans or loads plug-in
//! binaries from the picker UI thread.

mod category;
mod filter;
mod insert;
mod list_view;
mod overlay;
mod prefs;
mod search_index;
mod sidebar;
mod state;

pub use category::{NormalizedCategory, normalize_category, normalized_category_label};
pub use filter::{FilterCounts, FilterResult, compute_filter_result, picker_perf_debug};
pub use insert::{InsertValidation, PluginInsertKind, PluginInsertTarget, validate_insert};
pub use list_view::PluginDragItem;
pub use overlay::{
    page_size_for_height, plugin_picker_overlay, plugin_picker_panel, visible_plugin_id_at,
};
pub use prefs::PluginPickerPrefs;
pub use search_index::PluginSearchIndex;
pub use state::{
    CatalogStatus, KindTab, PickerFilter, PluginFilterState, PluginPickerLoadState,
    PluginPickerScrollHandles, PluginPickerState,
};

use std::sync::Arc;

use gpui::{App, ParentElement, Styled, Window};

/// Legacy sentinel rejected by current VST3-only insert creation.
pub const STUB_PLUGIN_ID: &str = "futureboard.stub.gain";

/// Legacy alias preserved for older call sites.
pub const CATEGORY_ALL: &str = "All";

#[derive(Clone)]
pub struct PluginPickerCallbacks {
    pub on_close: Arc<dyn Fn(&(), &mut Window, &mut App) + 'static>,
    pub on_select: Arc<dyn Fn(&String, &mut Window, &mut App) + 'static>,
    pub on_pick: Arc<dyn Fn(&String, &mut Window, &mut App) + 'static>,
    pub on_select_filter: Arc<dyn Fn(&PickerFilter, &mut Window, &mut App) + 'static>,
    pub on_select_kind: Arc<dyn Fn(&KindTab, &mut Window, &mut App) + 'static>,
    /// Folds or unfolds the rail's vendor list.
    pub on_toggle_vendors: Arc<dyn Fn(&(), &mut Window, &mut App) + 'static>,
    pub on_toggle_favorite: Arc<dyn Fn(&String, &mut Window, &mut App) + 'static>,
    pub on_retry_load: Arc<dyn Fn(&(), &mut Window, &mut App) + 'static>,
    pub on_open_plugin_manager: Arc<dyn Fn(&(), &mut Window, &mut App) + 'static>,
    pub on_rebuild_database: Arc<dyn Fn(&(), &mut Window, &mut App) + 'static>,
    pub on_drop_plugin: Arc<dyn Fn(&PluginDragItem, &mut Window, &mut App) + 'static>,
}

pub struct PluginDragPreview {
    pub(crate) label: String,
}

impl gpui::Render for PluginDragPreview {
    fn render(
        &mut self,
        _window: &mut gpui::Window,
        _cx: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        gpui::div()
            .px(gpui::px(crate::theme::space::BASE))
            .py(gpui::px(crate::theme::space::TIGHT))
            .rounded(gpui::px(crate::theme::radius::CONTROL))
            .bg(crate::theme::Colors::surface_raised())
            .text_color(crate::theme::Colors::text_primary())
            .child(self.label.clone())
    }
}

pub fn move_highlight(state: &mut PluginPickerState, delta: isize, visible_len: usize) {
    if visible_len == 0 {
        state.highlighted_index = 0;
        state.selected_id = None;
        return;
    }
    let next = state.highlighted_index as isize + delta;
    state.highlighted_index = next.clamp(0, visible_len as isize - 1) as usize;
}

pub fn sync_selection_from_highlight(
    state: &mut PluginPickerState,
    index: &PluginSearchIndex,
    prefs: &PluginPickerPrefs,
) {
    state.selected_id = visible_plugin_id_at(state, index, prefs);
}

pub fn ensure_default_highlight(
    state: &mut PluginPickerState,
    index: &PluginSearchIndex,
    prefs: &PluginPickerPrefs,
) {
    let result = crate::components::plugin_picker::filter::compute_filter_result(
        index,
        &state.query,
        &state.filters,
        prefs,
        false,
    );
    state.clamp_highlight(result.indices.len());
    if state.selected_id.is_none() && !result.indices.is_empty() {
        // Reuse the pass we just ran rather than recomputing it inside
        // `visible_plugin_id_at` — this is on the per-keystroke path.
        state.selected_id = result
            .indices
            .get(state.highlighted_index)
            .and_then(|&i| index.plugin_at(i))
            .map(|plugin| plugin.id.clone());
    }
}

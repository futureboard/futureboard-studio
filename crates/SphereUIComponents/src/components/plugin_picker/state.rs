//! Plugin picker state, filters, and catalog load status.

use gpui::{ScrollHandle, UniformListScrollHandle};

use crate::components::plugin_picker::insert::{PluginInsertKind, PluginInsertTarget};
use crate::components::timeline::timeline_state::TrackType;
use SpherePluginHost::PluginFormat;

/// Stable scroll handles for the insert picker sidebar and plug-in list.
#[derive(Clone, Default)]
pub struct PluginPickerScrollHandles {
    pub sidebar: ScrollHandle,
    pub list: UniformListScrollHandle,
    /// The row the list was last scrolled to reveal, and how long the list was
    /// then. The list follows the keyboard highlight only when either moves,
    /// so the wheel stays free to scroll away from it — and a new filter, which
    /// resets the highlight to the top, still brings the list back up.
    pub revealed_row: std::rc::Rc<std::cell::Cell<Option<(usize, usize)>>>,
}

/// Sidebar filter rail — composes with search query and optional secondary filters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerFilter {
    All,
    Favorites,
    RecentlyUsed,
    Instruments,
    Effects,
    Format(PluginFormat),
    /// Futureboard built-in (stock) plug-ins (`builtin:` id).
    Builtin,
    Vendor(String),
    Category(String),
    Failed,
}

impl Default for PickerFilter {
    fn default() -> Self {
        Self::All
    }
}

/// The kind tabs across the top of the picker. They compose with the rail, so
/// "Favorites" under "Instruments" is the favourite instruments.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum KindTab {
    #[default]
    All,
    Effects,
    Instruments,
}

impl KindTab {
    pub fn matches(self, plugin: &SpherePluginHost::RegistryPlugin) -> bool {
        match self {
            KindTab::All => true,
            // A plug-in that declared no class is insertable as an effect, so
            // it is listed with them rather than hidden from the slot that
            // needs it.
            KindTab::Effects => plugin.kind.usable_as_effect(),
            KindTab::Instruments => plugin.kind == SpherePluginHost::PluginKind::Instrument,
        }
    }
}

/// Multi-dimensional filter state applied together with the search query.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginFilterState {
    pub kind: KindTab,
    pub sidebar: PickerFilter,
    pub format: Option<PluginFormat>,
    pub vendor: Option<String>,
    pub category: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PluginPickerState {
    pub is_open: bool,
    pub insert_target: PluginInsertTarget,
    pub filters: PluginFilterState,
    pub query: String,
    pub selected_id: Option<String>,
    pub highlighted_index: usize,
    pub show_details: bool,
    /// Whether the rail's vendor list is unfolded. Folded by default: a
    /// library with a few hundred vendors buried the rest of the rail.
    pub vendors_expanded: bool,
}

impl PluginPickerState {
    pub fn closed() -> Self {
        Self {
            is_open: false,
            insert_target: PluginInsertTarget {
                track_id: String::new(),
                track_name: String::new(),
                track_type: TrackType::Audio,
                next_slot_index: 0,
                desired_kind: PluginInsertKind::Effect,
            },
            filters: PluginFilterState::default(),
            query: String::new(),
            selected_id: None,
            highlighted_index: 0,
            show_details: true,
            vendors_expanded: false,
        }
    }

    pub fn open_for(
        track_id: &str,
        track_name: &str,
        track_type: TrackType,
        next_slot_index: usize,
        show_details: bool,
    ) -> Self {
        Self::open_for_with_filter(
            track_id,
            track_name,
            track_type,
            next_slot_index,
            show_details,
            PickerFilter::All,
            PluginInsertKind::Effect,
        )
    }

    pub fn open_for_with_filter(
        track_id: &str,
        track_name: &str,
        track_type: TrackType,
        next_slot_index: usize,
        show_details: bool,
        sidebar_filter: PickerFilter,
        desired_kind: PluginInsertKind,
    ) -> Self {
        // Kind is a tab now, not a rail entry: a caller asking for the
        // instrument or effect rail gets that tab with the whole library under
        // it.
        let (kind, sidebar) = match sidebar_filter {
            PickerFilter::Instruments => (KindTab::Instruments, PickerFilter::All),
            PickerFilter::Effects => (KindTab::Effects, PickerFilter::All),
            other => (KindTab::All, other),
        };
        Self {
            is_open: true,
            insert_target: PluginInsertTarget {
                track_id: track_id.to_string(),
                track_name: track_name.to_string(),
                track_type,
                next_slot_index,
                desired_kind,
            },
            filters: PluginFilterState {
                kind,
                sidebar,
                ..PluginFilterState::default()
            },
            query: String::new(),
            selected_id: None,
            highlighted_index: 0,
            show_details,
            vendors_expanded: false,
        }
    }

    pub fn set_kind_tab(&mut self, kind: KindTab) {
        self.filters.kind = kind;
        self.reset_selection_for_filter_change();
    }

    pub fn reset_selection_for_filter_change(&mut self) {
        self.highlighted_index = 0;
        self.selected_id = None;
    }

    pub fn clamp_highlight(&mut self, visible_len: usize) {
        if visible_len == 0 {
            self.highlighted_index = 0;
            self.selected_id = None;
            return;
        }
        if self.highlighted_index >= visible_len {
            self.highlighted_index = visible_len - 1;
        }
    }

    pub fn set_sidebar_filter(&mut self, filter: PickerFilter) {
        self.filters.sidebar = filter;
        // Sidebar Format / Built-in entries own format selection; clear any
        // latent secondary format so facets cannot AND against a stale value.
        self.filters.format = None;
        self.reset_selection_for_filter_change();
    }
}

/// Loading / error state for the cached catalog.
#[derive(Debug, Clone)]
pub enum CatalogStatus {
    Loading,
    Ready,
    MissingDatabase,
    Error(String),
}

pub type PluginPickerLoadState = CatalogStatus;

#[cfg(test)]
mod tests {
    use super::*;

    /// Callers still ask for the effect or instrument rail; they now get the
    /// matching tab with the whole library on the rail.
    #[test]
    fn a_kind_rail_request_opens_on_that_tab() {
        let state = PluginPickerState::open_for_with_filter(
            "t1",
            "Bass",
            TrackType::Instrument,
            0,
            true,
            PickerFilter::Instruments,
            PluginInsertKind::Instrument,
        );
        assert_eq!(state.filters.kind, KindTab::Instruments);
        assert_eq!(state.filters.sidebar, PickerFilter::All);
        assert!(!state.vendors_expanded);
    }
}

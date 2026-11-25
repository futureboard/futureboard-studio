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

/// Where a plug-in comes from: Futureboard's own, or a plug-in format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginSource {
    Builtin,
    Format(PluginFormat),
}

impl PluginSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Builtin => "Built-in",
            Self::Format(format) => format.label(),
        }
    }

    pub fn matches(self, plugin: &SpherePluginHost::RegistryPlugin) -> bool {
        match self {
            Self::Builtin => plugin.is_builtin(),
            Self::Format(format) => !plugin.is_builtin() && plugin.format == format,
        }
    }
}

/// A chip above the list. Chips compose with the kind tab, the rail and the
/// search; `None` clears that row of chips.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerFacet {
    Source(Option<PluginSource>),
    Category(Option<String>),
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
    /// The source chip (Built-in, VST3, CLAP…).
    pub source: Option<PluginSource>,
    pub vendor: Option<String>,
    /// The category chip, as a normalized category label.
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
        // Kind is a tab and source/category are chips, not rail entries: a
        // caller asking for one of those rails gets the tab or chip instead,
        // with the whole library on the rail.
        let mut filters = PluginFilterState::default();
        match sidebar_filter {
            PickerFilter::Instruments => filters.kind = KindTab::Instruments,
            PickerFilter::Effects => filters.kind = KindTab::Effects,
            PickerFilter::Builtin => filters.source = Some(PluginSource::Builtin),
            PickerFilter::Format(format) => filters.source = Some(PluginSource::Format(format)),
            PickerFilter::Category(category) => filters.category = Some(category),
            other => filters.sidebar = other,
        }
        Self {
            is_open: true,
            insert_target: PluginInsertTarget {
                track_id: track_id.to_string(),
                track_name: track_name.to_string(),
                track_type,
                next_slot_index,
                desired_kind,
            },
            filters,
            query: String::new(),
            selected_id: None,
            highlighted_index: 0,
            show_details,
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

    pub fn set_facet(&mut self, facet: PickerFacet) {
        match facet {
            PickerFacet::Source(source) => self.filters.source = source,
            PickerFacet::Category(category) => self.filters.category = category,
        }
        self.reset_selection_for_filter_change();
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
    }

    /// The rail no longer lists sources or categories; a caller asking for
    /// one opens on the matching chip.
    #[test]
    fn a_source_or_category_request_opens_on_that_chip() {
        let open = |filter| {
            PluginPickerState::open_for_with_filter(
                "t1",
                "Bass",
                TrackType::Audio,
                0,
                true,
                filter,
                PluginInsertKind::Effect,
            )
        };
        let builtin = open(PickerFilter::Builtin);
        assert_eq!(builtin.filters.source, Some(PluginSource::Builtin));
        assert_eq!(builtin.filters.sidebar, PickerFilter::All);
        let eq = open(PickerFilter::Category("EQ".into()));
        assert_eq!(eq.filters.category.as_deref(), Some("EQ"));
        assert_eq!(eq.filters.sidebar, PickerFilter::All);
    }
}

//! The Add Track dialog's instrument picker: a search field, a Vendor / Name /
//! Format switch, and the scanned instruments as a collapsible tree under it.
//!
//! The model (`instrument_rows`) is a pure function of the registry, the
//! grouping, the query and the user's expand/collapse toggles, so keyboard
//! navigation and the rendered list always index the same flat row sequence.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, svg, AnyElement, App, InteractiveElement, IntoElement, ParentElement, ScrollHandle,
    StatefulInteractiveElement, Styled, Window,
};
use SpherePluginHost::{PluginFormat, RegistryPlugin};

use crate::assets;
use crate::components::controls::{fb_segment, fb_segmented_track, FbSegment};
use crate::components::plugin_picker::PluginPickerPrefs;
use crate::theme::{menu, space, typography, Colors};

use super::plugin_format_label;

/// With this many groups or fewer, every folder starts open: a handful of
/// folders is quicker to scan open than to click through. Past it (a large
/// vendor list) only the folder holding the current instrument opens.
const AUTO_EXPAND_MAX_GROUPS: usize = 6;

/// Indent of a plug-in row under its folder: the chevron's slot plus the gap
/// after it, so plug-in names line up with the folder name.
const NESTED_INDENT: f32 = menu::CHEVRON_SIZE + space::SNUG;

/// How the picker arranges the scanned instruments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InstrumentGrouping {
    /// Folder per vendor.
    Vendor,
    /// One alphabetical list.
    Name,
    /// Folder per plug-in format (Built-in, VST3, CLAP, AU, VST2, LV2).
    Format,
}

/// The grouping last chosen, cached after the first read of the picker prefs
/// so opening the dialog does not touch the disk. `UNLOADED` until then.
static SESSION_GROUPING: AtomicU8 = AtomicU8::new(UNLOADED);
const UNLOADED: u8 = u8::MAX;

impl InstrumentGrouping {
    pub const ALL: [Self; 3] = [Self::Vendor, Self::Name, Self::Format];

    pub fn label(self) -> &'static str {
        match self {
            Self::Vendor => "Vendor",
            Self::Name => "Name",
            Self::Format => "Format",
        }
    }

    /// The grouping every Add Track window opens with: the last one chosen,
    /// persisted with the plug-in picker's other UI preferences.
    pub fn session() -> Self {
        let cached = SESSION_GROUPING.load(Ordering::Relaxed);
        if cached != UNLOADED {
            return Self::from_index(cached);
        }
        let loaded = Self::from_key(&PluginPickerPrefs::load().instrument_grouping);
        SESSION_GROUPING.store(loaded.index(), Ordering::Relaxed);
        loaded
    }

    pub fn remember(self) {
        SESSION_GROUPING.store(self.index(), Ordering::Relaxed);
        // Re-read before writing so favorites/recents saved by the plug-in
        // picker since this window opened are not overwritten.
        let mut prefs = PluginPickerPrefs::load();
        if prefs.instrument_grouping != self.key() {
            prefs.instrument_grouping = self.key().to_string();
            let _ = prefs.save();
        }
    }

    fn index(self) -> u8 {
        match self {
            Self::Vendor => 0,
            Self::Name => 1,
            Self::Format => 2,
        }
    }

    fn from_index(index: u8) -> Self {
        match index {
            1 => Self::Name,
            2 => Self::Format,
            _ => Self::Vendor,
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::Vendor => "vendor",
            Self::Name => "name",
            Self::Format => "format",
        }
    }

    fn from_key(key: &str) -> Self {
        match key {
            "name" => Self::Name,
            "format" => Self::Format,
            _ => Self::Vendor,
        }
    }
}

/// One line of the picker, in display order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstrumentRow {
    /// The pinned "No Instrument" entry. `hint` explains an empty registry.
    NoInstrument { hint: Option<&'static str> },
    /// A folder. `key` is stable across renders and namespaced by grouping.
    Group {
        key: String,
        label: String,
        count: usize,
        expanded: bool,
    },
    Plugin {
        id: String,
        label: String,
        detail: String,
        nested: bool,
    },
    /// Shown in place of the tree when the query matches nothing.
    NoMatches,
}

impl InstrumentRow {
    pub fn is_navigable(&self) -> bool {
        !matches!(self, Self::NoMatches)
    }
}

/// Per-window picker state that is not the selection itself.
pub struct InstrumentTreeState {
    pub grouping: InstrumentGrouping,
    /// Folders the user flipped away from their default open/closed state.
    pub toggled: HashSet<String>,
    /// Keyboard target, as an index into the current rows. `None` means "the
    /// natural target": the first match while searching, else the selection.
    pub highlight: Option<usize>,
    pub scroll: ScrollHandle,
}

impl InstrumentTreeState {
    pub fn new() -> Self {
        Self {
            grouping: InstrumentGrouping::session(),
            toggled: HashSet::new(),
            highlight: None,
            scroll: ScrollHandle::new(),
        }
    }

    /// Fresh state for a newly opened popover; keeps the grouping.
    pub fn reset(&mut self) {
        self.toggled.clear();
        self.highlight = None;
        self.scroll = ScrollHandle::new();
    }

    pub fn set_grouping(&mut self, grouping: InstrumentGrouping) {
        grouping.remember();
        self.grouping = grouping;
        self.reset();
    }

    pub fn toggle_group(&mut self, key: &str) {
        if !self.toggled.remove(key) {
            self.toggled.insert(key.to_string());
        }
    }

    pub fn rows(
        &self,
        plugins: &[RegistryPlugin],
        query: &str,
        selected: Option<&str>,
    ) -> Vec<InstrumentRow> {
        instrument_rows(plugins, self.grouping, query, selected, &self.toggled)
    }

    /// Move the keyboard target by `delta` navigable rows and scroll it in.
    pub fn move_highlight(
        &mut self,
        rows: &[InstrumentRow],
        selected: Option<&str>,
        searching: bool,
        delta: i32,
    ) {
        let navigable: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.is_navigable())
            .map(|(index, _)| index)
            .collect();
        if navigable.is_empty() {
            return;
        }
        let current = effective_highlight(rows, self.highlight, selected, searching);
        let position = current
            .and_then(|current| navigable.iter().position(|&index| index == current))
            .map(|position| position as i32 + delta)
            .unwrap_or(0)
            .clamp(0, navigable.len() as i32 - 1) as usize;
        let target = navigable[position];
        self.highlight = Some(target);
        self.scroll.scroll_to_item(target);
    }
}

impl Default for InstrumentTreeState {
    fn default() -> Self {
        Self::new()
    }
}

/// The row the keyboard acts on this frame.
///
/// Without an explicit target it is the first match while searching (so
/// typing then Enter picks the best hit), otherwise the current selection.
pub fn effective_highlight(
    rows: &[InstrumentRow],
    highlight: Option<usize>,
    selected: Option<&str>,
    searching: bool,
) -> Option<usize> {
    if let Some(index) =
        highlight.filter(|&index| rows.get(index).is_some_and(|r| r.is_navigable()))
    {
        return Some(index);
    }
    let first_plugin = || {
        rows.iter()
            .position(|row| matches!(row, InstrumentRow::Plugin { .. }))
    };
    if searching {
        if let Some(first) = first_plugin() {
            return Some(first);
        }
    }
    let selected_row = rows.iter().position(|row| match row {
        InstrumentRow::Plugin { id, .. } => selected == Some(id.as_str()),
        InstrumentRow::NoInstrument { .. } => selected.is_none_or(str::is_empty),
        _ => false,
    });
    selected_row.or_else(|| rows.iter().position(InstrumentRow::is_navigable))
}

fn format_rank(plugin: &RegistryPlugin) -> u8 {
    if plugin.is_builtin() {
        return 0;
    }
    match plugin.format {
        PluginFormat::Vst3 => 1,
        PluginFormat::Clap => 2,
        PluginFormat::Au => 3,
        // Below the rest: when an instrument ships in several formats, VST2 is
        // the legacy one.
        PluginFormat::Vst2 => 4,
        PluginFormat::Lv2 => 5,
        PluginFormat::Unknown => 6,
    }
}

fn format_group_label(plugin: &RegistryPlugin) -> &'static str {
    if plugin.is_builtin() {
        "Built-in"
    } else {
        plugin_format_label(plugin.format)
    }
}

/// Every whitespace-separated term must appear in the name, vendor or format,
/// so "ample vst3" narrows to Ample Sound's VST3 builds.
fn matches_query(plugin: &RegistryPlugin, terms: &[String]) -> bool {
    if terms.is_empty() {
        return true;
    }
    let haystack = format!(
        "{} {} {} {}",
        plugin.name,
        plugin.vendor,
        plugin_format_label(plugin.format),
        format_group_label(plugin)
    )
    .to_lowercase();
    terms.iter().all(|term| haystack.contains(term.as_str()))
}

fn by_name(a: &&RegistryPlugin, b: &&RegistryPlugin) -> std::cmp::Ordering {
    a.name
        .to_lowercase()
        .cmp(&b.name.to_lowercase())
        .then_with(|| format_rank(a).cmp(&format_rank(b)))
        .then_with(|| a.id.cmp(&b.id))
}

/// Label shown for a plug-in, with its format appended only when another
/// entry in the same list carries the same name.
fn plugin_rows<'a>(
    members: &[&'a RegistryPlugin],
    detail: impl Fn(&RegistryPlugin) -> String,
    nested: bool,
) -> Vec<InstrumentRow> {
    let mut name_counts: HashMap<String, usize> = HashMap::new();
    for plugin in members {
        *name_counts.entry(plugin.name.to_lowercase()).or_default() += 1;
    }
    members
        .iter()
        .map(|plugin| {
            let label = if name_counts
                .get(&plugin.name.to_lowercase())
                .copied()
                .unwrap_or(0)
                > 1
            {
                format!("{} ({})", plugin.name, plugin_format_label(plugin.format))
            } else {
                plugin.name.clone()
            };
            InstrumentRow::Plugin {
                id: plugin.id.clone(),
                label,
                detail: detail(plugin),
                nested,
            }
        })
        .collect()
}

pub fn instrument_rows(
    plugins: &[RegistryPlugin],
    grouping: InstrumentGrouping,
    query: &str,
    selected: Option<&str>,
    toggled: &HashSet<String>,
) -> Vec<InstrumentRow> {
    let mut rows = vec![InstrumentRow::NoInstrument {
        hint: plugins
            .is_empty()
            .then_some("Open Plugin Manager to scan instruments"),
    }];
    if plugins.is_empty() {
        return rows;
    }
    let terms: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    let searching = !terms.is_empty();
    let mut matched: Vec<&RegistryPlugin> = plugins
        .iter()
        .filter(|plugin| matches_query(plugin, &terms))
        .collect();
    if matched.is_empty() {
        rows.push(InstrumentRow::NoMatches);
        return rows;
    }
    matched.sort_by(by_name);

    if grouping == InstrumentGrouping::Name {
        rows.extend(plugin_rows(
            &matched,
            |plugin| {
                let vendor = plugin.vendor.trim();
                let format = format_group_label(plugin);
                if vendor.is_empty() {
                    format.to_string()
                } else {
                    format!("{vendor} / {format}")
                }
            },
            false,
        ));
        return rows;
    }

    // (sort key, display label) → members, already in name order.
    let mut groups: BTreeMap<(u8, String), (String, Vec<&RegistryPlugin>)> = BTreeMap::new();
    for plugin in matched {
        let (sort_key, label) = match grouping {
            InstrumentGrouping::Format => (
                (format_rank(plugin), String::new()),
                format_group_label(plugin).to_string(),
            ),
            _ => {
                let vendor = plugin.vendor.trim();
                if vendor.is_empty() {
                    // Unknown vendor sorts after every named one.
                    ((1, String::new()), "Unknown Vendor".to_string())
                } else {
                    ((0, vendor.to_lowercase()), vendor.to_string())
                }
            }
        };
        groups
            .entry(sort_key)
            .or_insert_with(|| (label, Vec::new()))
            .1
            .push(plugin);
    }

    let auto_expand_all = groups.len() <= AUTO_EXPAND_MAX_GROUPS;
    let prefix = grouping.key();
    for ((rank, sort_name), (label, members)) in groups {
        let key = format!("{prefix}:{rank}:{sort_name}");
        let holds_selection =
            selected.is_some_and(|selected| members.iter().any(|plugin| plugin.id == selected));
        let default_open = auto_expand_all || holds_selection;
        // A search opens every folder that still has a match; the user's
        // toggles apply again once the query is cleared.
        let expanded = searching || (default_open != toggled.contains(&key));
        rows.push(InstrumentRow::Group {
            key,
            label,
            count: members.len(),
            expanded,
        });
        if expanded {
            let detail: fn(&RegistryPlugin) -> String = match grouping {
                InstrumentGrouping::Format => |plugin| plugin.vendor.trim().to_string(),
                _ => |plugin| format_group_label(plugin).to_string(),
            };
            rows.extend(plugin_rows(&members, detail, true));
        }
    }
    rows
}

/// Text on the closed trigger for the current selection.
pub fn selected_instrument_label(plugins: &[RegistryPlugin], selected: Option<&str>) -> String {
    let Some(selected) = selected.filter(|selected| !selected.is_empty()) else {
        return "No Instrument".to_string();
    };
    let Some(plugin) = plugins.iter().find(|plugin| plugin.id == selected) else {
        return "Select instrument...".to_string();
    };
    let same_name = plugins
        .iter()
        .filter(|other| other.name.eq_ignore_ascii_case(&plugin.name))
        .count();
    if same_name > 1 {
        format!("{} ({})", plugin.name, plugin_format_label(plugin.format))
    } else {
        plugin.name.clone()
    }
}

pub type GroupingCb = Arc<dyn Fn(&InstrumentGrouping, &mut Window, &mut App) + 'static>;
pub type StringCb = Arc<dyn Fn(&String, &mut Window, &mut App) + 'static>;

pub struct InstrumentTreeCallbacks {
    pub on_grouping: GroupingCb,
    pub on_toggle_group: StringCb,
    /// Called with the plug-in id, or "" for No Instrument.
    pub on_select: StringCb,
}

fn grouping_switch(grouping: InstrumentGrouping, on_grouping: GroupingCb) -> impl IntoElement {
    let last = InstrumentGrouping::ALL.len() - 1;
    let mut track = fb_segmented_track().flex_1();
    for (index, mode) in InstrumentGrouping::ALL.into_iter().enumerate() {
        let position = match index {
            0 => FbSegment::First,
            i if i == last => FbSegment::Last,
            _ => FbSegment::Middle,
        };
        let on_grouping = on_grouping.clone();
        track = track.child(fb_segment(
            ("add-track-instrument-grouping", index),
            mode.label(),
            mode == grouping,
            position,
            move |_, window, cx| on_grouping(&mode, window, cx),
        ));
    }
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::BASE))
        .child(
            div()
                .flex_shrink_0()
                .text_size(px(typography::UI_XS))
                .text_color(Colors::text_faint())
                .child("Group by"),
        )
        .child(track)
}

fn check_icon() -> impl IntoElement {
    svg()
        .path(assets::ICON_CHECK_PATH)
        .w(px(menu::ICON_SIZE))
        .h(px(menu::ICON_SIZE))
        .flex_shrink_0()
        .text_color(Colors::accent_primary())
}

fn row_shell(index: usize, active: bool, highlighted: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(("add-track-instrument-row", index))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::SNUG))
        .w_full()
        .min_h(px(menu::ROW_HEIGHT))
        .px(px(menu::ROW_PAD_X))
        .rounded(px(crate::theme::radius::CONTROL_SM))
        .cursor(gpui::CursorStyle::PointingHand)
        .bg(if active {
            Colors::accent_muted()
        } else if highlighted {
            Colors::surface_control_hover()
        } else {
            gpui::transparent_black().into()
        })
        .hover(move |s| {
            if active {
                s
            } else {
                s.bg(Colors::surface_control_hover())
            }
        })
}

fn render_row(
    index: usize,
    row: InstrumentRow,
    selected: Option<&str>,
    highlighted: bool,
    callbacks: &InstrumentTreeCallbacks,
) -> AnyElement {
    match row {
        InstrumentRow::NoInstrument { hint } => {
            let active = selected.is_none_or(str::is_empty);
            let on_select = callbacks.on_select.clone();
            row_shell(index, active, highlighted)
                .on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    on_select(&String::new(), window, cx);
                })
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .truncate()
                                .text_size(px(menu::LABEL_TEXT_SIZE))
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
                                .child("No Instrument"),
                        )
                        .children(hint.map(|hint| {
                            div()
                                .truncate()
                                .text_size(px(typography::DENSE_CAPTION))
                                .text_color(Colors::text_faint())
                                .child(hint)
                        })),
                )
                .children(active.then(check_icon))
                .into_any_element()
        }
        InstrumentRow::Group {
            key,
            label,
            count,
            expanded,
        } => {
            let on_toggle = callbacks.on_toggle_group.clone();
            row_shell(index, false, highlighted)
                .on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    on_toggle(&key, window, cx);
                })
                .child(
                    svg()
                        .path(if expanded {
                            assets::ICON_CHEVRON_DOWN_PATH
                        } else {
                            assets::ICON_CHEVRON_RIGHT_PATH
                        })
                        .w(px(menu::CHEVRON_SIZE))
                        .h(px(menu::CHEVRON_SIZE))
                        .flex_shrink_0()
                        .text_color(Colors::text_faint()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .truncate()
                        .text_size(px(menu::LABEL_TEXT_SIZE))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(Colors::text_secondary())
                        .child(label),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(menu::META_TEXT_SIZE))
                        .text_color(Colors::text_faint())
                        .child(count.to_string()),
                )
                .into_any_element()
        }
        InstrumentRow::Plugin {
            id,
            label,
            detail,
            nested,
        } => {
            let active = selected == Some(id.as_str());
            let on_select = callbacks.on_select.clone();
            row_shell(index, active, highlighted)
                .when(nested, |row| row.pl(px(menu::ROW_PAD_X + NESTED_INDENT)))
                .on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    on_select(&id, window, cx);
                })
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .truncate()
                        .text_size(px(menu::LABEL_TEXT_SIZE))
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
                .when(!detail.is_empty(), |row| {
                    row.child(
                        div()
                            .flex_shrink_0()
                            .max_w(px(160.0))
                            .truncate()
                            .text_size(px(typography::DENSE_CAPTION))
                            .text_color(Colors::text_faint())
                            .child(detail),
                    )
                })
                .children(active.then(check_icon))
                .into_any_element()
        }
        InstrumentRow::NoMatches => div()
            .id(("add-track-instrument-row", index))
            .px(px(menu::ROW_PAD_X))
            .py(px(space::BASE))
            .text_size(px(menu::LABEL_TEXT_SIZE))
            .text_color(Colors::text_faint())
            .child("No matching instruments")
            .into_any_element(),
    }
}

/// The open popover's body: pinned search + grouping switch, then the tree in
/// its own scroll region. Rows are direct children of the scroll region so
/// `ScrollHandle::scroll_to_item` can bring the keyboard target into view.
pub fn instrument_tree_menu(
    search_field: AnyElement,
    tree: &InstrumentTreeState,
    rows: Vec<InstrumentRow>,
    selected: Option<&str>,
    searching: bool,
    callbacks: InstrumentTreeCallbacks,
) -> AnyElement {
    let highlight = effective_highlight(&rows, tree.highlight, selected, searching);
    div()
        .flex()
        .flex_col()
        .min_h(px(0.0))
        .flex_shrink_1()
        .child(
            div()
                .flex_shrink_0()
                .flex()
                .flex_col()
                .gap(px(space::SNUG))
                .p(px(space::HAIR))
                .pb(px(space::SNUG))
                .child(search_field)
                .child(grouping_switch(
                    tree.grouping,
                    callbacks.on_grouping.clone(),
                )),
        )
        .child(
            div()
                .id("add-track-instrument-tree")
                .flex()
                .flex_col()
                .gap(px(menu::ITEM_GAP))
                .min_h(px(0.0))
                .flex_shrink_1()
                .pt(px(space::TIGHT))
                .border_t(px(1.0))
                .border_color(Colors::border_subtle())
                .overflow_y_scroll()
                .track_scroll(&tree.scroll)
                .children(rows.into_iter().enumerate().map(|(index, row)| {
                    render_row(index, row, selected, highlight == Some(index), &callbacks)
                })),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use SpherePluginHost::{PluginKind, PluginScanStatus, PluginStatus};

    fn plugin(id: &str, name: &str, vendor: &str, format: PluginFormat) -> RegistryPlugin {
        RegistryPlugin {
            id: id.into(),
            name: name.into(),
            vendor: vendor.into(),
            format,
            category: "Instrument".into(),
            raw_category: None,
            sub_categories: None,
            kind: PluginKind::Instrument,
            path: PathBuf::new(),
            class_id: None,
            version: None,
            sdk_metadata_loaded: true,
            is_ara: false,
            midi_output: false,
            preset_path: PathBuf::new(),
            scanned_at_ms: 0,
            status: PluginStatus::PresetReady,
            scan_status: PluginScanStatus::Success,
            error_message: None,
        }
    }

    fn library() -> Vec<RegistryPlugin> {
        vec![
            plugin(
                "ad2-vst3",
                "Addictive Drums 2",
                "XLN Audio",
                PluginFormat::Vst3,
            ),
            plugin(
                "ad2-vst2",
                "Addictive Drums 2",
                "XLN Audio",
                PluginFormat::Vst2,
            ),
            plugin(
                "lp-vst3",
                "Ample Guitar LP",
                "Ample Sound",
                PluginFormat::Vst3,
            ),
            plugin(
                "surge-clap",
                "Surge XT",
                "Surge Synth Team",
                PluginFormat::Clap,
            ),
            plugin("mystery", "Mystery Synth", "", PluginFormat::Vst3),
        ]
    }

    fn labels(rows: &[InstrumentRow]) -> Vec<String> {
        rows.iter()
            .map(|row| match row {
                InstrumentRow::NoInstrument { .. } => "-".into(),
                InstrumentRow::Group { label, count, .. } => format!("[{label} {count}]"),
                InstrumentRow::Plugin { label, .. } => label.clone(),
                InstrumentRow::NoMatches => "?".into(),
            })
            .collect()
    }

    #[test]
    fn vendor_mode_folders_vendors_with_unknown_last() {
        let rows = instrument_rows(
            &library(),
            InstrumentGrouping::Vendor,
            "",
            None,
            &HashSet::new(),
        );
        assert_eq!(
            labels(&rows),
            [
                "-",
                "[Ample Sound 1]",
                "Ample Guitar LP",
                "[Surge Synth Team 1]",
                "Surge XT",
                "[XLN Audio 2]",
                "Addictive Drums 2 (VST3)",
                "Addictive Drums 2 (VST2)",
                "[Unknown Vendor 1]",
                "Mystery Synth",
            ]
        );
    }

    #[test]
    fn format_mode_orders_formats_and_name_mode_is_flat() {
        let rows = instrument_rows(
            &library(),
            InstrumentGrouping::Format,
            "",
            None,
            &HashSet::new(),
        );
        let groups: Vec<_> = labels(&rows)
            .into_iter()
            .filter(|label| label.starts_with('['))
            .collect();
        assert_eq!(groups, ["[VST3 3]", "[CLAP 1]", "[VST2 1]"]);

        let rows = instrument_rows(
            &library(),
            InstrumentGrouping::Name,
            "",
            None,
            &HashSet::new(),
        );
        assert!(rows
            .iter()
            .all(|row| !matches!(row, InstrumentRow::Group { .. })));
        assert_eq!(labels(&rows)[1], "Addictive Drums 2 (VST3)");
    }

    #[test]
    fn toggling_collapses_and_search_reopens_matching_folders() {
        let library = library();
        let rows = instrument_rows(
            &library,
            InstrumentGrouping::Vendor,
            "",
            None,
            &HashSet::new(),
        );
        let InstrumentRow::Group { key, .. } = &rows[5] else {
            panic!("expected the XLN folder");
        };
        let toggled: HashSet<String> = [key.clone()].into();
        let collapsed = instrument_rows(&library, InstrumentGrouping::Vendor, "", None, &toggled);
        assert_eq!(collapsed.len(), rows.len() - 2);

        let searched = instrument_rows(
            &library,
            InstrumentGrouping::Vendor,
            "drums vst2",
            None,
            &toggled,
        );
        assert_eq!(
            labels(&searched),
            ["-", "[XLN Audio 1]", "Addictive Drums 2"]
        );
        assert_eq!(
            effective_highlight(&searched, None, None, true),
            Some(2),
            "while searching, Enter picks the first match"
        );
        assert_eq!(
            effective_highlight(&rows, None, Some("lp-vst3"), false),
            Some(2)
        );
        assert_eq!(effective_highlight(&rows, None, None, false), Some(0));
    }

    #[test]
    fn large_vendor_lists_open_only_the_selected_folder() {
        let many: Vec<_> = (0..10)
            .map(|i| {
                plugin(
                    &format!("p{i}"),
                    &format!("Synth {i}"),
                    &format!("Vendor {i}"),
                    PluginFormat::Vst3,
                )
            })
            .collect();
        let rows = instrument_rows(
            &many,
            InstrumentGrouping::Vendor,
            "",
            Some("p3"),
            &HashSet::new(),
        );
        let open: Vec<_> = rows
            .iter()
            .filter_map(|row| match row {
                InstrumentRow::Group {
                    label,
                    expanded: true,
                    ..
                } => Some(label.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(open, ["Vendor 3"]);
    }

    #[test]
    fn no_match_shows_a_single_disabled_row_under_no_instrument() {
        let rows = instrument_rows(
            &library(),
            InstrumentGrouping::Format,
            "zzz",
            None,
            &HashSet::new(),
        );
        assert_eq!(labels(&rows), ["-", "?"]);
    }
}

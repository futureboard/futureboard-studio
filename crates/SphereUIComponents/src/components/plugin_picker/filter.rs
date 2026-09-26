//! Filter counts and multi-filter matching.

use std::collections::BTreeMap;

use SpherePluginHost::{PluginFormat, PluginKind, PluginScanStatus, RegistryPlugin};

use crate::components::plugin_picker::category::normalized_category_label;
use crate::components::plugin_picker::prefs::PluginPickerPrefs;
use crate::components::plugin_picker::search_index::PluginSearchIndex;
use crate::components::plugin_picker::state::{PickerFilter, PluginFilterState};

#[derive(Debug, Clone, Default)]
pub struct FilterCounts {
    /// Every plug-in in the library, whatever the kind tab — the "All" tab.
    pub library: usize,
    /// Library-wide instruments — the "Instruments" tab.
    pub instruments: usize,
    /// Everything the Effects tab offers — declared effects plus plug-ins that
    /// declared no class at all, which are insertable as effects.
    pub effects: usize,
    /// Subset of `effects` that never declared a class.
    pub unknown: usize,
    // Everything below counts only plug-ins under the active kind tab, so the
    // rail says what clicking it would show.
    pub all: usize,
    pub favorites: usize,
    pub recent: usize,
    pub vst3: usize,
    pub vst2: usize,
    pub clap: usize,
    pub au: usize,
    pub builtin: usize,
    pub failed: usize,
    /// Per entry of `FilterResult::vendors`.
    pub vendors: Vec<usize>,
    /// Per entry of `FilterResult::categories`.
    pub categories: Vec<usize>,
}

#[derive(Debug, Clone)]
pub struct FilterResult {
    pub indices: Vec<usize>,
    pub counts: FilterCounts,
    pub vendors: Vec<String>,
    pub categories: Vec<String>,
}

/// Cached `FUTUREBOARD_PLUGIN_PICKER_DEBUG` flag — gates the picker perf timing
/// lines used to localize typing-latency. Read once, not per keypress.
pub fn picker_perf_debug() -> bool {
    use std::sync::OnceLock;
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| std::env::var_os("FUTUREBOARD_PLUGIN_PICKER_DEBUG").is_some())
}

pub fn compute_filter_result(
    index: &PluginSearchIndex,
    query: &str,
    filters: &PluginFilterState,
    prefs: &PluginPickerPrefs,
    debug_mode: bool,
) -> FilterResult {
    let perf = picker_perf_debug();
    let started = perf.then(std::time::Instant::now);
    let plugins = index.plugins();
    let q = query.trim().to_ascii_lowercase();
    let vendor_filter = filters
        .vendor
        .as_ref()
        .map(|vendor| vendor.to_ascii_lowercase());
    let category_filter = filters
        .category
        .as_ref()
        .map(|category| category.to_ascii_lowercase());

    let mut counts = FilterCounts {
        vendors: vec![0; index.sidebar_vendors().len()],
        categories: vec![0; index.sidebar_categories().len()],
        ..FilterCounts::default()
    };
    let mut indices = Vec::new();

    // One pass: tally the tab and rail counts and collect the matching rows.
    // The facet labels themselves are precomputed on the shared index, and
    // each plug-in's facet position is too, so typing only pays for the cheap
    // checks below, never per-keypress String work.
    for (idx, plugin) in plugins.iter().enumerate() {
        update_tab_counts(&mut counts, plugin);
        if !filters.kind.matches(plugin) {
            continue;
        }
        update_rail_counts(&mut counts, index, idx, plugin, prefs, debug_mode);

        if !matches_sidebar(&filters.sidebar, plugin, prefs, debug_mode) {
            continue;
        }
        // Sidebar Format / Built-in already owns format identity. A stale
        // secondary `filters.format` (e.g. leftover VST3 pin) must not empty
        // the Built-in or CLAP lists while their sidebar counts stay nonzero.
        let sidebar_owns_format = matches!(
            filters.sidebar,
            PickerFilter::Format(_) | PickerFilter::Builtin
        );
        if let Some(fmt) = filters.format {
            if !sidebar_owns_format && plugin.format != fmt {
                continue;
            }
        }
        if let Some(vendor) = vendor_filter.as_deref() {
            if index.vendor_lower(idx) != vendor {
                continue;
            }
        }
        if let Some(category) = category_filter.as_deref() {
            if index.category_lower(idx) != category {
                continue;
            }
        }
        if !q.is_empty() && !index.search_text(idx).contains(&q) {
            continue;
        }
        indices.push(idx);
    }

    if let Some(started) = started {
        eprintln!(
            "[picker-perf] compute_filter_result q={:?} plugins={} results={} took_us={}",
            q,
            plugins.len(),
            indices.len(),
            started.elapsed().as_micros()
        );
    }

    FilterResult {
        indices,
        counts,
        vendors: index.sidebar_vendors().to_vec(),
        categories: index.sidebar_categories().to_vec(),
    }
}

fn update_tab_counts(counts: &mut FilterCounts, plugin: &RegistryPlugin) {
    counts.library += 1;
    if plugin.kind == PluginKind::Instrument {
        counts.instruments += 1;
    } else {
        counts.effects += 1;
        if plugin.kind == PluginKind::Unknown {
            counts.unknown += 1;
        }
    }
}

fn update_rail_counts(
    counts: &mut FilterCounts,
    index: &PluginSearchIndex,
    idx: usize,
    plugin: &RegistryPlugin,
    prefs: &PluginPickerPrefs,
    debug_mode: bool,
) {
    counts.all += 1;
    if prefs.is_favorite(&plugin.id) {
        counts.favorites += 1;
    }
    if prefs.recent.contains(&plugin.id) {
        counts.recent += 1;
    }
    if plugin.is_builtin() {
        counts.builtin += 1;
    } else {
        match plugin.format {
            PluginFormat::Vst3 => counts.vst3 += 1,
            PluginFormat::Vst2 => counts.vst2 += 1,
            PluginFormat::Clap => counts.clap += 1,
            PluginFormat::Au => counts.au += 1,
            _ => {}
        }
    }
    if let Some(slot) = index.vendor_slot(idx) {
        counts.vendors[slot] += 1;
    }
    if let Some(slot) = index.category_slot(idx) {
        counts.categories[slot] += 1;
    }
    if debug_mode && is_failed_plugin(plugin) {
        counts.failed += 1;
    }
}
fn is_failed_plugin(plugin: &RegistryPlugin) -> bool {
    !plugin.scan_status.is_usable()
        || matches!(
            plugin.scan_status,
            PluginScanStatus::Failed | PluginScanStatus::Crashed | PluginScanStatus::MetadataOnly
        )
}

fn matches_sidebar(
    filter: &PickerFilter,
    plugin: &RegistryPlugin,
    prefs: &PluginPickerPrefs,
    debug_mode: bool,
) -> bool {
    match filter {
        PickerFilter::All => true,
        PickerFilter::Favorites => prefs.is_favorite(&plugin.id),
        PickerFilter::RecentlyUsed => prefs.recent.contains(&plugin.id),
        PickerFilter::Instruments => plugin.kind == PluginKind::Instrument,
        // `usable_as_effect` keeps undeclared plug-ins reachable: the picker
        // opens pre-filtered to Effects for an effect slot, so anything hidden
        // here is unreachable in the flow that needs it.
        PickerFilter::Effects => plugin.kind.usable_as_effect(),
        PickerFilter::Format(fmt) => !plugin.is_builtin() && plugin.format == *fmt,
        PickerFilter::Builtin => plugin.is_builtin(),
        PickerFilter::Vendor(v) => plugin.vendor.eq_ignore_ascii_case(v),
        PickerFilter::Category(c) => normalized_category_label(plugin).eq_ignore_ascii_case(c),
        PickerFilter::Failed => debug_mode && is_failed_plugin(plugin),
    }
}

pub fn vendor_counts(plugins: &[RegistryPlugin]) -> BTreeMap<String, usize> {
    let mut map = BTreeMap::new();
    for plugin in plugins {
        if plugin.vendor.is_empty() {
            continue;
        }
        *map.entry(plugin.vendor.clone()).or_insert(0) += 1;
    }
    map
}

#[cfg(test)]
mod builtin_filter_tests {
    use super::*;
    use crate::components::plugin_picker::search_index::PluginSearchIndex;
    use crate::components::plugin_picker::state::PickerFilter;
    use SpherePluginHost::with_builtins;

    /// The whole stock catalog, and its size. Taken from the catalog itself:
    /// what these tests assert is that the Built-in sidebar shows *every*
    /// built-in, which a literal count silently turns into "shows however many
    /// there were when this was written".
    fn builtin_catalog() -> (Vec<RegistryPlugin>, usize) {
        let catalog = with_builtins(Vec::new(), 0);
        let count = catalog.len();
        assert!(count > 0, "the built-in catalog is never empty");
        (catalog, count)
    }

    #[test]
    fn builtin_sidebar_lists_stock_plugins() {
        let (catalog, count) = builtin_catalog();
        let index = PluginSearchIndex::from_plugins(catalog);
        let prefs = PluginPickerPrefs::default_with_size();
        let filters = PluginFilterState {
            sidebar: PickerFilter::Builtin,
            format: None,
            ..Default::default()
        };
        let result = compute_filter_result(&index, "", &filters, &prefs, false);
        assert_eq!(result.counts.builtin, count);
        assert_eq!(result.indices.len(), count);
        assert!(result
            .indices
            .iter()
            .all(|&i| index.plugin_at(i).is_some_and(|p| p.is_builtin())));
    }

    #[test]
    fn stale_vst3_secondary_format_must_not_hide_builtins() {
        // Regression: open_insert_picker used to pin filters.format=Vst3, which
        // zeroed the Built-in list while the sidebar count stayed at the full
        // catalog size.
        let (catalog, count) = builtin_catalog();
        let index = PluginSearchIndex::from_plugins(catalog);
        let prefs = PluginPickerPrefs::default_with_size();
        let filters = PluginFilterState {
            sidebar: PickerFilter::Builtin,
            format: Some(PluginFormat::Vst3),
            ..Default::default()
        };
        let result = compute_filter_result(&index, "", &filters, &prefs, false);
        assert_eq!(result.counts.builtin, count);
        assert_eq!(result.indices.len(), count);
    }
}

#[cfg(test)]
mod perf_tests {
    use super::*;
    use crate::components::plugin_picker::search_index::PluginSearchIndex;
    use crate::components::plugin_picker::state::PickerFilter;
    use std::path::PathBuf;
    use std::time::Instant;
    use SpherePluginHost::PluginStatus;

    fn synth_plugins(n: usize) -> Vec<RegistryPlugin> {
        let vendors = [
            "FabFilter",
            "Acme",
            "Antares",
            "IK Multimedia",
            "Ample Sound",
            "Celemony",
            "u-he",
            "Valhalla",
            "Native Instruments",
            "Waves",
        ];
        let cats = [
            "EQ", "Dynamics", "Reverb", "Delay", "Analyzer", "Synth", "Other",
        ];
        (0..n)
            .map(|i| RegistryPlugin {
                id: format!("vendor.plugin.{i}"),
                name: format!("MiniMeters - Audio Scope {i}"),
                vendor: vendors[i % vendors.len()].to_string(),
                format: if i % 3 == 0 {
                    PluginFormat::Clap
                } else {
                    PluginFormat::Vst3
                },
                category: cats[i % cats.len()].to_string(),
                is_ara: false,
                raw_category: Some("Fx|Analyzer".to_string()),
                sub_categories: Some("Fx|Analyzer".to_string()),
                kind: if i % 7 == 0 {
                    PluginKind::Instrument
                } else {
                    PluginKind::Effect
                },
                path: PathBuf::from(format!("C:/Plugins/Plugin{i}.vst3")),
                class_id: Some(format!("com.vendor.plugin{i}")),
                version: Some("1.0.0".to_string()),
                sdk_metadata_loaded: true,
                preset_path: PathBuf::from(format!("C:/Cache/{i}.pst")),
                scanned_at_ms: 0,
                status: PluginStatus::PresetReady,
                scan_status: PluginScanStatus::Success,
                error_message: None,
            })
            .collect()
    }

    // Run with: cargo test -p sphere_ui_components picker_filter_cost -- --nocapture
    #[test]
    fn picker_filter_cost() {
        let plugins = synth_plugins(1031);
        let t = Instant::now();
        let index = PluginSearchIndex::from_plugins(plugins);
        eprintln!(
            "[perf-test] index build (1031): {} us",
            t.elapsed().as_micros()
        );

        let t = Instant::now();
        let _clone = index.clone();
        eprintln!(
            "[perf-test] index DEEP clone (old per-keystroke cost): {} us",
            t.elapsed().as_micros()
        );

        let prefs = PluginPickerPrefs::default_with_size();
        let filters = PluginFilterState {
            sidebar: PickerFilter::Effects,
            ..Default::default()
        };

        for q in ["", "eq", "zzzznomatch"] {
            // Average over a few runs to smooth scheduler noise.
            let runs = 20;
            let t = Instant::now();
            let mut last = 0;
            for _ in 0..runs {
                last = compute_filter_result(&index, q, &filters, &prefs, false)
                    .indices
                    .len();
            }
            eprintln!(
                "[perf-test] compute_filter_result q={q:?} results={last} avg={} us ({runs} runs)",
                t.elapsed().as_micros() / runs
            );
        }
    }
}

#[cfg(test)]
mod kind_tab_tests {
    use super::*;
    use crate::components::plugin_picker::search_index::PluginSearchIndex;
    use crate::components::plugin_picker::state::{KindTab, PickerFilter};
    use std::path::PathBuf;
    use SpherePluginHost::PluginStatus;

    fn plugin(id: &str, vendor: &str, kind: PluginKind) -> RegistryPlugin {
        RegistryPlugin {
            id: id.to_string(),
            name: id.to_string(),
            vendor: vendor.to_string(),
            format: PluginFormat::Vst3,
            category: "EQ".to_string(),
            is_ara: false,
            raw_category: None,
            sub_categories: None,
            kind,
            path: PathBuf::from(format!("C:/Plugins/{id}.vst3")),
            class_id: None,
            version: None,
            sdk_metadata_loaded: true,
            preset_path: PathBuf::from(format!("C:/Cache/{id}.pst")),
            scanned_at_ms: 0,
            status: PluginStatus::PresetReady,
            scan_status: PluginScanStatus::Success,
            error_message: None,
        }
    }

    fn library() -> PluginSearchIndex {
        PluginSearchIndex::from_plugins(vec![
            plugin("synth", "Acme", PluginKind::Instrument),
            plugin("eq", "Acme", PluginKind::Effect),
            plugin("comp", "Zeta", PluginKind::Effect),
            plugin("mystery", "Zeta", PluginKind::Unknown),
        ])
    }

    /// The tab narrows the list and every rail count; the tabs themselves
    /// always count the whole library, so switching never looks empty.
    #[test]
    fn the_kind_tab_scopes_the_list_and_the_rail() {
        let index = library();
        let prefs = PluginPickerPrefs::default_with_size();
        let filters = PluginFilterState {
            kind: KindTab::Effects,
            ..Default::default()
        };
        let result = compute_filter_result(&index, "", &filters, &prefs, false);
        // An undeclared plug-in inserts as an effect, so it is listed here.
        assert_eq!(result.indices.len(), 3);
        assert_eq!(result.counts.all, 3);
        assert_eq!(result.counts.library, 4);
        assert_eq!(result.counts.instruments, 1);
        assert_eq!(result.counts.effects, 3);
        let acme = result.vendors.iter().position(|v| v == "Acme").unwrap();
        let zeta = result.vendors.iter().position(|v| v == "Zeta").unwrap();
        assert_eq!(
            result.counts.vendors[acme], 1,
            "the Acme synth is not an effect"
        );
        assert_eq!(result.counts.vendors[zeta], 2);
    }

    #[test]
    fn the_rail_composes_with_the_tab() {
        let index = library();
        let prefs = PluginPickerPrefs::default_with_size();
        let filters = PluginFilterState {
            kind: KindTab::Instruments,
            sidebar: PickerFilter::Vendor("Acme".to_string()),
            ..Default::default()
        };
        let result = compute_filter_result(&index, "", &filters, &prefs, false);
        let names: Vec<&str> = result
            .indices
            .iter()
            .map(|&i| index.plugin_at(i).unwrap().name.as_str())
            .collect();
        assert_eq!(names, vec!["synth"]);
    }
}

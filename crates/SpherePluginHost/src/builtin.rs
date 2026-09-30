//! Built-in (stock) plugin catalog.
//!
//! Futureboard's own DSP cores (`crates/BuiltinAudioPlugins`) are surfaced to the
//! plug-in manager and Add-Track dialog as ordinary [`RegistryPlugin`] rows, so
//! the existing `Vec<RegistryPlugin>` UI pipelines list them next to scanned
//! VST3/CLAP plug-ins with **no new [`PluginFormat`] variant** (that enum is
//! matched exhaustively in ~150 places). Built-ins are identified by a
//! `builtin:` id prefix via [`is_builtin_id`] / [`RegistryPlugin::is_builtin`].
//!
//! Each built-in with a React editor is served to CEF through the shared
//! `mikoplugin://<plugin>/index.html` scheme; [`builtin_editor_url`] builds it.
//! This module is pure data + string mapping — no DSP crate dependency, so the
//! host crate stays lean.

use crate::plugin_db::PluginScanStatus;
use crate::registry::{PluginFormat, PluginKind, PluginStatus, RegistryPlugin};

/// Id prefix that marks a registry row as a Futureboard built-in.
pub const BUILTIN_ID_PREFIX: &str = "builtin:";

/// Custom URL scheme the shared CEF host uses for embedded plugin editors. Must
/// stay in sync with `builtin_audio_plugins::ui::PLUGIN_URL_SCHEME` (kept as a
/// literal here so the host crate does not depend on the DSP umbrella crate).
pub const PLUGIN_URL_SCHEME: &str = "mikoplugin";

/// One entry in the curated built-in catalog.
struct BuiltinEntry {
    /// Library / plugin id stem (also the `mikoplugin://<stem>` origin).
    stem: &'static str,
    name: &'static str,
    category: &'static str,
    kind: PluginKind,
    /// Whether the plugin ships an embeddable React editor (`editor/` or
    /// `editorui/` — both bundle layouts are in use).
    has_editor: bool,
}

/// The curated built-in catalog. Mirrors the workspace members under
/// `crates/BuiltinAudioPlugins/crates`.
const CATALOG: &[BuiltinEntry] = &[
    BuiltinEntry {
        stem: "rodharerist",
        name: "Rodhareist",
        category: "Multi-FX",
        kind: PluginKind::Effect,
        has_editor: true,
    },
    BuiltinEntry {
        stem: "equz8",
        name: "EQ-Z8",
        category: "EQ",
        kind: PluginKind::Effect,
        has_editor: true,
    },
    BuiltinEntry {
        stem: "compresser",
        name: "Compressor",
        category: "Dynamics",
        kind: PluginKind::Effect,
        has_editor: true,
    },
    BuiltinEntry {
        stem: "fa2a",
        name: "FA-2A",
        category: "Dynamics",
        kind: PluginKind::Effect,
        has_editor: true,
    },
    BuiltinEntry {
        stem: "zcomp",
        name: "Z-Comp",
        category: "Dynamics",
        kind: PluginKind::Effect,
        has_editor: true,
    },
    BuiltinEntry {
        stem: "mixstation",
        name: "MixStation",
        category: "Effect",
        kind: PluginKind::Effect,
        has_editor: true,
    },
    BuiltinEntry {
        stem: "echospace",
        name: "EchoSpace",
        category: "Delay",
        kind: PluginKind::Effect,
        has_editor: true,
    },
    BuiltinEntry {
        stem: "verbspace",
        name: "VerbSpace",
        category: "Reverb",
        kind: PluginKind::Effect,
        has_editor: true,
    },
    BuiltinEntry {
        stem: "imager",
        name: "Imager",
        category: "Utility",
        kind: PluginKind::Effect,
        has_editor: true,
    },
    BuiltinEntry {
        stem: "fa76",
        name: "FA-76",
        category: "Dynamics",
        kind: PluginKind::Effect,
        has_editor: true,
    },
    BuiltinEntry {
        stem: "burnlimit",
        name: "BurnLimit",
        category: "Dynamics",
        kind: PluginKind::Effect,
        has_editor: true,
    },
    BuiltinEntry {
        stem: "clipper67",
        name: "67Clipper",
        category: "Dynamics",
        kind: PluginKind::Effect,
        has_editor: true,
    },
    BuiltinEntry {
        stem: "transient",
        name: "Transient",
        category: "Dynamics",
        kind: PluginKind::Effect,
        has_editor: true,
    },
    BuiltinEntry {
        stem: "c1073",
        name: "C1073",
        category: "EQ",
        kind: PluginKind::Effect,
        has_editor: false,
    },
    BuiltinEntry {
        stem: "meowsyn",
        name: "MeowSyn",
        category: "Instrument",
        kind: PluginKind::Instrument,
        has_editor: false,
    },
    BuiltinEntry {
        stem: "wrapsynth",
        name: "WrapSynth",
        category: "Instrument",
        kind: PluginKind::Instrument,
        has_editor: true,
    },
    BuiltinEntry {
        stem: "drumsampler",
        name: "Drum Sampler",
        category: "Instrument",
        kind: PluginKind::Instrument,
        has_editor: true,
    },
];

const VENDOR: &str = "Futureboard";

/// The registry id for a built-in stem, e.g. `builtin:rodharerist`.
pub fn builtin_id(stem: &str) -> String {
    format!("{BUILTIN_ID_PREFIX}{stem}")
}

/// Whether a registry id denotes a built-in plug-in.
pub fn is_builtin_id(id: &str) -> bool {
    id.starts_with(BUILTIN_ID_PREFIX)
}

/// The stem (`rodharerist`) for a built-in id (`builtin:rodharerist`).
pub fn builtin_stem(id: &str) -> Option<&str> {
    id.strip_prefix(BUILTIN_ID_PREFIX)
}

/// Resolve either identifier form a built-in travels under to its catalog stem.
///
/// A built-in is referred to by two different strings depending on where you
/// are:
///
/// * `builtin:rodharerist` — the [`RegistryPlugin::id`], used by the plugin
///   picker and catalog;
/// * `rodharerist` — the [`RegistryPlugin::class_id`], which is what an insert
///   slot stores in its `plugin_id` field (`apply_picked_insert` prefers
///   `class_id` over `id`, because for VST3 that is the controller class).
///
/// Anything reached from an insert slot therefore cannot use
/// [`is_builtin_id`] alone. Matching is validated against [`CATALOG`] rather
/// than by shape, so an external plug-in whose class id merely lacks a prefix
/// is never mistaken for a built-in.
pub fn resolve_builtin_stem(id: &str) -> Option<&'static str> {
    let candidate = builtin_stem(id).unwrap_or(id);
    CATALOG
        .iter()
        .find(|entry| entry.stem == candidate)
        .map(|entry| entry.stem)
}

/// Whether `id` denotes a built-in in **either** identifier form. Use this, not
/// [`is_builtin_id`], for anything sourced from an insert slot.
pub fn is_builtin_ref(id: &str) -> bool {
    resolve_builtin_stem(id).is_some()
}

/// Built-ins the plugin-host process can actually instantiate a DSP for.
///
/// Keep this narrower than the catalog: an unsupported built-in must retain its
/// existing path instead of being routed to a host that cannot instantiate it.
/// Must stay in sync with `BuiltinHostProcessor::new` in the host binary — the
/// two are covered by [`tests::bridge_supported_builtins_are_catalogued`] here
/// and by the host's own construction test.
pub const AUDIO_BRIDGE_STEMS: &[&str] = &[
    "rodharerist",
    "equz8",
    "verbspace",
    "echospace",
    "imager",
    "fa2a",
    "fa76",
    "burnlimit",
    "clipper67",
    "transient",
    "wrapsynth",
    "drumsampler",
    "zcomp",
    "mixstation",
    "compresser",
];

/// Whether this built-in currently has an out-of-process audio DSP runtime.
pub fn builtin_audio_bridge_supported(id: &str) -> bool {
    resolve_builtin_stem(id).is_some_and(|stem| AUDIO_BRIDGE_STEMS.contains(&stem))
}

/// The catalog display name for a built-in id, in either identifier form.
/// Used by the plugin host so a load event reports the plugin the user picked
/// rather than a name hard-coded at the load site.
pub fn builtin_display_name(id: &str) -> Option<&'static str> {
    let stem = resolve_builtin_stem(id)?;
    CATALOG
        .iter()
        .find(|entry| entry.stem == stem)
        .map(|entry| entry.name)
}

/// The `mikoplugin://<stem>/index.html` editor URL for a built-in id, or `None`
/// when the id is not a built-in or that built-in ships no editor.
pub fn builtin_editor_url(id: &str) -> Option<String> {
    let stem = builtin_stem(id)?;
    let entry = CATALOG.iter().find(|e| e.stem == stem)?;
    entry
        .has_editor
        .then(|| format!("{PLUGIN_URL_SCHEME}://{stem}/index.html"))
}

/// Whether a built-in id has an embeddable editor.
pub fn builtin_has_editor(id: &str) -> bool {
    builtin_editor_url(id).is_some()
}

/// Build the built-in catalog as `RegistryPlugin` rows the existing UI consumes.
///
/// `scanned_at_ms` is stamped so built-ins sort/merge consistently with scanned
/// rows. Built-ins are always `PresetReady` (they need no `.pst` on disk) and
/// carry the `Builtin`-less `Unknown` format with a `builtin:` id.
pub fn builtin_catalog(scanned_at_ms: i64) -> Vec<RegistryPlugin> {
    CATALOG
        .iter()
        .map(|entry| RegistryPlugin {
            id: builtin_id(entry.stem),
            name: entry.name.to_string(),
            vendor: VENDOR.to_string(),
            format: PluginFormat::Unknown,
            category: entry.category.to_string(),
            raw_category: Some(entry.category.to_string()),
            sub_categories: None,
            kind: entry.kind,
            path: std::path::PathBuf::new(),
            class_id: Some(entry.stem.to_string()),
            version: Some(env!("CARGO_PKG_VERSION").to_string()),
            sdk_metadata_loaded: true,
            // Stock plug-ins are hosted directly; none of them is an ARA peer.
            is_ara: false,
            preset_path: std::path::PathBuf::new(),
            scanned_at_ms,
            status: PluginStatus::PresetReady,
            scan_status: PluginScanStatus::Success,
            error_message: None,
        })
        .collect()
}

/// Merge the built-in catalog into a scanned catalog, de-duplicating by id so a
/// re-merge is idempotent. Built-ins are prepended (they lead the list).
pub fn with_builtins(mut scanned: Vec<RegistryPlugin>, scanned_at_ms: i64) -> Vec<RegistryPlugin> {
    scanned.retain(|plugin| !is_builtin_id(&plugin.id));
    let mut merged = builtin_catalog(scanned_at_ms);
    merged.extend(scanned);
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_ids_are_prefixed_and_unique() {
        let rows = builtin_catalog(0);
        assert_eq!(rows.len(), CATALOG.len());
        let mut ids: Vec<_> = rows.iter().map(|p| p.id.clone()).collect();
        assert!(ids.iter().all(|id| is_builtin_id(id)));
        ids.sort();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "duplicate built-in id");
    }

    #[test]
    fn builtins_are_insertable() {
        for plugin in builtin_catalog(0) {
            assert!(plugin.is_builtin());
            assert!(
                plugin.supports_insert(),
                "{} should be insertable",
                plugin.name
            );
        }
    }

    #[test]
    fn editors_are_limited_to_builtins_that_ship_one() {
        assert_eq!(
            builtin_editor_url(&builtin_id("rodharerist")).as_deref(),
            Some("mikoplugin://rodharerist/index.html")
        );
        assert_eq!(
            builtin_editor_url(&builtin_id("equz8")).as_deref(),
            Some("mikoplugin://equz8/index.html")
        );
        assert_eq!(
            builtin_editor_url(&builtin_id("verbspace")).as_deref(),
            Some("mikoplugin://verbspace/index.html")
        );
        assert_eq!(
            builtin_editor_url(&builtin_id("echospace")).as_deref(),
            Some("mikoplugin://echospace/index.html")
        );
        assert_eq!(
            builtin_editor_url(&builtin_id("imager")).as_deref(),
            Some("mikoplugin://imager/index.html")
        );
        assert_eq!(
            builtin_editor_url(&builtin_id("fa2a")).as_deref(),
            Some("mikoplugin://fa2a/index.html")
        );
        assert_eq!(
            builtin_editor_url(&builtin_id("zcomp")).as_deref(),
            Some("mikoplugin://zcomp/index.html")
        );
        assert_eq!(
            builtin_editor_url(&builtin_id("mixstation")).as_deref(),
            Some("mikoplugin://mixstation/index.html")
        );
        assert_eq!(
            builtin_editor_url(&builtin_id("fa76")).as_deref(),
            Some("mikoplugin://fa76/index.html")
        );
        assert_eq!(
            builtin_editor_url(&builtin_id("burnlimit")).as_deref(),
            Some("mikoplugin://burnlimit/index.html")
        );
        assert_eq!(
            builtin_editor_url(&builtin_id("clipper67")).as_deref(),
            Some("mikoplugin://clipper67/index.html")
        );
        assert_eq!(
            builtin_editor_url(&builtin_id("transient")).as_deref(),
            Some("mikoplugin://transient/index.html")
        );
        assert_eq!(
            builtin_editor_url(&builtin_id("compresser")).as_deref(),
            Some("mikoplugin://compresser/index.html")
        );
        assert!(builtin_editor_url(&builtin_id("c1073")).is_none());
        assert!(builtin_editor_url("vst3:whatever").is_none());
    }

    #[test]
    fn stem_round_trips() {
        assert_eq!(builtin_stem(&builtin_id("meowsyn")), Some("meowsyn"));
        assert_eq!(builtin_stem(&builtin_id("wrapsynth")), Some("wrapsynth"));
        assert!(builtin_stem("clap:foo").is_none());
    }

    #[test]
    fn audio_bridge_support_covers_both_id_forms_and_nothing_else() {
        assert!(builtin_audio_bridge_supported("rodharerist"));
        assert!(builtin_audio_bridge_supported("builtin:rodharerist"));
        assert!(builtin_audio_bridge_supported("equz8"));
        assert!(builtin_audio_bridge_supported("builtin:equz8"));
        assert!(builtin_audio_bridge_supported("verbspace"));
        assert!(builtin_audio_bridge_supported("builtin:verbspace"));
        assert!(builtin_audio_bridge_supported("echospace"));
        assert!(builtin_audio_bridge_supported("builtin:echospace"));
        assert!(builtin_audio_bridge_supported("imager"));
        assert!(builtin_audio_bridge_supported("builtin:imager"));
        assert!(builtin_audio_bridge_supported("fa2a"));
        assert!(builtin_audio_bridge_supported("builtin:fa2a"));
        assert!(builtin_audio_bridge_supported("zcomp"));
        assert!(builtin_audio_bridge_supported("builtin:zcomp"));
        assert!(builtin_audio_bridge_supported("mixstation"));
        assert!(builtin_audio_bridge_supported("builtin:mixstation"));
        assert!(builtin_audio_bridge_supported("fa76"));
        assert!(builtin_audio_bridge_supported("builtin:fa76"));
        assert!(builtin_audio_bridge_supported("burnlimit"));
        assert!(builtin_audio_bridge_supported("builtin:burnlimit"));
        assert!(builtin_audio_bridge_supported("clipper67"));
        assert!(builtin_audio_bridge_supported("builtin:clipper67"));
        assert!(builtin_audio_bridge_supported("transient"));
        assert!(builtin_audio_bridge_supported("builtin:transient"));
        assert!(builtin_audio_bridge_supported("wrapsynth"));
        assert!(builtin_audio_bridge_supported("builtin:wrapsynth"));
        assert!(builtin_audio_bridge_supported("drumsampler"));
        assert!(builtin_audio_bridge_supported("builtin:drumsampler"));
        assert!(builtin_audio_bridge_supported("compresser"));
        assert!(builtin_audio_bridge_supported("builtin:compresser"));
        // Catalogued, but the host has no DSP for it — must keep its old path.
        assert!(!builtin_audio_bridge_supported("c1073"));
        assert!(!builtin_audio_bridge_supported("vst3:whatever"));
    }

    /// A typo here would silently route an insert to a host that cannot build
    /// its DSP, which surfaces as a load failure instead of a compile error.
    #[test]
    fn bridge_supported_builtins_are_catalogued() {
        for stem in AUDIO_BRIDGE_STEMS {
            assert_eq!(
                resolve_builtin_stem(stem),
                Some(*stem),
                "`{stem}` is bridge-enabled but not in the catalog"
            );
        }
    }

    #[test]
    fn display_names_resolve_from_either_id_form() {
        assert_eq!(builtin_display_name("builtin:equz8"), Some("EQ-Z8"));
        assert_eq!(builtin_display_name("equz8"), Some("EQ-Z8"));
        assert_eq!(builtin_display_name("rodharerist"), Some("Rodhareist"));
        assert_eq!(builtin_display_name("builtin:verbspace"), Some("VerbSpace"));
        assert_eq!(builtin_display_name("echospace"), Some("EchoSpace"));
        assert_eq!(builtin_display_name("builtin:imager"), Some("Imager"));
        assert_eq!(builtin_display_name("builtin:fa2a"), Some("FA-2A"));
        assert_eq!(builtin_display_name("builtin:zcomp"), Some("Z-Comp"));
        assert_eq!(
            builtin_display_name("builtin:mixstation"),
            Some("MixStation")
        );
        assert_eq!(builtin_display_name("builtin:fa76"), Some("FA-76"));
        assert_eq!(builtin_display_name("builtin:burnlimit"), Some("BurnLimit"));
        assert_eq!(builtin_display_name("builtin:clipper67"), Some("67Clipper"));
        assert_eq!(builtin_display_name("builtin:transient"), Some("Transient"));
        assert_eq!(builtin_display_name("builtin:wrapsynth"), Some("WrapSynth"));
        assert_eq!(builtin_display_name("compresser"), Some("Compressor"));
        assert_eq!(builtin_display_name("vst3:whatever"), None);
    }

    #[test]
    fn merge_is_idempotent() {
        let once = with_builtins(Vec::new(), 5);
        let twice = with_builtins(once.clone(), 5);
        assert_eq!(once.len(), twice.len());
        assert_eq!(once.len(), CATALOG.len());
    }

    #[test]
    fn merge_keeps_scanned_rows_after_builtins() {
        let scanned = vec![RegistryPlugin {
            id: "vst3:acme.synth".to_string(),
            name: "Acme".to_string(),
            vendor: "Acme".to_string(),
            format: PluginFormat::Vst3,
            category: "Instrument".to_string(),
            raw_category: None,
            sub_categories: None,
            kind: PluginKind::Instrument,
            path: std::path::PathBuf::from("acme.vst3"),
            class_id: None,
            version: None,
            sdk_metadata_loaded: true,
            is_ara: false,
            preset_path: std::path::PathBuf::new(),
            scanned_at_ms: 1,
            status: PluginStatus::PresetReady,
            scan_status: PluginScanStatus::Success,
            error_message: None,
        }];
        let merged = with_builtins(scanned, 0);
        assert!(merged.first().unwrap().is_builtin());
        assert_eq!(merged.last().unwrap().id, "vst3:acme.synth");
    }
}

use std::ffi::{CStr, CString};
use std::fs;
use std::os::raw::c_char;
use std::path::{Path, PathBuf};

use crate::types::PluginInfo;

#[repr(C)]
struct SpherePluginHostString {
    data: *const c_char,
    len: u64,
}

extern "C" {
    fn sphere_vst3_scan_path_json(path: *const c_char) -> SpherePluginHostString;
    fn sphere_clap_scan_path_json(path: *const c_char) -> SpherePluginHostString;
    fn sphere_vst2_scan_path_json(path: *const c_char) -> SpherePluginHostString;
    fn sphere_plugin_host_free_string(value: SpherePluginHostString);
    fn sphere_vst3_scan_left_work_running() -> bool;
}

/// Whether a VST3 scan in this process left a bus probe running inside a
/// plug-in past its deadline (a plug-in that does its whole start-up in
/// `initialize()`, like ZENOLOGY). Its rows were still reported, without a bus
/// layout. The process has to end without normal teardown once its result is
/// out: the abandoned thread is still inside the plug-in's module.
pub fn scan_left_work_running() -> bool {
    unsafe { sphere_vst3_scan_left_work_running() }
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativePluginInfo {
    pub name: String,
    pub vendor: String,
    pub category: String,
    pub format: String,
    pub path: String,
    #[serde(default)]
    pub sub_categories: Option<String>,
    #[serde(default)]
    pub module_path: Option<String>,
    #[serde(default)]
    pub class_id: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub sdk_version: Option<String>,
    #[serde(default)]
    pub is_shell_child: Option<bool>,
    /// Absent for scanners that predate ARA detection, and for the CLAP and
    /// VST2 backends, which never emit it.
    #[serde(default)]
    pub is_ara: bool,
    /// The VST3 component exposes an event output bus — read by instantiating
    /// it once during the scan. Absent (false) for older scanners and for the
    /// CLAP and VST2 backends, which classify MIDI effects by other means.
    #[serde(default)]
    pub midi_output: bool,
    pub sdk_metadata_loaded: bool,
    #[serde(default)]
    pub load_error: Option<String>,
}

pub fn scan_vst3_paths(paths: &[String]) -> Result<Vec<PluginInfo>, String> {
    scan_paths_for_format(paths, PluginFormat::Vst3)
}

pub fn scan_clap_paths(paths: &[String]) -> Result<Vec<PluginInfo>, String> {
    scan_paths_for_format(paths, PluginFormat::Clap)
}

pub fn scan_vst2_paths(paths: &[String]) -> Result<Vec<PluginInfo>, String> {
    scan_paths_for_format(paths, PluginFormat::Vst2)
}

#[allow(dead_code)]
pub fn scan_audio_plugin_paths(paths: &[String]) -> Result<Vec<PluginInfo>, String> {
    let mut plugins = scan_paths_for_format(paths, PluginFormat::Vst3)?;
    plugins.append(&mut scan_paths_for_format(paths, PluginFormat::Clap)?);
    plugins.append(&mut scan_paths_for_format(paths, PluginFormat::Vst2)?);
    sort_and_dedup(&mut plugins);
    Ok(plugins)
}

#[derive(Clone, Copy)]
enum PluginFormat {
    Vst3,
    Vst2,
    Clap,
}

impl PluginFormat {
    fn label(self) -> &'static str {
        match self {
            Self::Vst3 => "VST3",
            Self::Vst2 => "VST2",
            Self::Clap => "CLAP",
        }
    }

    /// Bundle extension for this format. VST2 only has one on macOS (`.vst`);
    /// on Windows a VST2 plug-in is a bare `.dll`, which is why VST2 is
    /// discovered by scanning folders rather than by matching bundle names.
    fn id_prefix(self) -> &'static str {
        match self {
            Self::Vst3 => "vst3",
            Self::Vst2 => "vst",
            Self::Clap => "clap",
        }
    }
}

fn scan_paths_for_format(
    paths: &[String],
    format: PluginFormat,
) -> Result<Vec<PluginInfo>, String> {
    let mut plugins = Vec::new();
    for path in paths {
        match scan_native_root(path, format) {
            Ok(mut native_plugins) => {
                plugins.append(&mut native_plugins);
                continue;
            }
            Err(_) => {
                // Keep scanning usable even if a malformed path cannot cross the C ABI.
            }
        }

        let root = PathBuf::from(path);
        if !root.exists() {
            continue;
        }
        collect_plugin_entries(&root, &mut plugins, format)?;
    }
    sort_and_dedup(&mut plugins);
    Ok(plugins)
}

fn sort_and_dedup(plugins: &mut Vec<PluginInfo>) {
    // Deduplicate by stable id (path + classId hash) so multi-class modules
    // like WaveShell keep all their plugin entries — only true duplicates
    // (same classId from the same module scanned twice) are removed.
    //
    // Use a seen-set, NOT `sort_by(name)` + `Vec::dedup_by(id)`: dedup_by only
    // collapses *consecutive* equal elements, so when two same-id entries are
    // separated by a different-id plugin that happens to share a display name
    // (common — many vendors ship "Compressor", "EQ", …), the sort-by-name
    // order leaves them non-adjacent and the duplicate survives. `retain` with
    // a seen-set is order-independent and keeps the first occurrence of each id.
    let mut seen = std::collections::HashSet::new();
    plugins.retain(|plugin| seen.insert(plugin.id.clone()));
    // Display order is alphabetical by name; done after dedup so ordering can
    // never affect which entries are removed.
    plugins.sort_by_key(|plugin| plugin.name.to_lowercase());
}

fn scan_native_root(path: &str, format: PluginFormat) -> Result<Vec<PluginInfo>, String> {
    let c_path = CString::new(path).map_err(|error| error.to_string())?;
    let native = unsafe {
        match format {
            PluginFormat::Vst3 => sphere_vst3_scan_path_json(c_path.as_ptr()),
            PluginFormat::Vst2 => sphere_vst2_scan_path_json(c_path.as_ptr()),
            PluginFormat::Clap => sphere_clap_scan_path_json(c_path.as_ptr()),
        }
    };
    if native.data.is_null() {
        return Err(format!(
            "{} scanner returned an empty native string",
            format.label()
        ));
    }

    let json = unsafe { CStr::from_ptr(native.data) }
        .to_string_lossy()
        .to_string();
    unsafe { sphere_plugin_host_free_string(native) };

    let scanned: Vec<NativePluginInfo> = serde_json::from_str(&json)
        .map_err(|error| format!("Invalid {} scanner JSON: {error}", format.label()))?;
    Ok(scanned
        .into_iter()
        .map(|plugin| {
            let id_source = plugin
                .class_id
                .as_ref()
                .map(|class_id| format!("{}:{class_id}", plugin.path))
                .unwrap_or_else(|| plugin.path.clone());
            let module_path = plugin.module_path.unwrap_or_else(|| plugin.path.clone());
            PluginInfo {
                id: stable_id(format.id_prefix(), &id_source),
                name: plugin.name,
                vendor: plugin.vendor,
                category: plugin.category,
                sub_categories: plugin.sub_categories,
                format: plugin.format,
                path: plugin.path,
                module_path: Some(module_path),
                class_id: plugin.class_id,
                version: plugin.version,
                sdk_version: plugin.sdk_version,
                is_shell_child: plugin.is_shell_child.unwrap_or(false),
                is_ara: plugin.is_ara,
                midi_output: plugin.midi_output,
                sdk_metadata_loaded: plugin.sdk_metadata_loaded,
                load_error: plugin.load_error,
            }
        })
        .collect())
}

fn collect_plugin_entries(
    path: &Path,
    plugins: &mut Vec<PluginInfo>,
    format: PluginFormat,
) -> Result<(), String> {
    if is_plugin_bundle(path, format) {
        plugins.push(plugin_from_path(path, format));
        return Ok(());
    }

    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) => return Err(format!("Failed to read {}: {error}", path.display())),
    };

    for entry in entries.flatten() {
        let p = entry.path();
        // A bundle is a leaf: `X.vst3/Contents/x86_64-win/X.vst3` shares the
        // extension with its own directory, so descending into a matched bundle
        // reports the same module twice under two different paths — and the two
        // rows get different ids, so dedup never collapsed them.
        if is_plugin_bundle(&p, format) {
            plugins.push(plugin_from_path(&p, format));
            continue;
        }
        if p.is_dir() {
            let _ = collect_plugin_entries(&p, plugins, format);
        }
    }
    Ok(())
}

pub fn discover_plugin_bundles(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for root in roots {
        if !root.exists() {
            continue;
        }
        discover_bundles_recursive(root, &mut found);
    }
    found.sort();
    found.dedup();
    found
}

/// Formats whose plug-ins are addressable as a single bundle path, so the
/// isolated scanner can be pointed at one plug-in at a time.
///
/// VST2 qualifies only on macOS, where a plug-in is a `.vst` bundle. On Windows
/// it is a bare `.dll` that cannot be told apart from any other library by name,
/// so VST2 there is discovered by scanning whole folders instead.
const BUNDLE_FORMATS: &[PluginFormat] = if cfg!(target_os = "macos") {
    &[PluginFormat::Vst3, PluginFormat::Clap, PluginFormat::Vst2]
} else {
    &[PluginFormat::Vst3, PluginFormat::Clap]
};

fn is_any_plugin_bundle(path: &Path) -> bool {
    BUNDLE_FORMATS
        .iter()
        .any(|format| is_plugin_bundle(path, *format))
        || is_windows_vst2_candidate(path)
}

/// A Windows VST2 plug-in is a bare `.dll`, indistinguishable from a support
/// library by name. Every `.dll` under a scan root is therefore offered to the
/// isolated scanner, which loads it, probes for a VST2 entry point, and returns
/// nothing when it is not a plug-in. That subprocess-per-file cost is the price
/// of the format having no manifest; it is also what keeps a malformed DLL from
/// taking down the app.
fn is_windows_vst2_candidate(path: &Path) -> bool {
    cfg!(target_os = "windows")
        && path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("dll") || ext.eq_ignore_ascii_case("vst2"))
}

fn discover_bundles_recursive(current: &Path, found: &mut Vec<PathBuf>) {
    if is_any_plugin_bundle(current) {
        found.push(current.to_path_buf());
        return;
    }
    if !current.is_dir() {
        return;
    }
    let Ok(entries) = fs::read_dir(current) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if is_any_plugin_bundle(&path) {
            found.push(path);
            continue;
        }
        if path.is_dir() {
            discover_bundles_recursive(&path, found);
        }
    }
}

/// Scan one plug-in bundle (`.vst3`, `.clap`, or `.vst` on macOS) via the
/// native metadata scanner.
pub fn scan_plugin_bundle(bundle_path: &Path) -> Result<Vec<PluginInfo>, String> {
    let path = bundle_path.to_string_lossy().into_owned();
    for format in BUNDLE_FORMATS {
        if is_plugin_bundle(bundle_path, *format) {
            return scan_native_root(&path, *format);
        }
    }
    if is_windows_vst2_candidate(bundle_path) {
        return scan_native_root(&path, PluginFormat::Vst2);
    }
    Err(format!("Not a plug-in bundle: {}", bundle_path.display()))
}

fn is_plugin_bundle(path: &Path, format: PluginFormat) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case(format.id_prefix()))
}

fn plugin_from_path(path: &Path, format: PluginFormat) -> PluginInfo {
    let name = path
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("Unknown Plug-in")
        .to_string();
    let path_string = path.to_string_lossy().to_string();
    PluginInfo {
        id: stable_id(format.id_prefix(), &path_string),
        name,
        vendor: "Unknown Vendor".to_string(),
        category: "Uncategorized".to_string(),
        sub_categories: None,
        format: format.label().to_string(),
        path: path_string.clone(),
        module_path: Some(path_string),
        class_id: None,
        version: None,
        sdk_version: None,
        is_shell_child: false,
        is_ara: false,
        midi_output: false,
        sdk_metadata_loaded: false,
        load_error: Some("Plug-in metadata was not read".to_string()),
    }
}

fn stable_id(prefix: &str, input: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{prefix}:{hash:016x}")
}

pub fn stable_id_for_au(input: &str) -> String {
    stable_id("au", input)
}

#[cfg(test)]
mod tests {
    use super::{sort_and_dedup, NativePluginInfo, PluginInfo};

    /// The C++ VST3 scanner emits `isAra` for every class; CLAP, VST2, and any
    /// pre-ARA cached payload omit it entirely and must still parse.
    #[test]
    fn native_json_carries_ara_flag_and_defaults_to_false() {
        let with_ara: NativePluginInfo = serde_json::from_str(
            r#"{"name":"Melodyne","vendor":"Celemony","category":"Audio Module Class",
                "format":"VST3","path":"C:/m.vst3","classId":"ABCD",
                "isShellChild":false,"isAra":true,"sdkMetadataLoaded":true}"#,
        )
        .expect("VST3 payload with isAra must parse");
        assert!(with_ara.is_ara);

        let legacy: NativePluginInfo = serde_json::from_str(
            r#"{"name":"Comp","vendor":"Acme","category":"Audio Module Class",
                "format":"CLAP","path":"C:/c.clap","sdkMetadataLoaded":true}"#,
        )
        .expect("payload without isAra must still parse");
        assert!(!legacy.is_ara);
    }

    /// The C++ VST3 scanner emits `midiOutput` from the bus probe; CLAP, VST2,
    /// and payloads from scanners that predate the probe omit it.
    #[test]
    fn native_json_carries_midi_output_and_defaults_to_false() {
        let with_midi: NativePluginInfo = serde_json::from_str(
            r#"{"name":"Chords","vendor":"Acme","category":"Audio Module Class",
                "subCategories":"Instrument","format":"VST3","path":"C:/s.vst3",
                "classId":"ABCD","isShellChild":false,"isAra":false,
                "midiInput":true,"midiOutput":true,"audioOutputs":1,
                "sdkMetadataLoaded":true}"#,
        )
        .expect("VST3 payload with midiOutput must parse");
        assert!(with_midi.midi_output);

        let no_midi: NativePluginInfo = serde_json::from_str(
            r#"{"name":"Comp","vendor":"Acme","category":"Audio Module Class",
                "format":"VST3","path":"C:/c.vst3","classId":"EF01",
                "midiInput":false,"midiOutput":false,"audioOutputs":1,
                "sdkMetadataLoaded":true}"#,
        )
        .expect("VST3 payload with midiOutput:false must parse");
        assert!(!no_midi.midi_output);

        let legacy: NativePluginInfo = serde_json::from_str(
            r#"{"name":"Comp","vendor":"Acme","category":"Audio Module Class",
                "format":"VST3","path":"C:/c.vst3","sdkMetadataLoaded":true}"#,
        )
        .expect("payload without midiOutput must still parse");
        assert!(!legacy.midi_output);
    }

    fn plugin(id: &str, name: &str) -> PluginInfo {
        PluginInfo {
            id: id.to_string(),
            name: name.to_string(),
            vendor: String::new(),
            category: String::new(),
            sub_categories: None,
            format: "VST3".to_string(),
            path: String::new(),
            module_path: None,
            class_id: None,
            is_ara: false,
            midi_output: false,
            version: None,
            sdk_version: None,
            is_shell_child: false,
            sdk_metadata_loaded: false,
            load_error: None,
        }
    }

    fn ids(plugins: &[PluginInfo]) -> Vec<&str> {
        plugins.iter().map(|p| p.id.as_str()).collect()
    }

    #[test]
    fn dedup_removes_true_duplicates_and_sorts_by_name() {
        let mut plugins = vec![
            plugin("vst3:2", "Zebra"),
            plugin("vst3:1", "Alpha"),
            plugin("vst3:2", "Zebra"),
        ];
        sort_and_dedup(&mut plugins);
        assert_eq!(ids(&plugins), vec!["vst3:1", "vst3:2"]);
    }

    #[test]
    fn dedup_survives_same_name_different_id_separator() {
        // Regression: the old sort-by-name + dedup_by(id) left same-id entries
        // non-adjacent when a different-id plugin shared their display name, so
        // the duplicate was never removed. All three share the name "Compressor".
        let mut plugins = vec![
            plugin("vst3:aaa", "Compressor"),
            plugin("vst3:bbb", "Compressor"),
            plugin("vst3:aaa", "Compressor"),
        ];
        sort_and_dedup(&mut plugins);
        let mut got = ids(&plugins);
        got.sort_unstable();
        assert_eq!(got, vec!["vst3:aaa", "vst3:bbb"]);
    }

    #[test]
    fn dedup_keeps_distinct_ids_from_multiclass_module() {
        // WaveShell-style: one module path, many class ids -> all distinct ids
        // must be preserved.
        let mut plugins = vec![
            plugin("vst3:shell#1", "WaveComp"),
            plugin("vst3:shell#2", "WaveEQ"),
            plugin("vst3:shell#3", "WaveGate"),
        ];
        sort_and_dedup(&mut plugins);
        assert_eq!(plugins.len(), 3);
    }
}

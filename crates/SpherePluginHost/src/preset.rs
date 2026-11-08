//! Futureboard `.pst` preset files (FBPST format, aligned with Electron `PluginHostNative`).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::plugin_db::PluginScanStatus;
use crate::registry::{default_preset_root, RegistryPlugin};

const PRESET_MAGIC: &[u8; 5] = b"FBPST";

/// Header version of a file about one plug-in: a scan-cache row (no state) or
/// a user preset (that plug-in's state).
const VERSION_SINGLE: u16 = 1;
/// Header version of a bundle: a chain of plug-ins, or a whole track. Older
/// readers that only know version 1 see a file whose metadata names no
/// plug-in and refuse it, rather than misreading it as one.
const VERSION_BUNDLE: u16 = 2;

/// The header's version field. Files written before the field was checked are
/// all version 1.
fn header_version(bytes: &[u8]) -> u16 {
    if bytes.len() < 8 {
        return VERSION_SINGLE;
    }
    u16::from_le_bytes([bytes[6], bytes[7]])
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PresetMetadata<'a> {
    preset_format: &'static str,
    version: u32,
    created_at: i64,
    plugin_metadata: PresetPluginMetadata<'a>,
    plugin_state: PresetPluginState,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PresetPluginMetadata<'a> {
    id: &'a str,
    name: &'a str,
    vendor: &'a str,
    format: &'a str,
    category: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    raw_category: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sub_categories: Option<&'a str>,
    kind: &'a str,
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    class_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<&'a str>,
    sdk_metadata_loaded: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PresetPluginState {
    encoding: &'static str,
    byte_length: u32,
    source: &'static str,
}

/// Ensure `Documents/Futureboard Studio/Audio Plug-ins/{VST3,VST2,CLAP}/{Instruments,Effects}` exist.
pub fn ensure_preset_folders() -> Result<(), String> {
    for folder in preset_subfolders() {
        fs::create_dir_all(&folder).map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub fn preset_subfolders() -> Vec<PathBuf> {
    let root = default_preset_root();
    [
        "VST3/Instruments",
        "VST3/Effects",
        "VST2/Instruments",
        "VST2/Effects",
        "CLAP/Instruments",
        "CLAP/Effects",
    ]
    .into_iter()
    .map(|rel| root.join(rel))
    .collect()
}

/// Delete every `.pst` under the preset root. Returns number of files removed.
pub fn clear_all_presets() -> Result<u32, String> {
    let root = default_preset_root();
    if !root.exists() {
        return Ok(0);
    }
    clear_pst_files_recursive(&root)
}

fn clear_pst_files_recursive(dir: &Path) -> Result<u32, String> {
    let mut deleted = 0u32;
    let entries = fs::read_dir(dir).map_err(|e| e.to_string())?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            deleted = deleted.saturating_add(clear_pst_files_recursive(&path)?);
        } else if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("pst"))
            // A user preset shares the extension but is the user's work, not
            // cache: clearing the cache must never delete one.
            && !is_state_preset_file(&path)
        {
            fs::remove_file(&path).map_err(|e| e.to_string())?;
            deleted += 1;
        }
    }
    Ok(deleted)
}

/// Whether `path` is a user preset — an FBPST file carrying plug-in state, or
/// a chain or track bundle — rather than a scan-cache row, which never carries
/// any.
///
/// All are `.pst` in the same container, so the header tells them apart: a
/// bundle by its version, a one-plug-in preset by its declared state length.
/// Only the 16 header bytes are read.
pub fn is_state_preset_file(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut file) = fs::File::open(path) else {
        return false;
    };
    let mut header = [0u8; 16];
    if file.read_exact(&mut header).is_err() || &header[..5] != PRESET_MAGIC {
        return false;
    }
    header_version(&header) >= VERSION_BUNDLE
        || u32::from_le_bytes([header[12], header[13], header[14], header[15]]) > 0
}

/// Validate that a plug-in binary exists before registration. Formats with no
/// module file (AU, addressed by component id) have nothing to check here.
pub fn validate_plugin_for_registration(plugin: &RegistryPlugin) -> Result<(), String> {
    if plugin.format.has_module_file() && !plugin.path.exists() {
        return Err(format!(
            "Plug-in binary is missing: {}",
            plugin.path.display()
        ));
    }
    Ok(())
}

/// Write `.pst` for this registry row (does not change [`RegistryPlugin::status`]).
pub fn write_preset(plugin: &RegistryPlugin) -> Result<(), String> {
    validate_plugin_for_registration(plugin)?;
    ensure_preset_folders()?;

    if let Some(parent) = plugin.preset_path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    let bytes = build_preset_binary(plugin);
    let tmp = plugin.preset_path.with_extension("pst.tmp");
    {
        let mut file = fs::File::create(&tmp).map_err(|e| e.to_string())?;
        file.write_all(&bytes).map_err(|e| e.to_string())?;
    }
    fs::rename(&tmp, &plugin.preset_path).map_err(|e| e.to_string())?;
    Ok(())
}

/// Validate and write `.pst`; marks the row as [`crate::registry::PluginStatus::PresetReady`].
pub fn register_plugin(plugin: &mut RegistryPlugin) -> Result<(), String> {
    write_preset(plugin)?;
    plugin.status = crate::registry::PluginStatus::PresetReady;
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PresetMetadataOwned {
    #[serde(default)]
    created_at: i64,
    plugin_metadata: PresetPluginMetadataOwned,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PresetPluginMetadataOwned {
    id: String,
    name: String,
    #[serde(default)]
    vendor: String,
    #[serde(default)]
    format: String,
    #[serde(default)]
    category: String,
    #[serde(default)]
    raw_category: Option<String>,
    #[serde(default)]
    sub_categories: Option<String>,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    class_id: Option<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    sdk_metadata_loaded: bool,
}

/// Read a single `.pst` and reconstruct its [`RegistryPlugin`] row. No plugin
/// binary is touched — the binary `status` reflects whether the path on disk
/// still exists.
pub fn read_preset_file(preset_path: &Path) -> Result<RegistryPlugin, String> {
    use crate::registry::{display_category, PluginFormat, PluginKind, PluginStatus};

    let bytes = fs::read(preset_path).map_err(|e| e.to_string())?;
    if bytes.len() < 24 || &bytes[..5] != PRESET_MAGIC {
        return Err("Not an FBPST preset".to_string());
    }
    if header_version(&bytes) >= VERSION_BUNDLE {
        return Err("A chain or track preset, not a plug-in cache row".to_string());
    }
    // A user preset carries state; a cache row never does. Reading a user
    // preset as a row would list it as a phantom plug-in.
    let state_len = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
    if state_len > 0 {
        return Err("A user preset, not a plug-in cache row".to_string());
    }
    let meta_len = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize;
    let meta_start = 24usize;
    let meta_end = meta_start
        .checked_add(meta_len)
        .ok_or_else(|| "Preset metadata length overflow".to_string())?;
    if bytes.len() < meta_end {
        return Err("Preset metadata truncated".to_string());
    }
    let parsed: PresetMetadataOwned =
        serde_json::from_slice(&bytes[meta_start..meta_end]).map_err(|e| e.to_string())?;

    let pm = parsed.plugin_metadata;
    let format = PluginFormat::from_str_lossy(&pm.format);
    // Legacy `.pst` files written before `unknown` existed only ever stored
    // "instrument"/"effect", so an unrecognised token still reads as an effect
    // and old caches keep loading unchanged.
    let kind = match pm.kind.to_ascii_lowercase().as_str() {
        "instrument" => PluginKind::Instrument,
        "unknown" => PluginKind::Unknown,
        _ => PluginKind::Effect,
    };
    let category = if pm.category.is_empty() {
        display_category(
            format,
            &pm.category,
            pm.raw_category.as_deref(),
            pm.sub_categories.as_deref(),
        )
    } else {
        pm.category.clone()
    };
    let binary_path = PathBuf::from(&pm.path);
    let status = if binary_path.exists() {
        PluginStatus::PresetReady
    } else {
        PluginStatus::MissingPreset
    };
    let scan_status = if binary_path.exists() {
        PluginScanStatus::Success
    } else {
        PluginScanStatus::MetadataOnly
    };

    Ok(RegistryPlugin {
        id: pm.id,
        name: pm.name,
        vendor: pm.vendor,
        format,
        category,
        raw_category: pm.raw_category,
        sub_categories: pm.sub_categories,
        kind,
        path: binary_path,
        class_id: pm.class_id,
        version: pm.version,
        sdk_metadata_loaded: pm.sdk_metadata_loaded,
        // Presets predate ARA detection and store no ARA flag; the catalog row
        // from the scan is the authority, so this projection stays conservative.
        is_ara: false,
        preset_path: preset_path.to_path_buf(),
        scanned_at_ms: parsed.created_at,
        status,
        scan_status,
        error_message: None,
    })
}

/// Walk the preset root and load every cached `.pst` row. This does **not**
/// touch any plug-in binary, scan default OS folders, or invoke the VST3/CLAP
/// SDK; it is safe to call on the UI thread when the cache is small.
pub fn load_cached_plugins() -> Vec<RegistryPlugin> {
    let mut out = Vec::new();
    let root = default_preset_root();
    if !root.exists() {
        return out;
    }
    collect_pst_files(&root, &mut |path| {
        if let Ok(plugin) = read_preset_file(path) {
            out.push(plugin);
        }
    });
    out
}

fn collect_pst_files(dir: &Path, visit: &mut dyn FnMut(&Path)) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_pst_files(&path, visit);
        } else if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("pst"))
        {
            visit(&path);
        }
    }
}

/// Delete the entire preset cache directory tree. Used by Plugin Manager
/// "Clear Plugin Cache". Returns the number of `.pst` files removed.
pub fn clear_plugin_cache() -> Result<u32, String> {
    clear_all_presets()
}

// ── Where user presets live ─────────────────────────────────────────────────

/// A folder name the OS will take for a plug-in called `plugin_name`: the
/// characters Windows refuses become `_`, and trailing dots and spaces go.
/// `None` when nothing is left.
pub fn preset_folder_name(plugin_name: &str) -> Option<String> {
    let name: String = plugin_name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let name = name.trim().trim_end_matches(['.', ' ']).to_string();
    (!name.is_empty()).then_some(name)
}

/// One plug-in's user presets:
/// `Documents/Futureboard Studio/Presets/Plug-ins/{plugin name}/`.
///
/// Named for the plug-in rather than its id, so the folder is the one the
/// user would look for. The formats of one plug-in (its VST3 and its CLAP)
/// share it; each file names the plug-in id it belongs to, and
/// [`state_preset_owner`] tells them apart.
pub fn user_preset_dir(plugin_name: &str) -> Option<PathBuf> {
    Some(crate::registry::user_preset_root().join(preset_folder_name(plugin_name)?))
}

/// One plug-in's user presets, sorted by name: `(name, file)`.
///
/// The folder is shared by the plug-in's formats, so only the `.pst` files
/// that say they belong to `plugin_id` are listed: another's (the VST3's,
/// seen from the CLAP) would be refused on load, and so would a file that
/// names no plug-in at all.
pub fn list_user_presets(plugin_id: &str, plugin_name: &str) -> Vec<(String, PathBuf)> {
    let Some(dir) = user_preset_dir(plugin_name) else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut presets: Vec<(String, PathBuf)> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("pst"))
                && state_preset_owner(path).as_deref() == Some(plugin_id)
        })
        .filter_map(|path| {
            let name = path.file_stem()?.to_str()?.to_string();
            Some((name, path))
        })
        .collect();
    presets.sort_by_key(|(name, _)| name.to_lowercase());
    presets
}

/// The plug-in id a user preset file says it belongs to, read from its
/// header and metadata only. `None` for anything that is not a one-plug-in
/// FBPST preset. Remembered per file and modification time, so listing a
/// folder again does not reopen every file.
pub fn state_preset_owner(path: &Path) -> Option<String> {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    use std::time::SystemTime;
    static SEEN: OnceLock<Mutex<HashMap<PathBuf, (SystemTime, Option<String>)>>> = OnceLock::new();
    let modified = fs::metadata(path).and_then(|meta| meta.modified()).ok()?;
    let seen = SEEN.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some((when, owner)) = seen.lock().ok()?.get(path) {
        if *when == modified {
            return owner.clone();
        }
    }
    let owner = read_state_preset_owner(path);
    if let Ok(mut seen) = seen.lock() {
        seen.insert(path.to_path_buf(), (modified, owner.clone()));
    }
    owner
}

fn read_state_preset_owner(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut file = fs::File::open(path).ok()?;
    let mut header = [0u8; 24];
    file.read_exact(&mut header).ok()?;
    if &header[..5] != PRESET_MAGIC || header_version(&header) >= VERSION_BUNDLE {
        return None;
    }
    let meta_len = u32::from_le_bytes([header[8], header[9], header[10], header[11]]) as usize;
    let mut meta = vec![0u8; meta_len.min(1024 * 1024)];
    file.read_exact(&mut meta).ok()?;
    let parsed: PresetMetadataOwned = serde_json::from_slice(&meta).ok()?;
    Some(parsed.plugin_metadata.id)
}

/// One user preset: a plug-in's own opaque state under a name.
///
/// Written in the same FBPST container as the scan cache — one format for every
/// `.pst` the app writes — with the state in the payload the header has always
/// reserved for it and the metadata saying so.
pub struct StatePreset {
    /// Stable plug-in identifier the state belongs to. A preset is only ever
    /// restored into the plug-in that produced it.
    pub plugin_id: String,
    /// Plug-in display name, for anything that reads the file on its own.
    pub plugin_name: String,
    /// The plug-in's opaque state, exactly as it handed it over.
    pub state: Vec<u8>,
}

/// Writes a user preset to `path`, creating parent directories.
pub fn write_state_preset(path: &Path, preset: &StatePreset) -> Result<(), String> {
    if preset.state.is_empty() {
        return Err("preset has no plug-in state to store".to_string());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let bytes = build_state_preset_binary(preset);
    // Written aside and renamed, like the scan cache: a preset half-written by
    // an interrupted save is worse than no preset.
    let tmp = path.with_extension("pst.tmp");
    {
        let mut file = fs::File::create(&tmp).map_err(|e| e.to_string())?;
        file.write_all(&bytes).map_err(|e| e.to_string())?;
    }
    fs::rename(&tmp, path).map_err(|e| e.to_string())?;
    Ok(())
}

/// Reads back the plug-in state a user preset carries.
///
/// The `plugin_id` in the file is returned with it: state belongs to one
/// plug-in, and handing it to another is not a preset, it is corruption.
pub fn read_state_preset(path: &Path) -> Result<(String, Vec<u8>), String> {
    let bytes = fs::read(path).map_err(|e| e.to_string())?;
    if bytes.len() < 24 || &bytes[..5] != PRESET_MAGIC {
        return Err("Not an FBPST preset".to_string());
    }
    if header_version(&bytes) >= VERSION_BUNDLE {
        return Err("A chain or track preset, not one plug-in's preset".to_string());
    }
    let meta_len = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize;
    let state_len = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]) as usize;
    let meta_start = 24usize;
    let meta_end = meta_start
        .checked_add(meta_len)
        .ok_or_else(|| "Preset metadata length overflow".to_string())?;
    let state_end = meta_end
        .checked_add(state_len)
        .ok_or_else(|| "Preset state length overflow".to_string())?;
    if bytes.len() < state_end {
        return Err("Preset state truncated".to_string());
    }
    if state_len == 0 {
        return Err("Preset carries no plug-in state".to_string());
    }
    let parsed: PresetMetadataOwned =
        serde_json::from_slice(&bytes[meta_start..meta_end]).map_err(|e| e.to_string())?;
    Ok((
        parsed.plugin_metadata.id,
        bytes[meta_end..state_end].to_vec(),
    ))
}

fn build_state_preset_binary(preset: &StatePreset) -> Vec<u8> {
    let metadata = PresetMetadata {
        preset_format: "Mochi preset: Futureboard",
        version: 1,
        created_at: now_millis(),
        plugin_metadata: PresetPluginMetadata {
            id: &preset.plugin_id,
            name: &preset.plugin_name,
            vendor: "",
            format: "",
            category: "",
            raw_category: None,
            sub_categories: None,
            kind: "effect",
            path: String::new(),
            class_id: None,
            version: None,
            sdk_metadata_loaded: false,
        },
        plugin_state: PresetPluginState {
            encoding: "binary",
            byte_length: preset.state.len() as u32,
            source: "captured",
        },
    };
    let meta = serde_json::to_vec(&metadata).unwrap_or_default();
    let mut out = Vec::with_capacity(24 + meta.len() + preset.state.len());
    out.extend_from_slice(&preset_header(
        VERSION_SINGLE,
        meta.len(),
        preset.state.len(),
    ));
    out.extend_from_slice(&meta);
    out.extend_from_slice(&preset.state);
    out
}

/// The 24-byte FBPST header shared by every `.pst` this app writes.
fn preset_header(version: u16, meta_len: usize, state_len: usize) -> [u8; 24] {
    let mut header = [0u8; 24];
    header[..5].copy_from_slice(PRESET_MAGIC);
    header[6..8].copy_from_slice(&version.to_le_bytes());
    header[8..12].copy_from_slice(&(meta_len as u32).to_le_bytes());
    header[12..16].copy_from_slice(&(state_len as u32).to_le_bytes());
    header
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

fn build_preset_binary(plugin: &RegistryPlugin) -> Vec<u8> {
    let kind = plugin.kind.as_str();
    let metadata = PresetMetadata {
        preset_format: "Mochi preset: Futureboard",
        version: 1,
        created_at: plugin.scanned_at_ms,
        plugin_metadata: PresetPluginMetadata {
            id: &plugin.id,
            name: &plugin.name,
            vendor: &plugin.vendor,
            format: plugin.format.label(),
            category: &plugin.category,
            raw_category: plugin.raw_category.as_deref(),
            sub_categories: plugin.sub_categories.as_deref(),
            kind,
            path: plugin.path.display().to_string(),
            class_id: plugin.class_id.as_deref(),
            version: plugin.version.as_deref(),
            sdk_metadata_loaded: plugin.sdk_metadata_loaded,
        },
        plugin_state: PresetPluginState {
            encoding: "binary",
            byte_length: 0,
            source: "pending-native-instantiation",
        },
    };

    let meta = serde_json::to_vec(&metadata).unwrap_or_default();
    let mut out = Vec::with_capacity(24 + meta.len());
    out.extend_from_slice(&preset_header(VERSION_SINGLE, meta.len(), 0));
    out.extend_from_slice(&meta);
    out
}

// ── Bundles: chains and tracks ──────────────────────────────────────────────

/// What a bundle `.pst` holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BundleKind {
    /// A track's effect chain, in order. Loads onto any track that can take
    /// it; the track's instrument stays.
    Chain,
    /// A whole track: its kind, name, colour, level, pan and input monitor,
    /// and every plug-in on it, instrument first. Loads as a new track.
    Track,
}

/// One plug-in in a bundle, with its own opaque state.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PresetSlot {
    /// The insert's plug-in id (class id where the format has one). State is
    /// only ever restored into the plug-in that produced it.
    pub plugin_id: String,
    /// Plug-in name from the registry, for a file read on its own.
    pub plugin_name: String,
    pub vendor: String,
    /// Format label: `VST3`, `VST2`, `CLAP`, `AU`, `LV2`, or empty for a
    /// built-in.
    pub format: String,
    /// Module path the plug-in loaded from, where the format has one.
    pub plugin_path: String,
    pub version: Option<String>,
    /// Whether the slot is an instrument. `None` when the host did not say.
    pub is_instrument: Option<bool>,
    /// The name the slot showed (the user may have renamed it).
    pub display_name: String,
    pub enabled: bool,
    pub bypassed: bool,
    /// 1-based instrument output channels mixed down; empty means 1/2.
    pub output_channels: Vec<u8>,
    pub multiout_collapsed: bool,
    /// The plug-in's opaque state. Empty means its defaults.
    pub state: Vec<u8>,
}

/// The track settings a track bundle carries. Strings rather than the UI's
/// own types, so this crate stays free of them and the file stays readable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PresetTrack {
    /// `audio`, `midi`, `instrument`, `bus` or `return`.
    pub track_type: String,
    pub name: String,
    /// `#rrggbb`.
    pub color: String,
    /// Fader position, normalized as the track stores it.
    pub volume: f32,
    /// -1 (left) to 1 (right).
    pub pan: f32,
    /// `off`, `auto` or `always`.
    pub input_monitor: String,
}

/// A chain or track preset: several plug-ins, and for a track its settings.
#[derive(Debug, Clone, PartialEq)]
pub struct BundlePreset {
    pub kind: BundleKind,
    /// What the file is called in the browser.
    pub name: String,
    pub slots: Vec<PresetSlot>,
    /// Present for [`BundleKind::Track`].
    pub track: Option<PresetTrack>,
}

/// Bundle metadata: everything but the states, which follow it back to back in
/// the payload, each found by its own offset and length.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BundleMetadata {
    preset_format: String,
    version: u32,
    kind: BundleKind,
    #[serde(default)]
    created_at: i64,
    /// The bundle's own name where single-plug-in files keep the plug-in's,
    /// so a listing that reads `pluginMetadata.name` shows the right thing.
    plugin_metadata: BundleNameOnly,
    slots: Vec<BundleSlotMetadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    track: Option<PresetTrack>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BundleNameOnly {
    #[serde(default)]
    id: String,
    name: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BundleSlotMetadata {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    vendor: String,
    #[serde(default)]
    format: String,
    #[serde(default)]
    path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    instrument: Option<bool>,
    #[serde(default)]
    display_name: String,
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default)]
    bypassed: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    output_channels: Vec<u8>,
    #[serde(default)]
    multiout_collapsed: bool,
    state_offset: u64,
    state_length: u64,
}

fn default_true() -> bool {
    true
}

/// Writes a chain or track preset to `path`, creating parent directories.
pub fn write_bundle_preset(path: &Path, preset: &BundlePreset) -> Result<(), String> {
    if preset.slots.is_empty() && preset.kind == BundleKind::Chain {
        return Err("a chain preset needs at least one plug-in".to_string());
    }
    if preset.kind == BundleKind::Track && preset.track.is_none() {
        return Err("a track preset needs the track's settings".to_string());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let bytes = build_bundle_binary(preset)?;
    let tmp = path.with_extension("pst.tmp");
    {
        let mut file = fs::File::create(&tmp).map_err(|e| e.to_string())?;
        file.write_all(&bytes).map_err(|e| e.to_string())?;
    }
    fs::rename(&tmp, path).map_err(|e| e.to_string())?;
    Ok(())
}

fn build_bundle_binary(preset: &BundlePreset) -> Result<Vec<u8>, String> {
    let mut states = Vec::new();
    let mut slots = Vec::with_capacity(preset.slots.len());
    for slot in &preset.slots {
        let offset = states.len() as u64;
        states.extend_from_slice(&slot.state);
        slots.push(BundleSlotMetadata {
            id: slot.plugin_id.clone(),
            name: slot.plugin_name.clone(),
            vendor: slot.vendor.clone(),
            format: slot.format.clone(),
            path: slot.plugin_path.clone(),
            version: slot.version.clone(),
            instrument: slot.is_instrument,
            display_name: slot.display_name.clone(),
            enabled: slot.enabled,
            bypassed: slot.bypassed,
            output_channels: slot.output_channels.clone(),
            multiout_collapsed: slot.multiout_collapsed,
            state_offset: offset,
            state_length: slot.state.len() as u64,
        });
    }
    if states.len() > u32::MAX as usize {
        return Err("the plug-in states are too large for one preset".to_string());
    }
    let metadata = BundleMetadata {
        preset_format: "Futureboard preset".to_string(),
        version: VERSION_BUNDLE as u32,
        kind: preset.kind,
        created_at: now_millis(),
        plugin_metadata: BundleNameOnly {
            id: String::new(),
            name: preset.name.clone(),
        },
        slots,
        track: preset.track.clone(),
    };
    let meta = serde_json::to_vec(&metadata).map_err(|e| e.to_string())?;
    let mut out = Vec::with_capacity(24 + meta.len() + states.len());
    out.extend_from_slice(&preset_header(VERSION_BUNDLE, meta.len(), states.len()));
    out.extend_from_slice(&meta);
    out.extend_from_slice(&states);
    Ok(out)
}

/// Reads a chain or track preset back.
pub fn read_bundle_preset(path: &Path) -> Result<BundlePreset, String> {
    let bytes = fs::read(path).map_err(|e| e.to_string())?;
    if bytes.len() < 24 || &bytes[..5] != PRESET_MAGIC {
        return Err("Not an FBPST preset".to_string());
    }
    if header_version(&bytes) != VERSION_BUNDLE {
        return Err("A one-plug-in preset, not a chain or track preset".to_string());
    }
    let meta_len = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize;
    let state_len = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]) as usize;
    let meta_end = 24usize
        .checked_add(meta_len)
        .ok_or_else(|| "Preset metadata length overflow".to_string())?;
    let state_end = meta_end
        .checked_add(state_len)
        .ok_or_else(|| "Preset state length overflow".to_string())?;
    if bytes.len() < state_end {
        return Err("Preset truncated".to_string());
    }
    let metadata: BundleMetadata =
        serde_json::from_slice(&bytes[24..meta_end]).map_err(|e| e.to_string())?;
    let states = &bytes[meta_end..state_end];
    let mut slots = Vec::with_capacity(metadata.slots.len());
    for slot in metadata.slots {
        let start = usize::try_from(slot.state_offset).map_err(|_| "state offset".to_string())?;
        let len = usize::try_from(slot.state_length).map_err(|_| "state length".to_string())?;
        let end = start
            .checked_add(len)
            .filter(|end| *end <= states.len())
            .ok_or_else(|| format!("the state of {} runs past the file", slot.name))?;
        slots.push(PresetSlot {
            plugin_id: slot.id,
            plugin_name: slot.name,
            vendor: slot.vendor,
            format: slot.format,
            plugin_path: slot.path,
            version: slot.version,
            is_instrument: slot.instrument,
            display_name: slot.display_name,
            enabled: slot.enabled,
            bypassed: slot.bypassed,
            output_channels: slot.output_channels,
            multiout_collapsed: slot.multiout_collapsed,
            state: states[start..end].to_vec(),
        });
    }
    if metadata.kind == BundleKind::Track && metadata.track.is_none() {
        return Err("A track preset without its track settings".to_string());
    }
    Ok(BundlePreset {
        kind: metadata.kind,
        name: metadata.plugin_metadata.name,
        slots,
        track: metadata.track,
    })
}

/// What a `.pst` is, read from its header (and, for a bundle, its metadata).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PstKind {
    /// A plug-in scan-cache row: a plug-in to insert with its defaults.
    PluginRow,
    /// One plug-in's saved sound.
    PluginState,
    /// A chain or track bundle.
    Bundle(BundleKind),
}

/// Classifies a `.pst` without decoding its states. `None` for anything that
/// is not an FBPST file.
pub fn pst_kind(path: &Path) -> Option<PstKind> {
    use std::io::Read;
    let mut file = fs::File::open(path).ok()?;
    let mut header = [0u8; 24];
    file.read_exact(&mut header).ok()?;
    if &header[..5] != PRESET_MAGIC {
        return None;
    }
    if header_version(&header) >= VERSION_BUNDLE {
        let meta_len = u32::from_le_bytes([header[8], header[9], header[10], header[11]]) as usize;
        let mut meta = vec![0u8; meta_len.min(16 * 1024 * 1024)];
        file.read_exact(&mut meta).ok()?;
        #[derive(Deserialize)]
        struct KindOnly {
            kind: BundleKind,
        }
        let kind: KindOnly = serde_json::from_slice(&meta).ok()?;
        return Some(PstKind::Bundle(kind.kind));
    }
    let state_len = u32::from_le_bytes([header[12], header[13], header[14], header[15]]);
    Some(if state_len > 0 {
        PstKind::PluginState
    } else {
        PstKind::PluginRow
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundle(kind: BundleKind) -> BundlePreset {
        BundlePreset {
            kind,
            name: "Vocal Chain".to_string(),
            slots: vec![
                PresetSlot {
                    plugin_id: "builtin:equz8".to_string(),
                    plugin_name: "EQ-Z8".to_string(),
                    display_name: "EQ-Z8".to_string(),
                    enabled: true,
                    state: br#"{"bands":[1,2,3]}"#.to_vec(),
                    ..Default::default()
                },
                PresetSlot {
                    plugin_id: "vst3:abc".to_string(),
                    plugin_name: "Comp".to_string(),
                    format: "VST3".to_string(),
                    plugin_path: "C:/VST3/Comp.vst3".to_string(),
                    display_name: "Bus Comp".to_string(),
                    enabled: true,
                    bypassed: true,
                    // A stateless plug-in keeps its defaults.
                    state: Vec::new(),
                    ..Default::default()
                },
                PresetSlot {
                    plugin_id: "vst3:def".to_string(),
                    plugin_name: "Limiter".to_string(),
                    enabled: true,
                    state: (0u8..=255).cycle().take(5000).collect(),
                    ..Default::default()
                },
            ],
            track: (kind == BundleKind::Track).then(|| PresetTrack {
                track_type: "audio".to_string(),
                name: "Lead Vocal".to_string(),
                color: "#ff8800".to_string(),
                volume: 0.7,
                pan: -0.25,
                input_monitor: "auto".to_string(),
            }),
        }
    }

    /// Every slot comes back in order with its own state byte for byte, and a
    /// track keeps its settings.
    #[test]
    fn a_bundle_round_trips_every_slot_and_its_state() {
        let dir = std::env::temp_dir().join("fb-preset-bundle");
        let _ = fs::remove_dir_all(&dir);
        for kind in [BundleKind::Chain, BundleKind::Track] {
            let path = dir.join(format!("{kind:?}.pst"));
            let written = bundle(kind);
            write_bundle_preset(&path, &written).expect("write");
            assert_eq!(read_bundle_preset(&path).expect("read"), written);
            assert_eq!(pst_kind(&path), Some(PstKind::Bundle(kind)));
        }
        let _ = fs::remove_dir_all(&dir);
    }

    /// A bundle shares the container but is never read as one plug-in: not as
    /// a cache row (a phantom plug-in), not as a plug-in's preset (state handed
    /// to the wrong plug-in), and Clear Database keeps it.
    #[test]
    fn a_bundle_is_not_mistaken_for_a_plug_in_or_cache() {
        let dir = std::env::temp_dir().join("fb-preset-bundle-not-single");
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("Chain.pst");
        let mut stateless = bundle(BundleKind::Chain);
        for slot in &mut stateless.slots {
            slot.state.clear();
        }
        write_bundle_preset(&path, &stateless).expect("write");
        assert!(read_preset_file(&path).is_err(), "no phantom plug-in row");
        assert!(read_state_preset(&path).is_err(), "not one plug-in's state");
        assert!(is_state_preset_file(&path), "user work, even with no state");
        assert_eq!(clear_pst_files_recursive(&dir).unwrap(), 0);
        assert!(path.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    /// The single-plug-in files classify as they always read.
    #[test]
    fn single_plug_in_files_classify_as_before() {
        let dir = std::env::temp_dir().join("fb-preset-classify");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let user = dir.join("user.pst");
        write_state_preset(
            &user,
            &StatePreset {
                plugin_id: "x".to_string(),
                plugin_name: "X".to_string(),
                state: vec![1],
            },
        )
        .unwrap();
        let row = dir.join("row.pst");
        let meta = br#"{"pluginMetadata":{"id":"x","name":"X"}}"#;
        let mut bytes = preset_header(VERSION_SINGLE, meta.len(), 0).to_vec();
        bytes.extend_from_slice(meta);
        fs::write(&row, &bytes).unwrap();
        assert_eq!(pst_kind(&user), Some(PstKind::PluginState));
        assert_eq!(pst_kind(&row), Some(PstKind::PluginRow));
        assert!(read_bundle_preset(&user).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    /// A user preset is an FBPST file like any other: same magic, same header,
    /// with the state in the payload the header reserves. Anything that reads
    /// `.pst` reads this one too.
    #[test]
    fn a_state_preset_round_trips_through_the_shared_container() {
        let dir = std::env::temp_dir().join("fb-preset-roundtrip");
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("Preset 1.pst");
        let state: Vec<u8> = (0u8..=255).cycle().take(4096).collect();
        write_state_preset(
            &path,
            &StatePreset {
                plugin_id: "vst3:abcdef".to_string(),
                plugin_name: "Auto-Tune Pro".to_string(),
                state: state.clone(),
            },
        )
        .expect("write");

        let raw = fs::read(&path).expect("read back");
        assert_eq!(&raw[..5], PRESET_MAGIC, "same container as the scan cache");
        let declared = u32::from_le_bytes([raw[12], raw[13], raw[14], raw[15]]) as usize;
        assert_eq!(declared, state.len(), "header declares the state length");

        let (plugin_id, restored) = read_state_preset(&path).expect("parse");
        assert_eq!(plugin_id, "vst3:abcdef");
        assert_eq!(restored, state, "state comes back byte for byte");

        let _ = fs::remove_dir_all(&dir);
    }

    /// A user `.pst` under the preset root is neither a plug-in row nor cache
    /// to clear: the scan cache reader skips it and Clear Database keeps it.
    #[test]
    fn a_user_preset_is_not_mistaken_for_the_scan_cache() {
        let dir = std::env::temp_dir().join("fb-preset-not-cache");
        let _ = fs::remove_dir_all(&dir);
        let user = dir.join("User").join("vst3_x").join("Warm Vocal.pst");
        write_state_preset(
            &user,
            &StatePreset {
                plugin_id: "vst3:x".to_string(),
                plugin_name: "X".to_string(),
                state: vec![1, 2, 3],
            },
        )
        .expect("write");
        let cache = dir.join("VST3").join("Effects").join("x.pst");
        fs::create_dir_all(cache.parent().unwrap()).unwrap();
        let meta = br#"{"pluginMetadata":{"id":"x","name":"X"}}"#;
        let mut bytes = preset_header(VERSION_SINGLE, meta.len(), 0).to_vec();
        bytes.extend_from_slice(meta);
        fs::write(&cache, &bytes).unwrap();

        assert!(is_state_preset_file(&user));
        assert!(!is_state_preset_file(&cache));
        assert!(read_preset_file(&user).is_err(), "no phantom plug-in row");
        assert!(read_preset_file(&cache).is_ok());
        assert_eq!(clear_pst_files_recursive(&dir).unwrap(), 1);
        assert!(user.exists(), "the user's preset survives Clear Database");
        assert!(!cache.exists());

        let _ = fs::remove_dir_all(&dir);
    }

    /// A plug-in's name becomes a folder Windows will take.
    #[test]
    fn plug_in_names_become_safe_folder_names() {
        assert_eq!(preset_folder_name("Serum").as_deref(), Some("Serum"));
        assert_eq!(
            preset_folder_name("FabFilter Pro-Q 3").as_deref(),
            Some("FabFilter Pro-Q 3")
        );
        assert_eq!(
            preset_folder_name("A/B: Test?").as_deref(),
            Some("A_B_ Test_")
        );
        assert_eq!(preset_folder_name("Trail. ").as_deref(), Some("Trail"));
        assert_eq!(preset_folder_name("  "), None);
    }

    /// Each preset in a plug-in's folder names the plug-in it belongs to, so
    /// the formats sharing the folder each find only their own; a file that
    /// is not a one-plug-in preset names none.
    #[test]
    fn a_preset_names_the_plug_in_it_belongs_to() {
        let dir = std::env::temp_dir().join("fb-preset-owner");
        let _ = fs::remove_dir_all(&dir);
        let preset = |name: &str, id: &str| {
            let path = dir.join(name);
            write_state_preset(
                &path,
                &StatePreset {
                    plugin_id: id.to_string(),
                    plugin_name: "X".to_string(),
                    state: vec![7],
                },
            )
            .unwrap();
            path
        };
        let vst3 = preset("Warm.pst", "vst3:x");
        let clap = preset("Bright.pst", "clap:x");
        assert_eq!(state_preset_owner(&vst3).as_deref(), Some("vst3:x"));
        assert_eq!(state_preset_owner(&clap).as_deref(), Some("clap:x"));
        let chain = dir.join("Chain.pst");
        write_bundle_preset(&chain, &bundle(BundleKind::Chain)).unwrap();
        assert_eq!(state_preset_owner(&chain), None);
        let _ = fs::remove_dir_all(&dir);
    }

    /// The scan cache's own presets carry no state, and asking for one says so
    /// rather than handing back an empty blob a plug-in would swallow.
    #[test]
    fn a_cache_preset_reports_that_it_carries_no_state() {
        let dir = std::env::temp_dir().join("fb-preset-nostate");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("cache.pst");
        let meta = br#"{"pluginMetadata":{"id":"x","name":"X"}}"#;
        let mut bytes = preset_header(VERSION_SINGLE, meta.len(), 0).to_vec();
        bytes.extend_from_slice(meta);
        fs::write(&path, &bytes).expect("write");

        assert!(read_state_preset(&path).is_err());

        let _ = fs::remove_dir_all(&dir);
    }
}

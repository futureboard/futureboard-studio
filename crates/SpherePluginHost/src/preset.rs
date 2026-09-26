//! Futureboard `.pst` preset files (FBPST format, aligned with Electron `PluginHostNative`).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::plugin_db::PluginScanStatus;
use crate::registry::{default_preset_root, RegistryPlugin};

const PRESET_MAGIC: &[u8; 5] = b"FBPST";

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

/// Whether `path` is a user preset — an FBPST file carrying plug-in state —
/// rather than a scan-cache row, which never carries any.
///
/// Both are `.pst` in the same container, so the header's declared state
/// length is what tells them apart. Only the 16 header bytes are read.
pub fn is_state_preset_file(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut file) = fs::File::open(path) else {
        return false;
    };
    let mut header = [0u8; 16];
    if file.read_exact(&mut header).is_err() || &header[..5] != PRESET_MAGIC {
        return false;
    }
    u32::from_le_bytes([header[12], header[13], header[14], header[15]]) > 0
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
    out.extend_from_slice(&preset_header(meta.len(), preset.state.len()));
    out.extend_from_slice(&meta);
    out.extend_from_slice(&preset.state);
    out
}

/// The 24-byte FBPST header shared by every `.pst` this app writes.
fn preset_header(meta_len: usize, state_len: usize) -> [u8; 24] {
    let mut header = [0u8; 24];
    header[..5].copy_from_slice(PRESET_MAGIC);
    header[6..8].copy_from_slice(&1u16.to_le_bytes());
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
    out.extend_from_slice(&preset_header(meta.len(), 0));
    out.extend_from_slice(&meta);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let mut bytes = preset_header(meta.len(), 0).to_vec();
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

    /// The scan cache's own presets carry no state, and asking for one says so
    /// rather than handing back an empty blob a plug-in would swallow.
    #[test]
    fn a_cache_preset_reports_that_it_carries_no_state() {
        let dir = std::env::temp_dir().join("fb-preset-nostate");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("cache.pst");
        let meta = br#"{"pluginMetadata":{"id":"x","name":"X"}}"#;
        let mut bytes = preset_header(meta.len(), 0).to_vec();
        bytes.extend_from_slice(meta);
        fs::write(&path, &bytes).expect("write");

        assert!(read_state_preset(&path).is_err());

        let _ = fs::remove_dir_all(&dir);
    }
}

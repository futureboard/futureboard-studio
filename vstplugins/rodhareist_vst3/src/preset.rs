//! `SpherePluginHost::preset`'s user presets: a plug-in's opaque state in
//! Studio's FBPST container. The rest of that module is Studio's plug-in scan
//! cache, which the plug-in has no use for.
//!
//! Byte-compatible with Studio, so a Rodhareist preset saved in either loads
//! in both. Keep it in step with `crates/SpherePluginHost/src/preset.rs`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const PRESET_MAGIC: &[u8; 5] = b"FBPST";
/// A file about one plug-in.
const VERSION_SINGLE: u16 = 1;
/// A chain or track bundle, which this reader refuses.
const VERSION_BUNDLE: u16 = 2;

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
    kind: &'a str,
    path: String,
    sdk_metadata_loaded: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PresetPluginState {
    encoding: &'static str,
    byte_length: u32,
    source: &'static str,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PresetMetadataOwned {
    plugin_metadata: PresetPluginMetadataOwned,
}

#[derive(Deserialize)]
struct PresetPluginMetadataOwned {
    id: String,
}

/// One user preset: a plug-in's own opaque state under a name.
pub struct StatePreset {
    pub plugin_id: String,
    pub plugin_name: String,
    pub state: Vec<u8>,
}

/// Writes a user preset to `path`, creating parent directories. Written aside
/// and renamed: a preset half-written by an interrupted save is worse than
/// none.
pub fn write_state_preset(path: &Path, preset: &StatePreset) -> Result<(), String> {
    if preset.state.is_empty() {
        return Err("preset has no plug-in state to store".to_string());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
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
            kind: "effect",
            path: String::new(),
            sdk_metadata_loaded: false,
        },
        plugin_state: PresetPluginState {
            encoding: "binary",
            byte_length: preset.state.len() as u32,
            source: "captured",
        },
    };
    let meta = serde_json::to_vec(&metadata).map_err(|e| e.to_string())?;
    let mut bytes = Vec::with_capacity(24 + meta.len() + preset.state.len());
    let mut header = [0u8; 24];
    header[..5].copy_from_slice(PRESET_MAGIC);
    header[6..8].copy_from_slice(&VERSION_SINGLE.to_le_bytes());
    header[8..12].copy_from_slice(&(meta.len() as u32).to_le_bytes());
    header[12..16].copy_from_slice(&(preset.state.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&header);
    bytes.extend_from_slice(&meta);
    bytes.extend_from_slice(&preset.state);

    let tmp = path.with_extension("pst.tmp");
    {
        let mut file = fs::File::create(&tmp).map_err(|e| e.to_string())?;
        file.write_all(&bytes).map_err(|e| e.to_string())?;
    }
    fs::rename(&tmp, path).map_err(|e| e.to_string())
}

/// Reads back the plug-in state a user preset carries, with the id of the
/// plug-in it belongs to.
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
    let meta_end = 24usize
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
        serde_json::from_slice(&bytes[24..meta_end]).map_err(|e| e.to_string())?;
    Ok((
        parsed.plugin_metadata.id,
        bytes[meta_end..state_end].to_vec(),
    ))
}

/// A folder name the OS will take for a plug-in called `plugin_name`.
/// Keep in step with `SpherePluginHost::preset::preset_folder_name`.
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
/// `Documents/Futureboard Studio/Presets/Plug-ins/{plugin name}/`, the
/// folder Studio uses too.
pub fn user_preset_dir(plugin_name: &str) -> Option<PathBuf> {
    Some(crate::registry::user_preset_root().join(preset_folder_name(plugin_name)?))
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_state_preset_round_trips() {
        let dir = std::env::temp_dir().join(format!("rodhareist-pst-{}", std::process::id()));
        let path = dir.join("Round Trip.pst");
        write_state_preset(
            &path,
            &StatePreset {
                plugin_id: rodharerist::PLUGIN_ID.to_string(),
                plugin_name: "Rodhareist".to_string(),
                state: b"{\"state\":1}".to_vec(),
            },
        )
        .unwrap();
        let (id, state) = read_state_preset(&path).unwrap();
        assert_eq!(id, rodharerist::PLUGIN_ID);
        assert_eq!(state, b"{\"state\":1}");
        let _ = fs::remove_dir_all(dir);
    }
}

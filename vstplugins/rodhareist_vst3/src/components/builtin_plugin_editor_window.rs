//! The host-ops types Studio's built-in editors are written against, with
//! the sources the Rodhareist editor uses, and Studio's user-file import.

use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PluginInstanceKey {
    pub track_id: String,
    pub insert_id: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct BuiltinMeterFrame {
    pub in_peak: f32,
    pub in_rms: f32,
    pub out_peak: f32,
    pub out_rms: f32,
    pub in_clip: bool,
    pub out_clip: bool,
}

/// A `.nam` capture on its way to the DSP.
#[derive(Debug, Clone)]
pub struct BuiltinNamLoadRequest {
    pub name: String,
    pub json: String,
    pub stereo: bool,
    pub full_rig: bool,
}

/// A `.wav` impulse response on its way to the DSP.
#[derive(Debug, Clone)]
pub struct BuiltinIrLoadRequest {
    pub name: String,
    pub bytes: Vec<u8>,
}

pub type BuiltinParamForwarder = Arc<dyn Fn(&PluginInstanceKey, u32, f32, &mut gpui::App)>;
pub type BuiltinNamLoadForwarder = Arc<dyn Fn(&PluginInstanceKey, BuiltinNamLoadRequest)>;
pub type BuiltinIrLoadForwarder = Arc<dyn Fn(&PluginInstanceKey, BuiltinIrLoadRequest)>;
pub type BuiltinMeterSource = Arc<dyn Fn(&PluginInstanceKey) -> Option<BuiltinMeterFrame>>;
/// `(sample_rate, block_frames, latency_samples, tempo_bpm)`.
pub type BuiltinHostStatusSource = Arc<dyn Fn(&PluginInstanceKey) -> Option<(u32, u32, u32, f64)>>;
/// `(publish sequence, telemetry slots)`.
pub type BuiltinPadLevelSource =
    Arc<dyn Fn(&PluginInstanceKey) -> Option<(u32, [f32; crate::audio_bridge::BUILTIN_PAD_SLOTS])>>;

#[derive(Clone, Default)]
pub struct BuiltinEditorHostOps {
    pub forward_param: Option<BuiltinParamForwarder>,
    pub load_nam_capture: Option<BuiltinNamLoadForwarder>,
    pub load_ir: Option<BuiltinIrLoadForwarder>,
    pub meter_source: Option<BuiltinMeterSource>,
    pub host_status_source: Option<BuiltinHostStatusSource>,
    pub pad_level_source: Option<BuiltinPadLevelSource>,
}

/// Studio's import of a picked user file — a NAM capture into Rodhareist's
/// NAMs folder, an impulse response into its IRs folder: written under `dir`
/// as `file_name`, deduped by name, a byte-identical file reused. Keep it in
/// step with `builtin_plugin_editor_window.rs`, so Studio and the plug-in
/// share one library. Background-thread only (filesystem I/O).
pub(crate) fn import_user_file_bytes(
    dir: &std::path::Path,
    kind: crate::components::builtin_plugin_files::BuiltinFileKind,
    file_name: &str,
    bytes: &[u8],
) -> Result<String, String> {
    use crate::components::builtin_plugin_files::sanitize_file_name;

    std::fs::create_dir_all(dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    let clean = sanitize_file_name(kind, file_name)
        .ok_or_else(|| format!("'{file_name}' is not a supported file name"))?;
    let (stem, ext) = clean
        .rsplit_once('.')
        .map(|(stem, ext)| (stem.to_string(), ext.to_string()))
        .unwrap_or((clean.clone(), String::new()));
    for n in 1..1000 {
        let name = if n == 1 {
            clean.clone()
        } else {
            format!("{stem} {n}.{ext}")
        };
        let target = dir.join(&name);
        if !target.exists() {
            std::fs::write(&target, bytes)
                .map_err(|e| format!("could not write {}: {e}", target.display()))?;
            return Ok(name);
        }
        if std::fs::read(&target).is_ok_and(|existing| existing == bytes) {
            return Ok(name);
        }
    }
    Err("could not find a free file name".to_string())
}

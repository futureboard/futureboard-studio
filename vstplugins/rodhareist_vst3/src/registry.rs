//! `SpherePluginHost::registry`, as far as the editor and `paths.rs` use it.

use std::path::PathBuf;

/// Studio's plug-in scan cache root (user presets are in
/// [`user_preset_root`]). Keep it in step with `SpherePluginHost::registry`.
pub fn default_preset_root() -> PathBuf {
    dirs::document_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Futureboard Studio")
        .join("Audio Plug-ins")
}

/// Where user presets live, one folder per plug-in:
/// `Documents/Futureboard Studio/Presets/Plug-ins`. A `.pst` saved in the
/// plug-in shows up in Studio and the other way round.
pub fn user_preset_root() -> PathBuf {
    dirs::document_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Futureboard Studio")
        .join("Presets")
        .join("Plug-ins")
}

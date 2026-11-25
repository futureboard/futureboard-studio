//! Spawn configuration shared by the process manager and IPC client.

use std::path::PathBuf;

/// Configuration for spawning a plugin-host child from the DAW.
#[derive(Debug, Clone)]
pub struct PluginHostSpawnConfig {
    pub instance_id: String,
    pub project_id: String,
    pub main_hwnd: Option<isize>,
    pub ipc_token: String,
    pub log_dir: PathBuf,
    pub plugin_path: Option<PathBuf>,
    /// Spawn the 32-bit host (`FutureboardPluginHostX86.exe`) instead of the
    /// 64-bit one, for plug-ins built for 32-bit Windows.
    pub x86: bool,
}

impl Default for PluginHostSpawnConfig {
    fn default() -> Self {
        Self {
            instance_id: "bridge-shared".to_string(),
            project_id: "studio".to_string(),
            main_hwnd: crate::plugin_host_main_window::main_window_hwnd(),
            ipc_token: crate::ipc::PROTOCOL_VERSION.to_string(),
            log_dir: crate::plugin_host_logging::default_log_dir(),
            plugin_path: None,
            x86: false,
        }
    }
}

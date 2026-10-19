//! Sphere plug-in host: VST3/VST2/CLAP scan (N-API cdylib for Electron, rlib for native GPUI).
//!
//! Electron loads `sphere_plugin_host` as `PluginHost.node`; GPUI links the rlib and uses
//! [`registry`] for types and VST3/CLAP registry scan.

#![allow(clippy::needless_pass_by_value)]
#![allow(non_snake_case)]

/// Audio Unit runtime, hosted in the plug-in host process like the built-ins
/// (AU has no in-process engine path). Non-macOS links a stub whose `open`
/// reports the platform, so the host's block path and dispatch can name AU
/// without a `cfg` at every branch.
pub mod au_host;
pub mod au_scanner;
/// Stage 2 lock-free shared-memory audio bridge layout (audio in/out, MIDI ring,
/// parameter-automation ring, status/latency/meter block) shared by the engine
/// and the `FutureboardPluginHostX64` process.
pub mod audio_bridge;
/// Curated catalog of Futureboard's built-in (stock) DSP plug-ins, surfaced to
/// the plug-in manager / Add-Track UI as ordinary [`registry::RegistryPlugin`]
/// rows (identified by a `builtin:` id).
pub mod builtin;
pub mod editor_quirk;
/// Control-thread manager for live built-in instances and the one shared CEF
/// editor binding per plug-in type.
pub mod instance_manager;
/// Cross-process IPC protocol (commands/events + JSON framing) shared by the
/// main app and the `FutureboardPluginHostX64` process.
pub mod ipc;
/// Plain-Rust facade over the editor C ABI — always built so the native
/// binary can drive the IPlugView lifecycle without N-API.
pub mod native_editor;
pub mod platform;
/// Stage 3b engine-facing realtime sink: implements DirectAudio's `PluginBridgeSink`
/// over the shared-memory audio region.
pub mod plugin_bridge_sink;
pub mod plugin_db;
/// Main-app client that spawns and drives the separated plugin host process.
pub mod plugin_host_client;
/// Windows job object + coordinated plugin-host shutdown for the main app.
pub mod plugin_host_lifecycle;
/// File logging for the separated plugin host process (hidden console builds).
pub mod plugin_host_logging;
pub mod plugin_host_main_window;
#[cfg(feature = "plugin-host-bin")]
pub mod plugin_host_preview;
pub mod plugin_host_spawn_config;
pub mod preset;
/// Central plugin-host process manager for the DAW session.
pub mod process_manager;
pub mod registry;
pub mod scan;
mod scanner;
pub mod spectrum;
/// Which hosted plug-ins may have changed their own state, and when the host
/// reports it to the studio (`HostEvent::PluginStateTouched`).
pub mod state_touch;
mod types;

pub use builtin::{
    builtin_audio_bridge_supported, builtin_catalog, builtin_display_name, builtin_editor_kind,
    builtin_has_native_editor, builtin_id, is_builtin_id, is_builtin_ref, resolve_builtin_stem,
    with_builtins, BuiltinEditorKind, BUILTIN_ID_PREFIX,
};
pub use editor_quirk::{
    detect_plugin_editor_runtime, match_quirk, PluginEditorHostMode, PluginEditorQuirk,
    PluginEditorRuntimeKind,
};
pub use instance_manager::{InsertSlot, InstanceId, InstanceManager, PluginInstance};
pub use plugin_db::{
    database_dir, database_exists, database_path, open_database, open_database_readonly,
    PluginCatalog, PluginCatalogEntry, PluginScanStatus,
};
pub use preset::{
    clear_all_presets, clear_plugin_cache, ensure_preset_folders, load_cached_plugins,
    read_preset_file, register_plugin, validate_plugin_for_registration, write_preset,
};
pub use registry::{
    classify_kind, default_preset_root, default_scan_paths, display_category, native_host_status,
    registry_plugin_from_scan, CatalogLoad, NativeHostStatus, PluginFormat, PluginKind,
    PluginRegistry, PluginScanFailure, PluginStatus, RegistryPlugin, RegistryScanResult,
    ScanOptions, ScanProgress,
};
pub use scan::{
    load_au_cache_state, save_au_cache_state, AuScanCacheState, FormatCacheStatus,
    PluginDescriptor, PluginScanError, PluginScanFormat, ScanResultPayload,
};
pub use scanner::{discover_plugin_bundles, scan_plugin_bundle};

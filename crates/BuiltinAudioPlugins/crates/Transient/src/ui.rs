//! Transient's editor origin.
//!
//! The editor is native GPUI now (`sphere_ui_components::components::dyn_window`);
//! the CEF `editorui/` bundle was removed. What stays is the stem Studio routes
//! this plug-in's inserts, state mirror and native editor by.

/// The plug-in's stem. Must match the `stem` in `SpherePluginHost`'s built-in
/// catalog.
pub const UI_ORIGIN: &str = "transient";

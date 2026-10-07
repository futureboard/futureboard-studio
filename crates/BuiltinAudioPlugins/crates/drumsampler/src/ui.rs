//! Drum Sampler's editor origin.
//!
//! The editor is native GPUI (`sphere_ui_components::components::drum_sampler_window`);
//! the CEF `editor/` bundle was removed. What stays is the stem Studio routes
//! this plug-in's inserts, state mirror and native editor by.

/// The plug-in's stem. Must match the `stem` in `SpherePluginHost`'s built-in
/// catalog.
pub const UI_ORIGIN: &str = "drumsampler";

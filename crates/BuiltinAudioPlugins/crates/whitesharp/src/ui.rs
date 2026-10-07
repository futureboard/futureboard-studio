//! WhiteSharp's editor origin.
//!
//! The editor is native GPUI (`sphere_ui_components::components::white_sharp_window`),
//! its pitch graph drawn with wgpu. What lives here is the stem Studio routes
//! this plug-in's inserts, state mirror and native editor by.

/// The plug-in's stem. Must match the `stem` in `SpherePluginHost`'s built-in
/// catalog.
pub const UI_ORIGIN: &str = "whitesharp";

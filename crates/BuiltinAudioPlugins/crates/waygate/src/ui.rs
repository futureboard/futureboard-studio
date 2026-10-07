//! WayGate's editor origin.
//!
//! The editor is native GPUI (`sphere_ui_components::components::gate_panel`)
//! in Studio, and a web port of it in LiveStage. What lives here is the stem
//! Studio routes this plug-in's inserts, state mirror and native editor by.

/// The plug-in's stem. Must match the `stem` in `SpherePluginHost`'s built-in
/// catalog and in LiveStage's `BUILTIN_EFFECTS`.
pub const UI_ORIGIN: &str = "waygate";

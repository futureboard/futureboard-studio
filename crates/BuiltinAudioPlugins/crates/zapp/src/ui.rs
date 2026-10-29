//! Zapp's editor origin.
//!
//! The editor is native GPUI (`sphere_ui_components`); it draws its transfer
//! curve, test-tone scope and harmonics from [`crate::saturate`], the curve
//! the DSP runs. What lives here is the stem Studio routes this plug-in's
//! inserts, state mirror and native editor by.

/// The plug-in's stem. Must match the `stem` in `SpherePluginHost`'s built-in
/// catalog.
pub const UI_ORIGIN: &str = "zapp";

use super::Plugin;

/// Auxiliary metadata needed for a VST2 plugin.
///
/// Enable the `vst2` feature and call [`nih_export_vst2!()`][crate::nih_export_vst2] to export the
/// plugin. VST2 uses a public C ABI, so this does not link against the Steinberg SDK.
pub trait Vst2Plugin: Plugin {
    /// Four-character unique id (`*b"Abcd"`). Hosts store this in projects to find the plugin.
    /// Pick an id that does not collide with another plugin and do not change it afterwards.
    const VST2_UID: [u8; 4];
}

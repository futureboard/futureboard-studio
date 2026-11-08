//! Studio's editor shell, without its chrome: in a plug-in the host's window
//! has the title bar, and there is no track or insert to name.

use gpui::{div, AnyElement, App, IntoElement, ParentElement, Styled, Window};

use crate::components::builtin_plugin_editor_window::{
    BuiltinEditorHostOps, BuiltinMeterSource, PluginInstanceKey,
};
use crate::theme::Colors;

pub trait NativeBuiltinEditor: gpui::Render + Sized {
    fn rebind_insert(
        &mut self,
        key: PluginInstanceKey,
        identity: ShellIdentity,
        host_ops: BuiltinEditorHostOps,
        cx: &mut gpui::Context<Self>,
    );
}

#[derive(Debug, Clone, PartialEq)]
pub struct ShellIdentity {
    pub plugin_name: String,
    pub track_name: String,
    pub insert_number: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ShellMeter {
    pub peak: f32,
    pub rms: f32,
    pub clip: bool,
}

impl ShellMeter {
    /// Nothing shows this meter here; the editor's own In and Out meters
    /// read the source.
    pub fn poll(&mut self, _source: Option<&BuiltinMeterSource>, _key: &PluginInstanceKey) -> bool {
        false
    }
}

/// Studio's shell with a preset menu in its strip. The plug-in has no strip,
/// and its host keeps the presets, so the menu is not shown.
pub fn native_plugin_shell_with(
    close_id: &'static str,
    identity: &ShellIdentity,
    level: ShellMeter,
    on_close: impl Fn(&mut Window, &mut App) + Clone + 'static,
    _presets: Option<AnyElement>,
    content: AnyElement,
) -> AnyElement {
    native_plugin_shell(close_id, identity, level, on_close, content)
}

pub fn native_plugin_shell(
    _close_id: &'static str,
    _identity: &ShellIdentity,
    _level: ShellMeter,
    _on_close: impl Fn(&mut Window, &mut App) + Clone + 'static,
    content: AnyElement,
) -> AnyElement {
    div()
        .size_full()
        .relative()
        .overflow_hidden()
        .font(crate::theme::ui_font())
        .bg(Colors::surface_window())
        .child(content)
        .into_any_element()
}

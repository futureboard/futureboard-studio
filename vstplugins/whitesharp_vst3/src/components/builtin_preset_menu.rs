//! Stands in for Studio's built-in preset menu: in the plug-in, the host
//! keeps the presets, so the menu draws nothing.

use gpui::{div, Context, IntoElement, Render, Window};

use crate::components::builtin_plugin_editor_window::PluginInstanceKey;
use crate::components::native_plugin_shell::ShellIdentity;

pub struct BuiltinPresetMenu;

impl BuiltinPresetMenu {
    pub fn new(
        _key: PluginInstanceKey,
        _identity: &ShellIdentity,
        _cx: &mut Context<Self>,
    ) -> Self {
        Self
    }

    pub fn rebind(
        &mut self,
        _key: PluginInstanceKey,
        _identity: &ShellIdentity,
        _cx: &mut Context<Self>,
    ) {
    }
}

impl Render for BuiltinPresetMenu {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

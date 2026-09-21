//! Global keymap manager instance for tooltip shortcut lookups.

use std::sync::OnceLock;

use gpui::{App, Window};

use crate::components::controls::fb_tooltip;
use crate::keymap::manager::KeymapManager;

static GLOBAL_KEYMAP: OnceLock<KeymapManager> = OnceLock::new();

/// Initialize the global keymap manager. Call once at app startup.
pub fn init_global_keymap(app_data: std::path::PathBuf) {
    GLOBAL_KEYMAP.set(KeymapManager::new(app_data)).ok();
}

/// Get the global keymap manager instance.
pub fn global_keymap() -> Option<&'static KeymapManager> {
    GLOBAL_KEYMAP.get()
}

/// Get the shortcut display string for a command, suitable for tooltips.
pub fn shortcut_for_command(command: &str) -> Option<String> {
    global_keymap().and_then(|k| k.shortcut_for_command(command))
}

/// Create a tooltip showing the shortcut for a command.
pub fn shortcut_tooltip(command: &str) -> impl Fn(&mut Window, &mut App) -> gpui::AnyView + 'static {
    let text = shortcut_for_command(command).unwrap_or_else(|| "—".to_string());
    fb_tooltip(text)
}
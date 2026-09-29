//! The studio's live keymap, for surfaces that show a shortcut but do not
//! own the manager: tooltips, menus, the command palette.

use std::sync::RwLock;

use gpui::{App, Window};

use crate::components::controls::fb_tooltip;
use crate::keymap::manager::KeymapManager;

/// Replaced whenever the studio's keymap changes — a profile switch or an
/// edit in the keymap window — so a tooltip never shows the key a profile
/// the user left behind used.
static GLOBAL_KEYMAP: RwLock<Option<KeymapManager>> = RwLock::new(None);

/// Initialize the global keymap manager. Call once at app startup.
pub fn init_global_keymap(app_data: std::path::PathBuf) {
    set_global_keymap(KeymapManager::new(app_data));
}

/// Publish the studio's current keymap.
pub fn set_global_keymap(manager: KeymapManager) {
    if let Ok(mut slot) = GLOBAL_KEYMAP.write() {
        *slot = Some(manager);
    }
}

/// Get the shortcut display string for a command, suitable for tooltips.
pub fn shortcut_for_command(command: &str) -> Option<String> {
    GLOBAL_KEYMAP
        .read()
        .ok()?
        .as_ref()
        .and_then(|keymap| keymap.shortcut_for_command(command))
}

/// The shortcut a menu row shows for `command`: the live keymap's once it is
/// loaded — including "none" — and the manifest's `fallback` before that.
pub fn global_keymap_shortcut(command: &str, fallback: Option<&str>) -> Option<String> {
    let loaded = GLOBAL_KEYMAP.read().ok().is_some_and(|slot| slot.is_some());
    if loaded {
        shortcut_for_command(command)
    } else {
        fallback.map(crate::keymap::accel_display)
    }
}

/// Create a tooltip showing the shortcut for a command.
pub fn shortcut_tooltip(
    command: &str,
) -> impl Fn(&mut Window, &mut App) -> gpui::AnyView + 'static {
    let text = shortcut_for_command(command).unwrap_or_else(|| "—".to_string());
    fb_tooltip(text)
}

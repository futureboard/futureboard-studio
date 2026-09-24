//! Keyboard shortcut profiles and the central [`KeymapManager`] service.

pub mod conflicts;
pub mod global;
pub mod manager;
pub mod model;
pub mod normalize;
pub mod storage;

pub use global::{init_global_keymap, global_keymap, shortcut_for_command, shortcut_tooltip};
pub use manager::{format_keystroke_list, profile_label, shortcut_debug_enabled, KeymapManager};
pub use model::{
    KeyBinding, KeymapConflict, KeymapProfile, KeymapRow, KeymapSource, ProfileDescriptor,
    ResolvedKeyBinding, PROFILE_DESCRIPTORS, USER_OVERRIDES_FILE,
};
pub use normalize::{
    accel_display, canonical_accel, canonical_event, canonical_keystroke, event_to_accel_string,
    format_accel_display, is_modifier_only_key, keystroke_to_accel_string, use_mac_accel_style,
};
pub use storage::{ensure_user_keymaps_dir, user_keymaps_dir};

/// Legacy compatibility wrapper used by older call sites.
#[derive(Debug, Clone, Default)]
pub struct Keymap {
    pub id: String,
    pub label: String,
    bindings: std::collections::HashMap<String, String>,
    reverse: std::collections::HashMap<String, String>,
}

impl Keymap {
    pub fn bundled_default() -> Self {
        KeymapManager::default().into_legacy()
    }

    pub fn load_profile(id: &str) -> Option<Self> {
        let mut manager = KeymapManager::default();
        manager.set_active_profile(id).ok()?;
        Some(manager.into_legacy())
    }

    pub fn command_for_event(&self, event: &gpui::KeyDownEvent) -> Option<&str> {
        let token = canonical_event(event)?;
        self.reverse.get(&token).map(String::as_str)
    }
}

impl KeymapManager {
    pub fn into_legacy(self) -> Keymap {
        let mut bindings = std::collections::HashMap::new();
        for row in self.rows() {
            if let Some(key) = row.keystrokes.first() {
                bindings.insert(row.action_id.clone(), key.clone());
            }
        }
        Keymap {
            id: self.active_profile_id().to_string(),
            label: self.active_profile_label().to_string(),
            bindings,
            reverse: self.dispatch_reverse().clone(),
        }
    }
}

/// Folder next to the executable (legacy install layout).
pub fn legacy_app_keymaps_dir() -> Option<std::path::PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|p| p.join("Keymaps")))
}

/// `{AppDir}/Keymaps` — legacy runtime profile path.
pub fn keymaps_dir() -> Option<std::path::PathBuf> {
    legacy_app_keymaps_dir()
}

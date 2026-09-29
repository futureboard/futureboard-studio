//! Keyboard shortcut profiles and the central [`KeymapManager`] service.
//!
//! A profile binds actions per [`KeymapScope`]: `global` keys work
//! everywhere, and each editor's keys work while that editor has the
//! keyboard. See `packages/keymaps/default.json`.

pub mod conflicts;
pub mod global;
pub mod manager;
pub mod model;
pub mod normalize;
pub mod storage;

pub use global::{
    global_keymap_shortcut, init_global_keymap, set_global_keymap, shortcut_for_command,
    shortcut_tooltip,
};
pub use manager::{format_keystroke_list, profile_label, shortcut_debug_enabled, KeymapManager};
pub use model::{
    KeyBinding, KeymapConflict, KeymapProfile, KeymapRow, KeymapScope, KeymapSource,
    ProfileDescriptor, ResolvedKeyBinding, PROFILE_DESCRIPTORS, USER_OVERRIDES_FILE,
};
pub use normalize::{
    accel_display, canonical_accel, canonical_event, canonical_keystroke, event_to_accel_string,
    format_accel_display, is_modifier_only_key, keystroke_to_accel_string, use_mac_accel_style,
};
pub use storage::{ensure_user_keymaps_dir, user_keymaps_dir};

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

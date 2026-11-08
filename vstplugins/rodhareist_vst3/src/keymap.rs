//! The plug-in has no command keymap: the host owns the shortcuts.

pub fn shortcut_for_command(_command: &str) -> Option<String> {
    None
}

/// A shortcut as a menu shows it. The text field's menu passes them ready to
/// read (`Ctrl+C`); Studio's version also rewrites them for macOS, which the
/// plug-in's Windows build does not need.
pub fn accel_display(accel: &str) -> String {
    accel.to_string()
}

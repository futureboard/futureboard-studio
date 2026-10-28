//! The plug-in has no command keymap: the host owns the shortcuts.

pub fn shortcut_for_command(_command: &str) -> Option<String> {
    None
}

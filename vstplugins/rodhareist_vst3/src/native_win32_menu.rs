//! The plug-in has no menu bar to re-theme.

pub fn apply_theme_to_all() {}

/// Studio forces its popup menus to the app theme through uxtheme's
/// process-wide app mode. Inside a host that would restyle the host's own
/// menus, so the plug-in's popups keep the host process's appearance.
#[cfg(windows)]
pub(crate) fn theme_popups_for(_hwnd: windows::Win32::Foundation::HWND) {}

//! Only Studio's floating-window opener reaches for this; the plug-in's
//! window is created and embedded by `nih_plug_gpui`.

pub fn external_dialog_window_options_partial() -> gpui::WindowOptions {
    gpui::WindowOptions::default()
}

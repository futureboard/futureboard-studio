//! Solfege Voxtune — the vocal note editor, on its own.
//!
//! The same editor Futureboard Studio shows in its tuner panel and the
//! VST3/ARA plug-in shows in a DAW, as a desktop app: open a vocal take,
//! edit its notes, hear the result, export it.
//!
//! ```txt
//! sphere_voxtune                       analysis, notes, TD-PSOLA render, undo
//! SphereUIComponents::voxtune          the editor view
//! this crate                           the window, the output device, files
//! ```

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod output;

use gpui::Application;
use sphere_ui_components::embedded_assets::EmbeddedAssets;
use sphere_ui_components::theme;

fn main() {
    // A file passed on the command line (or dropped on the exe) opens at once.
    let open = std::env::args_os().nth(1).map(std::path::PathBuf::from);
    // The Win32 menu bar is painted with GDI into the window's redirection
    // surface. DirectComposition windows have none (no redirection bitmap),
    // which leaves the bar's strip see-through; Studio turns it off for the
    // same reason (its native child windows). Set before GPUI reads it.
    #[cfg(target_os = "windows")]
    if std::env::var_os("GPUI_DISABLE_DIRECT_COMPOSITION").is_none() {
        // SAFETY: no other thread exists yet.
        unsafe { std::env::set_var("GPUI_DISABLE_DIRECT_COMPOSITION", "1") };
    }
    application()
        .with_assets(EmbeddedAssets::new())
        .run(move |cx| {
            let _ = theme::initialize_theme_system();
            let saved_theme = sphere_ui_components::settings::SettingsSchema::load_from_disk()
                .appearance
                .theme;
            let _ = theme::activate_theme_by_id(&saved_theme);
            sphere_ui_components::assets::register_fonts(cx);
            if let Err(error) = app::open_window(open, cx) {
                eprintln!("[voxtune] the window could not be opened: {error}");
                cx.quit();
            }
        });
}

fn application() -> Application {
    #[cfg(target_os = "windows")]
    let platform: std::rc::Rc<dyn gpui::Platform> = std::rc::Rc::new(
        gpui_windows::WindowsPlatform::new(false).expect("failed to initialize Windows platform"),
    );
    #[cfg(target_os = "macos")]
    let platform: std::rc::Rc<dyn gpui::Platform> =
        std::rc::Rc::new(gpui_macos::MacPlatform::new(false));
    #[cfg(target_os = "linux")]
    let platform: std::rc::Rc<dyn gpui::Platform> = gpui_linux::current_platform(false);

    Application::with_platform(platform)
}

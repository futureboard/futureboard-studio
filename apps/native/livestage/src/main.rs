//! LiveStage: a live mixer and effects rack.
//!
//! The engine ([`livestage_engine`]) does the work — the interface, the
//! strips, the inserts, the patch, the recorder. This binary is its desktop
//! face, built from Studio's controls and Studio's native built-in editors:
//!
//! * [`app`] — the window, its toolbar and the three views;
//! * [`strip`] — a channel, bus or master strip;
//! * [`patch`] — inputs to channels, mixes to outputs;
//! * [`setup`] — the interface and the recorder;
//! * [`editors`] — built-in editors (Studio's) and third-party ones (the
//!   plug-in host's, attached into a window of ours).
//!
//! The same engine runs without a window as `livestage-server`.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod editors;
mod fader_law;
mod patch;
#[cfg(feature = "ui-preview")]
mod preview;
mod setup;
mod strip;

use gpui::Application;
use sphere_ui_components::embedded_assets::EmbeddedAssets;
use sphere_ui_components::theme;

fn main() {
    // Third-party editors are child windows of ours; DirectComposition would
    // draw GPUI over them.
    #[cfg(target_os = "windows")]
    unsafe {
        std::env::set_var("GPUI_DISABLE_DIRECT_COMPOSITION", "1");
    }

    application().with_assets(EmbeddedAssets::new()).run(|cx| {
        let _ = theme::initialize_theme_system();
        let saved_theme = sphere_ui_components::settings::SettingsSchema::load_from_disk()
            .appearance
            .theme;
        let _ = theme::activate_theme_by_id(&saved_theme);
        sphere_ui_components::assets::register_fonts(cx);

        #[cfg(feature = "ui-preview")]
        {
            let mut args = std::env::args().skip(1);
            if args.next().as_deref() == Some("--preview") {
                let out = args
                    .next()
                    .unwrap_or_else(|| "target/ui-preview/livestage".into());
                preview::run(out.into(), cx);
                return;
            }
        }

        match app::open_main_window(cx) {
            // Editor windows come and go; the mixer's window is the app.
            Ok(main) => cx
                .on_window_closed(move |cx, _| {
                    if main.update(cx, |_, _, _| ()).is_err() {
                        cx.quit();
                    }
                })
                .detach(),
            Err(error) => {
                eprintln!("[livestage] the window could not be opened: {error}");
                cx.quit();
            }
        }
    });
}

fn application() -> Application {
    #[cfg(target_os = "windows")]
    let platform: std::rc::Rc<dyn gpui::Platform> = std::rc::Rc::new(
        gpui_windows::WindowsPlatform::new(false).expect("failed to initialize Windows platform"),
    );
    #[cfg(target_os = "linux")]
    let platform: std::rc::Rc<dyn gpui::Platform> = gpui_linux::current_platform(false);

    Application::with_platform(platform)
}

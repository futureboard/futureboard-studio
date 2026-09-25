#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod audio_state;
mod crash_reporting;
mod updater;
mod window;

#[cfg(feature = "professional")]
mod professional_edition;

use sphere_platform::platform;
use sphere_ui_components::boot;
use sphere_ui_components::embedded_assets::EmbeddedAssets;

fn main() {
    // A file-backed logger and startup marker must exist before CEF dispatch,
    // DirectX device creation, settings I/O, or any window construction.
    boot::init_process();

    // ── Phase -1 — CEF process dispatch ───────────────────────────────────────
    // CEF re-launches THIS executable for its own helper processes (renderer,
    // GPU, utility). Those launches must be detected and serviced before any
    // other initialization: a helper process must not create a window, open the
    // audio device, touch settings, or run the elevation check. `execute_process`
    // returns >= 0 only in a helper, and in that case the only correct action is
    // to exit with the code it hands back.
    //
    // The app passed here declares the `mikoplugin://` scheme. CEF requires the
    // same object to be handed to browser-process initialization, so ownership
    // is transferred to the UI-thread host after dispatch.
    #[cfg(feature = "builtin-plugin-editor")]
    dispatch_cef_process();

    // ── fbrd:// links ─────────────────────────────────────────────────────────
    // The OS launches a fresh process for a scheme URL. If a Studio is already
    // running this process hands the URL over and leaves before it has opened
    // an audio device, touched settings, or shown a window — none of which a
    // second instance should ever do. Otherwise the URL is queued and this
    // process becomes the instance that acts on it once it has booted.
    if let Some(url) = sphere_ui_components::deeplink::url_from_args(std::env::args_os().skip(1)) {
        if sphere_ui_components::deeplink::forward_to_running_instance(&url) {
            boot::log("fbrd link handed to the running instance");
            return;
        }
        sphere_ui_components::deeplink::enqueue(url);
    }

    // Privilege safety — must run before audio, plugins, settings, or project I/O.
    // Community and Professional Edition both block elevated launches. Developer
    // builds may opt in with `--features allow_elevated_for_testing`.
    platform::abort_if_elevated();

    // Install Crashpad before account, plugin, audio, or window initialization.
    // CEF helper processes have already exited from the dispatch above, so only
    // the browser/main process reaches this point.
    crash_reporting::install();

    // A Futureboard account is not an entitlement, so sign-in is installed for
    // every edition. Runs before the Professional install so a licensed build
    // layers licensing on top of the same identity rather than a second one.
    sphere_ui_components::account::install_default_account_provider();

    #[cfg(feature = "professional")]
    professional_edition::install().expect("failed to install Professional Edition providers");

    // ── Phase 0 — process setup ───────────────────────────────────────────────
    // env flags (before GPUI/window creation), panic hook, logging. No window,
    // no settings I/O, no device/plugin work here.
    boot::log("process setup start");
    eprintln!(
        "[process] role=main pid={} exe=futureboard_native",
        std::process::id()
    );
    // Publish the app package version to the shared UI crate. `CARGO_PKG_VERSION`
    // here is the application crate's version; the UI crate cannot read it
    // itself because its own package version differs.
    sphere_ui_components::edition::set_app_version(env!("CARGO_PKG_VERSION"));

    // Plugin runtime selection diagnostics. External PluginHost bridge is the
    // default; legacy in-process VST3 requires FUTUREBOARD_PLUGIN_LEGACY_IN_PROCESS=1.
    sphere_ui_components::plugin_host_client::log_bridge_env();
    let soundfont_backend =
        sphere_ui_components::soundfont_player::soundfont_player_backend_status();
    boot::log(&format!(
        "soundfont player backend: {} available={}",
        soundfont_backend.backend, soundfont_backend.available
    ));
    sphere_ui_components::plugin_host_lifecycle::init_plugin_host_job();
    // Same explicit AppUserModelID as the plugin-host process: keeps any
    // app-visible plugin window from spawning a stray taskbar identity.
    sphere_ui_components::plugin_host_lifecycle::set_futureboard_app_user_model_id();

    // Discord IPC is optional and never runs on the GPUI thread. The
    // application ID resolves to Futureboard's own unless a build or runtime
    // override points elsewhere, so a plain checkout gets Rich Presence without
    // any local configuration. Missing Discord itself must not block startup.
    let discord_rpc_enabled = sphere_ui_components::settings::SettingsSchema::load_from_disk()
        .general
        .discord_rpc_enabled;
    let discord_rpc = sphere_discord_rpc::DiscordRpcConfig::from_env(env!("CARGO_PKG_VERSION"))
        .and_then(|config| {
            match sphere_discord_rpc::DiscordRpc::start(
                config,
                sphere_discord_rpc::Presence::Welcome,
                discord_rpc_enabled,
            ) {
                Ok(rpc) => {
                    app::install_discord_rpc(rpc.handle());
                    Some(rpc)
                }
                Err(error) => {
                    boot::log(&format!("Discord RPC disabled: {error}"));
                    None
                }
            }
        });

    // Catch any panic that escapes the GPUI render loop so we see *why*
    // the window blanks out instead of getting a silent crash.
    std::panic::set_hook(Box::new(|info| {
        eprintln!("[panic] {info}");
        let bt = std::backtrace::Backtrace::force_capture();
        eprintln!("[panic] backtrace:\n{bt}");
        sphere_ui_components::plugin_host_lifecycle::PluginHostProcessManager::global()
            .shutdown_all(sphere_ui_components::plugin_host_lifecycle::HOST_SHUTDOWN_TIMEOUT)
            .ok();
        app::shutdown_discord_rpc();
    }));

    // GPUI's default DirectComposition target is created with topmost=true, which
    // draws above all WS_CHILD HWNDs. Plugin editors embed VST3 UI as children of
    // the GPUI window; without this, transparent windows show the DAW behind them
    // instead of the native plugin. Disabling DComp lets child HWNDs composite
    // above the swap chain. MUST be set before GPUI creates any window.
    #[cfg(target_os = "windows")]
    if std::env::var_os("GPUI_DISABLE_DIRECT_COMPOSITION").is_none() {
        std::env::set_var("GPUI_DISABLE_DIRECT_COMPOSITION", "1");
        boot::log("GPUI_DISABLE_DIRECT_COMPOSITION=1 (plugin editor HWND embedding)");
    }

    // The GPU chosen in Preferences → Performance. GPUI creates its D3D11 /
    // Metal device while the platform is built below, so the choice has to be
    // in place before `application()` — the settings model does not exist yet.
    sphere_ui_components::startup::export_gpu_adapter_preference();

    boot::log("process setup done");
    boot::log(
        "Initialize Win32/GPUI platform; Windows D3D11 device creation follows synchronously",
    );
    let application = application().with_assets(EmbeddedAssets::new());
    // macOS hands a `fbrd://` link to the running app as an Apple event; the
    // shell drains this queue on the UI thread. Windows and Linux never call
    // this — they launch a new process, which is handled above.
    application.on_open_urls(|urls| {
        for url in urls {
            sphere_ui_components::deeplink::enqueue(url);
        }
    });
    application.run(app::setup);
    #[cfg(feature = "builtin-plugin-editor")]
    sphere_ui_components::components::builtin_plugin_editor::shutdown();
    if let Some(discord_rpc) = discord_rpc {
        discord_rpc.shutdown();
    }
    boot::log("gpui application exited");
}

#[cfg(feature = "builtin-plugin-editor")]
fn dispatch_cef_process() {
    use sphere_webview::runtime::ProcessDispatch;

    sphere_webview::runtime::log_process_entry();
    let mut scheme_app = match sphere_webview::scheme::plugin_scheme_app() {
        Ok(app) => app,
        Err(error) => {
            handle_cef_process_setup_failure(&error);
            return;
        }
    };
    match sphere_webview::runtime::execute_subprocess(Some(&mut scheme_app)) {
        Ok(ProcessDispatch::SubprocessExit(code)) => {
            boot::log(&format!("CEF subprocess dispatch complete code={code}"));
            eprintln!(
                "[cef-process] subprocess_exit pid={} code={} before_futureboard_startup=true",
                std::process::id(),
                code
            );
            std::process::exit(code);
        }
        Ok(ProcessDispatch::BrowserProcess) => {
            boot::log("CEF execute_process returned browser process");
            if boot::has_flag("--disable-cef") {
                boot::log("CEF disabled by --disable-cef; native application remains available");
                return;
            }
            #[cfg(target_os = "macos")]
            if let Err(error) = gpui_macos::configure_cef_application() {
                eprintln!(
                    "[cef-process] macos_application_setup_failed error={error}; \
                     built-in plugin editors will be unavailable"
                );
                return;
            }
            if let Err(error) =
                sphere_ui_components::components::builtin_plugin_editor::install_process_app(
                    scheme_app,
                )
            {
                eprintln!("[cef-process] browser_app_install_failed error={error}");
            }
        }
        Err(error) => handle_cef_process_setup_failure(&error),
    }
}

#[cfg(feature = "builtin-plugin-editor")]
fn handle_cef_process_setup_failure(error: &sphere_webview::runtime::CefRuntimeError) {
    let subprocess = sphere_webview::runtime::is_subprocess_command_line();
    eprintln!(
        "[cef-process] setup_failed pid={} subprocess={} error={error}",
        std::process::id(),
        subprocess
    );
    if subprocess {
        std::process::exit(1);
    }
}

/// Text backend to build the Windows platform with.
///
/// The text system is created before the app — and therefore before the
/// settings model — exists, so the preference is read straight off disk here.
/// `SettingsSchema::load_from_disk` falls back to defaults on any read or parse
/// error, so a damaged settings file cannot stop the app from starting.
#[cfg(target_os = "windows")]
fn windows_text_backend() -> gpui_windows::WindowsTextBackend {
    use sphere_ui_components::settings::{SettingsSchema, TextRenderingBackend};

    match SettingsSchema::load_from_disk().appearance.text_rendering {
        TextRenderingBackend::DirectWrite => gpui_windows::WindowsTextBackend::DirectWrite,
        TextRenderingBackend::Gdi => gpui_windows::WindowsTextBackend::Gdi,
    }
}

/// Builds a GPUI [`Application`] with the correct OS platform backend.
///
/// The vendored standalone gpui removed `Application::new()`; the platform must
/// now be constructed explicitly. We mirror `gpui_platform::current_platform`
/// here instead of depending on `gpui_platform`, because that crate
/// force-enables gpui's `windows-manifest` feature, which would embed a second
/// application manifest and collide (CVT1100) with this binary's own manifest
/// from `app.rc`.
fn application() -> gpui::Application {
    #[cfg(target_os = "windows")]
    let platform: std::rc::Rc<dyn gpui::Platform> = std::rc::Rc::new(
        gpui_windows::WindowsPlatform::new_with_text_backend(false, windows_text_backend())
            .expect("failed to initialize Windows platform"),
    );

    #[cfg(target_os = "macos")]
    let platform: std::rc::Rc<dyn gpui::Platform> =
        std::rc::Rc::new(gpui_macos::MacPlatform::new(false));

    #[cfg(target_os = "linux")]
    let platform: std::rc::Rc<dyn gpui::Platform> = gpui_linux::current_platform(false);

    gpui::Application::with_platform(platform)
}

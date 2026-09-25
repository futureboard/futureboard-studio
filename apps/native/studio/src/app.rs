use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use crate::window::{studio_window_options, welcome_window_options};
use gpui::{App, AppContext, BorrowAppContext, Global, WindowHandle};
use sphere_ui_components::app_state::{AppMode, AppSessionGate};
use sphere_ui_components::assets;
use sphere_ui_components::boot;
use sphere_ui_components::components::progress_dialog::ProgressBarValue;
use sphere_ui_components::components::{
    message_box_dialog::{
        open_standalone_message_box_window, MessageBoxKind, MessageBoxOptions, MessageBoxResponseCb,
    },
    plugin_manager::open_startup_plugin_scan_progress_dialog,
};
use sphere_ui_components::layout::{
    PendingCloseAction, PreparedWorkspaceFinish, ProjectOpenOptions, StudioLayout,
};
use sphere_ui_components::loading_session::{
    begin_pre_studio_workspace_prepare, begin_project_session_load,
    begin_studio_project_session_load, begin_studio_session_shutdown, complete_project_lifecycle,
    is_loading_session_window_open, is_project_lifecycle_busy, set_return_to_welcome_handler,
    show_loading_session_error, update_loading_session_progress, FailureSurface, LoadFailedContext,
    LoadedSessionPackage, ProjectLifecycleTarget, ReplacementSurfaces, SessionRollbackSnapshot,
    SessionShutdownReason,
};
use sphere_ui_components::project::{FutureboardProject, ProjectTemplate};
use sphere_ui_components::settings::{GlobalSettingsModel, SettingsSchema};
use sphere_ui_components::splash::SplashWindowHandle;
use sphere_ui_components::startup::{
    log_startup_phase, run_lightweight_boot, StartupPhase, StartupRoute,
};
#[cfg(feature = "professional")]
use sphere_ui_components::welcome::WelcomeFooterAction;
use sphere_ui_components::welcome::{WelcomeAction, WelcomeCallbacks, WelcomeWindow};

static DISCORD_RPC: OnceLock<sphere_discord_rpc::DiscordRpcHandle> = OnceLock::new();
static UPDATE_CHECK_STARTED: AtomicBool = AtomicBool::new(false);

pub fn install_discord_rpc(handle: sphere_discord_rpc::DiscordRpcHandle) {
    let _ = DISCORD_RPC.set(handle);
}

pub fn shutdown_discord_rpc() {
    if let Some(rpc) = DISCORD_RPC.get() {
        rpc.request_shutdown();
    }
}

fn set_discord_enabled(enabled: bool) {
    if let Some(rpc) = DISCORD_RPC.get() {
        rpc.set_enabled(enabled);
    }
}

fn set_discord_presence(presence: sphere_discord_rpc::Presence) {
    if let Some(rpc) = DISCORD_RPC.get() {
        rpc.set_presence(presence);
    }
}

/// Retains shell window handles at app scope so GPUI never drops the last window
/// during Welcome → LoadingSession → Studio transitions, and so Welcome can be
/// retired (or reused) only once a replacement surface actually exists.
#[derive(Default)]
struct NativeShellWindows {
    studio: Option<WindowHandle<StudioLayout>>,
    welcome: Option<WindowHandle<WelcomeWindow>>,
}

impl Global for NativeShellWindows {}

pub fn setup(cx: &mut App) {
    boot::log("app boot start");
    cx.set_global(AppSessionGate {
        mode: AppMode::Welcome,
    });
    cx.set_global(NativeShellWindows::default());
    // A terminal error on the session transaction window needs a real way out.
    // Only the shell owns Welcome, so it supplies that route.
    set_return_to_welcome_handler(Arc::new(|cx: &mut App| {
        ensure_welcome_window(cx);
        finish_loading_to_welcome(cx);
    }));
    sphere_ui_components::settings::install_settings_change_listener(Arc::new(|settings| {
        set_discord_enabled(settings.general.discord_rpc_enabled);
    }));
    register_update_provider();
    install_deep_links(cx);

    let theme_report = sphere_ui_components::theme::initialize_theme_system();
    let saved_theme = sphere_ui_components::settings::SettingsSchema::load_from_disk()
        .appearance
        .theme;
    let _ = sphere_ui_components::theme::activate_theme_by_id(&saved_theme);
    boot::log(&format!(
        "theme initialized: {} ({})",
        theme_report.active_id, theme_report.active_name
    ));

    // Fonts must be registered before the first native view renders.
    assets::register_fonts(cx);
    boot::log("fonts registered");

    // On macOS CEF must install its integrated CFRunLoop observer during
    // applicationDidFinishLaunching, before GPUI creates the first native
    // window. Deferring initialization to the first main-queue turn is too
    // late and causes the pinned CEF framework to assert in its run-loop
    // observer. The probe under SphereWebView covers this ordering.
    #[cfg(all(feature = "builtin-plugin-editor", target_os = "macos"))]
    {
        use sphere_ui_components::components::builtin_plugin_editor;
        match builtin_plugin_editor::init_at_boot() {
            Ok(()) => {
                boot::log("builtin plugin editor host (CEF) initialized before first macOS window");
                builtin_plugin_editor::preload();
            }
            Err(err) => boot::log(&format!("builtin plugin editor host unavailable: {err}")),
        }
    }

    // Prewarm CEF on the GPUI UI thread, but outside this active App update.
    // CEF may synchronously dispatch Win32 messages while initializing; running
    // it as foreground work prevents those messages from re-entering a borrowed
    // AppCell. A failure stays non-fatal and is surfaced by the editor window.
    #[cfg(all(
        feature = "builtin-plugin-editor",
        not(target_os = "macos"),
        not(target_os = "windows")
    ))]
    cx.spawn(async move |cx| {
        use sphere_ui_components::components::builtin_plugin_editor;
        match builtin_plugin_editor::init_at_boot() {
            Ok(()) => {
                boot::log("builtin plugin editor host (CEF) initialized");
                // Warm-up browser: spawns Chromium's helper processes (GPU,
                // network, renderer) behind the loading screen so the first
                // editor open only pays for its own page.
                builtin_plugin_editor::preload();
                // Drive CEF briefly — with no editor window open nothing else
                // pumps its message loop, and the warm-up needs it to finish
                // launching those processes.
                if !boot::has_flag("--disable-cef-warmup")
                    && std::env::var_os("FUTUREBOARD_DISABLE_CEF_WARMUP").is_none()
                {
                    for _ in 0..150 {
                        cx.background_executor()
                            .timer(std::time::Duration::from_millis(16))
                            .await;
                        builtin_plugin_editor::pump();
                    }
                    boot::log("builtin plugin editor warm-up pump finished");
                } else {
                    boot::log("builtin plugin editor warm-up pump skipped");
                }
            }
            Err(err) => boot::log(&format!("builtin plugin editor host unavailable: {err}")),
        }
    })
    .detach();

    // Windows CEF initialization and CreateBrowserSync both run on the
    // browser-process UI thread. Keep them out of the native app's boot path;
    // the first built-in editor initializes CEF lazily after the Studio shell
    // is already usable. This also removes the fixed 2.4 s boot-time message
    // pump that existed solely for the hidden warm-up browser.
    #[cfg(all(feature = "builtin-plugin-editor", target_os = "windows"))]
    {
        if boot::has_flag("--disable-cef") {
            boot::log("CEF startup skipped by --disable-cef");
        } else if boot::has_flag("--disable-cef-warmup") {
            boot::log("CEF startup deferred; --disable-cef-warmup is active");
        } else {
            boot::log("CEF startup deferred until a built-in editor is requested");
        }
    }

    // macOS initialization and browser creation happened synchronously above
    // at the proven AppKit lifecycle point. Drive the warm-up after this App
    // update is released; CEF remains owned by the same main thread.
    #[cfg(all(feature = "builtin-plugin-editor", target_os = "macos"))]
    cx.spawn(async move |cx| {
        use sphere_ui_components::components::builtin_plugin_editor;
        if !boot::has_flag("--disable-cef")
            && !boot::has_flag("--disable-cef-warmup")
            && std::env::var_os("FUTUREBOARD_DISABLE_CEF_WARMUP").is_none()
        {
            for _ in 0..150 {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(16))
                    .await;
                builtin_plugin_editor::pump();
            }
            boot::log("builtin plugin editor warm-up pump finished");
        } else {
            boot::log("builtin plugin editor warm-up pump skipped");
        }
    })
    .detach();

    // Apply the saved renderer preference now (before any window) so the GPU
    // renderer can be warmed during the loading screen rather than stalling the
    // first studio frame.
    sphere_ui_components::layout::apply_saved_renderer_preference(cx);
    boot::log("renderer preference applied");

    // Splash first — before Welcome/Studio windows or heavy UI initialization.
    let splash = match SplashWindowHandle::open(cx) {
        Ok(handle) => Some(handle),
        Err(error) => {
            boot::log(&format!("splash open failed: {error}"));
            None
        }
    };

    cx.spawn(async move |cx| {
        // Adapter enumeration builds a Vulkan/DX12/GL instance and can take
        // seconds on a machine with several GPUs. It only records availability
        // for stem extraction's automatic device choice, which the user reaches
        // long after boot — so it runs off the main thread and boot does not
        // wait for it. It used to run inside `cx.update`, which froze the splash
        // for the whole enumeration while the splash was showing the adapter
        // names it had not finished reading.
        cx.background_executor()
            .spawn(async {
                let _ = sphere_ui_components::startup::probe_gpus();
            })
            .detach();
        if let Some(splash) = splash.as_ref() {
            cx.update(|app| splash.set_status(app, "Starting up…"));
        }

        let plan = run_lightweight_boot(cx).await;
        boot::log(&format!(
            "startup route resolved: {:?} (show_welcome={})",
            plan.route, plan.show_welcome_screen
        ));

        // On first launch the scan question is the next surface after Splash.
        // Welcome/Studio must not open behind it because that reverses the
        // intended startup flow and allows both windows to paint at once.
        let needs_scan_prompt = !SettingsSchema::load_from_disk()
            .general
            .plugin_scan_prompt_answered;
        if needs_scan_prompt {
            if let Some(splash) = splash.as_ref() {
                cx.update(|app| splash.set_status(app, "Checking installed plug-ins…"));
            }
            let prompt_opened = cx.update(open_first_launch_plugin_scan_prompt);
            if let Err(error) = prompt_opened {
                eprintln!("[plugin-scan] failed to open first-launch prompt: {error}");
                cx.update(open_welcome_after_startup_scan);
            }
            if let Some(splash) = splash {
                cx.update(|app| splash.close(app));
            }
            return;
        }

        // Open the next surface before closing splash so Windows does not quit
        // on LastWindowClosed while transitioning.
        if let Some(splash) = splash.as_ref() {
            let opening = match plan.route {
                StartupRoute::Welcome => "Opening Welcome…",
                _ => "Opening Studio…",
            };
            cx.update(|app| splash.set_status(app, opening));
        }
        match plan.route {
            StartupRoute::Welcome => {
                log_startup_phase(StartupPhase::OpeningWelcome);
                cx.update(open_welcome_window);
                // Work from an untitled session that never reached its first
                // save survives only as an autosave; offer it back once.
                cx.update(|app| {
                    sphere_ui_components::loading_session::offer_untitled_autosave_recovery(
                        Arc::new(|path, app| {
                            begin_load_project_from_welcome(
                                path,
                                ProjectOpenOptions::default(),
                                app,
                            );
                        }),
                        app,
                    );
                });
            }
            StartupRoute::EmptyWorkspace => {
                log_startup_phase(StartupPhase::OpeningStudio);
                cx.update(|app| {
                    begin_workspace_session(
                        "Preparing Workspace…",
                        FutureboardProject::new("Untitled Project"),
                        PreparedWorkspaceFinish::EmptyUntitled,
                        app,
                    );
                });
            }
            StartupRoute::RestoreLastProject(path) => {
                log_startup_phase(StartupPhase::OpeningStudio);
                cx.update(|app| {
                    begin_load_project_from_welcome(
                        path,
                        ProjectOpenOptions { from_recent: true },
                        app,
                    );
                });
            }
            StartupRoute::OpenProject(path) => {
                log_startup_phase(StartupPhase::OpeningStudio);
                cx.update(|app| {
                    begin_load_project_from_welcome(path, ProjectOpenOptions::default(), app);
                });
            }
        }

        if let Some(splash) = splash {
            cx.update(|app| splash.close(app));
        }

        // First-run EULA gate (Professional Edition). Opens on top of the first
        // surface; declining quits. No-op once accepted for this version.
        #[cfg(feature = "professional")]
        {
            let _ = cx.update(|app| crate::professional_edition::show_eula_if_needed(app));
        }
    })
    .detach();
}

fn persist_plugin_scan_prompt_answered(cx: &mut App) {
    let settings = cx
        .try_global::<GlobalSettingsModel>()
        .map(|global| global.0.clone());
    if let Some(settings) = settings {
        settings.update(cx, |settings, cx| {
            settings.update_setting(
                |schema| schema.general.plugin_scan_prompt_answered = true,
                cx,
            );
        });
    } else {
        SettingsSchema::persist_plugin_scan_prompt_answered();
    }
}

fn open_first_launch_plugin_scan_prompt(cx: &mut App) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let scan_locations = "the standard VST3, CLAP, and AudioUnit locations";
    #[cfg(not(target_os = "macos"))]
    let scan_locations = "the standard VST3 and CLAP locations";

    let options = MessageBoxOptions::new("Scan installed plug-ins now?")
        .title("Plug-in Scan")
        .detail(format!(
            "Futureboard will scan {scan_locations}. You can run this later from Plug-in Manager."
        ))
        .kind(MessageBoxKind::Question)
        .buttons(["Yes", "No"])
        .default_id(0)
        .cancel_id(1);
    let on_response: MessageBoxResponseCb =
        Arc::new(|result, _window: &mut gpui::Window, cx: &mut App| {
            persist_plugin_scan_prompt_answered(cx);
            if result.response == 0 {
                let on_complete = Arc::new(open_welcome_after_startup_scan);
                if let Err(error) = open_startup_plugin_scan_progress_dialog(on_complete, cx) {
                    eprintln!("[plugin-scan] failed to open first-launch scanner: {error}");
                    open_welcome_after_startup_scan(cx);
                }
            } else {
                open_welcome_after_startup_scan(cx);
            }
        });
    open_standalone_message_box_window(options, on_response, cx).map(|_| ())
}

fn open_welcome_after_startup_scan(cx: &mut App) {
    log_startup_phase(StartupPhase::OpeningWelcome);
    open_welcome_window(cx);

    // First-run EULA gate (Professional Edition) belongs on the first real app
    // surface, after the plug-in scan handoff has completed.
    #[cfg(feature = "professional")]
    let _ = crate::professional_edition::show_eula_if_needed(cx);
}

fn set_app_mode(cx: &mut App, mode: AppMode) {
    let from = cx
        .try_global::<AppSessionGate>()
        .map(|gate| gate.mode)
        .unwrap_or(AppMode::Welcome);
    if from != mode {
        sphere_ui_components::window_lifecycle::log_app_mode_change(from, mode, "app_shell");
        match mode {
            AppMode::Welcome | AppMode::LoadFailed => {
                set_discord_presence(sphere_discord_rpc::Presence::Welcome)
            }
            AppMode::LoadingSession => set_discord_presence(sphere_discord_rpc::Presence::Loading),
            AppMode::Studio => {}
        }
    }
    cx.update_global::<AppSessionGate, _>(|gate, _| gate.mode = mode);
}

fn studio_shell_alive(cx: &App) -> bool {
    cx.try_global::<NativeShellWindows>()
        .map(|shell| shell.studio.is_some())
        .unwrap_or(false)
}

/// Windows Welcome can currently be handed off to.
fn replacement_surfaces(cx: &App) -> ReplacementSurfaces {
    ReplacementSurfaces {
        transaction_window_open: is_loading_session_window_open(cx),
        studio_mounted: studio_shell_alive(cx),
    }
}

/// Close Welcome only when another window is already on screen. Retiring it any
/// earlier leaves the app with nothing visible — and nothing to report a failed
/// handoff on if the studio never mounts.
fn retire_welcome_if_replaced(welcome_window: &mut gpui::Window, cx: &mut App) {
    if !replacement_surfaces(cx).may_retire_welcome() {
        eprintln!("[WindowLifecycle] kept Welcome — no replacement surface yet");
        return;
    }
    cx.update_global::<NativeShellWindows, _>(|shell, _| {
        shell.welcome = None;
    });
    welcome_window.remove_window();
}

/// Retire a Welcome window that survived a handoff because the transaction ran
/// without its own window.
fn retire_welcome_window(cx: &mut App) {
    let Some(welcome) = cx
        .try_global::<NativeShellWindows>()
        .and_then(|shell| shell.welcome)
    else {
        return;
    };
    cx.update_global::<NativeShellWindows, _>(|shell, _| {
        shell.welcome = None;
    });
    let _ = welcome.update(cx, |_view, window, _cx| window.remove_window());
    eprintln!("[WindowLifecycle] retired Welcome after replacement window opened");
}

/// Bring the start screen back, reusing the live Welcome window when the handoff
/// never retired it so the user never gets a second copy.
fn ensure_welcome_window(cx: &mut App) -> Option<WindowHandle<WelcomeWindow>> {
    if let Some(handle) = cx
        .try_global::<NativeShellWindows>()
        .and_then(|shell| shell.welcome)
    {
        if handle
            .update(cx, |_view, window, _cx| window.activate_window())
            .is_ok()
        {
            set_app_mode(cx, AppMode::Welcome);
            eprintln!("[WindowLifecycle] reused live Welcome window");
            return Some(handle);
        }
        cx.update_global::<NativeShellWindows, _>(|shell, _| {
            shell.welcome = None;
        });
    }
    open_welcome_window(cx);
    cx.try_global::<NativeShellWindows>()
        .and_then(|shell| shell.welcome)
}

fn log_window_registry(cx: &App, stage: &str) {
    let studio_alive = studio_shell_alive(cx);
    let loader_alive = sphere_ui_components::loading_session::is_loading_session_window_open(cx);
    let app_mode = cx
        .try_global::<AppSessionGate>()
        .map(|gate| gate.mode.label())
        .unwrap_or("unknown");
    sphere_ui_components::window_lifecycle::log_shell_window_registry(
        stage,
        studio_alive,
        loader_alive,
        app_mode,
    );
}

/// Wire the `fbrd://` scheme: own it, listen for later launches, and act on
/// what arrives.
///
/// macOS delivers URLs to the running app through Apple events, which GPUI
/// surfaces as `Application::on_open_urls` (hooked in `main`); Windows and
/// Linux launch a new process, which `main` forwards over the relay. Both land
/// in the same queue, drained here on the UI thread at a rate a person cannot
/// notice and a frame does not feel.
fn install_deep_links(cx: &mut App) {
    use sphere_ui_components::deeplink;

    deeplink::start_relay_listener();
    deeplink::ensure_registered();
    #[cfg(target_os = "macos")]
    {
        let task = cx.register_url_scheme(deeplink::SCHEME);
        cx.spawn(async move |_cx| match task.await {
            Ok(()) => deeplink::mark_registered(true),
            Err(error) => boot::log(&format!("fbrd scheme registration failed: {error}")),
        })
        .detach();
    }

    cx.spawn(async move |cx| loop {
        cx.background_executor()
            .timer(Duration::from_millis(200))
            .await;
        let urls = deeplink::drain();
        if urls.is_empty() {
            continue;
        }
        cx.update(|cx| {
            for url in urls {
                handle_deep_link(&url, cx);
            }
        });
    })
    .detach();
}

/// A jam link that arrived before there was a Studio window to open it in.
fn pending_jam_link() -> &'static std::sync::Mutex<Option<String>> {
    static PENDING: std::sync::OnceLock<std::sync::Mutex<Option<String>>> =
        std::sync::OnceLock::new();
    PENDING.get_or_init(|| std::sync::Mutex::new(None))
}

fn handle_deep_link(url: &str, cx: &mut App) {
    use sphere_ui_components::deeplink::{self, DeepLink};

    match deeplink::parse(url) {
        Some(DeepLink::AuthCallback {
            state,
            session,
            error,
        }) => {
            if !sphere_ui_components::auth::complete_scheme_sign_in(
                &state,
                &session,
                error.as_deref(),
            ) {
                boot::log("fbrd sign-in callback ignored: nothing was waiting for it");
            }
        }
        Some(DeepLink::Jam { link }) => {
            let studio = cx.global::<NativeShellWindows>().studio;
            match studio {
                Some(handle) => open_jam_from_link(handle, link, cx),
                None => {
                    // Welcome is up, or Studio is still loading. Held until a
                    // Studio window exists; see `store_studio_window_handle`.
                    boot::log("fbrd jam link held until Studio opens");
                    if let Ok(mut pending) = pending_jam_link().lock() {
                        *pending = Some(link);
                    }
                }
            }
        }
        None => boot::log(&format!("fbrd link not understood: {url}")),
    }
}

/// Open the Audio Jam window in `studio` and join what `link` names.
///
/// Opening the window is what installs the jam controller, so it happens
/// first and synchronously; the join is a network round trip and runs on the
/// background pool exactly as a pasted link does.
fn open_jam_from_link(studio: WindowHandle<StudioLayout>, link: String, cx: &mut App) {
    let opened = studio.update(cx, |layout, _window, cx| {
        layout.dispatch_command_id("window:audio-jam", cx);
    });
    if opened.is_err() {
        boot::log("fbrd jam link: the Studio window is gone");
        return;
    }
    cx.background_executor()
        .spawn(async move {
            if let Err(error) = sphere_ui_components::jam::with_controller(|controller| {
                controller.join_with_link(&link)
            }) {
                boot::log(&format!(
                    "fbrd jam link: join failed: {}",
                    error.user_message()
                ));
            }
        })
        .detach();
}

fn store_studio_window_handle(cx: &mut App, handle: WindowHandle<StudioLayout>) {
    cx.update_global::<NativeShellWindows, _>(|shell, _| {
        shell.studio = Some(handle);
    });
    eprintln!("[StudioOpen] stored studio window handle");
    // A jam link that arrived while Welcome was up is acted on now that there
    // is a Studio to open the window in.
    let held = pending_jam_link()
        .lock()
        .ok()
        .and_then(|mut pending| pending.take());
    if let Some(link) = held {
        open_jam_from_link(handle, link, cx);
    }
}

fn schedule_auto_update_check(cx: &mut App) {
    if cfg!(debug_assertions) && std::env::var("FUTUREBOARD_ENABLE_UPDATER").as_deref() != Ok("1") {
        return;
    }
    let general = SettingsSchema::load_from_disk().general;
    if !general.check_updates || UPDATE_CHECK_STARTED.swap(true, Ordering::AcqRel) {
        return;
    }

    // Go through the registered provider rather than calling the GitHub
    // transport directly: a Professional build replaces it with the licensed R2
    // one, and the automatic check must resolve the same release the menu
    // command would.
    let Some(provider) = sphere_ui_components::update_service::update_provider() else {
        return;
    };
    let channel = general.update_channel;
    let executor = cx.background_executor().clone();
    cx.spawn(async move |cx| {
        executor.timer(Duration::from_secs(3)).await;
        let result = executor.spawn(async move { provider.check(channel) }).await;
        match result {
            Ok(Some(candidate)) => cx.update(|app| show_update_available(candidate, app)),
            Ok(None) => {}
            Err(error) => boot::log(&format!("automatic update check unavailable: {error}")),
        }
    })
    .detach();
}

/// Register the update transport the Software Update dialog (Help → Check for
/// Updates) drives.
///
/// Community resolves releases from the public GitHub list. A Professional
/// build gets the licensed Cloudflare R2 transport instead — its installers are
/// not published on GitHub, so falling back there would offer a Community build
/// as an "update" to a paid one.
fn register_update_provider() {
    #[cfg(feature = "professional")]
    if crate::professional_edition::register_update_provider() {
        return;
    }

    sphere_ui_components::update_service::set_update_provider(std::sync::Arc::new(
        crate::updater::GithubUpdateProvider,
    ));
}

/// The automatic check hands the release it found to the same Software Update
/// window the menu command opens, so download / install has one surface.
fn show_update_available(
    candidate: sphere_ui_components::update_service::UpdateCandidate,
    cx: &mut App,
) {
    if let Err(error) =
        sphere_ui_components::components::update_dialog::open_update_dialog_for(candidate, cx)
    {
        boot::log(&format!("failed to show software update prompt: {error}"));
    }
}

fn finish_loading_to_studio(cx: &mut App) {
    log_window_registry(cx, "before loading ui close");
    eprintln!("[ProjectSwitch] close loading ui");
    // Never close the loading window synchronously from a path that may still be
    // inside a LoadingSessionWindow update (GPUI double-lease). Activate the
    // studio only after the loader is gone so users never stare at an empty shell.
    cx.defer(|cx| {
        if !studio_shell_alive(cx) {
            eprintln!("[WindowLifecycle] refused to close loader — studio shell not alive");
            return;
        }
        complete_project_lifecycle(cx, ProjectLifecycleTarget::Studio);
        // Welcome is still up when the transaction ran without its own window.
        // The studio shell is the confirmed replacement, so retire it now.
        retire_welcome_window(cx);
        if let Some(studio) = cx
            .try_global::<NativeShellWindows>()
            .and_then(|shell| shell.studio)
        {
            let _ = studio.update(cx, |_layout, window, _cx| {
                window.activate_window();
            });
        }
        log_window_registry(cx, "after loading ui close");
        eprintln!("[SessionLoad] studio ready");
        schedule_auto_update_check(cx);
    });
}

fn finish_loading_to_welcome(cx: &mut App) {
    log_window_registry(cx, "before loading ui close (welcome)");
    eprintln!("[ProjectClose] lifecycle completing target=welcome");
    complete_project_lifecycle(cx, ProjectLifecycleTarget::Welcome);
    log_window_registry(cx, "after loading ui close (welcome)");
    eprintln!("[ProjectClose] close-to-welcome complete");
}

fn transition_loaded_package_to_existing_studio(
    studio: gpui::WindowHandle<StudioLayout>,
    package: LoadedSessionPackage,
    cx: &mut App,
) {
    let project_name = package.project.name.clone();
    log_window_registry(cx, "before install into existing studio");
    eprintln!("[ProjectSwitch] installing loaded session");
    eprintln!("[AppMode] LoadingSession -> Studio");
    set_app_mode(cx, AppMode::Studio);
    store_studio_window_handle(cx, studio);
    let install_result = studio.update(cx, |layout, _window, cx| {
        layout.install_loaded_session(package, cx);
        if !layout.has_self_window() {
            layout.set_self_window(studio);
        }
        cx.notify();
    });
    if install_result.is_err() {
        eprintln!("[ProjectSwitch] install into existing studio failed");
        set_app_mode(cx, AppMode::LoadFailed);
        report_load_failure(
            LoadFailedContext {
                title: "Open Project Failed".to_string(),
                message: "The loaded project could not be installed into the studio.".to_string(),
                detail: None,
                path: None,
                open_options: ProjectOpenOptions::default(),
                rollback: None,
            },
            cx,
        );
        return;
    }
    eprintln!("[SessionLoad] install complete");
    set_discord_presence(sphere_discord_rpc::Presence::editing(project_name));
    eprintln!("[ProjectSwitch] notify root window");
    finish_loading_to_studio(cx);
    log_window_registry(cx, "after switch install");
    eprintln!("[ProjectSwitch] complete");
}

/// Open the Welcome window (start screen). Used after splash boot and when
/// returning from Close Project — never shows the splash window again.
fn open_welcome_window(cx: &mut App) {
    set_app_mode(cx, AppMode::Welcome);
    schedule_auto_update_check(cx);
    let callbacks = WelcomeCallbacks {
        on_action: Arc::new(|action, welcome_window, cx| {
            // Welcome now outlives the start of a handoff whenever the session
            // transaction has no window of its own, so it must refuse to start a
            // second transaction on top of the one already running.
            if is_project_lifecycle_busy() {
                eprintln!(
                    "[WindowLifecycle] ignored Welcome action — session transaction in flight"
                );
                return;
            }
            match action {
                // Retire Welcome only once the session transaction window (or the
                // studio shell itself) is up. On Linux, QuitMode::LastWindowClosed
                // would otherwise quit the app the moment Welcome closes with no
                // replacement window yet; on macOS the user would be left staring at
                // nothing while the workspace prepares headless.
                WelcomeAction::OpenProjectFile(path) => {
                    begin_load_project_from_welcome(path, ProjectOpenOptions::default(), cx);
                    retire_welcome_if_replaced(welcome_window, cx);
                }
                WelcomeAction::OpenRecent(path) => {
                    begin_load_project_from_welcome(
                        path,
                        ProjectOpenOptions { from_recent: true },
                        cx,
                    );
                    retire_welcome_if_replaced(welcome_window, cx);
                }
                other => {
                    open_studio_for_action(other, cx);
                    retire_welcome_if_replaced(welcome_window, cx);
                }
            }
        }),
        footer_action: {
            #[cfg(feature = "professional")]
            {
                Some(WelcomeFooterAction {
                    label: "License",
                    icon: assets::ICON_FILE_PATH,
                    on_click: Arc::new(|welcome_window, cx| {
                        crate::professional_edition::open_license_activation(
                            Some(welcome_window.bounds()),
                            cx,
                        );
                    }),
                })
            }
            #[cfg(not(feature = "professional"))]
            {
                None
            }
        },
    };
    let welcome_options = welcome_window_options(cx);
    match cx.open_window(welcome_options, |window, cx| {
        window.on_next_frame(|_window, _cx| boot::complete_startup());
        cx.new(|cx| WelcomeWindow::new(callbacks, cx.focus_handle()))
    }) {
        Ok(handle) => {
            cx.update_global::<NativeShellWindows, _>(|shell, _| {
                shell.welcome = Some(handle);
            });
            boot::log("welcome window shown");
        }
        Err(error) => {
            // Never panic here: a caller recovering from a failed handoff still
            // has the transaction window or a message box to report on.
            eprintln!("[WindowLifecycle] welcome window open failed: {error}");
            boot::log(&format!("welcome window open failed: {error}"));
        }
    }
}

fn transition_loaded_package_to_studio(package: LoadedSessionPackage, cx: &mut App) {
    eprintln!("[AppMode] LoadingSession -> Studio");
    set_app_mode(cx, AppMode::Studio);
    update_loading_session_progress(cx, "Opening studio", ProgressBarValue::value(0.98));
    cx.defer(
        move |cx| match open_studio_workspace(WorkspaceInit::Loaded(package), cx) {
            Ok(()) => {
                eprintln!("[StudioMount] mounted after ready");
                finish_loading_to_studio(cx);
            }
            Err(error) => report_studio_open_failure(error, cx),
        },
    );
}

fn report_studio_open_failure(error: String, cx: &mut App) {
    eprintln!("[StudioOpen] studio window open failed: {error}");
    set_app_mode(cx, AppMode::LoadFailed);
    report_load_failure(
        LoadFailedContext {
            title: "Open Workspace Failed".to_string(),
            message: "The studio workspace window could not be opened.".to_string(),
            detail: Some(format!("Details: {error}")),
            path: None,
            open_options: ProjectOpenOptions::default(),
            rollback: None,
        },
        cx,
    );
}

fn transition_prepared_package_to_studio(
    package: LoadedSessionPackage,
    follow_up: PreparedWorkspaceFinish,
    cx: &mut App,
) {
    eprintln!("[AppMode] LoadingSession -> Studio (prepared workspace)");
    set_app_mode(cx, AppMode::Studio);
    update_loading_session_progress(cx, "Opening studio", ProgressBarValue::value(0.98));
    let init = WorkspaceInit::Prepared { package, follow_up };
    cx.defer(move |cx| match open_studio_workspace(init, cx) {
        Ok(()) => {
            eprintln!("[StudioMount] mounted after workspace prepare");
            finish_loading_to_studio(cx);
        }
        Err(error) => report_studio_open_failure(error, cx),
    });
}

fn begin_close_project_session(
    reason: SessionShutdownReason,
    owner_bounds: Option<gpui::Bounds<gpui::Pixels>>,
    studio: gpui::WindowHandle<StudioLayout>,
    cx: &mut App,
) {
    let on_complete = Arc::new(move |cx: &mut App| {
        let _ = studio.update(cx, |layout, window, cx| {
            sphere_ui_components::window_position::persist_studio_window_from_window(window, cx);
            window.remove_window();
            let _ = layout;
        });
        cx.update_global::<NativeShellWindows, _>(|shell, _| {
            shell.studio = None;
        });
        set_app_mode(cx, AppMode::Welcome);
        open_welcome_window(cx);
        finish_loading_to_welcome(cx);
    });
    begin_studio_session_shutdown(reason, studio, owner_bounds, true, on_complete, cx);
}

fn begin_load_project_from_welcome(path: PathBuf, open_options: ProjectOpenOptions, cx: &mut App) {
    set_discord_presence(sphere_discord_rpc::Presence::Loading);
    let on_success = Arc::new(|package: LoadedSessionPackage, cx: &mut App| {
        transition_loaded_package_to_studio(package, cx);
    });
    let on_failure = Arc::new(|ctx: LoadFailedContext, cx: &mut App| {
        handle_load_failed(ctx, cx);
    });
    begin_project_session_load(
        path,
        open_options,
        None,
        None,
        None,
        on_success,
        on_failure,
        cx,
    );
}

fn begin_workspace_session(
    heading: &str,
    project: FutureboardProject,
    follow_up: PreparedWorkspaceFinish,
    cx: &mut App,
) {
    set_discord_presence(sphere_discord_rpc::Presence::Loading);
    let on_success = Arc::new(move |package: LoadedSessionPackage, cx: &mut App| {
        transition_prepared_package_to_studio(package, follow_up.clone(), cx);
    });
    let on_failure = Arc::new(|ctx: LoadFailedContext, cx: &mut App| {
        handle_load_failed(ctx, cx);
    });
    begin_pre_studio_workspace_prepare(heading, project, on_success, on_failure, cx);
}

fn handle_project_switch_load_failed(
    studio: gpui::WindowHandle<StudioLayout>,
    ctx: LoadFailedContext,
    cx: &mut App,
) {
    eprintln!(
        "[SessionLoad] project switch failed: {} — {}",
        ctx.title, ctx.message
    );
    if let Some(snapshot) = ctx.rollback {
        let project_name = snapshot.session.name.clone();
        set_app_mode(cx, AppMode::Studio);
        store_studio_window_handle(cx, studio);
        let _ = studio.update(cx, |layout, _window, cx| {
            layout.restore_session_rollback_snapshot(snapshot, cx);
            layout.show_project_open_failed_dialog(
                &ctx.title,
                &ctx.message,
                ctx.detail.clone(),
                ctx.path,
                ctx.open_options,
                cx,
            );
            cx.notify();
        });
        set_discord_presence(sphere_discord_rpc::Presence::editing(project_name));
        finish_loading_to_studio(cx);
        log_window_registry(cx, "after switch failure rollback");
        return;
    }
    handle_load_failed(ctx, cx);
}

fn request_project_switch_in_studio(
    studio: gpui::WindowHandle<StudioLayout>,
    path: PathBuf,
    open_options: ProjectOpenOptions,
    cx: &mut App,
) {
    // Never call `studio.update` synchronously here — this hook is invoked from
    // inside an active `StudioLayout` update (e.g. File → Open Project).
    cx.defer(move |cx| {
        log_window_registry(cx, "before switch");
        eprintln!("[ProjectSwitch] begin target={}", path.display());
        eprintln!(
            "[ProjectSwitch] AppMode before={}",
            cx.try_global::<AppSessionGate>()
                .map(|gate| gate.mode.label())
                .unwrap_or("unknown")
        );

        let prepared = studio.update(cx, |layout, _window, cx| {
            layout.prepare_for_in_studio_project_switch_transaction(cx)
        });
        let Ok((rollback, owner_bounds)) = prepared else {
            eprintln!("[ProjectSwitch] failed to prepare studio for in-studio switch");
            return;
        };

        eprintln!("[SessionShutdown] begin reason=project_switch");
        log_window_registry(cx, "after prepare before load");
        set_app_mode(cx, AppMode::LoadingSession);
        eprintln!("[ProjectSwitch] AppMode -> LoadingSession");

        // Keep the studio shell alive — on Windows GPUI quits when the last
        // window closes (QuitMode::LastWindowClosed).
        let studio_for_success = studio;
        let studio_for_failure = studio;
        let on_success = Arc::new(move |package: LoadedSessionPackage, cx: &mut App| {
            transition_loaded_package_to_existing_studio(studio_for_success, package, cx);
        });
        let on_failure = Arc::new(move |ctx: LoadFailedContext, cx: &mut App| {
            handle_project_switch_load_failed(studio_for_failure, ctx, cx);
        });
        eprintln!("[SessionLoad] begin target={}", path.display());
        begin_studio_project_session_load(
            path,
            open_options,
            rollback,
            studio,
            owner_bounds,
            on_success,
            on_failure,
            cx,
        );
    });
}

fn handle_load_failed(ctx: LoadFailedContext, cx: &mut App) {
    eprintln!("[SessionLoad] open failed: {} — {}", ctx.title, ctx.message);
    if let Some(snapshot) = ctx.rollback {
        let project_name = snapshot.session.name.clone();
        set_app_mode(cx, AppMode::Studio);
        if let Some(studio) = cx
            .try_global::<NativeShellWindows>()
            .and_then(|shell| shell.studio)
        {
            log_window_registry(cx, "rollback onto existing studio");
            let _ = studio.update(cx, |layout, _window, cx| {
                layout.restore_session_rollback_snapshot(snapshot, cx);
                cx.notify();
            });
            set_discord_presence(sphere_discord_rpc::Presence::editing(project_name));
            finish_loading_to_studio(cx);
            return;
        }
        match open_studio_workspace(WorkspaceInit::Rollback(snapshot), cx) {
            Ok(()) => finish_loading_to_studio(cx),
            Err(error) => {
                eprintln!("[StudioOpen] rollback studio open failed: {error}");
                set_app_mode(cx, AppMode::LoadFailed);
                report_load_failure(
                    LoadFailedContext {
                        title: ctx.title,
                        message: ctx.message,
                        detail: Some(format!("Rollback failed: {error}")),
                        path: ctx.path,
                        open_options: ctx.open_options,
                        rollback: None,
                    },
                    cx,
                );
            }
        }
        return;
    }
    set_app_mode(cx, AppMode::LoadFailed);
    report_load_failure(ctx, cx);
}

/// Show why a pre-studio transaction failed and leave one obvious way forward.
/// The user is never dropped back onto a fresh Welcome screen with no reason.
fn report_load_failure(ctx: LoadFailedContext, cx: &mut App) {
    match replacement_surfaces(cx).failure_surface() {
        FailureSurface::TransactionWindow => {
            let detail = ctx
                .detail
                .as_deref()
                .map(|detail| format!("\n\n{detail}"))
                .unwrap_or_default();
            if show_loading_session_error(cx, ctx.title.clone(), format!("{}{detail}", ctx.message))
            {
                eprintln!("[SessionLoad] failure shown on transaction window");
                return;
            }
            report_load_failure_on_welcome(ctx, cx);
        }
        FailureSurface::WelcomeWindow => report_load_failure_on_welcome(ctx, cx),
    }
}

fn report_load_failure_on_welcome(ctx: LoadFailedContext, cx: &mut App) {
    use sphere_ui_components::components::message_box_dialog::{
        open_message_box_window, MessageBoxKind, MessageBoxOptions, MessageBoxResponseCb,
    };

    let welcome = ensure_welcome_window(cx);
    complete_project_lifecycle(cx, ProjectLifecycleTarget::Welcome);

    let owner_bounds = welcome.and_then(|handle| {
        handle
            .update(cx, |_view, window, _cx| window.bounds())
            .ok()
            .filter(|bounds| sphere_ui_components::window_position::is_valid_owner_bounds(*bounds))
    });
    let options = MessageBoxOptions {
        kind: MessageBoxKind::Error,
        title: ctx.title,
        message: ctx.message,
        detail: ctx.detail,
        buttons: vec!["OK".to_string()],
        default_id: 0,
        cancel_id: Some(0),
    };
    let on_response: MessageBoxResponseCb = Arc::new(|_result, _window, _cx| {});
    if let Err(error) = open_message_box_window(owner_bounds, options, on_response, cx) {
        eprintln!("[SessionLoad] failure dialog unavailable: {error}");
    }
}

/// What the freshly opened workspace should do once its window exists.
enum WorkspaceInit {
    /// Install a decoded project that was loaded before studio mounted.
    Loaded(LoadedSessionPackage),
    /// Pre-studio handoff plus a lightweight workspace finish hook.
    Prepared {
        package: LoadedSessionPackage,
        follow_up: PreparedWorkspaceFinish,
    },
    /// Restore a rollback snapshot after a failed in-studio replace.
    Rollback(SessionRollbackSnapshot),
}

impl WorkspaceInit {
    fn project_name(&self) -> &str {
        match self {
            Self::Loaded(package) | Self::Prepared { package, .. } => &package.project.name,
            Self::Rollback(snapshot) => &snapshot.session.name,
        }
    }
}

fn open_studio_for_action(action: WelcomeAction, cx: &mut App) {
    match action {
        WelcomeAction::OpenProjectFile(_) | WelcomeAction::OpenRecent(_) => (),
        WelcomeAction::EmptyProject | WelcomeAction::OpenEmptyWorkspace => {
            begin_workspace_session(
                "Preparing Workspace…",
                FutureboardProject::new("Untitled Project"),
                PreparedWorkspaceFinish::EmptyUntitled,
                cx,
            );
        }
        WelcomeAction::MidiComposer => begin_workspace_session(
            "Preparing Workspace…",
            FutureboardProject::new("Untitled Project"),
            PreparedWorkspaceFinish::Template(ProjectTemplate::BeatMaking),
            cx,
        ),
        WelcomeAction::AudioSession => begin_workspace_session(
            "Preparing Workspace…",
            FutureboardProject::new("Untitled Project"),
            PreparedWorkspaceFinish::Template(ProjectTemplate::Recording),
            cx,
        ),
        WelcomeAction::MixTemplate => begin_workspace_session(
            "Preparing Workspace…",
            FutureboardProject::new("Untitled Project"),
            PreparedWorkspaceFinish::Template(ProjectTemplate::Mixing),
            cx,
        ),
        WelcomeAction::OpenProject => begin_workspace_session(
            "Preparing Workspace…",
            FutureboardProject::new("Untitled Project"),
            PreparedWorkspaceFinish::OpenDialog,
            cx,
        ),
        WelcomeAction::CreateProject(options) => begin_workspace_session(
            "Preparing Workspace…",
            FutureboardProject::new(&options.name),
            PreparedWorkspaceFinish::CreateProject(options),
            cx,
        ),
    }
}

fn open_studio_workspace(init: WorkspaceInit, cx: &mut App) -> Result<(), String> {
    if !cx
        .try_global::<AppSessionGate>()
        .map(|g| g.mode.allows_studio_mount())
        .unwrap_or(false)
    {
        eprintln!("[StudioMount] blocked because session not ready");
        boot::log("studio mount blocked — app mode is not Studio");
        return Err("app mode is not Studio".to_string());
    }

    let project_name = init.project_name().to_string();
    eprintln!("[StudioOpen] opening studio window");
    let studio_options = studio_window_options(cx);
    let studio = cx
        .open_window(studio_options, |window, cx| {
            boot::log("build StudioLayout");
            let layout = cx.new(StudioLayout::new);
            boot::log("StudioLayout built");

            // WCO / OS window close → quit the app, but go through the
            // unsaved-changes guard first. Returning `false` always prevents
            // GPUI's default close: `request_quit` drives `cx.quit()` only once
            // the user confirms, so Cancel keeps the app open and the close
            // never routes to Welcome.
            let studio_entity = layout.clone();
            window.on_window_should_close(cx, move |window, cx| {
                sphere_ui_components::window_position::persist_studio_window_from_window(
                    window, cx,
                );
                let bounds = window.bounds();
                studio_entity.update(cx, |studio, cx| {
                    studio.request_close(PendingCloseAction::QuitApp, Some(bounds), cx);
                });
                // Always veto the platform close; confirmed quit runs via `do_quit`.
                false
            });

            layout
        })
        .map_err(|e| e.to_string())?;
    eprintln!("[StudioOpen] studio window opened");

    let studio_handle = studio;
    store_studio_window_handle(cx, studio_handle);

    studio
        .update(cx, move |layout, _window, cx| {
            // Wire the workspace lifecycle: its own window handle (so Close Project
            // can close it) and the hook that re-opens Welcome.
            layout.set_self_window(studio_handle);
            layout.set_request_welcome_callback(Arc::new(open_welcome_window));
            layout.set_request_session_shutdown_callback(Arc::new({
                move |reason, owner_bounds, studio, cx| {
                    begin_close_project_session(reason, owner_bounds, studio, cx);
                }
            }));
            layout.set_request_project_load_callback(Arc::new({
                let studio_handle = studio_handle;
                move |path, open_options, cx| {
                    request_project_switch_in_studio(studio_handle, path, open_options, cx);
                }
            }));

            match init {
                WorkspaceInit::Loaded(package) => layout.install_loaded_session(package, cx),
                WorkspaceInit::Prepared { package, follow_up } => {
                    layout.install_prepared_workspace(package, follow_up, cx);
                }
                WorkspaceInit::Rollback(snapshot) => {
                    layout.restore_session_rollback_snapshot(snapshot, cx)
                }
            }
        })
        .map_err(|e| e.to_string())?;

    set_discord_presence(sphere_discord_rpc::Presence::editing(project_name));

    // The studio window is created hidden (see `platform_chrome::studio_window_options`,
    // `show: false`) so the OS never displays its empty black client area while the
    // heavy first layout / workspace install runs. Reveal it after the first frame
    // has painted — `activate_window` applies the stored initial placement and shows
    // the window with real content instead of a black flash.
    let _ = studio.update(cx, |_layout, window, _cx| {
        window.on_next_frame(|window, _cx| {
            boot::complete_startup();
            window.activate_window();
        });
    });

    boot::log("workspace entered");
    Ok(())
}

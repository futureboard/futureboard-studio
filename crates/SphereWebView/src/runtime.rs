//! Feature-gated native-window CEF runtime.

use std::cell::Cell;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::{self, ThreadId};

use cef::rc::Rc as _;
use cef::{ImplBrowser, ImplBrowserHost, ImplFrame, LogSeverity};
use thiserror::Error;

pub use cef;

/// Opaque ARGB background for windowless browsers (`#111318`, the panel
/// surface the editor chrome uses) so an unpainted frame is never transparent.
const OPAQUE_BACKGROUND: u32 = 0xFF11_1318;

/// Windowless paint rate cap. CEF clamps this to 1..=60.
const WINDOWLESS_FRAME_RATE: i32 = 60;

/// Largest browser dimension handed to CEF. Larger backing stores fail macOS
/// `vm_map` once several editors are live.
const MAX_BROWSER_DIMENSION: i32 = 8192;

thread_local! {
    static CEF_UI_THREAD: Cell<bool> = const { Cell::new(false) };
}

/// Record the calling thread as the CEF UI thread. `CefRuntime::initialize`
/// is the only caller.
pub fn mark_cef_ui_thread() {
    CEF_UI_THREAD.with(|flag| flag.set(true));
}

/// Whether the calling thread initialized CEF.
pub fn on_cef_ui_thread() -> bool {
    CEF_UI_THREAD.with(Cell::get)
}

/// Run `operation` only when the caller is already the CEF UI thread.
///
/// This does not hop threads. A `dispatch_sync` onto the UI thread from a
/// CEF callback deadlocks the browser process.
pub fn run_on_cef_ui(operation_name: &str, operation: impl FnOnce()) {
    if on_cef_ui_thread() {
        operation();
        return;
    }
    eprintln!(
        "[cef-thread] rejected operation={operation_name} reason=not-cef-ui-thread {}",
        thread_label()
    );
}

/// Run `operation` only on the platform main thread.
///
/// On macOS that is `pthread_main_np`, which is also where AppKit and the
/// integrated CEF run loop live. This does not hop threads.
pub fn run_on_main_thread(operation_name: &str, operation: impl FnOnce()) {
    if platform_main_thread() {
        operation();
        return;
    }
    eprintln!(
        "[cef-thread] rejected operation={operation_name} reason=not-main-thread {}",
        thread_label()
    );
}

fn platform_main_thread() -> bool {
    #[cfg(target_os = "macos")]
    {
        unsafe { pthread_main_np() == 1 }
    }
    #[cfg(not(target_os = "macos"))]
    {
        // Windows and Linux drive CEF from the thread that called
        // `CefInitialize`. That thread is the one AppKit would call "main"
        // on macOS; there is no separate main-thread check.
        on_cef_ui_thread()
    }
}

/// Human-readable thread identity for crash logs.
pub fn thread_label() -> String {
    let cef_ui = on_cef_ui_thread();
    #[cfg(target_os = "macos")]
    let main = unsafe { pthread_main_np() == 1 };
    #[cfg(not(target_os = "macos"))]
    let main = false;
    format!("{:?} main={main} cef_ui={cef_ui}", thread::current().id())
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn pthread_main_np() -> i32;
}

/// Persistent Chromium log. Fatal CHECK lines land here even when stderr is
/// not attached to a terminal.
pub fn cef_diagnostic_log_path() -> String {
    #[cfg(target_os = "windows")]
    {
        std::env::temp_dir()
            .join("futureboard-cef.log")
            .display()
            .to_string()
    }
    #[cfg(not(target_os = "windows"))]
    {
        "/tmp/futureboard-cef.log".to_string()
    }
}

fn log_browser(browser_id: i32, event: &str) {
    if !crate::scheme::cef_diagnostics_enabled() {
        return;
    }
    eprintln!("[CEF][Browser {browser_id}] {event} {}", thread_label());
}

fn sanitize_bounds(bounds: WindowBounds) -> WindowBounds {
    let width = bounds.width.clamp(1, MAX_BROWSER_DIMENSION);
    let height = bounds.height.clamp(1, MAX_BROWSER_DIMENSION);
    if width != bounds.width || height != bounds.height {
        eprintln!(
            "[cef-lifecycle] event=clamp_bounds requested={}x{} applied={width}x{height}",
            bounds.width, bounds.height
        );
    }
    WindowBounds {
        x: bounds.x,
        y: bounds.y,
        width,
        height,
    }
}

/// Futureboard uses CEF only as an ephemeral renderer for local built-in UI.
/// Application persistence and every secret remain native-owned.
pub const CEF_EPHEMERAL_LOCAL_UI: bool = true;
const CEF_PERSIST_SESSION_COOKIES: i32 = 0;
const CEF_EXCLUDE_DEFAULT_COOKIE_SCHEMES: i32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessDispatch {
    BrowserProcess,
    SubprocessExit(i32),
}

impl ProcessDispatch {
    fn from_exit_code(exit_code: i32) -> Self {
        if exit_code < 0 {
            Self::BrowserProcess
        } else {
            Self::SubprocessExit(exit_code)
        }
    }
}

#[derive(Debug)]
struct ProcessIdentity {
    executable: PathBuf,
    process_type: Option<String>,
    utility_sub_type: Option<String>,
    argument_count: usize,
}

impl ProcessIdentity {
    fn current() -> Result<Self, CefRuntimeError> {
        let args: Vec<String> = std::env::args_os()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        Ok(Self {
            executable: std::env::current_exe()?,
            process_type: command_line_switch(&args, "--type").map(str::to_owned),
            utility_sub_type: command_line_switch(&args, "--utility-sub-type").map(str::to_owned),
            argument_count: args.len(),
        })
    }
}

/// Load platform runtime state and bind the CEF API version for this process.
///
/// macOS does not link the Chromium framework into the executable. The
/// framework must be loaded from the application bundle before even the first
/// generated CEF wrapper function is called; otherwise that wrapper dispatches
/// through an unresolved null function pointer. The process-wide loader is
/// intentionally retained for the lifetime of the process.
pub fn prepare_process() -> Result<(), CefRuntimeError> {
    #[cfg(target_os = "macos")]
    load_macos_framework()?;
    ensure_api_version();
    Ok(())
}

/// Bind the CEF API version after the platform runtime has been loaded.
///
/// cef-rs stamps a version into every wrapper object it creates. Until this has
/// run that version is `-1`, and the first C→C++ call aborts the process with
/// `CefApp_0_CToCpp called with invalid version -1`. It therefore has to happen
/// before **any** CEF object exists — including the [`cef::App`] that
/// [`execute_subprocess`] and [`CefRuntime::initialize`] are handed, which is
/// constructed by the caller long before either runs.
///
/// Idempotent. Call [`prepare_process`] at public process entry points so the
/// macOS framework has been loaded before this function invokes CEF.
pub fn ensure_api_version() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let hash = cef::api_hash(cef::sys::CEF_API_VERSION_LAST, 0);
        eprintln!(
            "[cef-process] api_version={} api_version_last={} api_hash_available={}",
            cef::api_version(),
            cef::sys::CEF_API_VERSION_LAST,
            !hash.is_null()
        );
        log::info!(
            "process API version={} api_version_last={} api_hash_available={}",
            cef::api_version(),
            cef::sys::CEF_API_VERSION_LAST,
            !hash.is_null()
        );
    });
}

/// Log process identity before any Futureboard subsystem is initialized.
///
/// CEF appends `--type` and, for the Network Service, `--utility-sub-type` to
/// the same executable. Keeping this at the first statement of `main` makes it
/// unambiguous whether a helper escaped into normal application startup.
pub fn log_process_entry() {
    match ProcessIdentity::current() {
        Ok(identity) => {
            eprintln!(
                "[cef-process] entry pid={} executable={} type={:?} utility_sub_type={:?} argument_count={}",
                std::process::id(),
                identity.executable.display(),
                identity.process_type.as_deref().unwrap_or("<browser>"),
                identity.utility_sub_type.as_deref().unwrap_or("<none>"),
                identity.argument_count,
            );
            log::info!(
                "process entry pid={} executable={} type={:?} utility_sub_type={:?} argument_count={}",
                std::process::id(),
                identity.executable.display(),
                identity.process_type.as_deref().unwrap_or("<browser>"),
                identity.utility_sub_type.as_deref().unwrap_or("<none>"),
                identity.argument_count,
            );
        }
        Err(error) => {
            eprintln!(
                "[cef-process] entry pid={} executable=<unresolved> error={error}",
                std::process::id()
            );
            log::error!(
                "process entry pid={} executable unresolved: {error}",
                std::process::id()
            );
        }
    }
}

/// Whether this invocation has the CEF `--type` switch used by renderer, GPU,
/// utility, and other helper processes.
pub fn is_subprocess_command_line() -> bool {
    let args: Vec<String> = std::env::args_os()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    command_line_switch(&args, "--type").is_some()
}

fn command_line_switch<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().enumerate().find_map(|(index, arg)| {
        arg.strip_prefix(&format!("{name}=")).or_else(|| {
            (arg == name)
                .then(|| args.get(index + 1).map(String::as_str))
                .flatten()
        })
    })
}

/// Dispatch CEF subprocess command lines before starting the native UI.
pub fn execute_subprocess(
    application: Option<&mut cef::App>,
) -> Result<ProcessDispatch, CefRuntimeError> {
    prepare_process()?;
    let args = cef::args::Args::new();
    let exit_code =
        cef::execute_process(Some(args.as_main_args()), application, std::ptr::null_mut());
    eprintln!(
        "[cef-process] cef_execute_process_return={} pid={} thread={:?}",
        exit_code,
        std::process::id(),
        std::thread::current().id()
    );
    Ok(ProcessDispatch::from_exit_code(exit_code))
}

/// Executable CEF should launch for renderer, GPU, utility, and other helper
/// processes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum BrowserSubprocess {
    /// CEF re-launches the browser executable. This is represented by an empty
    /// `browser_subprocess_path`, per CEF's API contract, and requires the
    /// executable to call [`execute_subprocess`] before normal startup.
    #[default]
    CurrentExecutable,
    /// Use a separately packaged helper executable.
    SeparateExecutable(PathBuf),
}

pub const MACOS_HELPER_EXECUTABLE_NAME: &str = "Futureboard Studio Helper";
pub const MACOS_CEF_FRAMEWORK_NAME: &str = "Chromium Embedded Framework.framework";

/// Resolve and validate the CEF helper shipped beside a macOS application.
///
/// `executable` must have the standard
/// `<app>.app/Contents/MacOS/<executable>` shape. No current-working-directory
/// fallback is permitted because Finder launches do not inherit a useful cwd.
#[cfg(target_os = "macos")]
pub fn macos_browser_subprocess(executable: &Path) -> Result<BrowserSubprocess, CefRuntimeError> {
    let contents = executable
        .parent()
        .and_then(Path::parent)
        .filter(|path| path.file_name().is_some_and(|name| name == "Contents"))
        .ok_or_else(|| CefRuntimeError::MacBundleLayout(executable.to_path_buf()))?;
    let frameworks = contents.join("Frameworks");
    let framework = frameworks.join(MACOS_CEF_FRAMEWORK_NAME);
    if !framework.join("Chromium Embedded Framework").is_file() {
        return Err(CefRuntimeError::MacFrameworkMissing(framework));
    }
    let helper = frameworks
        .join(format!("{MACOS_HELPER_EXECUTABLE_NAME}.app"))
        .join("Contents")
        .join("MacOS")
        .join(MACOS_HELPER_EXECUTABLE_NAME);
    if !helper.is_file() {
        return Err(CefRuntimeError::MacHelperMissing(helper));
    }
    Ok(BrowserSubprocess::SeparateExecutable(helper))
}

#[cfg(not(target_os = "macos"))]
pub fn platform_browser_subprocess() -> Result<BrowserSubprocess, CefRuntimeError> {
    Ok(BrowserSubprocess::CurrentExecutable)
}

#[cfg(target_os = "macos")]
pub fn platform_browser_subprocess() -> Result<BrowserSubprocess, CefRuntimeError> {
    let executable = std::env::current_exe()?;
    macos_browser_subprocess(&executable)
}

/// Browser-process configuration. The runtime uses a portable, integrated
/// message pump and must be driven from the creating UI thread.
#[derive(Debug, Clone, Default)]
pub struct CefRuntimeConfig {
    pub locale: Option<String>,
    pub user_agent: Option<String>,
    pub remote_debugging_port: Option<u16>,
    /// Helper-process ownership is a packaging decision. Futureboard uses the
    /// integrated current-executable model; a separate helper must only be
    /// selected by packaging that actually supplies one.
    pub browser_subprocess: BrowserSubprocess,
    /// Allow [`RenderMode::Windowless`] browsers in this process. Chromium
    /// decides this once, at `cef_initialize`, so a runtime that may ever need
    /// off-screen rendering must opt in before any browser is created.
    pub windowless_rendering: bool,
    /// Let the embedding application schedule `cef_do_message_loop_work`
    /// through `BrowserProcessHandler::on_schedule_message_pump_work`.
    ///
    /// Callers must install that callback on the `CefApp` passed to
    /// [`CefRuntime::initialize`]. This remains disabled by default so existing
    /// Windows and Linux integrations are unchanged.
    pub external_message_pump: bool,
}

/// Pixel bounds in the native parent's client coordinate space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowBounds {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl WindowBounds {
    pub fn new(x: i32, y: i32, width: i32, height: i32) -> Result<Self, CefRuntimeError> {
        if width <= 0 || height <= 0 {
            return Err(CefRuntimeError::InvalidBounds { width, height });
        }
        Ok(Self {
            x,
            y,
            width,
            height,
        })
    }

    fn as_cef_rect(self) -> cef::Rect {
        cef::Rect {
            x: self.x,
            y: self.y,
            width: self.width,
            height: self.height,
        }
    }
}

/// A native parent HWND (Windows), X11 Window (Linux), or NSView pointer
/// (macOS).
#[derive(Clone, Copy)]
pub struct NativeParent(cef::sys::cef_window_handle_t);

impl NativeParent {
    /// # Safety
    ///
    /// The handle must stay valid for all child [`WebView`] instances and must
    /// belong to the thread that owns [`CefRuntime`].
    pub unsafe fn from_raw(handle: cef::sys::cef_window_handle_t) -> Self {
        Self(handle)
    }

    pub fn as_raw(self) -> cef::sys::cef_window_handle_t {
        self.0
    }
}

/// How a browser is presented: a real native child window, or an off-screen
/// framebuffer the embedder draws itself.
#[derive(Clone)]
pub enum RenderMode {
    /// A native CEF child window inside `parent`. No render handler, shared
    /// texture, or off-screen rendering path is used.
    Windowed,
    /// Windowless rendering into `surface`. `parent` is still passed to CEF —
    /// it is only used to resolve monitor info and to parent dialogs — and may
    /// be null on platforms where no such handle exists.
    Windowless { surface: crate::osr::OsrSurface },
}

impl std::fmt::Debug for RenderMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Windowed => write!(f, "Windowed"),
            Self::Windowless { .. } => write!(f, "Windowless"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct WebViewConfig {
    pub url: String,
    pub bounds: WindowBounds,
    pub render_mode: RenderMode,
}

impl WebViewConfig {
    pub fn new(url: impl Into<String>, bounds: WindowBounds) -> Self {
        Self {
            url: url.into(),
            bounds,
            render_mode: RenderMode::Windowed,
        }
    }

    /// Render off-screen into `surface` instead of a native child window.
    pub fn windowless(mut self, surface: crate::osr::OsrSurface) -> Self {
        self.render_mode = RenderMode::Windowless { surface };
        self
    }
}

/// Process-wide CEF state. This is intentionally `!Send` and `!Sync`.
pub struct CefRuntime {
    owner_thread: ThreadId,
    shutdown: bool,
    ephemeral_root_cache: Option<tempfile::TempDir>,
    _not_send: PhantomData<Rc<()>>,
}

impl CefRuntime {
    pub fn initialize(
        config: CefRuntimeConfig,
        application: Option<&mut cef::App>,
    ) -> Result<Self, CefRuntimeError> {
        prepare_process()?;
        mark_cef_ui_thread();
        let identity = ProcessIdentity::current()?;
        let args = cef::args::Args::new();
        let browser_subprocess_path = match &config.browser_subprocess {
            BrowserSubprocess::CurrentExecutable => cef::CefString::default(),
            BrowserSubprocess::SeparateExecutable(path) => {
                cef::CefString::from(path.to_string_lossy().as_ref())
            }
        };
        let resolved_subprocess = match &config.browser_subprocess {
            BrowserSubprocess::CurrentExecutable => identity.executable.as_path(),
            BrowserSubprocess::SeparateExecutable(path) => path.as_path(),
        };
        let ephemeral_root_cache = create_ephemeral_root_cache()?;
        eprintln!(
            "[cef-runtime] initialize begin executable={} process_type={:?} subprocess_model={} subprocess_path={} cache_path=<memory> root_cache_path=<temporary> resources_path=<cef-default> locales_path=<cef-default> message_loop=manual-do-work windowless={} remote_debugging_port={} thread={:?}",
            identity.executable.display(),
            identity.process_type.as_deref().unwrap_or("<browser>"),
            match &config.browser_subprocess {
                BrowserSubprocess::CurrentExecutable => "integrated",
                BrowserSubprocess::SeparateExecutable(_) => "separate",
            },
            resolved_subprocess.display(),
            config.windowless_rendering,
            config.remote_debugging_port.unwrap_or(0),
            thread::current().id(),
        );
        log::info!(
            "initialize begin executable={} process_type={:?} subprocess_model={} windowless={} remote_debugging_port={} thread={:?}",
            identity.executable.display(),
            identity.process_type.as_deref().unwrap_or("<browser>"),
            match &config.browser_subprocess {
                BrowserSubprocess::CurrentExecutable => "integrated",
                BrowserSubprocess::SeparateExecutable(_) => "separate",
            },
            config.windowless_rendering,
            config.remote_debugging_port.unwrap_or(0),
            thread::current().id(),
        );
        let settings = build_settings(
            &config,
            browser_subprocess_path,
            ephemeral_root_cache.path(),
        );
        if let Err(error) = validate_ephemeral_settings(&settings) {
            return Err(error);
        }
        if cef::initialize(
            Some(args.as_main_args()),
            Some(&settings),
            application,
            std::ptr::null_mut(),
        ) != 1
        {
            eprintln!("[cef-runtime] initialize result=false");
            log::error!("initialize failed: cef_initialize returned false");
            return Err(CefRuntimeError::InitializeFailed);
        }
        log_local_ui_runtime_summary();
        eprintln!(
            "[cef-runtime] log_file={} log_severity={}",
            cef_diagnostic_log_path(),
            if crate::scheme::cef_diagnostics_enabled() {
                "verbose"
            } else {
                "default"
            }
        );
        eprintln!(
            "[cef-runtime] initialize result=true owner_thread={:?}",
            thread::current().id()
        );
        log::info!(
            "initialize result=true owner_thread={:?}",
            thread::current().id()
        );

        Ok(Self {
            owner_thread: thread::current().id(),
            shutdown: false,
            ephemeral_root_cache: Some(ephemeral_root_cache),
            _not_send: PhantomData,
        })
    }

    /// Advance CEF from the native application's UI loop.
    pub fn do_message_loop_work(&self) -> Result<(), CefRuntimeError> {
        self.ensure_thread()?;
        cef::do_message_loop_work();
        Ok(())
    }

    /// Create a browser: either a real native CEF child window, or — when the
    /// config selects [`RenderMode::Windowless`] — an off-screen browser that
    /// paints into the supplied [`crate::osr::OsrSurface`].
    ///
    /// `client` should normally be `Some` — see [`crate::client`]. `None` is
    /// still accepted (e.g. a test double) but gives the browser zero
    /// navigation policy and no crash signal. A windowless browser additionally
    /// needs the client to expose an OSR render handler, otherwise CEF has
    /// nowhere to paint.
    pub fn create_webview<'runtime>(
        &'runtime self,
        parent: NativeParent,
        config: WebViewConfig,
        client: Option<&mut cef::Client>,
    ) -> Result<WebView<'runtime>, CefRuntimeError> {
        self.ensure_thread()?;
        if config.url.trim().is_empty() {
            return Err(CefRuntimeError::EmptyUrl);
        }
        let bounds = sanitize_bounds(config.bounds);
        let windowless = matches!(config.render_mode, RenderMode::Windowless { .. });
        let accelerated = matches!(
            &config.render_mode,
            RenderMode::Windowless { surface } if surface.is_accelerated()
        );
        let mut window_info = if windowless {
            cef::WindowInfo::default().set_as_windowless(parent.as_raw())
        } else {
            cef::WindowInfo::default().set_as_child(parent.as_raw(), &bounds.as_cef_rect())
        };
        #[cfg(target_os = "windows")]
        {
            // CEF only calls OnAcceleratedPaint when this is set. The shared
            // handle is copied synchronously into host-owned GPU memory before
            // the callback returns; external begin frames remain disabled so
            // CEF owns frame pacing at the configured 60 Hz cap.
            window_info.shared_texture_enabled = i32::from(accelerated);
        }
        #[cfg(not(target_os = "windows"))]
        let _ = accelerated;
        debug_assert_eq!(
            window_info.windowless_rendering_enabled,
            i32::from(windowless)
        );
        if crate::scheme::cef_diagnostics_enabled() {
            eprintln!(
                "[cef-lifecycle] event=CreateBrowserSync begin url={:?} parent={:?} bounds={:?} render_mode={:?} accelerated={} thread={:?}",
                config.url,
                parent.as_raw(),
                config.bounds,
                config.render_mode,
                accelerated,
                std::thread::current().id()
            );
            log::info!(
                "CreateBrowserSync begin url={:?} parent={:?} bounds={:?} render_mode={:?} accelerated={} thread={:?}",
                config.url,
                parent.as_raw(),
                config.bounds,
                config.render_mode,
                accelerated,
                std::thread::current().id()
            );
        }
        let browser_settings = cef::BrowserSettings {
            // A transparent windowless surface would composite the timeline
            // through the editor; an opaque background also lets the host
            // upload frames without premultiplied-alpha handling. A windowed
            // child gets the same panel colour so its HWND never flashes
            // Chromium's default white before the page's first paint.
            background_color: OPAQUE_BACKGROUND,
            windowless_frame_rate: if windowless { WINDOWLESS_FRAME_RATE } else { 0 },
            ..Default::default()
        };
        let browser = cef::browser_host_create_browser_sync(
            Some(&window_info),
            client,
            Some(&cef::CefString::from(config.url.as_str())),
            Some(&browser_settings),
            None,
            // All built-in plugin browsers deliberately share the global,
            // ephemeral request context configured during initialization.
            None,
        );
        let Some(browser) = browser else {
            eprintln!(
                "[cef-lifecycle] event=CreateBrowserSync result=false url={:?} thread={:?}",
                config.url,
                std::thread::current().id()
            );
            log::error!(
                "CreateBrowserSync failed url={:?} render_mode={:?} accelerated={accelerated}",
                config.url,
                config.render_mode
            );
            return Err(CefRuntimeError::CreateBrowserFailed);
        };
        if crate::scheme::cef_diagnostics_enabled() {
            eprintln!(
                "[cef-lifecycle] event=CreateBrowserSync result=true browser_id={} url={:?} thread={:?}",
                browser.identifier(),
                config.url,
                std::thread::current().id()
            );
            log::info!(
                "CreateBrowserSync result=true browser_id={} url={:?} thread={:?}",
                browser.identifier(),
                config.url,
                std::thread::current().id()
            );
        }

        let browser_id = browser.identifier();
        log_browser(browser_id, &format!("created url={:?}", config.url));
        Ok(WebView {
            browser,
            render_mode: config.render_mode,
            owner_thread: self.owner_thread,
            resize_generation: AtomicU64::new(0),
            applied_windowless: Cell::new((0, 0, 0)),
            _runtime: PhantomData,
            _not_send: PhantomData,
        })
    }

    /// Create a web view whose lifetime is not tied to this borrow.
    ///
    /// [`Self::create_webview`] returns a `WebView<'runtime>`, which cannot be
    /// stored in the same struct as the runtime it borrows. A host that owns
    /// both (one CEF runtime plus a map of open editor views) needs this.
    ///
    /// # Safety
    ///
    /// The returned view must be dropped, or [`WebView::close`]d, **before**
    /// the [`CefRuntime`] it came from. Storing both in one struct satisfies
    /// this by declaring the view field before the runtime field, since Rust
    /// drops fields in declaration order.
    pub unsafe fn create_webview_detached(
        &self,
        parent: NativeParent,
        config: WebViewConfig,
        client: Option<&mut cef::Client>,
    ) -> Result<WebView<'static>, CefRuntimeError> {
        let view = self.create_webview(parent, config, client)?;
        // Only the PhantomData borrow marker changes; the browser handle and
        // its thread affinity are carried over unchanged.
        Ok(WebView {
            browser: view.browser.clone(),
            render_mode: view.render_mode.clone(),
            owner_thread: view.owner_thread,
            resize_generation: AtomicU64::new(view.resize_generation.load(Ordering::Relaxed)),
            applied_windowless: Cell::new(view.applied_windowless.get()),
            _runtime: PhantomData,
            _not_send: PhantomData,
        })
    }

    /// Skip `CefShutdown` when browsers are still alive.
    ///
    /// Chromium CHECK-fails (SIGTRAP) if shutdown races `OnBeforeClose`.
    /// The process is exiting; leaking the runtime is safer than that trap.
    pub fn suppress_shutdown(&mut self) {
        if !self.shutdown {
            eprintln!(
                "[cef-runtime] shutdown suppressed reason=live-browsers {}",
                thread_label()
            );
            self.shutdown = true;
        }
    }

    pub fn shutdown(mut self) -> Result<(), CefRuntimeError> {
        self.ensure_thread()?;
        if !self.shutdown {
            eprintln!(
                "[cef-runtime] shutdown begin owner_thread={:?}",
                self.owner_thread
            );
            cef::shutdown();
            self.shutdown = true;
            self.cleanup_ephemeral_root();
            eprintln!("[cef-runtime] shutdown complete");
        }
        Ok(())
    }

    fn cleanup_ephemeral_root(&mut self) {
        let Some(directory) = self.ephemeral_root_cache.take() else {
            return;
        };
        if let Err(error) = directory.close() {
            if error.kind() != std::io::ErrorKind::NotFound {
                eprintln!("[cef-runtime] temporary storage cleanup failed: {error}");
            }
        }
    }

    fn ensure_thread(&self) -> Result<(), CefRuntimeError> {
        if thread::current().id() != self.owner_thread {
            return Err(CefRuntimeError::WrongThread);
        }
        Ok(())
    }
}

fn build_settings(
    config: &CefRuntimeConfig,
    browser_subprocess_path: cef::CefString,
    ephemeral_root_cache_path: &Path,
) -> cef::Settings {
    cef::Settings {
        no_sandbox: 1,
        multi_threaded_message_loop: 0,
        external_message_pump: i32::from(config.external_message_pump),
        windowless_rendering_enabled: i32::from(config.windowless_rendering),
        // Empty cache_path is CEF's incognito mode: no profile-specific
        // cookies, preferences, localStorage, IndexedDB, or HTTP cache is
        // persisted. A non-persistent root is still required because an empty
        // root_cache_path can fall back to CEF's generic user-data directory.
        cache_path: cef::CefString::default(),
        root_cache_path: cef::CefString::from(ephemeral_root_cache_path.to_string_lossy().as_ref()),
        persist_session_cookies: CEF_PERSIST_SESSION_COOKIES,
        // Built-in pages do not use cookies. Empty list + exclude defaults
        // disables http/https/ws/wss cookies and does not make
        // mikoplugin:// cookieable.
        cookieable_schemes_list: cef::CefString::default(),
        cookieable_schemes_exclude_defaults: CEF_EXCLUDE_DEFAULT_COOKIE_SCHEMES,
        browser_subprocess_path,
        locale: cef_string(config.locale.as_deref()),
        user_agent: cef_string(config.user_agent.as_deref()),
        log_file: cef::CefString::from(cef_diagnostic_log_path().as_str()),
        log_severity: if crate::scheme::cef_diagnostics_enabled() {
            LogSeverity::VERBOSE
        } else {
            LogSeverity::DEFAULT
        },
        remote_debugging_port: config.remote_debugging_port.unwrap_or(0) as i32,
        ..Default::default()
    }
}

/// Settings for any future isolated request context. Production currently uses
/// only the global context, but keeping this constructor beside the global
/// policy prevents a later browser path from accidentally gaining persistence.
pub fn ephemeral_request_context_settings() -> cef::RequestContextSettings {
    cef::RequestContextSettings {
        cache_path: cef::CefString::default(),
        persist_session_cookies: CEF_PERSIST_SESSION_COOKIES,
        cookieable_schemes_list: cef::CefString::default(),
        cookieable_schemes_exclude_defaults: CEF_EXCLUDE_DEFAULT_COOKIE_SCHEMES,
        ..Default::default()
    }
}

fn validate_ephemeral_settings(settings: &cef::Settings) -> Result<(), CefRuntimeError> {
    let cache_path = settings.cache_path.to_string();
    let root_cache_path = settings.root_cache_path.to_string();
    let cookieable_schemes = settings.cookieable_schemes_list.to_string();
    validate_ephemeral_values(
        &cache_path,
        &root_cache_path,
        settings.persist_session_cookies,
        &cookieable_schemes,
        settings.cookieable_schemes_exclude_defaults,
    )
}

fn validate_ephemeral_values(
    cache_path: &str,
    root_cache_path: &str,
    persist_session_cookies: i32,
    cookieable_schemes: &str,
    exclude_default_cookie_schemes: i32,
) -> Result<(), CefRuntimeError> {
    if !CEF_EPHEMERAL_LOCAL_UI
        || !cache_path.is_empty()
        || root_cache_path.is_empty()
        || persist_session_cookies != CEF_PERSIST_SESSION_COOKIES
        || !cookieable_schemes.is_empty()
        || exclude_default_cookie_schemes != CEF_EXCLUDE_DEFAULT_COOKIE_SCHEMES
    {
        return Err(CefRuntimeError::PersistentStorageForbidden);
    }
    Ok(())
}

fn create_ephemeral_root_cache() -> Result<tempfile::TempDir, CefRuntimeError> {
    tempfile::Builder::new()
        .prefix("futureboard-cef-ephemeral-")
        .tempdir()
        .map_err(CefRuntimeError::EphemeralStorage)
}

fn log_local_ui_runtime_summary() {
    eprintln!("CEF local UI runtime: enabled");
    eprintln!("CEF storage mode: ephemeral");
    eprintln!("CEF persistent session cookies: disabled");
    eprintln!("CEF persistent preferences: disabled");
    #[cfg(target_os = "macos")]
    eprintln!("CEF mock Keychain: enabled");
    eprintln!("CEF external navigation: restricted");
}

impl Drop for CefRuntime {
    fn drop(&mut self) {
        if !self.shutdown && thread::current().id() == self.owner_thread {
            eprintln!(
                "[cef-runtime] shutdown begin source=drop owner_thread={:?}",
                self.owner_thread
            );
            cef::shutdown();
            self.shutdown = true;
            self.cleanup_ephemeral_root();
            eprintln!("[cef-runtime] shutdown complete source=drop");
        } else if !self.shutdown {
            eprintln!(
                "[cef-runtime] shutdown skipped reason=wrong-thread owner_thread={:?} current_thread={:?}",
                self.owner_thread,
                thread::current().id()
            );
        }
    }
}

pub struct WebView<'runtime> {
    browser: cef::Browser,
    render_mode: RenderMode,
    owner_thread: ThreadId,
    resize_generation: AtomicU64,
    /// Last logical size and scale bits passed to `was_resized`.
    ///
    /// The off-screen surface is often updated by the caller before
    /// `set_bounds`, so the surface's current size cannot tell us whether CEF
    /// has already been notified.
    applied_windowless: Cell<(i32, i32, u32)>,
    _runtime: PhantomData<&'runtime CefRuntime>,
    _not_send: PhantomData<Rc<()>>,
}

impl Drop for WebView<'_> {
    fn drop(&mut self) {
        let browser_id = self.browser.identifier();
        let last_host_ref = self.browser.has_one_ref();
        if crate::scheme::cef_diagnostics_enabled() {
            eprintln!(
                "[cef-ref] object_type=cef_browser_t browser_id={browser_id} event=webview_release has_one_ref={last_host_ref} has_at_least_one_ref={} thread={:?}",
                self.browser.has_at_least_one_ref(),
                std::thread::current().id()
            );
        }
        if last_host_ref {
            log_browser(browser_id, "released");
        }
    }
}

impl WebView<'_> {
    pub fn browser_identifier(&self) -> i32 {
        self.browser.identifier()
    }

    pub fn load_url(&self, url: &str) -> Result<(), CefRuntimeError> {
        self.ensure_thread()?;
        if url.trim().is_empty() {
            return Err(CefRuntimeError::EmptyUrl);
        }
        let frame = self
            .browser
            .main_frame()
            .ok_or(CefRuntimeError::MissingMainFrame)?;
        log_browser(self.browser.identifier(), &format!("navigate {url}"));
        frame.load_url(Some(&cef::CefString::from(url)));
        Ok(())
    }

    /// Run `code` in the document's main frame. Fire-and-forget — CEF gives no
    /// synchronous return value for this call. Used to push bridge protocol
    /// messages (`futureboard.selectInstance`, ...) into the already-loaded
    /// React app without navigating or reloading the page.
    pub fn execute_javascript(&self, code: &str) -> Result<(), CefRuntimeError> {
        self.ensure_thread()?;
        let frame = self
            .browser
            .main_frame()
            .ok_or(CefRuntimeError::MissingMainFrame)?;
        frame.execute_java_script(Some(&cef::CefString::from(code)), None, 0);
        Ok(())
    }

    /// Resize the view. For a windowed browser this moves the native child
    /// window; for a windowless one it republishes the logical view size to the
    /// off-screen surface and asks CEF to re-read it, which produces a fresh
    /// `OnPaint` at the new size.
    pub fn set_bounds(&self, bounds: WindowBounds) -> Result<(), CefRuntimeError> {
        self.ensure_thread()?;
        let bounds = sanitize_bounds(bounds);
        let host = self
            .browser
            .host()
            .ok_or(CefRuntimeError::MissingBrowserHost)?;
        match &self.render_mode {
            RenderMode::Windowed => {
                if platform_set_bounds(host.window_handle(), bounds)? {
                    let generation = self.resize_generation.fetch_add(1, Ordering::Release) + 1;
                    log_browser(
                        self.browser.identifier(),
                        &format!(
                            "resize {}x{} generation={generation}",
                            bounds.width, bounds.height
                        ),
                    );
                    host.notify_move_or_resize_started();
                }
            }
            RenderMode::Windowless { surface } => {
                let scale = surface.scale_factor();
                let next = (bounds.width, bounds.height, scale.to_bits());
                if self.applied_windowless.get() != next {
                    if surface.view_size() != (bounds.width, bounds.height) {
                        surface.set_view_size(bounds.width, bounds.height, scale);
                    }
                    self.applied_windowless.set(next);
                    let generation = self.resize_generation.fetch_add(1, Ordering::Release) + 1;
                    log_browser(
                        self.browser.identifier(),
                        &format!(
                            "resize {}x{} generation={generation}",
                            bounds.width, bounds.height
                        ),
                    );
                    host.was_resized();
                }
            }
        }
        Ok(())
    }

    /// Ask a windowed compositor to drop a stale IOSurface after sleep or a
    /// long main-thread stall. Does not change the view size.
    pub fn refresh_compositor(&self) -> Result<(), CefRuntimeError> {
        self.ensure_thread()?;
        if matches!(self.render_mode, RenderMode::Windowless { .. }) {
            return Ok(());
        }
        log_browser(self.browser.identifier(), "compositor-refresh");
        self.browser
            .host()
            .ok_or(CefRuntimeError::MissingBrowserHost)?
            .notify_move_or_resize_started();
        Ok(())
    }

    /// The off-screen surface this view paints into, if it is windowless.
    pub fn osr_surface(&self) -> Option<&crate::osr::OsrSurface> {
        match &self.render_mode {
            RenderMode::Windowed => None,
            RenderMode::Windowless { surface } => Some(surface),
        }
    }

    /// Tell CEF the windowless view rect changed (after updating the surface).
    /// No-op for a windowed browser, which resizes through its own HWND.
    pub fn notify_windowless_resized(&self) -> Result<(), CefRuntimeError> {
        self.ensure_thread()?;
        if matches!(self.render_mode, RenderMode::Windowed) {
            return Ok(());
        }
        self.browser
            .host()
            .ok_or(CefRuntimeError::MissingBrowserHost)?
            .was_resized();
        Ok(())
    }

    /// Tell CEF to re-read `GetScreenInfo` and `GetScreenPoint`.
    ///
    /// Required after a device-scale-factor change or a move to a different
    /// display. [`Self::notify_windowless_resized`] is **not** a substitute:
    /// `WasResized` re-queries the view rect only, so a browser told about a
    /// DPI change through that call alone keeps rendering at the old scale.
    /// Callers must update the surface first — Chromium reads the handler back
    /// synchronously from inside this call.
    pub fn notify_screen_info_changed(&self) -> Result<(), CefRuntimeError> {
        self.ensure_thread()?;
        if matches!(self.render_mode, RenderMode::Windowed) {
            return Ok(());
        }
        self.browser
            .host()
            .ok_or(CefRuntimeError::MissingBrowserHost)?
            .notify_screen_info_changed();
        Ok(())
    }

    /// Replay one input event into a windowless browser. Rejected for a
    /// windowed browser, which receives real platform input directly.
    pub fn send_input(&self, input: crate::osr::OsrInput) -> Result<(), CefRuntimeError> {
        self.ensure_thread()?;
        if matches!(self.render_mode, RenderMode::Windowed) {
            return Err(CefRuntimeError::NotWindowless);
        }
        let host = self
            .browser
            .host()
            .ok_or(CefRuntimeError::MissingBrowserHost)?;
        crate::osr::dispatch_input(&host, input);
        Ok(())
    }

    pub fn native_window_handle(&self) -> Result<cef::sys::cef_window_handle_t, CefRuntimeError> {
        self.ensure_thread()?;
        self.browser
            .host()
            .map(|host| host.window_handle())
            .ok_or(CefRuntimeError::MissingBrowserHost)
    }

    pub fn go_back(&self) -> Result<(), CefRuntimeError> {
        self.ensure_thread()?;
        if self.browser.can_go_back() != 0 {
            self.browser.go_back();
        }
        Ok(())
    }

    pub fn go_forward(&self) -> Result<(), CefRuntimeError> {
        self.ensure_thread()?;
        if self.browser.can_go_forward() != 0 {
            self.browser.go_forward();
        }
        Ok(())
    }

    pub fn reload(&self) -> Result<(), CefRuntimeError> {
        self.ensure_thread()?;
        self.browser.reload();
        Ok(())
    }

    /// Pin the browser's page zoom. Built-in editors use `0.0` (100%).
    pub fn set_zoom_level(&self, zoom_level: f64) -> Result<(), CefRuntimeError> {
        self.ensure_thread()?;
        self.browser
            .host()
            .ok_or(CefRuntimeError::MissingBrowserHost)?
            .set_zoom_level(zoom_level);
        Ok(())
    }

    pub fn close(&self, force: bool) -> Result<(), CefRuntimeError> {
        self.ensure_thread()?;
        let host = self
            .browser
            .host()
            .ok_or(CefRuntimeError::MissingBrowserHost)?;
        log_browser(
            self.browser.identifier(),
            &format!("close requested force={force}"),
        );
        host.close_browser(i32::from(force));
        Ok(())
    }

    fn ensure_thread(&self) -> Result<(), CefRuntimeError> {
        if thread::current().id() != self.owner_thread {
            return Err(CefRuntimeError::WrongThread);
        }
        Ok(())
    }
}

fn cef_string(value: Option<&str>) -> cef::CefString {
    value.map(cef::CefString::from).unwrap_or_default()
}

#[cfg(target_os = "macos")]
fn load_macos_framework() -> Result<(), CefRuntimeError> {
    static LIBRARY: std::sync::OnceLock<Result<cef::library_loader::LibraryLoader, ()>> =
        std::sync::OnceLock::new();

    match LIBRARY.get_or_init(|| {
        let executable = std::env::current_exe().map_err(|_| ())?;
        let helper = executable
            .ancestors()
            .find(|path| path.extension().is_some_and(|extension| extension == "app"))
            .and_then(Path::parent)
            .is_some_and(|parent| parent.file_name().is_some_and(|name| name == "Frameworks"));
        let loader = std::panic::catch_unwind(|| {
            cef::library_loader::LibraryLoader::new(&executable, helper)
        })
        .map_err(|_| ())?;
        loader.load().then_some(loader).ok_or(())
    }) {
        Ok(_) => Ok(()),
        Err(()) => Err(CefRuntimeError::MacFrameworkNotFound),
    }
}

#[cfg(windows)]
fn platform_set_bounds(
    handle: cef::sys::cef_window_handle_t,
    bounds: WindowBounds,
) -> Result<bool, CefRuntimeError> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{SetWindowPos, SWP_NOACTIVATE, SWP_NOZORDER};
    let ok = unsafe {
        SetWindowPos(
            handle.0.cast(),
            std::ptr::null_mut(),
            bounds.x,
            bounds.y,
            bounds.width,
            bounds.height,
            SWP_NOACTIVATE | SWP_NOZORDER,
        )
    };
    if ok == 0 {
        return Err(CefRuntimeError::PlatformResizeFailed(
            std::io::Error::last_os_error().to_string(),
        ));
    }
    Ok(true)
}

#[cfg(target_os = "linux")]
fn platform_set_bounds(
    handle: cef::sys::cef_window_handle_t,
    bounds: WindowBounds,
) -> Result<bool, CefRuntimeError> {
    let xlib = x11_dl::xlib::Xlib::open()
        .map_err(|error| CefRuntimeError::PlatformResizeFailed(error.to_string()))?;
    let display = unsafe { (xlib.XOpenDisplay)(std::ptr::null()) };
    if display.is_null() {
        return Err(CefRuntimeError::PlatformResizeFailed(
            "XOpenDisplay returned null".to_owned(),
        ));
    }
    unsafe {
        (xlib.XMoveResizeWindow)(
            display,
            handle,
            bounds.x,
            bounds.y,
            bounds.width as u32,
            bounds.height as u32,
        );
        (xlib.XFlush)(display);
        (xlib.XCloseDisplay)(display);
    }
    Ok(true)
}

#[cfg(target_os = "macos")]
fn platform_set_bounds(
    handle: cef::sys::cef_window_handle_t,
    bounds: WindowBounds,
) -> Result<bool, CefRuntimeError> {
    use objc2::{msg_send, runtime::AnyObject};
    use objc2_foundation::{NSPoint, NSRect, NSSize};

    if handle.is_null() {
        return Err(CefRuntimeError::PlatformResizeFailed(
            "CEF returned a null NSView".to_owned(),
        ));
    }
    let view = unsafe { &*handle.cast::<AnyObject>() };
    let frame = NSRect {
        origin: NSPoint {
            x: bounds.x as f64,
            y: bounds.y as f64,
        },
        size: NSSize {
            width: bounds.width as f64,
            height: bounds.height as f64,
        },
    };
    let current: NSRect = unsafe { msg_send![view, frame] };
    if rects_match(current, frame) {
        return Ok(false);
    }
    unsafe {
        let _: () = msg_send![view, setFrame: frame];
    }
    Ok(true)
}

#[cfg(target_os = "macos")]
fn rects_match(left: objc2_foundation::NSRect, right: objc2_foundation::NSRect) -> bool {
    const EPS: f64 = 0.01;
    (left.origin.x - right.origin.x).abs() < EPS
        && (left.origin.y - right.origin.y).abs() < EPS
        && (left.size.width - right.size.width).abs() < EPS
        && (left.size.height - right.size.height).abs() < EPS
}

#[derive(Debug, Error)]
pub enum CefRuntimeError {
    #[error("CEF initialization failed")]
    InitializeFailed,
    #[error("persistent CEF browser storage is forbidden for the local UI runtime")]
    PersistentStorageForbidden,
    #[error("failed to create temporary CEF storage: {0}")]
    EphemeralStorage(std::io::Error),
    #[error("CEF browser creation failed")]
    CreateBrowserFailed,
    #[error("CEF operations must run on the runtime's creating thread")]
    WrongThread,
    #[error("web view URL cannot be empty")]
    EmptyUrl,
    #[error("invalid web view bounds {width}x{height}")]
    InvalidBounds { width: i32, height: i32 },
    #[error("CEF browser has no main frame")]
    MissingMainFrame,
    #[error("CEF browser has no host")]
    MissingBrowserHost,
    #[error("native web view resize failed: {0}")]
    PlatformResizeFailed(String),
    #[error("this operation is only valid for a windowless (off-screen) web view")]
    NotWindowless,
    #[error("failed to resolve the current executable: {0}")]
    CurrentExecutable(#[from] std::io::Error),
    #[cfg(target_os = "macos")]
    #[error("Chromium Embedded Framework.framework was not found in the application bundle")]
    MacFrameworkNotFound,
    #[cfg(target_os = "macos")]
    #[error("macOS executable is not inside an application bundle: {0}")]
    MacBundleLayout(PathBuf),
    #[cfg(target_os = "macos")]
    #[error("macOS CEF helper executable is missing: {0}")]
    MacHelperMissing(PathBuf),
    #[cfg(target_os = "macos")]
    #[error("macOS Chromium framework is missing or incomplete: {0}")]
    MacFrameworkMissing(PathBuf),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_positive_bounds() {
        assert!(WindowBounds::new(0, 0, 0, 100).is_err());
        assert!(WindowBounds::new(0, 0, 100, -1).is_err());
    }

    #[test]
    fn accepts_native_child_bounds() {
        assert_eq!(
            WindowBounds::new(4, 8, 1280, 720).unwrap(),
            WindowBounds {
                x: 4,
                y: 8,
                width: 1280,
                height: 720,
            }
        );
    }

    #[test]
    fn parses_equals_and_separate_switch_values() {
        let args = vec![
            "FutureboardNative".to_owned(),
            "--type=renderer".to_owned(),
            "--utility-sub-type".to_owned(),
            "network.mojom.NetworkService".to_owned(),
        ];
        assert_eq!(command_line_switch(&args, "--type"), Some("renderer"));
        assert_eq!(
            command_line_switch(&args, "--utility-sub-type"),
            Some("network.mojom.NetworkService")
        );
    }

    #[test]
    fn process_dispatch_matches_cef_return_contract() {
        assert_eq!(
            ProcessDispatch::from_exit_code(-1),
            ProcessDispatch::BrowserProcess
        );
        assert_eq!(
            ProcessDispatch::from_exit_code(0),
            ProcessDispatch::SubprocessExit(0)
        );
        assert_eq!(
            ProcessDispatch::from_exit_code(9),
            ProcessDispatch::SubprocessExit(9)
        );
    }

    #[test]
    fn global_policy_enforces_ephemeral_cookie_free_storage() {
        // Do not construct a non-empty CefString in a unit-test process: the
        // pinned CEF binding requires its bundled framework to be loaded first.
        assert!(validate_ephemeral_values("", "/temporary", 0, "", 1).is_ok());
    }

    #[test]
    fn custom_request_context_policy_is_ephemeral_and_cookie_free() {
        let settings = ephemeral_request_context_settings();

        assert!(settings.cache_path.to_string().is_empty());
        assert_eq!(settings.persist_session_cookies, 0);
        assert!(settings.cookieable_schemes_list.to_string().is_empty());
        assert_eq!(settings.cookieable_schemes_exclude_defaults, 1);
    }

    #[test]
    fn startup_validation_rejects_a_persistent_profile() {
        assert!(matches!(
            validate_ephemeral_values("persistent-profile", "/temporary", 0, "", 1),
            Err(CefRuntimeError::PersistentStorageForbidden)
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn resolves_helper_from_application_executable() {
        let root = std::env::temp_dir().join(format!(
            "futureboard-cef-layout-test-{}",
            std::process::id()
        ));
        let executable = root.join("Futureboard Studio.app/Contents/MacOS/FutureboardNative");
        let frameworks = root.join("Futureboard Studio.app/Contents/Frameworks");
        let framework = frameworks
            .join(MACOS_CEF_FRAMEWORK_NAME)
            .join("Chromium Embedded Framework");
        let helper = frameworks
            .join(format!("{MACOS_HELPER_EXECUTABLE_NAME}.app"))
            .join("Contents/MacOS")
            .join(MACOS_HELPER_EXECUTABLE_NAME);
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::create_dir_all(framework.parent().unwrap()).unwrap();
        std::fs::create_dir_all(helper.parent().unwrap()).unwrap();
        std::fs::write(&executable, []).unwrap();
        std::fs::write(&framework, []).unwrap();
        std::fs::write(&helper, []).unwrap();

        assert_eq!(
            macos_browser_subprocess(&executable).unwrap(),
            BrowserSubprocess::SeparateExecutable(helper)
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn missing_helper_has_actionable_diagnostic() {
        let root = std::env::temp_dir().join(format!(
            "futureboard-cef-missing-helper-test-{}",
            std::process::id()
        ));
        let executable = root.join("Futureboard Studio.app/Contents/MacOS/FutureboardNative");
        let framework = root
            .join("Futureboard Studio.app/Contents/Frameworks")
            .join(MACOS_CEF_FRAMEWORK_NAME)
            .join("Chromium Embedded Framework");
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::create_dir_all(framework.parent().unwrap()).unwrap();
        std::fs::write(&executable, []).unwrap();
        std::fs::write(&framework, []).unwrap();

        assert!(matches!(
            macos_browser_subprocess(&executable),
            Err(CefRuntimeError::MacHelperMissing(_))
        ));
        std::fs::remove_dir_all(&root).unwrap();
    }
}

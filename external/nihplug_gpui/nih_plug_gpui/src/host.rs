//! The process-wide GPUI application that all editors are hosted on.
//!
//! GPUI owns its event loop: [`gpui::Application::run()`] only returns once the application quits.
//! Audio plugins can't take over the host's main thread, so on Windows and Linux we run a single
//! GPUI application on a dedicated thread and drive it by sending it closures. That thread is
//! started lazily when the first editor is opened and is never shut down again, which is
//! deliberate:
//!
//! - GPUI's Windows backend calls `ExitProcess()` when its event loop terminates. If we ever let
//!   the application quit we would take the entire DAW down with us. For the same reason the
//!   application is created with [`QuitMode::Explicit`], so that closing the last editor window
//!   doesn't implicitly quit.
//! - Starting a GPUI application involves setting up a renderer, a text system, and platform event
//!   handling. Reusing a single application across every plugin instance keeps opening an editor
//!   cheap.
//!
//! macOS is different: AppKit only works on the main thread, which belongs to the host and is
//! already running the host's `NSApplication`. There the application is created on the main thread
//! with GPUI's embedded platform ([`gpui_macos::MacPlatform::new_embedded()`]), which never touches
//! `NSApplication` and schedules its work on the main dispatch queue that the host's run loop
//! already drains. Closures are sent to it the same way, so the rest of this crate does not need to
//! know which of the two it is talking to.

use gpui::{App, AssetSource, AsyncApp, QuitMode, SharedString};
use nih_plug::nih_error;
use std::borrow::Cow;
use std::sync::{Arc, OnceLock};

/// A closure to be run on the GPUI application thread.
pub(crate) type HostTask = Box<dyn FnOnce(&mut App) + Send>;

static HOST: OnceLock<Option<async_channel::Sender<HostTask>>> = OnceLock::new();

/// The asset source the application is started with, if the plugin installed one.
static ASSETS: OnceLock<Arc<dyn AssetSource>> = OnceLock::new();

/// Install the asset source the application will be started with. Returns `false` if one was
/// already installed or the application is already running, in which case `source` is not used.
pub(crate) fn set_asset_source(source: Arc<dyn AssetSource>) -> bool {
    if HOST.get().is_some() {
        return false;
    }

    ASSETS.set(source).is_ok()
}

/// [`gpui::Application::with_assets()`] takes its source by value, while the installed one is
/// shared.
struct SharedAssets(Arc<dyn AssetSource>);

impl AssetSource for SharedAssets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        self.0.load(path)
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
        self.0.list(path)
    }
}

/// Run `task` on the GPUI application thread, starting that thread if it isn't running yet. Tasks
/// are run in the order they were submitted. Returns `false` if GPUI could not be hosted on this
/// platform, in which case the task is dropped without being run.
pub(crate) fn run_on_gpui_thread(task: HostTask) -> bool {
    // The application can only be created on the main thread on macOS. Every later call may come
    // from anywhere: sending only queues the task.
    #[cfg(target_os = "macos")]
    if HOST.get().is_none() && !is_main_thread() {
        nih_error!("The first GPUI editor must be opened from the main thread");
        return false;
    }

    match HOST.get_or_init(spawn_host_thread) {
        // The channel is unbounded, so this never actually blocks
        Some(sender) => sender.send_blocking(task).is_ok(),
        None => false,
    }
}

/// Keep this module mapped until the process exits.
///
/// The GPUI thread and its vsync thread never return: ending the GPUI loop calls
/// `ExitProcess`. A host that `FreeLibrary`s the plugin while those threads are
/// still running executes code that is no longer mapped.
#[cfg(target_os = "windows")]
fn pin_this_module() {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetModuleHandleExW(
            flags: u32,
            module_name: *const u16,
            module: *mut *mut std::ffi::c_void,
        ) -> i32;
    }
    const GET_MODULE_HANDLE_EX_FLAG_PIN: u32 = 0x0000_0001;
    const GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS: u32 = 0x0000_0004;

    unsafe {
        let mut module = std::ptr::null_mut();
        let ok = GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_PIN,
            pin_this_module as *const () as *const u16,
            &mut module,
        );
        if ok == 0 {
            nih_error!("Could not pin the plugin module; unloading the plugin may crash the host");
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn spawn_host_thread() -> Option<async_channel::Sender<HostTask>> {
    #[cfg(target_os = "windows")]
    pin_this_module();

    let (sender, receiver) = async_channel::unbounded::<HostTask>();

    let thread = std::thread::Builder::new()
        .name(String::from("nih_plug_gpui"))
        .spawn(move || {
            application()
                // See the module level documentation, quitting is never safe for us
                .with_quit_mode(QuitMode::Explicit)
                .run(move |cx: &mut App| {
                    // This foreground task lives for as long as the application does. Awaiting the
                    // receiver parks the task without blocking the event loop, and sending a task
                    // wakes the loop back up through GPUI's foreground executor.
                    cx.spawn(async move |cx: &mut AsyncApp| {
                        while let Ok(task) = receiver.recv().await {
                            cx.update(|cx| task(cx));
                        }
                    })
                    .detach();
                });

            // `Application::run()` only returns when the application quits, which we never ask it
            // to do
            nih_error!("The GPUI host thread exited unexpectedly");
        });

    match thread {
        Ok(_) => Some(sender),
        Err(err) => {
            nih_error!("Could not start the GPUI host thread: {err}");

            None
        }
    }
}

/// Build a GPUI application backed by the current platform's backend.
///
/// This is what `gpui_platform::application()` does. That crate isn't used here because it depends
/// on `gpui` with its default features enabled, which would make this workspace unresolvable. See
/// the comment in `Cargo.toml`.
#[cfg(not(target_os = "macos"))]
fn application() -> gpui::Application {
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    let platform = gpui_linux::current_platform(false);
    #[cfg(target_os = "windows")]
    let platform: std::rc::Rc<dyn gpui::Platform> = std::rc::Rc::new(
        gpui_windows::WindowsPlatform::new(false)
            .expect("Could not initialize GPUI's Windows backend"),
    );

    let application = gpui::Application::with_platform(platform);
    match ASSETS.get() {
        Some(assets) => application.with_assets(SharedAssets(assets.clone())),
        None => application,
    }
}

/// Whether this is the process' main thread, the only one AppKit may be used from.
#[cfg(target_os = "macos")]
pub(crate) fn is_main_thread() -> bool {
    extern "C" {
        fn pthread_main_np() -> std::ffi::c_int;
    }

    unsafe { pthread_main_np() != 0 }
}

/// Keep this image loaded until the process exits.
///
/// GPUI registers Objective-C classes whose methods live in this image, and AppKit keeps calling
/// them (views, display links, dispatch callbacks) for as long as the application exists, which is
/// the rest of the process. A host that unloads the plug-in bundle afterwards would leave those
/// pointing at unmapped code.
#[cfg(target_os = "macos")]
fn pin_this_module() {
    use std::ffi::{c_char, c_int, c_void};

    #[repr(C)]
    struct DlInfo {
        dli_fname: *const c_char,
        dli_fbase: *mut c_void,
        dli_sname: *const c_char,
        dli_saddr: *mut c_void,
    }

    extern "C" {
        fn dladdr(address: *const c_void, info: *mut DlInfo) -> c_int;
        fn dlopen(path: *const c_char, mode: c_int) -> *mut c_void;
    }

    const RTLD_LAZY: c_int = 0x1;
    const RTLD_NOLOAD: c_int = 0x10;
    const RTLD_NODELETE: c_int = 0x80;

    unsafe {
        let mut info = std::mem::zeroed::<DlInfo>();
        if dladdr(pin_this_module as *const c_void, &mut info) == 0 || info.dli_fname.is_null() {
            nih_error!("Could not find the plugin image; unloading the plugin may crash the host");
            return;
        }
        // The handle is leaked on purpose: it is what keeps the image loaded.
        if dlopen(info.dli_fname, RTLD_LAZY | RTLD_NOLOAD | RTLD_NODELETE).is_null() {
            nih_error!("Could not pin the plugin image; unloading the plugin may crash the host");
        }
    }
}

/// Start the application on the host's main thread and run loop. See the module documentation.
#[cfg(target_os = "macos")]
fn spawn_host_thread() -> Option<async_channel::Sender<HostTask>> {
    pin_this_module();

    let (sender, receiver) = async_channel::unbounded::<HostTask>();

    let platform: std::rc::Rc<dyn gpui::Platform> =
        std::rc::Rc::new(gpui_macos::MacPlatform::new_embedded());
    let application = gpui::Application::with_platform(platform);
    let application = match ASSETS.get() {
        Some(assets) => application.with_assets(SharedAssets(assets.clone())),
        None => application,
    };

    application
        // The host decides when the process ends, and an embedded platform can't quit anyway
        .with_quit_mode(QuitMode::Explicit)
        // Returns right away and keeps the application alive: the host's run loop drives it
        .run_embedded(move |cx: &mut App| {
            cx.spawn(async move |cx: &mut AsyncApp| {
                while let Ok(task) = receiver.recv().await {
                    cx.update(|cx| task(cx));
                }
            })
            .detach();
        });

    Some(sender)
}

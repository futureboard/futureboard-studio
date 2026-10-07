//! Windows Job Object, AppUserModelID, and coordinated plugin-host shutdown.
//!
//! Implementation lives in [`crate::process_manager`] and [`crate::platform`].

pub use crate::plugin_host_spawn_config::PluginHostSpawnConfig;
pub use crate::process_manager::{
    init_plugin_host_job, shutdown_host_client, shutdown_host_client_with_timeout,
    BridgeHostManager, BridgeHostRecord, HostLifecycleState, PluginHostHandle, PluginHostId,
    PluginHostProcessManager, HOST_SHUTDOWN_TIMEOUT,
};

/// Shared Windows shell identity for FutureboardNative and PluginHost.
pub const APP_USER_MODEL_ID: &str = "studio.futureboard.Futureboard";

/// Set the process-wide explicit AppUserModelID so plugin-host and editor
/// windows group under the DAW shell identity.
pub fn set_futureboard_app_user_model_id() {
    set_app_user_model_id();
}

#[cfg(windows)]
pub fn set_app_user_model_id() {
    use windows::core::w;
    use windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
    // SAFETY: `w!` is a 'static NUL-terminated UTF-16 literal.
    let result =
        unsafe { SetCurrentProcessExplicitAppUserModelID(w!("studio.futureboard.Futureboard")) };
    match result {
        Ok(()) => eprintln!(
            "[app-id] SetCurrentProcessExplicitAppUserModelID id={APP_USER_MODEL_ID} ok=true"
        ),
        Err(error) => eprintln!(
            "[app-id] SetCurrentProcessExplicitAppUserModelID id={APP_USER_MODEL_ID} ok=false error={error}"
        ),
    }
}

#[cfg(not(windows))]
pub fn set_app_user_model_id() {}

/// Takes this process's own console window off screen and off the taskbar.
///
/// The plug-in host is a service of the DAW, not an app: it has no window of
/// its own any more — the editor's window belongs to the main process — so the
/// only thing it can put on the taskbar is its console, which a debug build
/// still gets. That console is a blank window sitting between the DAW's own,
/// and its output already goes to the host log and up the pipe to the parent,
/// so nothing is lost by hiding it.
///
/// Only ever hides a console this process owns. A host spawned with the
/// parent's console attached would otherwise hide the DAW's.
#[cfg(windows)]
pub fn hide_own_console_window() {
    use windows::Win32::System::Console::GetConsoleWindow;
    use windows::Win32::System::Threading::GetCurrentProcessId;
    use windows::Win32::UI::WindowsAndMessaging::{GetWindowThreadProcessId, ShowWindow, SW_HIDE};

    // SAFETY: plain Win32 queries; every handle is checked before use.
    unsafe {
        let console = GetConsoleWindow();
        if console.is_invalid() {
            return;
        }
        let mut owner_pid = 0u32;
        GetWindowThreadProcessId(console, Some(&mut owner_pid));
        if owner_pid != GetCurrentProcessId() {
            eprintln!("[plugin-host] console belongs to another process; left alone");
            return;
        }
        let _ = ShowWindow(console, SW_HIDE);
        eprintln!("[plugin-host] own console window hidden (kept off the taskbar)");
    }
}

#[cfg(not(windows))]
pub fn hide_own_console_window() {}

/// Takes the protocol pipe for the host alone, and points the process's
/// standard output at its standard error.
///
/// The host speaks newline-delimited JSON to Studio on what was its stdout.
/// Plug-ins share the process, and some write to stdout — Arturia's print
/// `## ExtendedFilename : 0` lines as they load — which landed in the middle
/// of the protocol: Studio read a line that was not a frame and dropped the
/// host as broken, so the plug-in "failed to load" though it had loaded.
///
/// So, before any plug-in is loaded, the pipe is duplicated into a handle
/// only the host writes, and the standard output handle and the C runtime's
/// descriptor 1 are both re-pointed at stderr: whatever a plug-in prints from
/// then on ends up in the host's log instead.
///
/// `None`, logged, when the handles cannot be moved: the host then speaks on
/// stdout as before, unprotected, rather than not at all.
#[cfg(windows)]
pub fn take_protocol_stdout() -> Option<std::fs::File> {
    use std::os::windows::io::FromRawHandle;
    use windows::Win32::Foundation::{DuplicateHandle, DUPLICATE_SAME_ACCESS, HANDLE};
    use windows::Win32::System::Console::{
        GetStdHandle, SetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
    };
    use windows::Win32::System::Threading::GetCurrentProcess;

    // SAFETY: plain handle plumbing on this process's own standard handles,
    // done once at start-up before any other thread or plug-in exists. The
    // duplicated handle is owned by the returned `File` alone.
    unsafe {
        let original = match GetStdHandle(STD_OUTPUT_HANDLE) {
            Ok(handle) if !handle.is_invalid() && !handle.0.is_null() => handle,
            _ => {
                eprintln!("[plugin-host] no stdout handle; protocol pipe left as it is");
                return None;
            }
        };
        let process = GetCurrentProcess();
        let mut private = HANDLE::default();
        if let Err(error) = DuplicateHandle(
            process,
            original,
            process,
            &mut private,
            0,
            false,
            DUPLICATE_SAME_ACCESS,
        ) {
            eprintln!("[plugin-host] protocol pipe not isolated: {error}");
            return None;
        }
        // The C runtime's descriptor 1 — what `printf` and `std::cout` write
        // through the shared UCRT — onto stderr. This closes the CRT's copy
        // of the original handle; the duplicate above stays open.
        libc::dup2(2, 1);
        if let Ok(stderr) = GetStdHandle(STD_ERROR_HANDLE) {
            let _ = SetStdHandle(STD_OUTPUT_HANDLE, stderr);
        }
        eprintln!("[plugin-host] protocol pipe isolated; plug-in stdout goes to stderr");
        Some(std::fs::File::from_raw_handle(private.0))
    }
}

/// See the Windows version: the same move with POSIX descriptors.
#[cfg(unix)]
pub fn take_protocol_stdout() -> Option<std::fs::File> {
    use std::os::unix::io::FromRawFd;

    // SAFETY: descriptor plumbing on this process's own standard streams,
    // once at start-up; the duplicate is owned by the returned `File` alone.
    unsafe {
        let private = libc::dup(1);
        if private < 0 {
            eprintln!("[plugin-host] protocol pipe not isolated: dup failed");
            return None;
        }
        // Not handed down to anything the host or a plug-in starts.
        libc::fcntl(private, libc::F_SETFD, libc::FD_CLOEXEC);
        libc::dup2(2, 1);
        eprintln!("[plugin-host] protocol pipe isolated; plug-in stdout goes to stderr");
        Some(std::fs::File::from_raw_fd(private))
    }
}

/// Ends the host now, past the plug-ins' own teardown.
///
/// By the time the host exits it has closed every editor and released every
/// plug-in. What is left is the operating system unloading their modules, and
/// some run code then that crashes — Arturia's Mix DRUMS faults in its
/// `DLL_PROCESS_DETACH` — which turned every clean shutdown into an access
/// violation exit code. Nothing a module could still do at that point is the
/// host's to wait for, so the process is ended without running it.
pub fn exit_now(code: i32) -> ! {
    use std::io::Write;
    let _ = std::io::stderr().flush();
    #[cfg(windows)]
    {
        use windows::Win32::System::Threading::{GetCurrentProcess, TerminateProcess};
        // SAFETY: terminating this process; nothing runs after it.
        unsafe {
            let _ = TerminateProcess(GetCurrentProcess(), code as u32);
        }
    }
    #[cfg(unix)]
    {
        // SAFETY: `_exit` skips atexit handlers and static destructors —
        // the point — and never returns.
        unsafe { libc::_exit(code) }
    }
    #[allow(unreachable_code)]
    std::process::exit(code)
}

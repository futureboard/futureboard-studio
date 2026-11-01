//! Crashpad installation for the native Studio process.
//!
//! The handler is downloaded by `crashpad-rs-sys`'s `prebuilt` feature and
//! copied beside the application binary by the build dependency. Runtime
//! paths deliberately live under app data rather than the install directory,
//! because packaged applications may be read-only and crash reports can be
//! written before the UI starts.

use crashpad_rs::{CrashpadClient, CrashpadConfig};
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::OnceLock;

const CRASHPAD_PATH: &str = "/crashpad/minidump";
/// See [`crash_now_if_asked`].
const TEST_CRASH_ENV: &str = "FUTUREBOARD_TEST_CRASH";

static CLIENT: OnceLock<CrashpadClient> = OnceLock::new();

/// The environment Studio was started with, before startup changed its own.
static LAUNCH_ENVIRONMENT: OnceLock<Vec<(OsString, OsString)>> = OnceLock::new();

/// Remembers the environment Studio was launched with, for a relaunch after
/// a crash. Call first thing, before startup sets anything.
///
/// Startup writes to its own environment — safe graphics mode after an
/// interrupted launch, the GPU chosen in Preferences, DirectComposition off —
/// and a relaunch spawned from the crashed process would inherit all of it:
/// one crash in safe mode would keep the next Studio in safe mode, and a GPU
/// changed since would be ignored. The test switch is left out, so answering
/// Yes after a forced crash does not crash again.
pub fn capture_launch_environment() {
    let _ = LAUNCH_ENVIRONMENT.set(
        std::env::vars_os()
            .filter(|(key, _)| key != TEST_CRASH_ENV)
            .collect(),
    );
}

/// [`capture_launch_environment`]'s copy, or the current environment if it
/// was never taken.
fn launch_environment() -> Vec<(OsString, OsString)> {
    LAUNCH_ENVIRONMENT
        .get()
        .cloned()
        .unwrap_or_else(|| std::env::vars_os().collect())
}

/// Start Crashpad for the main browser process.
///
/// Crash reporting is best-effort: a missing endpoint or handler must never
/// prevent Studio from opening. Crashpad persists pending reports locally and
/// uploads them asynchronously, so startup does not wait for the API.
pub fn install() {
    if CLIENT.get().is_some() {
        return;
    }
    // Before Crashpad, so the relaunch prompt is there even when the handler
    // cannot start. Its crash hook goes in after Crashpad: on Windows it
    // chains to Crashpad's exception filter; on macOS its signal handlers run
    // before Crashpad's Mach handler sees the crash.
    relaunch::prepare();
    start_crashpad();
    relaunch::install_crash_filter();
}

/// `FUTUREBOARD_TEST_CRASH=panic` or `=fault` ends Studio right after
/// [`install`], the way a real crash would, so the report and the relaunch
/// prompt can be checked on any build. Unset, it does nothing.
pub fn crash_now_if_asked() {
    match std::env::var(TEST_CRASH_ENV).as_deref() {
        Ok("panic") => panic!("FUTUREBOARD_TEST_CRASH=panic"),
        Ok("fault") => {
            eprintln!("[crashpad] FUTUREBOARD_TEST_CRASH=fault: reading address 0");
            // SAFETY: none — this is the access violation that was asked for.
            unsafe {
                std::ptr::read_volatile(std::ptr::null::<u8>());
            }
        }
        _ => {}
    }
}

fn start_crashpad() {
    let paths = sphere_ui_components::paths::FutureboardPaths::resolve();
    let crashpad_dir = paths.app_data.join("Crashpad");
    let Some(handler_path) = handler_path() else {
        eprintln!("[crashpad] disabled: target uses an in-process handler");
        return;
    };
    let mut config = CrashpadConfig::builder()
        .handler_path(handler_path)
        .database_path(crashpad_dir.join("database"))
        .metrics_path(crashpad_dir.join("metrics"))
        .rate_limit(true)
        .upload_gzip(true)
        .periodic_tasks(true)
        .identify_client_via_url(true);
    // Without an endpoint the handler still runs and keeps reports in the
    // database: a build made without `.env` used to skip Crashpad entirely,
    // so its crashes left no handler process and no dump at all.
    let upload_url = upload_url();
    if let Some(url) = upload_url.clone() {
        config = config.url(url);
    }
    let config = config.build();

    let mut annotations = HashMap::new();
    annotations.insert("format".to_string(), "minidump".to_string());
    annotations.insert("prod".to_string(), "futureboard-studio".to_string());
    annotations.insert("ver".to_string(), env!("CARGO_PKG_VERSION").to_string());
    annotations.insert("platform".to_string(), std::env::consts::OS.to_string());
    annotations.insert("arch".to_string(), std::env::consts::ARCH.to_string());
    annotations.insert("edition".to_string(), edition_name().to_string());

    let client = match CrashpadClient::new() {
        Ok(client) => client,
        Err(error) => {
            eprintln!("[crashpad] client initialization failed: {error}");
            return;
        }
    };
    if let Err(error) = client.start_with_config(&config, &annotations) {
        eprintln!("[crashpad] handler startup failed: {error}");
        return;
    }

    let _ = CLIENT.set(client);
    if upload_url.is_some() {
        eprintln!("[crashpad] handler started; upload endpoint configured");
    } else {
        eprintln!("[crashpad] handler started; no upload endpoint, reports stay local");
    }
}

/// A panic is about to end the process: write a report, then offer to
/// relaunch.
///
/// Release builds abort on panic, and on Windows Rust's abort is
/// `__fastfail`, which ends the process without running any exception filter
/// — so Crashpad, which only hears about crashes through one, never saw the
/// commonest way Studio dies. The report is asked for here instead, while the
/// process is intact. Elsewhere the abort raises `SIGABRT`, which Crashpad
/// reports itself; a dump here as well would file every panic twice. Builds
/// that unwind leave panics alone: one may still be caught, and the process
/// live on.
pub fn on_panic() {
    if !cfg!(panic = "abort") {
        return;
    }
    if cfg!(windows) {
        if let Some(client) = CLIENT.get() {
            client.dump_without_crash();
        }
    }
    relaunch::spawn_prompt();
}

fn upload_url() -> Option<String> {
    #[cfg(debug_assertions)]
    if let Ok(value) = std::env::var("FUTUREBOARD_CRASHPAD_URL") {
        let value = value.trim().trim_end_matches('/');
        if acceptable_endpoint(value) {
            return Some(value.to_string());
        }
        eprintln!("[crashpad] ignoring invalid FUTUREBOARD_CRASHPAD_URL");
    }

    sphere_ui_components::auth::api_base_url().map(|base| format!("{base}{CRASHPAD_PATH}"))
}

fn acceptable_endpoint(url: &str) -> bool {
    url.starts_with("https://")
        || (cfg!(debug_assertions)
            && (url.starts_with("http://127.0.0.1") || url.starts_with("http://localhost")))
}

fn handler_path() -> Option<PathBuf> {
    if cfg!(any(
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos"
    )) {
        return None;
    }

    if let Some(path) = std::env::var_os("CRASHPAD_HANDLER").filter(|path| !path.is_empty()) {
        return Some(PathBuf::from(path));
    }

    let name = if cfg!(target_os = "android") {
        "libcrashpad_handler.so"
    } else if cfg!(windows) {
        "crashpad_handler.exe"
    } else {
        "crashpad_handler"
    };
    if let Ok(executable) = std::env::current_exe() {
        if let Some(parent) = executable.parent() {
            let beside_executable = parent.join(name);
            if beside_executable.is_file() {
                return Some(beside_executable);
            }
        }
    }

    Some(
        option_env!("CRASHPAD_HANDLER_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(name)),
    )
}

fn edition_name() -> &'static str {
    if cfg!(feature = "professional") {
        "professional"
    } else {
        "community"
    }
}

/// The "Relaunch Futureboard Studio?" prompt after a crash.
///
/// It runs in a new launcher process (`engine_abi::CRASH_RELAUNCH_ARG`), not
/// here: a crashed process cannot be trusted to draw a dialog, and the new
/// Studio has to wait for this one to be gone anyway. Everything the spawn
/// needs is built up front by [`prepare`], because a crash is no time to
/// allocate — the heap may be what broke.
#[cfg(windows)]
mod relaunch {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};

    use crate::engine_abi::CRASH_RELAUNCH_ARG;

    const EXCEPTION_CONTINUE_SEARCH: i32 = 0;
    const CREATE_UNICODE_ENVIRONMENT: u32 = 0x0000_0400;
    /// Spawning a process needs more stack than an overflowed thread has
    /// left; trying would end the process before Crashpad's filter writes
    /// the report, so that crash goes without the prompt.
    const STATUS_STACK_OVERFLOW: u32 = 0xC000_00FD;

    type ExceptionFilter = unsafe extern "system" fn(*mut c_void) -> i32;

    #[repr(C)]
    struct StartupInfoW {
        cb: u32,
        reserved: *mut u16,
        desktop: *mut u16,
        title: *mut u16,
        x: u32,
        y: u32,
        x_size: u32,
        y_size: u32,
        x_count_chars: u32,
        y_count_chars: u32,
        fill_attribute: u32,
        flags: u32,
        show_window: u16,
        reserved2_len: u16,
        reserved2: *mut u8,
        std_input: *mut c_void,
        std_output: *mut c_void,
        std_error: *mut c_void,
    }

    #[repr(C)]
    struct ProcessInformation {
        process: *mut c_void,
        thread: *mut c_void,
        process_id: u32,
        thread_id: u32,
    }

    #[link(name = "kernel32")]
    extern "system" {
        #[allow(clippy::too_many_arguments)]
        fn CreateProcessW(
            application: *const u16,
            command_line: *mut u16,
            process_attributes: *mut c_void,
            thread_attributes: *mut c_void,
            inherit_handles: i32,
            creation_flags: u32,
            environment: *mut c_void,
            current_directory: *const u16,
            startup_info: *mut StartupInfoW,
            process_information: *mut ProcessInformation,
        ) -> i32;
        fn CloseHandle(handle: *mut c_void) -> i32;
        fn SetUnhandledExceptionFilter(filter: Option<ExceptionFilter>) -> Option<ExceptionFilter>;
    }

    /// `"<launcher>" --futureboard-crash-relaunch=<pid>`, NUL-terminated and
    /// writable, as `CreateProcessW` wants it. Leaked: it lives as long as the
    /// process.
    static COMMAND_LINE: AtomicPtr<u16> = AtomicPtr::new(std::ptr::null_mut());
    /// The launch environment as a `CreateProcessW` block: `KEY=value` pairs,
    /// each NUL-terminated, sorted by key, ending in an empty one. Leaked.
    static ENVIRONMENT: AtomicPtr<u16> = AtomicPtr::new(std::ptr::null_mut());
    /// One prompt per process, whichever of panic and fault gets there first.
    static SPAWNED: AtomicBool = AtomicBool::new(false);
    /// The filter installed before ours — Crashpad's — as a function address.
    static PREVIOUS_FILTER: AtomicUsize = AtomicUsize::new(0);

    pub fn prepare() {
        let Ok(launcher) = std::env::current_exe() else {
            return;
        };
        let command = format!(
            "\"{}\" {CRASH_RELAUNCH_ARG}{}",
            launcher.display(),
            std::process::id()
        );
        let wide: Box<[u16]> = command.encode_utf16().chain(Some(0)).collect();
        COMMAND_LINE.store(Box::leak(wide).as_mut_ptr(), Ordering::Release);

        let mut variables = super::launch_environment();
        variables.sort_by_key(|(key, _)| key.to_ascii_uppercase());
        let mut block: Vec<u16> = Vec::new();
        for (key, value) in &variables {
            block.extend(key.encode_wide());
            block.push(u16::from(b'='));
            block.extend(value.encode_wide());
            block.push(0);
        }
        if block.is_empty() {
            block.push(0);
        }
        block.push(0);
        ENVIRONMENT.store(
            Box::leak(block.into_boxed_slice()).as_mut_ptr(),
            Ordering::Release,
        );
    }

    /// Starts the prompt. It builds nothing, so it is safe from a fault on a
    /// broken heap; see [`STATUS_STACK_OVERFLOW`] for the one case it skips.
    pub fn spawn_prompt() {
        let command_line = COMMAND_LINE.load(Ordering::Acquire);
        if command_line.is_null() || SPAWNED.swap(true, Ordering::AcqRel) {
            return;
        }
        let environment = ENVIRONMENT.load(Ordering::Acquire);
        // SAFETY: both structs are plain data zero-initialised as the API
        // expects, `cb` is set, the command line is a live, writable,
        // NUL-terminated buffer, and the environment is a live, terminated
        // UTF-16 block (or null, to inherit).
        unsafe {
            let mut startup: StartupInfoW = std::mem::zeroed();
            startup.cb = size_of::<StartupInfoW>() as u32;
            let mut process: ProcessInformation = std::mem::zeroed();
            let created = CreateProcessW(
                std::ptr::null(),
                command_line,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
                CREATE_UNICODE_ENVIRONMENT,
                environment.cast(),
                std::ptr::null(),
                &mut startup,
                &mut process,
            );
            if created != 0 {
                CloseHandle(process.thread);
                CloseHandle(process.process);
            }
        }
    }

    /// Puts [`on_unhandled_exception`] in front of whatever filter is there —
    /// Crashpad's, once it started — so a native fault offers the relaunch too.
    pub fn install_crash_filter() {
        // SAFETY: installing a process-wide filter; the previous one is kept
        // and called from ours.
        let previous = unsafe { SetUnhandledExceptionFilter(Some(on_unhandled_exception)) };
        PREVIOUS_FILTER.store(
            previous.map_or(0, |filter| filter as usize),
            Ordering::Release,
        );
    }

    unsafe extern "system" fn on_unhandled_exception(pointers: *mut c_void) -> i32 {
        // SAFETY: the system hands the filter a valid `EXCEPTION_POINTERS`.
        if unsafe { exception_code(pointers) } != Some(STATUS_STACK_OVERFLOW) {
            spawn_prompt();
        }
        match PREVIOUS_FILTER.load(Ordering::Acquire) {
            0 => EXCEPTION_CONTINUE_SEARCH,
            // SAFETY: the value was stored from a filter function pointer.
            address => unsafe {
                let previous: ExceptionFilter = std::mem::transmute(address);
                previous(pointers)
            },
        }
    }

    /// The exception code: `EXCEPTION_POINTERS` begins with a pointer to the
    /// `EXCEPTION_RECORD`, which begins with the code.
    ///
    /// # Safety
    /// `pointers` is null or a valid `EXCEPTION_POINTERS`.
    unsafe fn exception_code(pointers: *mut c_void) -> Option<u32> {
        if pointers.is_null() {
            return None;
        }
        let record = unsafe { *pointers.cast::<*const u32>() };
        (!record.is_null()).then(|| unsafe { *record })
    }
}

/// The macOS side of the prompt: the same launcher argument, spawned with
/// `posix_spawn` from arguments built up front.
///
/// Crashpad hears about a native crash through Mach `EXC_CRASH`, which the
/// kernel raises only once a fatal signal's default action has run — so a
/// signal handler sees the crash first. Ours spawns the prompt, puts the
/// previous handler back and returns: the faulting instruction runs again
/// under that handler (Rust's stack-overflow report, or the default action),
/// the process ends, and Crashpad takes the dump as before.
#[cfg(target_os = "macos")]
mod relaunch {
    use std::ffi::{c_char, c_int, c_void, CString};
    use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
    use std::sync::OnceLock;

    use crate::engine_abi::CRASH_RELAUNCH_ARG;

    const SIGILL: c_int = 4;
    const SIGTRAP: c_int = 5;
    const SIGABRT: c_int = 6;
    const SIGFPE: c_int = 8;
    const SIGBUS: c_int = 10;
    const SIGSEGV: c_int = 11;
    /// The signals a crash arrives as. `SIGTRAP` is how a Swift or
    /// Objective-C runtime trap ends a process; `SIGABRT`, an `abort()` from
    /// C++ (a panic has already spawned the prompt by then).
    const CRASH_SIGNALS: [c_int; 6] = [SIGSEGV, SIGBUS, SIGILL, SIGFPE, SIGTRAP, SIGABRT];

    const SA_ONSTACK: c_int = 0x0001;
    const SA_SIGINFO: c_int = 0x0040;

    /// `struct sigaction` as macOS's libc declares it: the handler union,
    /// `sigset_t` (a `u32`), then the flags.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct SigAction {
        handler: usize,
        mask: u32,
        flags: c_int,
    }

    extern "C" {
        fn sigaction(signal: c_int, action: *const SigAction, old: *mut SigAction) -> c_int;
        fn posix_spawn(
            pid: *mut c_int,
            path: *const c_char,
            file_actions: *const c_void,
            attributes: *const c_void,
            argv: *const *mut c_char,
            envp: *const *mut c_char,
        ) -> c_int;
        /// `environ` is not reachable directly from a dynamic library on
        /// macOS; this is the supported way to it.
        fn _NSGetEnviron() -> *mut *const *mut c_char;
    }

    /// `[launcher, --futureboard-crash-relaunch=<pid>, NULL]`. Leaked: it
    /// lives as long as the process.
    static ARGV: AtomicPtr<*mut c_char> = AtomicPtr::new(std::ptr::null_mut());
    /// The launch environment as `KEY=value` strings, NULL-terminated. Leaked.
    static ENVP: AtomicPtr<*mut c_char> = AtomicPtr::new(std::ptr::null_mut());
    /// One prompt per process, whichever of panic and signal gets there first.
    static SPAWNED: AtomicBool = AtomicBool::new(false);
    /// What each of [`CRASH_SIGNALS`] was handled by before ours.
    static PREVIOUS: OnceLock<[SigAction; CRASH_SIGNALS.len()]> = OnceLock::new();

    pub fn prepare() {
        let Ok(launcher) = std::env::current_exe() else {
            return;
        };
        let (Ok(path), Ok(argument)) = (
            CString::new(launcher.as_os_str().as_encoded_bytes()),
            CString::new(format!("{CRASH_RELAUNCH_ARG}{}", std::process::id())),
        ) else {
            return;
        };
        let argv: Box<[*mut c_char; 3]> =
            Box::new([path.into_raw(), argument.into_raw(), std::ptr::null_mut()]);
        ARGV.store(Box::leak(argv).as_mut_ptr(), Ordering::Release);

        let mut envp: Vec<*mut c_char> = super::launch_environment()
            .into_iter()
            .filter_map(|(key, value)| {
                let mut entry = key.into_encoded_bytes();
                entry.push(b'=');
                entry.extend(value.into_encoded_bytes());
                CString::new(entry).ok().map(CString::into_raw)
            })
            .collect();
        envp.push(std::ptr::null_mut());
        ENVP.store(
            Box::leak(envp.into_boxed_slice()).as_mut_ptr(),
            Ordering::Release,
        );
    }

    /// Starts the prompt. Allocation-free, so it is safe from a signal.
    pub fn spawn_prompt() {
        let argv = ARGV.load(Ordering::Acquire);
        if argv.is_null() || SPAWNED.swap(true, Ordering::AcqRel) {
            return;
        }
        let mut pid: c_int = 0;
        // SAFETY: `argv` and `envp` are live, NULL-terminated arrays of
        // NUL-terminated strings, `argv`'s first entry the launcher's path;
        // the fallback environment comes from libc.
        unsafe {
            let envp = match ENVP.load(Ordering::Acquire) {
                envp if envp.is_null() => *_NSGetEnviron(),
                envp => envp.cast_const(),
            };
            posix_spawn(
                &mut pid,
                *argv,
                std::ptr::null(),
                std::ptr::null(),
                argv,
                envp,
            );
        }
    }

    pub fn install_crash_filter() {
        let handler: extern "C" fn(c_int, *mut c_void, *mut c_void) = on_crash_signal;
        let ours = SigAction {
            handler: handler as usize,
            mask: 0,
            // On the alternate stack where one is set up, so a stack
            // overflow still reaches the handler.
            flags: SA_SIGINFO | SA_ONSTACK,
        };
        // Read every previous action before installing any of ours: a
        // handler that found nothing to restore would return into the same
        // fault forever.
        let mut previous = [SigAction {
            handler: 0,
            mask: 0,
            flags: 0,
        }; CRASH_SIGNALS.len()];
        for (slot, &signal) in previous.iter_mut().zip(CRASH_SIGNALS.iter()) {
            // SAFETY: a query; `slot` is a valid `SigAction`.
            if unsafe { sigaction(signal, std::ptr::null(), slot) } != 0 {
                return;
            }
        }
        if PREVIOUS.set(previous).is_err() {
            return;
        }
        for &signal in &CRASH_SIGNALS {
            // SAFETY: `ours` is a valid `SigAction` naming a handler of the
            // `SA_SIGINFO` shape.
            unsafe {
                sigaction(signal, &ours, std::ptr::null_mut());
            }
        }
    }

    extern "C" fn on_crash_signal(signal: c_int, _info: *mut c_void, _context: *mut c_void) {
        spawn_prompt();
        // The action this signal had before ours; the default (`SIG_DFL`,
        // handler 0) if that is somehow unknown, so the return never loops.
        let restore = CRASH_SIGNALS
            .iter()
            .position(|&s| s == signal)
            .zip(PREVIOUS.get())
            .map(|(index, previous)| previous[index])
            .unwrap_or(SigAction {
                handler: 0,
                mask: 0,
                flags: 0,
            });
        // SAFETY: `restore` is a valid `SigAction`.
        unsafe {
            sigaction(signal, &restore, std::ptr::null_mut());
        }
    }
}

/// Elsewhere a crash ends Studio as before; the report is still written.
#[cfg(not(any(windows, target_os = "macos")))]
mod relaunch {
    pub fn prepare() {}
    pub fn spawn_prompt() {}
    pub fn install_crash_filter() {}
}

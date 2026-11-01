#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! `FutureboardNative` — the Futureboard Studio launcher.
//!
//! The Studio itself lives in the `FutureboardEngine_x64` library built by this
//! package (`lib.rs`). This executable keeps what has to belong to the process
//! image — the Windows icon, manifest and version resources, and the process
//! identity that CEF helper launches, `fbrd://` links, the updater and Crashpad
//! all point at — loads the engine from its own directory and hands the main
//! thread to it. It deliberately uses nothing but `std`.

mod engine_abi;

use std::ffi::{c_char, c_void, CStr};
use std::path::PathBuf;

fn main() {
    if let Some(pid) = crashed_studio_pid() {
        platform::offer_relaunch(pid);
        return;
    }
    let code = match launch() {
        Ok(code) => code,
        Err(detail) => {
            report_launch_failure(&detail);
            1
        }
    };
    std::process::exit(code);
}

fn launch() -> Result<i32, String> {
    let path = engine_path()?;
    let library = platform::Library::open(&path)
        .map_err(|error| format!("Could not load {}:\n{error}", path.display()))?;

    // SAFETY: every symbol is looked up under the name and signature declared
    // in `engine_abi.rs`, which `lib.rs` exports.
    let abi_version: extern "C" fn() -> u32 =
        unsafe { library.function(engine_abi::ABI_VERSION_SYMBOL)? };
    let engine_abi_version = abi_version();
    if engine_abi_version != engine_abi::ABI_VERSION {
        return Err(format!(
            "{} has engine ABI {engine_abi_version}; this launcher needs ABI {}.",
            path.display(),
            engine_abi::ABI_VERSION
        ));
    }

    let version: extern "C" fn() -> *const c_char =
        unsafe { library.function(engine_abi::VERSION_SYMBOL)? };
    // SAFETY: the engine returns a pointer to a static NUL-terminated string.
    let engine_version = unsafe { CStr::from_ptr(version()) }.to_string_lossy();
    if engine_version != env!("CARGO_PKG_VERSION") {
        return Err(format!(
            "{} is version {engine_version}, but FutureboardNative is version {}.",
            path.display(),
            env!("CARGO_PKG_VERSION")
        ));
    }

    let engine_main: extern "C" fn() -> i32 = unsafe { library.function(engine_abi::MAIN_SYMBOL)? };
    // The engine owns the process from here on and is never unloaded: its
    // audio, plug-in host and CEF threads can outlive the call below.
    std::mem::forget(library);
    Ok(engine_main())
}

/// The engine library beside this executable. On macOS that is the bundle's
/// `Contents/MacOS`; on Linux the real install directory behind any symlink.
fn engine_path() -> Result<PathBuf, String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("Could not locate FutureboardNative: {error}"))?;
    #[cfg(unix)]
    let executable = std::fs::canonicalize(&executable).unwrap_or(executable);
    let directory = executable
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", executable.display()))?;
    Ok(directory.join(engine_abi::library_file_name()))
}

/// The crashed Studio's process id, when this launch is the relaunch prompt a
/// crash spawned (see [`engine_abi::CRASH_RELAUNCH_ARG`]).
fn crashed_studio_pid() -> Option<u32> {
    // `args_os`: `args` panics on an argument that is not UTF-8, and this runs
    // for every launch, before anything can report it.
    std::env::args_os().skip(1).find_map(|arg| {
        arg.to_str()?
            .strip_prefix(engine_abi::CRASH_RELAUNCH_ARG)?
            .parse()
            .ok()
    })
}

fn report_launch_failure(detail: &str) {
    let mut message = format!(
        "Futureboard Studio could not start its engine.\n\n{detail}\n\n\
         Reinstall Futureboard Studio to restore missing or mismatched files."
    );
    if cfg!(debug_assertions) {
        message.push_str(
            "\n\nDeveloper build: `cargo run` and `cargo build --bin FutureboardNative` \
             build only the launcher. Build the whole package with \
             `cargo build -p futureboard_native` to (re)build the engine library.",
        );
    }
    eprintln!("[launcher] {message}");
    platform::show_error("Futureboard Studio", &message);
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    const LOAD_WITH_ALTERED_SEARCH_PATH: u32 = 0x0000_0008;
    const MB_YESNO: u32 = 0x0000_0004;
    const MB_ICONERROR: u32 = 0x0000_0010;
    const MB_SETFOREGROUND: u32 = 0x0001_0000;
    const MB_TOPMOST: u32 = 0x0004_0000;
    const IDYES: i32 = 6;
    const SYNCHRONIZE: u32 = 0x0010_0000;
    /// How long a relaunch waits for the crashed process to finish dying.
    const CRASHED_EXIT_WAIT_MS: u32 = 30_000;

    #[link(name = "kernel32")]
    extern "system" {
        fn LoadLibraryExW(file_name: *const u16, file: *mut c_void, flags: u32) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const c_char) -> *mut c_void;
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
        fn WaitForSingleObject(handle: *mut c_void, milliseconds: u32) -> u32;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }

    #[link(name = "user32")]
    extern "system" {
        fn MessageBoxW(owner: *mut c_void, text: *const u16, caption: *const u16, kind: u32)
            -> i32;
    }

    pub struct Library(*mut c_void);

    impl Library {
        /// Loads by absolute path; the altered search path resolves the
        /// engine's own imports (`libcef.dll`, the VC runtime) from its
        /// directory first.
        pub fn open(path: &Path) -> Result<Self, String> {
            let wide = wide(path.as_os_str());
            let module = unsafe {
                LoadLibraryExW(
                    wide.as_ptr(),
                    std::ptr::null_mut(),
                    LOAD_WITH_ALTERED_SEARCH_PATH,
                )
            };
            if module.is_null() {
                return Err(std::io::Error::last_os_error().to_string());
            }
            Ok(Self(module))
        }

        /// # Safety
        /// `F` must be the function pointer type the symbol was exported with.
        pub unsafe fn function<F: Copy>(&self, name: &CStr) -> Result<F, String> {
            assert_eq!(size_of::<F>(), size_of::<*mut c_void>());
            let address = GetProcAddress(self.0, name.as_ptr());
            if address.is_null() {
                return Err(format!(
                    "The engine library does not export `{}`.",
                    name.to_string_lossy()
                ));
            }
            Ok(std::mem::transmute_copy(&address))
        }
    }

    pub fn show_error(caption: &str, message: &str) {
        let text = wide(message.as_ref());
        let caption = wide(caption.as_ref());
        unsafe {
            MessageBoxW(
                std::ptr::null_mut(),
                text.as_ptr(),
                caption.as_ptr(),
                MB_ICONERROR,
            );
        }
    }

    /// Asks whether to start Studio again after process `pid` crashed, and on
    /// yes starts it once that process has exited: until then it still holds
    /// the audio device, the plug-in hosts and the session lock, and a new
    /// Studio would find them taken.
    pub fn offer_relaunch(pid: u32) {
        // Taken before the question: by the time it is answered the crashed
        // process may be gone and its id handed to an unrelated one, which a
        // relaunch would then wait on.
        let crashed = unsafe { OpenProcess(SYNCHRONIZE, 0, pid) };
        let text = wide(
            "Futureboard Studio stopped unexpectedly.\n\n\
             Relaunch Futureboard Studio?"
                .as_ref(),
        );
        let caption = wide("Futureboard Studio".as_ref());
        let answer = unsafe {
            MessageBoxW(
                std::ptr::null_mut(),
                text.as_ptr(),
                caption.as_ptr(),
                MB_YESNO | MB_ICONERROR | MB_SETFOREGROUND | MB_TOPMOST,
            )
        };
        if !crashed.is_null() {
            unsafe {
                if answer == IDYES {
                    WaitForSingleObject(crashed, CRASHED_EXIT_WAIT_MS);
                }
                CloseHandle(crashed);
            }
        }
        if answer != IDYES {
            return;
        }
        let started = std::env::current_exe()
            .and_then(|executable| std::process::Command::new(executable).spawn());
        if let Err(error) = started {
            show_error(
                "Futureboard Studio",
                &format!("Futureboard Studio could not be relaunched:\n{error}"),
            );
        }
    }

    fn wide(value: &std::ffi::OsStr) -> Vec<u16> {
        value.encode_wide().chain(Some(0)).collect()
    }
}

#[cfg(unix)]
mod platform {
    use super::*;
    use std::ffi::{c_int, CString};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    const RTLD_NOW: c_int = 2;

    #[cfg_attr(all(target_os = "linux", target_env = "gnu"), link(name = "dl"))]
    extern "C" {
        fn dlopen(file_name: *const c_char, flags: c_int) -> *mut c_void;
        fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
        fn dlerror() -> *const c_char;
    }

    pub struct Library(*mut c_void);

    impl Library {
        pub fn open(path: &Path) -> Result<Self, String> {
            let file_name = CString::new(path.as_os_str().as_bytes())
                .map_err(|_| "the path contains a NUL byte".to_string())?;
            let handle = unsafe { dlopen(file_name.as_ptr(), RTLD_NOW) };
            if handle.is_null() {
                return Err(last_dl_error());
            }
            Ok(Self(handle))
        }

        /// # Safety
        /// `F` must be the function pointer type the symbol was exported with.
        pub unsafe fn function<F: Copy>(&self, name: &CStr) -> Result<F, String> {
            assert_eq!(size_of::<F>(), size_of::<*mut c_void>());
            let address = dlsym(self.0, name.as_ptr());
            if address.is_null() {
                return Err(format!(
                    "The engine library does not export `{}`: {}",
                    name.to_string_lossy(),
                    last_dl_error()
                ));
            }
            Ok(std::mem::transmute_copy(&address))
        }
    }

    fn last_dl_error() -> String {
        let error = unsafe { dlerror() };
        if error.is_null() {
            "unknown dynamic loader error".to_string()
        } else {
            unsafe { CStr::from_ptr(error) }
                .to_string_lossy()
                .into_owned()
        }
    }

    pub fn show_error(_caption: &str, _message: &str) {
        // Already on stderr; there is no toolkit loaded to show a dialog with.
    }

    /// Asks whether to start Studio again after process `pid` crashed, and on
    /// yes starts it once that process has exited (see the Windows version).
    ///
    /// A `CFUserNotification` alert: it needs no toolkit and no app bundle
    /// identity in this process, which is just the launcher binary.
    #[cfg(target_os = "macos")]
    pub fn offer_relaunch(pid: u32) {
        if !mac::ask_relaunch() {
            return;
        }
        mac::wait_for_exit(pid);
        // Through Launch Services when inside the bundle, so the new Studio
        // is a proper app launch; the bare binary otherwise.
        let executable = match std::env::current_exe() {
            Ok(executable) => executable,
            Err(_) => return,
        };
        let bundle = executable
            .ancestors()
            .find(|path| path.extension().is_some_and(|ext| ext == "app"));
        let started = match bundle {
            Some(bundle) => std::process::Command::new("/usr/bin/open")
                .arg("-n")
                .arg(bundle)
                .spawn(),
            None => std::process::Command::new(&executable).spawn(),
        };
        if let Err(error) = started {
            eprintln!("[launcher] Futureboard Studio could not be relaunched: {error}");
        }
    }

    /// The engine spawns the prompt on Windows and macOS only; see
    /// crash_reporting.rs.
    #[cfg(not(target_os = "macos"))]
    pub fn offer_relaunch(_pid: u32) {}

    #[cfg(target_os = "macos")]
    mod mac {
        use std::ffi::{c_char, c_int, c_void, CString};

        const UTF8: u32 = 0x0800_0100;
        const STOP_ALERT_LEVEL: usize = 0;
        /// The low bits of the response: which button was pressed.
        const RESPONSE_MASK: usize = 0x3;
        const DEFAULT_RESPONSE: usize = 0;
        const ESRCH: i32 = 3;
        /// How long a relaunch waits for the crashed process to finish dying.
        const CRASHED_EXIT_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

        #[link(name = "CoreFoundation", kind = "framework")]
        extern "C" {
            fn CFStringCreateWithCString(
                allocator: *const c_void,
                text: *const c_char,
                encoding: u32,
            ) -> *const c_void;
            fn CFRelease(object: *const c_void);
            #[allow(clippy::too_many_arguments)]
            fn CFUserNotificationDisplayAlert(
                timeout: f64,
                flags: usize,
                icon_url: *const c_void,
                sound_url: *const c_void,
                localization_url: *const c_void,
                header: *const c_void,
                message: *const c_void,
                default_button: *const c_void,
                alternate_button: *const c_void,
                other_button: *const c_void,
                response: *mut usize,
            ) -> i32;
        }

        extern "C" {
            fn kill(pid: c_int, signal: c_int) -> c_int;
        }

        /// A `CFString` released when dropped.
        struct CfString(*const c_void);

        impl CfString {
            fn new(text: &str) -> Self {
                let text = CString::new(text).unwrap_or_default();
                // SAFETY: a NUL-terminated UTF-8 string; null allocator is
                // the default one.
                Self(unsafe { CFStringCreateWithCString(std::ptr::null(), text.as_ptr(), UTF8) })
            }
        }

        impl Drop for CfString {
            fn drop(&mut self) {
                if !self.0.is_null() {
                    // SAFETY: created above and released once.
                    unsafe { CFRelease(self.0) }
                }
            }
        }

        /// Whether "Relaunch" (the default button) was pressed.
        pub fn ask_relaunch() -> bool {
            let header = CfString::new("Futureboard Studio stopped unexpectedly.");
            let message = CfString::new("Relaunch Futureboard Studio?");
            let relaunch = CfString::new("Relaunch");
            let close = CfString::new("Close");
            let mut response: usize = 0;
            // SAFETY: every string is a live CFString or null, and
            // `response` is a valid out-pointer; no timeout.
            let status = unsafe {
                CFUserNotificationDisplayAlert(
                    0.0,
                    STOP_ALERT_LEVEL,
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    header.0,
                    message.0,
                    relaunch.0,
                    close.0,
                    std::ptr::null(),
                    &mut response,
                )
            };
            status == 0 && response & RESPONSE_MASK == DEFAULT_RESPONSE
        }

        /// Waits, up to a limit, for process `pid` to be gone.
        pub fn wait_for_exit(pid: u32) {
            let Ok(pid) = c_int::try_from(pid) else {
                return;
            };
            let deadline = std::time::Instant::now() + CRASHED_EXIT_WAIT;
            while std::time::Instant::now() < deadline {
                // SAFETY: signal 0 only asks whether the process exists.
                let alive = unsafe { kill(pid, 0) } == 0
                    || std::io::Error::last_os_error().raw_os_error() != Some(ESRCH);
                if !alive {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }
}

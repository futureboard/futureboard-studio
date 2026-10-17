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
    const MB_ICONERROR: u32 = 0x0000_0010;

    #[link(name = "kernel32")]
    extern "system" {
        fn LoadLibraryExW(file_name: *const u16, file: *mut c_void, flags: u32) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const c_char) -> *mut c_void;
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
}

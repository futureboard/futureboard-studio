//! The C ABI between the `FutureboardNative` launcher and the
//! `FutureboardEngine_x64` library.
//!
//! Compiled into both crates of this package, so the launcher and the engine
//! cannot disagree about a symbol name or the ABI revision. Only C types cross
//! this boundary: the two modules have separate Rust runtimes and allocators.

use std::ffi::CStr;

/// File stem of the engine library. The platform prefix and suffix are added by
/// [`library_file_name`]: `FutureboardEngine_x64.dll`,
/// `libFutureboardEngine_x64.so`, `libFutureboardEngine_x64.dylib`. The `x64`
/// suffix is a fixed name on every architecture, like
/// `FutureboardPluginHostX64`.
pub const LIBRARY_STEM: &str = "FutureboardEngine_x64";

/// Revision of the exported functions below. Bump it whenever one of their
/// signatures or meanings changes.
pub const ABI_VERSION: u32 = 1;

/// `extern "C" fn() -> u32` returning [`ABI_VERSION`].
pub const ABI_VERSION_SYMBOL: &CStr = c"futureboard_engine_abi_version";

/// `extern "C" fn() -> *const c_char` returning the engine's package version as
/// a static NUL-terminated string.
pub const VERSION_SYMBOL: &CStr = c"futureboard_engine_version";

/// `extern "C" fn() -> i32`: runs the whole Studio process and returns its exit
/// code. Called once, on the process main thread.
pub const MAIN_SYMBOL: &CStr = c"futureboard_engine_main";

/// The engine library's file name on the platform this crate is built for.
pub fn library_file_name() -> String {
    format!(
        "{}{LIBRARY_STEM}{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    )
}

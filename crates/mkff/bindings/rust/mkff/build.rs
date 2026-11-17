use std::env;
use std::path::Path;

/// `mkff-sys` emits `cargo:rustc-link-search` / `cargo:rustc-link-lib`
/// (those propagate). `cargo:rustc-link-arg` does not — rpath must be
/// re-emitted here so `cargo test` / `cargo run` binaries for this crate
/// find the bundled native libraries on Linux and macOS.
fn main() {
    println!("cargo:rerun-if-env-changed=MKFF_LIB_DIR");
    println!("cargo:rerun-if-env-changed=DEP_MKFF_LIB_DIR");

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "linux" && target_os != "macos" {
        return;
    }

    if let Ok(lib_dir) = env::var("DEP_MKFF_LIB_DIR") {
        emit_rpath(Path::new(&lib_dir));
        return;
    }

    if let Ok(lib_dir) = env::var("MKFF_LIB_DIR") {
        emit_rpath(Path::new(&lib_dir));
    }
}

fn emit_rpath(lib_dir: &Path) {
    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", lib_dir.display());
}

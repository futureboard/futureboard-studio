use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let repo_root = repo_root_from_manifest(&manifest_dir);

    emit_rerun_directives(&repo_root);

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    // TARGET/HOST are consulted by the cmake crate for cross builds; reading
    // them here also documents the variables this script is aware of.
    let _ = env::var("TARGET");
    let _ = env::var("HOST");
    let _ = env::var("PROFILE");

    if cfg!(feature = "bundled") {
        build_and_link_bundled(&repo_root, &target_os, &target_env);
        return;
    }

    if let Ok(lib_dir) = env::var("MKFF_LIB_DIR") {
        let lib_dir = PathBuf::from(lib_dir);
        let include_dir = env::var("MKFF_INCLUDE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| repo_root.join("include"));
        link_external(&lib_dir, &include_dir, &target_os);
        return;
    }

    if let Some(lib_dir) = pkg_config_lib_dir() {
        let include_dir = env::var("MKFF_INCLUDE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| repo_root.join("include"));
        link_external(&lib_dir, &include_dir, &target_os);
        return;
    }

    if let Some(lib_dir) = find_build_tree_lib_dir(&repo_root, &target_os) {
        let include_dir = env::var("MKFF_INCLUDE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| repo_root.join("include"));
        link_external(&lib_dir, &include_dir, &target_os);
        return;
    }

    panic!("{}", missing_native_error(&target_os));
}

fn repo_root_from_manifest(manifest_dir: &Path) -> PathBuf {
    manifest_dir
        .parent() // bindings/rust
        .and_then(Path::parent) // bindings
        .and_then(Path::parent) // repo root
        .expect("bindings/rust/mkff-sys is expected to live three directories below the repo root")
        .to_path_buf()
}

fn emit_rerun_directives(repo_root: &Path) {
    println!("cargo:rerun-if-env-changed=MKFF_LIB_DIR");
    println!("cargo:rerun-if-env-changed=MKFF_INCLUDE_DIR");
    println!("cargo:rerun-if-env-changed=CARGO_TARGET_DIR");

    // Watch sources and CMake inputs. Do not watch native build output dirs.
    for rel in [
        "CMakeLists.txt",
        "CMakePresets.json",
        "include/mkff",
        "src/common",
        "src/core",
        "src/codecs",
        "src/demux",
        "src/platform",
        "src/cli/CMakeLists.txt",
        "tests/CMakeLists.txt",
    ] {
        println!("cargo:rerun-if-changed={}", repo_root.join(rel).display());
    }
}

#[cfg(feature = "bundled")]
fn build_and_link_bundled(repo_root: &Path, target_os: &str, target_env: &str) {
    let profile = match env::var("PROFILE").as_deref() {
        Ok("release") => "Release",
        _ => "Debug",
    };

    let hevc = cfg!(feature = "hevc");
    let hevc_sw = cfg!(feature = "hevc-software-fallback");

    let dst = cmake::Config::new(repo_root)
        .profile(profile)
        .define("MKFF_BUILD_CLI", "OFF")
        .define("MKFF_BUILD_TESTS", "OFF")
        .define("MKFF_BUILD_RUST_BINDINGS", "OFF")
        .define("MKFF_BUILD_EXAMPLES", "OFF")
        .define("BUILD_TESTING", "OFF")
        .define("MKFF_ENABLE_HEVC", if hevc { "ON" } else { "OFF" })
        .define(
            "MKFF_ENABLE_HEVC_SOFTWARE",
            if hevc_sw { "ON" } else { "OFF" },
        )
        .build();

    let lib_dir = dst.join("lib");
    let bin_dir = dst.join("bin");
    let include_dir = dst.join("include");

    validate_bundled_artifacts(&dst, target_os, target_env);

    emit_link_search(&lib_dir);
    emit_link_lib(target_os);
    emit_runtime_search(target_os, &lib_dir);

    if target_os == "windows" {
        copy_windows_runtime_dlls(&bin_dir, &lib_dir);
    }

    println!("cargo:root={}", dst.display());
    println!("cargo:lib_dir={}", lib_dir.display());
    println!("cargo:include_dir={}", include_dir.display());
    println!("cargo:bin_dir={}", bin_dir.display());
}

#[cfg(not(feature = "bundled"))]
fn build_and_link_bundled(_repo_root: &Path, _target_os: &str, _target_env: &str) {
    unreachable!("bundled feature disabled");
}

fn validate_bundled_artifacts(dst: &Path, target_os: &str, target_env: &str) {
    let lib_dir = dst.join("lib");
    let bin_dir = dst.join("bin");

    match target_os {
        "windows" => {
            let import_lib = lib_dir.join("MKFF.lib");
            if !import_lib.is_file() {
                panic!(
                    "bundled MKFF build did not produce {} (MSVC import library). \
                     Ensure a Windows MSVC toolchain is available (target_env={target_env}).",
                    import_lib.display()
                );
            }
            for name in ["MKFF.dll", "MKFF.Platform.Windows.dll"] {
                let in_bin = bin_dir.join(name);
                let in_lib = lib_dir.join(name);
                if !in_bin.is_file() && !in_lib.is_file() {
                    panic!(
                        "bundled MKFF build did not produce {name} under {} or {}",
                        bin_dir.display(),
                        lib_dir.display()
                    );
                }
            }
        }
        "linux" => {
            for (stem, ext) in [
                ("libmkff", "so"),
                ("libmkff_platform_linux", "so"),
            ] {
                if !shared_lib_present(&lib_dir, stem, ext) {
                    panic!(
                        "bundled MKFF build did not produce {stem}.{ext} under {}",
                        lib_dir.display()
                    );
                }
            }
        }
        "macos" => {
            for (stem, ext) in [
                ("libmkff", "dylib"),
                ("libmkff_platform_macos", "dylib"),
            ] {
                if !shared_lib_present(&lib_dir, stem, ext) {
                    panic!(
                        "bundled MKFF build did not produce {stem}.{ext} under {}",
                        lib_dir.display()
                    );
                }
            }
        }
        other => panic!("unsupported CARGO_CFG_TARGET_OS for bundled MKFF: {other}"),
    }
}

/// True if `stem.ext` exists, or a SONAME-versioned variant like `stem.ext.0`.
fn shared_lib_present(dir: &Path, stem: &str, ext: &str) -> bool {
    let exact = format!("{stem}.{ext}");
    if dir.join(&exact).is_file() {
        return true;
    }
    let versioned_prefix = format!("{stem}.{ext}.");
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        name == exact || name.starts_with(&versioned_prefix)
    })
}

fn link_external(lib_dir: &Path, include_dir: &Path, target_os: &str) {
    if !lib_dir.is_dir() {
        panic!(
            "MKFF_LIB_DIR does not exist or is not a directory: {}",
            lib_dir.display()
        );
    }

    emit_link_search(lib_dir);
    emit_link_lib(target_os);
    emit_runtime_search(target_os, lib_dir);

    if target_os == "windows" {
        copy_windows_runtime_dlls(lib_dir, lib_dir);
        // Also check sibling bin/ (cmake --install layout).
        if let Some(prefix) = lib_dir.parent() {
            let bin_dir = prefix.join("bin");
            if bin_dir.is_dir() {
                copy_windows_runtime_dlls(&bin_dir, lib_dir);
            }
        }
    }

    println!("cargo:lib_dir={}", lib_dir.display());
    println!("cargo:include_dir={}", include_dir.display());
}

fn emit_link_search(lib_dir: &Path) {
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
}

fn emit_link_lib(target_os: &str) {
    match target_os {
        "windows" => println!("cargo:rustc-link-lib=dylib=MKFF"),
        _ => println!("cargo:rustc-link-lib=dylib=mkff"),
    }
}

fn emit_runtime_search(target_os: &str, lib_dir: &Path) {
    match target_os {
        "linux" => {
            println!("cargo:rustc-link-arg=-Wl,-rpath,{}", lib_dir.display());
        }
        "macos" => {
            // Absolute rpath to the bundled/install lib dir; install names
            // use @rpath so both core and platform dylibs resolve together.
            println!("cargo:rustc-link-arg=-Wl,-rpath,{}", lib_dir.display());
        }
        "windows" => {
            // No rpath on Windows; DLLs are copied beside Cargo executables.
        }
        _ => {}
    }
}

fn copy_windows_runtime_dlls(primary_dir: &Path, fallback_dir: &Path) {
    let Some(profile_dir) = cargo_profile_dir() else {
        println!("cargo:warning=could not derive Cargo profile dir from OUT_DIR; Windows DLLs were not copied");
        return;
    };

    let dll_names = ["MKFF.dll", "MKFF.Platform.Windows.dll"];
    let dest_dirs = [
        profile_dir.clone(),
        profile_dir.join("deps"),
        profile_dir.join("examples"),
    ];

    for name in dll_names {
        let src = [primary_dir.join(name), fallback_dir.join(name)]
            .into_iter()
            .find(|p| p.is_file());
        let Some(src) = src else {
            println!("cargo:warning=bundled/runtime DLL not found for copy: {name}");
            continue;
        };

        for dest_dir in &dest_dirs {
            if let Err(err) = fs::create_dir_all(dest_dir) {
                println!(
                    "cargo:warning=failed to create {}: {err}",
                    dest_dir.display()
                );
                continue;
            }
            let dest = dest_dir.join(name);
            if let Err(err) = copy_if_different(&src, &dest) {
                println!(
                    "cargo:warning=failed to copy {} -> {}: {err}",
                    src.display(),
                    dest.display()
                );
            }
        }
    }
}

/// Derive `target/{profile}` or `target/{triple}/{profile}` from OUT_DIR
/// (preferred) or CARGO_TARGET_DIR + PROFILE/TARGET.
fn cargo_profile_dir() -> Option<PathBuf> {
    if let Ok(out_dir) = env::var("OUT_DIR") {
        // OUT_DIR = .../{profile}/build/{pkg}/out  or
        //           .../{triple}/{profile}/build/{pkg}/out
        let mut dir = PathBuf::from(out_dir);
        for _ in 0..3 {
            if !dir.pop() {
                break;
            }
        }
        if is_profile_dir_name(dir.file_name().and_then(|s| s.to_str())) {
            return Some(dir);
        }
    }

    let target_dir = env::var_os("CARGO_TARGET_DIR").map(PathBuf::from)?;
    let profile = env::var("PROFILE").ok()?;
    let target = env::var("TARGET").ok()?;
    let host = env::var("HOST").ok()?;

    if target == host {
        Some(target_dir.join(profile))
    } else {
        Some(target_dir.join(target).join(profile))
    }
}

fn is_profile_dir_name(name: Option<&str>) -> bool {
    matches!(
        name,
        Some("debug") | Some("release") | Some("bench") | Some("test")
    )
}

fn copy_if_different(src: &Path, dst: &Path) -> std::io::Result<()> {
    if dst.is_file() {
        let src_bytes = fs::read(src)?;
        let dst_bytes = fs::read(dst)?;
        if src_bytes == dst_bytes {
            return Ok(());
        }
    }
    fs::copy(src, dst)?;
    Ok(())
}

fn pkg_config_lib_dir() -> Option<PathBuf> {
    // Optional discovery only — never required for bundled builds, and
    // never assumed to exist on Windows/macOS.
    let output = std::process::Command::new("pkg-config")
        .args(["--libs-only-L", "mkff"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let flag = stdout.split_whitespace().find(|s| s.starts_with("-L"))?;
    Some(PathBuf::from(flag.trim_start_matches("-L")))
}

fn find_build_tree_lib_dir(repo_root: &Path, target_os: &str) -> Option<PathBuf> {
    let build_dir = repo_root.join("build");
    let entries = fs::read_dir(&build_dir).ok()?;
    for entry in entries.flatten() {
        let lib_dir = entry.path().join("lib");
        if library_present_in(&lib_dir, target_os) {
            return Some(lib_dir);
        }
        // Multi-config generators may nest under lib/<Config>.
        if let Ok(sub) = fs::read_dir(&lib_dir) {
            for sub_entry in sub.flatten() {
                let candidate = sub_entry.path();
                if library_present_in(&candidate, target_os) {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

fn library_present_in(lib_dir: &Path, target_os: &str) -> bool {
    match target_os {
        "windows" => {
            lib_dir.join("MKFF.lib").is_file()
                || lib_dir.join("mkff.lib").is_file()
                || lib_dir.join("MKFF.dll").is_file()
                || lib_dir.join("mkff.dll").is_file()
        }
        "macos" => shared_lib_present(lib_dir, "libmkff", "dylib"),
        _ => shared_lib_present(lib_dir, "libmkff", "so"),
    }
}

fn missing_native_error(target_os: &str) -> String {
    match target_os {
        "windows" => "could not locate MKFF (MKFF.lib / MKFF.dll). Enable the default \
             `bundled` feature, set MKFF_LIB_DIR to a directory containing the \
             import library, or build the C project first \
             (cmake --build --preset windows-msvc-debug)."
            .into(),
        "macos" => "could not locate libmkff.dylib. Enable the default `bundled` feature, \
             set MKFF_LIB_DIR, or build the C project first \
             (cmake --build --preset macos-clang-debug)."
            .into(),
        "linux" => "could not locate libmkff.so. Enable the default `bundled` feature, \
             set MKFF_LIB_DIR, install a system mkff package (pkg-config), or build \
             the C project first (cmake --build --preset linux-clang-debug)."
            .into(),
        other => format!(
            "could not locate a native MKFF library for target OS `{other}`. \
             Enable the default `bundled` feature or set MKFF_LIB_DIR."
        ),
    }
}

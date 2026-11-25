//! The Futureboard Visualizer (`apps/native/visualizer`): a C++/SDL3 app
//! built with CMake and staged beside the Studio as
//! `FutureboardVisualizer(.exe)`. It reads Studio's shared-memory visualizer
//! link at run time; nothing in the Studio links against it.
//!
//! The CMake build lives in `target/visualizer/<profile>` and is reused
//! between packages. SDL3 comes from an installed package or is built from
//! source by FetchContent; `SDL3_SOURCE_DIR` points FetchContent at a local
//! checkout for an offline build (the configured cache remembers it).

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

const APP_DIR: &str = "apps/native/visualizer";
const BUILD_DIR: &str = "target/visualizer";
const TARGET: &str = "FutureboardVisualizer";

/// Whether a package for `target_triple`, built on `host_triple`, carries the
/// visualizer: native (not cross) Windows and Linux builds. The CMake build
/// compiles for the machine it runs on.
pub fn applies_to(target_triple: &str, host_triple: &str) -> bool {
    target_triple == host_triple
        && (target_triple.contains("windows") || target_triple.contains("linux"))
}

/// The CMake configuration a Cargo profile maps to.
fn cmake_config(profile: &str) -> &'static str {
    if profile == "dev" {
        "RelWithDebInfo"
    } else {
        "Release"
    }
}

/// The file name the app is staged under on `target_triple`.
pub fn staged_name(target_triple: &str) -> &'static str {
    if target_triple.contains("windows") {
        "FutureboardVisualizer.exe"
    } else {
        "FutureboardVisualizer"
    }
}

fn cmake_program() -> PathBuf {
    if let Some(explicit) = std::env::var_os("CMAKE") {
        return PathBuf::from(explicit);
    }
    #[cfg(windows)]
    {
        let installed = Path::new(r"C:\Program Files\CMake\bin\cmake.exe");
        if Command::new("cmake").arg("--version").output().is_err() && installed.is_file() {
            return installed.to_path_buf();
        }
    }
    PathBuf::from("cmake")
}

fn run(command: &mut Command, what: &str) -> Result<()> {
    let status = command
        .status()
        .with_context(|| format!("failed to run CMake to {what} (install CMake 3.24+ or set CMAKE, or pass --no-visualizer)"))?;
    if !status.success() {
        bail!("CMake failed to {what} ({status})");
    }
    Ok(())
}

/// Configure (first time) and build the visualizer for `profile`; returns the
/// built executable.
pub fn build(workspace: &Path, profile: &str, target_triple: &str) -> Result<PathBuf> {
    let cmake = cmake_program();
    let source = workspace.join(APP_DIR);
    let build_dir = workspace.join(BUILD_DIR).join(profile);
    let config = cmake_config(profile);
    eprintln!(
        "[xtask] building the visualizer app (CMake {config}, {})",
        build_dir.display()
    );

    if !build_dir.join("CMakeCache.txt").is_file() {
        let mut configure = Command::new(&cmake);
        configure.arg("-S").arg(&source).arg("-B").arg(&build_dir);
        if target_triple.contains("windows") {
            if target_triple.starts_with("x86_64") {
                configure.args(["-A", "x64"]);
            }
        } else {
            configure.arg(format!("-DCMAKE_BUILD_TYPE={config}"));
        }
        if let Some(sdl) = std::env::var_os("SDL3_SOURCE_DIR") {
            let sdl = PathBuf::from(sdl);
            let sdl = sdl.to_string_lossy().replace('\\', "/");
            configure.arg(format!("-DFETCHCONTENT_SOURCE_DIR_SDL3={sdl}"));
        }
        run(&mut configure, "configure the visualizer")?;
    }

    let mut compile = Command::new(&cmake);
    compile.arg("--build").arg(&build_dir).args([
        "--config",
        config,
        "--parallel",
        "--target",
        TARGET,
    ]);
    run(&mut compile, "build the visualizer")?;

    // Multi-config generators (Visual Studio) put it under the config's
    // folder; single-config ones (Makefiles, Ninja) at the top.
    let name = staged_name(target_triple);
    [build_dir.join(config).join(name), build_dir.join(name)]
        .into_iter()
        .find(|path| path.is_file())
        .with_context(|| format!("CMake built no {name} under {}", build_dir.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_native_windows_and_linux_packages_carry_the_visualizer() {
        let win = "x86_64-pc-windows-msvc";
        let linux = "x86_64-unknown-linux-gnu";
        assert!(applies_to(win, win));
        assert!(applies_to(linux, linux));
        assert!(!applies_to("aarch64-apple-darwin", "aarch64-apple-darwin"));
        assert!(
            !applies_to("aarch64-pc-windows-msvc", win),
            "no cross builds"
        );
    }

    #[test]
    fn names_and_configs() {
        assert_eq!(
            staged_name("x86_64-pc-windows-msvc"),
            "FutureboardVisualizer.exe"
        );
        assert_eq!(
            staged_name("x86_64-unknown-linux-gnu"),
            "FutureboardVisualizer"
        );
        assert_eq!(cmake_config("release"), "Release");
        assert_eq!(cmake_config("dev"), "RelWithDebInfo");
    }
}

//! Platform vocabulary for packaging.
//!
//! Keeps every piece of platform-specific string mapping in one module so the
//! rest of the packager stays OS-agnostic. Adding a new target triple only
//! requires a row in [`platform_folder`]; nothing here shells out.

use std::path::Path;

use anyhow::{Context, Result, anyhow};

/// The one Cargo target directory the package, the workspace aliases
/// (`build-studio`, `check-studio`, ...) and the jam client share.
pub const TARGET_DIR: &str = "target/studio";

/// Features that compile the Professional code (licensing, ASIO, Score view)
/// into the package. They grant nothing by themselves: Studio installs the
/// Professional providers only for a verified licence, so an unlicensed
/// machine running this package gets the Community feature set.
pub const PROFESSIONAL_FEATURES: &str =
    "futureboard_native/professional,sphere_directaudioengine/asio";

/// Where the private Professional sources live. A public checkout does not
/// have them and builds the same package without the Professional code.
const PROFESSIONAL_SOURCES: &str = "crates/ExclusiveEdition/src";

/// Whether this package compiles the Professional code in.
///
/// There is one package, not two editions: the licence decides at runtime.
/// The code is included for Windows and Linux whenever this checkout carries
/// the private sources. Windows also compiles the ASIO host (`asio-sys`);
/// that toolchain is applied only for Windows targets. The licence client,
/// Score view, EULA, and licensed updater are the same code on both.
pub fn includes_professional(workspace: &Path, target_triple: &str) -> bool {
    professional_target(target_triple)
        && cfg!(professional_toolchain)
        && workspace.join(PROFESSIONAL_SOURCES).is_dir()
}

/// Targets that ship the Professional client. Linux matches the packaged
/// gnu/musl triples (`*-unknown-linux-*`), which keeps Android triples out.
fn professional_target(target_triple: &str) -> bool {
    target_triple.contains("windows") || target_triple.contains("unknown-linux")
}

/// `asio-sys` needs the Steinberg SDK and libclang. Linux compiles the same
/// Professional feature set; the ASIO host itself stays behind `cfg(windows)`.
pub fn requires_asio_toolchain(target_triple: &str) -> bool {
    target_triple.contains("windows")
}

/// Map a Cargo target triple to a short, readable output folder name.
///
/// Unknown triples never panic — they are normalized into a filesystem-safe
/// slug so a novel target still produces a sane directory under `out/`.
pub fn platform_folder(triple: &str) -> String {
    match triple {
        "x86_64-pc-windows-msvc" | "x86_64-pc-windows-gnu" => "windows-x64".to_string(),
        "aarch64-pc-windows-msvc" => "windows-arm64".to_string(),
        "x86_64-unknown-linux-gnu" | "x86_64-unknown-linux-musl" => "linux-x64".to_string(),
        "aarch64-unknown-linux-gnu" | "aarch64-unknown-linux-musl" => "linux-arm64".to_string(),
        "x86_64-apple-darwin" => "macos-x64".to_string(),
        "aarch64-apple-darwin" => "macos-arm64".to_string(),
        other => normalize_folder(other),
    }
}

/// The dynamic-library file extension (without dot) for a target triple's OS:
/// `dll` on Windows, `dylib` on Apple, `so` elsewhere.
pub fn dynamic_library_extension(triple: &str) -> &'static str {
    if triple.contains("windows") {
        "dll"
    } else if triple.contains("apple") || triple.contains("darwin") {
        "dylib"
    } else {
        "so"
    }
}

/// The dynamic-library file-name prefix for a target triple's OS: empty on
/// Windows, `lib` on Unix-likes (Linux/macOS).
pub fn dynamic_library_prefix(triple: &str) -> &'static str {
    if triple.contains("windows") {
        ""
    } else {
        "lib"
    }
}

/// The platform-correct dynamic-library file name for a crate `base_name`
/// (Cargo's `[lib] name`), e.g. `rodharerist` → `rodharerist.dll` /
/// `librodharerist.so` / `librodharerist.dylib`.
pub fn dynamic_library_file_name(base_name: &str, triple: &str) -> String {
    format!(
        "{}{}.{}",
        dynamic_library_prefix(triple),
        base_name,
        dynamic_library_extension(triple)
    )
}

/// Turn an arbitrary triple into a lower-case `[a-z0-9-]` slug so it is always
/// a valid directory name on every supported filesystem.
fn normalize_folder(triple: &str) -> String {
    let mut slug = String::with_capacity(triple.len());
    let mut prev_dash = false;
    for character in triple.chars() {
        let mapped = if character.is_ascii_alphanumeric() {
            character.to_ascii_lowercase()
        } else {
            '-'
        };
        if mapped == '-' {
            if prev_dash {
                continue;
            }
            prev_dash = true;
        } else {
            prev_dash = false;
        }
        slug.push(mapped);
    }
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        "unknown-target".to_string()
    } else {
        slug
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_triples_map_to_readable_names() {
        assert_eq!(platform_folder("x86_64-pc-windows-msvc"), "windows-x64");
        assert_eq!(platform_folder("aarch64-pc-windows-msvc"), "windows-arm64");
        assert_eq!(platform_folder("x86_64-unknown-linux-gnu"), "linux-x64");
        assert_eq!(platform_folder("aarch64-unknown-linux-gnu"), "linux-arm64");
        assert_eq!(platform_folder("x86_64-apple-darwin"), "macos-x64");
        assert_eq!(platform_folder("aarch64-apple-darwin"), "macos-arm64");
    }

    #[test]
    fn unknown_triple_is_normalized_not_panicked() {
        assert_eq!(
            platform_folder("riscv64gc-unknown-linux-gnu"),
            "riscv64gc-unknown-linux-gnu"
        );
        // Collapses runs of non-alphanumerics and trims edges.
        assert_eq!(platform_folder("Weird__Target!!"), "weird-target");
        assert_eq!(platform_folder("///"), "unknown-target");
    }

    #[test]
    fn dynamic_library_naming_matches_platform() {
        assert_eq!(dynamic_library_extension("x86_64-pc-windows-msvc"), "dll");
        assert_eq!(dynamic_library_extension("x86_64-unknown-linux-gnu"), "so");
        assert_eq!(dynamic_library_extension("aarch64-apple-darwin"), "dylib");

        assert_eq!(dynamic_library_prefix("x86_64-pc-windows-msvc"), "");
        assert_eq!(dynamic_library_prefix("x86_64-unknown-linux-gnu"), "lib");
        assert_eq!(dynamic_library_prefix("aarch64-apple-darwin"), "lib");

        assert_eq!(
            dynamic_library_file_name("rodharerist", "x86_64-pc-windows-msvc"),
            "rodharerist.dll"
        );
        assert_eq!(
            dynamic_library_file_name("rodharerist", "x86_64-unknown-linux-gnu"),
            "librodharerist.so"
        );
        assert_eq!(
            dynamic_library_file_name("rodharerist", "aarch64-apple-darwin"),
            "librodharerist.dylib"
        );
    }

    #[test]
    fn professional_code_follows_the_private_checkout_on_windows_and_linux() {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let compiled_in =
            cfg!(professional_toolchain) && workspace.join(PROFESSIONAL_SOURCES).is_dir();

        assert_eq!(
            includes_professional(&workspace, "x86_64-pc-windows-msvc"),
            compiled_in
        );
        assert_eq!(
            includes_professional(&workspace, "x86_64-unknown-linux-gnu"),
            compiled_in
        );
        assert_eq!(
            includes_professional(&workspace, "aarch64-unknown-linux-musl"),
            compiled_in
        );
        assert!(!includes_professional(&workspace, "aarch64-apple-darwin"));
        assert!(!includes_professional(&workspace, "aarch64-linux-android"));
        assert!(requires_asio_toolchain("x86_64-pc-windows-msvc"));
        assert!(!requires_asio_toolchain("x86_64-unknown-linux-gnu"));
    }

    #[test]
    fn a_checkout_without_the_private_sources_builds_without_professional_code() {
        let empty = tempfile::tempdir().unwrap();
        assert!(!includes_professional(
            empty.path(),
            "x86_64-pc-windows-msvc"
        ));
        assert!(!includes_professional(
            empty.path(),
            "x86_64-unknown-linux-gnu"
        ));
    }
}

/// The host target triple, parsed from `rustc -vV`'s `host:` line.
pub fn host_target() -> Result<String> {
    let output =
        std::process::Command::new(std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string()))
            .arg("-vV")
            .output()
            .context("failed to run rustc -vV")?;
    let text = String::from_utf8(output.stdout).context("rustc -vV output was not UTF-8")?;
    text.lines()
        .find_map(|line| line.strip_prefix("host: "))
        .map(|host| host.trim().to_string())
        .ok_or_else(|| anyhow!("rustc -vV did not report a host triple"))
}

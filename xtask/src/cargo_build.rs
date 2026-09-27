//! Drive Cargo and discover the real executable paths.
//!
//! We never assume `target/<profile>/FutureboardNative.exe`. Cargo is run with
//! `--message-format=json-render-diagnostics`; the emitted `compiler-artifact`
//! messages tell us exactly where each binary landed, which keeps working across
//! custom target triples, profiles and per-edition target directories.
//!
//! The application ships more than one executable: at runtime
//! `FutureboardNative.exe` spawns two sidecar processes it resolves *next to
//! itself* — the out-of-process plugin/editor host (`FutureboardPluginHostX64`)
//! and the isolated plugin scanner (`FutureboardPluginScanner`). Both are
//! `[[bin]]` targets of the `sphere-plugin-host` package. The distributable also
//! carries the APAK installer and CLI tools under `bin/`, so all required
//! executables are built in one invocation and discovered from Cargo messages.

use std::collections::BTreeMap;
use std::fs;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};
use cargo_metadata::{Artifact, Message};

use crate::platform::{Edition, host_target};
use crate::toolchain;

/// The Futureboard workspace root (xtask lives at `<root>/xtask`).
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask must live under the workspace root")
        .to_path_buf()
}

/// Whether the build targets Windows, where ASIO exists at all.
fn target_is_windows(target: Option<&str>) -> bool {
    target
        .map(|triple| triple.contains("windows"))
        .unwrap_or(cfg!(target_os = "windows"))
}

/// The application package and its primary binary.
pub const APP_PACKAGE: &str = "futureboard_native";
pub const APP_BINARY: &str = "FutureboardNative";

/// Package that owns the runtime sidecar executables.
const SIDECAR_PACKAGE: &str = "sphere-plugin-host";
const CEF_HELPER_PACKAGE: &str = "futureboard_cef_helper";
const APAK_PACKAGE: &str = "apakinstaller";
pub const CEF_HELPER_BINARY: &str = "futureboard_cef_helper";

/// Sidecar binaries `FutureboardNative` spawns from its own directory. These are
/// separate `[[bin]]` targets, so building the app package alone does not
/// produce them — they must be requested explicitly.
pub const SIDECAR_BINARIES: &[&str] = &["FutureboardPluginHostX64", "FutureboardPluginScanner"];

/// APAK tools shipped under the staged application's `bin/` directory.
pub const APAK_BINARIES: &[&str] = &["apakinstaller", "apak", "makeapak"];

/// Feature flags that unlock the sidecar `[[bin]]` targets (their
/// `required-features`).
const SIDECAR_FEATURES: &[&str] = &[
    "sphere-plugin-host/plugin-host-bin",
    "sphere-plugin-host/plugin-scanner-bin",
];

/// Result of a successful build: every executable Cargo produced that the
/// package needs.
#[derive(Debug, Clone)]
pub struct BuildOutput {
    /// Absolute path to the primary application binary.
    pub app_executable: PathBuf,
    /// Absolute paths to the runtime sidecar executables, in the order of
    /// [`SIDECAR_BINARIES`].
    pub sidecar_executables: Vec<PathBuf>,
    /// Dedicated CEF subprocess entry point, built only for macOS packages.
    pub cef_helper_executable: Option<PathBuf>,
    /// APAK GUI/CLI executables, in the order of [`APAK_BINARIES`].
    pub apak_executables: Vec<PathBuf>,
}

/// Build the application and its sidecars for the requested profile / target /
/// edition, returning the actual executable paths parsed from Cargo's output.
pub fn build(
    profile: &str,
    target: Option<&str>,
    edition: Edition,
    cef_path: Option<&Path>,
) -> Result<BuildOutput> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let workspace = workspace_root();
    let target_dir = workspace.join(edition.target_dir());
    let target_triple = match target {
        Some(target) => target.to_owned(),
        None => host_target()?,
    };
    let target_is_macos = target_triple.ends_with("apple-darwin");

    let mut command = Command::new(&cargo);
    command
        .current_dir(&workspace)
        // Build scripts do not receive Cargo's `--target-dir` CLI value in
        // `CARGO_TARGET_DIR`. Crashpad's prebuilt setup uses that environment
        // variable when copying crashpad_handler, so keep both paths explicit
        // and identical or the handler lands under apps/native/target instead
        // of the edition target used by the package.
        .env("CARGO_TARGET_DIR", &target_dir)
        .arg("build")
        .arg("--message-format=json-render-diagnostics")
        .args(["--package", APP_PACKAGE])
        .args(["--package", SIDECAR_PACKAGE])
        .args(["--package", APAK_PACKAGE])
        .args(["--bin", APP_BINARY])
        .args(["--profile", profile])
        .arg("--target-dir")
        .arg(&target_dir);

    if target_triple.contains("windows") && target_triple.contains("msvc") {
        // The pinned CEF SDK defaults its wrapper to /MT, while the prebuilt
        // Crashpad libraries use /MD. Give cmake-rs a target-specific toolchain
        // so the CEF wrapper uses the same CRT and can link into the DAW.
        let cmake_toolchain = workspace.join("xtask/cef-msvc-dynamic-runtime.cmake");
        reset_stale_cef_wrapper(&cargo, &workspace, &target_dir, target, profile)?;
        command.env(
            format!("CMAKE_TOOLCHAIN_FILE_{}", target_triple.replace('-', "_")),
            &cmake_toolchain,
        );
    }

    if let Some(shim_dir) = macos_cross_cxx_shim(&target_dir, &target_triple)? {
        reset_stale_crashpad_wrapper(&cargo, &workspace, &target_dir, &target_triple, profile)?;
        let path = std::env::var_os("PATH").unwrap_or_default();
        let joined = std::env::join_paths(
            std::iter::once(shim_dir.clone()).chain(std::env::split_paths(&path)),
        )
        .context("failed to prepend the macOS cross c++ shim to PATH")?;
        command.env("PATH", joined);
        eprintln!(
            "[xtask] cross-building {target_triple}: bare `c++` gets the target arch via {}",
            shim_dir.display()
        );
    }

    for bin in SIDECAR_BINARIES {
        command.args(["--bin", bin]);
    }
    for bin in APAK_BINARIES {
        command.args(["--bin", bin]);
    }
    if target_is_macos {
        command
            .args(["--package", CEF_HELPER_PACKAGE])
            .args(["--bin", CEF_HELPER_BINARY]);
    }
    if let Some(target) = target {
        command.args(["--target", target]);
    }
    if let Some(cef_path) = cef_path {
        // Always pass an absolute, target-matched distribution. cef-dll-sys
        // treats relative CEF_PATH values as version roots and may append its
        // own version/platform components.
        command.env("CEF_PATH", cef_path);
    }

    // The Professional build compiles `asio-sys`, which needs the Steinberg SDK
    // and libclang. Resolving them here — rather than letting `asio-sys` fetch
    // the SDK into %TEMP% — is what stops a half-extracted download from being
    // compiled against forever after.
    if edition == Edition::Professional && target_is_windows(target) {
        toolchain::prepare_professional(&workspace_root())?.apply(&mut command);
    }

    // Merge the edition features with the sidecar bin features into one
    // `--features`, so a single build graph unifies shared-dependency features
    // (no rebuild thrash between the app and its sidecars).
    let features = merged_features(edition);
    if !features.is_empty() {
        command.args(["--features", &features]);
    }

    eprintln!(
        "[xtask] building {APP_BINARY} + sidecars + APAK tools (edition={edition}, profile={profile}, target={})",
        target.unwrap_or("<host>")
    );

    // Artifacts arrive as JSON on stdout; let rendered diagnostics/progress
    // stream to the inherited stderr so the developer sees a normal build.
    command.stdout(Stdio::piped()).stderr(Stdio::inherit());

    let mut child = command
        .spawn()
        .with_context(|| format!("failed to spawn `{cargo} build`"))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("cargo produced no stdout stream"))?;

    let mut executables: BTreeMap<String, PathBuf> = BTreeMap::new();
    for message in Message::parse_stream(BufReader::new(stdout)) {
        let message = message.context("failed to parse a cargo JSON message")?;
        if let Message::CompilerArtifact(artifact) = message {
            if let Some((name, path)) = wanted_executable(&artifact) {
                executables.insert(name, path);
            }
        }
    }

    let status = child.wait().context("failed to wait on cargo build")?;
    if !status.success() {
        bail!("cargo build failed with {status}");
    }

    let app_executable = executables.remove(APP_BINARY).ok_or_else(|| {
        anyhow!(
            "cargo build succeeded but emitted no executable artifact for `{APP_BINARY}`; \
             is the `{APP_PACKAGE}` package still producing a `[[bin]]` named `{APP_BINARY}`?"
        )
    })?;

    let mut sidecar_executables = Vec::with_capacity(SIDECAR_BINARIES.len());
    for bin in SIDECAR_BINARIES {
        let path = executables.remove(*bin).ok_or_else(|| {
            anyhow!(
                "cargo build succeeded but emitted no executable artifact for sidecar `{bin}`; \
                 the app spawns it at runtime and it must ship in the package"
            )
        })?;
        sidecar_executables.push(path);
    }
    let cef_helper_executable = if target_is_macos {
        Some(executables.remove(CEF_HELPER_BINARY).ok_or_else(|| {
            anyhow!(
                "cargo build succeeded but emitted no executable artifact for macOS CEF helper \
                 `{CEF_HELPER_BINARY}`"
            )
        })?)
    } else {
        None
    };
    let mut apak_executables = Vec::with_capacity(APAK_BINARIES.len());
    for bin in APAK_BINARIES {
        let path = executables.remove(*bin).ok_or_else(|| {
            anyhow!(
                "cargo build succeeded but emitted no executable artifact for APAK tool `{bin}`"
            )
        })?;
        apak_executables.push(path);
    }

    Ok(BuildOutput {
        app_executable,
        sidecar_executables,
        cef_helper_executable,
        apak_executables,
    })
}

/// Remove only the generated CEF wrapper build when an older CMake cache still
/// has the static MSVC CRT selected or GNU-style compiler/archiver tools cached.
/// This keeps both the Crashpad/CEF runtime and the MSVC command-line tools
/// aligned after developers update the wrapper toolchain.
/// Mach-O architecture name for a macOS target triple.
fn macos_arch(triple: &str) -> Option<&'static str> {
    if !triple.ends_with("apple-darwin") {
        return None;
    }
    if triple.starts_with("x86_64") {
        Some("x86_64")
    } else if triple.starts_with("aarch64") || triple.starts_with("arm64") {
        Some("arm64")
    } else {
        None
    }
}

/// The `c++` wrapper written for a macOS cross build. It adds `-arch <arch>`
/// unless the caller already chose an architecture (`cc` and GN always do).
fn cross_cxx_shim_script(arch: &str) -> String {
    format!(
        "#!/bin/bash\n\
         # Written by xtask. crashpad-rs-sys builds crashpad_wrapper.cc with bare\n\
         # `c++` and no architecture flag, so a cross build gets a host-arch\n\
         # wrapper next to target-arch Crashpad libraries and fails to link.\n\
         for arg in \"$@\"; do\n\
         \x20 case \"$arg\" in -arch|-target|--target=*) exec /usr/bin/c++ \"$@\" ;; esac\n\
         done\n\
         exec /usr/bin/c++ -arch {arch} \"$@\"\n"
    )
}

/// On a macOS host building for the *other* macOS architecture, write a `c++`
/// shim that adds the target `-arch` and return its directory for `PATH`.
///
/// Crashpad publishes a prebuilt archive for arm64 only, so the Intel slice of
/// a universal build compiles Crashpad from source. crashpad-rs-sys (v0.2.7)
/// builds the Crashpad libraries for the right CPU through GN, but compiles its
/// own `crashpad_wrapper.cc` with plain `c++` and no `-arch`, i.e. for the host.
/// The x86_64 link then fails on `_crashpad_client_new`. The shim leaves every
/// compile that names an architecture alone.
fn macos_cross_cxx_shim(target_dir: &Path, target_triple: &str) -> Result<Option<PathBuf>> {
    let Some(target_arch) = macos_arch(target_triple) else {
        return Ok(None);
    };
    let host = host_target()?;
    let Some(host_arch) = macos_arch(&host) else {
        return Ok(None);
    };
    if host_arch == target_arch {
        return Ok(None);
    }
    let dir = target_dir.join("xtask-cxx-shim").join(target_triple);
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create c++ shim directory {}", dir.display()))?;
    let shim = dir.join("c++");
    let script = cross_cxx_shim_script(target_arch);
    if fs::read_to_string(&shim).ok().as_deref() != Some(script.as_str()) {
        fs::write(&shim, &script)
            .with_context(|| format!("failed to write c++ shim {}", shim.display()))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&shim, fs::Permissions::from_mode(0o755))
            .with_context(|| format!("failed to mark c++ shim executable {}", shim.display()))?;
    }
    Ok(Some(dir))
}

/// A Crashpad wrapper cached by a cross build that predates the shim holds
/// the host architecture, and Cargo will not rerun the build script for a
/// `PATH` change. Clean the crate when the cached object is the wrong arch.
fn reset_stale_crashpad_wrapper(
    cargo: &str,
    workspace: &Path,
    target_dir: &Path,
    target_triple: &str,
    profile: &str,
) -> Result<()> {
    let Some(arch) = macos_arch(target_triple) else {
        return Ok(());
    };
    let profile_dir = if profile == "dev" { "debug" } else { profile };
    let build_dir = target_dir
        .join(target_triple)
        .join(profile_dir)
        .join("build");
    let Ok(entries) = fs::read_dir(&build_dir) else {
        return Ok(());
    };
    let stale = entries.flatten().any(|entry| {
        entry
            .file_name()
            .to_string_lossy()
            .starts_with("crashpad-rs-sys-")
            && {
                let object = entry.path().join("out").join("crashpad_wrapper.o");
                object.is_file()
                    && Command::new("lipo")
                        .arg("-archs")
                        .arg(&object)
                        .output()
                        .ok()
                        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
                        .is_some_and(|archs| !archs.split_whitespace().any(|a| a == arch))
            }
    });
    if !stale {
        return Ok(());
    }
    eprintln!(
        "[xtask] cached crashpad_wrapper.o in {} is not {arch}; rebuilding crashpad-rs-sys",
        build_dir.display()
    );
    let status = Command::new(cargo)
        .current_dir(workspace)
        .args([
            "clean",
            "--package",
            "crashpad-rs-sys",
            "--target",
            target_triple,
        ])
        .args(["--profile", profile, "--target-dir"])
        .arg(target_dir)
        .status()
        .context("failed to reset the stale Crashpad wrapper build")?;
    if !status.success() {
        bail!("cargo clean for the stale Crashpad wrapper failed with {status}");
    }
    Ok(())
}

fn reset_stale_cef_wrapper(
    cargo: &str,
    workspace: &Path,
    target_dir: &Path,
    target: Option<&str>,
    profile: &str,
) -> Result<()> {
    let profile_dir = match target {
        Some(target) => target_dir.join(target).join(profile),
        None => target_dir.join(profile),
    };
    let build_dir = profile_dir.join("build");
    let entries = match fs::read_dir(&build_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to inspect CEF build cache at {}",
                    build_dir.display()
                )
            });
        }
    };

    let mut stale = false;
    let mut cef_build_dirs = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| {
            format!(
                "failed to inspect CEF build cache at {}",
                build_dir.display()
            )
        })?;
        if !entry.file_type()?.is_dir()
            || !entry
                .file_name()
                .to_string_lossy()
                .starts_with("cef-dll-sys-")
        {
            continue;
        }

        let cef_build_dir = entry.path();
        cef_build_dirs.push(cef_build_dir.clone());
        let cache = cef_build_dir
            .join("out")
            .join("build")
            .join("CMakeCache.txt");
        if !cache.is_file() {
            continue;
        }
        let contents = fs::read_to_string(&cache)
            .with_context(|| format!("failed to read CEF CMake cache {}", cache.display()))?;
        if cef_wrapper_cache_is_stale(&contents) {
            stale = true;
        }
    }

    if !stale {
        return Ok(());
    }

    eprintln!(
        "[xtask] resetting stale CEF wrapper cache in {} to align the compiler and Crashpad CRT (/MD)",
        profile_dir.display()
    );
    let status = Command::new(cargo)
        .current_dir(workspace)
        .args(["clean", "--package", "cef-dll-sys", "--target-dir"])
        .arg(target_dir)
        .status()
        .with_context(|| "failed to reset the stale CEF wrapper build")?;
    if !status.success() {
        bail!("cargo clean for the stale CEF wrapper failed with {status}");
    }

    // `cargo clean --package` can leave a CMake/Ninja build tree behind when
    // the build script failed before Cargo recorded its output. Remove only
    // direct `cef-dll-sys-*` directories under this edition/profile cache so
    // the next CMake configure regenerates compiler and archive rules.
    for cef_build_dir in cef_build_dirs {
        if cef_build_dir.parent() != Some(build_dir.as_path())
            || !cef_build_dir
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("cef-dll-sys-"))
        {
            bail!(
                "refusing to remove CEF build cache outside {}: {}",
                build_dir.display(),
                cef_build_dir.display()
            );
        }
        match fs::remove_dir_all(&cef_build_dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to remove stale CEF build cache {}",
                        cef_build_dir.display()
                    )
                });
            }
        }
    }
    Ok(())
}

fn cmake_cache_value<'a>(contents: &'a str, key: &str) -> Option<&'a str> {
    contents.lines().find_map(|line| {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return None;
        }
        let (name, value) = line.split_once('=')?;
        let name = name.split_once(':')?.0;
        (name == key).then_some(value.trim())
    })
}

fn cef_wrapper_cache_is_stale(contents: &str) -> bool {
    let crt = cmake_cache_value(contents, "CMAKE_MSVC_RUNTIME_LIBRARY");
    let cef_crt = cmake_cache_value(contents, "CEF_RUNTIME_LIBRARY_FLAG");
    let c_compiler = cmake_cache_value(contents, "CMAKE_C_COMPILER");
    let cxx_compiler = cmake_cache_value(contents, "CMAKE_CXX_COMPILER");
    let archiver = cmake_cache_value(contents, "CMAKE_AR");
    let c_archiver = cmake_cache_value(contents, "CMAKE_C_COMPILER_AR");
    let cxx_archiver = cmake_cache_value(contents, "CMAKE_CXX_COMPILER_AR");
    crt != Some("MultiThreadedDLL")
        || cef_crt != Some("/MD")
        || c_compiler.is_some_and(is_gnu_style_clang)
        || cxx_compiler.is_some_and(is_gnu_style_clang)
        || archiver.is_some_and(is_gnu_llvm_archiver)
        || c_archiver.is_some_and(is_gnu_llvm_archiver)
        || cxx_archiver.is_some_and(is_gnu_llvm_archiver)
}

fn is_gnu_style_clang(compiler: &str) -> bool {
    let name = compiler
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(compiler)
        .to_ascii_lowercase();
    matches!(
        name.as_str(),
        "clang" | "clang.exe" | "clang++" | "clang++.exe"
    )
}

fn is_gnu_llvm_archiver(archiver: &str) -> bool {
    let name = archiver
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(archiver)
        .to_ascii_lowercase();
    matches!(name.as_str(), "llvm-ar" | "llvm-ar.exe")
}

/// Comma-joined `--features` value combining edition features (if any) with the
/// sidecar bin features.
fn merged_features(edition: Edition) -> String {
    let mut features: Vec<&str> = Vec::new();
    if let Some(edition_features) = edition.cargo_features() {
        features.push(edition_features);
    }
    features.extend_from_slice(SIDECAR_FEATURES);
    features.join(",")
}

/// Return `(binary_name, executable_path)` if this artifact is one of the
/// executables we asked Cargo to build.
fn wanted_executable(artifact: &Artifact) -> Option<(String, PathBuf)> {
    let name = artifact.target.name.as_str();
    let is_wanted = (name == APP_BINARY
        || name == CEF_HELPER_BINARY
        || SIDECAR_BINARIES.contains(&name)
        || APAK_BINARIES.contains(&name))
        && artifact
            .target
            .kind
            .iter()
            .any(|kind| kind.as_str() == "bin");
    if !is_wanted {
        return None;
    }
    artifact
        .executable
        .as_ref()
        .map(|path| (name.to_string(), PathBuf::from(path.as_std_path())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_arch_names_match_lipo() {
        assert_eq!(macos_arch("x86_64-apple-darwin"), Some("x86_64"));
        assert_eq!(macos_arch("aarch64-apple-darwin"), Some("arm64"));
        assert_eq!(macos_arch("x86_64-pc-windows-msvc"), None);
    }

    #[test]
    fn cross_cxx_shim_adds_arch_only_when_unset() {
        let script = cross_cxx_shim_script("x86_64");
        assert!(script.starts_with("#!/bin/bash\n"));
        assert!(script.contains("exec /usr/bin/c++ -arch x86_64 \"$@\""));
        assert!(script.contains("-arch|-target|--target=*) exec /usr/bin/c++ \"$@\""));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn cross_cxx_shim_compiles_for_the_target_arch() {
        let dir = std::env::temp_dir().join(format!("xtask-shim-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let shim = dir.join("c++");
        fs::write(&shim, cross_cxx_shim_script("x86_64")).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let source = dir.join("t.cc");
        fs::write(&source, "int f() { return 1; }\n").unwrap();
        let plain = dir.join("plain.o");
        let explicit = dir.join("explicit.o");
        assert!(
            Command::new(&shim)
                .args(["-c", "-o"])
                .arg(&plain)
                .arg(&source)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new(&shim)
                .args(["--target=arm64-apple-macosx", "-c", "-o"])
                .arg(&explicit)
                .arg(&source)
                .status()
                .unwrap()
                .success()
        );
        let archs = |path: &Path| {
            String::from_utf8(
                Command::new("lipo")
                    .arg("-archs")
                    .arg(path)
                    .output()
                    .unwrap()
                    .stdout,
            )
            .unwrap()
            .trim()
            .to_string()
        };
        assert_eq!(archs(&plain), "x86_64");
        assert_eq!(archs(&explicit), "arm64");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn community_features_are_sidecar_only() {
        assert_eq!(
            merged_features(Edition::Community),
            "sphere-plugin-host/plugin-host-bin,sphere-plugin-host/plugin-scanner-bin"
        );
    }

    #[test]
    fn professional_features_prepend_edition_flags() {
        assert_eq!(
            merged_features(Edition::Professional),
            "futureboard_native/professional,sphere_directaudioengine/asio,\
sphere-plugin-host/plugin-host-bin,sphere-plugin-host/plugin-scanner-bin"
        );
    }

    #[test]
    fn stale_cef_cache_detects_gnu_style_clang_despite_matching_crt() {
        let cache = "CMAKE_MSVC_RUNTIME_LIBRARY:STRING=MultiThreadedDLL\n\
CEF_RUNTIME_LIBRARY_FLAG:STRING=/MD\n\
CMAKE_C_COMPILER:FILEPATH=W:/LLVM/bin/clang.exe\n\
CMAKE_CXX_COMPILER:FILEPATH=W:/LLVM/bin/clang++.exe\n";
        assert!(cef_wrapper_cache_is_stale(cache));
    }

    #[test]
    fn current_cef_cache_accepts_clang_cl() {
        let cache = "CMAKE_MSVC_RUNTIME_LIBRARY:STRING=MultiThreadedDLL\n\
CEF_RUNTIME_LIBRARY_FLAG:STRING=/MD\n\
CMAKE_C_COMPILER:FILEPATH=W:/LLVM/bin/clang-cl.exe\n\
CMAKE_CXX_COMPILER:FILEPATH=W:/LLVM/bin/clang-cl.exe\n\
CMAKE_AR:FILEPATH=W:/LLVM/bin/llvm-lib.exe\n\
CMAKE_C_COMPILER_AR:FILEPATH=W:/LLVM/bin/llvm-lib.exe\n\
CMAKE_CXX_COMPILER_AR:FILEPATH=W:/LLVM/bin/llvm-lib.exe\n";
        assert!(!cef_wrapper_cache_is_stale(cache));
    }

    #[test]
    fn stale_cef_cache_detects_gnu_llvm_archiver() {
        let cache = "CMAKE_MSVC_RUNTIME_LIBRARY:STRING=MultiThreadedDLL\n\
CEF_RUNTIME_LIBRARY_FLAG:STRING=/MD\n\
CMAKE_C_COMPILER:FILEPATH=W:/LLVM/bin/clang-cl.exe\n\
CMAKE_CXX_COMPILER:FILEPATH=W:/LLVM/bin/clang-cl.exe\n\
CMAKE_AR:FILEPATH=W:/LLVM/bin/llvm-lib.exe\n\
CMAKE_C_COMPILER_AR:FILEPATH=W:/LLVM/bin/llvm-ar.exe\n\
CMAKE_CXX_COMPILER_AR:FILEPATH=W:/LLVM/bin/llvm-ar.exe\n";
        assert!(cef_wrapper_cache_is_stale(cache));
    }
}

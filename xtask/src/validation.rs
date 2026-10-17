//! Validate a staged application before it is published.
//!
//! Every check runs against the staging directory only. If any check fails the
//! caller aborts before the atomic swap, so the previously published package
//! stays intact.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::cef::{LOCALES_DIR, required_runtime_files};
use crate::platform::dynamic_library_extension;
use crate::staging::{BIN_DIR, BUILD_INFO_FILE, PLUGINS_DIR, RESOURCES_DIR, SYMBOLS_DIR};

/// File extensions that must never appear in a distributable package — Cargo
/// intermediates and dev-only artifacts. `.lib`/`.pdb` are still allowed only
/// under the opt-in `symbols/` directory (handled separately).
const FORBIDDEN_EXTENSIONS: &[&str] = &["exp", "d", "rlib", "rmeta"];

/// `.pdb`/`.lib` are legitimate only inside `symbols/`; flagged anywhere else.
const SYMBOL_ONLY_EXTENSIONS: &[&str] = &["pdb", "lib"];

/// Directory names that indicate the Cargo target tree leaked into staging.
const FORBIDDEN_DIRS: &[&str] = &["incremental", "deps", "examples", "build"];

/// Directory names that must never be created by the packager: CEF ships flat
/// (never in a `CEF/` subdir) and the embedded-UI design forbids a deployed
/// `PluginUI/` folder.
const BANNED_LAYOUT_DIRS: &[&str] = &["CEF", "PluginUI"];

/// Everything a validation pass needs to know about the staged package.
pub struct ValidationInputs<'a> {
    pub staging_dir: &'a Path,
    pub binary_name: &'a str,
    /// Dynamic libraries the binary loads from its own directory at startup
    /// (the Studio engine library).
    pub libraries: &'a [String],
    pub sidecars: &'a [String],
    /// Required executables staged at relative paths such as `bin/apak.exe`.
    pub tools: &'a [String],
    pub symbols_enabled: bool,
    /// Effective target triple (drives dynamic-library and CEF file names).
    pub triple: &'a str,
    /// Whether the shared CEF runtime was staged into this package.
    pub cef_staged: bool,
}

/// Run all pre-publish checks on the staged package.
pub fn validate_staging(inputs: &ValidationInputs<'_>) -> Result<()> {
    let staging_dir = inputs.staging_dir;
    check_executable(staging_dir, inputs.binary_name)?;
    for library in inputs.libraries {
        check_executable(staging_dir, library)
            .with_context(|| format!("required library `{library}` missing or empty"))?;
    }
    for sidecar in inputs.sidecars {
        check_executable(staging_dir, sidecar)
            .with_context(|| format!("required sidecar `{sidecar}` missing or empty"))?;
    }
    for tool in inputs.tools {
        check_executable(staging_dir, tool)
            .with_context(|| format!("required staged tool `{tool}` missing or empty"))?;
    }
    check_required_dirs(staging_dir)?;
    check_build_info(staging_dir)?;
    check_no_forbidden_artifacts(staging_dir, inputs.symbols_enabled)?;
    check_no_banned_layout_dirs(staging_dir)?;
    check_plugin_naming(staging_dir, inputs.triple)?;
    if inputs.cef_staged {
        check_cef_layout(staging_dir, inputs.triple)?;
    }
    check_all_within_root(staging_dir)?;
    Ok(())
}

/// No `CEF/` or `PluginUI/` directory exists anywhere in the package.
fn check_no_banned_layout_dirs(staging_dir: &Path) -> Result<()> {
    for entry in walk(staging_dir)? {
        if !entry.is_dir() {
            continue;
        }
        if let Some(name) = entry.file_name().and_then(|n| n.to_str()) {
            if BANNED_LAYOUT_DIRS.contains(&name) {
                bail!(
                    "banned layout directory `{name}/` found in package: {} \
                     (CEF must ship flat; embedded UI must not be deployed as PluginUI/)",
                    entry.display()
                );
            }
        }
    }
    Ok(())
}

/// Every file under `Plugins/` carries the platform-correct dynamic-library
/// extension. An empty `Plugins/` is allowed (a package may ship no plugins yet).
fn check_plugin_naming(staging_dir: &Path, triple: &str) -> Result<()> {
    let plugins = staging_dir.join(PLUGINS_DIR);
    if !plugins.is_dir() {
        return Ok(());
    }
    let expected = dynamic_library_extension(triple);
    for entry in fs::read_dir(&plugins)? {
        let path = entry?.path();
        if !path.is_file() {
            continue;
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default();
        if ext != expected {
            bail!(
                "plugin `{}` has extension `.{ext}` but target {triple} requires `.{expected}`",
                path.display()
            );
        }
    }
    Ok(())
}

/// The shared CEF runtime is complete: flat with `locales/` on Windows/Linux,
/// or as a framework with `.lproj` locales on macOS.
fn check_cef_layout(staging_dir: &Path, triple: &str) -> Result<()> {
    for required in required_runtime_files(triple) {
        let path = staging_dir.join(&required);
        if !path.is_file() {
            bail!(
                "required CEF runtime file missing beside binary: {}",
                path.display()
            );
        }
    }
    let locales = if triple.ends_with("apple-darwin") {
        staging_dir.join("Chromium Embedded Framework.framework/Resources")
    } else {
        staging_dir.join(LOCALES_DIR)
    };
    let populated = if triple.ends_with("apple-darwin") {
        fs::read_dir(&locales)
            .map(|entries| {
                entries.filter_map(Result::ok).any(|entry| {
                    entry.path().extension().is_some_and(|ext| ext == "lproj")
                        && entry.path().join("locale.pak").is_file()
                })
            })
            .unwrap_or(false)
    } else {
        locales.is_dir()
            && fs::read_dir(&locales)
                .map(|mut entries| entries.next().is_some())
                .unwrap_or(false)
    };
    if !populated {
        bail!(
            "required CEF `locales/` directory missing or empty: {}",
            locales.display()
        );
    }
    Ok(())
}

/// A required executable exists at its staged relative path and is non-empty.
fn check_executable(staging_dir: &Path, binary_name: &str) -> Result<()> {
    let exe = staging_dir.join(binary_name);
    let meta = fs::metadata(&exe)
        .with_context(|| format!("staged executable missing: {}", exe.display()))?;
    if !meta.is_file() {
        bail!("staged executable is not a file: {}", exe.display());
    }
    if meta.len() == 0 {
        bail!("staged executable is empty: {}", exe.display());
    }
    Ok(())
}

/// The expected application directories exist.
fn check_required_dirs(staging_dir: &Path) -> Result<()> {
    for dir in [BIN_DIR, PLUGINS_DIR, RESOURCES_DIR] {
        let path = staging_dir.join(dir);
        if !path.is_dir() {
            bail!(
                "required directory missing from package: {}",
                path.display()
            );
        }
    }
    Ok(())
}

/// `build-info.json` exists and parses as JSON.
fn check_build_info(staging_dir: &Path) -> Result<()> {
    let path = staging_dir.join(BUILD_INFO_FILE);
    let text = fs::read_to_string(&path)
        .with_context(|| format!("build metadata missing: {}", path.display()))?;
    serde_json::from_str::<serde_json::Value>(&text)
        .with_context(|| format!("build metadata is not valid JSON: {}", path.display()))?;
    Ok(())
}

/// No Cargo intermediates were copied into the package. The optional `symbols/`
/// directory (populated only via `--symbols`) is skipped so its `.pdb` is not
/// flagged.
fn check_no_forbidden_artifacts(staging_dir: &Path, symbols_enabled: bool) -> Result<()> {
    for entry in walk(staging_dir)? {
        if symbols_enabled && entry.starts_with(staging_dir.join(SYMBOLS_DIR)) {
            continue;
        }
        let name = entry
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if is_forbidden_artifact(name, entry.is_dir()) {
            bail!(
                "forbidden Cargo/dev artifact found in package: {}",
                entry.display()
            );
        }
    }
    Ok(())
}

/// Whether a staged entry name is a forbidden intermediate artifact.
///
/// `.pdb`/`.lib` are treated as forbidden here (they are only allowed under
/// `symbols/`, which the caller excludes before calling this).
pub fn is_forbidden_artifact(name: &str, is_dir: bool) -> bool {
    if is_dir {
        return FORBIDDEN_DIRS.contains(&name);
    }
    match name.rsplit_once('.') {
        Some((_, ext)) => {
            let ext = ext.to_ascii_lowercase();
            FORBIDDEN_EXTENSIONS.contains(&ext.as_str())
                || SYMBOL_ONLY_EXTENSIONS.contains(&ext.as_str())
        }
        None => false,
    }
}

/// Every staged path resolves to somewhere inside the staging root — a final
/// guard against symlink/traversal escapes.
fn check_all_within_root(staging_dir: &Path) -> Result<()> {
    let root = staging_dir
        .canonicalize()
        .with_context(|| format!("cannot canonicalize staging root {}", staging_dir.display()))?;
    for entry in walk(staging_dir)? {
        let resolved = entry
            .canonicalize()
            .with_context(|| format!("cannot canonicalize {}", entry.display()))?;
        if !resolved.starts_with(&root) {
            bail!("staged path escapes package root: {}", entry.display());
        }
    }
    Ok(())
}

/// Recursively list every file and directory under `root` (excluding `root`).
fn walk(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir)
            .with_context(|| format!("cannot read directory {}", dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path.clone());
            }
            out.push(path);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::staging;

    const TRIPLE: &str = "x86_64-pc-windows-msvc";

    /// Build a minimally valid staged package for the happy-path checks.
    fn valid_package(dir: &Path) {
        fs::write(dir.join("FutureboardNative.exe"), b"MZ binary").unwrap();
        fs::create_dir_all(dir.join(BIN_DIR)).unwrap();
        fs::create_dir_all(dir.join(PLUGINS_DIR)).unwrap();
        fs::create_dir_all(dir.join(RESOURCES_DIR)).unwrap();
        fs::write(dir.join(BUILD_INFO_FILE), "{\"schemaVersion\":1}").unwrap();
    }

    /// Stage the flat CEF runtime files a Windows package must carry.
    fn add_cef(dir: &Path) {
        for f in required_runtime_files(TRIPLE) {
            fs::write(dir.join(f), b"x").unwrap();
        }
        fs::create_dir_all(dir.join(LOCALES_DIR)).unwrap();
        fs::write(dir.join(LOCALES_DIR).join("en-US.pak"), b"x").unwrap();
    }

    fn inputs<'a>(
        dir: &'a Path,
        binary: &'a str,
        sidecars: &'a [String],
        symbols: bool,
        cef: bool,
    ) -> ValidationInputs<'a> {
        ValidationInputs {
            staging_dir: dir,
            binary_name: binary,
            libraries: &[],
            sidecars,
            tools: &[],
            symbols_enabled: symbols,
            triple: TRIPLE,
            cef_staged: cef,
        }
    }

    #[test]
    fn forbidden_artifact_detection() {
        assert!(is_forbidden_artifact("libcore.rlib", false));
        assert!(is_forbidden_artifact("FutureboardNative.pdb", false));
        assert!(is_forbidden_artifact("thing.d", false));
        assert!(is_forbidden_artifact("mod.rmeta", false));
        assert!(is_forbidden_artifact("rodharerist.lib", false));
        assert!(is_forbidden_artifact("deps", true));
        assert!(is_forbidden_artifact("incremental", true));

        assert!(!is_forbidden_artifact("FutureboardNative.exe", false));
        assert!(!is_forbidden_artifact("onnxruntime.dll", false));
        assert!(!is_forbidden_artifact("Plugins", true));
        assert!(!is_forbidden_artifact("build-info.json", false));
    }

    #[test]
    fn valid_package_passes() {
        let temp = tempfile::tempdir().unwrap();
        valid_package(temp.path());
        validate_staging(&inputs(
            temp.path(),
            "FutureboardNative.exe",
            &[],
            false,
            false,
        ))
        .unwrap();
    }

    #[test]
    fn valid_package_with_sidecars_passes() {
        let temp = tempfile::tempdir().unwrap();
        valid_package(temp.path());
        fs::write(temp.path().join("FutureboardPluginHostX64.exe"), b"MZ").unwrap();
        fs::write(temp.path().join("FutureboardPluginScanner.exe"), b"MZ").unwrap();
        let sidecars = [
            "FutureboardPluginHostX64.exe".to_string(),
            "FutureboardPluginScanner.exe".to_string(),
        ];
        validate_staging(&inputs(
            temp.path(),
            "FutureboardNative.exe",
            &sidecars,
            false,
            false,
        ))
        .unwrap();
    }

    #[test]
    fn required_apak_tools_must_exist_under_bin() {
        let temp = tempfile::tempdir().unwrap();
        valid_package(temp.path());
        fs::write(temp.path().join("bin/apakinstaller.exe"), b"MZ").unwrap();
        fs::write(temp.path().join("bin/apak.exe"), b"MZ").unwrap();
        let tools = [
            "bin/apakinstaller.exe".to_string(),
            "bin/apak.exe".to_string(),
            "bin/makeapak.exe".to_string(),
        ];
        let inputs = ValidationInputs {
            staging_dir: temp.path(),
            binary_name: "FutureboardNative.exe",
            libraries: &[],
            sidecars: &[],
            tools: &tools,
            symbols_enabled: false,
            triple: TRIPLE,
            cef_staged: false,
        };

        assert!(validate_staging(&inputs).is_err());
        fs::write(temp.path().join("bin/makeapak.exe"), b"MZ").unwrap();
        validate_staging(&inputs).unwrap();
    }

    #[test]
    fn missing_engine_library_fails() {
        let temp = tempfile::tempdir().unwrap();
        valid_package(temp.path());
        let libraries = ["FutureboardEngine_x64.dll".to_string()];
        let inputs = ValidationInputs {
            libraries: &libraries,
            ..inputs(temp.path(), "FutureboardNative.exe", &[], false, false)
        };

        assert!(validate_staging(&inputs).is_err());
        fs::write(temp.path().join("FutureboardEngine_x64.dll"), b"MZ").unwrap();
        validate_staging(&inputs).unwrap();
    }

    #[test]
    fn missing_required_sidecar_fails() {
        let temp = tempfile::tempdir().unwrap();
        valid_package(temp.path());
        fs::write(temp.path().join("FutureboardPluginHostX64.exe"), b"MZ").unwrap();
        let sidecars = [
            "FutureboardPluginHostX64.exe".to_string(),
            "FutureboardPluginScanner.exe".to_string(),
        ];
        assert!(
            validate_staging(&inputs(
                temp.path(),
                "FutureboardNative.exe",
                &sidecars,
                false,
                false
            ))
            .is_err()
        );
    }

    #[test]
    fn empty_executable_fails() {
        let temp = tempfile::tempdir().unwrap();
        valid_package(temp.path());
        fs::write(temp.path().join("FutureboardNative.exe"), b"").unwrap();
        assert!(
            validate_staging(&inputs(
                temp.path(),
                "FutureboardNative.exe",
                &[],
                false,
                false
            ))
            .is_err()
        );
    }

    #[test]
    fn missing_required_dir_fails() {
        let temp = tempfile::tempdir().unwrap();
        valid_package(temp.path());
        fs::remove_dir_all(temp.path().join(PLUGINS_DIR)).unwrap();
        assert!(
            validate_staging(&inputs(
                temp.path(),
                "FutureboardNative.exe",
                &[],
                false,
                false
            ))
            .is_err()
        );
    }

    #[test]
    fn invalid_build_info_fails() {
        let temp = tempfile::tempdir().unwrap();
        valid_package(temp.path());
        fs::write(temp.path().join(BUILD_INFO_FILE), "not json {").unwrap();
        assert!(
            validate_staging(&inputs(
                temp.path(),
                "FutureboardNative.exe",
                &[],
                false,
                false
            ))
            .is_err()
        );
    }

    #[test]
    fn leaked_cargo_artifact_fails() {
        let temp = tempfile::tempdir().unwrap();
        valid_package(temp.path());
        fs::write(temp.path().join("libjunk.rlib"), b"junk").unwrap();
        assert!(
            validate_staging(&inputs(
                temp.path(),
                "FutureboardNative.exe",
                &[],
                false,
                false
            ))
            .is_err()
        );
    }

    #[test]
    fn symbols_pdb_allowed_only_under_symbols_dir() {
        let temp = tempfile::tempdir().unwrap();
        valid_package(temp.path());
        let sym = temp.path().join(staging::SYMBOLS_DIR);
        fs::create_dir_all(&sym).unwrap();
        fs::write(sym.join("FutureboardNative.pdb"), b"pdb").unwrap();
        validate_staging(&inputs(
            temp.path(),
            "FutureboardNative.exe",
            &[],
            true,
            false,
        ))
        .unwrap();
        assert!(
            validate_staging(&inputs(
                temp.path(),
                "FutureboardNative.exe",
                &[],
                false,
                false
            ))
            .is_err()
        );
    }

    #[test]
    fn cef_subdirectory_layout_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        valid_package(temp.path());
        fs::create_dir_all(temp.path().join("CEF")).unwrap();
        fs::write(temp.path().join("CEF/libcef.dll"), b"x").unwrap();
        assert!(
            validate_staging(&inputs(
                temp.path(),
                "FutureboardNative.exe",
                &[],
                false,
                false
            ))
            .is_err()
        );
    }

    #[test]
    fn plugin_ui_deployment_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        valid_package(temp.path());
        fs::create_dir_all(temp.path().join("PluginUI")).unwrap();
        assert!(
            validate_staging(&inputs(
                temp.path(),
                "FutureboardNative.exe",
                &[],
                false,
                false
            ))
            .is_err()
        );
    }

    #[test]
    fn flat_cef_layout_passes_and_incomplete_fails() {
        let temp = tempfile::tempdir().unwrap();
        valid_package(temp.path());
        add_cef(temp.path());
        // Flat, complete CEF beside the binary passes.
        validate_staging(&inputs(
            temp.path(),
            "FutureboardNative.exe",
            &[],
            false,
            true,
        ))
        .unwrap();
        // Missing a required CEF file fails when CEF is expected.
        fs::remove_file(temp.path().join("icudtl.dat")).unwrap();
        assert!(
            validate_staging(&inputs(
                temp.path(),
                "FutureboardNative.exe",
                &[],
                false,
                true
            ))
            .is_err()
        );
    }

    #[test]
    fn wrong_plugin_extension_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        valid_package(temp.path());
        // A `.so` under Plugins/ is wrong for a Windows target.
        fs::write(
            temp.path().join(PLUGINS_DIR).join("librodharerist.so"),
            b"x",
        )
        .unwrap();
        assert!(
            validate_staging(&inputs(
                temp.path(),
                "FutureboardNative.exe",
                &[],
                false,
                false
            ))
            .is_err()
        );
    }

    #[test]
    fn correct_plugin_extension_passes() {
        let temp = tempfile::tempdir().unwrap();
        valid_package(temp.path());
        fs::write(temp.path().join(PLUGINS_DIR).join("rodharerist.dll"), b"MZ").unwrap();
        validate_staging(&inputs(
            temp.path(),
            "FutureboardNative.exe",
            &[],
            false,
            false,
        ))
        .unwrap();
    }
}

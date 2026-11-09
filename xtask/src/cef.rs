//! Stage the shared CEF runtime beside the application binary.
//!
//! CEF is a single, shared runtime — never one copy per plugin and never a
//! `CEF/` subdirectory. Windows and Linux use a flat runtime with `locales/`;
//! macOS keeps `Chromium Embedded Framework.framework` intact so the app
//! bundler can place it under `Contents/Frameworks`.
//!
//! The file list is taken from the CEF distribution the repository already uses.
//! The installer uses the versioned layout, while the previous flat
//! `<workspace>/build/cef` layout remains supported for existing workspaces. We
//! do not download another runtime.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sphere_webview::{CEF_SHORT_VERSION, CefTarget};

use crate::platform::dynamic_library_extension;
use crate::staging::copy_into;

/// Directory (relative to `build/cef`) holding the runtime binaries.
const RELEASE_DIR: &str = "Release";
/// Directory (relative to `build/cef`) holding the `.pak`/`icu` resources.
const RESOURCES_DIR: &str = "Resources";
const MAC_FRAMEWORK_DIR: &str = "Chromium Embedded Framework.framework";
/// The one CEF subdirectory that must keep its name at the app root.
pub const LOCALES_DIR: &str = "locales";

/// Dev-only or loader artifacts in `Release/` that must never ship.
const SKIP_EXTENSIONS: &[&str] = &["lib", "exp", "pdb", "ilk"];
const SKIP_NAMES: &[&str] = &[
    "bootstrap.exe",
    "bootstrapc.exe",
    sphere_webview::INSTALL_MANIFEST,
];

/// Result of staging the CEF runtime.
#[derive(Debug, Clone, Default)]
pub struct CefStageReport {
    /// Flat runtime files copied beside the binary (file names only).
    pub runtime_files: Vec<String>,
    /// Number of locale `.pak` files copied into `locales/`.
    pub locale_count: usize,
}

/// Candidate locations, in preference order, for the target CEF distribution.
///
/// Keeping this in one place makes package diagnostics report the exact
/// absolute paths that were checked.
pub fn cef_dist_candidates(workspace_root: &Path, triple: &str) -> Vec<PathBuf> {
    let Some(platform_dir) = platform_dir(triple) else {
        return Vec::new();
    };
    let cef_root = workspace_root.join("build").join("cef");
    vec![
        cef_root.join(CEF_SHORT_VERSION).join(platform_dir),
        // Compatibility with the original installer and existing developer
        // workspaces. This can be removed after those installations migrate.
        cef_root,
    ]
}

/// Locate a complete, pinned CEF distribution for `triple`.
///
/// The versioned installer layout is preferred. A valid flat `build/cef`
/// distribution is accepted as a compatibility fallback.
pub fn locate_cef_dist(workspace_root: &Path, triple: &str) -> Option<PathBuf> {
    cef_dist_candidates(workspace_root, triple)
        .into_iter()
        .find(|dist| validate_pinned_distribution(dist).is_ok())
}

/// Locate the pinned distribution and repair the duplicated layout produced
/// when `cef-rs` is given a distribution directory as though it were a version
/// root:
///
/// `<version>/<platform>/<version>/<platform>`.
///
/// The promotion is atomic at the directory level. The valid inner
/// distribution is moved aside first, the invalid wrapper is renamed to a
/// backup, and the inner distribution is promoted into the canonical path.
/// This runs only in xtask's package preflight, never from a Cargo build script.
pub fn prepare_cef_dist(workspace_root: &Path, triple: &str) -> Result<Option<PathBuf>> {
    let Some(platform_dir) = platform_dir(triple) else {
        return Ok(None);
    };
    let cef_root = workspace_root.join("build").join("cef");
    let canonical = cef_root.join(CEF_SHORT_VERSION).join(platform_dir);
    if validate_pinned_distribution(&canonical).is_ok() {
        return Ok(Some(canonical));
    }

    let duplicated = canonical.join(CEF_SHORT_VERSION).join(platform_dir);
    if validate_pinned_distribution(&duplicated).is_ok() {
        promote_duplicated_distribution(&canonical, &duplicated)?;
        return Ok(Some(canonical));
    }

    Ok(locate_cef_dist(workspace_root, triple))
}

fn promote_duplicated_distribution(canonical: &Path, duplicated: &Path) -> Result<()> {
    let parent = canonical
        .parent()
        .context("canonical CEF distribution must have a version directory")?;
    let directory_name = canonical
        .file_name()
        .and_then(|name| name.to_str())
        .context("canonical CEF distribution must have a UTF-8 directory name")?;
    let process_id = std::process::id();
    let promoted = parent.join(format!(".{directory_name}.promote-{process_id}"));
    let backup = parent.join(format!(".{directory_name}.layout-backup-{process_id}"));
    if promoted.exists() || backup.exists() {
        bail!(
            "CEF layout repair scratch path already exists: {} or {}",
            promoted.display(),
            backup.display()
        );
    }

    fs::rename(duplicated, &promoted).with_context(|| {
        format!(
            "cannot move duplicated CEF distribution {} to {}",
            duplicated.display(),
            promoted.display()
        )
    })?;
    if let Err(error) = fs::rename(canonical, &backup) {
        let _ = fs::rename(&promoted, duplicated);
        return Err(error).with_context(|| {
            format!(
                "cannot move invalid CEF wrapper {} to {}",
                canonical.display(),
                backup.display()
            )
        });
    }
    if let Err(error) = fs::rename(&promoted, canonical) {
        let _ = fs::rename(&backup, canonical);
        return Err(error).with_context(|| {
            format!(
                "cannot promote CEF distribution {} to {}",
                promoted.display(),
                canonical.display()
            )
        });
    }

    validate_pinned_distribution(canonical)
        .context("promoted CEF distribution failed validation")?;
    fs::remove_dir_all(&backup).with_context(|| {
        format!(
            "cannot remove duplicated CEF layout backup {}",
            backup.display()
        )
    })?;
    eprintln!(
        "[xtask] repaired duplicated CEF layout: {}",
        canonical.display()
    );
    Ok(())
}

/// Refuse to package a CEF tree that is not byte-for-byte the pinned install.
///
/// Only the versioned `build/cef/<version>/<platform>` layout written by
/// `install_cef` carries the install manifest, so only it can be checked file
/// by file. The legacy flat `build/cef` fallback has no such record and is
/// refused rather than packaged unchecked.
pub fn verify_dist_for_packaging(workspace_root: &Path, dist: &Path, triple: &str) -> Result<()> {
    let target = CefTarget::from_target_triple(triple)
        .with_context(|| format!("no pinned CEF distribution for `{triple}`"))?;
    let canonical = sphere_webview::cef_path(workspace_root, target);
    if dist != canonical {
        bail!(
            "CEF at {} is not the verified install layout ({}). Packaging does not \
             ship an unchecked runtime; run `cargo run -p SphereWebView --example \
             install_cef --features installer` (add `--target {triple}` when cross \
             packaging), or pass --no-cef",
            dist.display(),
            canonical.display()
        );
    }
    sphere_webview::verify_installed_distribution(dist, target).with_context(|| {
        format!(
            "CEF at {} does not match the pinned install; reinstall it with \
             `cargo run -p SphereWebView --example install_cef --features installer -- --force`",
            dist.display()
        )
    })
}

fn platform_dir(triple: &str) -> Option<&'static str> {
    CefTarget::from_target_triple(triple)
        .ok()
        .map(CefTarget::directory_name)
}

fn validate_pinned_distribution(dist: &Path) -> Result<()> {
    let version_header = dist.join("include/cef_version.h");
    let version = fs::read_to_string(&version_header)
        .with_context(|| format!("cannot read {}", version_header.display()))?;
    let expected = format!("#define CEF_VERSION \"{CEF_SHORT_VERSION}+");
    if !version.contains(&expected) {
        bail!(
            "CEF SDK version mismatch in {}: expected {CEF_SHORT_VERSION}",
            version_header.display()
        );
    }

    let archive_path = dist.join("archive.json");
    let archive = fs::read_to_string(&archive_path)
        .with_context(|| format!("cannot read {}", archive_path.display()))?;
    let expected_archive = format!("cef_binary_{CEF_SHORT_VERSION}+");
    if !archive.contains(&expected_archive) {
        bail!(
            "CEF archive mismatch in {}: expected {CEF_SHORT_VERSION}",
            archive_path.display()
        );
    }
    Ok(())
}

/// The runtime files the flat layout must contain for `triple` — used both to
/// assert staging succeeded and by package validation. Resource files are
/// platform-independent; only the shared library name differs.
pub fn required_runtime_files(triple: &str) -> Vec<String> {
    if triple.ends_with("apple-darwin") {
        return [
            "Chromium Embedded Framework",
            "Resources/resources.pak",
            "Resources/chrome_100_percent.pak",
            "Resources/chrome_200_percent.pak",
            "Resources/icudtl.dat",
        ]
        .into_iter()
        .map(|path| format!("{MAC_FRAMEWORK_DIR}/{path}"))
        .collect();
    }

    let mut files = vec![
        "resources.pak".to_string(),
        "chrome_100_percent.pak".to_string(),
        "chrome_200_percent.pak".to_string(),
        "icudtl.dat".to_string(),
        "v8_context_snapshot.bin".to_string(),
    ];
    match dynamic_library_extension(triple) {
        "dll" => {
            files.push("libcef.dll".to_string());
            files.push("chrome_elf.dll".to_string());
        }
        "dylib" => unreachable!("macOS framework paths are handled above"),
        _ => files.push("libcef.so".to_string()),
    }
    files
}

/// Copy the CEF runtime flat into `staging_dir`.
///
/// * every non-dev file directly under `Release/` → staging root
/// * every file directly under `Resources/` (paks, `icudtl.dat`) → staging root
/// * `Resources/locales/*` → `staging_dir/locales/`
///
/// Fails if a required file (see [`required_runtime_files`]) is absent afterward,
/// so a broken/partial CEF distribution is caught before publishing.
pub fn stage_cef(staging_dir: &Path, dist_dir: &Path, triple: &str) -> Result<CefStageReport> {
    let mut report = CefStageReport::default();

    validate_pinned_distribution(dist_dir)?;
    if triple.ends_with("apple-darwin") {
        let root_framework = dist_dir.join(MAC_FRAMEWORK_DIR);
        let release_framework = dist_dir.join(RELEASE_DIR).join(MAC_FRAMEWORK_DIR);
        let framework = if root_framework.is_dir() {
            root_framework
        } else {
            release_framework
        };
        copy_macos_framework(&framework, staging_dir, &mut report)?;
        report.runtime_files.sort();
        report.runtime_files.dedup();
        validate_staged_runtime(staging_dir, dist_dir, triple, report.locale_count)?;
        return Ok(report);
    }

    let release = if dist_dir.join(RELEASE_DIR).is_dir() {
        dist_dir.join(RELEASE_DIR)
    } else {
        dist_dir.to_path_buf()
    };
    for name in flat_files(&release)? {
        if should_skip(&name) {
            continue;
        }
        copy_into(staging_dir, &name, &release.join(&name))?;
        report.runtime_files.push(name);
    }

    let resources = if dist_dir.join(RESOURCES_DIR).is_dir() {
        dist_dir.join(RESOURCES_DIR)
    } else {
        dist_dir.to_path_buf()
    };
    if resources.is_dir() {
        for name in flat_files(&resources)? {
            if should_skip(&name) {
                continue;
            }
            copy_into(staging_dir, &name, &resources.join(&name))?;
            report.runtime_files.push(name);
        }
        // locales/ is the single required CEF subdirectory.
        let locales = resources.join(LOCALES_DIR);
        if locales.is_dir() {
            for name in flat_files(&locales)? {
                let relative = format!("{LOCALES_DIR}/{name}");
                copy_into(staging_dir, &relative, &locales.join(&name))?;
                report.locale_count += 1;
            }
        }
    }

    report.runtime_files.sort();
    report.runtime_files.dedup();

    validate_staged_runtime(staging_dir, dist_dir, triple, report.locale_count)?;
    Ok(report)
}

fn validate_staged_runtime(
    staging_dir: &Path,
    dist_dir: &Path,
    triple: &str,
    locale_count: usize,
) -> Result<()> {
    for required in required_runtime_files(triple) {
        if !staging_dir.join(&required).is_file() {
            bail!(
                "CEF runtime is incomplete: `{required}` was not staged from {}",
                dist_dir.display()
            );
        }
    }
    if locale_count == 0 {
        bail!(
            "CEF locale resources are empty or missing under {}",
            dist_dir.display()
        );
    }
    Ok(())
}

fn copy_macos_framework(
    framework: &Path,
    staging_dir: &Path,
    report: &mut CefStageReport,
) -> Result<()> {
    if !framework.is_dir() {
        bail!("CEF macOS framework is missing: {}", framework.display());
    }
    copy_macos_framework_dir(framework, framework, staging_dir, report)
}

fn copy_macos_framework_dir(
    framework: &Path,
    directory: &Path,
    staging_dir: &Path,
    report: &mut CefStageReport,
) -> Result<()> {
    for entry in fs::read_dir(directory).with_context(|| {
        format!(
            "cannot read CEF framework directory {}",
            directory.display()
        )
    })? {
        let entry = entry?;
        let source = entry.path();
        if source.is_dir() {
            copy_macos_framework_dir(framework, &source, staging_dir, report)?;
            continue;
        }
        if !source.is_file() {
            continue;
        }
        let relative = source
            .strip_prefix(framework)
            .expect("framework entry must remain under framework root");
        let relative_string = Path::new(MAC_FRAMEWORK_DIR)
            .join(relative)
            .to_string_lossy()
            .replace('\\', "/");
        copy_into(staging_dir, &relative_string, &source)?;
        if source.file_name().is_some_and(|name| name == "locale.pak")
            || relative
                .components()
                .any(|component| component.as_os_str() == LOCALES_DIR)
        {
            report.locale_count += 1;
        }
        report.runtime_files.push(relative_string);
    }
    Ok(())
}

/// File names (not directories) directly inside `dir`.
fn flat_files(dir: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in
        fs::read_dir(dir).with_context(|| format!("cannot read CEF directory {}", dir.display()))?
    {
        let entry = entry?;
        if entry.path().is_file() {
            if let Some(name) = entry.file_name().to_str() {
                names.push(name.to_string());
            }
        }
    }
    names.sort();
    Ok(names)
}

/// Whether a `Release/` entry is a dev-only/loader artifact to leave behind.
fn should_skip(name: &str) -> bool {
    if SKIP_NAMES.contains(&name) {
        return true;
    }
    match name.rsplit_once('.') {
        Some((_, ext)) => SKIP_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal fake `build/cef` distribution mirroring the real Windows
    /// layout (Release/ + Resources/ + Resources/locales/).
    fn fake_windows_dist(root: &Path) -> PathBuf {
        let dist = root.join("build/cef");
        let release = dist.join(RELEASE_DIR);
        fs::create_dir_all(&release).unwrap();
        fs::create_dir_all(dist.join("include")).unwrap();
        fs::write(
            dist.join("include/cef_version.h"),
            "#define CEF_VERSION \"150.0.11+gtest+chromium-150.0.0.0\"\n",
        )
        .unwrap();
        fs::write(
            dist.join("archive.json"),
            "{\"name\":\"cef_binary_150.0.11+gtest_windows64.tar.bz2\"}",
        )
        .unwrap();
        for f in [
            "libcef.dll",
            "chrome_elf.dll",
            "v8_context_snapshot.bin",
            "libGLESv2.dll",
            // dev-only / loader artifacts that must be filtered out:
            "libcef.lib",
            "bootstrap.exe",
        ] {
            fs::write(release.join(f), b"x").unwrap();
        }
        let resources = dist.join(RESOURCES_DIR);
        fs::create_dir_all(resources.join(LOCALES_DIR)).unwrap();
        for f in [
            "resources.pak",
            "chrome_100_percent.pak",
            "chrome_200_percent.pak",
            "icudtl.dat",
        ] {
            fs::write(resources.join(f), b"x").unwrap();
        }
        fs::write(resources.join(LOCALES_DIR).join("en-US.pak"), b"x").unwrap();
        fs::write(resources.join(LOCALES_DIR).join("fr.pak"), b"x").unwrap();
        dist
    }

    fn copy_test_distribution(source: &Path, destination: &Path) {
        fs::create_dir_all(destination).unwrap();
        for entry in fs::read_dir(source).unwrap() {
            let entry = entry.unwrap();
            let source_path = entry.path();
            let destination_path = destination.join(entry.file_name());
            if source_path.is_dir() {
                copy_test_distribution(&source_path, &destination_path);
            } else {
                fs::copy(source_path, destination_path).unwrap();
            }
        }
    }

    #[test]
    fn locate_returns_none_without_release() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("build/cef")).unwrap();
        assert!(locate_cef_dist(temp.path(), "x86_64-pc-windows-msvc").is_none());
    }

    #[test]
    fn locate_accepts_existing_flat_distribution() {
        let temp = tempfile::tempdir().unwrap();
        let dist = fake_windows_dist(temp.path());
        assert_eq!(
            locate_cef_dist(temp.path(), "x86_64-pc-windows-msvc"),
            Some(dist)
        );
    }

    #[test]
    fn locate_prefers_versioned_distribution() {
        let temp = tempfile::tempdir().unwrap();
        fake_windows_dist(temp.path());
        let source = fake_windows_dist(&temp.path().join("versioned-source"));
        let versioned = temp
            .path()
            .join("build/cef")
            .join(CEF_SHORT_VERSION)
            .join("cef_windows_x86_64");
        fs::create_dir_all(versioned.parent().unwrap()).unwrap();
        copy_test_distribution(&source, &versioned);
        assert_eq!(
            locate_cef_dist(temp.path(), "x86_64-pc-windows-msvc"),
            Some(versioned)
        );
    }

    #[test]
    fn prepare_repairs_duplicated_version_and_platform_layout() {
        let temp = tempfile::tempdir().unwrap();
        let source = fake_windows_dist(&temp.path().join("source"));
        let canonical = temp
            .path()
            .join("build/cef")
            .join(CEF_SHORT_VERSION)
            .join("cef_windows_x86_64");
        let duplicated = canonical.join(CEF_SHORT_VERSION).join("cef_windows_x86_64");
        copy_test_distribution(&source, &duplicated);

        assert_eq!(
            prepare_cef_dist(temp.path(), "x86_64-pc-windows-msvc").unwrap(),
            Some(canonical.clone())
        );
        assert!(canonical.join("archive.json").is_file());
        assert!(canonical.join("include/cef_version.h").is_file());
        assert!(!canonical.join(CEF_SHORT_VERSION).exists());
    }

    #[test]
    fn rejects_distribution_version_mismatch() {
        let temp = tempfile::tempdir().unwrap();
        let dist = fake_windows_dist(temp.path());
        fs::write(
            dist.join("include/cef_version.h"),
            "#define CEF_VERSION \"150.0.14+gwrong+chromium-150.0.0.0\"\n",
        )
        .unwrap();
        assert!(validate_pinned_distribution(&dist).is_err());
    }

    #[test]
    fn stages_runtime_flat_with_locales_subdir() {
        let temp = tempfile::tempdir().unwrap();
        let dist = fake_windows_dist(temp.path());
        let staging = temp.path().join("stage");
        fs::create_dir_all(&staging).unwrap();

        let report = stage_cef(&staging, &dist, "x86_64-pc-windows-msvc").unwrap();

        // Flat beside the binary — never a CEF/ subdir.
        assert!(staging.join("libcef.dll").is_file());
        assert!(staging.join("chrome_elf.dll").is_file());
        assert!(staging.join("v8_context_snapshot.bin").is_file());
        assert!(staging.join("resources.pak").is_file());
        assert!(staging.join("icudtl.dat").is_file());
        assert!(!staging.join("CEF").exists());

        // locales/ preserved as the one required subdirectory.
        assert!(staging.join("locales/en-US.pak").is_file());
        assert_eq!(report.locale_count, 2);

        // Dev-only / loader artifacts filtered out.
        assert!(!staging.join("libcef.lib").exists());
        assert!(!staging.join("bootstrap.exe").exists());
    }

    #[test]
    fn flat_versioned_distribution_never_stages_import_libraries() {
        let temp = tempfile::tempdir().unwrap();
        let dist = temp.path().join("cef");
        fs::create_dir_all(dist.join("include")).unwrap();
        fs::create_dir_all(dist.join(LOCALES_DIR)).unwrap();
        fs::write(
            dist.join("include/cef_version.h"),
            "#define CEF_VERSION \"150.0.11+gtest+chromium-150.0.0.0\"\n",
        )
        .unwrap();
        fs::write(
            dist.join("archive.json"),
            "{\"name\":\"cef_binary_150.0.11+gtest_windows64.tar.bz2\"}",
        )
        .unwrap();
        for name in required_runtime_files("x86_64-pc-windows-msvc") {
            fs::write(dist.join(name), b"x").unwrap();
        }
        fs::write(dist.join("libcef.lib"), b"forbidden").unwrap();
        fs::write(dist.join(LOCALES_DIR).join("en-US.pak"), b"x").unwrap();

        let staging = temp.path().join("stage");
        fs::create_dir_all(&staging).unwrap();
        stage_cef(&staging, &dist, "x86_64-pc-windows-msvc").unwrap();
        assert!(!staging.join("libcef.lib").exists());
    }

    #[test]
    fn incomplete_distribution_fails() {
        let temp = tempfile::tempdir().unwrap();
        let dist = fake_windows_dist(temp.path());
        // Remove a required file after building the fake dist.
        fs::remove_file(dist.join(RESOURCES_DIR).join("icudtl.dat")).unwrap();
        let staging = temp.path().join("stage");
        fs::create_dir_all(&staging).unwrap();
        assert!(stage_cef(&staging, &dist, "x86_64-pc-windows-msvc").is_err());
    }

    #[test]
    fn required_files_track_platform_library() {
        assert!(
            required_runtime_files("x86_64-pc-windows-msvc").contains(&"libcef.dll".to_string())
        );
        assert!(
            required_runtime_files("x86_64-unknown-linux-gnu").contains(&"libcef.so".to_string())
        );
        assert!(required_runtime_files("aarch64-apple-darwin").contains(
            &"Chromium Embedded Framework.framework/Chromium Embedded Framework".to_string()
        ));
    }

    #[test]
    fn stages_macos_framework_with_resources_and_locales() {
        let temp = tempfile::tempdir().unwrap();
        let dist = temp.path().join("cef");
        let framework = dist.join(MAC_FRAMEWORK_DIR);
        let resources = framework.join("Resources");
        fs::create_dir_all(resources.join("en.lproj")).unwrap();
        fs::create_dir_all(dist.join("include")).unwrap();
        fs::write(
            dist.join("include/cef_version.h"),
            "#define CEF_VERSION \"150.0.11+gtest+chromium-150.0.0.0\"\n",
        )
        .unwrap();
        fs::write(
            dist.join("archive.json"),
            "{\"name\":\"cef_binary_150.0.11+gtest_macosarm64.tar.bz2\"}",
        )
        .unwrap();
        for required in required_runtime_files("aarch64-apple-darwin") {
            let relative = Path::new(&required)
                .strip_prefix(MAC_FRAMEWORK_DIR)
                .unwrap();
            let path = framework.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"x").unwrap();
        }
        fs::write(resources.join("en.lproj/locale.pak"), b"x").unwrap();

        let staging = temp.path().join("stage");
        fs::create_dir_all(&staging).unwrap();
        let report = stage_cef(&staging, &dist, "aarch64-apple-darwin").unwrap();
        assert_eq!(report.locale_count, 1);
        assert!(
            staging
                .join(MAC_FRAMEWORK_DIR)
                .join("Resources/resources.pak")
                .is_file()
        );
        assert!(
            staging
                .join(MAC_FRAMEWORK_DIR)
                .join("Resources/en.lproj/locale.pak")
                .is_file()
        );
    }
}

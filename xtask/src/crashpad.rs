//! Verified Crashpad prebuilt for every build xtask runs (#288).
//!
//! crashpad-rs-sys v0.2.7 (`prebuilt` feature) downloads
//! `crashpad-<version>-<target>.tar.gz` from the crashpad-rs GitHub release,
//! extracts it into `CRASHPAD_CACHE_DIR/prebuilt/<version>/<target>/` and
//! links whatever it finds there once the `.crashpad-ok` marker exists. It
//! checks no digest. xtask therefore seeds that cache itself: it downloads the
//! pinned archive, checks its SHA-256, extracts it fresh into a cache private
//! to this checkout (`target/crashpad-prebuilt`), writes the marker, and runs
//! Cargo with `CRASHPAD_CACHE_DIR` pointing there, so the build script never
//! downloads. Packaging then compares the staged `crashpad_handler` with the
//! one from the verified archive.
//!
//! Limitation: a plain `cargo build` that does not get `CRASHPAD_CACHE_DIR`
//! from xtask (`package`, the build/check aliases, or CI's
//! `cargo xtask crashpad-prebuilt` step) still uses crashpad-rs-sys's own,
//! unverified download into the user cache. That is a development path, not a
//! release path; release artifacts are built by `cargo xtask package`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::staging::crashpad_handler_name;

/// The crashpad-rs release locked in Cargo.lock (git tag v0.2.7).
pub const CRASHPAD_VERSION: &str = "0.2.7";
/// Marker crashpad-rs-sys's `build/prebuilt.rs` looks for before it skips
/// its own download.
const OK_MARKER: &str = ".crashpad-ok";

/// SHA-256 of the v0.2.7 prebuilt archives (the `digest` GitHub records for
/// each release asset). Only targets that select the `prebuilt` feature in
/// `apps/native/studio/Cargo.toml` are listed; the others build from source.
///
/// Not covered here: on the `vendored` strategy (the macOS x86_64 slice of the
/// universal release, Linux aarch64, mobile) and `vendored-depot` (Windows
/// ARM64), crashpad-rs-sys compiles Crashpad from the sources in its locked
/// git checkout, and `vendored` downloads GN and Ninja from
/// chrome-infra-packages.appspot.com by version tag with no digest check
/// (`crashpad-sys/build/tools.rs`); `vendored-depot` clones the moving
/// depot_tools branch from chromium.googlesource.com. Those are build tools,
/// not shipped files, but a compromised tool could still alter what it builds.
pub fn pinned_sha256(target: &str) -> Option<&'static str> {
    match target {
        "x86_64-unknown-linux-gnu" => {
            Some("17d71ee7c0662a7c105347e1f4f01f131a14b8231924a820db0327a07e9672a6")
        }
        "x86_64-pc-windows-msvc" => {
            Some("52ad85cbc51a5b37219099cd470edecf2a9be833c0e8fde0d9b0e0384e2715bf")
        }
        "aarch64-apple-darwin" => {
            Some("e476c184aca45b1b54157ce4b3ef85986beb0aac2780a69b5a4ad047fd18dcc8")
        }
        _ => None,
    }
}

/// A verified, freshly extracted prebuilt.
#[derive(Debug, Clone)]
pub struct Prebuilt {
    /// Value for `CRASHPAD_CACHE_DIR`.
    pub cache_root: PathBuf,
    /// `<cache_root>/prebuilt/<version>/<target>`, the directory crashpad-rs-sys
    /// links from and copies `crashpad_handler` out of.
    pub dir: PathBuf,
    pub target: String,
}

/// Private cache root for this checkout. `target/` is never shared between
/// worktrees, unlike the user-wide crashpad-rs cache.
pub fn cache_root(workspace: &Path) -> PathBuf {
    workspace.join("target").join("crashpad-prebuilt")
}

/// Download (or reuse) the pinned archive for `target`, verify it and extract
/// it into the cache layout crashpad-rs-sys expects. `Ok(None)` for targets
/// that build Crashpad from source, which this does not verify (see
/// [`pinned_sha256`]: GN/Ninja are fetched by tag without a digest).
pub fn prepare(workspace: &Path, target: &str) -> Result<Option<Prebuilt>> {
    let Some(pin) = pinned_sha256(target) else {
        return Ok(None);
    };
    let cache_root = cache_root(workspace);
    let downloads = cache_root.join("downloads");
    fs::create_dir_all(&downloads)
        .with_context(|| format!("cannot create {}", downloads.display()))?;
    let archive_name = format!("crashpad-{CRASHPAD_VERSION}-{target}.tar.gz");
    let archive = downloads.join(&archive_name);

    if !sha256_matches(&archive, pin)? {
        let url = format!(
            "https://github.com/bahamoth/crashpad-rs/releases/download/v{CRASHPAD_VERSION}/{archive_name}"
        );
        eprintln!("[xtask] downloading {url}");
        download(&url, &archive)?;
        if !sha256_matches(&archive, pin)? {
            let _ = fs::remove_file(&archive);
            bail!("Crashpad archive {url} does not match pinned SHA-256 {pin}");
        }
    }

    // Always re-extract: `target/` is restored from CI caches, so an existing
    // tree (and its marker) proves nothing.
    let version_dir = cache_root.join("prebuilt").join(CRASHPAD_VERSION);
    let dir = version_dir.join(target);
    let staging = version_dir.join(format!(".{target}.extract-{}", std::process::id()));
    if staging.exists() {
        fs::remove_dir_all(&staging)?;
    }
    fs::create_dir_all(&staging)?;
    if let Err(error) = extract(&archive, &staging) {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }
    if let Some(handler) = crashpad_handler_name(target) {
        if !staging.join(handler).is_file() {
            let _ = fs::remove_dir_all(&staging);
            bail!("verified Crashpad archive {archive_name} has no {handler}");
        }
    }
    if dir.exists() {
        fs::remove_dir_all(&dir).with_context(|| format!("cannot replace {}", dir.display()))?;
    }
    fs::rename(&staging, &dir)
        .with_context(|| format!("cannot move {} to {}", staging.display(), dir.display()))?;
    fs::write(dir.join(OK_MARKER), "")?;
    eprintln!(
        "[xtask] Crashpad {CRASHPAD_VERSION} prebuilt verified (SHA-256 {pin}): {}",
        dir.display()
    );
    Ok(Some(Prebuilt {
        cache_root,
        dir,
        target: target.to_owned(),
    }))
}

/// [`prepare`] for a Cargo invocation that builds into `target_dir` with the
/// `dev` and/or `release` profiles (aliases, CI), then reset stale build
/// state in those profile directories. `target` is the `--target` value, if
/// any; the host triple otherwise.
pub fn seed_for_target_dir(
    workspace: &Path,
    target: Option<&str>,
    target_dir: &Path,
) -> Result<Option<Prebuilt>> {
    let triple = match target {
        Some(triple) => triple.to_owned(),
        None => crate::platform::host_target()?,
    };
    let Some(prebuilt) = prepare(workspace, &triple)? else {
        return Ok(None);
    };
    let base = match target {
        Some(triple) => target_dir.join(triple),
        None => target_dir.to_path_buf(),
    };
    for profile in ["debug", "release"] {
        reset_stale_build_state(&base.join(profile), &prebuilt)?;
    }
    Ok(Some(prebuilt))
}

/// Cargo reruns crashpad-rs-sys's build script only when its own sources
/// change, so a target dir built before (or restored from a cache) keeps
/// linking from whatever cache path it first recorded. Drop that crate's build
/// state under `profile_dir` (`<target-dir>[/<triple>]/<profile>`) when its
/// recorded link path is not the verified cache.
pub fn reset_stale_build_state(profile_dir: &Path, prebuilt: &Prebuilt) -> Result<()> {
    let build_dir = profile_dir.join("build");
    let Ok(entries) = fs::read_dir(&build_dir) else {
        return Ok(());
    };
    let expected = format!("rustc-link-search={}", prebuilt.dir.display());
    let stale = entries.flatten().any(|entry| {
        entry
            .file_name()
            .to_string_lossy()
            .starts_with("crashpad-rs-sys-")
            && fs::read_to_string(entry.path().join("output")).is_ok_and(|output| {
                output.contains("rustc-link-search=") && !output.contains(&expected)
            })
    });
    if !stale {
        return Ok(());
    }
    eprintln!(
        "[xtask] crashpad-rs-sys in {} links from another cache; rebuilding it from {}",
        profile_dir.display(),
        prebuilt.dir.display()
    );
    for sub in ["build", ".fingerprint"] {
        let Ok(entries) = fs::read_dir(profile_dir.join(sub)) else {
            continue;
        };
        for entry in entries.flatten() {
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with("crashpad-rs-sys-")
            {
                fs::remove_dir_all(entry.path())
                    .with_context(|| format!("cannot remove {}", entry.path().display()))?;
            }
        }
    }
    Ok(())
}

/// Fail when the staged `crashpad_handler` is not the one in the verified
/// archive (the upstream archive ships no per-file digest, so the freshly
/// extracted, SHA-256-checked copy is the reference).
pub fn verify_staged_handler(prebuilt: &Prebuilt, staged: &Path) -> Result<()> {
    let Some(name) = crashpad_handler_name(&prebuilt.target) else {
        return Ok(());
    };
    let reference = prebuilt.dir.join(name);
    let expected = sphere_webview::sha256_file(&reference)
        .with_context(|| format!("cannot hash {}", reference.display()))?;
    let actual = sphere_webview::sha256_file(staged)
        .with_context(|| format!("cannot hash {}", staged.display()))?;
    if actual != expected {
        bail!(
            "staged {} (SHA-256 {actual}) is not the handler from the verified Crashpad \
             {CRASHPAD_VERSION} archive (SHA-256 {expected})",
            staged.display()
        );
    }
    eprintln!("[xtask] Crashpad handler matches the verified archive (SHA-256 {expected})");
    Ok(())
}

fn sha256_matches(path: &Path, expected: &str) -> Result<bool> {
    if !path.is_file() {
        return Ok(false);
    }
    let actual = sphere_webview::sha256_file(path)
        .with_context(|| format!("cannot hash {}", path.display()))?;
    Ok(actual == expected)
}

fn download(url: &str, destination: &Path) -> Result<()> {
    let partial = destination.with_extension("part");
    let response = ureq::get(url)
        .call()
        .with_context(|| format!("failed to download {url}"))?;
    let mut reader = response.into_body().into_reader();
    let mut file = fs::File::create(&partial)
        .with_context(|| format!("cannot create {}", partial.display()))?;
    std::io::copy(&mut reader, &mut file).with_context(|| format!("failed to read {url}"))?;
    file.flush()?;
    drop(file);
    fs::rename(&partial, destination)?;
    Ok(())
}

fn extract(archive: &Path, destination: &Path) -> Result<()> {
    // Windows ships bsdtar in System32. A GNU tar from Git/MSYS earlier on
    // PATH reads `C:\...` as a remote host, so name the system one.
    let tar = if cfg!(windows) {
        std::env::var_os("SystemRoot")
            .map(|root| PathBuf::from(root).join("System32").join("tar.exe"))
            .unwrap_or_else(|| PathBuf::from("tar"))
    } else {
        PathBuf::from("tar")
    };
    let directory = archive
        .parent()
        .context("archive has no parent directory")?;
    let name = archive.file_name().context("archive has no file name")?;
    let status = Command::new(&tar)
        .current_dir(directory)
        .arg("-xzf")
        .arg(name)
        .arg("-C")
        .arg(destination)
        .status()
        .with_context(|| format!("failed to run {}", tar.display()))?;
    if !status.success() {
        bail!(
            "{} -xzf {} failed with {status}",
            tar.display(),
            archive.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prebuilt_targets_have_sha256_pins() {
        for target in [
            "x86_64-unknown-linux-gnu",
            "x86_64-pc-windows-msvc",
            "aarch64-apple-darwin",
        ] {
            let pin = pinned_sha256(target).unwrap();
            assert_eq!(pin.len(), 64);
            assert!(
                pin.chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
            );
        }
        // Source-built targets never get a prebuilt.
        assert!(pinned_sha256("x86_64-apple-darwin").is_none());
        assert!(pinned_sha256("aarch64-pc-windows-msvc").is_none());
    }

    #[test]
    fn stale_crashpad_build_state_is_dropped() {
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join("release");
        let prebuilt = Prebuilt {
            cache_root: temp.path().join("cache"),
            dir: temp
                .path()
                .join("cache/prebuilt/0.2.7/x86_64-pc-windows-msvc"),
            target: "x86_64-pc-windows-msvc".to_owned(),
        };
        let run = profile.join("build/crashpad-rs-sys-1111");
        let fingerprint = profile.join(".fingerprint/crashpad-rs-sys-1111");
        let other = profile.join("build/crashpad-rs-2222");
        for dir in [&run, &fingerprint, &other] {
            fs::create_dir_all(dir).unwrap();
        }

        // Links from the verified cache: kept.
        fs::write(
            run.join("output"),
            format!("cargo:rustc-link-search={}\n", prebuilt.dir.display()),
        )
        .unwrap();
        reset_stale_build_state(&profile, &prebuilt).unwrap();
        assert!(run.is_dir() && fingerprint.is_dir());

        // Links from the user-wide cache: dropped, other crates untouched.
        fs::write(
            run.join("output"),
            "cargo:rustc-link-search=C:\\Users\\x\\AppData\\Local\\crashpad-rs\\prebuilt\n",
        )
        .unwrap();
        reset_stale_build_state(&profile, &prebuilt).unwrap();
        assert!(!run.exists() && !fingerprint.exists());
        assert!(other.is_dir());
    }

    #[test]
    fn staged_handler_must_match_the_verified_archive() {
        let temp = tempfile::tempdir().unwrap();
        let prebuilt = Prebuilt {
            cache_root: temp.path().to_path_buf(),
            dir: temp.path().join("verified"),
            target: "x86_64-pc-windows-msvc".to_owned(),
        };
        fs::create_dir_all(&prebuilt.dir).unwrap();
        fs::write(prebuilt.dir.join("crashpad_handler.exe"), b"pinned").unwrap();
        let staged = temp.path().join("crashpad_handler.exe");
        fs::write(&staged, b"pinned").unwrap();
        verify_staged_handler(&prebuilt, &staged).unwrap();
        fs::write(&staged, b"other").unwrap();
        assert!(verify_staged_handler(&prebuilt, &staged).is_err());
    }
}

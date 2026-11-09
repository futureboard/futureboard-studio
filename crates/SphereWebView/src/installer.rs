use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use crate::{
    CefDistributionError, CefTarget, INSTALL_MANIFEST, render_install_manifest, sha256_file,
    verify_installed_distribution, workspace_cef_path, workspace_root,
};

/// Downloads and installs the pinned CEF distribution for the host
/// into `<workspace>/build/cef/<version>/<platform>`.
///
/// This is an explicit tooling operation, never a Cargo build-script side
/// effect. An existing SDK is kept when `libcef` matches the pin and every
/// other file matches the install manifest, and replaced when that check fails
/// or `force` is set.
pub fn install_cef(force: bool) -> Result<PathBuf, CefDistributionError> {
    install_cef_target(CefTarget::current()?, force)
}

/// Same as [`install_cef`], for an explicitly chosen distribution.
///
/// Every step — archive download, SHA-256 check, extraction and validation — is
/// keyed off `target`, so this installs a foreign architecture just as well as
/// the host one. macOS universal packaging needs both `MacOsX86_64` and
/// `MacOsAarch64` present on one machine, and each lands in its own
/// `build/cef/<version>/<platform>` directory.
pub fn install_cef_target(target: CefTarget, force: bool) -> Result<PathBuf, CefDistributionError> {
    let destination = workspace_cef_path(target);
    let mut replace = force;
    if destination.exists() && !force {
        match verify_installed_distribution(&destination, target) {
            Ok(()) => return Ok(destination),
            Err(
                CefDistributionError::Integrity { .. }
                | CefDistributionError::ManifestMismatch { .. }
                | CefDistributionError::MissingFile(_)
                | CefDistributionError::VersionMismatch { .. },
            ) => {
                eprintln!(
                    "cached CEF at {} failed the pinned check; reinstalling",
                    destination.display()
                );
                replace = true;
            }
            Err(error) => return Err(error),
        }
    }

    let cef_root = workspace_root().join("build").join("cef");
    let destination_parent = destination
        .parent()
        .expect("versioned CEF destination must have a parent");
    fs::create_dir_all(destination_parent)?;
    let staging = cef_root.join(format!(".cef-install-{}", std::process::id()));
    if staging.exists() {
        fs::remove_dir_all(&staging)?;
    }
    fs::create_dir_all(&staging)?;

    let result = install_into_staging(target, &staging, &destination, replace);
    if staging.exists() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

fn install_into_staging(
    target: CefTarget,
    staging: &Path,
    destination: &Path,
    force: bool,
) -> Result<PathBuf, CefDistributionError> {
    // The CDN index SHA-1 is not consulted. The archive URL and digest are
    // both pinned in this repository; a CDN that serves a new archive and a
    // matching SHA-1 still fails here.
    let archive = staging.join(target.archive_name());
    download_exact_archive(target, &archive)?;
    let actual = sha256_file(&archive)?;
    let expected = target.archive_sha256();
    if actual != expected {
        return Err(CefDistributionError::Integrity {
            kind: "archive",
            target: target.target_triple().to_owned(),
            expected,
        });
    }

    let extracted =
        download_cef::extract_target_archive(target.target_triple(), &archive, staging, true)?;
    write_archive_json(&extracted, &archive, target)?;
    // Record every unpacked file while the tree is known to come from the
    // pinned archive. Packaging re-checks the tree against this list.
    let manifest = render_install_manifest(&extracted, target)?;
    fs::write(extracted.join(INSTALL_MANIFEST), manifest)?;
    verify_installed_distribution(&extracted, target)?;

    let backup = destination.with_extension(format!("backup-{}", std::process::id()));
    if destination.exists() {
        if !force {
            return Err(CefDistributionError::DestinationExists(
                destination.to_path_buf(),
            ));
        }
        if backup.exists() {
            fs::remove_dir_all(&backup)?;
        }
        fs::rename(destination, &backup)?;
    }

    if let Err(error) = fs::rename(&extracted, destination) {
        if backup.exists() {
            let _ = fs::rename(&backup, destination);
        }
        return Err(error.into());
    }
    if backup.exists() {
        fs::remove_dir_all(backup)?;
    }

    verify_installed_distribution(destination, target)?;
    Ok(destination.to_path_buf())
}

/// Write `archive.json` in the shape cef-rs' build script parses
/// (`download_cef::CefFile`: `type`, `name`, `sha1`). Without `sha1` that
/// parse fails and cef-dll-sys silently downloads its own CEF, checked only
/// against the CDN index SHA-1. The SHA-1 is computed from the archive that
/// just passed the SHA-256 pin; `sha256` is an extra field serde ignores.
fn write_archive_json(
    extracted: &Path,
    archive: &Path,
    target: CefTarget,
) -> Result<(), CefDistributionError> {
    let sha1 = download_cef::CefFile::try_from(archive)?.sha1;
    fs::write(
        extracted.join("archive.json"),
        archive_json(target.archive_name(), &sha1, target.archive_sha256()),
    )?;
    Ok(())
}

/// Archive names and hex digests contain no JSON metacharacters.
fn archive_json(name: &str, sha1: &str, sha256: &str) -> String {
    format!(
        "{{\n  \"type\": \"standard\",\n  \"name\": \"{name}\",\n  \"sha1\": \"{sha1}\",\n  \"sha256\": \"{sha256}\"\n}}\n"
    )
}

fn download_exact_archive(
    target: CefTarget,
    destination: &Path,
) -> Result<(), CefDistributionError> {
    println!("Downloading {}", target.archive_url());
    let response = ureq::get(target.archive_url())
        .call()
        .map_err(|error| CefDistributionError::Http(Box::new(error)))?;
    let mut reader = response.into_body().into_reader();
    let mut writer = BufWriter::new(File::create(destination)?);
    std::io::copy(&mut reader, &mut writer)?;
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The version cef-rs passes at the pinned rev (its `CARGO_PKG_VERSION`).
    const CEF_RS_PACKAGE_VERSION: &str = "150.1.0+150.0.11";

    fn write_in_temp(tag: &str, body: &str) -> Result<(), download_cef::Error> {
        let dir = std::env::temp_dir().join(format!("fb-cef-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("archive.json"), body).unwrap();
        let checked =
            download_cef::check_archive_json(CEF_RS_PACKAGE_VERSION, &dir.to_string_lossy());
        let _ = fs::remove_dir_all(&dir);
        checked
    }

    /// cef-rs' build script must accept what the installer writes, or it
    /// falls back to its own CDN download (#303).
    #[test]
    fn archive_json_parses_as_download_cef_file() {
        let target = CefTarget::WindowsX86_64;
        let sha1 = "0123456789abcdef0123456789abcdef01234567";
        let body = archive_json(target.archive_name(), sha1, target.archive_sha256());
        write_in_temp("archive-json", &body).unwrap();
        assert!(body.contains(&format!("\"sha1\": \"{sha1}\"")));
        assert!(body.contains(&format!("\"sha256\": \"{}\"", target.archive_sha256())));
    }

    /// The previous shape (no `sha1`) is what cef-rs rejected.
    #[test]
    fn archive_json_without_sha1_is_rejected_by_download_cef() {
        let body = format!(
            "{{\"type\":\"standard\",\"name\":\"{}\",\"sha256\":\"00\"}}",
            CefTarget::WindowsX86_64.archive_name()
        );
        assert!(write_in_temp("archive-nosha1", &body).is_err());
    }
}

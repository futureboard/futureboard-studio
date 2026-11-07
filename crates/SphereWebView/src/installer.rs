use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use crate::{
    CefDistributionError, CefTarget, validate_cef_path, workspace_cef_path, workspace_root,
};

/// Downloads and installs the pinned minimal CEF distribution for the host
/// into `<workspace>/build/cef/<version>/<platform>`.
///
/// This is an explicit tooling operation, never a Cargo build-script side
/// effect. Existing SDKs are preserved unless `force` is true.
pub fn install_cef(force: bool) -> Result<PathBuf, CefDistributionError> {
    install_cef_target(CefTarget::current()?, force)
}

/// Same as [`install_cef`], for an explicitly chosen distribution.
///
/// Every step — index lookup, archive selection, extraction and validation — is
/// keyed off `target`, so this installs a foreign architecture just as well as
/// the host one. macOS universal packaging needs both `MacOsX86_64` and
/// `MacOsAarch64` present on one machine, and each lands in its own
/// `build/cef/<version>/<platform>` directory.
pub fn install_cef_target(target: CefTarget, force: bool) -> Result<PathBuf, CefDistributionError> {
    let destination = workspace_cef_path(target);
    if destination.exists() && !force {
        validate_cef_path(&destination, target)?;
        return Ok(destination);
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

    let result = install_into_staging(target, &staging, &destination, force);
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
    let index = download_cef::CefIndex::download()?;
    let version = index
        .platform(target.target_triple())?
        .version("150.0.11")?;
    let file = version
        .files
        .iter()
        .find(|file| file.name == target.archive_name())
        .ok_or_else(|| download_cef::Error::VersionNotFound(target.archive_name().to_owned()))?;
    let archive = staging.join(target.archive_name());
    download_exact_archive(target, &archive)?;
    let downloaded = download_cef::CefFile::try_from(archive.as_path())?;
    if downloaded.sha1 != file.sha1 {
        return Err(download_cef::Error::CorruptedFile(archive.display().to_string()).into());
    }

    let extracted =
        download_cef::extract_target_archive(target.target_triple(), &archive, staging, true)?;
    file.write_archive_json(&extracted)?;
    validate_cef_path(&extracted, target)?;

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

    validate_cef_path(destination, target)?;
    Ok(destination.to_path_buf())
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

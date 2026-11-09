//! Offline MDX-NET / UVR model package download.
//!
//! Classification: scanner/offline path (worker thread only). Downloads ONNX
//! weights into `Documents/Futureboard Studio/Utilities/Models/`.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::error::StemExtractError;
use crate::model::{StemModel, StemModelFile, StemModelPackage};
use crate::progress::StemExtractCancelToken;
use sha2::{Digest, Sha256};

/// Public UVR model release hosting MDX-NET ONNX weights.
pub const UVR_MODEL_RELEASE_BASE: &str =
    "https://github.com/TRvlvr/model_repo/releases/download/all_public_uvr_models";

/// StemSplit Hugging Face repository hosting the single-file HT-Demucs ONNX
/// export pinned to immutable commit `d54ed9eb60e258ea82131c6ee14578628816456a`.
pub const HTDEMUCS_MODEL_BASE: &str = "https://huggingface.co/StemSplitio/htdemucs-onnx/resolve/d54ed9eb60e258ea82131c6ee14578628816456a";

/// Progress event while downloading one or more ONNX files for a model.
#[derive(Clone, Debug)]
pub struct StemModelDownloadProgress {
    pub model: StemModel,
    pub file_name: String,
    pub file_index: usize,
    pub file_count: usize,
    pub bytes_downloaded: u64,
    pub bytes_total: Option<u64>,
    pub percent: f32,
    pub detail: String,
}

impl StemModelDownloadProgress {
    pub fn new(
        model: StemModel,
        file_name: impl Into<String>,
        file_index: usize,
        file_count: usize,
        bytes_downloaded: u64,
        bytes_total: Option<u64>,
        percent: f32,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            model,
            file_name: file_name.into(),
            file_index,
            file_count,
            bytes_downloaded,
            bytes_total,
            percent: percent.clamp(0.0, 100.0),
            detail: detail.into(),
        }
    }
}

/// Default install directory: `~/Documents/Futureboard Studio/Utilities/Models/`.
pub fn default_models_dir() -> PathBuf {
    documents_dir()
        .join("Futureboard Studio")
        .join("Utilities")
        .join("Models")
}

fn documents_dir() -> PathBuf {
    if let Some(dir) = dirs::document_dir() {
        return dir;
    }
    if let Some(home) = dirs::home_dir() {
        return home.join("Documents");
    }
    PathBuf::from("Documents")
}

/// Ensure the models directory exists.
pub fn ensure_models_dir(models_dir: &Path) -> Result<(), StemExtractError> {
    fs::create_dir_all(models_dir).map_err(|e| {
        StemExtractError::Backend(format!(
            "could not create model folder {}: {e}",
            models_dir.display()
        ))
    })
}

/// Verify that an existing file matches both its expected size and SHA-256 hash.
pub fn verify_file_hash(path: &Path, expected_bytes: u64, expected_sha256: &str) -> bool {
    if !path.is_file() {
        return false;
    }
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    if meta.len() != expected_bytes {
        return false;
    }
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buf[..n]),
            Err(_) => return false,
        }
    }
    format!("{:x}", hasher.finalize()).eq_ignore_ascii_case(expected_sha256)
}

/// True when every ONNX file for `model` is present under `models_dir` and matches its pinned SHA-256 hash.
pub fn model_installed(model: StemModel, models_dir: &Path) -> bool {
    verify_model_installed(model, models_dir)
}

/// Verify that every ONNX file for `model` is present and matches its pinned SHA-256 hash.
pub fn verify_model_installed(model: StemModel, models_dir: &Path) -> bool {
    let package = model.package();
    package.files.iter().all(|file| {
        let path = models_dir.join(file.file_name);
        verify_file_hash(&path, file.expected_bytes, file.sha256)
    })
}

/// Absolute paths for installed model files, or `None` if incomplete or hash verification fails.
pub fn resolve_installed_model_files(model: StemModel, models_dir: &Path) -> Option<Vec<PathBuf>> {
    if !model_installed(model, models_dir) {
        return None;
    }
    Some(
        model
            .package()
            .files
            .iter()
            .map(|file| models_dir.join(file.file_name))
            .collect(),
    )
}

fn download_url(base_url: &str, file_name: &str) -> String {
    format!("{}/{file_name}", base_url.trim_end_matches('/'))
}

/// Download every ONNX file for `model` into `models_dir`.
///
/// Existing verified installs are a no-op. Partial files are written to
/// `*.partial`, verified against the pinned SHA-256 hash and size, and
/// atomically renamed on success.
pub fn download_model(
    model: StemModel,
    models_dir: &Path,
    cancel: &StemExtractCancelToken,
    on_progress: &mut dyn FnMut(StemModelDownloadProgress),
) -> Result<(), StemExtractError> {
    ensure_models_dir(models_dir)?;
    let package = model.package();
    if verify_model_installed(model, models_dir) {
        on_progress(StemModelDownloadProgress::new(
            model,
            package
                .files
                .first()
                .map(|f| f.file_name)
                .unwrap_or("model"),
            package.file_count().saturating_sub(1),
            package.file_count().max(1),
            package.approx_bytes,
            Some(package.approx_bytes),
            100.0,
            format!("{} already installed", model.label()),
        ));
        return Ok(());
    }

    let file_count = package.file_count().max(1);
    for (file_index, file) in package.files.iter().enumerate() {
        if cancel.is_cancelled() {
            return Err(StemExtractError::Cancelled);
        }
        let dest = models_dir.join(file.file_name);
        if verify_file_hash(&dest, file.expected_bytes, file.sha256) {
            let base = (file_index as f32 / file_count as f32) * 100.0;
            on_progress(StemModelDownloadProgress::new(
                model,
                file.file_name,
                file_index,
                file_count,
                file.expected_bytes,
                Some(file.expected_bytes),
                base,
                format!("Skipping verified {}", file.file_name),
            ));
            continue;
        } else if dest.exists() {
            // Broken or truncated file from an earlier failed run; delete before re-downloading.
            let _ = fs::remove_file(&dest);
        }

        download_one_file(
            model,
            package,
            file,
            file_index,
            file_count,
            &dest,
            cancel,
            on_progress,
        )?;
    }

    if !verify_model_installed(model, models_dir) {
        return Err(StemExtractError::Backend(format!(
            "download finished but {} verification failed",
            model.label()
        )));
    }
    on_progress(StemModelDownloadProgress::new(
        model,
        package.files.last().map(|f| f.file_name).unwrap_or("model"),
        file_count.saturating_sub(1),
        file_count,
        package.approx_bytes,
        Some(package.approx_bytes),
        100.0,
        format!("{} installed", model.label()),
    ));
    Ok(())
}

fn download_one_file(
    model: StemModel,
    package: StemModelPackage,
    file_info: &StemModelFile,
    file_index: usize,
    file_count: usize,
    dest: &Path,
    cancel: &StemExtractCancelToken,
    on_progress: &mut dyn FnMut(StemModelDownloadProgress),
) -> Result<(), StemExtractError> {
    let file_name = file_info.file_name;
    let url = download_url(package.base_url, file_name);
    let partial = dest.with_extension("onnx.partial");
    let _ = fs::remove_file(&partial);

    on_progress(StemModelDownloadProgress::new(
        model,
        file_name,
        file_index,
        file_count,
        0,
        Some(file_info.expected_bytes),
        (file_index as f32 / file_count as f32) * 100.0,
        format!("Connecting to {}", package.source_label),
    ));

    let response = ureq::get(&url)
        .call()
        .map_err(|e| StemExtractError::Backend(format!("download failed for {file_name}: {e}")))?;

    let content_length = response
        .headers()
        .get("Content-Length")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|n| *n > 0);

    if let Some(cl) = content_length {
        if cl != file_info.expected_bytes {
            return Err(StemExtractError::Backend(format!(
                "Content-Length mismatch for {file_name}: expected {} bytes, server reported {cl} bytes",
                file_info.expected_bytes
            )));
        }
    }

    let mut reader = response.into_body().into_reader();
    let mut file = File::create(&partial).map_err(|e| {
        StemExtractError::Backend(format!("could not create {}: {e}", partial.display()))
    })?;

    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut downloaded = 0u64;
    let file_span = 100.0 / file_count as f32;
    let file_base = file_index as f32 * file_span;

    let read_result = (|| -> Result<(), StemExtractError> {
        loop {
            if cancel.is_cancelled() {
                return Err(StemExtractError::Cancelled);
            }
            let n = reader.read(&mut buf).map_err(|e| {
                StemExtractError::Backend(format!("download read failed for {file_name}: {e}"))
            })?;
            if n == 0 {
                break;
            }
            if downloaded + n as u64 > file_info.expected_bytes {
                return Err(StemExtractError::Backend(format!(
                    "download for {file_name} exceeded expected size ({} bytes)",
                    file_info.expected_bytes
                )));
            }
            file.write_all(&buf[..n]).map_err(|e| {
                StemExtractError::Backend(format!("download write failed for {file_name}: {e}"))
            })?;
            hasher.update(&buf[..n]);
            downloaded += n as u64;

            let within = (downloaded as f32 / file_info.expected_bytes as f32).clamp(0.0, 1.0);
            let percent = file_base + within * file_span;
            on_progress(StemModelDownloadProgress::new(
                model,
                file_name,
                file_index,
                file_count,
                downloaded,
                Some(file_info.expected_bytes),
                percent,
                format!("Downloading {file_name}"),
            ));
        }
        file.flush().map_err(|e| {
            StemExtractError::Backend(format!("download flush failed for {file_name}: {e}"))
        })?;
        Ok(())
    })();

    drop(file);

    if let Err(err) = read_result {
        let _ = fs::remove_file(&partial);
        return Err(err);
    }

    if downloaded != file_info.expected_bytes {
        let _ = fs::remove_file(&partial);
        return Err(StemExtractError::Backend(format!(
            "download for {file_name} was incomplete: got {downloaded} bytes, expected {}",
            file_info.expected_bytes
        )));
    }

    let digest = format!("{:x}", hasher.finalize());
    if !digest.eq_ignore_ascii_case(file_info.sha256) {
        let _ = fs::remove_file(&partial);
        return Err(StemExtractError::Backend(format!(
            "SHA-256 hash mismatch for {file_name}: expected {}, got {digest}",
            file_info.sha256
        )));
    }

    fs::rename(&partial, dest).map_err(|e| {
        let _ = fs::remove_file(&partial);
        StemExtractError::Backend(format!("could not finalize {}: {e}", dest.display()))
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installed_requires_matching_sha256_and_every_package_file() {
        let dir = std::env::temp_dir().join(format!(
            "fb-stem-models-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        assert!(!model_installed(StemModel::MdxNetKaraoke, &dir));
        let path = dir.join("UVR_MDXNET_KARA.onnx");
        // A partial/broken download of 2048 bytes must NOT count as installed
        fs::write(&path, vec![0u8; 2048]).unwrap();
        assert!(!model_installed(StemModel::MdxNetKaraoke, &dir));
        assert!(!verify_model_installed(StemModel::MdxNetKaraoke, &dir));
        assert!(resolve_installed_model_files(StemModel::MdxNetKaraoke, &dir).is_none());

        // Matching size with invalid content must NOT count as installed (#359)
        let file = File::create(&path).unwrap();
        file.set_len(StemModel::MdxNetKaraoke.package().files[0].expected_bytes)
            .unwrap();
        drop(file);
        assert!(!model_installed(StemModel::MdxNetKaraoke, &dir));
        assert!(!verify_model_installed(StemModel::MdxNetKaraoke, &dir));
        assert!(resolve_installed_model_files(StemModel::MdxNetKaraoke, &dir).is_none());

        assert!(!model_installed(StemModel::MdxNet, &dir));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn verify_file_hash_validates_real_content() {
        let dir = std::env::temp_dir().join(format!(
            "fb-stem-hash-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.bin");
        fs::write(&path, b"hello stem world").unwrap();
        let expected_sha = format!("{:x}", Sha256::digest(b"hello stem world"));
        // Known-answer check: FIPS 180-2 test vector for "abc".
        assert_eq!(
            format!("{:x}", Sha256::digest(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(verify_file_hash(&path, 16, &expected_sha));
        // Wrong size
        assert!(!verify_file_hash(&path, 15, &expected_sha));
        // Wrong hash
        assert!(!verify_file_hash(
            &path,
            16,
            "0000000000000000000000000000000000000000000000000000000000000000"
        ));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn default_models_dir_ends_with_utilities_models() {
        let dir = default_models_dir();
        assert!(dir.ends_with(Path::new("Utilities").join("Models")));
        assert!(
            !dir.starts_with("."),
            "models dir must not resolve to a relative CWD path: {}",
            dir.display()
        );
    }

    #[test]
    fn live_download_karaoke_when_enabled() {
        if std::env::var_os("STEM_LIVE_DL").is_none() {
            return;
        }
        let dir = default_models_dir();
        ensure_models_dir(&dir).unwrap();
        let cancel = crate::progress::StemExtractCancelToken::new();
        download_model(StemModel::MdxNetKaraoke, &dir, &cancel, &mut |progress| {
            eprintln!(
                "[dl] {:.0}% {} ({})",
                progress.percent, progress.detail, progress.file_name
            );
        })
        .unwrap();
        assert!(model_installed(StemModel::MdxNetKaraoke, &dir));
        assert!(verify_model_installed(StemModel::MdxNetKaraoke, &dir));
        let path = dir.join("UVR_MDXNET_KARA.onnx");
        let len = fs::metadata(&path).unwrap().len();
        eprintln!("downloaded bytes={len} path={}", path.display());
        assert_eq!(len, 29_704_436);
    }
}

//! Chromium Embedded Framework views for Futureboard Studio.
//!
//! The CEF SDK is intentionally not downloaded by normal workspace builds.
//! Run the `install_cef` example with the `installer` feature to populate the
//! workspace-local `build/cef/<version>/<platform>` directory, then enable
//! `cef-runtime` in the executable that owns the CEF process lifecycle.
//!
//! On Windows and macOS built-in editors are native CEF child windows
//! ([`runtime::RenderMode::Windowed`]); Linux uses windowless/off-screen
//! rendering (see [`osr`]), where accelerated D3D11 shared textures and
//! software BGRA frames remain available.

use std::path::{Path, PathBuf};

use thiserror::Error;

/// CEF release selected for every supported desktop target.
pub const CEF_SHORT_VERSION: &str = "150.0.11";
pub const CEF_VERSION: &str = "150.0.11+gb887805";
pub const CHROMIUM_VERSION: &str = "150.0.7871.115";

pub const WINDOWS_X86_64_URL: &str = "https://cef-builds.spotifycdn.com/cef_binary_150.0.11%2Bgb887805%2Bchromium-150.0.7871.115_windows64.tar.bz2";
pub const LINUX_X86_64_URL: &str = "https://cef-builds.spotifycdn.com/cef_binary_150.0.11%2Bgb887805%2Bchromium-150.0.7871.115_linux64.tar.bz2";
pub const MACOS_X86_64_URL: &str = "https://cef-builds.spotifycdn.com/cef_binary_150.0.11%2Bgb887805%2Bchromium-150.0.7871.115_macosx64.tar.bz2";
pub const MACOS_AARCH64_URL: &str = "https://cef-builds.spotifycdn.com/cef_binary_150.0.11%2Bgb887805%2Bchromium-150.0.7871.115_macosarm64.tar.bz2";

/// Desktop CEF distributions currently pinned by Futureboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CefTarget {
    WindowsX86_64,
    LinuxX86_64,
    MacOsX86_64,
    MacOsAarch64,
}

impl CefTarget {
    pub const fn target_triple(self) -> &'static str {
        match self {
            Self::WindowsX86_64 => "x86_64-pc-windows-msvc",
            Self::LinuxX86_64 => "x86_64-unknown-linux-gnu",
            Self::MacOsX86_64 => "x86_64-apple-darwin",
            Self::MacOsAarch64 => "aarch64-apple-darwin",
        }
    }

    pub const fn archive_url(self) -> &'static str {
        match self {
            Self::WindowsX86_64 => WINDOWS_X86_64_URL,
            Self::LinuxX86_64 => LINUX_X86_64_URL,
            Self::MacOsX86_64 => MACOS_X86_64_URL,
            Self::MacOsAarch64 => MACOS_AARCH64_URL,
        }
    }

    pub const fn archive_name(self) -> &'static str {
        match self {
            Self::WindowsX86_64 => {
                "cef_binary_150.0.11+gb887805+chromium-150.0.7871.115_windows64.tar.bz2"
            }
            Self::LinuxX86_64 => {
                "cef_binary_150.0.11+gb887805+chromium-150.0.7871.115_linux64.tar.bz2"
            }
            Self::MacOsX86_64 => {
                "cef_binary_150.0.11+gb887805+chromium-150.0.7871.115_macosx64.tar.bz2"
            }
            Self::MacOsAarch64 => {
                "cef_binary_150.0.11+gb887805+chromium-150.0.7871.115_macosarm64.tar.bz2"
            }
        }
    }

    pub const fn directory_name(self) -> &'static str {
        match self {
            Self::WindowsX86_64 => "cef_windows_x86_64",
            Self::LinuxX86_64 => "cef_linux_x86_64",
            Self::MacOsX86_64 => "cef_macos_x86_64",
            Self::MacOsAarch64 => "cef_macos_aarch64",
        }
    }

    /// SHA-256 of the archive at [`Self::archive_url`].
    ///
    /// The CEF CDN index publishes a SHA-1 of the same object it serves. That
    /// pair is not a pin: whoever can change the archive can change the hash.
    /// These digests are the check install and release packaging use.
    pub const fn archive_sha256(self) -> &'static str {
        match self {
            Self::WindowsX86_64 => {
                "19df14cd13c8d491077fef921dbb9730a702c26bf5eee1ba7497178bb13df981"
            }
            Self::LinuxX86_64 => "d9f203726d42f17866c0085c7dde0297d428051cfaa9d07870c08fbec0cea755",
            Self::MacOsX86_64 => "5f792edce31cf696bdb44fbb5c8a9eafb01d3a623083ca11e35700e9ff3c57be",
            Self::MacOsAarch64 => {
                "fbd7652b5ac4224e446fdde909fa2fcd88ac75c173ef042363738a86d5ad3f0a"
            }
        }
    }

    /// SHA-256 of [`Self::runtime_library`] after the archive is unpacked.
    ///
    /// A restored `build/cef` tree is not re-downloaded. This is the check
    /// that tree still contains the runtime the pin describes.
    pub const fn runtime_sha256(self) -> &'static str {
        match self {
            Self::WindowsX86_64 => {
                "15e5158c92373c209a9e8687fc382bca96147641dff93f29b711af923d397a16"
            }
            Self::LinuxX86_64 => "28c7671aaea559b4fda0b9e252c0acd7289f89c3637001e2a0e844ebb706ddce",
            Self::MacOsX86_64 => "108d999fffeddbdc6f345d71d15e061ef7f23ad9be2edd856dff1901b8377f02",
            Self::MacOsAarch64 => {
                "1d2a7d2b9ea4bc420c62afa804fe587d0a7e40c123fcc61b23bc0f20274fd185"
            }
        }
    }

    pub fn from_target_triple(target: &str) -> Result<Self, CefDistributionError> {
        match target {
            "x86_64-pc-windows-msvc" => Ok(Self::WindowsX86_64),
            "x86_64-unknown-linux-gnu" => Ok(Self::LinuxX86_64),
            "x86_64-apple-darwin" => Ok(Self::MacOsX86_64),
            "aarch64-apple-darwin" => Ok(Self::MacOsAarch64),
            _ => Err(CefDistributionError::UnsupportedTarget(target.to_owned())),
        }
    }

    pub fn current() -> Result<Self, CefDistributionError> {
        #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
        return Ok(Self::WindowsX86_64);
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        return Ok(Self::LinuxX86_64);
        #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
        return Ok(Self::MacOsX86_64);
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        return Ok(Self::MacOsAarch64);
        #[allow(unreachable_code)]
        Err(CefDistributionError::UnsupportedTarget(format!(
            "{}-{}",
            std::env::consts::ARCH,
            std::env::consts::OS
        )))
    }

    fn runtime_library(self) -> &'static str {
        match self {
            Self::WindowsX86_64 => "libcef.dll",
            Self::LinuxX86_64 => "libcef.so",
            Self::MacOsX86_64 | Self::MacOsAarch64 => {
                "Chromium Embedded Framework.framework/Chromium Embedded Framework"
            }
        }
    }
}

/// Returns the versioned, platform-specific path consumed by `cef-dll-sys`.
pub fn cef_path(workspace_root: impl AsRef<Path>, target: CefTarget) -> PathBuf {
    workspace_root
        .as_ref()
        .join("build")
        .join("cef")
        .join(CEF_SHORT_VERSION)
        .join(target.directory_name())
}

/// Resolves the Futureboard workspace from this crate's compile-time location.
pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("SphereWebView must remain under <workspace>/crates")
        .to_path_buf()
}

pub fn workspace_cef_path(target: CefTarget) -> PathBuf {
    cef_path(workspace_root(), target)
}

/// Verifies that a prepared SDK has the headers, CMake metadata, archive
/// manifest and target runtime required by cef-rs.
pub fn validate_cef_path(
    path: impl AsRef<Path>,
    target: CefTarget,
) -> Result<(), CefDistributionError> {
    let path = path.as_ref();
    for relative in [
        Path::new("archive.json"),
        Path::new("CMakeLists.txt"),
        Path::new("include/cef_app.h"),
        Path::new("include/cef_version.h"),
        Path::new(target.runtime_library()),
    ] {
        let candidate = path.join(relative);
        if !candidate.is_file() {
            return Err(CefDistributionError::MissingFile(candidate));
        }
    }

    let version_header = path.join("include/cef_version.h");
    let version_contents = std::fs::read_to_string(&version_header)?;
    let expected_version = format!("#define CEF_VERSION \"{CEF_VERSION}+");
    if !version_contents.contains(&expected_version) {
        return Err(CefDistributionError::VersionMismatch {
            path: version_header,
            expected: CEF_VERSION,
        });
    }

    let archive_path = path.join("archive.json");
    let archive_contents = std::fs::read_to_string(&archive_path)?;
    if !archive_contents.contains(&format!("cef_binary_{CEF_VERSION}+")) {
        return Err(CefDistributionError::VersionMismatch {
            path: archive_path,
            expected: CEF_VERSION,
        });
    }
    Ok(())
}

/// Hash `path` as SHA-256, lowercase hex.
pub fn sha256_file(path: &Path) -> Result<String, CefDistributionError> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// Confirm an already-unpacked distribution is the pinned runtime.
///
/// File presence and the version string in `cef_version.h` are not enough:
/// a restored CI cache can carry a matching tree of different bytes.
pub fn verify_pinned_runtime(
    path: impl AsRef<Path>,
    target: CefTarget,
) -> Result<(), CefDistributionError> {
    let path = path.as_ref();
    validate_cef_path(path, target)?;
    let runtime = path.join(target.runtime_library());
    let actual = sha256_file(&runtime)?;
    let expected = target.runtime_sha256();
    if actual != expected {
        return Err(CefDistributionError::Integrity {
            kind: "runtime",
            target: target.target_triple().to_owned(),
            expected,
        });
    }
    Ok(())
}

/// Name of the per-file SHA-256 manifest the installer writes at the root of
/// a distribution it unpacked from the pinned archive.
pub const INSTALL_MANIFEST: &str = "futureboard-cef-sha256.txt";
const MANIFEST_HEADER: &str = "# futureboard cef install manifest v1";

/// Render the install manifest for the tree at `root`: one
/// `<sha256>  <relative path>` line per file, sorted, below a header that names
/// the pinned archive the tree came from.
pub fn render_install_manifest(
    root: &Path,
    target: CefTarget,
) -> Result<String, CefDistributionError> {
    let mut out = format!("{MANIFEST_HEADER}\n# archive {}\n", target.archive_sha256());
    for (relative, digest) in manifest_entries(root)? {
        out.push_str(&digest);
        out.push_str("  ");
        out.push_str(&relative);
        out.push('\n');
    }
    Ok(out)
}

/// Check every file of an installed distribution, not just `libcef`.
///
/// `libcef` is checked against the repository pin. Every other file
/// (`chrome_elf`, ANGLE, SwiftShader, `.pak`/`.dat`/snapshot, locales, the
/// macOS framework libraries) is checked against [`INSTALL_MANIFEST`], which
/// the installer wrote right after it verified the archive SHA-256. A missing,
/// changed or extra file fails, as does a tree without a manifest.
///
/// Limitation: only `libcef` is pinned in the repository. The manifest lives
/// in the cached tree, so it catches corrupted, partial or swapped files, but
/// not someone who can rewrite both a file and the manifest in that cache.
pub fn verify_installed_distribution(
    path: impl AsRef<Path>,
    target: CefTarget,
) -> Result<(), CefDistributionError> {
    let path = path.as_ref();
    verify_pinned_runtime(path, target)?;
    check_install_manifest(path, target)
}

fn check_install_manifest(path: &Path, target: CefTarget) -> Result<(), CefDistributionError> {
    let manifest_path = path.join(INSTALL_MANIFEST);
    if !manifest_path.is_file() {
        return Err(CefDistributionError::MissingFile(manifest_path));
    }
    let recorded = std::fs::read_to_string(&manifest_path)?;
    let actual = render_install_manifest(path, target)?;
    if recorded == actual {
        return Ok(());
    }
    // Name the first difference so the failure is actionable.
    let parse = |text: &str| -> std::collections::BTreeMap<String, String> {
        text.lines()
            .filter(|line| !line.starts_with('#'))
            .filter_map(|line| line.split_once("  "))
            .map(|(digest, relative)| (relative.to_owned(), digest.to_owned()))
            .collect()
    };
    let recorded_header = recorded.lines().take(2).collect::<Vec<_>>();
    let actual_header = actual.lines().take(2).collect::<Vec<_>>();
    let (recorded, actual) = (parse(&recorded), parse(&actual));
    let (relative, detail) = if recorded_header != actual_header {
        (INSTALL_MANIFEST.to_owned(), "for a different archive")
    } else if let Some(relative) = recorded.keys().find(|key| !actual.contains_key(*key)) {
        (relative.clone(), "missing")
    } else if let Some(relative) = actual.keys().find(|key| !recorded.contains_key(*key)) {
        (relative.clone(), "not in the install manifest")
    } else if let Some(relative) = recorded.keys().find(|key| recorded[*key] != actual[*key]) {
        (relative.clone(), "modified")
    } else {
        (INSTALL_MANIFEST.to_owned(), "malformed")
    };
    Err(CefDistributionError::ManifestMismatch {
        path: path.join(relative),
        detail,
    })
}

fn manifest_entries(root: &Path) -> Result<Vec<(String, String)>, CefDistributionError> {
    fn walk(
        root: &Path,
        dir: &Path,
        out: &mut Vec<(String, String)>,
    ) -> Result<(), CefDistributionError> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .map_err(|_| CefDistributionError::MissingFile(path.clone()))?
                .components()
                .map(|part| part.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            if relative == INSTALL_MANIFEST {
                continue;
            }
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                // Recorded by link text; links are not followed.
                use sha2::{Digest, Sha256};
                let target = std::fs::read_link(&path)?;
                let digest = Sha256::digest(target.to_string_lossy().as_bytes())
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>();
                out.push((relative, format!("link:{digest}")));
            } else if kind.is_dir() {
                walk(root, &path, out)?;
            } else {
                out.push((relative, sha256_file(&path)?));
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(root, root, &mut out)?;
    out.sort();
    Ok(out)
}

#[derive(Debug, Error)]
pub enum CefDistributionError {
    #[error("unsupported CEF target: {0}")]
    UnsupportedTarget(String),
    #[error("CEF distribution is missing required file: {0}")]
    MissingFile(PathBuf),
    #[error("CEF distribution at {path} does not match pinned version {expected}")]
    VersionMismatch {
        path: PathBuf,
        expected: &'static str,
    },
    #[error("CEF destination already exists: {0}")]
    DestinationExists(PathBuf),
    #[error("CEF {kind} for {target} does not match pinned SHA-256 {expected}")]
    Integrity {
        kind: &'static str,
        target: String,
        expected: &'static str,
    },
    #[error("CEF file {path} is {detail} (install manifest check)")]
    ManifestMismatch { path: PathBuf, detail: &'static str },
    #[error("CEF install I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[cfg(feature = "installer")]
    #[error("CEF download or extraction failed: {0}")]
    Download(#[from] download_cef::Error),
    #[cfg(feature = "installer")]
    #[error("CEF HTTP request failed: {0}")]
    Http(#[from] Box<ureq::Error>),
}

#[cfg(feature = "installer")]
mod installer;
#[cfg(feature = "installer")]
pub use installer::{install_cef, install_cef_target};

#[cfg(feature = "cef-runtime")]
pub mod client;

#[cfg(feature = "cef-runtime")]
pub mod osr;

#[cfg(feature = "cef-runtime")]
pub mod runtime;

#[cfg(feature = "cef-runtime")]
pub mod scheme;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_supported_targets_to_the_pinned_archives() {
        let cases = [
            ("x86_64-pc-windows-msvc", "windows64", "cef_windows_x86_64"),
            ("x86_64-unknown-linux-gnu", "linux64", "cef_linux_x86_64"),
            ("x86_64-apple-darwin", "macosx64", "cef_macos_x86_64"),
            ("aarch64-apple-darwin", "macosarm64", "cef_macos_aarch64"),
        ];
        for (triple, archive_platform, directory_name) in cases {
            let target = CefTarget::from_target_triple(triple).unwrap();
            assert_eq!(target.target_triple(), triple);
            assert_eq!(target.directory_name(), directory_name);
            assert!(target.archive_url().contains(archive_platform));
            assert!(target.archive_name().contains(archive_platform));
        }
    }

    #[test]
    fn cef_paths_are_versioned_and_platform_specific() {
        assert_eq!(
            cef_path(Path::new("workspace"), CefTarget::WindowsX86_64),
            Path::new("workspace")
                .join("build")
                .join("cef")
                .join("150.0.11")
                .join("cef_windows_x86_64")
        );
        assert_eq!(
            cef_path(Path::new("workspace"), CefTarget::LinuxX86_64),
            Path::new("workspace")
                .join("build")
                .join("cef")
                .join("150.0.11")
                .join("cef_linux_x86_64")
        );
    }

    #[test]
    fn pinned_cef_digests_are_sha256_hex() {
        for target in [
            CefTarget::WindowsX86_64,
            CefTarget::LinuxX86_64,
            CefTarget::MacOsX86_64,
            CefTarget::MacOsAarch64,
        ] {
            for digest in [target.archive_sha256(), target.runtime_sha256()] {
                assert_eq!(digest.len(), 64, "{target:?} {digest}");
                assert!(
                    digest
                        .chars()
                        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
                    "{target:?} {digest}"
                );
            }
            assert_ne!(target.archive_sha256(), target.runtime_sha256());
        }
    }

    #[test]
    fn sha256_file_matches_known_digest() {
        let path = std::env::temp_dir().join(format!("fb-cef-sha-{}", std::process::id()));
        std::fs::write(&path, b"abc").unwrap();
        let digest = sha256_file(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            digest,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn install_manifest_catches_missing_changed_and_extra_files() {
        let root = std::env::temp_dir().join(format!("fb-cef-manifest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("locales")).unwrap();
        std::fs::write(root.join("chrome_elf.dll"), b"elf").unwrap();
        std::fs::write(root.join("locales/en-US.pak"), b"pak").unwrap();
        let target = CefTarget::WindowsX86_64;
        let manifest = root.join(INSTALL_MANIFEST);

        // A tree that was never installed through the pinned archive.
        assert!(matches!(
            check_install_manifest(&root, target),
            Err(CefDistributionError::MissingFile(path)) if path == manifest
        ));

        std::fs::write(&manifest, render_install_manifest(&root, target).unwrap()).unwrap();
        check_install_manifest(&root, target).unwrap();
        let text = std::fs::read_to_string(&manifest).unwrap();
        assert!(text.contains(target.archive_sha256()));
        assert!(text.contains("  locales/en-US.pak\n"));

        std::fs::write(root.join("chrome_elf.dll"), b"swapped").unwrap();
        assert_eq!(detail_for(&root, target), "modified");
        std::fs::write(root.join("chrome_elf.dll"), b"elf").unwrap();
        std::fs::write(root.join("locales/xx.pak"), b"extra").unwrap();
        assert_eq!(detail_for(&root, target), "not in the install manifest");
        std::fs::remove_file(root.join("locales/xx.pak")).unwrap();
        std::fs::remove_file(root.join("locales/en-US.pak")).unwrap();
        assert_eq!(detail_for(&root, target), "missing");
        std::fs::write(root.join("locales/en-US.pak"), b"pak").unwrap();
        check_install_manifest(&root, target).unwrap();
        // A manifest written for another pinned archive does not vouch for this one.
        assert_eq!(
            detail_for(&root, CefTarget::LinuxX86_64),
            "for a different archive"
        );
        let _ = std::fs::remove_dir_all(&root);

        fn detail_for(root: &Path, target: CefTarget) -> &'static str {
            match check_install_manifest(root, target) {
                Err(CefDistributionError::ManifestMismatch { detail, .. }) => detail,
                other => panic!("expected a manifest mismatch, got {other:?}"),
            }
        }
    }

    #[test]
    fn unsupported_target_is_explicit() {
        assert!(matches!(
            CefTarget::from_target_triple("wasm32-unknown-unknown"),
            Err(CefDistributionError::UnsupportedTarget(_))
        ));
    }
}

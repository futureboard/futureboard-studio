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
    fn unsupported_target_is_explicit() {
        assert!(matches!(
            CefTarget::from_target_triple("wasm32-unknown-unknown"),
            Err(CefDistributionError::UnsupportedTarget(_))
        ));
    }
}

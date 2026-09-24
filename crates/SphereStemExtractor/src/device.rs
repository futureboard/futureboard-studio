use std::sync::atomic::{AtomicU8, Ordering};

use serde::{Deserialize, Serialize};

use crate::error::StemExtractError;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StemPlatform {
    Windows,
    MacOs,
    Linux,
    Other,
}

impl StemPlatform {
    pub fn label(self) -> &'static str {
        match self {
            Self::Windows => "Windows",
            Self::MacOs => "macOS",
            Self::Linux => "Linux",
            Self::Other => "this platform",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StemPlatformRuntime {
    OnnxCpu,
    CoreMl,
}

impl StemPlatformRuntime {
    pub fn label(self) -> &'static str {
        match self {
            Self::OnnxCpu => "ONNX Runtime (CPU)",
            Self::CoreMl => "Apple Core ML",
        }
    }

    pub fn device(self) -> InferDevice {
        match self {
            Self::OnnxCpu => InferDevice::Cpu,
            Self::CoreMl => InferDevice::Gpu,
        }
    }
}

pub fn current_stem_platform() -> StemPlatform {
    #[cfg(target_os = "windows")]
    return StemPlatform::Windows;
    #[cfg(target_os = "macos")]
    return StemPlatform::MacOs;
    #[cfg(target_os = "linux")]
    return StemPlatform::Linux;
    #[allow(unreachable_code)]
    StemPlatform::Other
}

pub fn resolve_platform_runtime(
    platform: StemPlatform,
) -> Result<StemPlatformRuntime, StemExtractError> {
    match platform {
        StemPlatform::Windows => Ok(StemPlatformRuntime::OnnxCpu),
        StemPlatform::MacOs => Ok(StemPlatformRuntime::CoreMl),
        StemPlatform::Linux | StemPlatform::Other => {
            Err(StemExtractError::UnsupportedPlatform { platform })
        }
    }
}

pub fn resolve_current_platform_runtime() -> Result<StemPlatformRuntime, StemExtractError> {
    resolve_platform_runtime(current_stem_platform())
}

/// Application-provided GPU probe result: 0 = not probed, 1 = none, 2 = present.
static GPU_PROBE: AtomicU8 = AtomicU8::new(0);

/// Record whether the application detected a usable hardware GPU (e.g. from
/// wgpu adapter enumeration during startup). Once set, this is the
/// authoritative answer for [`gpu_available`], enabling automatic GPU stem
/// extraction without a per-machine heuristic guess.
pub fn set_gpu_detected(present: bool) {
    GPU_PROBE.store(if present { 2 } else { 1 }, Ordering::Relaxed);
}

/// Inference device for MDX-NET stem extraction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum InferDevice {
    #[default]
    Cpu,
    Gpu,
}

impl InferDevice {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Gpu => "gpu",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Cpu => "CPU",
            Self::Gpu => "GPU",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "cpu" | "CPU" => Some(Self::Cpu),
            "gpu" | "GPU" | "cuda" | "directml" | "metal" => Some(Self::Gpu),
            _ => None,
        }
    }
}

/// Best-effort GPU availability probe for the Stem Extractor dialog.
///
/// Returns true when a known accelerator hint is present. Absence does not
/// prove a GPU cannot work after the ONNX/DirectML/CUDA runtime is installed;
/// the UI still offers GPU and falls back with a clear error when unavailable.
pub fn gpu_available() -> bool {
    if std::env::var_os("FUTUREBOARD_STEM_FORCE_GPU").is_some() {
        return true;
    }
    if std::env::var_os("FUTUREBOARD_STEM_FORCE_NO_GPU").is_some() {
        return false;
    }
    // The application's startup GPU enumeration is authoritative once it runs.
    match GPU_PROBE.load(Ordering::Relaxed) {
        2 => return true,
        1 => return false,
        _ => {}
    }
    // Lightweight hints only (before the app probes) — no CUDA/DirectML library
    // load on the UI path.
    std::path::Path::new("/dev/nvidia0").exists()
        || std::path::Path::new("/dev/dxg").exists()
        || std::env::var_os("CUDA_PATH").is_some()
        || std::env::var_os("CUDA_VISIBLE_DEVICES").is_some_and(|v| !v.is_empty())
}

/// Resolve the requested device, optionally falling back to CPU when GPU is
/// unavailable and `allow_cpu_fallback` is true.
pub fn resolve_device(
    requested: InferDevice,
    allow_cpu_fallback: bool,
) -> Result<InferDevice, StemExtractError> {
    match requested {
        InferDevice::Cpu => Ok(InferDevice::Cpu),
        InferDevice::Gpu if gpu_available() => Ok(InferDevice::Gpu),
        InferDevice::Gpu if allow_cpu_fallback => Ok(InferDevice::Cpu),
        InferDevice::Gpu => Err(StemExtractError::DeviceUnavailable {
            device: InferDevice::Gpu,
            reason: "no GPU accelerator was detected for MDX-NET inference".into(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_runtime_selects_windows_onnx_cpu_and_macos_core_ml() {
        assert_eq!(
            resolve_platform_runtime(StemPlatform::Windows).unwrap(),
            StemPlatformRuntime::OnnxCpu
        );
        assert_eq!(
            resolve_platform_runtime(StemPlatform::MacOs).unwrap(),
            StemPlatformRuntime::CoreMl
        );
    }

    #[test]
    fn platform_runtime_rejects_linux_until_onnx_cpu_is_supported() {
        assert_eq!(
            resolve_platform_runtime(StemPlatform::Linux),
            Err(StemExtractError::UnsupportedPlatform {
                platform: StemPlatform::Linux,
            })
        );
    }

    #[test]
    fn cpu_always_resolves() {
        assert_eq!(
            resolve_device(InferDevice::Cpu, false).unwrap(),
            InferDevice::Cpu
        );
    }

    #[test]
    fn gpu_falls_back_when_allowed() {
        // Isolate from host GPU hints for a deterministic assertion.
        // SAFETY: test-only env mutation; this suite runs single-threaded here.
        unsafe {
            std::env::set_var("FUTUREBOARD_STEM_FORCE_NO_GPU", "1");
            std::env::remove_var("FUTUREBOARD_STEM_FORCE_GPU");
        }
        assert_eq!(
            resolve_device(InferDevice::Gpu, true).unwrap(),
            InferDevice::Cpu
        );
        let err = resolve_device(InferDevice::Gpu, false).unwrap_err();
        assert!(matches!(err, StemExtractError::DeviceUnavailable { .. }));
        unsafe {
            std::env::remove_var("FUTUREBOARD_STEM_FORCE_NO_GPU");
        }
    }
}

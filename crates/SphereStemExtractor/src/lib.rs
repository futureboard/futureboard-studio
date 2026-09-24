//! Offline stem extraction for Futureboard Studio.
//!
//! This crate owns the MDX-NET model catalog, CPU/GPU device selection, stem
//! slot selection, ONNX package download into
//! `Documents/Futureboard Studio/Utilities/Models/`, and a buffer-in /
//! stems-out extraction API. It does **not** touch the realtime audio callback.
//!
//! Classification: scanner/offline path (worker thread only).
//!
//! When built with the `onnx` feature and the model's ONNX weights are
//! installed, real MDX-NET inference runs through ONNX Runtime
//! ([`backend::OnnxMdxBackend`]) on CPU or, with the `cuda` / `directml` /
//! `coreml` features, GPU. Otherwise it falls back to a deterministic spectral
//! stub ([`backend::SpectralStubBackend`]) so the UI/job pipeline still runs.
//! The ONNX Runtime shared library is loaded dynamically; set `ORT_DYLIB_PATH`
//! to a (optionally GPU-enabled) `onnxruntime` build to select it at runtime.

pub mod backend;
pub mod device;
pub mod download;
pub mod error;
pub mod extractor;
pub mod model;
pub mod params;
pub mod progress;
pub mod stems;

pub use backend::{InferBackendKind, StemInferBackend, create_mdx_net_backend};
pub use device::{
    InferDevice, StemPlatform, StemPlatformRuntime, current_stem_platform, gpu_available,
    resolve_current_platform_runtime, resolve_device, resolve_platform_runtime, set_gpu_detected,
};
pub use download::{
    HTDEMUCS_MODEL_BASE, StemModelDownloadProgress, UVR_MODEL_RELEASE_BASE, default_models_dir,
    download_model, ensure_models_dir, model_installed, resolve_installed_model_files,
};
pub use error::StemExtractError;
pub use extractor::{StemExtractInput, StemExtractOutput, StemExtractResult, extract_stems};
pub use model::{STEM_MODELS, StemModel, StemModelFile, StemModelInfo, StemModelPackage};
pub use params::{StemExtractParams, StemExtractQuality};
pub use progress::{StemExtractCancelToken, StemExtractProgress, StemExtractStage};
pub use stems::{StemKind, StemSet};

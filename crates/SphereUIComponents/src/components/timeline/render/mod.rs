//! Hybrid timeline rendering: immutable snapshots + pluggable backends.
//!
//! - [`TimelineViewport`] — scroll/zoom bounds for the arrangement region
//! - [`TimelineRenderSnapshot`] — read-only frame description (built on UI thread)
//! - [`TimelineRenderer`] — GPUI paint or offscreen WGPU draw
//!
//! Normal UI (menus, dialogs, headers, lanes as interactive GPUI elements) stays
//! in GPUI; dense paint (grid, future clip/waveform batches) routes here.

pub mod clip_geometry;
pub mod gpui_paint;
pub mod renderer;
pub mod snapshot;
pub mod viewport;
#[cfg(feature = "gpu-renderer")]
pub mod wgpu_renderer;

pub use gpui_paint::GpuiPaintTimelineRenderer;
pub use renderer::{
    create_timeline_renderer, create_timeline_renderer_with_fallback, set_preferred_backend,
    TimelineRenderOutput, TimelineRenderer, TimelineRendererBackend,
};
pub use snapshot::{
    BarShadeSnapshot, GridLineSnapshot, PlayheadSnapshot, RenderClipKind, RenderClipSnapshot,
    RenderLaneSnapshot, SelectionSnapshot, SnapshotBuildOptions, TimelineRenderSnapshot,
    VisibleBeatRange, VisibleTrackRange, WaveformChunkHandle, WaveformReadyKind,
};
pub use viewport::TimelineViewport;
#[cfg(feature = "gpu-renderer")]
pub use wgpu_renderer::{
    cached_gpu_devices, detect_gpu_class, gpu_devices, list_available_gpu_devices,
    set_preferred_gpu_device_id, GpuDeviceInfo, TimelineGpuPreference, WgpuOffscreenFrame,
    WgpuTimelineRenderer,
};

#[cfg(not(feature = "gpu-renderer"))]
pub fn set_preferred_gpu_device_id(_id: &str) {}

/// Without the `gpu-renderer` feature there is no adapter enumeration, so the
/// UI keeps its normal render cost rather than guessing at the hardware.
#[cfg(not(feature = "gpu-renderer"))]
pub fn detect_gpu_class() -> crate::perf::GpuClass {
    crate::perf::GpuClass::Unknown
}

/// Stub used when the `gpu-renderer` cargo feature isn't enabled so the
/// settings UI can still compile and show a flat "GPU unavailable" state
/// without conditional code on every call site.
#[cfg(not(feature = "gpu-renderer"))]
#[derive(Debug, Clone)]
pub struct GpuDeviceInfo {
    pub id: String,
    pub name: String,
    pub backend: Option<String>,
    pub device_type: Option<String>,
    pub vendor_id: Option<u32>,
    pub device_id: Option<u32>,
}

#[cfg(not(feature = "gpu-renderer"))]
pub fn list_available_gpu_devices() -> Vec<GpuDeviceInfo> {
    Vec::new()
}

#[cfg(not(feature = "gpu-renderer"))]
pub fn gpu_devices() -> std::sync::Arc<Vec<GpuDeviceInfo>> {
    std::sync::Arc::new(Vec::new())
}

#[cfg(not(feature = "gpu-renderer"))]
pub fn cached_gpu_devices() -> Option<std::sync::Arc<Vec<GpuDeviceInfo>>> {
    Some(gpu_devices())
}

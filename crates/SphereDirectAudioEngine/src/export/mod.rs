//! Offline arrangement export.
//!
//! This module renders the audible arrangement *offline* (no audio device, no
//! realtime callback) by driving the same render kernel the cpal callback uses
//! — [`render_project_block_interleaved`](crate::engine::render_project_block_interleaved)
//! and [`schedule_midi_render_block`](crate::engine::schedule_midi_render_block)
//! — over a plain [`EngineProjectSnapshot`](crate::types::EngineProjectSnapshot).
//!
//! Boundaries (see CLAUDE.md): the renderer here produces interleaved PCM
//! frames; `sphere_encoder` turns them into a container; the UI/layout builds
//! the snapshot + request. No FFmpeg, no external processes, no realtime device.
//!
//! Plugin support: in-process inserts render directly. Callers may also supply
//! the live external bridge endpoints; the export worker then uses a blocking
//! handshake while the realtime callback remains wait-free.

mod exporter;
mod level_meter;
mod offline_renderer;
#[cfg(test)]
pub(crate) use offline_renderer::{make_track_snapshot, silence_snapshot};
mod render_progress;
mod render_request;

pub use exporter::{
    export_arrangement, export_arrangement_with_bridges, export_render_job,
    export_render_job_with_bridges, export_tracks_single_pass,
    export_tracks_single_pass_with_bridges, partial_path_for, record_render_job,
    ArrangementExportRequest, ArrangementExportSummary, RenderJob, TrackExportTarget,
};
pub use level_meter::{LevelMeter, LevelReport};

/// See [`crate::engine::EngineInner::render_latency_frames`].
pub(crate) fn live_render_latency_frames(
    latency_graph: &crate::latency_graph::RuntimeLatencyGraph,
    pdc_enabled: bool,
) -> u64 {
    offline_renderer::export_warmup_frames(latency_graph, pdc_enabled)
}
pub use offline_renderer::{
    render_offline, render_offline_tracks, render_offline_tracks_with_bridges,
    render_offline_with_bridges, OfflineRenderSummary,
};
pub use render_progress::{ExportCancelToken, ExportProgress, ExportStage};
pub use render_request::{
    arrangement_bounds_samples, beats_to_samples, ExportNormalizeMode, ExportTailMode,
    OfflineRenderRequest,
};

/// Errors surfaced by the offline export pipeline. Never panics across the
/// render/encode loop; every failure is one of these.
#[derive(Debug)]
pub enum ExportError {
    /// The runtime graph could not be built from the snapshot.
    Build(String),
    /// The encoder rejected the spec or failed to write.
    Encode(sphere_encoder::EncodeError),
    Io(std::io::Error),
    /// The user cancelled; partial output has been removed.
    Cancelled,
    /// Request/settings were invalid.
    Settings(String),
}

impl std::fmt::Display for ExportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Build(message) => write!(f, "render graph build failed: {message}"),
            Self::Encode(error) => write!(f, "encode failed: {error}"),
            Self::Io(error) => write!(f, "io error: {error}"),
            Self::Cancelled => write!(f, "export cancelled"),
            Self::Settings(message) => write!(f, "invalid export settings: {message}"),
        }
    }
}

impl std::error::Error for ExportError {}

impl From<std::io::Error> for ExportError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<sphere_encoder::EncodeError> for ExportError {
    fn from(value: sphere_encoder::EncodeError) -> Self {
        Self::Encode(value)
    }
}

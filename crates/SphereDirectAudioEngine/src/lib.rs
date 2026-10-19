//! SphereDirectAudioEngine (DAUx) — the native realtime audio engine.
//!
//! The native shell drives it through the [`AudioEngine`] facade below.
//!
//! Thread safety contract:
//!   - The underlying `EngineInner` is `Send + Sync` — its hot-path state is
//!     accessed by the audio thread via atomics and a lock-free channel.
//!   - Calls that touch the stream (open/close/start/stop) hold a
//!     `parking_lot::Mutex` for the duration — not realtime-safe, so they
//!     run on control threads only.

#![deny(clippy::all)]
#![allow(non_snake_case)] // lib name "DirectAudio" is intentional branding

mod analysis_tap;
#[cfg(target_os = "windows")]
pub mod asio_registry;
mod audio_file;
mod audio_graph;
mod audio_source;
pub mod backend;
pub mod capture_tap;
pub mod clap_processor;
mod command;
pub mod device;
mod dsp;
pub mod editor_chrome;
pub mod engine;
pub mod error;
pub mod export;
pub mod forensic_trace;
mod graph;
mod graveyard;
pub mod input_ring;
/// Audio Jam bridge: lock-free rings between the jam network threads and
/// the realtime callback. See [`jam_bus`] for why the engine owns them.
pub mod jam_bus;
mod latency_graph;
pub mod loopback;
pub mod monitor;
pub mod native;
/// Multi-core processing: the worker pool the render pass spreads track
/// insert chains over.
mod parallel;
pub mod plugin_backend;
pub mod plugin_bridge;
pub mod recording;
pub mod render_capture;
mod runtime;
mod streaming_source;
pub mod tempo_map;
pub mod time_signature_map;
pub mod transport;
pub mod types;
pub mod visualizer_tap;
pub mod vst2_processor;
pub mod vst3_processor;

// ── Native Rust facade ───────────────────────────────────────────────────
//
// Re-exports so the Rust-Native shell can write:
//
//     use sphere_directaudioengine::{AudioEngine, EngineConfig, AudioBackend, EngineStats};
//
// without reaching into the engine's internal modules.
pub use crate::analysis_tap::{analysis_tap, clip_id_hash, AnalysisTap};
pub use crate::audio_file::{
    generate_audio_peaks, load_audio_bytes, load_audio_file, load_audio_file_for_edit,
    probe_audio_file, AudioFileBuffer, AudioFileFormat, AudioFileInfo, AudioPeak, AudioPeakFile,
    AudioPeakLod, AUDITION_PREVIEW_SECONDS, MAX_EDIT_DECODE_BYTES, MAX_IN_MEMORY_DECODE_BYTES,
    PEAK_LOD_LEVELS, STREAMING_WAV_THRESHOLD_BYTES,
};
pub use crate::audio_graph::{
    plan_runtime_audio_graph, AudioGraphNode, AudioGraphNodeKind, GraphRouteIssue, GraphRouteKind,
    GraphValidationError, RuntimeAudioGraph,
};
pub use crate::audio_source::{
    open_clip_audio_source, read_frame_stereo, sample_source_stereo, ClipAudioSource,
    MappedWavSource,
};
pub use crate::engine::{DropoutDiagnostics, DropoutProtectionMode, DropoutReason};
/// Instruction set of the render kernels: AVX2 by default, SSE as the fallback.
pub use crate::dsp::simd::{
    active_level as active_simd_level, cpu_supports_avx2, set_simd_level, SimdLevel,
};
pub use crate::error::SphereAudioError;
pub use crate::export::{
    arrangement_bounds_samples, beats_to_samples, export_arrangement,
    export_arrangement_with_bridges, export_render_job, export_render_job_with_bridges,
    export_tracks_single_pass, export_tracks_single_pass_with_bridges, partial_path_for,
    record_render_job, render_offline, render_offline_tracks, ArrangementExportRequest,
    ArrangementExportSummary, ExportCancelToken, ExportError, ExportNormalizeMode, ExportProgress,
    ExportStage, ExportTailMode, LevelReport, OfflineRenderRequest, OfflineRenderSummary,
    RenderJob, TrackExportTarget,
};
pub use crate::jam_bus::{
    is_jam_device, jam_device_id, jam_stream_id, JamAudioBus, JamChannelMode, JamInputSlot,
    JamPublishSlot, JAM_DEVICE_PREFIX,
};
pub use crate::latency_graph::{
    apply_pdc_delay_block, plan_runtime_latency_graph, strip_plugin_latency_samples,
    RuntimeLatencyGraph,
};
pub use crate::native::{
    asio_support_enabled, AudioBackend, AudioDeviceId, AudioEngine, EngineConfig,
    EngineDebugSnapshot, EngineDeviceInfo, EngineInsertStatus, EngineStats, DEFAULT_BUFFER_SIZE,
    DEFAULT_SAMPLE_RATE,
};
pub use crate::parallel::{
    auto_processing_threads, configure_multicore, multicore_status, MulticoreStatus,
    MAX_PROCESSING_THREADS,
};
pub use crate::plugin_backend::PluginModuleFormat;
/// Shared automation curve shaping — the UI lane renderer calls this so the drawn
/// curve matches realtime playback and offline export exactly.
pub use crate::runtime::automation_curve_factor;
pub use crate::visualizer_tap::{TapRead, VisualizerTap, visualizer_tap};
// ARA renderers are built by the app (which owns the ARA document) and installed
// with `AudioEngine::set_ara_renderers`, so the type has to be nameable outside
// this crate.
pub use crate::runtime::RuntimeAraRenderer;
pub use crate::tempo_map::{
    RuntimeTempoMapSnapshot, TempoCurve, TempoMap, TempoPoint, TempoSegment,
};
pub use crate::transport::RuntimeTransportSnapshot;
pub use crate::vst3_processor::{
    AraMainFactory, RuntimeTransportContext, Vst3MidiEvent, Vst3MidiEventKind, Vst3PluginState,
    Vst3RuntimeProcessor, VST3_KEY_COMMAND, VST3_KEY_SHIFT,
};

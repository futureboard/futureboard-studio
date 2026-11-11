//! Runtime playback graph sent to the CPAL callback.
//!
//! The control thread builds this from an `EngineProjectSnapshot`, including
//! decoding supported media files.  The audio thread then owns a local clone of
//! the graph and can render without touching locks or parsing JSON.

use std::collections::HashMap;

use std::path::PathBuf;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use std::sync::{Arc, Mutex, OnceLock};

use std::time::UNIX_EPOCH;

use fbmx_runtime::{AudioModel, FbmxModel, LstmRuntime};

use solfege_audio::SampleRate;

use solfege_core::{BowedStringConfig, RuntimeInstrument};

use solfege_engine::sfm::SfmMode;

use solfege_engine::{EngineConfig as SolfegeEngineConfig, SamplerEngine, SharedMetrics};

use solfege_event::Event as SolfegeEvent;

use solfege_model::SfmFile;

use sphere_midi_service::mpe::{MpeChannelAllocator, MpeZone};

use crate::audio_graph::{plan_runtime_audio_graph, GraphValidationError, RuntimeAudioGraph};

use crate::audio_source::{open_clip_audio_source, ClipAudioSource};

use crate::latency_graph::{
    plan_runtime_latency_graph, recompute_runtime_latency_graph, resolve_latency_routing_indices,
    RuntimeLatencyGraph,
};

use serde_json::Value;

use sphere_audio_plugins::{canonical_plugin_id, AudioPluginDspState};

use sphere_soundfont_player::{
    SoundFont, SoundfontChannels, SoundfontEnvelope, SoundfontPlayer, SoundfontPlayerMode,
    SoundfontPlayerSettings, SoundfontRenderQuality,
};

use SphereAudioProcessor::{
    create_stretch_processor, effective_pitch_ratio, effective_time_ratio, resolve_backend,
    source_read_rate_for_repitch, stretched_duration_samples, DenoiseProcessor, StretchAlgorithm,
    StretchBackend, StretchMode, StretchParams, StretchProcessor,
};

use crate::tempo_map::{RuntimeTempoMapSnapshot, TempoMap, TempoPoint};

use crate::types::{
    EngineAutomationLaneSnapshot, EngineClipAudioProcess, EngineClipSnapshot,
    EngineMidiClipSnapshot, EngineProjectSnapshot, EngineSolfegeSnapshot, EngineTrackSnapshot,
};

use crate::vst3_processor::{vst3_midi_debug_enabled, Vst3MidiEvent, Vst3RuntimeProcessor};

// ── Submodules ─────────────────────────────────────────────────────────────────

mod soundfont;
pub use self::soundfont::*;
mod solfege;
pub use self::solfege::*;
mod track;
pub use self::track::*;
mod automation;
pub use self::automation::*;
mod inserts;
pub use self::inserts::*;
mod clip;
pub use self::clip::*;
mod warp;
pub use self::warp::*;
mod clip_build;
use self::clip_build::*;
mod midi;
pub use self::midi::*;
mod midi_build;
pub use self::midi_build::*;
mod solo;
pub(crate) use self::solo::*;
mod project_buffers;
mod project_build;
mod project_latency;
mod project_meters;
mod project_midi;
mod project_params;
mod project_preview;
mod project_routing;

/// `FUTUREBOARD_MIDI_ENGINE_DEBUG=1` enables eprintln traces for MIDI runtime
/// build + per-block scheduling. Cached on first read so the audio callback
/// never touches the environment.
pub fn midi_engine_debug_enabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| {
        std::env::var_os("FUTUREBOARD_FORENSIC_TRACE").is_some()
            || std::env::var_os("FUTUREBOARD_MIDI_ENGINE_DEBUG").is_some()
    })
}

/// `FUTUREBOARD_PDC_DEBUG=1` (or `FUTUREBOARD_ROUTING_DEBUG=1`) enables the
/// latency-refresh traces. Cached on first read: the refresh runs per block on
/// the audio thread, and `std::env::var_os` there takes the environment lock.
pub fn pdc_debug_enabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| {
        std::env::var_os("FUTUREBOARD_PDC_DEBUG").is_some()
            || std::env::var_os("FUTUREBOARD_ROUTING_DEBUG").is_some()
    })
}

/// Verbose MIDI/bridge tracing (off by default — safe for realtime audio).
pub fn midi_verbose_enabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| {
        std::env::var_os("FUTUREBOARD_FORENSIC_TRACE").is_some()
            || std::env::var_os("FUTUREBOARD_MIDI_VERBOSE").is_some()
    })
}

/// Read every cached flag of this module once, on a control thread: the first
/// read takes the environment (and allocates), which must not happen in the
/// callback. See `crate::engine::prime_audio_thread_resources`.
pub(crate) fn prime_debug_flags() {
    let _ = midi_engine_debug_enabled();
    let _ = pdc_debug_enabled();
    let _ = midi_verbose_enabled();
    let _ = clip_dsp_debug_enabled();
    let _ = signalsmith_enabled();
}

const DEFAULT_AUDIO_BLOCK_CAPACITY: usize = 8192;

#[derive(Debug, Clone, Default)]
pub struct RuntimeProject {
    pub sample_rate: u32,
    pub tracks: Vec<RuntimeTrack>,
    pub clips: Vec<RuntimeClip>,
    pub has_solo: bool,
    /// Authoritative hold-mode tempo map for beat/time/sample conversion.
    pub tempo_map: RuntimeTempoMapSnapshot,
    /// A second copy of [`Self::tempo_map`], made by the control thread for
    /// the metronome when this graph is sent to a running stream, so the
    /// graph swap hands it over instead of cloning on the audio thread. `None`
    /// on a graph nobody sends (the control-thread mirror, offline renders).
    pub metronome_tempo_map: Option<RuntimeTempoMapSnapshot>,
    /// Structural MIDI clips (logging / inspection).
    pub midi_clips: Vec<RuntimeMidiClip>,
    /// Per-track scheduling state driven by the audio callback.
    pub midi_tracks: Vec<RuntimeMidiTrack>,
    /// Per-track index of the bridged-instrument track that feeds it as a VSTi
    /// multi-out child ("Out Ch") strip, or `None` for every ordinary track.
    /// Resolved once in [`Self::resolve_indices`] so the render pass can ask
    /// "is my parent instrument soloed?" without scanning tracks × inserts ×
    /// child routes per block. Parallel to [`Self::tracks`].
    pub vsti_output_parent_indices: Vec<Option<usize>>,
    /// Precomputed pass order and routing validation (Phase O).
    pub audio_graph: RuntimeAudioGraph,
    /// Latency propagation and PDC delays (Phase V/W).
    pub latency_graph: RuntimeLatencyGraph,
    /// Effective playback PDC flag used when this runtime graph was built.
    pub pdc_enabled: bool,
    /// Bridge handshake period, frames: the configured device buffer size each
    /// bridged insert's one-block handshake is charged at
    /// ([`crate::plugin_bridge::bridged_insert_latency_samples`]). The live
    /// callback sets it from [`crate::engine::SharedState::bridge_period_frames_for_block`];
    /// 0 means not set yet, and the render pass then takes its first block.
    pub bridge_period_frames: u32,
    /// Times the audio thread has recomputed the compensation plan
    /// ([`Self::refresh_runtime_latency_graph`]). Diagnostics only.
    pub latency_replan_count: u64,
    /// When set, the render path ramps track/master fader gains across each block
    /// toward their target. Playback and offline export enable this so fader
    /// automation follows the same envelope. See `apply_fader`.
    pub fader_smoothing: bool,
    /// Smoothed master gain, ramped toward the live master volume each block when
    /// [`Self::fader_smoothing`] is set. Mirrors per-track `smoothed_gain_*`.
    pub smoothed_master_gain: f32,
    /// Stage 3b: realtime sinks for external plugin-host DSP output, keyed by
    /// insert `id` (one region + handshake per insert). Set via
    /// [`crate::command::EngineCommand::SetPluginBridgeSink`]
    /// and preserved across project reloads. Empty until the bridge installs one.
    pub plugin_bridge_sinks: std::collections::HashMap<
        String,
        std::sync::Arc<dyn crate::plugin_bridge::PluginBridgeSink>,
    >,
    /// Tracks whose external-bridge plugin editor is open. Bookkeeping for the
    /// UI/bridge handshake; it no longer decides whether the graph renders,
    /// because the graph renders whenever the engine does.
    pub bridge_editor_active: std::collections::HashSet<String>,
    /// Control Room / Listen Bus configuration and per-block scratch. Read
    /// only by the realtime device callback — see [`RuntimeMonitor`].
    pub monitor: RuntimeMonitor,
    /// Each track's bank/program selection and whether its instrument has
    /// been sent it — see [`RuntimeProject::flush_midi_programs`].
    pub midi_programs: Vec<RuntimeMidiProgram>,
    /// The realtime render being recorded, if one is. Set on the control
    /// thread for every graph it builds while a capture is active.
    pub render_capture: Option<Arc<crate::render_capture::RenderCapture>>,
    /// The take recording loopback-routed tracks, if one is running. Set on the
    /// control thread for every graph it builds while the take runs, like
    /// `render_capture`; see [`crate::loopback::LoopbackRecorder`].
    pub loopback_record: Option<Arc<crate::loopback::LoopbackRecorder>>,
    /// The spatial mix: format, room, the multichannel bus spatialised
    /// channels sum into, and how a surround bus reaches a stereo device.
    pub spatial: RuntimeSpatial,
}

#[inline]
fn seconds_to_samples(seconds: f64, sample_rate: u32) -> u64 {
    (seconds * sample_rate as f64).round().max(0.0) as u64
}

#[inline]
fn f32_store(v: f32) -> u32 {
    v.to_bits()
}

#[inline]
fn f32_load(v: u32) -> f32 {
    f32::from_bits(v)
}

#[cfg(test)]
mod track_input_source_tests;

#[cfg(test)]
mod warp_map_tests;

#[cfg(test)]
mod stretch_runtime_tests;

#[cfg(test)]
mod pdc_reset_tests;

#[cfg(test)]
mod midi_tests;

#[cfg(test)]
mod loopback_resolve_tests;

#[cfg(test)]
mod stream_lane_tests;

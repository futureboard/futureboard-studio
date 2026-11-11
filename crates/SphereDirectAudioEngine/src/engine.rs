//! Core engine logic.
//!
//! `EngineInner` is the real audio engine.  It is wrapped in `Arc` and shared
//! between the N-API class (on the JS/main thread) and the audio callback
//! (on cpal's realtime thread).
//!
//! Thread safety:
//!   - All control parameters are sent through a `crossbeam_channel` and
//!     consumed at the top of each audio block — no locking in the hot path.
//!   - Meter output uses `AtomicU32` (f32 bit-cast) — both sides access with
//!     `Relaxed` ordering, which is sufficient for meter display purposes.
//!   - Stream lifecycle (open/close) is guarded by `parking_lot::Mutex`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, OnceLock};

use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{BufferSize, FromSample, Sample, SampleFormat, SizedSample};
use crossbeam_channel::{bounded, Receiver, Sender};
use parking_lot::Mutex;
use sphere_audio_plugins::{canonical_plugin_id, process_stereo_sample};

use crate::audio_file::AuditionPlayer;
use crate::audio_graph::is_routing_track_type;
use crate::audio_source::{sample_source_stereo, ClipAudioSource};
#[cfg(target_os = "windows")]
use crate::backend::wasapi_exclusive::{self, WasapiExclusiveHandle};
#[cfg(target_os = "windows")]
use crate::backend::wdm_ks::{self, WdmKsHandle};
use crate::backend::{
    cpal_backend::{self, CpalStreamHandle},
    list_available_backends, BackendKind, DauxDeviceConfig,
};
use crate::command::EngineCommand;
use crate::device;
use crate::dsp::{meter::smooth_peak, oscillator::SineOscillator};
use crate::error::SphereAudioError;
use crate::graph::{MasterState, TrackState};
use crate::latency_graph::apply_pdc_delay_block;
use crate::recording::{self, RecordingSession};
use crate::runtime::{
    ClipDspProcessor, RuntimeInsert, RuntimePreviewMode, RuntimeProject, RuntimeTrack,
    RuntimeTrackInputSource,
};
use crate::tempo_map::{TempoMap, TempoPoint};
use crate::transport::{self, RuntimeTransportSnapshot};
use crate::types::{
    EngineProjectSnapshot, EngineStatus, EngineTrackInputSourceSnapshot, JsAudioDeviceInfo,
    JsDauxBackendInfo, JsDauxConfig, JsDauxStatus, JsDeviceOpenConfig, JsEngineDebugInfo,
    JsMeterSnapshot, JsPluginOutputMeterSnapshot, JsRecordingResult, JsRecordingStatus,
    JsSphereAudioStatus, JsStartRecordingConfig, JsTrackMeterSnapshot,
};
use crate::vst3_processor::RuntimeTransportContext;

// ── Version ───────────────────────────────────────────────────────────────────

pub const ENGINE_VERSION: &str = "0.1.0";

/// Keep a few sources that just disappeared from the arrangement so a normal
/// delete/undo cycle can reuse the open mapping or decoded buffer. Active
/// sources do not count toward this limit.
const MAX_INACTIVE_AUDIO_CACHE_ENTRIES: usize = 4;

fn command_debug_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("FUTUREBOARD_AUDIO_COMMAND_DEBUG").is_some())
}

/// `FUTUREBOARD_INPUT_DEBUG=1` enables throttled raw-input-peak traces from the
/// control thread (see `log_input_debug_throttled`). Off by default.
fn input_debug_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("FUTUREBOARD_INPUT_DEBUG").is_some())
}

/// `FUTUREBOARD_AUDIO_CALLBACK_DEBUG=1` enables the realtime callback's
/// occasional eprintln traces (graph swap, mute, render-path). Off by default
/// so the audio thread never formats strings or writes to stdio — see
/// `tasks/native/audio-system-spec.md` §1 and Phase A finding A.2.2.
fn callback_debug_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("FUTUREBOARD_AUDIO_CALLBACK_DEBUG").is_some())
}

fn plugin_restore_debug_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("FUTUREBOARD_PLUGIN_DEBUG").is_some())
}

/// `FUTUREBOARD_PLUGIN_BRIDGE_DEBUG=1` enables the throttled bridge
/// missed-deadline / recovered traces from the audio callback. Off by default —
/// stall accounting stays in `RuntimeInsert::bridge_missed_blocks` either way.
fn bridge_debug_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("FUTUREBOARD_PLUGIN_BRIDGE_DEBUG").is_some())
}

fn log_sphere_audio_processor_diagnostics_once() {
    static LOGGED: OnceLock<()> = OnceLock::new();
    LOGGED.get_or_init(|| {
        eprintln!(
            "SphereAudioProcessor:\n- InternalRePitch: available\n- Signalsmith Stretch: {}",
            if SphereAudioProcessor::signalsmith_stretch_available() {
                "available"
            } else {
                "unavailable"
            }
        );
    });
}

// ── Realtime constants shared with render.rs ──────────────────────────────────

pub const TEST_TONE_AMPLITUDE: f32 = 0.125; // −18 dBFS  (safe default test level)
pub const PEAK_DECAY: f32 = 0.94; // per audio block, responsive UI peak decay

// ── Atomic helpers (pub for render.rs) ────────────────────────────────────────

#[inline]
pub fn f32_store(v: f32) -> u32 {
    v.to_bits()
}
#[inline]
pub fn f32_load(v: u32) -> f32 {
    f32::from_bits(v)
}

/// Drain preview bins in `[from_index, head)` from one ring. Cheap clone on
/// the control thread; never blocks the audio path. Shared by the legacy
/// single-track `drain_recording_preview_peaks` and the per-track
/// `drain_recording_preview_peaks_for_track`.
fn drain_preview_ring(
    ring: &crate::input_ring::PreviewPeakRing,
    from_index: f64,
) -> Vec<crate::types::JsWaveformPeak> {
    let head = ring.head();
    let mut from = from_index.max(0.0) as u64;
    // If the consumer lagged past the ring window, clamp to what's retained.
    let cap = crate::input_ring::PreviewPeakRing::default_capacity();
    if head.saturating_sub(from) > cap {
        from = head.saturating_sub(cap);
    }
    let mut out = Vec::with_capacity((head.saturating_sub(from)) as usize);
    let mut i = from;
    while i < head {
        let p = ring.read(i);
        out.push(crate::types::JsWaveformPeak {
            min: p.min as f64,
            max: p.max as f64,
            rms: p.rms as f64,
        });
        i += 1;
    }
    out
}

#[inline]
fn monitor_channel_pair(channels: &[u32]) -> Option<(u32, u32)> {
    channels
        .first()
        .copied()
        .map(|left| (left, channels.get(1).copied().unwrap_or(left)))
}

#[inline]
const fn pack_monitor_source_pair(left: u32, right: u32) -> u64 {
    left as u64 | ((right as u64) << 32)
}

#[inline]
const fn unpack_monitor_source_pair(pair: u64) -> (u32, u32) {
    (pair as u32, (pair >> 32) as u32)
}

#[cfg(any(test, all(target_os = "windows", feature = "asio")))]
fn routed_input_peaks(input_peaks: &[f32], channels: &[u32]) -> Option<(f32, f32)> {
    let (left, right) = monitor_channel_pair(channels)?;
    let peak_l = input_peaks.get(left as usize).copied().unwrap_or(0.0);
    let peak_r = input_peaks.get(right as usize).copied().unwrap_or(0.0);
    Some((peak_l, peak_r))
}

#[inline]
pub(crate) fn atomic_max_f32_bits(target: &AtomicU32, value: f32) {
    let value = value.max(0.0);
    let mut current = target.load(Ordering::Relaxed);
    loop {
        if value <= f32::from_bits(current) {
            break;
        }
        match target.compare_exchange_weak(
            current,
            value.to_bits(),
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => break,
            Err(next) => current = next,
        }
    }
}

// ── Submodules ─────────────────────────────────────────────────────────────────
//
// `EngineInner`'s methods live in one module per concern, each its own
// `impl EngineInner` block; the struct and its constructor stay here. Items
// other modules name (`SharedState`, the diagnostics types, the stream
// builder) are re-exported, so their paths are unchanged.

mod diagnostics;
pub use self::diagnostics::*;
mod shared_state;
pub use self::shared_state::*;
mod stream;
use self::stream::*;

mod audition;
mod commands;
mod devices;
mod inserts;
mod jam;
mod lifecycle;
mod live_input;
mod meters;
mod mixer;
mod preview;
mod project;
mod recording_api;
mod transport_control;

// The structure key's tests reach it through this module.
#[cfg(test)]
use self::project::*;

// DSP / render kernel, re-exported so `crate::engine::*` call sites keep
// resolving unchanged.
mod render;
pub use self::render::*;

// ── Engine inner ───────────────────────────────────────────────────────────────

/// Active stream variant — cpal-backed, ASIO duplex session, or a raw
/// Windows backend thread.
enum ActiveStream {
    Cpal(CpalStreamHandle),
    #[cfg(all(target_os = "windows", feature = "asio"))]
    // Boxed: the duplex handle (with its per-channel meter bank) is much
    // larger than the other variants (clippy::large_enum_variant).
    AsioDuplex(Box<crate::backend::asio_session::AsioDuplexHandle>),
    #[cfg(target_os = "windows")]
    WasapiExclusive(WasapiExclusiveHandle),
    #[cfg(target_os = "windows")]
    WdmKs(WdmKsHandle),
}

impl ActiveStream {
    fn cmd_tx(&self) -> Option<&crossbeam_channel::Sender<EngineCommand>> {
        match self {
            ActiveStream::Cpal(h) => Some(&h.cmd_tx),
            #[cfg(all(target_os = "windows", feature = "asio"))]
            ActiveStream::AsioDuplex(h) => Some(&h.cmd_tx),
            #[cfg(target_os = "windows")]
            ActiveStream::WasapiExclusive(h) => Some(&h.cmd_tx),
            #[cfg(target_os = "windows")]
            ActiveStream::WdmKs(h) => Some(&h.cmd_tx),
        }
    }
    fn play(&self) -> Result<(), String> {
        match self {
            ActiveStream::Cpal(h) => h.play(),
            #[cfg(all(target_os = "windows", feature = "asio"))]
            ActiveStream::AsioDuplex(h) => h.play(),
            #[cfg(target_os = "windows")]
            ActiveStream::WasapiExclusive(_) => Ok(()), // already playing from stream start
            #[cfg(target_os = "windows")]
            ActiveStream::WdmKs(_) => Ok(()), // already playing from stream start
        }
    }
    fn pause(&self) -> Result<(), String> {
        match self {
            ActiveStream::Cpal(h) => h.pause(),
            #[cfg(all(target_os = "windows", feature = "asio"))]
            ActiveStream::AsioDuplex(h) => h.pause(),
            #[cfg(target_os = "windows")]
            ActiveStream::WasapiExclusive(_) => Ok(()), // no pause in exclusive — caller mutes output
            #[cfg(target_os = "windows")]
            ActiveStream::WdmKs(_) => Ok(()), // no pause in low-level backend — caller mutes output
        }
    }
    #[cfg(all(target_os = "windows", feature = "asio"))]
    fn as_asio_duplex(&self) -> Option<&crate::backend::asio_session::AsioDuplexHandle> {
        match self {
            ActiveStream::AsioDuplex(h) => Some(h.as_ref()),
            _ => None,
        }
    }
    #[allow(dead_code)]
    fn sample_rate(&self) -> u32 {
        match self {
            ActiveStream::Cpal(h) => h.sample_rate,
            #[cfg(all(target_os = "windows", feature = "asio"))]
            ActiveStream::AsioDuplex(h) => h.sample_rate,
            #[cfg(target_os = "windows")]
            ActiveStream::WasapiExclusive(h) => h.sample_rate,
            #[cfg(target_os = "windows")]
            ActiveStream::WdmKs(h) => h.sample_rate,
        }
    }
    #[allow(dead_code)]
    fn buffer_size(&self) -> u32 {
        match self {
            ActiveStream::Cpal(h) => h.buffer_size,
            #[cfg(all(target_os = "windows", feature = "asio"))]
            ActiveStream::AsioDuplex(h) => h.buffer_size,
            #[cfg(target_os = "windows")]
            ActiveStream::WasapiExclusive(h) => h.buffer_size,
            #[cfg(target_os = "windows")]
            ActiveStream::WdmKs(h) => h.buffer_size,
        }
    }
    #[allow(dead_code)]
    fn device_name(&self) -> &str {
        match self {
            ActiveStream::Cpal(h) => &h.device_name,
            #[cfg(all(target_os = "windows", feature = "asio"))]
            ActiveStream::AsioDuplex(h) => &h.device_name,
            #[cfg(target_os = "windows")]
            ActiveStream::WasapiExclusive(h) => &h.device_name,
            #[cfg(target_os = "windows")]
            ActiveStream::WdmKs(h) => &h.device_name,
        }
    }
    fn backend_name(&self) -> &str {
        match self {
            ActiveStream::Cpal(h) => &h.backend_name,
            #[cfg(all(target_os = "windows", feature = "asio"))]
            ActiveStream::AsioDuplex(_) => "DAUx ASIO",
            #[cfg(target_os = "windows")]
            ActiveStream::WasapiExclusive(_) => "DAUx WASAPI Exclusive",
            #[cfg(target_os = "windows")]
            ActiveStream::WdmKs(_) => "DAUx WDM-KS",
        }
    }
}

// Safety: same rationale as before — all access is on the JS/main thread under Mutex.
unsafe impl Send for ActiveStream {}
unsafe impl Sync for ActiveStream {}

/// Live input-level test stream (Settings "Test Input" button). Holds the
/// cpal input stream and a shared peak atomic the callback writes and the UI
/// polls. Stored only inside `EngineInner` and touched only on the control
/// thread, so it inherits `EngineInner`'s `Send`/`Sync` assertion.
struct InputTestHandle {
    _stream: cpal::Stream,
    peak: Arc<AtomicU32>,
}

/// Outcome of trying to run a take on the already-open capture stream.
///
/// `Unavailable` is not an error: it means this route cannot serve the take and
/// the own-stream fallback should. `Failed` is an error the fallback would hit
/// too, so it is reported rather than retried.
enum LiveTapStart {
    Started,
    Unavailable,
    Failed(SphereAudioError),
}

struct LiveInputHandle {
    _stream: cpal::Stream,
    device_id: Option<String>,
    /// Interleaved channel count and rate the stream actually opened at. A take
    /// tapping this stream records exactly this geometry, so it has to be known
    /// before the writers are built.
    channels: usize,
    sample_rate: u32,
    /// Installs and removes a take's record fan-out without touching the
    /// stream. See [`crate::capture_tap`] — this is why Record and Stop no
    /// longer open and close a capture client mid-session.
    tap: crate::capture_tap::CaptureTapControl,
}

/// Plug-in parameter edits since the snapshots that may still be loading.
///
/// A project sync builds its graph from a snapshot taken on the UI thread,
/// then swaps it in from a background thread. A parameter edit made in
/// between already went to the running graph, but the new graph — built from
/// the older snapshot — would drop it again (#365). Each edit is recorded
/// here with a revision; the UI reads [`EngineInner::insert_param_revision`]
/// as it takes a snapshot, and `load_project_as_of` replays every newer edit
/// into the new graph before it is swapped in.
#[derive(Debug, Default)]
struct InsertParamJournal {
    revision: u64,
    /// The newest edit of each `(track, insert, param)`.
    edits: Vec<JournaledInsertParam>,
}

#[derive(Debug)]
struct JournaledInsertParam {
    revision: u64,
    track_id: String,
    insert_id: String,
    param_id: String,
    value: f32,
}

impl InsertParamJournal {
    /// Edits kept between project syncs. Each key holds one entry, so this is
    /// only reached by a session touching that many distinct parameters with
    /// no sync between; the oldest edit then goes first.
    const MAX_EDITS: usize = 1024;

    fn record(&mut self, track_id: &str, insert_id: &str, param_id: &str, value: f32) {
        self.revision += 1;
        let revision = self.revision;
        if let Some(edit) = self.edits.iter_mut().find(|edit| {
            edit.param_id == param_id && edit.insert_id == insert_id && edit.track_id == track_id
        }) {
            edit.revision = revision;
            edit.value = value;
            return;
        }
        if self.edits.len() >= Self::MAX_EDITS {
            if let Some(oldest) = self
                .edits
                .iter()
                .enumerate()
                .min_by_key(|(_, edit)| edit.revision)
                .map(|(index, _)| index)
            {
                self.edits.swap_remove(oldest);
            }
        }
        self.edits.push(JournaledInsertParam {
            revision,
            track_id: track_id.to_string(),
            insert_id: insert_id.to_string(),
            param_id: param_id.to_string(),
            value,
        });
    }

    /// Apply every edit newer than `as_of` to a graph built from a snapshot
    /// taken at `as_of`, then forget the edits that snapshot already holds.
    fn replay_into(&mut self, runtime: &mut RuntimeProject, as_of: u64, sample_rate: u32) {
        for edit in self.edits.iter().filter(|edit| edit.revision > as_of) {
            let insert = runtime
                .tracks
                .iter_mut()
                .find(|track| track.id == edit.track_id)
                .and_then(|track| {
                    track
                        .inserts
                        .iter_mut()
                        .find(|insert| insert.id == edit.insert_id)
                });
            if let Some(insert) = insert {
                // The whole graph is about to be swapped in: its own state is
                // the change, so the prepared command half is not needed.
                let _ = stage_insert_param(insert, &edit.param_id, edit.value, sample_rate);
            }
        }
        self.edits.retain(|edit| edit.revision > as_of);
    }
}

/// Apply a parameter edit to a control-thread copy of an insert and prepare
/// what the audio thread needs to apply the same edit to its own copy.
fn stage_insert_param(
    insert: &mut RuntimeInsert,
    param_id: &str,
    value: f32,
    sample_rate: u32,
) -> (
    crate::command::ResolvedParamId,
    Option<Box<crate::command::InsertParamChange>>,
) {
    if param_id == "enabled" {
        insert.enabled = value >= 0.5;
        let mut new_params = (*insert.params).clone();
        new_params.insert("enabled".to_string(), serde_json::Value::from(value as f64));
        insert.params = std::sync::Arc::new(new_params);
        (crate::command::ResolvedParamId::Bypass, None)
    } else if insert.kind_tag == crate::runtime::RuntimeInsertKind::ExternalBridge {
        let wire_id = param_id.parse::<u32>().ok();
        let mut new_params = (*insert.params).clone();
        new_params.insert(param_id.to_string(), serde_json::Value::from(value as f64));
        insert.params = std::sync::Arc::new(new_params);
        let resolved = wire_id
            .map(crate::command::ResolvedParamId::Numeric)
            .unwrap_or_else(|| {
                crate::command::ResolvedParamId::Named(crate::command::ParamKey::new(param_id))
            });
        (resolved, None)
    } else if insert.vst3.is_some() {
        let vst3_id = param_id.parse::<u32>().ok();
        let mut new_params = (*insert.params).clone();
        new_params.insert(param_id.to_string(), serde_json::Value::from(value as f64));
        insert.params = std::sync::Arc::new(new_params);
        let resolved = vst3_id
            .map(crate::command::ResolvedParamId::Numeric)
            .unwrap_or_else(|| {
                crate::command::ResolvedParamId::Named(crate::command::ParamKey::new(param_id))
            });
        (resolved, None)
    } else {
        let mut new_params = (*insert.params).clone();
        new_params.insert(param_id.to_string(), serde_json::Value::from(value as f64));
        let plugin_id = sphere_audio_plugins::canonical_plugin_id(&insert.kind);
        if sphere_audio_plugins::should_rebuild_state(plugin_id, param_id) {
            insert.dsp.rebuild(plugin_id, &new_params, sample_rate);
        } else {
            insert.dsp.refresh_process_params(plugin_id, &new_params);
        }
        let new_params_arc = std::sync::Arc::new(new_params);
        insert.params = std::sync::Arc::clone(&new_params_arc);
        let new_dsp = insert.dsp.clone();
        let resolved = if let Ok(id) = param_id.parse::<u32>() {
            crate::command::ResolvedParamId::Numeric(id)
        } else {
            crate::command::ResolvedParamId::Named(crate::command::ParamKey::new(param_id))
        };
        (
            resolved,
            Some(Box::new(crate::command::InsertParamChange {
                params: Some(new_params_arc),
                dsp: Some(new_dsp),
            })),
        )
    }
}

pub struct EngineInner {
    // Shared atomic state
    pub shared: Arc<SharedState>,

    // Active stream (cpal or WASAPI exclusive).
    // Dropping this stops the stream.
    active_stream: Mutex<Option<ActiveStream>>,

    // Legacy cpal stream path kept for compatibility with open_device().
    stream: Mutex<Option<cpal::Stream>>,
    cmd_tx: Mutex<Option<Sender<EngineCommand>>>,

    // Non-realtime mutable status (device names, error strings, etc.)
    status: Mutex<EngineStatus>,

    // In-memory track graph — updated from project snapshots / param commands.
    tracks: Mutex<Vec<TrackState>>,
    #[allow(dead_code)]
    master: Mutex<MasterState>,

    // Last loaded project snapshot (optional, for reference/debugging).
    project: Mutex<Option<EngineProjectSnapshot>>,
    // Incremented only after an incremental arm/monitor/route transaction
    // commits. `load_project` uses this to merge a newer input edit that arrived
    // while its graph was being prepared, then serializes the command ordering
    // under `project` so stale graph loads cannot overwrite that edit.
    input_state_revision: AtomicU64,
    /// Plug-in parameter edits a loading snapshot may not hold yet, and the
    /// lock that orders `set_insert_param` against a graph swap (#365).
    /// Lock order: `project`, then this, then `runtime`.
    insert_param_journal: Mutex<InsertParamJournal>,

    // Prepared render graph shared with new streams and pushed to callbacks.
    runtime: Mutex<RuntimeProject>,
    /// Everything but the MIDI of the last snapshot `load_project` built in
    /// full, with the build parameters. A snapshot that matches it differs in
    /// MIDI only, and gets its new schedule swapped in instead of a rebuild.
    /// `None` until a full load has succeeded.
    structure_key: Mutex<Option<String>>,
    /// The tempo map the audio thread was last *sent* — not the one the last
    /// project load happened to carry. `set_tempo_map` skips a push whose
    /// segments match this, and a fresh stream is re-seeded from it, so a
    /// 120 -> 140 -> 120 sequence still reaches the callback while an unchanged
    /// map never costs a MIDI-model rebuild there.
    last_sent_tempo_map: Mutex<crate::tempo_map::RuntimeTempoMapSnapshot>,
    /// The time-signature map the audio thread was last sent. The runtime graph
    /// has no time-signature field, so without this the map has no durable home
    /// on the control side and a device reopen loses it.
    last_sent_time_signature_map: Mutex<crate::time_signature_map::RuntimeTimeSignatureMapSnapshot>,
    plugin_bridge_sinks: Mutex<crate::plugin_bridge::PluginBridgeSinkMap>,
    /// Control-thread mirror of the ARA renderers installed per track.
    ///
    /// `LoadProject` replaces the whole graph, so without this the renderers
    /// would vanish on the next project sync and every live ARA binding would be
    /// destroyed behind the user's back.
    ara_renderers: Mutex<HashMap<String, Vec<crate::runtime::RuntimeAraRenderer>>>,
    /// The renderers are lent to an offline render (see
    /// [`Self::lend_ara_renderers`]): the realtime graph holds none of them,
    /// and changes reach only the mirror until they are given back. Read and
    /// written under the `ara_renderers` lock.
    ara_lent: AtomicBool,
    /// The realtime render being recorded and the track ids its lanes carry,
    /// so every graph built while it runs is mapped to it. Also the control
    /// thread's own reference to the capture (see `SetRenderCapture`).
    render_capture: Mutex<Option<(Arc<crate::render_capture::RenderCapture>, Vec<String>)>>,
    /// The take recording loopback-routed tracks and the destination track id
    /// of each lane, so every graph built while it runs is mapped to it. Also
    /// the control thread's own reference (see `SetLoopbackRecord`), kept
    /// until the take has finalized.
    loopback_record: Mutex<Option<(Arc<crate::loopback::LoopbackRecorder>, Vec<String>)>>,
    /// The Control Room configuration as last set, applied to every graph a
    /// project sync builds (see [`MonitorMirror`]).
    monitor_mirror: Mutex<MonitorMirror>,
    audio_cache: Mutex<HashMap<String, Arc<ClipAudioSource>>>,
    inactive_audio_cache_lru: Mutex<VecDeque<String>>,

    // DAUx config & glitch counter (shared with audio thread for diagnostics).
    glitch_counter: Arc<AtomicU64>,
    daux_config: Mutex<DauxDeviceConfig>,

    // Active recording session (None when not recording).
    recording: Mutex<Option<RecordingSession>>,

    // Live input-level test stream (None when not testing input).
    input_test: Mutex<Option<InputTestHandle>>,

    // Engine-owned live input stream for armed/monitored track meters.
    live_input: Mutex<Option<LiveInputHandle>>,

    // A take that has been detached but whose files the disk writer is still
    // closing. Held here so `stop_recording` no longer has to block its caller
    // on the disk — see `begin_stop_recording` / `poll_stop_recording`.
    pending_stop: Mutex<Option<PendingStopContext>>,

    // Capabilities of the open ASIO session (None otherwise). Set by
    // `open_daux`, cleared by `close_device_inner`.
    asio_caps: Mutex<Option<crate::backend::AsioSessionCaps>>,

    // Settings input test reads the ASIO session peak instead of owning a
    // stream (true only between start/stop_input_test on the ASIO backend).
    input_test_uses_session: std::sync::atomic::AtomicBool,

    // Browser sample-preview decode requests (see `AuditionRequests`).
    audition_requests: Arc<AuditionRequests>,
}

/// Single-slot mailbox for Browser sample-preview decode requests.
///
/// Arrow-keying through a folder selects files far faster than they decode, and
/// a thread per selection let those decodes finish in any order: an older,
/// slower one could land last and audition a sample the user had already moved
/// past. One worker drains this mailbox, only the newest request survives in
/// it, and every request carries the token the callback checks before playing.
#[derive(Default)]
struct AuditionRequests {
    /// Newest un-decoded request; replaced (not queued) by a later selection.
    pending: Mutex<Option<(u64, String)>>,
    ready: parking_lot::Condvar,
    worker_running: AtomicBool,
}

/// A take that has been detached, and what placing it will need once the disk
/// writer is done. Held on the engine between `begin_stop_recording` and the
/// poll that collects it.
struct PendingStopContext {
    pending: recording::PendingRecordingStop,
    /// A loopback take's recorder, held until its files are closed so the
    /// audio thread is never the one to drop it.
    #[allow(dead_code)] // a keep-alive: dropped with the context, never read
    loopback_recorder: Option<Arc<crate::loopback::LoopbackRecorder>>,
    placement: TakePlacement,
}

impl PendingStopContext {
    fn placement(&self) -> TakePlacement {
        self.placement
    }
}

/// What placing a finished take needs, and nothing else.
///
/// Split out from the pending handle so the blocking and polling paths can
/// share one placement function without one of them having to fabricate a
/// handle it has already consumed. A take must land on the same beat whichever
/// way its stop was awaited; the cheapest way to guarantee that is for there to
/// be one function and one set of inputs.
#[derive(Clone, Copy)]
struct TakePlacement {
    armed_start_sample: u64,
    first_capture_sample: u64,
    live_tap: bool,
    /// The take recorded loopback-routed tracks from the render callback:
    /// there was no converter in the path, so no round trip to take back.
    loopback_tap: bool,
    /// The take recorded with delay compensation bypassed, so the mix the
    /// performer played to was not held back behind the slowest path.
    pdc_bypassed: bool,
    #[cfg(all(target_os = "windows", feature = "asio"))]
    asio_tap: bool,
}

#[derive(Clone)]
struct OutputStreamCandidate {
    config: cpal::StreamConfig,
    sample_format: SampleFormat,
    label: &'static str,
}

// ── Thread-safety declarations ────────────────────────────────────────────────
//
// On Windows, `cpal::Stream` (WASAPI backend) carries a
// `NotSendSyncAcrossAllPlatforms(PhantomData<*mut ()>)` marker that makes it
// `!Send`.  This is conservative — the WASAPI COM interfaces *can* be used from
// another thread as long as the stream is not concurrently accessed without
// synchronisation.
//
// Safety contract we uphold:
//   1. `EngineInner::stream` is accessed ONLY from the JS / main thread (via
//      `parking_lot::Mutex`).  No code path sends the `Mutex` guard across a
//      thread boundary.
//   2. The cpal audio callback has its OWN state captures (oscillator, local
//      vars) and accesses shared data only through `Arc<SharedState>` (which
//      is genuinely `Send + Sync` — plain atomics).  It never touches the
//      `Stream` handle.
//   3. The `Sender<EngineCommand>` inside `cmd_tx` is `Send`; it is only
//      written from the JS thread and read inside the audio callback's
//      `try_recv()`.
//
// Given these invariants, treating `EngineInner` as `Send + Sync` does not
// introduce data races.
unsafe impl Send for EngineInner {}
unsafe impl Sync for EngineInner {}

impl Default for EngineInner {
    fn default() -> Self {
        Self::new()
    }
}

/// Gated, throttled trace for [`EngineInner::insert_load`].
///
/// One line a second at most: the editor asks every poll, and a readout that is
/// wrong is wrong every poll too — the first line says as much as a thousand.
fn insert_load_trace(line: impl FnOnce() -> String) {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*ENABLED.get_or_init(|| std::env::var_os("FUTUREBOARD_PLUGIN_VIEW_DEBUG").is_some()) {
        return;
    }
    static LAST: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
    let Ok(mut last) = LAST.lock() else {
        return;
    };
    let now = std::time::Instant::now();
    if last.is_some_and(|last| now.duration_since(last) < std::time::Duration::from_secs(1)) {
        return;
    }
    *last = Some(now);
    eprintln!("[insert-load] {}", line());
}

/// Create, on the calling (control) thread, everything the audio callback
/// reaches through a process-wide lazy static, so the callback only ever finds
/// it ready: every graveyard channel and its drop-thread (#238), the analysis
/// and visualizer tap rings, and the cached debug flags (a first read takes
/// the environment, which allocates).
///
/// The one place they are primed. Called at engine creation (the Jam app
/// opens a device before it loads anything), before every device open or
/// reopen, and before commands that carry something the callback will retire.
/// Idempotent and cheap after the first call.
pub(crate) fn prime_audio_thread_resources() {
    crate::graveyard::prime();
    let _ = crate::analysis_tap::analysis_tap();
    let _ = crate::visualizer_tap::visualizer_tap();
    let _ = command_debug_enabled();
    let _ = input_debug_enabled();
    let _ = callback_debug_enabled();
    let _ = plugin_restore_debug_enabled();
    let _ = bridge_debug_enabled();
    let _ = recording_debug_enabled();
    let _ = transport_freeze_debug_enabled();
    render::prime_debug_flags();
    crate::backend::render::prime_debug_flags();
    crate::runtime::prime_debug_flags();
    crate::vst3_processor::prime_debug_flags();
    let _ = crate::forensic_trace::forensic_trace_enabled();
}

impl EngineInner {
    pub fn new() -> Self {
        log_sphere_audio_processor_diagnostics_once();
        prime_audio_thread_resources();
        // What a freshly opened stream's `LocalAudioState` also starts at, so
        // the first re-seed after a device open is a no-op rather than a push.
        let default_tempo_map = crate::tempo_map::RuntimeTempoMapSnapshot::static_tempo(120.0);
        let default_time_signature_map =
            crate::time_signature_map::RuntimeTimeSignatureMapSnapshot::static_sig(4, 4);
        Self {
            shared: Arc::new(SharedState::default()),
            active_stream: Mutex::new(None),
            stream: Mutex::new(None),
            cmd_tx: Mutex::new(None),
            status: Mutex::new(EngineStatus::default()),
            tracks: Mutex::new(Vec::new()),
            master: Mutex::new(MasterState::default()),
            project: Mutex::new(None),
            input_state_revision: AtomicU64::new(0),
            insert_param_journal: Mutex::new(InsertParamJournal::default()),
            runtime: Mutex::new(RuntimeProject::default()),
            structure_key: Mutex::new(None),
            last_sent_tempo_map: Mutex::new(default_tempo_map),
            last_sent_time_signature_map: Mutex::new(default_time_signature_map),
            plugin_bridge_sinks: Mutex::new(Default::default()),
            ara_renderers: Mutex::new(HashMap::new()),
            ara_lent: AtomicBool::new(false),
            render_capture: Mutex::new(None),
            loopback_record: Mutex::new(None),
            monitor_mirror: Mutex::new(MonitorMirror::default()),
            audio_cache: Mutex::new(HashMap::new()),
            inactive_audio_cache_lru: Mutex::new(VecDeque::new()),
            glitch_counter: Arc::new(AtomicU64::new(0)),
            daux_config: Mutex::new(DauxDeviceConfig::default()),
            recording: Mutex::new(None),
            input_test: Mutex::new(None),
            live_input: Mutex::new(None),
            pending_stop: Mutex::new(None),
            asio_caps: Mutex::new(None),
            input_test_uses_session: std::sync::atomic::AtomicBool::new(false),
            audition_requests: Arc::new(AuditionRequests::default()),
        }
    }

    #[inline]
    pub fn pdc_enabled(&self) -> bool {
        if std::env::var_os("FUTUREBOARD_PDC").is_some_and(|v| v == "0" || v == "false") {
            return false;
        }
        self.shared.pdc_enabled.load(Ordering::Relaxed)
    }

    pub fn set_pdc_enabled(&self, enabled: bool) {
        let prev = self.shared.pdc_enabled.swap(enabled, Ordering::Relaxed);
        if prev != enabled {
            // Latency-compensated graph changed; bump the generation so offline
            // export can stamp/compare the version it rendered against.
            self.shared
                .latency_graph_version
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Whether the next take bypasses delay compensation while it records.
    ///
    /// Bypassed, nothing is held back to line up with the slowest plug-in
    /// path: the mix and the click are heard the moment the transport starts
    /// and the input is monitored without the project's compensation delay.
    /// Tracks behind high-latency plug-ins are heard late by that latency for
    /// the take; compensation comes back once the transport stops.
    pub fn set_record_pdc_bypass_enabled(&self, enabled: bool) {
        self.shared
            .record_pdc_bypass_enabled
            .store(enabled, Ordering::Relaxed);
    }

    /// Settings → Audio → Shared Headroom. Takes effect on the next output
    /// block of an open WASAPI Shared stream; no reopen. See
    /// [`SharedState::shared_headroom`].
    pub fn set_shared_headroom(&self, enabled: bool) {
        self.shared
            .shared_headroom
            .store(enabled, Ordering::Relaxed);
    }

    pub fn shared_headroom(&self) -> bool {
        self.shared.shared_headroom.load(Ordering::Relaxed)
    }

    /// Monotonic generation of the realtime latency-compensation graph. Bumped
    /// whenever Global Latency Sync (PDC) is toggled. Stamped into the offline
    /// export snapshot so the exporter can report/verify it rendered against the
    /// same graph generation as realtime playback.
    #[inline]
    pub fn latency_graph_version(&self) -> u64 {
        self.shared.latency_graph_version.load(Ordering::Relaxed)
    }

    /// Snapshot the currently attached external-plugin DSP endpoints for an
    /// offline export worker. Cloning only increments the shared handles.
    pub fn plugin_bridge_sinks(&self) -> crate::plugin_bridge::PluginBridgeSinkMap {
        self.plugin_bridge_sinks.lock().clone()
    }

    /// Active Dropout Protection mode (Settings → Playback).
    pub fn dropout_protection_mode(&self) -> DropoutProtectionMode {
        DropoutProtectionMode::from_u8(self.shared.dropout_protection_mode.load(Ordering::Relaxed))
    }

    /// Set the Dropout Protection mode. Control → audio via a single relaxed
    /// store; the audio callback reads it when classifying each block.
    pub fn set_dropout_protection_mode(&self, mode: DropoutProtectionMode) {
        self.shared
            .dropout_protection_mode
            .store(mode as u8, Ordering::Relaxed);
        crate::audioguard::set_process_level(mode);
    }

    /// Snapshot the realtime dropout-protection counters for the control thread
    /// (status bar / settings). Pure atomic reads — never blocks the callback.
    pub fn dropout_diagnostics(&self) -> DropoutDiagnostics {
        let s = &self.shared;
        DropoutDiagnostics {
            protection_mode: DropoutProtectionMode::from_u8(
                s.dropout_protection_mode.load(Ordering::Relaxed),
            ),
            dropout_count: s.dropout_count.load(Ordering::Relaxed),
            last_reason: DropoutReason::from_u8(s.dropout_last_reason.load(Ordering::Relaxed)),
            callback_last_us: s.last_callback_us.load(Ordering::Relaxed),
            callback_max_us: s.max_callback_us.load(Ordering::Relaxed),
            callback_deadline_us: s.callback_deadline_us.load(Ordering::Relaxed),
            slow_callback_count: s.slow_callback_count.load(Ordering::Relaxed),
        }
    }

    /// Audioguard's state: level, render-ahead, ring misses, output guard.
    pub fn audioguard_status(&self) -> crate::audioguard::AudioguardStatus {
        crate::audioguard::AudioguardStatus::read(&self.shared)
    }

    /// Inserts Audioguard bypassed since the last call (each reported once).
    pub fn take_audioguard_tripped_inserts(&self) -> Vec<crate::audioguard::TrippedInsert> {
        crate::audioguard::take_tripped_inserts(&self.runtime.lock())
    }

    /// SoundFont voices sounding at the end of the last audio callback.
    ///
    /// Counts Futureboard's built-in SoundFont instrument only — a hosted VSTi
    /// owns its own voice pool and does not report one, so this is a floor on
    /// the project's polyphony, not a total. The transport meter says so.
    pub fn active_voice_count(&self) -> u32 {
        self.shared.active_voices.load(Ordering::Relaxed)
    }

    /// Current transport play flag (set/cleared only by Start/StopTransport).
    pub fn transport_playing(&self) -> bool {
        self.shared.playing.load(Ordering::Relaxed)
    }

    /// Current lifecycle gate of the realtime callback.
    pub fn engine_state(&self) -> AudioEngineState {
        AudioEngineState::from_u8(self.shared.engine_state.load(Ordering::Relaxed))
    }
}

/// `FUTUREBOARD_RECORDING_DEBUG=1` traces the recording session lifecycle:
/// where a take starts, where its capture ends, and which path ended it. Cached
/// so no hot path reads the environment.
/// Whether a project sync may open, close, or rebuild the standalone capture
/// stream.
///
/// One rule, named so it cannot be deleted by accident: **never while a take
/// owns the capture device.** The recording stream is the live input for the
/// duration of a take — it holds the endpoint, sets `live_input_active` and
/// fills `input_ring` — so a sync that rebuilt the standalone stream would put
/// a second client on the device and reset the ring under the recorder.
#[inline]
pub(crate) fn live_input_sync_may_touch_stream(take_owns_capture_device: bool) -> bool {
    !take_owns_capture_device
}

fn recording_debug_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("FUTUREBOARD_RECORDING_DEBUG").is_some())
}

fn transport_freeze_debug_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("FUTUREBOARD_TRANSPORT_FREEZE_DEBUG").is_some())
}

/// What each engine state means for the audio callback.
///
/// The distinction these assert is the difference between playback that
/// survives a project sync and playback that stops dead in the middle of a
/// song, so it is worth pinning rather than leaving to the one `matches!` in
/// the render path.

/// Where each of `track_ids` is in `snapshot`, which is the order the graph
/// built from it holds its tracks in.
/// The Control Room configuration as last set, kept on the control thread.
///
/// The callback's `RuntimeMonitor` is the graph's, so it is replaced with a
/// default one by every project sync; this is what puts the configuration back
/// into each new graph before it is sent. Each setter records here exactly what
/// its command does to the callback's copy.
#[derive(Default)]
struct MonitorMirror {
    source: crate::monitor::MonitorSource,
    control: crate::monitor::MonitorControl,
    output: crate::monitor::MonitorOutputTarget,
    hardware_owner: crate::monitor::HardwareOutputOwner,
    master_output: Option<(u16, u16)>,
    /// Listen taps by track id: indices move between graphs, ids do not.
    listen: HashMap<String, crate::monitor::ListenMode>,
    simulation: solfege_spatialaudio::SimulationSettings,
}

impl MonitorMirror {
    /// What `SetHardwareOutputOwnership` does on the callback.
    fn set_hardware_output_ownership(
        &mut self,
        owner: crate::monitor::HardwareOutputOwner,
        master: Option<(u16, u16)>,
        monitor: Option<(u16, u16)>,
    ) {
        self.hardware_owner = owner;
        self.master_output = master;
        if let Some((left, _right)) = monitor {
            self.output.left_channel = left;
        }
    }

    fn apply_to(&self, runtime: &mut RuntimeProject) {
        runtime.monitor.source = self.source.clone();
        runtime.monitor.control = self.control;
        runtime.monitor.output = self.output.clone();
        runtime.monitor.hardware_owner = self.hardware_owner;
        runtime.monitor.master_output = self.master_output;
        if let Some(simulator) = runtime.monitor.simulator.as_deref_mut() {
            simulator.configure_now(self.simulation);
        }
        for track in runtime.tracks.iter_mut() {
            track.listen = self
                .listen
                .get(&track.id)
                .copied()
                .unwrap_or(crate::monitor::ListenMode::Off);
        }
        // The source names a track by id; resolve it against this graph.
        runtime.resolve_indices();
    }
}

fn render_capture_indices(
    snapshot: &EngineProjectSnapshot,
    track_ids: &[String],
) -> Vec<Option<usize>> {
    track_ids
        .iter()
        .map(|id| snapshot.tracks.iter().position(|track| &track.id == id))
        .collect()
}

#[cfg(test)]
mod input_route_tests;

#[cfg(test)]
mod live_input_tests;

#[cfg(test)]
mod clip_fade_tests;

#[cfg(test)]
mod sample_rate_mismatch_tests;

#[cfg(test)]
mod clip_stretch_dsp_tests;

#[cfg(test)]
mod bridge_insert_tests;

#[cfg(test)]
mod routing_tests;

#[cfg(test)]
mod dropout_tests;

#[cfg(test)]
mod recording_preview_multi_track_tests;

#[cfg(test)]
mod engine_state_rendering_tests;

#[cfg(test)]
mod structure_key_tests;

#[cfg(test)]
mod render_position_tests;

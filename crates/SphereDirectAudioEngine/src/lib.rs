//! SphereDirectAudioEngine — N-API entry point.
//!
//! Exposes a single JS class `SphereDirectAudioEngine` that wraps the Rust
//! engine core behind a thread-safe `Arc<EngineInner>`.
//!
//! All public methods on the class are callable from the Electron main process
//! (or any Node.js environment) through the native `.node` addon.
//!
//! Thread safety contract:
//!   - The class instance may be accessed from the JS thread only; napi-rs
//!     enforces this.
//!   - The underlying `EngineInner` is `Send + Sync` — its hot-path state is
//!     accessed by the cpal audio thread via atomics and a lock-free channel.
//!   - Calls that touch the stream (open/close/start/stop) hold a
//!     `parking_lot::Mutex` for the duration — not realtime-safe, but they
//!     run only on the JS thread.

#![deny(clippy::all)]
#![allow(clippy::needless_pass_by_value)] // napi-rs requires owned String args
#![allow(non_snake_case)] // lib name "DirectAudio" is intentional branding

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
pub mod plugin_backend;
pub mod plugin_bridge;
pub mod recording;
mod runtime;
mod streaming_source;
pub mod tempo_map;
pub mod time_signature_map;
pub mod transport;
pub mod types;
pub mod vst2_processor;
pub mod vst3_processor;

// ── Native Rust facade ───────────────────────────────────────────────────
//
// Re-exports so the Rust-Native shell can write:
//
//     use sphere_directaudioengine::{AudioEngine, EngineConfig, AudioBackend, EngineStats};
//
// without reaching into the NAPI-flavored modules. Both this facade and
// the `SphereDirectAudioEngine` NAPI class wrap the same `EngineInner`.
pub use crate::audio_file::{
    generate_audio_peaks, load_audio_file, probe_audio_file, AudioFileBuffer, AudioFileFormat,
    AudioFileInfo, AudioPeak, AudioPeakFile, AudioPeakLod, AUDITION_PREVIEW_SECONDS,
    MAX_IN_MEMORY_DECODE_BYTES, PEAK_LOD_LEVELS, STREAMING_WAV_THRESHOLD_BYTES,
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
pub use crate::error::SphereAudioError;
pub use crate::export::{
    arrangement_bounds_samples, beats_to_samples, export_arrangement,
    export_arrangement_with_bridges, export_tracks_single_pass,
    export_tracks_single_pass_with_bridges, partial_path_for, render_offline,
    render_offline_tracks, ArrangementExportRequest, ArrangementExportSummary, ExportCancelToken,
    ExportError, ExportNormalizeMode, ExportProgress, ExportStage, ExportTailMode,
    OfflineRenderRequest, OfflineRenderSummary, TrackExportTarget,
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
pub use crate::plugin_backend::PluginModuleFormat;
/// Shared automation curve shaping — the UI lane renderer calls this so the drawn
/// curve matches realtime playback and offline export exactly.
pub use crate::runtime::automation_curve_factor;
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
    Vst3RuntimeProcessor,
};

#[cfg(feature = "napi")]
use napi_derive::napi;

#[cfg(feature = "napi")]
use std::sync::Arc;

#[cfg(feature = "napi")]
use engine::EngineInner;

#[cfg(feature = "napi")]
use types::{
    EngineProjectSnapshot, JsAudioDeviceInfo, JsAudioFileInfo, JsDauxBackendInfo, JsDauxConfig,
    JsDauxStatus, JsDeviceOpenConfig, JsEngineDebugInfo, JsLatencyInfo, JsMeterSnapshot,
    JsRecordingResult, JsRecordingStatus, JsSphereAudioStatus, JsStartRecordingConfig,
    JsWavExportResult, JsWavPeakResult,
};

// ── N-API class ───────────────────────────────────────────────────────────────

/// The main audio engine class exposed to Node.js.
///
/// Lifecycle:
/// ```js
/// const engine = new SphereDirectAudioEngine();
/// await engine.openDevice({ sampleRate: 44100, bufferSize: 256 });
/// engine.start();          // start audio stream (silent until play or test tone)
/// engine.setTestTone(true, 440);
/// engine.stop();           // pause stream
/// engine.closeDevice();
/// ```
#[cfg(feature = "napi")]
#[napi]
pub struct SphereDirectAudioEngine {
    inner: Arc<EngineInner>,
}

#[cfg(feature = "napi")]
impl Default for SphereDirectAudioEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "napi")]
#[napi]
impl SphereDirectAudioEngine {
    /// Create a new engine instance.  The audio stream is **not** started
    /// automatically — call `openDevice()` then `start()`.
    #[napi(constructor)]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(EngineInner::new()),
        }
    }

    // ── Version / Status ─────────────────────────────────────────────────────

    /// Return the engine version string (e.g. `"0.1.0"`).
    #[napi]
    pub fn get_version(&self) -> String {
        self.inner.get_version()
    }

    /// Return a status snapshot — device names, sample rate, running state, etc.
    #[napi]
    pub fn get_status(&self) -> JsSphereAudioStatus {
        self.inner.get_status()
    }

    /// Return built-in SphereAudioPlugins descriptors as JSON.
    ///
    /// The renderer can use this to build extension-style plugin browsers while
    /// DAUx uses the same IDs in the realtime insert chain.
    #[napi]
    pub fn get_builtin_audio_plugins_json(&self) -> napi::Result<String> {
        serde_json::to_string(&sphere_audio_plugins::builtin_descriptors()).map_err(|error| {
            napi::Error::new(
                napi::Status::GenericFailure,
                format!("Failed to serialize built-in audio plugin descriptors: {error}"),
            )
        })
    }

    // ── Device enumeration ───────────────────────────────────────────────────

    /// List all available audio input devices on the system.
    #[napi]
    pub fn list_input_devices(&self) -> Vec<JsAudioDeviceInfo> {
        self.inner.list_input_devices()
    }

    /// List all available audio output devices on the system.
    #[napi]
    pub fn list_output_devices(&self) -> Vec<JsAudioDeviceInfo> {
        self.inner.list_output_devices()
    }

    // ── Stream lifecycle ─────────────────────────────────────────────────────

    /// Open (or re-open) the audio output stream with the given configuration.
    ///
    /// Closes any previously open stream first.
    /// Call `start()` afterwards to begin audio output.
    ///
    /// Throws if the device is not found or the stream cannot be created.
    #[napi]
    pub fn open_device(&self, config: JsDeviceOpenConfig) -> napi::Result<()> {
        self.inner.open_device(config).map_err(Into::into)
    }

    /// Stop and close the audio stream, freeing the device.
    #[napi]
    pub fn close_device(&self) {
        self.inner.close_device();
    }

    /// Start the audio stream (calls cpal `play()`).
    ///
    /// The transport cursor starts **paused** — call `play()` to begin
    /// advancing the timeline.  The test tone will play immediately if enabled.
    ///
    /// Throws if no stream is open.
    #[napi]
    pub fn start(&self) -> napi::Result<()> {
        self.inner.start().map_err(Into::into)
    }

    /// Pause (silence) the audio stream without closing the device.
    #[napi]
    pub fn stop(&self) {
        self.inner.stop();
    }

    // ── Transport ────────────────────────────────────────────────────────────

    /// Advance the transport cursor (begin timeline playback).
    ///
    /// Throws if no stream is open.
    #[napi]
    pub fn play(&self) -> napi::Result<()> {
        self.inner.play().map_err(Into::into)
    }

    /// Pause the transport cursor (audio stream stays active for monitoring).
    ///
    /// Throws if no stream is open.
    #[napi]
    pub fn pause(&self) -> napi::Result<()> {
        self.inner.pause().map_err(Into::into)
    }

    /// Seek the transport cursor to `seconds` from the project start.
    ///
    /// Throws if no stream is open.
    #[napi]
    pub fn seek(&self, seconds: f64) -> napi::Result<()> {
        self.inner.seek(seconds).map_err(Into::into)
    }

    // ── Test tone ────────────────────────────────────────────────────────────

    /// Enable or disable the sine test tone.
    ///
    /// The tone sounds immediately when the stream is running, regardless of
    /// the transport play/pause state.  Useful for hardware verification.
    ///
    /// `frequency` — Hz (e.g. 440.0 for A4).  Defaults to 440 on `new()`.
    #[napi]
    pub fn set_test_tone(&self, enabled: bool, frequency: f64) {
        self.inner.set_test_tone(enabled, frequency as f32);
    }

    // ── Master volume ────────────────────────────────────────────────────────

    /// Set the master output volume.
    ///
    /// `value` — linear gain in `[0.0, 2.0]` (1.0 = unity, 2.0 = +6 dBFS).
    /// Values outside the range are clamped.
    ///
    /// This is applied inside the audio callback via an atomic — no locking.
    #[napi]
    pub fn set_master_volume(&self, value: f64) -> napi::Result<()> {
        self.inner
            .set_master_volume(value as f32)
            .map_err(Into::into)
    }

    // ── Project snapshot ─────────────────────────────────────────────────────

    /// Load a project snapshot from a JSON string.
    ///
    /// Expected format: `EngineProjectSnapshot` (see `types.rs` for the full
    /// schema).  The engine rebuilds its internal track graph from the snapshot.
    ///
    /// Throws on deserialization error.
    ///
    /// Example (TypeScript side):
    /// ```ts
    /// await engine.loadProject(JSON.stringify(projectSnapshot));
    /// ```
    #[napi]
    pub fn load_project(&self, snapshot_json: String) -> napi::Result<()> {
        let snapshot: EngineProjectSnapshot =
            serde_json::from_str(&snapshot_json).map_err(|e| {
                napi::Error::new(
                    napi::Status::InvalidArg,
                    format!("Invalid project snapshot JSON: {e}"),
                )
            })?;
        self.inner.load_project(snapshot).map_err(Into::into)
    }

    // ── Realtime param updates ───────────────────────────────────────────────

    /// Update a single parameter on a mixer track.
    ///
    /// `param_id` may be `"volume"` (0..2), `"pan"` (-1..1), or `"muted"` (0/1).
    ///
    /// The update is sent through the lock-free command queue and takes effect
    /// at the start of the next audio block.
    ///
    /// Throws if the stream is not open.
    #[napi]
    pub fn update_track_param(
        &self,
        track_id: String,
        param_id: String,
        value: f64,
    ) -> napi::Result<()> {
        self.inner
            .update_track_param(&track_id, &param_id, value)
            .map_err(Into::into)
    }

    /// Update a parameter on an insert effect on a specific track.
    ///
    /// Throws if the stream is not open.
    #[napi]
    pub fn update_insert_param(
        &self,
        track_id: String,
        insert_id: String,
        param_id: String,
        value: f64,
    ) -> napi::Result<()> {
        self.inner
            .update_insert_param(&track_id, &insert_id, &param_id, value)
            .map_err(Into::into)
    }

    /// Open the native VST3 editor bound to the existing insert processor.
    ///
    /// This must be called from the UI/control side only. It does not route
    /// audio through JS; editor parameter changes are queued natively and
    /// consumed by the audio callback on the next process block.
    #[napi]
    pub fn open_insert_editor(
        &self,
        track_id: String,
        insert_id: String,
        window_id: String,
        title: String,
        width: i32,
        height: i32,
    ) -> napi::Result<f64> {
        self.inner
            .open_insert_editor(&track_id, &insert_id, &window_id, &title, width, height)
            .map(|handle| handle as f64)
            .map_err(Into::into)
    }

    #[napi]
    pub fn close_insert_editor(&self, track_id: String, insert_id: String) -> napi::Result<()> {
        self.inner
            .close_insert_editor(&track_id, &insert_id)
            .map_err(Into::into)
    }

    #[napi]
    pub fn focus_insert_editor(&self, track_id: String, insert_id: String) -> napi::Result<bool> {
        self.inner
            .focus_insert_editor(&track_id, &insert_id)
            .map_err(Into::into)
    }

    /// Apply a JSON-encoded patch to a clip.
    ///
    /// **MVP note:** not yet processed by the audio callback; stored for future use.
    #[napi]
    pub fn update_clip(&self, clip_id: String, _patch_json: String) -> napi::Result<()> {
        eprintln!("[SphereAudio] updateClip '{clip_id}' — not yet implemented in MVP");
        Ok(())
    }

    // ── DAUx backend API ─────────────────────────────────────────────────────

    /// Return all DAUx backends available on the current platform.
    ///
    /// Use the returned `id` values with `openDaux()` to select a backend.
    #[napi]
    pub fn list_daux_backends(&self) -> Vec<JsDauxBackendInfo> {
        self.inner.list_daux_backends()
    }

    /// Open (or re-open) a DAUx stream with a specific backend, device, and
    /// buffer configuration.
    ///
    /// This is the preferred way to open the audio device in Electron.
    /// After a successful call, use `start()` to begin audio output.
    ///
    /// Example (TypeScript):
    /// ```ts
    /// engine.openDaux({ backendId: "wasapi-exclusive", bufferSize: 128, mmcssPriority: true });
    /// engine.start();
    /// ```
    #[napi]
    pub fn open_daux(&self, config: JsDauxConfig) -> napi::Result<()> {
        // Catch any Rust panic so it does not cross the NAPI boundary and
        // terminate the Electron process.
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.inner.open_daux(config).map_err(Into::into)
        }))
        .unwrap_or_else(|_| {
            Err(napi::Error::new(
                napi::Status::GenericFailure,
                "WASAPI: internal panic during audio backend open".to_string(),
            ))
        })
    }

    /// Safe variant of `openDaux` that restores the previous working backend if
    /// the requested config fails.  The stream is always left in an open (or
    /// previously-open) state.  On failure the returned error describes what
    /// happened and which backend was restored.
    #[napi]
    pub fn open_daux_safe(&self, config: JsDauxConfig) -> napi::Result<()> {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.inner.open_daux_safe(config).map_err(Into::into)
        }))
        .unwrap_or_else(|_| {
            Err(napi::Error::new(
                napi::Status::GenericFailure,
                "WASAPI: internal panic during safe audio backend switch".to_string(),
            ))
        })
    }

    /// Return the current DAUx runtime status: backend, device, latency, glitches.
    ///
    /// Poll this at ~1Hz to update the Settings / status bar UI.
    #[napi]
    pub fn get_daux_status(&self) -> JsDauxStatus {
        self.inner.get_daux_status()
    }

    /// Attempt to recover the audio device after a device-loss event, reusing
    /// the last-known-good config. Returns `true` if a recovery was performed,
    /// `false` if the device was not lost. Poll `getDauxStatus().deviceLost`
    /// and call this (e.g. on a "Reconnect" button or an auto-retry timer).
    #[napi]
    pub fn recover_daux(&self) -> napi::Result<bool> {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.inner.recover_daux().map_err(Into::into)
        }))
        .unwrap_or_else(|_| {
            Err(napi::Error::new(
                napi::Status::GenericFailure,
                "internal panic during audio device recovery".to_string(),
            ))
        })
    }

    /// Whether applying `config` would require a controlled device restart
    /// versus the currently-open device. The Settings UI uses this to mark
    /// changed fields "restart required" before the user applies them.
    #[napi]
    pub fn daux_requires_restart(&self, config: JsDauxConfig) -> bool {
        self.inner.daux_requires_restart(&config)
    }

    /// Begin an input-level test on `deviceId` (or the default input device).
    /// Poll `getInputTestLevel()` for the meter and call `stopInputTest()` when
    /// done. Independent of recording and the output device.
    #[napi]
    pub fn start_input_test(&self, device_id: Option<String>) -> napi::Result<()> {
        self.inner.start_input_test(device_id).map_err(Into::into)
    }

    /// Read (and reset) the peak input level since the last poll, `0.0..=1.0`.
    /// Returns `0.0` when no input test is active.
    #[napi]
    pub fn get_input_test_level(&self) -> f64 {
        self.inner.get_input_test_level() as f64
    }

    /// Stop and release the input-level test stream.
    #[napi]
    pub fn stop_input_test(&self) {
        self.inner.stop_input_test();
    }

    /// Aggregate latency report: device buffer latency plus per-track and master
    /// plug-in latency, for the Audio Settings / mixer UI. Reporting only —
    /// full plug-in delay compensation is a later phase.
    #[napi]
    pub fn get_latency_info(&self) -> JsLatencyInfo {
        self.inner.get_latency_info()
    }

    // ── Debug info ───────────────────────────────────────────────────────────

    /// Return a debug snapshot of the engine's current runtime state.
    ///
    /// Useful for verifying that the project was loaded and clips are ready:
    /// ```ts
    /// const info = engine.getDebugInfo();
    /// console.log(info.loadedClips, info.readyClips, info.clipSummaries);
    /// ```
    #[napi]
    pub fn get_debug_info(&self) -> JsEngineDebugInfo {
        self.inner.get_debug_info()
    }

    // ── Meters ───────────────────────────────────────────────────────────────

    /// Read the current meter snapshot (peak + RMS for L and R master bus).
    ///
    /// Values are linear amplitudes in `[0.0, 1.0]`.  Poll at ~20 fps from the
    /// JS side for smooth VU meter display.
    ///
    /// Returns zeros when the stream is not running.
    #[napi]
    pub fn get_meters(&self) -> JsMeterSnapshot {
        self.inner.get_meters()
    }

    /// Probe audio file metadata without loading it into the realtime engine.
    /// This is the source of truth for import duration in Electron/native UI.
    #[napi]
    pub fn probe_audio_file(&self, file_path: String) -> napi::Result<JsAudioFileInfo> {
        let info = audio_file::probe_audio_file(&file_path)
            .map_err(|e| napi::Error::from_reason(e.to_string()))?;
        Ok(JsAudioFileInfo {
            path: info.path.to_string_lossy().to_string(),
            sample_rate: info.sample_rate,
            channel_count: info.channels as u32,
            total_frames: info.total_frames as f64,
            duration_seconds: info.duration_seconds,
            format: info.format.as_str().to_string(),
        })
    }

    /// Generate Int16 min/max waveform peaks for a PCM WAV file by streaming
    /// the source from disk. This is used by Electron import/background jobs so
    /// renderer drag/drop never decodes or scans long files.
    #[napi]
    pub fn generate_wav_peaks(
        &self,
        file_path: String,
        file_id: String,
        samples_per_peak: u32,
    ) -> napi::Result<JsWavPeakResult> {
        let result = audio_file::generate_wav_peaks_from_path(&file_path, samples_per_peak)
            .map_err(|e| {
                napi::Error::from_reason(format!("Waveform peak generation failed: {e}"))
            })?;
        Ok(JsWavPeakResult {
            file_id,
            sample_rate: result.sample_rate,
            channel_count: result.channel_count,
            duration: result.duration,
            samples_per_peak: result.samples_per_peak,
            peak_count: result.peak_count,
            peaks: result.peaks,
        })
    }

    /// Convert an internal RAUF recording take to a standard WAV file without FFmpeg.
    ///
    /// This is the export/drag-out path for recorded takes. RAUF remains an
    /// internal project format; external consumers should receive the WAV path.
    #[napi]
    pub fn export_rauf_to_wav(
        &self,
        rauf_path: String,
        wav_path: String,
    ) -> napi::Result<JsWavExportResult> {
        let report = sphere_encoder::wav::convert_rauf_to_wav(&rauf_path, &wav_path)
            .map_err(|e| napi::Error::from_reason(format!("RAUF to WAV export failed: {e}")))?;
        Ok(JsWavExportResult {
            file_path: wav_path,
            frames_written: report.frames_written as f64,
            data_bytes: report.data_bytes as f64,
        })
    }

    // ── Recording ────────────────────────────────────────────────────────────

    /// Begin recording armed tracks to RAUF files in `<projectRoot>/recordings`.
    ///
    /// Opens a separate cpal input stream on the selected input device.
    /// Audio data is routed through a lock-free channel to a disk writer thread —
    /// the output audio callback is not affected.
    ///
    /// Throws if a session is already active or if the device cannot be opened.
    #[napi]
    pub fn start_recording(&self, config: JsStartRecordingConfig) -> napi::Result<()> {
        self.inner
            .start_recording(config)
            .map_err(|e| napi::Error::from_reason(e.to_string()))
    }

    /// Stop the active recording session, finalize RAUF files, and return per-track results.
    ///
    /// Drops the input stream (causing the disk writer to flush and close its
    /// files), then waits up to 60 s for finalization before returning.
    ///
    /// Throws if no recording is active.
    #[napi]
    pub fn stop_recording(&self) -> napi::Result<Vec<JsRecordingResult>> {
        self.inner
            .stop_recording()
            .map_err(|e| napi::Error::from_reason(e.to_string()))
    }

    /// Return a lightweight recording status snapshot for UI polling.
    #[napi]
    pub fn get_recording_status(&self) -> JsRecordingStatus {
        self.inner.get_recording_status()
    }
}

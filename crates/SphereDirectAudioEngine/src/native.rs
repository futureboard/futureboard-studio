//! Native Rust facade over [`crate::engine::EngineInner`].
//!
//! This module exists so the Rust-Native Futureboard Studio shell
//! (`apps/experimental/native`) can drive the audio engine without
//! touching any NAPI types or Node.js runtime. It is a thin wrapper —
//! the underlying state, audio thread, and DSP code stay the same.
//!
//! The existing NAPI surface in [`crate::SphereDirectAudioEngine`] is
//! untouched. Both entry points share the same `EngineInner` so a
//! command issued through either surface sees the same realtime state.
//!
//! Realtime safety contract is identical to the NAPI path:
//!   * `start` / `stop` / `open` calls run on the control thread and
//!     take a parking-lot lock — not realtime safe, but they only run
//!     from UI events.
//!   * The native facade adds no allocations on the audio thread.

use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::audio_file;
use crate::backend::{self, BackendKind};
use crate::device;
use crate::engine::EngineInner;
use crate::error::SphereAudioError;
use crate::types::{EngineProjectSnapshot, JsDauxConfig, JsMeterSnapshot, JsStartRecordingConfig};

/// Default sample rate used when the caller does not specify one. Mirrors
/// the system's "Auto" path — most backends ignore this and pick their
/// own default anyway.
pub const DEFAULT_SAMPLE_RATE: u32 = 48_000;
/// Default buffer size used when the caller does not specify one.
pub const DEFAULT_BUFFER_SIZE: u32 = 256;

/// Whether this binary was built with the Windows ASIO host.
pub fn asio_support_enabled() -> bool {
    crate::backend::asio_support_enabled()
}

/// Which DAUx audio backend to drive. Mirrors [`BackendKind`] but is
/// re-exposed under a more Rust-Native-friendly name and limited to the
/// values the native shell currently cares about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AudioBackend {
    /// Platform default — WASAPI Shared / CoreAudio / ALSA.
    #[default]
    Auto,
    /// cpal-backed best-effort (same as `Auto` for now; explicit selector).
    Cpal,
    /// Windows: WASAPI Shared system mixer (aggregate endpoint, no extra driver).
    WasapiShared,
    /// Windows: WASAPI Exclusive event-driven (lowest latency).
    WasapiExclusive,
    /// Windows: WDM-KS low-level driver path (experimental).
    WdmKs,
    /// Windows: ASIO driver path. Selectable only once an edition provider has
    /// registered an ASIO host — see [`AudioBackend::sanitize_for_current_build`].
    Asio,
}

impl AudioBackend {
    pub fn display_name(self) -> &'static str {
        self.to_backend_kind().display_name()
    }

    /// Normalize a backend that came from persisted settings or another
    /// untrusted string source. A backend this build/machine cannot actually
    /// drive — notably `Asio` with no registered host — becomes `Auto`.
    ///
    /// The `asio_host()` path already fails closed, but sanitizing here keeps a
    /// hand-edited `settings.json` from leaving the engine pointed at a backend
    /// the UI never offered. Mirrors
    /// [`BackendKind::sanitize_for_current_platform`].
    pub fn sanitize_for_current_build(self) -> Self {
        if self.to_backend_kind().is_allowed_on_current_platform() {
            self
        } else {
            AudioBackend::Auto
        }
    }

    fn to_backend_kind(self) -> BackendKind {
        match self {
            AudioBackend::Auto => BackendKind::Auto,
            AudioBackend::Cpal => BackendKind::Auto,
            AudioBackend::WasapiShared => BackendKind::WasapiShared,
            AudioBackend::WasapiExclusive => BackendKind::WasapiExclusive,
            AudioBackend::WdmKs => BackendKind::WdmKs,
            AudioBackend::Asio => BackendKind::Asio,
        }
    }

    fn backend_id(self) -> &'static str {
        self.to_backend_kind().id()
    }

    fn accepts_device_id(self, device_id: &AudioDeviceId) -> bool {
        matches!(
            (self, device_id),
            (
                AudioBackend::WasapiExclusive,
                AudioDeviceId::WasapiEndpoint(_)
            ) | (AudioBackend::WasapiShared, AudioDeviceId::DauxEndpoint(_))
                | (AudioBackend::WdmKs, AudioDeviceId::WdmKsFilterPin { .. })
                | (AudioBackend::Asio, AudioDeviceId::AsioDevice(_))
                | (
                    AudioBackend::Auto | AudioBackend::Cpal,
                    AudioDeviceId::DauxEndpoint(_)
                )
        )
    }
}

/// Backend-scoped audio device identifier.
///
/// Keeping these as distinct variants prevents accidentally applying a WASAPI
/// endpoint to WDM-KS, or a WDM-KS filter/pin path to a DAUx/cpal backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioDeviceId {
    /// Windows MMDevice endpoint id/name used by WASAPI professional.
    WasapiEndpoint(String),
    /// Windows Kernel Streaming filter path plus render pin id.
    WdmKsFilterPin { filter_path: String, pin_id: u32 },
    /// Cross-platform DAUx/cpal endpoint id/name (Auto/Cpal/CoreAudio/ALSA path).
    DauxEndpoint(String),
    /// ASIO driver/device name resolved only against CPAL's ASIO host.
    AsioDevice(String),
}

impl AudioDeviceId {
    pub fn raw_id(&self) -> &str {
        match self {
            AudioDeviceId::WasapiEndpoint(id)
            | AudioDeviceId::DauxEndpoint(id)
            | AudioDeviceId::AsioDevice(id) => id,
            AudioDeviceId::WdmKsFilterPin { filter_path, .. } => filter_path,
        }
    }
}

/// Configuration for opening the engine's audio stream.
///
/// `sample_rate == 0` or `buffer_size == 0` means "use the device default".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineConfig {
    pub sample_rate: u32,
    pub buffer_size: u32,
    pub channels: u16,
    pub backend: AudioBackend,
    pub input_device: Option<AudioDeviceId>,
    pub output_device: Option<AudioDeviceId>,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            sample_rate: DEFAULT_SAMPLE_RATE,
            buffer_size: DEFAULT_BUFFER_SIZE,
            channels: 2,
            backend: AudioBackend::Auto,
            input_device: None,
            output_device: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineAudioChannelInfo {
    pub index: u32,
    pub name: String,
    pub direction: String,
    pub active: bool,
    pub sample_format: String,
    pub stereo_pair: Option<u32>,
    pub label: String,
}

fn generic_channel_info(kind: &str, count: u32) -> Vec<EngineAudioChannelInfo> {
    (0..count)
        .map(|index| {
            let pair_start = index - (index % 2);
            let stereo_pair = (pair_start + 1 < count).then_some(pair_start / 2);
            let label = format!(
                "{} {}",
                if kind == "input" { "Input" } else { "Output" },
                index + 1
            );
            EngineAudioChannelInfo {
                index,
                name: label.clone(),
                direction: kind.to_string(),
                active: true,
                sample_format: "Unknown".to_string(),
                stereo_pair,
                label,
            }
        })
        .collect()
}

#[cfg(target_os = "windows")]
fn asio_session_matches_driver(
    session_driver: &str,
    descriptor: &crate::asio_registry::AsioDriverDescriptor,
) -> bool {
    session_driver.eq_ignore_ascii_case(&descriptor.stable_id)
        || session_driver.eq_ignore_ascii_case(&descriptor.display_name)
        || session_driver.eq_ignore_ascii_case(&descriptor.registry_key)
}

#[cfg(target_os = "windows")]
fn asio_channel_info(
    channel: &crate::backend::AsioChannelInfo,
    direction: &str,
) -> EngineAudioChannelInfo {
    EngineAudioChannelInfo {
        index: channel.index,
        name: channel.name.clone(),
        direction: direction.to_string(),
        active: channel.active,
        sample_format: channel.sample_format.clone(),
        stereo_pair: channel.stereo_pair,
        label: channel.name.clone(),
    }
}

/// Plain-Rust audio device descriptor returned by
/// [`AudioEngine::list_output_devices`] / [`AudioEngine::list_input_devices`].
///
/// Mirrors the NAPI `JsAudioDeviceInfo` shape but lives entirely in Rust so
/// the native shell does not pull NAPI types into its public surface.
#[derive(Debug, Clone)]
pub struct EngineDeviceInfo {
    pub id: String,
    pub device_id: AudioDeviceId,
    pub name: String,
    pub kind: String,
    pub channels: u32,
    pub channel_info: Vec<EngineAudioChannelInfo>,
    pub default_sample_rate: u32,
    pub is_default: bool,
    pub backend: String,
}

impl EngineDeviceInfo {
    #[cfg(target_os = "windows")]
    fn from_wdm_ks(d: crate::backend::wdm_ks::WdmKsDeviceInfo) -> Self {
        let channel_info = generic_channel_info("output", d.channels);
        Self {
            id: d.filter_path.clone(),
            device_id: AudioDeviceId::WdmKsFilterPin {
                filter_path: d.filter_path,
                pin_id: d.pin_id,
            },
            name: d.name,
            kind: "output".into(),
            channels: d.channels,
            channel_info,
            default_sample_rate: d.default_sample_rate,
            is_default: false,
            backend: "DAUx WDM-KS".into(),
        }
    }

    fn from_daux(d: crate::types::JsAudioDeviceInfo) -> Self {
        Self::from_daux_with_backend(d, None)
    }

    fn from_daux_with_backend(
        d: crate::types::JsAudioDeviceInfo,
        backend_override: Option<&str>,
    ) -> Self {
        let channel_info = generic_channel_info(&d.kind, d.channels);
        Self {
            device_id: AudioDeviceId::DauxEndpoint(d.id.clone()),
            id: d.id,
            name: d.name,
            kind: d.kind,
            channels: d.channels,
            channel_info,
            default_sample_rate: d.default_sample_rate,
            is_default: d.is_default,
            backend: backend_override.unwrap_or(&d.backend).to_string(),
        }
    }

    fn from_wasapi(d: crate::types::JsAudioDeviceInfo) -> Self {
        let channel_info = generic_channel_info(&d.kind, d.channels);
        Self {
            device_id: AudioDeviceId::WasapiEndpoint(d.id.clone()),
            id: d.id,
            name: d.name,
            kind: d.kind,
            channels: d.channels,
            channel_info,
            default_sample_rate: d.default_sample_rate,
            is_default: d.is_default,
            backend: "DAUx WASAPI Exclusive".into(),
        }
    }
}

/// Lightweight status snapshot suitable for status-bar polling.
#[derive(Debug, Clone, Default)]
pub struct EngineStats {
    pub running: bool,
    pub stream_open: bool,
    pub transport_playing: bool,
    pub position_seconds: f64,
    pub position_beats: f64,
    pub position_samples: u64,
    pub loop_enabled: bool,
    pub bpm: f64,
    pub time_signature_num: u32,
    pub time_signature_den: u32,
    /// Active runtime sample rate (Hz) — the rate the opened stream runs at.
    /// Authoritative for all timing; this is what the status bar shows.
    pub sample_rate: u32,
    /// Rate the device was requested to open at (Hz), or 0 for "device
    /// default". Differs from `sample_rate` on a shared-mode/professional fallback.
    pub requested_sample_rate: u32,
    pub buffer_size: u32,
    pub backend_name: String,
    pub output_device: Option<String>,
    pub last_error: Option<String>,
    pub glitch_count: u64,
    pub estimated_latency_ms: f64,
    /// See [`crate::types::JsDauxStatus::input_latency_ms`].
    pub input_latency_ms: f64,
    /// See [`crate::types::JsDauxStatus::round_trip_latency_ms`].
    pub round_trip_latency_ms: f64,
    /// `true` when the device was lost mid-stream and recovery is pending.
    pub device_lost: bool,
    /// Lifecycle state: "Closed" | "Ready" | "Running" | "DeviceLost".
    pub device_state: String,
    /// Active Dropout Protection mode ("Off" | "Light" | "Medium" | "High").
    pub dropout_protection_mode: String,
    /// Blocks flagged as dropout-risk since the stream opened.
    pub dropout_count: u64,
    /// Reason of the most recent dropout-risk block.
    pub dropout_last_reason: String,
    /// Most recent output-callback duration, microseconds.
    pub callback_last_us: u32,
    /// Worst output-callback duration since stream open, microseconds.
    pub callback_max_us: u32,
    /// Per-block wall-clock budget, microseconds.
    pub callback_deadline_us: u32,
    /// SoundFont voices sounding at the end of the last audio callback. Built-in
    /// instrument only — see `EngineInner::active_voice_count`.
    pub active_voices: u32,
}

/// Native Rust-facing handle to the engine.
///
/// Wraps [`EngineInner`] in an `Arc`. Cloning the handle is cheap and
/// shares the same underlying engine — the audio thread and any other
/// control surfaces all see the same state.
#[derive(Clone)]
pub struct AudioEngine {
    inner: Arc<EngineInner>,
    config: EngineConfig,
    asio_devices: Arc<parking_lot::Mutex<AsioDeviceCache>>,
}

#[derive(Default)]
struct AsioDeviceCache {
    refreshed_at: Option<std::time::Instant>,
    active_session: Option<crate::backend::AsioSessionCaps>,
    inputs: Vec<EngineDeviceInfo>,
    outputs: Vec<EngineDeviceInfo>,
}

/// How long a registry-derived ASIO driver list stays fresh before a device
/// query re-reads the registry. Refreshing is registry-only (no COM, no driver
/// loads), so it can never disturb a running stream.
const ASIO_DEVICE_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(2);

#[cfg(target_os = "windows")]
fn list_wdm_ks_output_devices() -> Vec<EngineDeviceInfo> {
    crate::backend::wdm_ks::list_output_devices()
        .into_iter()
        .map(EngineDeviceInfo::from_wdm_ks)
        .collect()
}

#[cfg(not(target_os = "windows"))]
fn list_wdm_ks_output_devices() -> Vec<EngineDeviceInfo> {
    Vec::new()
}

impl AudioEngine {
    /// The default native configuration. Equivalent to
    /// `EngineConfig::default()`; provided as a method so call sites read
    /// closer to the spec (`AudioEngine::default_config()`).
    pub fn default_config() -> EngineConfig {
        EngineConfig::default()
    }

    /// Build a new engine handle. Does **not** open or start the audio
    /// stream — call [`AudioEngine::start`] when ready.
    pub fn new(config: EngineConfig) -> Result<Self, SphereAudioError> {
        let mut config = config;
        let requested = config.backend;
        config.backend = config.backend.sanitize_for_current_build();
        if config.backend != requested {
            eprintln!(
                "[audio] EngineConfig backend {:?} unavailable; falling back to {:?}",
                requested, config.backend
            );
            // An ASIO (or other edition-scoped) device id is never valid on the
            // fallback backend — drop it so Auto/WASAPI uses the system default.
            config.output_device = None;
            config.input_device = None;
        }
        Ok(Self {
            inner: Arc::new(EngineInner::new()),
            config,
            asio_devices: Arc::new(parking_lot::Mutex::new(AsioDeviceCache::default())),
        })
    }

    /// Populate/refresh the ASIO driver list from the Windows registry.
    ///
    /// Never instantiates a driver: an ASIO driver is one duplex device, and
    /// COM-loading drivers just to list them is slow, can pop driver dialogs,
    /// and can clobber the driver that is currently streaming. Channel counts
    /// for the *active* driver are overlaid from the open session; drivers
    /// that are merely installed report zero capabilities until opened (the
    /// stream-open diagnostic is the real capability check).
    ///
    /// Fails closed: without a registered Professional Edition ASIO host, no
    /// enumeration happens at all — a hand-edited settings file selecting
    /// "asio" yields an error and an empty device list, not registry output.
    fn ensure_asio_device_cache(&self) -> Result<(), SphereAudioError> {
        if !backend::asio_support_enabled() {
            return Err(SphereAudioError::BackendUnavailable(
                "DAUx ASIO requires a Futureboard Professional Edition build".into(),
            ));
        }

        let session = self.inner.asio_session_caps();
        {
            let cache = self.asio_devices.lock();
            if cache
                .refreshed_at
                .is_some_and(|at| at.elapsed() < ASIO_DEVICE_CACHE_TTL)
                && cache.active_session == session
            {
                return Ok(());
            }
        }

        #[cfg(target_os = "windows")]
        {
            let mut inputs = Vec::new();
            let mut outputs = Vec::new();
            for descriptor in crate::asio_registry::enumerate_asio_drivers() {
                if !descriptor.is_loadable() {
                    continue;
                }
                let active = session
                    .as_ref()
                    .filter(|caps| asio_session_matches_driver(&caps.driver, &descriptor));
                let make = |kind: &str,
                            channels: u32,
                            channel_info: Vec<EngineAudioChannelInfo>,
                            sample_rate: u32| EngineDeviceInfo {
                    id: descriptor.stable_id.clone(),
                    device_id: AudioDeviceId::AsioDevice(descriptor.stable_id.clone()),
                    name: descriptor.display_name.clone(),
                    kind: kind.to_string(),
                    channels,
                    channel_info,
                    default_sample_rate: sample_rate,
                    is_default: active.is_some(),
                    backend: "DAUx ASIO".into(),
                };
                let (input_info, output_info, sr) = active
                    .map(|caps| {
                        (
                            caps.input_channel_info
                                .iter()
                                .map(|channel| asio_channel_info(channel, "input"))
                                .collect::<Vec<_>>(),
                            caps.output_channel_info
                                .iter()
                                .map(|channel| asio_channel_info(channel, "output"))
                                .collect::<Vec<_>>(),
                            caps.sample_rate,
                        )
                    })
                    .unwrap_or_default();
                inputs.push(make("input", input_info.len() as u32, input_info, sr));
                outputs.push(make("output", output_info.len() as u32, output_info, sr));
            }
            // Without an open session, mark the first driver as the default so
            // Settings has a deterministic pre-selection.
            if session.is_none() {
                if let Some(first) = inputs.first_mut() {
                    first.is_default = true;
                }
                if let Some(first) = outputs.first_mut() {
                    first.is_default = true;
                }
            }
            *self.asio_devices.lock() = AsioDeviceCache {
                refreshed_at: Some(std::time::Instant::now()),
                active_session: session,
                inputs,
                outputs,
            };
        }
        #[cfg(not(target_os = "windows"))]
        {
            *self.asio_devices.lock() = AsioDeviceCache {
                refreshed_at: Some(std::time::Instant::now()),
                active_session: session,
                inputs: Vec::new(),
                outputs: Vec::new(),
            };
        }
        Ok(())
    }

    /// Borrow the configuration the engine was created with. The active
    /// runtime sample rate / buffer size may differ once a stream is
    /// open — see [`AudioEngine::stats`].
    pub fn config(&self) -> &EngineConfig {
        &self.config
    }

    /// Engine semver string, e.g. `"0.1.0"`.
    pub fn version(&self) -> String {
        self.inner.get_version()
    }

    /// Current transport play flag (set/cleared only by Start/StopTransport).
    pub fn transport_playing(&self) -> bool {
        self.inner.transport_playing()
    }

    /// Current lifecycle gate of the realtime callback.
    pub fn engine_state(&self) -> crate::engine::AudioEngineState {
        self.inner.engine_state()
    }

    fn daux_config_from_engine_config(
        config: &EngineConfig,
    ) -> Result<JsDauxConfig, SphereAudioError> {
        Self::validate_config(config)?;
        Ok(JsDauxConfig {
            backend_id: config.backend.backend_id().to_string(),
            output_device_id: config
                .output_device
                .as_ref()
                .map(|id| id.raw_id().to_string()),
            sample_rate: if config.sample_rate > 0 {
                Some(config.sample_rate)
            } else {
                None
            },
            buffer_size: if config.buffer_size > 0 {
                Some(config.buffer_size)
            } else {
                None
            },
            mmcss_priority: false,
            safe_mode: false,
        })
    }

    pub fn validate_config(config: &EngineConfig) -> Result<(), SphereAudioError> {
        if let Some(device_id) = &config.output_device {
            if !config.backend.accepts_device_id(device_id) {
                return Err(SphereAudioError::InvalidConfig(format!(
                    "output device id is not valid for {}",
                    config.backend.display_name()
                )));
            }
        }
        if let Some(device_id) = &config.input_device {
            if !config.backend.accepts_device_id(device_id) {
                return Err(SphereAudioError::InvalidConfig(format!(
                    "input device id is not valid for {}",
                    config.backend.display_name()
                )));
            }
        }
        Ok(())
    }

    /// Open the audio stream and start it. The stream stays paused at the
    /// transport level — use [`AudioEngine::play`] to advance the
    /// timeline once playback work is wired up.
    pub fn start(&mut self) -> Result<(), SphereAudioError> {
        // Resume an already-open stream — never tear down and reopen here.
        // Reopening runs `get_initial_runtime` on the caller thread and can
        // re-decode every clip (UI freeze on Play after a project sync).
        let st = self.inner.get_status();
        if st.stream_open && st.running {
            return Ok(());
        }
        if st.stream_open {
            return self.inner.start();
        }
        if self.config.backend == AudioBackend::Asio {
            self.ensure_asio_device_cache()?;
        }
        let daux = Self::daux_config_from_engine_config(&self.config)?;
        self.inner.open_daux(daux)?;
        self.inner.start()
    }

    /// Re-open the active stream with a new native config while keeping the
    /// same engine/runtime handle alive. This is a control-thread operation for
    /// Settings changes; it never runs on the realtime callback.
    pub fn reopen_with_config(&mut self, mut config: EngineConfig) -> Result<(), SphereAudioError> {
        let requested = config.backend;
        config.backend = config.backend.sanitize_for_current_build();
        if config.backend != requested {
            eprintln!(
                "[audio] reopen backend {:?} unavailable; falling back to {:?}",
                requested, config.backend
            );
            config.output_device = None;
            config.input_device = None;
        }
        if config.backend == AudioBackend::Asio {
            self.ensure_asio_device_cache()?;
        }
        let daux = Self::daux_config_from_engine_config(&config)?;
        self.inner.open_daux_safe(daux)?;
        self.config = config;
        self.inner.start()
    }

    /// Stop the audio stream (closes the device, frees realtime resources).
    pub fn stop(&mut self) -> Result<(), SphereAudioError> {
        self.inner.stop();
        self.inner.close_device();
        Ok(())
    }

    /// Ordered shutdown before UI teardown. Idempotent.
    pub fn shutdown(&mut self) {
        self.inner.shutdown();
    }

    /// Whether the stream is currently active.
    pub fn is_running(&self) -> bool {
        self.inner.get_status().running
    }

    /// Begin advancing the transport cursor. The audio stream must already
    /// be open via [`AudioEngine::start`].
    pub fn play(&self) -> Result<(), SphereAudioError> {
        self.inner.play()
    }

    /// Pause the transport cursor. The audio stream remains active.
    pub fn pause(&self) -> Result<(), SphereAudioError> {
        self.inner.pause()
    }

    /// Seek the native transport to an absolute project time in seconds.
    pub fn seek(&self, position_seconds: f64) -> Result<(), SphereAudioError> {
        self.inner.seek(position_seconds.max(0.0))
    }

    /// Seek by musical position, converted through the engine's authoritative
    /// tempo map. Prefer this over converting beats with a single BPM: under
    /// tempo automation the two disagree, and the metronome re-arms from the
    /// engine's answer.
    pub fn seek_beats(&self, beat: f64) -> Result<(), SphereAudioError> {
        self.inner.seek_beats(beat)
    }

    pub fn set_metronome_suspended(&self, suspended: bool) -> Result<(), SphereAudioError> {
        self.inner.set_metronome_suspended(suspended)
    }

    pub fn set_metronome_enabled(&self, enabled: bool) -> Result<(), SphereAudioError> {
        self.inner.set_metronome_enabled(enabled)
    }

    /// Begin an audible record count-in at the tempo of `target_beat`. See
    /// [`crate::engine::EngineInner::start_count_in`].
    pub fn start_count_in(
        &self,
        beats: u32,
        beats_per_bar: u32,
        target_beat: f64,
    ) -> Result<(), SphereAudioError> {
        self.inner.start_count_in(beats, beats_per_bar, target_beat)
    }

    pub fn cancel_count_in(&self) -> Result<(), SphereAudioError> {
        self.inner.cancel_count_in()
    }

    /// Samples left in the count-in; `0` when none is running.
    pub fn count_in_remaining_samples(&self) -> u64 {
        self.inner.count_in_remaining_samples()
    }

    /// Click level (the persisted 0..1 control position, where the 0.8 default
    /// is unity) and timbre label ("Woodblock" / "Beep") from Settings.
    pub fn set_metronome_voice(
        &self,
        volume: f32,
        sound_label: &str,
    ) -> Result<(), SphereAudioError> {
        self.inner.set_metronome_voice(volume, sound_label)
    }

    pub fn set_bpm(&self, bpm: f64) -> Result<(), SphereAudioError> {
        self.inner.set_bpm(bpm)
    }

    pub fn set_tempo_map(
        &self,
        default_bpm: f64,
        points: Vec<crate::types::EngineTempoPointSnapshot>,
    ) -> Result<(), SphereAudioError> {
        self.inner.set_tempo_map(default_bpm, points)
    }

    /// Stage 3b: install (or clear) the realtime plugin-bridge sink for
    /// `track_id` so the audio callback mixes its external plugin-host DSP
    /// output into the master.
    pub fn set_plugin_bridge_sink(
        &self,
        insert_id: String,
        sink: Option<std::sync::Arc<dyn crate::plugin_bridge::PluginBridgeSink>>,
    ) -> Result<(), SphereAudioError> {
        self.inner.set_plugin_bridge_sink(insert_id, sink)
    }

    /// Wait (bounded, control thread only) until the audio callback has drained
    /// every command sent before this call. `true` = confirmed ack; `false` =
    /// timeout / no open stream, caller falls back to its own grace handling.
    pub fn wait_for_command_barrier(&self, timeout: std::time::Duration) -> bool {
        self.inner.wait_for_command_barrier(timeout)
    }

    pub fn set_bridge_editor_active(
        &self,
        track_id: String,
        active: bool,
    ) -> Result<(), SphereAudioError> {
        self.inner.set_bridge_editor_active(track_id, active)
    }

    pub fn set_time_signature(
        &self,
        numerator: u32,
        denominator: u32,
    ) -> Result<(), SphereAudioError> {
        self.inner.set_time_signature(numerator, denominator)
    }

    pub fn set_time_signature_map(
        &self,
        points: Vec<crate::time_signature_map::RuntimeTimeSignaturePointSnapshot>,
    ) -> Result<(), SphereAudioError> {
        self.inner.set_time_signature_map(points)
    }

    pub fn set_loop(
        &self,
        enabled: bool,
        start_seconds: f64,
        end_seconds: f64,
    ) -> Result<(), SphereAudioError> {
        self.inner.set_loop(enabled, start_seconds, end_seconds)
    }

    pub fn midi_preview_note_on(
        &self,
        track_id: String,
        channel: u8,
        pitch: u8,
        velocity: u8,
    ) -> Result<(), SphereAudioError> {
        self.inner
            .midi_preview_note_on(track_id, channel, pitch, velocity)
    }

    pub fn midi_preview_note_off(
        &self,
        track_id: String,
        channel: u8,
        pitch: u8,
    ) -> Result<(), SphereAudioError> {
        self.inner.midi_preview_note_off(track_id, channel, pitch)
    }

    pub fn midi_preview_control_change(
        &self,
        track_id: String,
        channel: u8,
        controller: u8,
        value: u8,
    ) -> Result<(), SphereAudioError> {
        self.inner
            .midi_preview_control_change(track_id, channel, controller, value)
    }

    pub fn midi_preview_all_notes_off(&self, track_id: String) -> Result<(), SphereAudioError> {
        self.inner.midi_preview_all_notes_off(track_id)
    }

    pub fn plugin_preview_note_on(
        &self,
        track_id: String,
        plugin_instance_id: String,
        channel: u8,
        pitch: u8,
        velocity: u8,
    ) -> Result<(), SphereAudioError> {
        self.inner
            .plugin_preview_note_on(track_id, plugin_instance_id, channel, pitch, velocity)
    }

    pub fn plugin_preview_note_off(
        &self,
        track_id: String,
        plugin_instance_id: String,
        channel: u8,
        pitch: u8,
    ) -> Result<(), SphereAudioError> {
        self.inner
            .plugin_preview_note_off(track_id, plugin_instance_id, channel, pitch)
    }

    pub fn plugin_preview_control_change(
        &self,
        track_id: String,
        plugin_instance_id: String,
        channel: u8,
        controller: u8,
        value: u8,
    ) -> Result<(), SphereAudioError> {
        self.inner.plugin_preview_control_change(
            track_id,
            plugin_instance_id,
            channel,
            controller,
            value,
        )
    }

    pub fn plugin_preview_all_notes_off(
        &self,
        track_id: String,
        plugin_instance_id: String,
    ) -> Result<(), SphereAudioError> {
        self.inner
            .plugin_preview_all_notes_off(track_id, plugin_instance_id)
    }

    /// Toggle the transport between play and pause. Returns the new playing
    /// state. No-ops cleanly if the stream is not open yet.
    pub fn toggle_transport(&self) -> Result<bool, SphereAudioError> {
        if self.inner.shared_playing() {
            self.inner.pause()?;
            Ok(false)
        } else {
            // `play` requires an open stream — surface the same error the
            // engine would have produced.
            self.inner.play()?;
            Ok(true)
        }
    }

    /// Audition (preview-play) a standalone audio file through the engine,
    /// independent of the timeline — the browser's "preview" affordance.
    ///
    /// Decodes off the realtime path, then starts a one-shot master audition.
    /// `Ok(true)` means the browser preview request was accepted without blocking UI.
    /// The source is decoded on a worker before it reaches the callback.
    /// The callback only mixes prepared PCM data; it never reads from disk.
    pub fn audition_file(&self, path: String) -> Result<bool, SphereAudioError> {
        self.inner.audition_file_async(path)?;
        Ok(true)
    }

    /// Stop any in-progress file audition.
    pub fn stop_audition(&self) -> Result<(), SphereAudioError> {
        self.inner.stop_audition()
    }

    /// Playhead of the Browser sample preview, in seconds of the source file.
    /// `None` when nothing is auditioning. Published by the render callback, so
    /// this is a single relaxed atomic read — safe to poll from the UI loop.
    pub fn audition_position_seconds(&self) -> Option<f64> {
        let raw = self
            .inner
            .shared
            .audition_position_us
            .load(std::sync::atomic::Ordering::Relaxed);
        (raw != crate::engine::AUDITION_POSITION_IDLE).then(|| raw as f64 / 1_000_000.0)
    }

    /// How much of a file a Browser preview plays before it fades out.
    pub fn audition_preview_seconds(&self) -> f64 {
        crate::audio_file::AUDITION_PREVIEW_SECONDS
    }

    /// Enumerate output devices for the engine's configured backend.
    pub fn list_output_devices(&self) -> Vec<EngineDeviceInfo> {
        self.list_output_devices_for_backend(self.config.backend)
    }

    /// Enumerate output devices for a specific backend. Settings UIs should use
    /// this with their draft backend so device selections never cross backends.
    pub fn list_output_devices_for_backend(&self, backend: AudioBackend) -> Vec<EngineDeviceInfo> {
        match backend {
            AudioBackend::WdmKs => list_wdm_ks_output_devices(),
            AudioBackend::WasapiExclusive => device::list_output_devices()
                .into_iter()
                .map(EngineDeviceInfo::from_wasapi)
                .collect(),
            AudioBackend::Asio => {
                if self.ensure_asio_device_cache().is_ok() {
                    self.asio_devices.lock().outputs.clone()
                } else {
                    Vec::new()
                }
            }
            AudioBackend::WasapiShared => device::list_output_devices()
                .into_iter()
                .map(|device| {
                    EngineDeviceInfo::from_daux_with_backend(
                        device,
                        Some("DAUx WASAPI Shared (Aggregate System)"),
                    )
                })
                .collect(),
            AudioBackend::Auto | AudioBackend::Cpal => device::list_output_devices()
                .into_iter()
                .map(EngineDeviceInfo::from_daux)
                .collect(),
        }
    }

    /// Enumerate input devices for the engine's configured backend.
    pub fn list_input_devices(&self) -> Vec<EngineDeviceInfo> {
        self.list_input_devices_for_backend(self.config.backend)
    }

    pub fn list_input_devices_for_backend(&self, backend: AudioBackend) -> Vec<EngineDeviceInfo> {
        match backend {
            AudioBackend::WdmKs => Vec::new(),
            AudioBackend::WasapiExclusive => device::list_input_devices()
                .into_iter()
                .map(EngineDeviceInfo::from_wasapi)
                .collect(),
            AudioBackend::Asio => {
                if self.ensure_asio_device_cache().is_ok() {
                    self.asio_devices.lock().inputs.clone()
                } else {
                    Vec::new()
                }
            }
            AudioBackend::WasapiShared => device::list_input_devices()
                .into_iter()
                .map(|device| {
                    EngineDeviceInfo::from_daux_with_backend(
                        device,
                        Some("DAUx WASAPI Shared (Aggregate System)"),
                    )
                })
                .collect(),
            AudioBackend::Auto | AudioBackend::Cpal => device::list_input_devices()
                .into_iter()
                .map(EngineDeviceInfo::from_daux)
                .collect(),
        }
    }

    /// Return the default output device descriptor, if the platform has one.
    pub fn default_output_device(&self) -> Option<EngineDeviceInfo> {
        self.list_output_devices()
            .into_iter()
            .find(|d| d.is_default)
    }

    /// The Audio Jam bridge belonging to this engine.
    ///
    /// Handed to the jam client so remote audio is written into the engine that
    /// already owns the device, instead of the jam opening a second one. The
    /// bus lives inside the shared state the audio callback already holds, so
    /// nothing new reaches the realtime path by handing this out.
    pub fn jam_bus(&self) -> std::sync::Arc<crate::engine::SharedState> {
        self.inner.jam_bus()
    }

    /// Polling snapshot for status bar / diagnostics.
    pub fn stats(&self) -> EngineStats {
        let st = self.inner.get_status();
        let daux = self.inner.get_daux_status();
        let transport = self.inner.transport_snapshot();
        let dropout = self.inner.dropout_diagnostics();
        EngineStats {
            running: st.running,
            stream_open: st.stream_open,
            transport_playing: self.inner.shared_playing(),
            position_seconds: transport.position_seconds,
            position_beats: transport.position_beats,
            position_samples: transport.position_samples,
            loop_enabled: transport.loop_enabled,
            bpm: transport.bpm,
            time_signature_num: transport.time_signature[0],
            time_signature_den: transport.time_signature[1],
            sample_rate: st.sample_rate,
            requested_sample_rate: daux.requested_sample_rate,
            buffer_size: st.buffer_size,
            backend_name: daux.backend_name,
            output_device: daux.output_device,
            last_error: daux.last_error.or(st.last_error),
            glitch_count: daux.glitch_count as u64,
            estimated_latency_ms: daux.estimated_latency_ms,
            input_latency_ms: daux.input_latency_ms,
            round_trip_latency_ms: daux.round_trip_latency_ms,
            device_lost: daux.device_lost,
            device_state: daux.device_state,
            dropout_protection_mode: dropout.protection_mode.as_str().to_string(),
            dropout_count: dropout.dropout_count,
            dropout_last_reason: dropout.last_reason.as_str().to_string(),
            active_voices: self.inner.active_voice_count(),
            callback_last_us: dropout.callback_last_us,
            callback_max_us: dropout.callback_max_us,
            callback_deadline_us: dropout.callback_deadline_us,
        }
    }

    /// Lightweight transport clock for latency-sensitive control input. Unlike
    /// [`AudioEngine::stats`], this avoids cloning device/status strings.
    pub fn transport_snapshot(&self) -> crate::transport::RuntimeTransportSnapshot {
        self.inner.transport_snapshot()
    }

    /// Attempt to recover the audio device after a device-loss event, reusing
    /// the last-known-good config. Returns `Ok(true)` if recovery ran,
    /// `Ok(false)` if the device was not lost.
    pub fn recover_device(&self) -> Result<bool, SphereAudioError> {
        self.inner.recover_daux()
    }

    /// Begin an input-level test on `device_id` (or the default input device).
    /// Poll [`AudioEngine::input_test_level`] for the meter and stop with
    /// [`AudioEngine::stop_input_test`].
    pub fn start_input_test(&self, device_id: Option<&str>) -> Result<(), SphereAudioError> {
        self.inner.start_input_test(device_id.map(str::to_string))
    }

    /// Peak input level since the last poll, `0.0..=1.0` (0 when inactive).
    pub fn input_test_level(&self) -> f32 {
        self.inner.get_input_test_level()
    }

    /// Stop and release the input-level test stream.
    pub fn stop_input_test(&self) {
        self.inner.stop_input_test();
    }

    /// Whether switching to `config` would require a controlled device restart
    /// versus the currently-open device.
    pub fn requires_restart(&self, config: &EngineConfig) -> bool {
        let daux = match Self::daux_config_from_engine_config(config) {
            Ok(daux) => daux,
            Err(_) => return true,
        };
        self.inner.daux_requires_restart(&daux)
    }

    /// Build/update the realtime runtime graph from a control-thread project snapshot.
    pub fn load_project(&self, snapshot: EngineProjectSnapshot) -> Result<(), SphereAudioError> {
        self.inner.load_project(snapshot)
    }

    pub fn start_recording(&self, config: JsStartRecordingConfig) -> Result<(), SphereAudioError> {
        self.inner.start_recording(config)
    }

    pub fn stop_recording(&self) -> Result<Vec<crate::types::JsRecordingResult>, SphereAudioError> {
        self.inner.stop_recording()
    }

    /// Detach the take without waiting for its files.
    ///
    /// Pair with [`Self::poll_stop_recording`]. The blocking `stop_recording`
    /// above is the same two calls back to back, kept for callers that have
    /// nothing to do while the disk works — the UI is not one of them.
    pub fn begin_stop_recording(&self) -> Result<(), SphereAudioError> {
        self.inner.begin_stop_recording()
    }

    /// Results of a detached take, or `None` while the writer is still going.
    pub fn poll_stop_recording(
        &self,
    ) -> Option<Result<Vec<crate::types::JsRecordingResult>, SphereAudioError>> {
        self.inner.poll_stop_recording()
    }

    pub fn export_rauf_to_wav(
        &self,
        rauf_path: &str,
        wav_path: &str,
    ) -> Result<crate::types::JsWavExportResult, SphereAudioError> {
        let report = sphere_encoder::wav::convert_rauf_to_wav(rauf_path, wav_path)
            .map_err(|error| SphereAudioError::NativeError(error.to_string()))?;
        Ok(crate::types::JsWavExportResult {
            file_path: wav_path.to_string(),
            frames_written: report.frames_written as f64,
            data_bytes: report.data_bytes as f64,
        })
    }

    pub fn recording_status(&self) -> crate::types::JsRecordingStatus {
        self.inner.get_recording_status()
    }

    /// Update a track or master parameter without rebuilding the full runtime graph.
    pub fn update_track_param(
        &self,
        track_id: &str,
        param_id: &str,
        value: f64,
    ) -> Result<(), SphereAudioError> {
        self.inner.update_track_param(track_id, param_id, value)
    }

    /// Set the linear monitor gain applied to the input bus before it reaches
    /// master. Drives the pinned Monitor strip's fader.
    pub fn set_monitor_gain(&self, value: f32) -> Result<(), SphereAudioError> {
        self.inner.set_monitor_gain(value)
    }

    /// Select what the Control Room monitors when no Listen is engaged.
    pub fn set_monitor_source(
        &self,
        source: crate::monitor::MonitorSource,
    ) -> Result<(), SphereAudioError> {
        self.inner.set_monitor_source(source)
    }

    /// Set Control Room gain, mute, dim, and mono in one atomic update.
    pub fn set_monitor_control(
        &self,
        control: crate::monitor::MonitorControl,
    ) -> Result<(), SphereAudioError> {
        self.inner.set_monitor_control(control)
    }

    /// Select the hardware output pair the Control Room feeds.
    pub fn set_monitor_output(
        &self,
        target: crate::monitor::MonitorOutputTarget,
    ) -> Result<(), SphereAudioError> {
        self.inner.set_monitor_output(target)
    }

    /// Publish which stage owns the physical output write, with Master's and
    /// the effective monitoring channel pairs already resolved.
    pub fn set_hardware_output_ownership(
        &self,
        owner: crate::monitor::HardwareOutputOwner,
        master: Option<(u16, u16)>,
        monitor: Option<(u16, u16)>,
    ) -> Result<(), SphereAudioError> {
        self.inner
            .set_hardware_output_ownership(owner, master, monitor)
    }

    /// Set one channel's Pre/After-Fader Listen state.
    pub fn set_track_listen(
        &self,
        track_id: &str,
        listen: crate::monitor::ListenMode,
    ) -> Result<(), SphereAudioError> {
        self.inner.set_track_listen(track_id, listen)
    }

    /// Clear Listen on every channel.
    pub fn clear_all_listen(&self) -> Result<(), SphereAudioError> {
        self.inner.clear_all_listen()
    }

    /// Apply only record-arm and effective monitor state while preserving the
    /// track's existing input route. Used by timeline controls that do not own
    /// the full route model.
    pub fn update_track_input_flags(
        &self,
        track_id: &str,
        record_armed: bool,
        monitor_enabled: bool,
    ) -> Result<(), SphereAudioError> {
        self.inner
            .update_track_input_flags(track_id, record_armed, monitor_enabled)
    }

    /// Apply an audio track's input route, arm state, and effective monitor state
    /// through the lightweight control path. This never reloads the project graph
    /// or restarts an already-open ASIO session.
    pub fn update_track_input_state(
        &self,
        track_id: &str,
        record_armed: bool,
        monitor_enabled: bool,
        input_source: crate::types::EngineTrackInputSourceSnapshot,
    ) -> Result<(), SphereAudioError> {
        self.inner
            .update_track_input_state(track_id, record_armed, monitor_enabled, input_source)
    }

    /// Start or stop sharing one track or bus over Audio Jam.
    ///
    /// Returns the publish slot the track now feeds, for the jam client to pull
    /// from. A publish is session state: it never reaches the project file and
    /// reopening a project does not resume it.
    pub fn set_track_jam_publish(
        &self,
        track_id: &str,
        enabled: bool,
    ) -> Result<Option<u32>, SphereAudioError> {
        self.inner.set_track_jam_publish(track_id, enabled)
    }

    /// Share `track_ids` as one Audio Jam multitrack stream, or stop with an
    /// empty list. Returns the channel count the stream carries.
    ///
    /// Like a single-track publish this is session state: it never reaches the
    /// project file, and reopening a project does not resume it.
    pub fn set_multitrack_jam_publish(
        &self,
        track_ids: &[String],
    ) -> Result<usize, SphereAudioError> {
        self.inner.set_multitrack_jam_publish(track_ids)
    }

    pub fn set_insert_param(
        &self,
        track_id: String,
        insert_id: String,
        param_id: String,
        value: f32,
    ) -> Result<(), SphereAudioError> {
        self.inner
            .set_insert_param(track_id, insert_id, param_id, value)
    }

    /// Clone the live runtime VST3 processor handle for an insert, if it has a
    /// ready native plugin instance. The GPUI PluginView uses this to open the
    /// editor from the *existing* instance/controller — never a new one — so
    /// GUI parameter edits affect the actual audio processor. The handle is
    /// `Arc`-backed; holding it keeps the C++ instance alive while the editor
    /// is open.
    pub fn insert_processor(
        &self,
        track_id: &str,
        insert_id: &str,
    ) -> Option<crate::vst3_processor::Vst3RuntimeProcessor> {
        self.inner.insert_processor(track_id, insert_id)
    }

    /// `(cpu_share, latency_samples)` for one insert — see
    /// [`crate::engine::EngineInner::insert_load`]. Control thread only.
    pub fn insert_load(&self, track_id: &str, insert_id: &str) -> Option<(f32, u32)> {
        self.inner.insert_load(track_id, insert_id)
    }

    /// Instantiate a VST3 plug-in in this process for ARA hosting.
    ///
    /// See [`crate::engine::EngineInner::create_ara_processor`] — ARA is the one
    /// plug-in path that cannot go through the out-of-process host.
    pub fn create_ara_processor(
        &self,
        plugin_path: &str,
        class_id: &str,
    ) -> Option<crate::vst3_processor::Vst3RuntimeProcessor> {
        self.inner.create_ara_processor(plugin_path, class_id)
    }

    /// Install the ARA playback renderers for one track, replacing any previous
    /// set. An empty list removes them.
    pub fn set_ara_renderers(
        &self,
        track_id: String,
        renderers: Vec<crate::runtime::RuntimeAraRenderer>,
    ) -> Result<(), SphereAudioError> {
        self.inner.set_ara_renderers(track_id, renderers)
    }

    /// Poll meter atomics and runtime track meters for UI display.
    pub fn meters(&self) -> JsMeterSnapshot {
        self.inner.get_meters()
    }

    /// Full audio-input pipeline diagnostics (Layer 10). Non-destructive — safe
    /// to poll alongside [`AudioEngine::meters`]. Use for a dev diagnostics
    /// panel or console dump to verify raw/bus/track input peaks at a glance.
    pub fn audio_diagnostics(&self) -> crate::types::JsAudioDiagnostics {
        self.inner.get_audio_diagnostics()
    }

    /// Emit a throttled (~500 ms) `[AudioRealtime]` trace when
    /// `FUTUREBOARD_INPUT_DEBUG` is set. Call from the UI poll loop; it is a
    /// cheap no-op when the env var is unset or the throttle window is open.
    pub fn log_input_debug(&self) {
        self.inner.log_input_debug_throttled();
    }

    /// Metadata + current bin count for the in-progress recording waveform
    /// preview (Part 1). Poll this, then drain with
    /// [`AudioEngine::drain_recording_preview_peaks`].
    pub fn recording_preview_info(&self) -> crate::types::JsRecordingPreviewInfo {
        self.inner.recording_preview_info()
    }

    /// Drain finalized preview peak bins in `[from_index, head)`. Cheap clone on
    /// the control thread; never blocks the audio path.
    pub fn drain_recording_preview_peaks(
        &self,
        from_index: f64,
    ) -> Vec<crate::types::JsWaveformPeak> {
        self.inner.drain_recording_preview_peaks(from_index)
    }

    /// Per-track recording-preview metadata (multi-track). Poll this, then
    /// drain each track with
    /// [`AudioEngine::drain_recording_preview_peaks_for_track`].
    pub fn recording_preview_tracks(&self) -> Vec<crate::types::JsRecordingPreviewTrackInfo> {
        self.inner.recording_preview_tracks()
    }

    /// Drain one track's finalized preview peak bins in `[from_index, head)`.
    pub fn drain_recording_preview_peaks_for_track(
        &self,
        track_id: String,
        from_index: f64,
    ) -> Vec<crate::types::JsWaveformPeak> {
        self.inner
            .drain_recording_preview_peaks_for_track(&track_id, from_index)
    }

    /// Aggregate latency report: device buffer latency plus per-track and master
    /// plug-in latency. Reporting only (Phase V); full plug-in delay
    /// compensation is Phase W.
    pub fn latency_info(&self) -> crate::types::JsLatencyInfo {
        self.inner.get_latency_info()
    }

    pub fn pdc_enabled(&self) -> bool {
        self.inner.pdc_enabled()
    }

    pub fn set_pdc_enabled(&self, enabled: bool) {
        self.inner.set_pdc_enabled(enabled);
    }

    pub fn latency_graph_version(&self) -> u64 {
        self.inner.latency_graph_version()
    }

    pub fn plugin_bridge_sinks(&self) -> crate::plugin_bridge::PluginBridgeSinkMap {
        self.inner.plugin_bridge_sinks()
    }

    /// Active Dropout Protection mode.
    pub fn dropout_protection_mode(&self) -> crate::engine::DropoutProtectionMode {
        self.inner.dropout_protection_mode()
    }

    /// Set the Dropout Protection mode (Settings → Playback).
    pub fn set_dropout_protection_mode(&self, mode: crate::engine::DropoutProtectionMode) {
        self.inner.set_dropout_protection_mode(mode);
    }

    /// Realtime dropout-protection diagnostics snapshot (atomic reads).
    pub fn dropout_diagnostics(&self) -> crate::engine::DropoutDiagnostics {
        self.inner.dropout_diagnostics()
    }

    /// Multi-LOD peak summary for any audio format supported by the
    /// engine's decoder. The Native UI's waveform pipeline calls this
    /// instead of running its own decoder, so the LOD ladder and decode
    /// quality stay in sync with the realtime engine's view of the file.
    ///
    /// Runs the full decode on the caller's thread — invoke from a
    /// background executor, never from render / layout / audio callback.
    pub fn generate_peaks(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<audio_file::AudioPeakFile, SphereAudioError> {
        audio_file::generate_audio_peaks(path)
    }

    /// Lightweight engine snapshot for diagnostic logs (Rust-side, no NAPI types).
    /// Use this to verify that clips made it into the realtime runtime — silent
    /// playback almost always means `loaded_clips == 0` or `ready_clips == 0`.
    pub fn debug_snapshot(&self) -> EngineDebugSnapshot {
        let info = self.inner.get_debug_info();
        EngineDebugSnapshot {
            loaded_tracks: info.loaded_tracks,
            loaded_clips: info.loaded_clips,
            ready_clips: info.ready_clips,
            is_playing: info.is_playing,
            position_seconds: info.position_seconds,
        }
    }

    /// Structured per-insert instantiation status (Phase 2b). Cheap — locks
    /// the runtime briefly and clones a small descriptor list. Used by the
    /// native shell to flip `InsertLoadStatus::Failed` on plugin load failure.
    pub fn insert_statuses(&self) -> Vec<EngineInsertStatus> {
        self.inner.insert_statuses()
    }
}

/// Per-insert instantiation status for UI readback (Phase 2b). Plain Rust,
/// no NAPI — consumed by the native shell's audio poll to surface
/// `InsertLoadStatus::Failed` when a native plugin fails to instantiate.
#[derive(Debug, Clone)]
pub struct EngineInsertStatus {
    pub track_id: String,
    pub insert_id: String,
    /// `true` when this is a native-plugin insert (vs. a built-in DSP).
    pub native: bool,
    /// `true` when the insert's processor is live. A native insert with
    /// `ready == false` is a definitive instantiation failure.
    pub ready: bool,
}

/// Plain-Rust mirror of the realtime engine's debug snapshot, suitable for
/// status-bar logs in the native shell.
#[derive(Debug, Clone, Default)]
pub struct EngineDebugSnapshot {
    pub loaded_tracks: u32,
    pub loaded_clips: u32,
    pub ready_clips: u32,
    pub is_playing: bool,
    pub position_seconds: f64,
}

// ── EngineInner helper accessor ──────────────────────────────────────────
//
// We need read access to `SharedState::playing` for the stats snapshot
// without exposing the entire shared state. A small accessor on
// `EngineInner` keeps the rest of the engine untouched.

impl EngineInner {
    /// Whether the transport is advancing. Used by [`AudioEngine::stats`].
    pub fn shared_playing(&self) -> bool {
        self.shared.playing.load(Ordering::Relaxed)
    }

    /// The Audio Jam bridge for this engine.
    ///
    /// Handed out so the jam client can write remote audio into the same
    /// engine that owns the device, instead of opening a second one. The bus is
    /// inside the shared state the audio callback already holds, so nothing new
    /// is published to the realtime path by handing this out.
    pub fn jam_bus(&self) -> Arc<crate::engine::SharedState> {
        Arc::clone(&self.shared)
    }
}

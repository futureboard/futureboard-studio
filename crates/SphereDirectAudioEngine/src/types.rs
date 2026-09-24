use serde::{Deserialize, Serialize};
use sphere_midi_service::mpe::MpeTrackConfiguration;
use sphere_midi_service::NoteExpression;
use sphere_soundfont_player::{SoundfontEnvelope, SoundfontRenderQuality};
use SphereAudioProcessor::StretchParams;

// ── DAUx backend selection types ──────────────────────────────────────────────

#[cfg(feature = "napi")]
use napi_derive::napi;

/// Information about one available DAUx backend.
#[cfg_attr(feature = "napi", napi(object))]
#[derive(Debug, Default, Clone)]
pub struct JsDauxBackendInfo {
    /// Machine-readable id: "auto" | "wasapi-shared" | "wasapi-exclusive" | "wdm-ks" | "coreaudio" | "alsa" | "mme"
    pub id: String,
    /// Human-readable name: "DAUx WASAPI Shared", etc.
    pub name: String,
    /// Whether this backend is currently usable on this platform.
    pub available: bool,
    /// Whether this is the platform default.
    pub is_default: bool,
    /// Short description.
    pub description: String,
}

/// Configuration for selecting / opening a DAUx backend.
#[cfg_attr(feature = "napi", napi(object))]
#[derive(Debug, Default, Clone)]
pub struct JsDauxConfig {
    /// Backend id string (see `JsDauxBackendInfo.id`).
    pub backend_id: String,
    /// Target output device name / id.  Empty = system default.
    pub output_device_id: Option<String>,
    /// Target sample rate in Hz (0 = device default).
    pub sample_rate: Option<u32>,
    /// Target buffer size in frames (0 = driver default).
    pub buffer_size: Option<u32>,
    /// Enable MMCSS "Pro Audio" thread priority on Windows.
    pub mmcss_priority: bool,
    /// Safe mode: use larger buffer to reduce glitches.
    pub safe_mode: bool,
}

/// Runtime status of the active DAUx backend.
#[cfg_attr(feature = "napi", napi(object))]
#[derive(Debug, Default, Clone)]
pub struct JsDauxStatus {
    /// Active backend id.
    pub backend_id: String,
    /// Active backend human-readable name.
    pub backend_name: String,
    /// Active output device name.
    pub output_device: Option<String>,
    /// Active sample rate (Hz) — the rate the opened stream actually runs at.
    /// This is the authoritative runtime rate used for all timing.
    pub sample_rate: u32,
    /// Sample rate the device was *requested* to open at (Hz), or 0 when no
    /// specific rate was requested ("device default"). May differ from
    /// `sample_rate` in WASAPI shared mode or after an exclusive-mode fallback.
    pub requested_sample_rate: u32,
    /// Active buffer size (frames).
    pub buffer_size: u32,
    /// Output latency (ms): the driver-reported figure on ASIO, the measured
    /// callback→play-out delay on cpal backends, else the buffer-size estimate.
    pub estimated_latency_ms: f64,
    /// Input (capture→callback) latency in ms, or `0` when the open backend
    /// has not reported/measured it.
    pub input_latency_ms: f64,
    /// `input_latency_ms + estimated_latency_ms`, or `0` when the input half
    /// is unknown — a round trip is never stated from one half.
    pub round_trip_latency_ms: f64,
    /// Number of audio glitches / underruns since the stream was opened.
    pub glitch_count: f64,
    /// Device-level underruns (ALSA xruns and equivalents) since the stream was
    /// opened. A subset of `glitch_count`, isolated so a starved audio thread
    /// is distinguishable from device-lost and other backend errors.
    pub device_xruns: f64,
    /// MMCSS priority active on audio thread (Windows only).
    pub mmcss_active: bool,
    /// Last backend error (e.g. WASAPI Exclusive failed reason). Cleared on success.
    pub last_error: Option<String>,
    /// `true` when the device disappeared mid-stream and a recovery is pending.
    pub device_lost: bool,
    /// Lifecycle state for the Audio Settings UI: "Closed" | "Ready" |
    /// "Running" | "DeviceLost".
    pub device_state: String,
}

// ── N-API–visible types ────────────────────────────────────────────────────────
// These cross the Rust/JS boundary via napi-derive.  Field names use camelCase
// so they arrive at JS looking natural.

#[cfg_attr(feature = "napi", napi(object))]
#[derive(Debug, Default)]
pub struct JsSphereAudioStatus {
    pub available: bool,
    pub running: bool,
    pub stream_open: bool,
    pub transport_playing: bool,
    pub position_seconds: f64,
    pub version: String,
    pub backend_name: String,
    pub sample_rate: u32,
    pub buffer_size: u32,
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    pub last_error: Option<String>,
}

#[cfg_attr(feature = "napi", napi(object))]
#[derive(Debug, Clone)]
pub struct JsAudioDeviceInfo {
    pub id: String,
    pub name: String,
    pub kind: String, // "input" | "output"
    pub channels: u32,
    pub default_sample_rate: u32,
    pub is_default: bool,
    pub backend: String,
}

#[cfg_attr(feature = "napi", napi(object))]
#[derive(Debug, Default)]
pub struct JsDeviceOpenConfig {
    pub input_device_id: Option<String>,
    pub output_device_id: Option<String>,
    pub sample_rate: Option<u32>,
    pub buffer_size: Option<u32>,
}

#[cfg_attr(feature = "napi", napi(object))]
#[derive(Debug, Default, Clone)]
pub struct JsTrackMeterSnapshot {
    pub track_id: String,
    pub peak_l: f64,
    pub peak_r: f64,
    pub rms_l: f64,
    pub rms_r: f64,
}

#[cfg_attr(feature = "napi", napi(object))]
#[derive(Debug, Default, Clone)]
pub struct JsPluginOutputMeterSnapshot {
    pub track_id: String,
    pub insert_id: String,
    pub channel: u32,
    pub peak: f64,
}

#[cfg_attr(feature = "napi", napi(object))]
#[derive(Debug, Default, Clone)]
pub struct JsMeterSnapshot {
    pub tracks: Vec<JsTrackMeterSnapshot>,
    pub plugin_outputs: Vec<JsPluginOutputMeterSnapshot>,
    pub master_peak_l: f64,
    pub master_peak_r: f64,
    pub master_rms_l: f64,
    pub master_rms_r: f64,
    pub input_peak_l: f64,
    pub input_peak_r: f64,
    /// True while at least one track has input monitoring engaged. Lets the
    /// Monitor strip distinguish "no input signal" from "monitoring is off".
    pub monitor_active: bool,
    /// Control Room output level — measured after the monitor insert chain and
    /// the monitor control processor, i.e. the signal actually leaving for the
    /// monitoring hardware output. Not the master bus level.
    pub monitor_peak_l: f64,
    pub monitor_peak_r: f64,
}

/// Per-track plugin latency (sum of enabled native-plugin insert latencies).
#[cfg_attr(feature = "napi", napi(object))]
#[derive(Debug, Default, Clone)]
pub struct JsTrackLatency {
    pub track_id: String,
    pub plugin_samples: u32,
    pub plugin_ms: f64,
    /// Path latency to master summing bus (Phase W graph).
    pub path_samples: u32,
    pub path_ms: f64,
    /// Playback delay compensation applied on this track's output.
    pub pdc_delay_samples: u32,
    pub pdc_delay_ms: f64,
}

/// Latency report for the Audio Settings / mixer UI (Phase V — reporting only;
/// full plug-in delay compensation is Phase W).
#[cfg_attr(feature = "napi", napi(object))]
#[derive(Debug, Default, Clone)]
pub struct JsLatencyInfo {
    pub sample_rate: u32,
    /// Output buffer size in frames (device/buffer latency basis).
    pub buffer_frames: u32,
    pub buffer_ms: f64,
    /// Per non-master track plugin latency.
    pub tracks: Vec<JsTrackLatency>,
    /// Plugin latency on the master track.
    pub master_samples: u32,
    pub master_ms: f64,
    /// Longest path latency to master — PDC basis (Phase W).
    pub max_path_samples: u32,
    pub max_path_ms: f64,
    /// Whether playback PDC is active (`FUTUREBOARD_PDC=0` disables compensation).
    pub pdc_enabled: bool,
    /// Longest per-track plugin latency (legacy field kept for older UI callers).
    pub max_track_samples: u32,
}

#[cfg_attr(feature = "napi", napi(object))]
#[derive(Debug, Default, Clone)]
pub struct JsWavPeakResult {
    pub file_id: String,
    pub sample_rate: u32,
    pub channel_count: u32,
    pub duration: f64,
    pub samples_per_peak: u32,
    pub peak_count: u32,
    /// Interleaved Int16 min/max pairs per peak/channel, widened for N-API.
    pub peaks: Vec<i32>,
}

#[cfg_attr(feature = "napi", napi(object))]
#[derive(Debug, Default, Clone)]
pub struct JsWavExportResult {
    pub file_path: String,
    pub frames_written: f64,
    pub data_bytes: f64,
}

#[cfg_attr(feature = "napi", napi(object))]
#[derive(Debug, Default, Clone)]
pub struct JsAudioFileInfo {
    pub path: String,
    pub sample_rate: u32,
    pub channel_count: u32,
    pub total_frames: f64,
    pub duration_seconds: f64,
    pub format: String,
}

// ── Internal (non-napi) serializable types ────────────────────────────────────
// These live purely on the Rust side and are used for project snapshots
// passed as JSON strings from the JS side.

/// A tempo marker passed from the UI project TempoMap.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct EngineTempoPointSnapshot {
    pub beat: f64,
    pub bpm: f64,
    /// How the tempo travels to the next marker, as
    /// [`crate::tempo_map::TempoCurve::to_tag`]. Defaulted so a project written
    /// before curves existed still loads as the step-hold map it was.
    #[serde(default)]
    pub curve: u8,
    /// Bend of a Linear ramp, `-1.0..=1.0` ([`crate::tempo_map::TempoPoint::tension`]).
    #[serde(default)]
    pub tension: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineProjectSnapshot {
    pub project_id: String,
    #[serde(default)]
    pub project_root: Option<String>,
    /// Globally-selected input device (Preferences → Audio → Input Device).
    /// Used as the fallback capture device for armed/monitored tracks whose
    /// own input routing does not pin a specific device (e.g. "All Inputs").
    /// `None`/empty falls back to the system default input.
    #[serde(default)]
    pub preferred_input_device: Option<String>,
    pub bpm: f64,
    /// Project-level tempo automation markers. Empty = static tempo at `bpm`.
    #[serde(default)]
    pub tempo_points: Vec<EngineTempoPointSnapshot>,
    pub time_signature: [u32; 2],
    pub sample_rate: u32,
    pub tracks: Vec<EngineTrackSnapshot>,
    pub clips: Vec<EngineClipSnapshot>,
    /// MIDI clips (Phase 2). Defaulted so older snapshots without the field
    /// still deserialize. Notes are stored relative to the clip start; the
    /// runtime converts them to absolute project beats/samples at build time.
    #[serde(default)]
    pub midi_clips: Vec<EngineMidiClipSnapshot>,
    /// Whether playback plug-in delay compensation (Global Latency Sync / PDC)
    /// is active. Carried in the snapshot so the offline exporter consumes the
    /// *same* latency-compensated graph as realtime playback instead of a
    /// separate behavior (see `export::offline_renderer`). Defaults to `true`
    /// (the engine default) for snapshots that predate the field.
    #[serde(default = "default_true")]
    pub pdc_enabled: bool,
    /// Realtime latency-graph generation this snapshot was stamped against.
    /// Set by the export snapshot builder from the live engine; used only for
    /// export diagnostics (graph-version parity warning). `0` = unstamped.
    #[serde(default)]
    pub latency_graph_version: u64,
    pub routing: EngineRoutingSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineMidiClipSnapshot {
    pub id: String,
    pub track_id: String,
    pub start_beat: f64,
    pub length_beats: f64,
    pub notes: Vec<EngineMidiNoteSnapshot>,
    /// MIDI controller (CC / pitch-bend / aftertouch) lanes for this clip.
    #[serde(default)]
    pub controllers: Vec<EngineMidiControllerLane>,
    /// Track-level MPE routing copied into the clip snapshot so realtime and
    /// offline MIDI scheduling use the same zone policy.
    #[serde(default)]
    pub mpe: MpeTrackConfiguration,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineMidiControllerPoint {
    /// Beat relative to the clip start.
    pub beat: f64,
    /// Normalized controller value, `0.0..=1.0`.
    pub value: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineMidiControllerLane {
    /// VST3 controller number: `0..=127` = MIDI CC, `128` = aftertouch,
    /// `129` = pitch bend. Matches `Steinberg::Vst::ControllerNumbers`.
    pub controller: u16,
    #[serde(default)]
    pub channel: u8,
    pub points: Vec<EngineMidiControllerPoint>,
}

/// One breakpoint of a note's sounding pitch, in absolute frequency.
///
/// Hz — not a semitone offset and not a pitch-bend value — because the
/// instruments that consume this (bowed-string physical models, the indexed
/// voicebank) drive a resonator/read pointer in Hz, and because the editor's
/// model is continuous: there is no 12-TET grid to bend away from.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnginePitchPoint {
    /// Beat relative to the **clip** start, matching every other beat in this
    /// snapshot.
    pub beat: f64,
    pub hz: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineMidiNoteSnapshot {
    pub id: u64,
    pub pitch: u8,
    /// Start beat relative to the clip start.
    pub start_beat: f64,
    pub length_beats: f64,
    pub velocity: u8,
    #[serde(default)]
    pub channel: u8,
    /// Note-owned expression. Transport/channel assignment is resolved by
    /// the playback adapter, so this field remains valid for MPE, native
    /// plug-in note expression, and future MIDI 2.0 output.
    #[serde(default)]
    pub expression: NoteExpression,
    /// Which recorded articulation this note asks the instrument to play.
    ///
    /// A **sampled-instrument** articulation id (pizzicato, spiccato, sustain,
    /// tremolo — the vocabulary of the recordings in the bank), not the score
    /// marking the editor shows. The two are different alphabets and the
    /// snapshot builder is where they are translated, so the engine never has
    /// to know about notation and the editor never has to know which
    /// recordings a particular bank happens to contain.
    ///
    /// `None` means "whatever the instrument's default is" — the behaviour
    /// before any articulation reached the engine at all.
    #[serde(default)]
    pub articulation: Option<u16>,
    /// Sounding-pitch trajectory across this note, already composed from the
    /// notated pitch, the drawn pitch curve and any note-to-note transition.
    ///
    /// **Empty means "this note sounds at `pitch`"** — the common case, and the
    /// reason a project full of untouched notes costs nothing here. Points are
    /// only emitted where the trajectory actually departs from the notated
    /// pitch, and are decimated to a musically inaudible tolerance, so this
    /// stays small even for a heavily drawn phrase.
    #[serde(default)]
    pub pitch_points: Vec<EnginePitchPoint>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineTrackSnapshot {
    pub id: String,
    #[serde(rename = "type")]
    pub track_type: String,
    pub volume: f32,
    pub pan: f32,
    pub muted: bool,
    pub solo: bool,
    pub armed: bool,
    #[serde(default)]
    pub input_monitor: bool,
    #[serde(default)]
    pub input_source: EngineTrackInputSourceSnapshot,
    #[serde(default = "default_preview_mode")]
    pub preview_mode: String,
    pub output_track_id: Option<String>,
    pub inserts: Vec<EngineInsertSnapshot>,
    #[serde(default)]
    pub sends: Vec<EngineSendSnapshot>,
    #[serde(default)]
    pub automation_lanes: Vec<EngineAutomationLaneSnapshot>,
    #[serde(default)]
    pub builtin_soundfont_player: bool,
    #[serde(default)]
    pub soundfont_path: Option<String>,
    #[serde(default)]
    pub soundfont_preset_bank: Option<i32>,
    #[serde(default)]
    pub soundfont_preset_patch: Option<i32>,
    #[serde(default = "default_soundfont_volume")]
    pub soundfont_volume: f32,
    #[serde(default = "default_true")]
    pub soundfont_reverb_chorus: bool,
    #[serde(default = "default_soundfont_polyphony")]
    pub soundfont_polyphony: usize,
    /// Amp envelope over the built-in player's output. Defaults to the bypassed
    /// envelope, which is what every project written before it existed had.
    #[serde(default)]
    pub soundfont_envelope: SoundfontEnvelope,
    /// Internal synthesis oversampling for the built-in player.
    #[serde(default)]
    pub soundfont_quality: SoundfontRenderQuality,
    /// Native Solfege physical/hybrid instrument wrapper. Kept optional so
    /// existing snapshots remain source-compatible and deserialize unchanged.
    #[serde(default)]
    pub solfege_engine: Option<EngineSolfegeSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineSolfegeSnapshot {
    #[serde(default)]
    pub model_path: Option<String>,
    #[serde(default = "default_solfege_instrument")]
    pub instrument: String,
    #[serde(default = "default_solfege_voice")]
    pub voice: String,
    #[serde(default = "default_solfege_preset")]
    pub preset: String,
    #[serde(default = "default_solfege_parameter")]
    pub bow_pressure: f32,
    #[serde(default = "default_solfege_vibrato")]
    pub vibrato: f32,
    #[serde(default = "default_solfege_dynamics")]
    pub dynamics: f32,
    #[serde(default = "default_solfege_expression")]
    pub expression: f32,
}

fn default_solfege_instrument() -> String {
    "Violin".to_string()
}

fn default_solfege_voice() -> String {
    "Solo Bowed String".to_string()
}

fn default_solfege_preset() -> String {
    "VSCO Solo Violin".to_string()
}

fn default_solfege_parameter() -> f32 {
    0.62
}

fn default_solfege_vibrato() -> f32 {
    0.18
}

fn default_solfege_dynamics() -> f32 {
    0.78
}

fn default_solfege_expression() -> f32 {
    1.0
}

fn default_soundfont_volume() -> f32 {
    1.0
}

fn default_soundfont_polyphony() -> usize {
    64
}

fn default_preview_mode() -> String {
    "stereo".to_string()
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineTrackInputSourceSnapshot {
    #[serde(default)]
    pub device_id: Option<String>,
    #[serde(default)]
    pub channels: Vec<u32>,
}

impl EngineTrackInputSourceSnapshot {
    /// Whether this route names an Audio Jam stream rather than a hardware
    /// device.
    ///
    /// A jam route must never reach the capture-stream logic: there is no
    /// device to open, no channel count to validate against a driver, and no
    /// reason for two tracks on two different remote performers to be treated
    /// as a routing conflict.
    pub fn is_jam(&self) -> bool {
        self.device_id
            .as_deref()
            .is_some_and(crate::jam_bus::is_jam_device)
    }

    /// Whether this route reads another track's output rather than a device.
    ///
    /// Like a jam route, it must never reach the capture-stream logic: there is
    /// no device to open and no channel count to validate against a driver.
    pub fn is_loopback(&self) -> bool {
        self.device_id
            .as_deref()
            .is_some_and(crate::loopback::is_loopback_device)
    }

    /// The source track this route names, if any.
    pub fn loopback_track_id(&self) -> Option<&str> {
        self.device_id
            .as_deref()
            .and_then(crate::loopback::loopback_track_id)
    }

    /// The remote stream this route names, if any.
    pub fn jam_stream_id(&self) -> Option<&str> {
        self.device_id
            .as_deref()
            .and_then(crate::jam_bus::jam_stream_id)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineInsertSnapshot {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub enabled: bool,
    pub params: std::collections::HashMap<String, serde_json::Value>,
    /// Packed `Vst3PluginState` ("FBV3" blob) to restore into an in-process
    /// processor before rendering. Set only by the offline-export snapshot
    /// builder; `None` (and omitted from serialization) on the live path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineSendSnapshot {
    pub id: String,
    pub return_track_id: String,
    pub level: f32,
    pub enabled: bool,
    /// `true` taps the signal before the source track fader; `false`
    /// (default) taps post-fader. Phase 3.
    #[serde(default)]
    pub pre_fader: bool,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineAutomationTargetSnapshot {
    /// Matches the native UI AutomationTarget tag:
    /// 0 volume, 1 pan, 2 mute, 3 plugin parameter, 4 send gain.
    pub tag: u8,
    #[serde(default)]
    pub insert_id: String,
    #[serde(default)]
    pub parameter_id: String,
    #[serde(default)]
    pub parameter_name: String,
    #[serde(default)]
    pub send_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineAutomationPointSnapshot {
    pub beat: f64,
    /// Normalized lane value in `0.0..=1.0`.
    pub value: f32,
    /// 0 linear, 1 hold, 2 smooth (S-curve). Stored on the left point of a
    /// segment; controls the shape toward the next point.
    #[serde(default)]
    pub curve: u8,
    /// Per-segment curve tension in `-1.0..=1.0` for the linear/curved kind:
    /// `0` = straight, `> 0` eases in (exponential), `< 0` eases out
    /// (logarithmic). Defaulted so curve-less projects load as straight lines.
    #[serde(default)]
    pub tension: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineAutomationLaneSnapshot {
    pub id: String,
    pub name: String,
    pub target: EngineAutomationTargetSnapshot,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub points: Vec<EngineAutomationPointSnapshot>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineClipSnapshot {
    pub id: String,
    pub track_id: String,
    pub asset_id: String,
    pub media_path: Option<String>,
    pub start_beat: f64,
    pub duration_beats: f64,
    pub offset_seconds: f64,
    pub gain: f32,
    /// Clip-level mute. Distinct from track mute — a muted clip is silent even
    /// on an audible track. Defaulted so older snapshots deserialize.
    #[serde(default)]
    pub muted: bool,
    /// An ARA plug-in renders this clip, so the engine must not mix the source
    /// file for it. Defaulted so older snapshots deserialize as non-ARA.
    #[serde(default)]
    pub ara_rendered: bool,
    #[serde(default)]
    pub fades: Option<EngineFadeSnapshot>,
    /// Authoritative audio stretch parameters for this clip. Defaults to Off for
    /// older snapshots; legacy `audio_process` is migrated at runtime only when
    /// this field is absent/default.
    #[serde(default)]
    pub stretch: StretchParams,
    #[serde(default)]
    pub audio_process: Option<EngineClipAudioProcess>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineFadeSnapshot {
    pub in_duration: f64,
    pub out_duration: f64,
    pub in_curve: String,
    pub out_curve: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineClipAudioProcess {
    pub speed_ratio: f64,
    #[serde(default = "default_one_f64")]
    pub effective_time_ratio: f64,
    #[serde(default = "default_one_f64")]
    pub pitch_ratio: f64,
    pub pitch_semitones: f64,
    pub preserve_pitch: bool,
    pub mode: String,
    pub quality: String,
    #[serde(default)]
    pub source_start_samples: u64,
    #[serde(default)]
    pub source_end_samples: u64,
    #[serde(default)]
    pub warp_markers: Vec<EngineWarpMarkerSnapshot>,
    /// Play the source window backwards. `speed_ratio` already folds time-stretch
    /// and pitch; reverse only flips the read direction. Defaulted so older
    /// snapshots deserialize.
    #[serde(default)]
    pub reverse: bool,
    /// Adaptive clip de-noise amount. Older snapshots deserialize as bypass.
    #[serde(default)]
    pub denoise_amount: f32,
    /// Clip channel transform. Older snapshots deserialize as stereo.
    #[serde(default)]
    pub channel_transform: u8,
    #[serde(default)]
    pub dc_remove: bool,
    #[serde(default)]
    pub dc_left: f32,
    #[serde(default)]
    pub dc_right: f32,
    /// Extra linear gain applied after clip gain. Preview overlay uses this;
    /// project snapshots keep it at 1.0.
    #[serde(default = "default_one_f32")]
    pub extra_gain: f32,
    #[serde(default)]
    pub dehum_hz: f32,
    #[serde(default)]
    pub dehum_harmonics: u8,
    #[serde(default)]
    pub dehum_reduction_db: f32,
    /// Envelope points as (normalized time, dB). Empty means unity.
    #[serde(default)]
    pub envelope_points: Vec<(f32, f32)>,
    #[serde(default)]
    pub preview_bypass: bool,
}

fn default_one_f32() -> f32 {
    1.0
}

fn default_one_f64() -> f64 {
    1.0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineWarpMarkerSnapshot {
    pub id: u64,
    pub source_sample: u64,
    pub timeline_beat: f64,
    pub locked: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineRoutingSnapshot {
    pub master_output_device: Option<String>,
    pub sample_rate: u32,
    pub buffer_size: u32,
}

/// Mutable engine status stored inside the engine under a lock.
/// Not exposed to JS directly — converted to JsSphereAudioStatus on read.
#[derive(Debug, Default, Clone)]
pub struct EngineStatus {
    pub stream_open: bool,
    pub running: bool,
    pub sample_rate: u32,
    pub buffer_size: u32,
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    pub last_error: Option<String>,
    pub loaded_project_id: Option<String>,
    /// Last WASAPI / backend error, displayed in Audio Settings UI.
    pub last_daux_error: Option<String>,
}

// ── Recording types ───────────────────────────────────────────────────────────

/// Config for one armed track being recorded.
#[cfg_attr(feature = "napi", napi(object))]
#[derive(Debug, Default, Clone)]
pub struct JsRecordingTrackConfig {
    pub track_id: String,
    /// 0-based input channel indices (e.g., [0, 1] for the first stereo pair).
    pub input_channels: Vec<u32>,
    /// Human-readable track name — used to derive the output filename.
    pub name: String,
}

/// Full config passed to `startRecording()`.
#[cfg_attr(feature = "napi", napi(object))]
#[derive(Debug, Default, Clone)]
pub struct JsStartRecordingConfig {
    /// Absolute path to the project folder root.
    pub project_root: String,
    /// Human-readable project name used as the recording filename prefix.
    pub project_name: String,
    /// Unique ID for this recording session (used to name temp files).
    pub session_id: String,
    /// Stable timestamp string for this recording take.
    pub timestamp: String,
    pub bpm: f64,
    pub start_beat: f64,
    pub sample_rate: u32,
    /// Input device name/id (None = system default).
    pub input_device_id: Option<String>,
    /// Armed tracks to record.
    pub tracks: Vec<JsRecordingTrackConfig>,
    /// Mix live input onto the master output while recording (software monitor).
    pub monitor_mix: bool,
    /// 0-based input channel indices used for software monitoring. One channel
    /// is duplicated to stereo; two or more use the first stereo pair.
    pub monitor_channels: Vec<u32>,
    /// When true, monitoring starts immediately but file capture, recording
    /// peaks, and preview bins wait for the transport to play. None/false keeps
    /// the direct API's immediate-capture behavior.
    pub capture_on_transport: Option<bool>,
}

/// Per-track result returned by `stopRecording()`.
#[cfg_attr(feature = "napi", napi(object))]
#[derive(Debug, Default, Clone)]
pub struct JsRecordingResult {
    pub track_id: String,
    /// Absolute path to the finalized internal recording file.
    pub file_path: String,
    /// Path relative to project root (e.g., "recordings/take_0001.rauf").
    pub relative_path: String,
    /// Transport beat at which recording started.
    pub start_beat: f64,
    pub duration_seconds: f64,
    pub sample_rate: u32,
    pub channels: u32,
    /// Sidecar metadata path relative to project root.
    pub metadata_path: String,
    /// PCM encoding for the internal recording file, currently "s32le".
    pub sample_format: String,
    /// Measured audio round-trip latency (output play-out + input capture) in
    /// seconds. The UI shifts the committed take earlier by this much so a live
    /// overdub lines up with what the performer heard. `0.0` when the backend
    /// reports no timestamp (no auto-compensation, matching the old behaviour).
    pub latency_seconds: f64,
    pub success: bool,
    pub error: Option<String>,
}

/// Snapshot of recording state for UI polling.
#[cfg_attr(feature = "napi", napi(object))]
#[derive(Debug, Default, Clone)]
pub struct JsRecordingStatus {
    pub active: bool,
    pub duration_seconds: f64,
    pub track_count: u32,
}

/// Debug state snapshot returned by `getDebugInfo()`.
/// Exposes the internal runtime graph so JS can verify the engine is loaded.
#[cfg_attr(feature = "napi", napi(object))]
#[derive(Debug, Default)]
pub struct JsEngineDebugInfo {
    /// Project ID from the last loaded snapshot.
    pub project_id: Option<String>,
    /// Number of tracks in the current runtime graph.
    pub loaded_tracks: u32,
    /// Number of clips in the current runtime graph (only clips with resolved paths).
    pub loaded_clips: u32,
    /// Number of clips whose audio buffer has frames > 0 (successfully decoded).
    pub ready_clips: u32,
    /// Whether the transport is currently playing.
    pub is_playing: bool,
    /// Current transport position in seconds.
    pub position_seconds: f64,
    /// Current transport position in beats (via tempo map; static BPM today).
    pub position_beats: f64,
    /// Whether loop playback is enabled in the engine.
    pub loop_enabled: bool,
    /// Whether any track has solo enabled.
    pub has_solo: bool,
    /// Human-readable summary of each loaded clip (id, trackId, startSec, durationSec, frames).
    pub clip_summaries: Vec<String>,
    /// Human-readable summary of inserts, including whether native VST3 processors are active.
    pub insert_summaries: Vec<String>,
    /// Total disk-stream underruns since process start (Phase F diagnostics).
    /// A streaming clip read that found its frame outside the buffered window.
    pub disk_underruns: f64,
    /// Number of active bounded disk-stream sources.
    pub disk_stream_active_sources: f64,
    /// Realtime streaming ring reads from the audio callback.
    pub disk_stream_cache_reads: f64,
    /// Reads served from already-buffered disk stream data.
    pub disk_stream_cache_hits: f64,
    /// Reads that missed the buffered disk stream window.
    pub disk_stream_cache_misses: f64,
    /// Approximate bounded stream cache memory currently allocated.
    pub disk_stream_cache_memory_used_mb: f64,
    /// Approximate bounded stream cache memory budget currently allocated.
    pub disk_stream_cache_memory_budget_mb: f64,
    /// Number of decoder/read blocks completed by stream workers.
    pub disk_stream_blocks_decoded: f64,
    /// Number of frames decoded/read by stream workers.
    pub disk_stream_frames_decoded: f64,
    /// Declarative audio graph node count (Phase O).
    pub graph_node_count: u32,
    /// Pass-1 source track count in the runtime graph plan.
    pub graph_pass1_count: u32,
    /// Pass-2 routing track count in topological order.
    pub graph_pass2_count: u32,
    /// Sends/main outputs rejected at graph plan time (cycle-unsafe or invalid target).
    pub graph_rejected_route_count: u32,
    /// Human-readable rejected route summaries for UI diagnostics.
    pub graph_rejected_route_summaries: Vec<String>,
}

// ── Audio input diagnostics (Layer 10) ─────────────────────────────────────────

/// Per-track input/monitor state snapshot for the audio diagnostics panel.
#[cfg_attr(feature = "napi", napi(object))]
#[derive(Debug, Default, Clone)]
pub struct JsTrackInputDiagnostics {
    pub track_id: String,
    pub record_armed: bool,
    pub monitor_enabled: bool,
    /// Human-readable input source: "None" | "Mono(ch)" | "Stereo(l,r)".
    pub input_source: String,
    pub track_input_peak: f64,
    pub track_output_peak: f64,
}

/// Whole-pipeline diagnostics snapshot. Mirrors the `AudioDiagnostics` struct in
/// the task spec — lets the UI (or a dev console dump) verify every layer of the
/// input path at a glance.
#[cfg_attr(feature = "napi", napi(object))]
#[derive(Debug, Default, Clone)]
pub struct JsAudioDiagnostics {
    pub backend: String,
    pub input_device_name: Option<String>,
    pub output_device_name: Option<String>,
    pub input_stream_running: bool,
    pub output_stream_running: bool,
    pub input_sample_rate: u32,
    pub input_channels: u32,
    /// Raw peak straight from the input callback (Layer 3).
    pub raw_input_peak: f64,
    /// Peak after the render callback reads the input ring (Layer 4).
    pub input_bus_peak: f64,
    /// Master output peak (Layer 7).
    pub output_peak: f64,
    pub tracks: Vec<JsTrackInputDiagnostics>,

    // ── Realtime counters (Part 4) ───────────────────────────────────────
    pub input_callback_count: f64,
    pub output_callback_count: f64,
    pub input_frames_received: f64,
    pub monitor_frames_consumed: f64,
    pub monitor_ring_underruns: f64,
    pub monitor_ring_overruns: f64,
    pub record_ring_overruns: f64,
    pub output_xruns: f64,
    pub monitor_output_peak: f64,
    pub record_peak: f64,
}

// ── Recording waveform preview (Part 1) ────────────────────────────────────────

/// One realtime preview peak bin (min/max/rms of one preview window).
#[cfg_attr(feature = "napi", napi(object))]
#[derive(Debug, Default, Clone, Copy)]
pub struct JsWaveformPeak {
    pub min: f64,
    pub max: f64,
    pub rms: f64,
}

/// Metadata + current bin count for the in-progress recording preview. The UI
/// polls this, then drains new bins with `drainRecordingPreviewPeaks(from)`.
#[cfg_attr(feature = "napi", napi(object))]
#[derive(Debug, Default, Clone)]
pub struct JsRecordingPreviewInfo {
    pub active: bool,
    /// Monotonic take id — changes between takes so the UI can drop stale data.
    pub recording_id: f64,
    /// Transport sample at which the take started (preview clip origin).
    pub start_sample: f64,
    pub sample_rate: u32,
    pub channels: u32,
    pub peaks_per_second: u32,
    /// Total bins produced so far (drain target / head index).
    pub peak_count: f64,
}

/// Per-track metadata + current bin count for one armed track's in-progress
/// recording preview (multi-track). The UI polls
/// `recordingPreviewTracks()`, then drains each track with
/// `drainRecordingPreviewPeaksForTrack(trackId, from)`.
#[cfg_attr(feature = "napi", napi(object))]
#[derive(Debug, Default, Clone)]
pub struct JsRecordingPreviewTrackInfo {
    pub track_id: String,
    /// Monotonic take id — changes between takes so the UI can drop stale data.
    pub recording_id: f64,
    /// Transport sample at which the take started (preview clip origin).
    pub start_sample: f64,
    pub sample_rate: u32,
    pub peaks_per_second: u32,
    /// Total bins produced so far (drain target / head index).
    pub peak_count: f64,
}

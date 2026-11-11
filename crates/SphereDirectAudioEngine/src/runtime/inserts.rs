//! Runtime inserts: kinds, ARA renderers, signal buses and VSTi output children.

use super::*;

/// `RuntimeInsert::kind` resolved to a compact tag at build time so the render
/// path never does per-block string compares (realtime rules).
/// One ARA playback renderer bound to a track.
///
/// ARA does not change how a plug-in is processed — it changes where its audio
/// comes from. The renderer is fed silence and produces the clip audio itself,
/// having read the source samples out of band through the ARA host callbacks in
/// `SphereAraHost`. The document graph and the binding live there; the engine
/// only owns the instance and calls it once per block.
///
/// `Clone` shares the instance the same way `RuntimeInsert` does: a runtime
/// graph is rebuilt by cloning, and the plug-in instance must survive that
/// instead of being torn down and re-created (which would drop its ARA binding).
#[derive(Debug, Clone)]
pub struct RuntimeAraRenderer {
    /// Insert/instance identity, matching the id the app bound in the session.
    pub instance_id: String,
    /// The in-process VST3 instance. Same handle type as a native insert; ARA
    /// binding happened on the control thread before this snapshot was built.
    pub processor: crate::vst3_processor::Vst3RuntimeProcessor,
    /// Reported plug-in latency, folded into the track's PDC total at build.
    pub latency_samples: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeInsertKind {
    /// In-process VST3 (`kind == "native-plugin"`).
    NativePlugin,
    /// Out-of-process bridged plugin (`kind == "external-bridge-plugin"`).
    ExternalBridge,
    /// Built-in DSP insert (everything else).
    BuiltIn,
}

impl RuntimeInsertKind {
    pub fn from_kind(kind: &str) -> Self {
        if kind.eq_ignore_ascii_case("native-plugin") {
            Self::NativePlugin
        } else if kind.eq_ignore_ascii_case("external-bridge-plugin") {
            Self::ExternalBridge
        } else {
            Self::BuiltIn
        }
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeInsert {
    pub id: String,
    pub kind: String,
    /// [`Self::kind`] resolved at build time — the audio callback branches on
    /// this tag, never on the string.
    pub kind_tag: RuntimeInsertKind,
    pub enabled: bool,
    /// The owning track's Insert Bypass automation lane for this insert,
    /// resolved to an index into its `automation_lanes` off the audio thread.
    pub bypass_lane: Option<usize>,
    /// Set per block from that lane while playing: the insert is bypassed by
    /// automation, whatever `enabled` says. Read through [`Self::active`].
    pub automation_bypassed: bool,
    pub params: std::sync::Arc<HashMap<String, Value>>,
    /// For [`RuntimeInsertKind::ExternalBridge`]: `params["role"] == "effect"`,
    /// resolved at build time so the block path never reads the params map.
    pub bridge_is_effect: bool,
    /// For [`RuntimeInsertKind::ExternalBridge`]: `params["format"] == "BuiltIn"`
    /// (a Futureboard built-in DSP hosted in the bridge process), resolved at
    /// build time. Built-in param edits travel as raw editor-unit values over
    /// the shared param ring, not normalized VST3 0..1.
    pub bridge_is_builtin: bool,
    /// For bridged instruments: 1-based plugin output channels selected by the
    /// inspector for the engine-side stereo downmix. Empty means all reported
    /// bridge channels so legacy/unsynced projects still pass multi-out audio.
    pub bridge_enabled_output_channels: Vec<u8>,
    /// For [`RuntimeInsertKind::ExternalBridge`]: the installed realtime sink.
    /// Cached from [`RuntimeProject::plugin_bridge_sinks`] by
    /// [`RuntimeProject::resolve_bridge_sinks`] (LoadProject /
    /// SetPluginBridgeSink) so the block path never does a `HashMap<String, _>`
    /// lookup.
    pub bridge_sink: Option<crate::plugin_bridge::SharedPluginBridgeSink>,
    pub dsp: InsertDspState,
    pub vst3: Option<Vst3RuntimeProcessor>,
    /// Smoothed cost of this insert's last blocks, in microseconds.
    ///
    /// Written by the audio callback and read by the control thread, so it is an
    /// atomic rather than a plain field: a plug-in editor showing what its own
    /// plug-in costs is the point of measuring it, and that read comes from the
    /// UI. `Arc` because a graph rebuild moves the insert but the meter should
    /// not restart.
    pub cpu_us: std::sync::Arc<std::sync::atomic::AtomicU32>,
    /// Audioguard's watch on this insert's output and deadlines.
    pub guard: crate::audioguard::InsertGuard,
    pub callback_process_log_done: bool,
    pub silent_process_blocks: u32,
    /// Consecutive blocks the external plugin host failed to deliver on time
    /// (its `read_output` returned 0). Drives the throttled missed-deadline /
    /// recovered logs in `apply_external_bridge_insert_block`.
    pub bridge_missed_blocks: u32,
    pub scratch_l: Vec<f32>,
    pub scratch_r: Vec<f32>,
    /// Multi-out (Slice 1): per-channel routes from this bridged instrument's
    /// output channels to child "Out Ch N" tracks. Empty unless the project
    /// defines separate-output child strips, so the default single-track fold
    /// path is unaffected. When non-empty, `apply_external_bridge_insert_block`
    /// reads the full plugin block once into [`Self::scratch_multi`] and the
    /// engine scatters each pair into the child track's receive buffer.
    pub vsti_output_children: Vec<RuntimeVstiOutputChild>,
    /// Interleaved multi-channel read buffer, sized lazily to `frames * channels`
    /// only when [`Self::vsti_output_children`] is non-empty (no allocation on
    /// the default path).
    pub scratch_multi: Vec<f32>,
    /// Latency from the track's input through this insert, inclusive, as last
    /// observed by [`RuntimeProject::refresh_runtime_latency_graph`]. A
    /// multi-out instrument's child strips are fed from right here, so their
    /// compensation is planned from it.
    pub chain_latency_samples: u32,
    /// `0` processes in the owning track's chain. `1` is the wet/dry/wet
    /// return: the chain skips it, and the return named by the split send
    /// runs it. Resolved from `params["signalBus"]` when the graph is built.
    pub signal_bus: u8,
}

/// Main insert chain. The audio callback compares this, never the param string.
pub(crate) const SIGNAL_BUS_MAIN: u8 = 0;

/// Wet/dry/wet send/return. See [`RuntimeInsert::signal_bus`].
pub(crate) const SIGNAL_BUS_WDW: u8 = 1;

pub(super) fn signal_bus_from_params(params: &HashMap<String, Value>) -> u8 {
    match params.get("signalBus").and_then(Value::as_str) {
        Some(bus) if bus.eq_ignore_ascii_case("wdw") => SIGNAL_BUS_WDW,
        _ => SIGNAL_BUS_MAIN,
    }
}

/// One multi-out route: a 1-based plugin output channel pair (`channel_l`,
/// `channel_r`) of a bridged instrument feeding a child "Out Ch" track.
#[derive(Debug, Clone)]
pub struct RuntimeVstiOutputChild {
    pub dest_track_id: String,
    /// [`Self::dest_track_id`] resolved to a track index at build time
    /// ([`RuntimeProject::resolve_indices`]); `None` when the track is missing.
    pub dest_track_index: Option<usize>,
    pub bus_index: u8,
    pub channel_count: u8,
    pub channel_l: u8,
    pub channel_r: u8,
}

pub type InsertDspState = AudioPluginDspState;

pub(super) const ENABLED_AUDIO_OUTPUT_CHANNELS_PARAM: &str = "enabledAudioOutputChannels";

/// Also bounds `scratch_multi` pre-sizing in [`RuntimeProject::resolve_bridge_sinks`]
/// and the audio-thread read in `apply_external_bridge_insert_block` — keep in sync.
pub(crate) const MAX_VSTI_OUTPUT_CHANNELS: u64 = 32;

pub(super) fn bridge_enabled_output_channels_from_params(
    params: &HashMap<String, Value>,
) -> Vec<u8> {
    let Some(values) = params
        .get(ENABLED_AUDIO_OUTPUT_CHANNELS_PARAM)
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };

    let mut channels = Vec::with_capacity(values.len().min(MAX_VSTI_OUTPUT_CHANNELS as usize));
    for value in values {
        let Some(channel) = value.as_u64() else {
            continue;
        };
        if (1..=MAX_VSTI_OUTPUT_CHANNELS).contains(&channel) {
            let channel = channel as u8;
            if !channels.contains(&channel) {
                channels.push(channel);
            }
        }
    }
    channels
}

pub(super) const VSTI_OUTPUT_CHILDREN_PARAM: &str = "vstiOutputChildren";

/// Parse the multi-out child routes from an insert's params. Each entry is
/// `{ "trackId": "<dest track>", "channelL": <1-32>, "channelR": <1-32> }`.
/// Absent/empty (the default) → no child routing, so the single-track fold path
/// is used. The snapshot emits this once the mixer creates child strips.
pub(super) fn vsti_output_children_from_params(
    params: &HashMap<String, Value>,
) -> Vec<RuntimeVstiOutputChild> {
    let Some(values) = params
        .get(VSTI_OUTPUT_CHILDREN_PARAM)
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    let mut children = Vec::with_capacity(values.len().min(MAX_VSTI_OUTPUT_CHANNELS as usize));
    for value in values {
        let Some(obj) = value.as_object() else {
            continue;
        };
        let Some(dest_track_id) = obj.get("trackId").and_then(Value::as_str) else {
            continue;
        };
        let channel_l = obj.get("channelL").and_then(Value::as_u64).unwrap_or(0);
        let channel_r = obj.get("channelR").and_then(Value::as_u64).unwrap_or(0);
        let bus_index = obj
            .get("busIndex")
            .and_then(Value::as_u64)
            .unwrap_or_else(|| channel_l.saturating_sub(1) / 2);
        let channel_count = obj
            .get("channelCount")
            .and_then(Value::as_u64)
            .unwrap_or(2)
            .clamp(1, MAX_VSTI_OUTPUT_CHANNELS);
        if !(1..=MAX_VSTI_OUTPUT_CHANNELS).contains(&channel_l)
            || !(1..=MAX_VSTI_OUTPUT_CHANNELS).contains(&channel_r)
            || bus_index > MAX_VSTI_OUTPUT_CHANNELS
        {
            continue;
        }
        children.push(RuntimeVstiOutputChild {
            dest_track_id: dest_track_id.to_string(),
            dest_track_index: None, // resolved in resolve_indices
            bus_index: bus_index as u8,
            channel_count: channel_count as u8,
            channel_l: channel_l as u8,
            channel_r: channel_r as u8,
        });
    }
    children
}

impl RuntimeInsert {
    /// Whether the insert processes this block: switched on, and not bypassed
    /// by automation.
    #[inline]
    pub fn active(&self) -> bool {
        self.enabled && !self.automation_bypassed && !self.guard.bypassed
    }
}

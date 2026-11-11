//! Realtime-safe sink the audio callback uses to exchange one block with the
//! external plugin host (Stage 3b of the shared-memory audio bridge).
//!
//! DAUx defines the contract; `sphere-plugin-host` implements it over the
//! shared-memory region (it cannot live there — the engine must not depend on
//! the host crate). **Every method runs on the audio thread**, so each must be
//! wait-free: no allocation, no locks, no syscalls, no blocking.

use std::sync::Arc;

/// Largest block, in frames, that one bridge exchange can carry.
///
/// Part of the contract rather than the implementation: the shared-memory
/// region is sized from it, and [`PluginBridgeSink::request_block`] clamps to
/// it, so a callback block larger than this loses its tail — bridged plugin
/// audio would be silently truncated. Backends must therefore keep the device
/// buffer they negotiate at or below this, since a callback can be handed the
/// device's entire ring in one go.
///
/// `sphere-plugin-host` asserts its own `MAX_BLOCK_FRAMES` matches this.
pub const MAX_BRIDGE_BLOCK_FRAMES: usize = 2048;

/// Latency a bridged insert adds to its track: what the plug-in reports plus
/// the bridge's one-block handshake.
///
/// The engine hands the host a block and plays back the block the host
/// finished one exchange earlier, so the handshake costs one period: the
/// block size the open stream steadily delivers — the size it reported until
/// the device shows another (see
/// [`crate::engine::SharedState::bridge_period_frames_for_block`]). It is not
/// the frame count of the callback being rendered. Variable-size callbacks
/// (WASAPI shared/event mode, variable-period drivers, partial renders) would
/// otherwise turn every block-size change into an apparent plug-in latency
/// change and replan compensation on the audio thread, moving every dry
/// track's delay with the block size.
///
/// Delay compensation and the user-facing latency report both go through
/// this, so the two cannot disagree.
#[inline]
pub fn bridged_insert_latency_samples(reported_samples: u32, bridge_period_frames: u32) -> u32 {
    reported_samples.saturating_add(bridge_period_frames)
}

/// How long a sink may wait for the host's block while Audioguard renders
/// ahead, nanoseconds; `0` outside a render-ahead.
///
/// The usual handoff grace is a fraction of a period, because it is spent on
/// the device callback. A block rendered ahead has the ring's headroom
/// instead, and the guard renders several blocks back to back while it fills,
/// so the host needs up to a period to answer each one. Process-wide rather
/// than per thread because the chain's parallel workers read it too; the
/// render it belongs to holds the graph, so no other render overlaps it.
static RENDER_AHEAD_GRACE_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Set (or, with zero, clear) the render-ahead grace. Called by the guard
/// thread around each block it renders.
pub fn set_render_ahead_grace(grace: std::time::Duration) {
    let nanos = grace.as_nanos().min(20_000_000) as u64;
    RENDER_AHEAD_GRACE_NS.store(nanos, std::sync::atomic::Ordering::Relaxed);
}

/// The grace a sink may use for the block being rendered now, if it is being
/// rendered ahead. One relaxed load.
#[inline]
pub fn render_ahead_grace() -> Option<std::time::Duration> {
    match RENDER_AHEAD_GRACE_NS.load(std::sync::atomic::Ordering::Relaxed) {
        0 => None,
        nanos => Some(std::time::Duration::from_nanos(nanos)),
    }
}

/// One-block exchange with the external plugin host, called from the audio
/// callback. The engine reads the host's previously produced block (one-block
/// latency) and requests the next one — it never spins waiting for the host.
pub trait PluginBridgeSink: Send + Sync + std::fmt::Debug {
    /// True when the host signals it is producing DSP output.
    fn dsp_ready(&self) -> bool;

    /// Actual main output channel count reported by the bridged plugin. This is
    /// routing metadata; the current audio sample exchange remains stereo until
    /// the multichannel buffer data path is enabled.
    fn plugin_output_channels(&self) -> u32 {
        2
    }

    /// True while the host still owns the input buffer for a previously
    /// published request. The engine must not overwrite or republish that single
    /// buffer until the request completes. Default sinks have no asynchronous
    /// owner and therefore report no in-flight request.
    fn request_in_flight(&self) -> bool {
        false
    }

    /// Read the host's most-recently produced block (deinterleaved) into
    /// `out_l` / `out_r` (each at least `frames` long). Returns the number of
    /// frames actually read. Each produced block is handed out **at most
    /// once**: 0 means the host has not produced a new block since the last
    /// read (stalled on an editor open/close, plugin load, or not started) —
    /// the caller must bypass or output silence for this block and must never
    /// reuse previous output. Wait-free.
    fn read_output(&self, out_l: &mut [f32], out_r: &mut [f32], frames: usize) -> usize;

    /// Read the same fresh block as [`Self::read_output`], but only fold the
    /// 1-based plugin output channels listed in `enabled_channels` into the
    /// engine's stereo track. Empty selection means "all reported channels" for
    /// shared-region sinks, so unsynced multi-out instruments stay audible.
    /// Default sinks ignore the selection and preserve the legacy stereo
    /// contract.
    fn read_output_for_channels(
        &self,
        out_l: &mut [f32],
        out_r: &mut [f32],
        frames: usize,
        _enabled_channels: &[u8],
    ) -> usize {
        self.read_output(out_l, out_r, frames)
    }

    /// Peak for a 1-based plugin output channel from the last fresh block read
    /// by the engine. This is meter-only metadata; reading it must be wait-free.
    fn output_channel_peak(&self, _channel: u8) -> f32 {
        0.0
    }

    /// Multi-out (Slice 1): read the fresh block as raw interleaved samples
    /// (`out_interleaved` laid out `[frame0_ch0, frame0_ch1, …]`). Returns
    /// `(frames_written, channels)`. Consumes the freshness guard exactly like
    /// [`Self::read_output`] — the caller reads ONCE per block, then folds the
    /// main pair and scatters the child pairs from the same buffer (a second
    /// sink read would see the guard return 0). Default sinks have no
    /// multichannel layout, so they report 0 frames / 0 channels.
    fn read_output_multichannel(
        &self,
        _out_interleaved: &mut [f32],
        _frames: usize,
    ) -> (usize, usize) {
        (0, 0)
    }

    /// Push one MIDI event for the host to apply to the next block (Stage 4 clip
    /// playback / automation). Wait-free ring push; dropped if the ring is full.
    fn push_midi(&self, status: u8, data1: u8, data2: u8, sample_offset: u32);

    /// Push one parameter change (normalized `0.0..=1.0` VST3 ParamID `value`)
    /// for the host to apply to the next block. Wait-free ring push; dropped if
    /// the ring is full. Default no-op for sinks that do not carry a param ring
    /// (test stubs); the shared-region sink forwards it to the host plugin.
    fn push_param(&self, _param_id: u32, _value: f32, _sample_offset: u32) {}

    /// Write the track's pre-plugin stereo input for effect inserts (engine →
    /// host `audio_in`). Must be called only after a successful
    /// [`Self::read_output`] for the previous cycle (or when no prior request is
    /// outstanding) so the host is not still copying the last block. Wait-free
    /// raw buffer copy.
    fn write_input(&self, in_l: &[f32], in_r: &[f32], frames: usize);

    /// Publish the request for the host to process `frames` next (sets the block
    /// size and bumps the request sequence). Wait-free.
    fn request_block(&self, frames: u32);

    /// The bridged plugin's reported processing latency in samples, as published
    /// by the host (0 if unknown / not yet reported). Does NOT include the
    /// one-block bridge handshake latency — callers add that separately. Default
    /// 0 for sinks without a host (test stubs).
    fn reported_latency_samples(&self) -> u32 {
        0
    }

    /// The bridged plug-in's own DSP cost, as a share of one block, or `None`
    /// when the host has not measured one yet.
    ///
    /// Measured in the host process around the plug-in's `process()`, which is
    /// the only place its real cost is visible: from the engine's side a bridged
    /// insert is a shared-memory read, and timing that would report the cost of
    /// the bridge rather than of the plug-in. Default `None` for sinks without a
    /// host (test stubs).
    fn reported_process_load(&self) -> Option<f32> {
        None
    }

    /// Publish the transport ProcessContext (tempo, time signature, project
    /// position, playing/recording) for the next block, so the host fills the
    /// bridged plugin's VST3 `ProcessContext` with real transport instead of a
    /// hardcoded stub. Called once per block right before [`Self::request_block`].
    /// Wait-free (plain atomic stores). Default no-op for sinks that do not
    /// carry transport (e.g. test stubs).
    fn set_transport(&self, _ctx: &crate::vst3_processor::RuntimeTransportContext) {}
}

/// Shared handle to a realtime plugin-bridge sink.
pub type SharedPluginBridgeSink = Arc<dyn PluginBridgeSink>;
pub type PluginBridgeSinkMap = std::collections::HashMap<String, SharedPluginBridgeSink>;

//! The offline render loop: builds a `RuntimeProject` from a snapshot and drives
//! the shared render kernel block by block with no audio device.

use std::collections::HashMap;
use std::sync::Arc;

use crate::audio_source::ClipAudioSource;
use crate::engine::{render_project_block_interleaved_with_taps, schedule_midi_render_block};
use crate::latency_graph::RuntimeLatencyGraph;
use crate::plugin_bridge::{PluginBridgeSink, PluginBridgeSinkMap};
use crate::runtime::{clip_dsp_debug_enabled, describe_clip_dsp_state, RuntimeProject};
use crate::types::EngineProjectSnapshot;

use super::render_progress::{ExportCancelToken, ExportProgress, ExportStage};
use super::render_request::{ExportTailMode, OfflineRenderRequest};
use super::ExportError;

/// Per-block callback receiving each rendered mixer-channel tap (stereo, in
/// runtime-track order) and the frame count valid this block.
type TrackBlockCallback<'a> = &'a mut dyn FnMut(&[Vec<f32>], usize) -> Result<(), ExportError>;

/// Result of an offline render pass.
#[derive(Debug, Clone)]
pub struct OfflineRenderSummary {
    pub frames_rendered: u64,
    /// Absolute peak sample magnitude observed across the render.
    pub peak: f32,
}

/// Export-only adapter: unlike the realtime callback, the worker may wait for
/// the external host to finish the requested block. This prevents a faster-than-
/// realtime bounce from outrunning the shared-memory producer and writing zeros.
///
/// The wait is cancellation-aware: cancelling the export aborts a pending wait
/// immediately (the read returns 0 and the render loop's cancel check exits)
/// instead of stalling until the per-read deadline when a host is wedged.
#[derive(Debug)]
struct OfflineBridgeSink {
    live: crate::plugin_bridge::SharedPluginBridgeSink,
    cancel: ExportCancelToken,
}

impl OfflineBridgeSink {
    fn wait_step(&self, deadline: std::time::Instant) -> bool {
        if self.cancel.is_cancelled() || std::time::Instant::now() >= deadline {
            false
        } else {
            std::thread::sleep(std::time::Duration::from_micros(100));
            true
        }
    }
}

impl PluginBridgeSink for OfflineBridgeSink {
    fn dsp_ready(&self) -> bool {
        self.live.dsp_ready()
    }
    fn plugin_output_channels(&self) -> u32 {
        self.live.plugin_output_channels()
    }
    fn request_in_flight(&self) -> bool {
        self.live.request_in_flight()
    }
    fn read_output(&self, l: &mut [f32], r: &mut [f32], frames: usize) -> usize {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let got = self.live.read_output(l, r, frames);
            // Require a full block. A short read would wet|dry-splice in
            // `apply_bridge_insert_output` and click in the bounce file.
            if got >= frames {
                return frames;
            }
            if got > 0 {
                // Freshness already consumed; refuse the partial rather than
                // waiting forever for a second copy that will never arrive.
                return 0;
            }
            if !self.wait_step(deadline) {
                return 0;
            }
        }
    }
    fn read_output_for_channels(
        &self,
        l: &mut [f32],
        r: &mut [f32],
        frames: usize,
        enabled: &[u8],
    ) -> usize {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let got = self.live.read_output_for_channels(l, r, frames, enabled);
            if got >= frames {
                return frames;
            }
            if got > 0 {
                return 0;
            }
            if !self.wait_step(deadline) {
                return 0;
            }
        }
    }
    fn output_channel_peak(&self, channel: u8) -> f32 {
        self.live.output_channel_peak(channel)
    }
    fn read_output_multichannel(&self, out: &mut [f32], frames: usize) -> (usize, usize) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let (got, channels) = self.live.read_output_multichannel(out, frames);
            if got >= frames {
                return (frames, channels);
            }
            if got > 0 {
                return (0, 0);
            }
            if !self.wait_step(deadline) {
                return (0, 0);
            }
        }
    }
    fn push_midi(&self, status: u8, data1: u8, data2: u8, offset: u32) {
        self.live.push_midi(status, data1, data2, offset);
    }
    fn push_param(&self, id: u32, value: f32, offset: u32) {
        self.live.push_param(id, value, offset);
    }
    fn write_input(&self, l: &[f32], r: &[f32], frames: usize) {
        self.live.write_input(l, r, frames);
    }
    fn request_block(&self, frames: u32) {
        self.live.request_block(frames);
    }
    fn reported_latency_samples(&self) -> u32 {
        self.live.reported_latency_samples()
    }
    fn set_transport(&self, ctx: &crate::vst3_processor::RuntimeTransportContext) {
        self.live.set_transport(ctx);
    }
}

/// Render the arrangement offline.
///
/// `on_block` receives interleaved frames at `request.channels`; returning an
/// error from it aborts the render (e.g. an encoder write failure). `on_gain`
/// is applied to every sample after the kernel render — use it to inject a
/// normalization gain on a second pass (pass `1.0` for the analysis pass).
///
/// This holds no GPUI/UI state: the snapshot is plain data and the loop runs on
/// whatever thread the caller chooses (a background worker, never the UI thread).
pub fn render_offline(
    snapshot: &EngineProjectSnapshot,
    request: &OfflineRenderRequest,
    cancel: &ExportCancelToken,
    gain: f32,
    on_block: impl FnMut(&[f32]) -> Result<(), ExportError>,
    on_progress: impl FnMut(ExportProgress),
) -> Result<OfflineRenderSummary, ExportError> {
    render_offline_with_bridges(snapshot, request, cancel, gain, None, on_block, on_progress)
}

pub fn render_offline_with_bridges(
    snapshot: &EngineProjectSnapshot,
    request: &OfflineRenderRequest,
    cancel: &ExportCancelToken,
    gain: f32,
    bridge_sinks: Option<&PluginBridgeSinkMap>,
    mut on_block: impl FnMut(&[f32]) -> Result<(), ExportError>,
    mut on_progress: impl FnMut(ExportProgress),
) -> Result<OfflineRenderSummary, ExportError> {
    render_offline_impl(
        snapshot,
        request,
        cancel,
        gain,
        bridge_sinks,
        false,
        &mut on_block,
        &mut |_taps, _frames| Ok(()),
        &mut on_progress,
    )
}

/// Render every mixer-channel tap alongside the master in one runtime graph
/// pass. Taps are stereo, post-insert/post-fader, and ordered like
/// `snapshot.tracks`; the slices are reused on the next callback.
pub fn render_offline_tracks(
    snapshot: &EngineProjectSnapshot,
    request: &OfflineRenderRequest,
    cancel: &ExportCancelToken,
    on_track_block: impl FnMut(&[Vec<f32>], usize) -> Result<(), ExportError>,
    on_progress: impl FnMut(ExportProgress),
) -> Result<OfflineRenderSummary, ExportError> {
    render_offline_tracks_with_bridges(snapshot, request, cancel, None, on_track_block, on_progress)
}

pub fn render_offline_tracks_with_bridges(
    snapshot: &EngineProjectSnapshot,
    request: &OfflineRenderRequest,
    cancel: &ExportCancelToken,
    bridge_sinks: Option<&PluginBridgeSinkMap>,
    mut on_track_block: impl FnMut(&[Vec<f32>], usize) -> Result<(), ExportError>,
    mut on_progress: impl FnMut(ExportProgress),
) -> Result<OfflineRenderSummary, ExportError> {
    render_offline_impl(
        snapshot,
        request,
        cancel,
        1.0,
        bridge_sinks,
        true,
        &mut |_block| Ok(()),
        &mut on_track_block,
        &mut on_progress,
    )
}

/// Render the master and, when `capture_tracks`, every mixer-channel tap in
/// one graph pass — the mixdown and the stems of one job from the same
/// render. `gain` applies to the master only.
#[allow(clippy::too_many_arguments)]
pub fn render_offline_mix_and_tracks_with_bridges(
    snapshot: &EngineProjectSnapshot,
    request: &OfflineRenderRequest,
    cancel: &ExportCancelToken,
    gain: f32,
    bridge_sinks: Option<&PluginBridgeSinkMap>,
    capture_tracks: bool,
    on_block: &mut dyn FnMut(&[f32]) -> Result<(), ExportError>,
    on_track_block: TrackBlockCallback,
    on_progress: &mut dyn FnMut(ExportProgress),
) -> Result<OfflineRenderSummary, ExportError> {
    render_offline_impl(
        snapshot,
        request,
        cancel,
        gain,
        bridge_sinks,
        capture_tracks,
        on_block,
        on_track_block,
        on_progress,
    )
}

// Shared implementation behind the four public render entry points; the
// parameter set IS the render contract (source, range, bridge handles, and the
// master/tap/progress callbacks), so grouping them into a struct would only
// move the same nine names one level down.
#[allow(clippy::too_many_arguments)]
fn render_offline_impl(
    snapshot: &EngineProjectSnapshot,
    request: &OfflineRenderRequest,
    cancel: &ExportCancelToken,
    gain: f32,
    bridge_sinks: Option<&PluginBridgeSinkMap>,
    capture_tracks: bool,
    on_block: &mut dyn FnMut(&[f32]) -> Result<(), ExportError>,
    on_track_block: TrackBlockCallback,
    on_progress: &mut dyn FnMut(ExportProgress),
) -> Result<OfflineRenderSummary, ExportError> {
    request.validate().map_err(ExportError::Settings)?;

    on_progress(ExportProgress::stage_only(
        ExportStage::Preparing,
        request.content_frames(),
    ));

    // Build the runtime graph from the snapshot. Fresh audio-source cache and no
    // VST3 reuse — this is an isolated offline graph. PDC is taken from the
    // snapshot (stamped from the live engine), NOT hardcoded, so the offline
    // graph applies the *same* plugin-delay compensation as realtime playback.
    let mut audio_cache: HashMap<String, Arc<ClipAudioSource>> = HashMap::new();
    let mut runtime = RuntimeProject::build(
        snapshot,
        request.sample_rate,
        &mut audio_cache,
        None,
        snapshot.pdc_enabled,
    )
    .map_err(|e| ExportError::Build(e.to_string()))?;
    runtime.sample_rate = request.sample_rate;
    if let Some(sinks) = bridge_sinks {
        runtime.plugin_bridge_sinks = sinks
            .iter()
            .map(|(id, sink)| {
                (
                    id.clone(),
                    Arc::new(OfflineBridgeSink {
                        live: sink.clone(),
                        cancel: cancel.clone(),
                    }) as crate::plugin_bridge::SharedPluginBridgeSink,
                )
            })
            .collect();
        runtime.resolve_bridge_sinks();
        // Prime the bridge's one-block pipeline with silence so the host does
        // not process leftover realtime `audio_in`. The first rendered block
        // consumes this silent response and requests the first timeline block,
        // matching realtime's one-block bridge latency (covered by PDC).
        let prime_frames = request.block_size.max(1);
        let zeros = vec![0.0f32; prime_frames];
        for sink in runtime.plugin_bridge_sinks.values() {
            sink.write_input(&zeros, &zeros, prime_frames);
            sink.request_block(prime_frames as u32);
        }
    }
    // Deterministic bounce: apply the exact constant per-block fader gain, never
    // the realtime anti-zipper ramp (which is for live fader drags).
    runtime.fader_smoothing = false;
    // Restore each in-process VST3 insert's saved state before any process() call
    // so instruments/effects render with the user's current tweaks. Runs once on
    // this worker thread, never the audio callback.
    restore_offline_plugin_states(snapshot, &mut runtime);

    let channels = request.channels.max(1) as usize;
    let block = request.block_size.max(1);
    let ts_num = snapshot.time_signature[0].max(1);
    let ts_den = snapshot.time_signature[1].max(1);

    // Recompute the latency graph now that saved plugin state is restored: a
    // plugin may report a different latency after `setState` (e.g. a linear-phase
    // / oversampling mode toggled on). The realtime callback refreshes per block;
    // we refresh once up front so the warmup below reflects the real latency.
    runtime.refresh_runtime_latency_graph(block as u32);

    // Pre-roll / warmup: when Global Latency Sync (PDC) is on, the realtime graph
    // delays every path to the master bus by `max_path_latency` (+ master insert
    // latency). Render that many frames FIRST and discard them so the written
    // file starts with fully-settled bar-1 content — PDC delay lines primed and
    // plugin latency flushed — instead of a latency ramp. This mirrors playback
    // exactly (same graph, same per-track delays); it only strips the constant
    // output latency so the bounce stays sample-aligned to the timeline.
    let warmup_frames = export_warmup_frames(&runtime.latency_graph, runtime.pdc_enabled);

    runtime.reset_midi_playback(request.start_sample);

    if export_latency_debug_enabled() {
        let lg = &runtime.latency_graph;
        eprintln!(
            "[export-latency] export_latency_sync_enabled={} graph_max_latency_samples={} \
             master_insert_latency_samples={} export_preroll_samples={} export_tail_samples={} \
             export_graph_version={} sample_rate={} block_size={}",
            runtime.pdc_enabled,
            lg.max_path_latency_samples,
            lg.master_plugin_latency,
            warmup_frames,
            request.max_tail_frames(),
            snapshot.latency_graph_version,
            request.sample_rate,
            block,
        );
        for (idx, track) in runtime.tracks.iter().enumerate() {
            eprintln!(
                "[export-latency] track={} plugin_reported_latency_samples={} \
                 track_total_latency_samples={} pdc_delay_samples={}",
                track.id,
                lg.track_plugin_latency.get(idx).copied().unwrap_or(0),
                lg.track_output_latency.get(idx).copied().unwrap_or(0),
                lg.track_pdc_delay.get(idx).copied().unwrap_or(0),
            );
        }
        if snapshot.latency_graph_version == 0 {
            eprintln!(
                "[export-latency] WARNING: snapshot graph version is unstamped (0); export may \
                 not match the live realtime graph version"
            );
        }
    }

    if clip_dsp_debug_enabled() {
        for clip in &snapshot.clips {
            if let Some(process) = &clip.audio_process {
                eprintln!(
                    "[clip-dsp][export-start] {}",
                    describe_clip_dsp_state(clip, process, snapshot.bpm)
                );
            }
        }
    }

    // The kernel renders stereo (channels >= 2). We always render into a 2-ch
    // scratch and fold down to mono on output when requested.
    let mut stereo = vec![0.0f32; block * 2];
    let mut out = vec![0.0f32; block * channels];
    let mut track_taps = if capture_tracks {
        vec![Vec::new(); runtime.tracks.len()]
    } else {
        Vec::new()
    };

    let content_frames = request.content_frames();
    let tail_cap = request.max_tail_frames();
    let total_with_tail = content_frames.saturating_add(tail_cap);

    // Phase boundaries by `produced` (total frames produced, incl. warmup):
    //   [0, write_start)        warmup / pre-roll → rendered then DISCARDED
    //   [write_start, content_end)  content        → written
    //   [content_end, produce_cap)  tail           → written (rings out)
    let write_start = warmup_frames;
    let content_end = warmup_frames.saturating_add(content_frames);
    let produce_cap = content_end.saturating_add(tail_cap);

    let mut pos = request.start_sample;
    let mut produced = 0u64; // total frames produced, including discarded warmup
    let mut written = 0u64; // frames actually emitted (content + tail)
    let mut peak = 0.0f32;
    let mut progress_throttle = 0u32;

    let until_silence = matches!(request.tail, ExportTailMode::UntilSilence { .. });
    let silence_threshold = match request.tail {
        ExportTailMode::UntilSilence { threshold_db, .. } => {
            10f32.powf(threshold_db / 20.0).clamp(0.0, 1.0)
        }
        _ => 0.0,
    };

    loop {
        if cancel.is_cancelled() {
            return Err(ExportError::Cancelled);
        }

        // Past content: stop now if there is no tail, else stop at the tail cap.
        if produced >= content_end {
            match request.tail {
                ExportTailMode::None => break,
                _ => {
                    if produced >= produce_cap {
                        break;
                    }
                }
            }
        }

        // Always drive the kernel + plugin bridge at the negotiated full block
        // size. Shrinking the last warmup/content/tail block used to change
        // `block_frames` on the shared-memory handshake; the host then wrote a
        // short wet block into `audio_out` without clearing the rest, and the
        // next full-size read pulled a stale wet tail — audible as stutter /
        // overlapping doubles in the bounce (and only on the export path).
        let this_block = block;
        let write_limit = match request.tail {
            ExportTailMode::None => content_end,
            _ => produce_cap,
        };

        let stereo_slice = &mut stereo[..this_block * 2];
        for s in stereo_slice.iter_mut() {
            *s = 0.0;
        }

        // Schedule MIDI for this block, then render the kernel block. Mirrors the
        // realtime callback order in `fill_output_f32_inner`.
        let _ = schedule_midi_render_block(&mut runtime, pos, this_block as u64, None);
        let frames = render_project_block_interleaved_with_taps(
            &mut runtime,
            pos,
            request.master_volume,
            stereo_slice,
            2,
            true,
            ts_num,
            ts_den,
            None,
            capture_tracks.then_some(track_taps.as_mut_slice()),
        );
        // Match the realtime path: per-block MIDI events are consumed once,
        // and so are the Solfege pitch and articulation lists. An export that
        // let those accumulate re-applied every earlier pitch change on every
        // block, so a bounced take drifted away from what the same project
        // played live.
        for track in &mut runtime.tracks {
            track.midi_block_events.clear();
            track.solfege_pitch_events.clear();
            track.solfege_articulation_events.clear();
        }

        let frames = frames as usize;
        if frames == 0 {
            // Defensive: avoid an infinite loop if the kernel returns nothing.
            break;
        }
        let frames_u64 = frames as u64;
        let block_end = produced.saturating_add(frames_u64);
        let mut stop_for_silence = false;

        // Emit only the portion that lands in [write_start, write_limit). Full
        // blocks may straddle warmup→content or content→tail; we slice here
        // instead of shrinking the DSP/bridge exchange.
        let emit_from_abs = produced.max(write_start);
        let emit_to_abs = block_end.min(write_limit);
        if emit_to_abs > emit_from_abs {
            let emit_from = (emit_from_abs - produced) as usize;
            let emit_to = (emit_to_abs - produced) as usize;
            let emit_len = emit_to.saturating_sub(emit_from);
            let mut block_peak = 0.0f32;
            let out_slice = &mut out[..emit_len * channels];
            for (out_i, f) in (emit_from..emit_to).enumerate() {
                let l = stereo_slice[f * 2] * gain;
                let r = stereo_slice[f * 2 + 1] * gain;
                block_peak = block_peak.max(l.abs()).max(r.abs());
                if channels == 1 {
                    out_slice[out_i] = (l + r) * 0.5;
                } else {
                    out_slice[out_i * channels] = l;
                    out_slice[out_i * channels + 1] = r;
                    for c in 2..channels {
                        out_slice[out_i * channels + c] = 0.0;
                    }
                }
            }
            peak = peak.max(block_peak);
            on_block(out_slice)?;
            if capture_tracks {
                if emit_from > 0 {
                    for tap in &mut track_taps {
                        let src = emit_from * 2;
                        let len = emit_len * 2;
                        if tap.len() >= src + len {
                            tap.copy_within(src..src + len, 0);
                        }
                    }
                }
                on_track_block(&track_taps, emit_len)?;
            }
            written = written.saturating_add(emit_len as u64);

            // UntilSilence: stop once content is done and the emitted peak has
            // decayed below threshold.
            if produced >= content_end && until_silence && block_peak < silence_threshold {
                stop_for_silence = true;
            }
        }

        produced = produced.saturating_add(frames_u64);
        pos = pos.saturating_add(frames_u64);
        if stop_for_silence {
            break;
        }

        // Throttle progress callbacks (~ every 16 blocks) to avoid flooding.
        progress_throttle = progress_throttle.wrapping_add(1);
        if progress_throttle.is_multiple_of(16) {
            on_progress(ExportProgress::new(
                ExportStage::Rendering,
                written.min(total_with_tail.max(1)),
                total_with_tail.max(1),
            ));
        }
    }

    if cancel.is_cancelled() {
        return Err(ExportError::Cancelled);
    }

    Ok(OfflineRenderSummary {
        frames_rendered: written,
        peak,
    })
}

/// Pre-roll / warmup frames the offline render must produce-and-discard so the
/// written file starts with fully-settled content rather than the latency ramp.
///
/// Equals the total constant latency the engine introduces from clip read to the
/// final master output when Global Latency Sync (PDC) is active:
/// `max_path_latency` (longest path to the master summing bus, which the PDC
/// delay lines align every other path to) + `master_plugin_latency` (master-bus
/// insert latency added after summing). When PDC is off the export reproduces
/// playback's uncompensated graph exactly, so no frames are stripped.
#[inline]
pub(crate) fn export_warmup_frames(latency_graph: &RuntimeLatencyGraph, pdc_enabled: bool) -> u64 {
    if !pdc_enabled {
        return 0;
    }
    latency_graph
        .max_path_latency_samples
        .saturating_add(latency_graph.master_plugin_latency) as u64
}

#[inline]
fn export_latency_debug_enabled() -> bool {
    std::env::var("FUTUREBOARD_PDC_DEBUG")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

#[inline]
fn plugin_state_debug_enabled() -> bool {
    std::env::var("FUTUREBOARD_PLUGIN_STATE_DEBUG")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Restore saved VST3 state into the freshly-built in-process processors.
///
/// Matches snapshot inserts to runtime inserts by track id + insert id, decodes
/// the packed `Vst3PluginState` blob carried by the offline-export snapshot, and
/// applies it. Inserts that failed to instantiate (`vst3 == None`, e.g. a plugin
/// that refuses a second instance) are skipped — the render still completes; the
/// kernel bypasses them (instrument → silence, effect → dry). Bad/empty blobs are
/// ignored. Control-thread only.
fn restore_offline_plugin_states(snapshot: &EngineProjectSnapshot, runtime: &mut RuntimeProject) {
    use crate::vst3_processor::Vst3PluginState;

    let debug = plugin_state_debug_enabled();
    for snap_track in &snapshot.tracks {
        let Some(rt_track) = runtime.tracks.iter_mut().find(|t| t.id == snap_track.id) else {
            continue;
        };
        for snap_insert in &snap_track.inserts {
            let Some(bytes) = snap_insert.state.as_ref() else {
                continue;
            };
            let Some(rt_insert) = rt_track.inserts.iter().find(|i| i.id == snap_insert.id) else {
                continue;
            };
            let Some(vst3) = rt_insert.vst3.as_ref() else {
                if debug {
                    eprintln!(
                        "[export-state] skip insert={} track={} reason=not_instantiated",
                        snap_insert.id, snap_track.id
                    );
                }
                continue;
            };
            match Vst3PluginState::from_packed_bytes(bytes) {
                Some(state) => {
                    let ok = vst3.set_state(&state);
                    if debug {
                        eprintln!(
                            "[export-state] restore insert={} track={} bytes={} ok={}",
                            snap_insert.id,
                            snap_track.id,
                            bytes.len(),
                            ok
                        );
                    }
                }
                None if debug => {
                    eprintln!(
                        "[export-state] skip insert={} track={} reason=unparseable_blob bytes={}",
                        snap_insert.id,
                        snap_track.id,
                        bytes.len()
                    );
                }
                None => {}
            }
        }
    }
}

/// Test-support: a plain unrouted audio track snapshot with unity fader.
#[cfg(test)]
pub(crate) fn make_track_snapshot(id: &str) -> crate::types::EngineTrackSnapshot {
    crate::types::EngineTrackSnapshot {
        midi_programs: Vec::new(),
        id: id.to_string(),
        track_type: "audio".to_string(),
        volume: 1.0,
        pan: 0.0,
        muted: false,
        solo: false,
        armed: false,
        input_monitor: false,
        input_source: Default::default(),
        preview_mode: "stereo".to_string(),
        output_track_id: None,
        inserts: Vec::new(),
        sends: Vec::new(),
        automation_lanes: Vec::new(),
        builtin_soundfont_player: false,
        soundfont_path: None,
        soundfont_preset_bank: None,
        soundfont_preset_patch: None,
        soundfont_volume: 1.0,
        soundfont_reverb_chorus: true,
        soundfont_polyphony: 64,
        soundfont_envelope: Default::default(),
        soundfont_quality: Default::default(),
        soundfont_mode: Default::default(),
        soundfont_channels: Default::default(),
        solfege_engine: None,
    }
}

#[cfg(test)]
pub(crate) fn silence_snapshot(sample_rate: u32) -> EngineProjectSnapshot {
    use crate::types::EngineRoutingSnapshot;
    EngineProjectSnapshot {
        spatial: Default::default(),
        project_id: "test".to_string(),
        project_root: None,
        preferred_input_device: None,
        bpm: 120.0,
        tempo_points: Vec::new(),
        time_signature: [4, 4],
        sample_rate,
        tracks: vec![make_track_snapshot("track-1")],
        clips: Vec::new(),
        midi_clips: Vec::new(),
        pdc_enabled: true,
        latency_graph_version: 1,
        routing: EngineRoutingSnapshot {
            master_output_device: None,
            sample_rate,
            buffer_size: 512,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::render_request::{ExportNormalizeMode, ExportTailMode};

    fn request(start: u64, end: u64) -> OfflineRenderRequest {
        OfflineRenderRequest {
            sample_rate: 48_000,
            channels: 2,
            start_sample: start,
            end_sample: end,
            master_volume: 1.0,
            block_size: 256,
            tail: ExportTailMode::None,
            normalize: ExportNormalizeMode::None,
        }
    }

    #[test]
    fn warmup_is_max_path_plus_master_when_pdc_on() {
        let lg = RuntimeLatencyGraph {
            max_path_latency_samples: 512,
            master_plugin_latency: 128,
            ..Default::default()
        };
        // PDC on → strip the full constant latency (path + master insert).
        assert_eq!(export_warmup_frames(&lg, true), 640);
        // PDC off → reproduce playback's uncompensated graph, strip nothing.
        assert_eq!(export_warmup_frames(&lg, false), 0);
    }

    #[test]
    fn warmup_is_zero_without_latency() {
        let lg = RuntimeLatencyGraph::default();
        assert_eq!(export_warmup_frames(&lg, true), 0);
        assert_eq!(export_warmup_frames(&lg, false), 0);
    }

    #[test]
    fn emit_slice_math_keeps_full_bridge_block_across_warmup() {
        // Warmup of 100 with block 256: first exchange stays 256 frames; only
        // the emitted file slice is trimmed. Shrinking the exchange used to
        // leave stale wet in shared memory for the next full-size read.
        let write_start = 100u64;
        let produced = 0u64;
        let frames = 256u64;
        let block_end = produced + frames;
        let emit_from = produced.max(write_start);
        let emit_to = block_end;
        assert_eq!(emit_to - emit_from, 156);
        assert_eq!(frames, 256, "bridge block size must stay full");
    }

    #[test]
    fn export_snapshot_pdc_flag_reaches_runtime_graph() {
        // The offline build must honor the snapshot's `pdc_enabled` (stamped from
        // the live engine), not a hardcoded value: this is what kept export from
        // matching playback. Verify both states build a graph with the flag.
        use std::collections::HashMap;
        for pdc in [true, false] {
            let mut snapshot = silence_snapshot(48_000);
            snapshot.pdc_enabled = pdc;
            let mut cache = HashMap::new();
            let runtime =
                RuntimeProject::build(&snapshot, 48_000, &mut cache, None, snapshot.pdc_enabled)
                    .expect("build");
            assert_eq!(
                runtime.pdc_enabled, pdc,
                "runtime graph must adopt the snapshot's PDC state"
            );
        }
    }

    #[test]
    fn empty_project_renders_exact_frame_count_of_silence() {
        let snapshot = silence_snapshot(48_000);
        let req = request(0, 1000);
        let cancel = ExportCancelToken::new();
        let mut total = 0u64;
        let summary = render_offline(
            &snapshot,
            &req,
            &cancel,
            1.0,
            |block| {
                total += (block.len() / 2) as u64;
                Ok(())
            },
            |_p| {},
        )
        .unwrap();
        assert_eq!(summary.frames_rendered, 1000);
        assert_eq!(total, 1000);
        assert_eq!(summary.peak, 0.0);
    }

    #[test]
    fn pre_cancelled_render_returns_cancelled() {
        let snapshot = silence_snapshot(48_000);
        let req = request(0, 100_000);
        let cancel = ExportCancelToken::new();
        cancel.cancel();
        let result = render_offline(&snapshot, &req, &cancel, 1.0, |_b| Ok(()), |_p| {});
        assert!(matches!(result, Err(ExportError::Cancelled)));
    }

    #[test]
    fn invalid_range_is_rejected() {
        let snapshot = silence_snapshot(48_000);
        let req = request(500, 500);
        let cancel = ExportCancelToken::new();
        let result = render_offline(&snapshot, &req, &cancel, 1.0, |_b| Ok(()), |_p| {});
        assert!(matches!(result, Err(ExportError::Settings(_))));
    }

    #[test]
    fn restore_offline_plugin_states_skips_uninstantiated_insert() {
        use crate::types::EngineInsertSnapshot;
        use std::collections::HashMap;

        let mut snapshot = silence_snapshot(48_000);
        // native-plugin insert with no module path → no in-process processor is
        // created (from_params returns None before any FFI). It still carries a
        // garbage state blob; restore must skip it without panicking.
        let mut params: HashMap<String, serde_json::Value> = HashMap::new();
        params.insert("format".to_string(), serde_json::json!("VST3"));
        snapshot.tracks[0].inserts.push(EngineInsertSnapshot {
            id: "insert-1".to_string(),
            kind: "native-plugin".to_string(),
            enabled: true,
            params,
            state: Some(vec![0xde, 0xad, 0xbe, 0xef]),
        });

        let mut cache = HashMap::new();
        let mut runtime =
            RuntimeProject::build(&snapshot, 48_000, &mut cache, None, false).expect("build");
        assert!(
            runtime.tracks[0].inserts[0].vst3.is_none(),
            "insert without a module path must not instantiate a processor"
        );
        // Must not panic on a missing processor / unparseable blob.
        restore_offline_plugin_states(&snapshot, &mut runtime);
    }

    #[test]
    fn mono_output_folds_to_single_channel() {
        let snapshot = silence_snapshot(48_000);
        let mut req = request(0, 480);
        req.channels = 1;
        let cancel = ExportCancelToken::new();
        let mut samples = 0u64;
        render_offline(
            &snapshot,
            &req,
            &cancel,
            1.0,
            |block| {
                samples += block.len() as u64;
                Ok(())
            },
            |_p| {},
        )
        .unwrap();
        assert_eq!(samples, 480);
    }
}

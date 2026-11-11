//! Track mixing: mute and solo, faders, meters, sends, spatial and main-output routing.

use super::*;

/// Routing track kinds (Phase 3): receive sends rather than hosting clips.
#[inline]
pub(super) fn is_routing_type(track_type: &str) -> bool {
    is_routing_track_type(track_type)
}

#[inline]
pub(super) fn is_vsti_output_child_track_id(track_id: &str) -> bool {
    track_id.starts_with("vsti-out:")
}

/// Two distinct mutable elements of a slice without allocation. Panics in
/// debug if `a == b`; callers guarantee distinct indices.
#[inline]
pub(super) fn two_mut<T>(v: &mut [T], a: usize, b: usize) -> (&mut T, &mut T) {
    debug_assert!(a != b);
    if a < b {
        let (lo, hi) = v.split_at_mut(b);
        (&mut lo[a], &mut hi[0])
    } else {
        let (lo, hi) = v.split_at_mut(a);
        (&mut hi[0], &mut lo[b])
    }
}

#[inline]
pub(crate) fn effective_track_muted(track: &RuntimeTrack, beat: f64) -> bool {
    if track.automation_lanes.is_empty() {
        return track.muted;
    }
    track
        .automation_values_at_beat(track.automation_clock.fader_beat(beat))
        .muted
        .unwrap_or(track.muted)
}

/// Source-track silence: its own mute (including mute automation), or another
/// track's solo. Routing tracks are not silenced by solo; see
/// [`routing_output_silenced`].
#[inline]
pub(super) fn source_output_silenced(
    runtime: &RuntimeProject,
    track_index: usize,
    beat: f64,
) -> bool {
    effective_track_muted(&runtime.tracks[track_index], beat)
        || (runtime.has_solo
            && !runtime.tracks[track_index].solo
            && !solo_keeps_source_audible(runtime, track_index))
}

/// Bus / return / group silence. Solo does not apply, except to a VSTi
/// multi-out strip, which is that instrument's own channel.
#[inline]
pub(super) fn routing_output_silenced(
    runtime: &RuntimeProject,
    track_index: usize,
    beat: f64,
) -> bool {
    if effective_track_muted(&runtime.tracks[track_index], beat) {
        return true;
    }
    runtime.has_solo
        && is_vsti_output_child_track_id(&runtime.tracks[track_index].id)
        && !runtime.tracks[track_index].solo
        && !has_soloed_vsti_output_parent(runtime, track_index)
}

/// True when this channel's compensation delay, or one of its send-alignment
/// delays, is in the signal path. Those rings have to keep moving while the
/// channel is silent; a channel with nothing to delay can stay skipped.
#[inline]
pub(super) fn compensation_delay_running(runtime: &RuntimeProject, track_index: usize) -> bool {
    if runtime
        .latency_graph
        .track_pdc_delay
        .get(track_index)
        .copied()
        .unwrap_or(0)
        > 0
    {
        return true;
    }
    runtime
        .latency_graph
        .send_pdc_delay
        .get(track_index)
        .is_some_and(|row| row.iter().any(|delay| *delay > 0))
}

/// Apply a track's fader (volume / pan / preview mode) to its `block_*`
/// (which already holds the post-insert signal), write the post-fader result
/// back into `block_*`, and accumulate the track meter. Does **not** sum to any
/// destination — routing is done separately by [`route_main_output`]. No
/// allocation.
#[inline]
pub(super) fn apply_fader(track: &mut RuntimeTrack, frames: usize, beat: f64, smooth: bool) {
    let automation = track.automation_values_at_beat(beat);
    let volume = automation.volume.unwrap_or(track.volume);
    // A spatialised channel is placed by its room position on the way into
    // the master, so its fader carries level only.
    let pan = if track.spatial.is_some() {
        0.0
    } else {
        automation.pan.unwrap_or(track.pan)
    };
    let (pan_l, pan_r) = pan_gains(pan);
    let target_l = volume * pan_l;
    let target_r = volume * pan_r;
    // Measured, not asserted: `pre` is what the channel produced, `post` is what
    // routing will sum. `automation_override=yes` is the case where `track.volume`
    // is set correctly and ignored anyway, which looks identical from outside.
    let fader_trace_pre = (fader_debug_enabled() && frames > 0).then(|| {
        track.block_l[..frames]
            .iter()
            .fold(0.0f32, |peak, s| peak.max(s.abs()))
    });
    let (block_l, block_r) = (&mut track.block_l[..frames], &mut track.block_r[..frames]);
    if !smooth {
        // Tests and callers without smoothing still apply a deterministic
        // constant gain, then keep the smoother aligned for the next block.
        simd::gain_stereo(block_l, block_r, target_l, target_r);
        fold_preview_mode_block(block_l, block_r, track.preview_mode);
        track.smoothed_gain_l = target_l;
        track.smoothed_gain_r = target_r;
        log_fader_trace(track, frames, fader_trace_pre, volume, &automation);
        return;
    }
    // Ramp from the previously applied gain to the new target across the block.
    // Each block ends on the target, so successive blocks are continuous.
    let start_l = track.smoothed_gain_l;
    let start_r = track.smoothed_gain_r;
    let inv = 1.0 / frames as f32;
    let inc_l = (target_l - start_l) * inv;
    let inc_r = (target_r - start_r) * inv;
    simd::ramp_gain_stereo(block_l, block_r, start_l, inc_l, start_r, inc_r);
    fold_preview_mode_block(block_l, block_r, track.preview_mode);
    track.smoothed_gain_l = target_l;
    track.smoothed_gain_r = target_r;
    log_fader_trace(track, frames, fader_trace_pre, volume, &automation);
}

/// Report what [`apply_fader`] measured. `FUTUREBOARD_FADER_DEBUG=1` only, and
/// throttled — this writes to stderr from the audio callback, which is a
/// diagnostic exception to the realtime rules, not a steady-state path.
///
/// Read it as: `applied` is the ratio the block actually moved by, `volume` is
/// what the engine believes the fader is set to. They agree ⇒ the engine is
/// doing its job and the fault is upstream of `SetTrackVolume`. They disagree ⇒
/// the fault is here.
#[inline]
pub(super) fn log_fader_trace(
    track: &RuntimeTrack,
    frames: usize,
    pre_peak: Option<f32>,
    volume: f32,
    automation: &crate::runtime::RuntimeTrackAutomationValues,
) {
    let Some(pre) = pre_peak else {
        return;
    };
    if pre <= 1.0e-6 {
        return; // silent block — the ratio would be meaningless
    }
    static FADER_TRACE_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    if !FADER_TRACE_COUNT
        .fetch_add(1, Ordering::Relaxed)
        .is_multiple_of(64)
    {
        return;
    }
    let post = track.block_l[..frames]
        .iter()
        .fold(0.0f32, |peak, s| peak.max(s.abs()));
    eprintln!(
        "[fader-apply] track={} volume={:.4} pan={:.2} automation_override={} pre_peak={:.6} post_peak={:.6} applied={:.4}",
        track.id,
        volume,
        track.pan,
        if automation.volume.is_some() {
            "yes(fader ignored)"
        } else {
            "no"
        },
        pre,
        post,
        post / pre
    );
}

/// [`apply_preview_mode`] over a whole block, for a block already scaled by
/// its fader: the same arithmetic, in the same order, as applying it per frame.
#[inline]
pub(super) fn fold_preview_mode_block(
    block_l: &mut [f32],
    block_r: &mut [f32],
    mode: RuntimePreviewMode,
) {
    match mode {
        RuntimePreviewMode::Stereo => {}
        RuntimePreviewMode::Mono | RuntimePreviewMode::Mid => simd::fold_mid(block_l, block_r),
        RuntimePreviewMode::Side => simd::fold_side(block_l, block_r),
    }
}

#[inline]
pub(super) fn accumulate_block_meter(track: &mut RuntimeTrack, frames: usize) {
    let (peak_l, sum_l) = simd::peak_and_sum_sq(&track.block_l[..frames]);
    let (peak_r, sum_r) = simd::peak_and_sum_sq(&track.block_r[..frames]);
    track.meter_peak_l = track.meter_peak_l.max(peak_l);
    track.meter_peak_r = track.meter_peak_r.max(peak_r);
    track.meter_sum_sq_l += sum_l;
    track.meter_sum_sq_r += sum_r;
}

/// Sum a track's post-fader `block_*` into its output destination.
///
/// If `output_track_id` resolves to a routing track (bus/group/return) the
/// full post-fader signal is added to that track's receive buffer (`recv_*`),
/// so it is processed in Pass 2; otherwise it sums into the interleaved master
/// output. Cycle-safe like [`accumulate_sends`]: routing to self, to a
/// non-routing track, or backward between routing tracks falls back to master.
/// No allocation.
#[inline]
pub(crate) fn route_main_output(
    runtime: &mut RuntimeProject,
    src_index: usize,
    frames: usize,
    output: &mut [f32],
    channels: usize,
) {
    // Resolved at build time (None for master/missing) — no id lookup on the
    // audio thread.
    let target = runtime.tracks[src_index]
        .output_track_index
        .filter(|&t| t < runtime.tracks.len());

    if let Some(t) = target {
        let accept = t != src_index && is_routing_type(&runtime.tracks[t].track_type);
        if accept {
            let (src, tgt) = two_mut(&mut runtime.tracks, src_index, t);
            simd::add_into(&mut tgt.recv_l[..frames], &src.block_l[..frames]);
            simd::add_into(&mut tgt.recv_r[..frames], &src.block_r[..frames]);
            return;
        }
    }

    // A spatial mix: the channel is rendered where it sits in the room, into
    // the spatial bus the master stage reads.
    {
        let crate::runtime::RuntimeProject {
            tracks, spatial, ..
        } = runtime;
        let track = &mut tracks[src_index];
        if let Some(spatialiser) = track.spatial.as_mut() {
            spatialiser.process(
                &track.block_l[..frames],
                &track.block_r[..frames],
                &track.spatial_params,
                &spatial.room,
                &mut spatial.bus,
            );
            return;
        }
    }

    // Default / fallback: sum into the master output.
    let track = &runtime.tracks[src_index];
    if channels == 2 {
        simd::add_planar_to_interleaved_stereo(
            &mut output[..frames * 2],
            &track.block_l[..frames],
            &track.block_r[..frames],
        );
        return;
    }
    for f in 0..frames {
        let out = &mut output[f * channels..f * channels + channels];
        out[0] += track.block_l[f];
        out[1] += track.block_r[f];
    }
}

/// Put the spatial bus into the interleaved device buffer. Returns the number
/// of channels a surround mix occupies when it went out discretely, or `None`
/// when the mix is in channels 0/1 (stereo, binaural, or a folded surround).
///
/// Realtime-safe: reads the preallocated bus, writes the device buffer; the
/// virtual speakers render with their own preallocated state.
pub(super) fn mix_spatial_bus(
    runtime: &mut RuntimeProject,
    frames: usize,
    output: &mut [f32],
    channels: usize,
) -> Option<usize> {
    if !runtime.spatial.active() || channels < 2 {
        return None;
    }
    let crate::runtime::RuntimeProject {
        spatial, monitor, ..
    } = runtime;
    let frames = frames.min(spatial.bus.capacity());
    // Stack scratch a chunk at a time: no allocation, and the renderers
    // carry their state across chunk boundaries as they do across blocks.
    const CHUNK: usize = 256;
    match spatial.format {
        solfege_spatialaudio::SpatialFormat::Binaural => {
            let mut l = [0.0f32; CHUNK];
            let mut r = [0.0f32; CHUNK];
            let mut offset = 0;
            while offset < frames {
                let n = (frames - offset).min(CHUNK);
                l[..n].copy_from_slice(&spatial.bus.channel(0, frames)[offset..offset + n]);
                r[..n].copy_from_slice(&spatial.bus.channel(1, frames)[offset..offset + n]);
                if let Some(tail) = spatial.tail.as_mut() {
                    tail.process(&mut l[..n], &mut r[..n]);
                }
                for i in 0..n {
                    output[(offset + i) * channels] += l[i];
                    output[(offset + i) * channels + 1] += r[i];
                }
                offset += n;
            }
            None
        }
        solfege_spatialaudio::SpatialFormat::Surround(layout) => {
            if let Some(count) = spatial.discrete_channels(channels, monitor) {
                for c in 0..count {
                    let bus = spatial.bus.channel(c, frames);
                    for f in 0..frames {
                        output[f * channels + c] += bus[f];
                    }
                }
                return Some(count);
            }
            // Folded into the first pair.
            let mut l = [0.0f32; CHUNK];
            let mut r = [0.0f32; CHUNK];
            let mut offset = 0;
            if spatial.fold == solfege_spatialaudio::MonitorFold::Binaural {
                if let Some(speakers) = spatial.virtual_speakers.as_mut() {
                    while offset < frames {
                        let n = (frames - offset).min(CHUNK);
                        l[..n].fill(0.0);
                        r[..n].fill(0.0);
                        speakers.render_range(
                            &spatial.bus,
                            offset,
                            n,
                            &spatial.room,
                            &mut l[..n],
                            &mut r[..n],
                        );
                        if let Some(tail) = spatial.tail.as_mut() {
                            tail.process(&mut l[..n], &mut r[..n]);
                        }
                        for i in 0..n {
                            output[(offset + i) * channels] += l[i];
                            output[(offset + i) * channels + 1] += r[i];
                        }
                        offset += n;
                    }
                    return None;
                }
            }
            while offset < frames {
                let n = (frames - offset).min(CHUNK);
                l[..n].fill(0.0);
                r[..n].fill(0.0);
                solfege_spatialaudio::fold_down_range(
                    &spatial.bus,
                    layout,
                    offset,
                    n,
                    &mut l[..n],
                    &mut r[..n],
                );
                for i in 0..n {
                    output[(offset + i) * channels] += l[i];
                    output[(offset + i) * channels + 1] += r[i];
                }
                offset += n;
            }
            None
        }
        solfege_spatialaudio::SpatialFormat::Stereo => None,
    }
}

/// Capture this channel's contribution to the Control Room at `stage`.
///
/// Two independent taps share this call:
///
/// * **Listen** — a channel with PFL/AFL engaged sums into the listen bus.
///   PFL is captured pre-fader so its level is independent of the fader; AFL is
///   captured post-fader so it follows the fader.
/// * **Source** — when the Control Room's selected source names this channel
///   (a bus, or a track tapped pre/post fader), its block is copied into the
///   monitor source scratch.
///
/// This only *reads* `block_*`; it never modifies the channel, so monitoring
/// cannot alter the master mix, an export, or a recording. Allocation-free, and
/// an early-out on the overwhelmingly common case (no Listen engaged, master
/// bus source) keeps it off the hot path.
#[inline]
pub(super) fn capture_monitor_taps(
    runtime: &mut RuntimeProject,
    track_index: usize,
    frames: usize,
    stage: TapStage,
) {
    let wants_listen = runtime.tracks[track_index].listen.is_active()
        && match runtime.tracks[track_index].listen {
            ListenMode::Pfl => stage == TapStage::PreFader,
            ListenMode::Afl => stage == TapStage::PostFader,
            ListenMode::Off => false,
        };
    let wants_source = runtime.monitor.source_track_index == Some(track_index)
        && runtime.monitor.source_stage == Some(stage);
    if !wants_listen && !wants_source {
        return;
    }
    if !runtime.monitor.has_block_capacity(frames) {
        return;
    }
    let (track, monitor) = (&runtime.tracks[track_index], &mut runtime.monitor);
    if wants_listen {
        for i in 0..frames {
            monitor.listen_l[i] += track.block_l[i];
            monitor.listen_r[i] += track.block_r[i];
        }
        monitor.listen_active = true;
    }
    if wants_source {
        monitor.source_l[..frames].copy_from_slice(&track.block_l[..frames]);
        monitor.source_r[..frames].copy_from_slice(&track.block_r[..frames]);
        monitor.source_captured = true;
    }
}

/// Everything a channel does after its insert chain: multi-out, sends, fader,
/// meter, monitor taps, PDC and routing. The chain itself runs first, in
/// [`run_track_chains`], so that independent channels can process it in
/// parallel while this part keeps its fixed order on the audio thread.
///
/// `emit` is false while mute or solo is holding the channel out of the mix.
/// The compensation and send delay lines still advance on the live signal —
/// stopping them replays the audio they held at the moment of silence — and
/// nothing is summed, metered, or monitored.
pub(super) fn finish_track_block(
    runtime: &mut RuntimeProject,
    track_index: usize,
    frames: usize,
    output: &mut [f32],
    channels: usize,
    beat: f64,
    emit: bool,
) {
    // Multi-out: demux this track's bridged-instrument output channels into the
    // child "Out Ch" tracks' receive buffers (no-op unless the insert defines
    // child routes). Runs before pass 2, where the child routing tracks consume
    // their `recv_*`. A silenced parent does not feed its children.
    if emit {
        scatter_vsti_output_children(runtime, track_index, frames, output, channels);
    }
    // The audio here left the timeline this track's latency ago: its sends
    // and fader read their lanes there, and the fader ramps to the lanes'
    // value at the block's end (see `RuntimeAutomationClock`).
    let clock = runtime.tracks[track_index].automation_clock;
    let (beat, fader_target_beat) = (clock.fader_beat(beat), clock.fader_end_beat(beat));
    // Pre-fader sends tap the post-insert signal currently in block_*.
    accumulate_sends(runtime, track_index, frames, true, beat, !emit);
    // PFL and a TrackPreFader monitor source tap the same point the pre-fader
    // sends do: after this channel's inserts, before its fader.
    if emit {
        capture_monitor_taps(runtime, track_index, frames, TapStage::PreFader);
    }
    let smooth = runtime.fader_smoothing;
    apply_fader(
        &mut runtime.tracks[track_index],
        frames,
        if smooth { fader_target_beat } else { beat },
        smooth,
    );
    if emit {
        accumulate_block_meter(&mut runtime.tracks[track_index], frames);
        // AFL, a TrackAfterFader source, and a Bus source tap after the fader and
        // channel processing, before PDC — matching the post-fader send tap so
        // every monitoring path stays time-aligned with the sends.
        capture_monitor_taps(runtime, track_index, frames, TapStage::PostFader);
    }
    // Post-fader sends tap *before* PDC so return FX latency can be compensated
    // on the dry/main path without also delaying the send feed (which would
    // push wet further behind dry).
    accumulate_sends(runtime, track_index, frames, false, beat, !emit);
    let pdc_delay = runtime
        .latency_graph
        .track_pdc_delay
        .get(track_index)
        .copied()
        .unwrap_or(0);
    if pdc_delay > 0 {
        let track = &mut runtime.tracks[track_index];
        apply_pdc_delay_block(
            &mut track.block_l[..frames],
            &mut track.block_r[..frames],
            &mut track.pdc_delay_l,
            &mut track.pdc_delay_r,
            &mut track.pdc_write_pos,
            pdc_delay,
            frames,
        );
    }
    if !emit {
        // The delay lines already hold this block. Drop it so the mix, the
        // export taps, and a loopback reader hear silence rather than the
        // delayed signal or a frozen earlier block.
        let track = &mut runtime.tracks[track_index];
        track.block_l[..frames].fill(0.0);
        track.block_r[..frames].fill(0.0);
        return;
    }
    // Route the post-fader (and PDC-aligned) signal to master or the track's output bus.
    route_main_output(runtime, track_index, frames, output, channels);
}

/// True when `from`'s output, a send, or a multi-out child route lands in
/// `to`'s receive buffer. Such a pair cannot share a chain batch: `to` has to
/// wait for everything `from` sums into it.
pub(super) fn feeds_track(tracks: &[RuntimeTrack], from: usize, to: usize) -> bool {
    let track = &tracks[from];
    track.output_track_index == Some(to)
        || track
            .sends
            .iter()
            .any(|send| send.return_track_index == Some(to))
        || track.inserts.iter().any(|insert| {
            insert
                .vsti_output_children
                .iter()
                .any(|child| child.dest_track_index == Some(to))
        })
}

/// Add the source track's block (`block_*`, holding either the post-insert or
/// post-fader signal depending on `pre_fader`) into each accepted send target's
/// receive buffer (`recv_*`), scaled by the send level. Only sends whose
/// `pre_fader` flag matches the requested phase are routed.
///
/// Cycle-safe by construction: a send is accepted only when the target is a
/// routing track (bus/return). Cyclic routes are rejected at graph-plan time
/// (`plan_runtime_audio_graph`) and pass-2 processes routing tracks in
/// topological order so chained bus→return feeds land before the target runs.
/// Sends to non-routing tracks or to self are dropped. No allocation on the
/// audio thread.
///
/// A send with a planned PDC delay (`send_pdc_delay`) feeds the return through
/// its own delay ring, so sources with different plug-in latency meet aligned
/// at the return. That ring keeps running while the send is off, at zero, or
/// `silent` because the source track is muted or solo-silenced, so switching
/// it back on does not replay stale audio.
#[inline]
pub(crate) fn accumulate_sends(
    runtime: &mut RuntimeProject,
    src_index: usize,
    frames: usize,
    pre_fader: bool,
    beat: f64,
    silent: bool,
) {
    let send_count = runtime.tracks[src_index].sends.len();
    if send_count == 0 {
        return;
    }
    for s in 0..send_count {
        let (enabled, level, target_index) = {
            let track = &runtime.tracks[src_index];
            let send = &track.sends[s];
            if send.pre_fader != pre_fader {
                continue;
            }
            // The send's own on/off and level, or its automation's.
            let (enabled, level) = track.send_state_at(s, beat);
            (enabled, level, send.return_track_index)
        };
        let delay = runtime
            .latency_graph
            .send_pdc_delay
            .get(src_index)
            .and_then(|row| row.get(s))
            .copied()
            .unwrap_or(0);
        let audible = !silent && enabled && level != 0.0;
        if !audible && delay == 0 {
            continue;
        }
        // Resolved at build time — no id lookup on the audio thread.
        let Some(t) = target_index.filter(|&t| t < runtime.tracks.len()) else {
            continue;
        };
        if t == src_index || !is_routing_type(&runtime.tracks[t].track_type) {
            continue;
        }
        let (src, tgt) = two_mut(&mut runtime.tracks, src_index, t);
        if delay > 0 {
            crate::latency_graph::accumulate_delayed_send(
                &src.block_l[..frames],
                &src.block_r[..frames],
                &mut tgt.recv_l[..frames],
                &mut tgt.recv_r[..frames],
                &mut src.sends[s].pdc,
                delay,
                if audible { level } else { 0.0 },
            );
            continue;
        }
        simd::add_scaled_into(&mut tgt.recv_l[..frames], &src.block_l[..frames], level);
        simd::add_scaled_into(&mut tgt.recv_r[..frames], &src.block_r[..frames], level);
    }
}

/// Multi-out (Slice 2): after a bridged instrument's chain has run and read the
/// full plugin block into each insert's `scratch_multi`, scatter every child
/// route's channel pair into the destination "Out Ch" track's receive buffer.
/// The dest tracks are routing-style and process in pass 2, so by the time they
/// run their `recv_*` already holds the demuxed pair. No-op (cheap bail) for the
/// overwhelmingly common case of a track with no multi-out children.
///
/// Children read the *raw* plugin output (pre the instrument track's fader), so
/// each child strip is independently mixable — exactly the multi-out contract.
#[inline]
pub(super) fn track_has_master_route(runtime: &RuntimeProject, track_index: usize) -> bool {
    let mut current = track_index;
    for _ in 0..runtime.tracks.len().saturating_add(1) {
        let Some(track) = runtime.tracks.get(current) else {
            return false;
        };
        let Some(next) = track.output_track_index else {
            return true;
        };
        if next >= runtime.tracks.len() || next == current {
            return true;
        }
        if !is_routing_type(&runtime.tracks[next].track_type) {
            return true;
        }
        current = next;
    }
    false
}

#[inline]
pub(super) fn add_bus_pair_to_master_output(
    scratch: &[f32],
    scratch_channels: usize,
    ch_l: usize,
    ch_r: usize,
    frames: usize,
    output: &mut [f32],
    output_channels: usize,
) {
    if output_channels < 2 || scratch_channels == 0 {
        return;
    }
    let n = frames
        .min(scratch.len() / scratch_channels)
        .min(output.len() / output_channels);
    for i in 0..n {
        let base = i * scratch_channels;
        let out = &mut output[i * output_channels..i * output_channels + output_channels];
        out[0] += scratch[base + ch_l - 1];
        out[1] += scratch[base + ch_r - 1];
    }
}

pub(crate) fn scatter_vsti_output_children(
    runtime: &mut RuntimeProject,
    source_track_index: usize,
    frames: usize,
    output: &mut [f32],
    output_channels: usize,
) {
    if frames == 0 || source_track_index >= runtime.tracks.len() {
        return;
    }
    // This function runs on the device callback. Production routing must not
    // allocate, format strings, scan peaks, or write stderr. The detailed bus
    // trace remains available behind the explicit forensic flag.
    let trace = crate::forensic_trace::forensic_trace_enabled();
    let insert_count = runtime.tracks[source_track_index].inserts.len();
    for ins in 0..insert_count {
        let child_count = runtime.tracks[source_track_index].inserts[ins]
            .vsti_output_children
            .len();
        if child_count == 0 {
            continue;
        }
        // `scratch_multi` is sized to exactly `frames * channels` by the read
        // above, so the channel stride is recoverable here without extra state.
        let channels = {
            let len = runtime.tracks[source_track_index].inserts[ins]
                .scratch_multi
                .len();
            if len == 0 {
                continue;
            }
            len / frames
        };
        if channels == 0 {
            continue;
        }
        for c in 0..child_count {
            let (dest_idx, ch_l, ch_r, bus_index, channel_count, trace_ids) = {
                let child =
                    &runtime.tracks[source_track_index].inserts[ins].vsti_output_children[c];
                let ch_l = child.channel_l as usize;
                let ch_r = child.channel_r as usize;
                if !(1..=channels).contains(&ch_l) || !(1..=channels).contains(&ch_r) {
                    continue;
                }
                (
                    child.dest_track_index,
                    ch_l,
                    ch_r,
                    child.bus_index,
                    child.channel_count,
                    trace.then(|| {
                        (
                            child.dest_track_id.clone(),
                            runtime.tracks[source_track_index].inserts[ins].id.clone(),
                        )
                    }),
                )
            };
            let scratch_len = runtime.tracks[source_track_index].inserts[ins]
                .scratch_multi
                .len();
            let n = frames.min(scratch_len / channels);
            if n == 0 {
                continue;
            }
            let (source_peak_l, source_peak_r, sum_l, sum_r) = if trace {
                let mut source_peak_l = 0.0f32;
                let mut source_peak_r = 0.0f32;
                let mut sum_l = 0.0f64;
                let mut sum_r = 0.0f64;
                let scratch = &runtime.tracks[source_track_index].inserts[ins].scratch_multi;
                for i in 0..n {
                    let base = i * channels;
                    let l = scratch[base + ch_l - 1];
                    let r = scratch[base + ch_r - 1];
                    source_peak_l = source_peak_l.max(l.abs());
                    source_peak_r = source_peak_r.max(r.abs());
                    sum_l += (l as f64) * (l as f64);
                    sum_r += (r as f64) * (r as f64);
                }
                (source_peak_l, source_peak_r, sum_l, sum_r)
            } else {
                (0.0, 0.0, 0.0, 0.0)
            };
            let nonzero = trace && source_peak_l.max(source_peak_r) > 0.0001;
            let process_seq = if trace {
                static BUS_AUDIO_ROUTE_SEQ: std::sync::atomic::AtomicU64 =
                    std::sync::atomic::AtomicU64::new(0);
                BUS_AUDIO_ROUTE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            } else {
                0
            };
            let route_log = trace && (process_seq.is_multiple_of(30) || nonzero);
            let (dest_track_id, insert_id) = trace_ids
                .as_ref()
                .map(|(dest, insert)| (dest.as_str(), insert.as_str()))
                .unwrap_or(("", ""));
            if route_log {
                let rms_l = (sum_l / n as f64).sqrt();
                let rms_r = (sum_r / n as f64).sqrt();
                eprintln!(
                    "[VSTI PROCESS OUT]\nplugin_instance_id={insert_id}\nplugin_name={insert_id}\nprocess_seq={process_seq}\nbus_index={bus_index}\nchannel_count={channel_count}\npeak_l={source_peak_l:.6}\npeak_r={source_peak_r:.6}\nrms_l={rms_l:.6}\nrms_r={rms_r:.6}\nnonzero={nonzero}"
                );
            }
            let Some(dest_idx) = dest_idx else {
                let scratch = &runtime.tracks[source_track_index].inserts[ins].scratch_multi;
                add_bus_pair_to_master_output(
                    scratch,
                    channels,
                    ch_l,
                    ch_r,
                    frames,
                    output,
                    output_channels,
                );
                if trace {
                    eprintln!(
                        "[ROUTING ERROR]\nplugin_instance_id={insert_id}\nbus_index={bus_index}\nreason=destination mixer channel does not exist\ndestination_mixer_channel_id={dest_track_id}\nfallback_downmix=true"
                    );
                }
                if route_log {
                    eprintln!(
                        "[BUS TO MIXER WRITE]\nplugin_instance_id={insert_id}\nbus_index={bus_index}\nsource_peak_l={source_peak_l:.6}\nsource_peak_r={source_peak_r:.6}\ndestination_mixer_channel_id={dest_track_id}\ndestination_exists=false\nsamples_written=false\nwrite_peak_l=0.000000\nwrite_peak_r=0.000000"
                    );
                }
                continue;
            };
            if dest_idx >= runtime.tracks.len() || dest_idx == source_track_index {
                let scratch = &runtime.tracks[source_track_index].inserts[ins].scratch_multi;
                add_bus_pair_to_master_output(
                    scratch,
                    channels,
                    ch_l,
                    ch_r,
                    frames,
                    output,
                    output_channels,
                );
                if trace {
                    eprintln!(
                        "[ROUTING ERROR]\nplugin_instance_id={insert_id}\nbus_index={bus_index}\nreason=stale mixer channel index\ndestination_mixer_channel_id={dest_track_id}\nfallback_downmix=true"
                    );
                }
                continue;
            }
            let route_to_master_exists = track_has_master_route(runtime, dest_idx);
            {
                let (src_track, dst_track) =
                    two_mut(&mut runtime.tracks, source_track_index, dest_idx);
                let scratch = &src_track.inserts[ins].scratch_multi;
                let n = n.min(dst_track.recv_l.len()).min(dst_track.recv_r.len());
                for i in 0..n {
                    let base = i * channels;
                    dst_track.recv_l[i] += scratch[base + ch_l - 1];
                    dst_track.recv_r[i] += scratch[base + ch_r - 1];
                }
            }
            if !route_to_master_exists {
                let scratch = &runtime.tracks[source_track_index].inserts[ins].scratch_multi;
                add_bus_pair_to_master_output(
                    scratch,
                    channels,
                    ch_l,
                    ch_r,
                    frames,
                    output,
                    output_channels,
                );
                if trace {
                    eprintln!(
                        "[ROUTING ERROR]\nplugin_instance_id={insert_id}\nbus_index={bus_index}\nreason=destination mixer channel has no route to master\ndestination_mixer_channel_id={dest_track_id}\nfallback_downmix=true"
                    );
                }
            }
            if route_log {
                let dst_track = &runtime.tracks[dest_idx];
                let n = n.min(dst_track.recv_l.len()).min(dst_track.recv_r.len());
                let write_peak_l = dst_track.recv_l[..n]
                    .iter()
                    .fold(0.0f32, |peak, sample| peak.max(sample.abs()));
                let write_peak_r = dst_track.recv_r[..n]
                    .iter()
                    .fold(0.0f32, |peak, sample| peak.max(sample.abs()));
                let mute = dst_track.muted;
                let solo = dst_track.solo;
                let gain = dst_track.volume;
                eprintln!(
                    "[BUS TO MIXER WRITE]\nplugin_instance_id={insert_id}\nbus_index={bus_index}\nsource_peak_l={source_peak_l:.6}\nsource_peak_r={source_peak_r:.6}\ndestination_mixer_channel_id={dest_track_id}\ndestination_exists=true\nsamples_written={}\nwrite_peak_l={write_peak_l:.6}\nwrite_peak_r={write_peak_r:.6}",
                    write_peak_l.max(write_peak_r) > 0.0001
                );
                eprintln!(
                    "[MIXER CHANNEL AFTER STRIP]\nmixer_channel_id={dest_track_id}\nroute_node_id={dest_track_id}\nbus_index={bus_index}\nmute={mute}\nsolo={solo}\ngain={gain:.6}\npost_strip_peak_l={write_peak_l:.6}\npost_strip_peak_r={write_peak_r:.6}\nroute_to_master_exists={route_to_master_exists}"
                );
            }
            if route_log && nonzero {
                let rms_l = (sum_l / n as f64).sqrt();
                let rms_r = (sum_r / n as f64).sqrt();
                eprintln!(
                    "[BUS AUDIO ROUTE]\nprocess_seq={process_seq}\nplugin_instance_id={insert_id}\nbus_index={bus_index}\nsource_processdata_output_index={bus_index}\nsource_channel_count={channel_count}\ndestination_mixer_channel_id={dest_track_id}\npeak_l={source_peak_l:.6}\npeak_r={source_peak_r:.6}\nrms_l={rms_l:.6}\nrms_r={rms_r:.6}\ncopied_to_parent_track=false\nfallback_downmix={}",
                    !route_to_master_exists
                );
            }
        }
    }
}

#[inline]
pub fn apply_preview_mode(l: f32, r: f32, mode: RuntimePreviewMode) -> (f32, f32) {
    match mode {
        RuntimePreviewMode::Stereo => (l, r),
        RuntimePreviewMode::Mono | RuntimePreviewMode::Mid => {
            let m = (l + r) * 0.5;
            (m, m)
        }
        RuntimePreviewMode::Side => {
            let s = (l - r) * 0.5;
            (s, s)
        }
    }
}

/// Channel pan law for the track fader stage — see
/// [`crate::dsp::gain::pan_gains`]. One law for the live callback and the
/// offline bounce so an export sits exactly where the mix was heard.
#[inline]
pub fn pan_gains(pan: f32) -> (f32, f32) {
    crate::dsp::gain::pan_gains(pan)
}

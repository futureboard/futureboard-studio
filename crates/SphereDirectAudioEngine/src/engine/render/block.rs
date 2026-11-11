//! The block renderer and its entry points.

use super::*;

/// `transport_active` — false when this block is rendered while the transport
/// is stopped (MIDI preview, post-panic bridge flush, open plugin editor). In
/// that mode the track/insert graph still runs (so bridged VSTi previews are
/// heard and the host handshake stays alive) but timeline clip material is
/// skipped — otherwise the frozen playhead would stutter-loop the same audio
/// clip slice every callback.
#[allow(clippy::too_many_arguments)]
pub fn render_project_block_interleaved(
    runtime: &mut RuntimeProject,
    base_sample: u64,
    master_volume: f32,
    output: &mut [f32],
    channels: usize,
    transport_active: bool,
    time_sig_num: u32,
    time_sig_den: u32,
    loop_bounds: Option<crate::transport::LoopBounds>,
) -> u64 {
    render_project_block_interleaved_core(
        runtime,
        base_sample,
        master_volume,
        output,
        channels,
        transport_active,
        time_sig_num,
        time_sig_den,
        loop_bounds,
        None,
        None,
        None,
    )
}

/// Realtime variant that injects the selected live-input block into every
/// software-monitored source track before Pass 1. The normal graph then runs
/// exactly once, preserving plugin state, PDC, sends, buses, and master DSP.
#[allow(clippy::too_many_arguments)]
pub fn render_project_block_interleaved_with_live_input(
    runtime: &mut RuntimeProject,
    base_sample: u64,
    master_volume: f32,
    output: &mut [f32],
    channels: usize,
    transport_active: bool,
    time_sig_num: u32,
    time_sig_den: u32,
    loop_bounds: Option<crate::transport::LoopBounds>,
    input_l: &[f32],
    input_r: &[f32],
) -> u64 {
    render_project_block_interleaved_core(
        runtime,
        base_sample,
        master_volume,
        output,
        channels,
        transport_active,
        time_sig_num,
        time_sig_den,
        loop_bounds,
        Some((input_l, input_r)),
        None,
        None,
    )
}

/// Realtime variant that additionally lets tracks routed to an Audio Jam stream
/// draw from the jam bus.
///
/// `live_input` and `jam` are separate on purpose: they are different sources
/// with different clocks, and a track is routed to exactly one of them. Passing
/// both here is normal — one session can have a guitarist on the interface and
/// a second guitarist over the network at the same time.
#[allow(clippy::too_many_arguments)]
pub fn render_project_block_interleaved_with_inputs(
    runtime: &mut RuntimeProject,
    base_sample: u64,
    master_volume: f32,
    output: &mut [f32],
    channels: usize,
    transport_active: bool,
    time_sig_num: u32,
    time_sig_den: u32,
    loop_bounds: Option<crate::transport::LoopBounds>,
    live_input: Option<(&[f32], &[f32])>,
    jam: Option<&crate::jam_bus::JamAudioBus>,
) -> u64 {
    render_project_block_interleaved_core(
        runtime,
        base_sample,
        master_volume,
        output,
        channels,
        transport_active,
        time_sig_num,
        time_sig_den,
        loop_bounds,
        live_input,
        None,
        jam,
    )
}

/// Offline/export variant that captures every mixer channel post-fader in the
/// same graph pass. `track_taps` follows `runtime.tracks` order and is ignored
/// by realtime callers through the wrapper above.
#[allow(clippy::too_many_arguments)]
pub fn render_project_block_interleaved_with_taps(
    runtime: &mut RuntimeProject,
    base_sample: u64,
    master_volume: f32,
    output: &mut [f32],
    channels: usize,
    transport_active: bool,
    time_sig_num: u32,
    time_sig_den: u32,
    loop_bounds: Option<crate::transport::LoopBounds>,
    track_taps: Option<&mut [Vec<f32>]>,
) -> u64 {
    render_project_block_interleaved_core(
        runtime,
        base_sample,
        master_volume,
        output,
        channels,
        transport_active,
        time_sig_num,
        time_sig_den,
        loop_bounds,
        None,
        track_taps,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn render_project_block_interleaved_core(
    runtime: &mut RuntimeProject,
    base_sample: u64,
    master_volume: f32,
    output: &mut [f32],
    channels: usize,
    transport_active: bool,
    time_sig_num: u32,
    time_sig_den: u32,
    loop_bounds: Option<crate::transport::LoopBounds>,
    live_input: Option<(&[f32], &[f32])>,
    mut track_taps: Option<&mut [Vec<f32>]>,
    jam: Option<&crate::jam_bus::JamAudioBus>,
) -> u64 {
    if channels < 2 {
        return 0;
    }
    let frames = output.len() / channels;
    if frames == 0 {
        return 0;
    }
    if let Some(taps) = track_taps.as_deref_mut() {
        for tap in taps.iter_mut().take(runtime.tracks.len()) {
            tap.resize(frames * 2, 0.0);
            tap.fill(0.0);
        }
    }
    // Reset the Control Room taps for this block. Cheap and unconditional: the
    // offline exporter runs the same graph but never consumes these buffers, so
    // clearing them costs a memset over an empty (unallocated) scratch there.
    runtime.monitor.begin_block(frames);
    // Bridged inserts are charged one *configured* period for their handshake,
    // never this callback's frame count: a callback whose size varies would
    // otherwise replan compensation and move every dry track's delay with it
    // (#377). The live callback sets the period from the open stream; a graph
    // rendered without one (tests, a bare render) takes its first block.
    if runtime.bridge_period_frames == 0 {
        runtime.bridge_period_frames = frames as u32;
    }
    runtime.refresh_runtime_latency_graph(runtime.bridge_period_frames);
    let block_beat = sample_to_beat(runtime, base_sample);
    // Real transport ProcessContext for every plugin processed this block —
    // tempo from the map at this position, time signature from the engine,
    // project position from the playhead, playing = transport state. Replaces
    // the old hardcoded 120 BPM / always-playing stub.
    let transport = RuntimeTransportContext {
        tempo_bpm: runtime.tempo_map.bpm_at_beat(block_beat),
        time_sig_num,
        time_sig_den,
        project_time_samples: base_sample as i64,
        ppq_position: block_beat,
        bar_position_ppq: RuntimeTransportContext::bar_start_ppq(
            block_beat,
            time_sig_num,
            time_sig_den,
        ),
        playing: transport_active,
        recording: false,
    };
    for frame in output.chunks_mut(channels) {
        frame[0] = 0.0;
        frame[1] = 0.0;
        for extra in frame.iter_mut().skip(2) {
            *extra = 0.0;
        }
    }

    for (track_index, track) in runtime.tracks.iter_mut().enumerate() {
        let is_routing_or_master = crate::audio_graph::is_routing_track_type(&track.track_type)
            || crate::audio_graph::is_master_track_type(&track.track_type);
        let source_active = runtime
            .audio_graph
            .active_source_mask
            .get(track_index)
            .copied()
            .unwrap_or(true)
            || track.monitor_enabled
            || !track.midi_block_events.is_empty();
        if !is_routing_or_master && !source_active {
            continue;
        }
        if track.block_l.len() < frames {
            track.block_l.resize(frames, 0.0);
            track.block_r.resize(frames, 0.0);
        }
        // Receive buffers grow lazily to the largest block seen; the audio
        // thread only `fill`s, never allocates, once warmed.
        if track.recv_l.len() < frames {
            track.recv_l.resize(frames, 0.0);
            track.recv_r.resize(frames, 0.0);
        }
        track.block_l[..frames].fill(0.0);
        track.block_r[..frames].fill(0.0);
        track.recv_l[..frames].fill(0.0);
        track.recv_r[..frames].fill(0.0);
    }

    if let Some((input_l, input_r)) = live_input {
        let input_frames = frames.min(input_l.len()).min(input_r.len());
        // A jam-routed track is fed from the jam bus below, not from the
        // hardware input bus. Without this filter a remote performer would
        // double with whatever is plugged into the interface.
        for track in runtime.tracks.iter_mut().filter(|track| {
            track.track_type == "audio"
                && track.monitor_enabled
                && track.input_source.is_routable()
                && !track.input_source.is_internal()
        }) {
            for frame in 0..input_frames {
                track.block_l[frame] += input_l[frame];
                track.block_r[frame] += input_r[frame];
            }
        }
    }

    // Audio Jam input. One relaxed load skips the whole branch for a project
    // that is not in a jam, and each slot read is atomics only — no allocation,
    // no lock, and never a wait on the network.
    if let Some(bus) = jam.filter(|bus| bus.has_inputs()) {
        for track in runtime.tracks.iter_mut().filter(|track| {
            track.track_type == "audio" && track.monitor_enabled && track.input_source.is_jam()
        }) {
            let crate::runtime::RuntimeTrackInputSource::Jam { slot, mode } = track.input_source
            else {
                continue;
            };
            let Some(slot) = bus.input(slot as usize) else {
                continue;
            };
            slot.mix_into(
                mode,
                &mut track.block_l[..frames],
                &mut track.block_r[..frames],
                frames,
            );
        }
    }

    // Track loopback input: another track's post-fader output, as this track's
    // input. What lands here is the block the source rendered in the *previous*
    // callback — see `crate::loopback` for why that is the design and not a
    // shortcoming.
    //
    // Two passes over the index list rather than one over the tracks, because
    // reading one track while writing another needs the two borrows split. Both
    // are bounded by the track count and do nothing but copy.
    if runtime.tracks.iter().any(|track| {
        track.track_type == "audio" && track.monitor_enabled && track.input_source.is_loopback()
    }) {
        for index in 0..runtime.tracks.len() {
            let track = &runtime.tracks[index];
            if track.track_type != "audio" || !track.monitor_enabled {
                continue;
            }
            let crate::runtime::RuntimeTrackInputSource::Loopback { source_index, mode } =
                track.input_source
            else {
                continue;
            };
            let source_index = source_index as usize;
            // A track pointed at itself would read its own previous block and
            // build a feedback loop the user never asked for. The control
            // thread refuses to resolve one; this is the render path's own
            // guard, since a stale index must never be trusted.
            if source_index == index || source_index >= runtime.tracks.len() {
                continue;
            }
            {
                let source = &runtime.tracks[source_index];
                if !source.loopback_publish
                    || source.loopback_out_l.len() < frames
                    || source.loopback_out_r.len() < frames
                {
                    continue;
                }
            }
            // Borrow the source's buffers out and put them back. `Vec::take` is
            // a pointer swap, so this costs nothing and keeps the read of one
            // track and the write of another out of each other's way without
            // reaching for unsafe.
            let source_l = std::mem::take(&mut runtime.tracks[source_index].loopback_out_l);
            let source_r = std::mem::take(&mut runtime.tracks[source_index].loopback_out_r);
            {
                let track = &mut runtime.tracks[index];
                for frame in 0..frames {
                    let (l, r) = mode.apply(source_l[frame], source_r[frame]);
                    track.block_l[frame] += l;
                    track.block_r[frame] += r;
                }
            }
            runtime.tracks[source_index].loopback_out_l = source_l;
            runtime.tracks[source_index].loopback_out_r = source_r;
        }
    }
    // Every reader has taken last block's output; start this block's silent.
    // A source the graph skips this block (no material, muted, not reached)
    // writes nothing below, and without this its readers — and a loopback take
    // — would hear the last block it did render repeated for as long as it
    // stays quiet. A fill over buffers that only grow; nothing for a track
    // nobody reads.
    for track in runtime
        .tracks
        .iter_mut()
        .filter(|track| track.loopback_publish)
    {
        let n = frames
            .min(track.loopback_out_l.len())
            .min(track.loopback_out_r.len());
        track.loopback_out_l[..n].fill(0.0);
        track.loopback_out_r[..n].fill(0.0);
    }

    let master_index = runtime.audio_graph.master_index;
    // A Master Volume lane that has something to say overrides the master
    // fader, the way a track's Volume lane overrides its own.
    let master_volume = master_automation_volume(&runtime.tracks, master_index, block_beat)
        .unwrap_or(master_volume);

    for clip_index in 0..runtime.clips.len() {
        if !transport_active {
            break; // stopped-transport preview block — no timeline material
        }
        // An ARA plug-in renders this clip itself (see `render_ara_block`), so
        // the file must not also be mixed in here.
        if runtime.clips[clip_index].ara_rendered {
            continue;
        }
        // The overwhelmingly common non-looping path rejects inactive clips
        // before cloning their Arc source or reading the rest of their DSP
        // metadata. Large arrangements otherwise touched every clip every
        // callback even when only a handful overlap the current block.
        if loop_bounds.is_none() {
            let clip = &runtime.clips[clip_index];
            let block_end = base_sample.saturating_add(frames as u64);
            let clip_end = clip.start_sample.saturating_add(clip.duration_samples);
            if clip_end <= base_sample || clip.start_sample >= block_end {
                continue;
            }
        }
        let (
            clip_muted,
            clip_track_index,
            source,
            clip_start,
            clip_duration,
            clip_offset_seconds,
            clip_source_read_rate,
            clip_effective_time_ratio,
            clip_processor,
            clip_reverse,
            clip_fade_in,
            clip_fade_out,
            clip_fade_in_curve,
            clip_fade_out_curve,
            clip_stretch_backend,
        ) = {
            let clip = &runtime.clips[clip_index];
            (
                clip.muted,
                clip.track_index,
                Arc::clone(&clip.source),
                clip.start_sample,
                clip.duration_samples,
                clip.offset_seconds,
                clip.source_read_rate,
                clip.effective_time_ratio,
                clip.processor,
                clip.reverse,
                clip.fade_in_samples,
                clip.fade_out_samples,
                clip.fade_in_curve,
                clip.fade_out_curve,
                clip.stretch_backend,
            )
        };
        if clip_muted {
            continue;
        }
        // Resolved at build time (RuntimeProject::resolve_indices) — no id
        // lookup on the audio thread.
        let Some(track_index) = clip_track_index.filter(|&ti| ti < runtime.tracks.len()) else {
            continue;
        };
        // A silenced track's clips still have to render when a compensation
        // or send delay is running. Otherwise those rings stop, and un-muting
        // or un-soloing replays the audio they held when the track went silent.
        if source_output_silenced(runtime, track_index, block_beat)
            && !compensation_delay_running(runtime, track_index)
        {
            continue;
        }

        let clip_end = clip_start.saturating_add(clip_duration);
        // #380: keep a looped disk-streamed clip's loop start prefetched in a
        // cue, so the wrap does not miss the stream's window and play a block
        // of silence. Arithmetic and a relaxed compare-and-store only.
        if let (Some(bounds), crate::audio_source::ClipAudioSource::Streaming(stream)) =
            (loop_bounds, source.as_ref())
        {
            if bounds.start >= clip_start && bounds.start < clip_end {
                crate::streaming_source::cue_loop_start(
                    stream,
                    &runtime.clips[clip_index],
                    bounds.start - clip_start,
                    runtime.sample_rate,
                );
            }
        }
        let mut segment_sample =
            crate::transport::normalize_loop_position(base_sample, loop_bounds);
        let mut callback_offset = 0usize;
        let mut remaining = frames as u64;
        while remaining > 0 {
            let segment_frames = crate::transport::segment_frames_until_loop_wrap(
                segment_sample,
                remaining,
                loop_bounds,
            );
            let block_start = segment_sample;
            let block_end = segment_sample.saturating_add(segment_frames);
            if block_end > clip_start && block_start < clip_end {
                let render_start = clip_start.saturating_sub(block_start) as usize;
                let render_end = (clip_end.min(block_end) - block_start) as usize;
                let segment_render_frames = render_end.saturating_sub(render_start);
                let project_render_start = segment_sample + render_start as u64;
                let rel_start = project_render_start - clip_start;
                // Warped clips stream through the stretcher too, so Keep
                // Pitch and transpose hold on them; only a reversed warp (the
                // editor refuses to create one) keeps the resample path.
                let warped = !runtime.clips[clip_index].warp_segments.is_empty();
                if clip_stretch_backend == StretchBackend::Signalsmith
                    && !(warped && clip_reverse)
                    && render_signalsmith_clip_segment(
                        runtime,
                        clip_index,
                        track_index,
                        project_render_start,
                        rel_start,
                        callback_offset + render_start,
                        segment_render_frames,
                    )
                {
                    // Rendered through the cached SphereAudioProcessor/Signalsmith
                    // path. Export uses this same render kernel.
                } else {
                    for frame_in_segment in render_start..render_end {
                        let frame_idx = callback_offset + frame_in_segment;
                        let project_sample = segment_sample + frame_in_segment as u64;
                        let rel = project_sample - clip_start;
                        let warp_segments = runtime.clips[clip_index].warp_segments.as_slice();
                        let warped = !warp_segments.is_empty();
                        let source_pos = resolved_clip_source_frame(
                            rel,
                            clip_duration,
                            clip_offset_seconds,
                            runtime.sample_rate,
                            source.sample_rate() as f64,
                            if matches!(clip_processor, ClipDspProcessor::PhaseVocoderBasic)
                                && !warped
                            {
                                1.0 / clip_effective_time_ratio.max(0.01)
                            } else {
                                clip_source_read_rate
                            },
                            clip_reverse,
                            warp_segments,
                        );
                        let dry_source_pos = if warped {
                            source_pos
                        } else {
                            resolved_clip_source_frame(
                                rel,
                                clip_duration,
                                clip_offset_seconds,
                                runtime.sample_rate,
                                source.sample_rate() as f64,
                                clip_source_read_rate,
                                clip_reverse,
                                warp_segments,
                            )
                        };
                        let processor = if warped {
                            ClipDspProcessor::Resample
                        } else {
                            clip_processor
                        };
                        let (l, r) = sample_clip_processor_stereo(
                            &source,
                            source_pos,
                            dry_source_pos,
                            clip_effective_time_ratio,
                            processor,
                        );
                        let fade = clip_fade_gain_with_curves(
                            rel,
                            clip_duration,
                            clip_fade_in,
                            clip_fade_out,
                            clip_fade_in_curve,
                            clip_fade_out_curve,
                        );
                        let (l, r) =
                            process_clip_frame(&mut runtime.clips[clip_index], rel, fade, l, r);
                        runtime.tracks[track_index].block_l[frame_idx] += l;
                        runtime.tracks[track_index].block_r[frame_idx] += r;
                    }
                }
            }
            callback_offset += segment_frames as usize;
            remaining -= segment_frames;
            if remaining == 0 {
                break;
            }
            segment_sample = crate::transport::advance_loop_position(
                segment_sample,
                segment_frames,
                loop_bounds,
            )
            .0;
        }
    }

    // ── Pass 1: source tracks (audio / midi / instrument) ───────────────
    // Clips → inserts → fader, sum the post-fader signal into the master
    // output, then feed sends into routing-track receive buffers. Routing
    // tracks (bus/return/group) are deferred to Pass 2 so their inputs are complete.
    // Take the precomputed pass order out by move (zero alloc) rather than
    // cloning the Vec every audio block; the loop body never reads it back, and
    // it is restored below. `audio_graph` is otherwise untouched here.
    if runtime.spatial.active() {
        runtime.spatial.bus.clear(frames);
    }
    // Each pass runs in batches: first the insert chains of every track in the
    // batch (in parallel when multi-core processing is on), then, in the fixed
    // order, everything that sums into other channels. With multi-core off a
    // batch is one track, which is the plain serial order.
    let parallel = crate::parallel::is_active();
    let mut jobs = std::mem::take(&mut runtime.audio_graph.chain_jobs);
    let pass1_indices = std::mem::take(&mut runtime.audio_graph.pass1_source_indices);
    // Reserved on the control thread for every track; this only grows a
    // hand-built graph that skipped the planner.
    jobs.clear();
    jobs.reserve(pass1_indices.len());
    // Source tracks never feed each other within a block (sends and outputs
    // only reach routing tracks; loopback reads the previous block), so with
    // multi-core on the whole pass is one batch.
    let mut cursor = 0;
    while cursor < pass1_indices.len() {
        jobs.clear();
        while cursor < pass1_indices.len() && (parallel || jobs.is_empty()) {
            let track_index = pass1_indices[cursor];
            cursor += 1;
            let source_active = runtime
                .audio_graph
                .active_source_mask
                .get(track_index)
                .copied()
                .unwrap_or(true)
                || runtime.tracks[track_index].monitor_enabled
                || !runtime.tracks[track_index].midi_block_events.is_empty();
            if !source_active {
                continue;
            }
            let silenced = source_output_silenced(runtime, track_index, block_beat);
            if silenced && !compensation_delay_running(runtime, track_index) {
                if hosts_instrument(&runtime.tracks[track_index]) {
                    jobs.push(ChainJob {
                        track_index,
                        audible: false,
                    });
                }
                continue;
            }
            if !silenced
                && callback_debug_enabled()
                && !runtime.tracks[track_index].inserts.is_empty()
                && !runtime.tracks[track_index].callback_clip_route_log_done
            {
                runtime.tracks[track_index].callback_clip_route_log_done = true;
                let track_id = runtime.tracks[track_index].id.clone();
                let block_start = base_sample;
                let block_end = base_sample.saturating_add(frames as u64);
                let input_peak_l = runtime.tracks[track_index].block_l[..frames]
                    .iter()
                    .fold(0.0f32, |peak, sample| peak.max(sample.abs()));
                let input_peak_r = runtime.tracks[track_index].block_r[..frames]
                    .iter()
                    .fold(0.0f32, |peak, sample| peak.max(sample.abs()));
                let mut clip_count = 0usize;
                let mut overlapping = 0usize;
                let mut first_clip = String::from("none");
                for clip in runtime
                    .clips
                    .iter()
                    .filter(|clip| clip.track_id == track_id)
                {
                    let clip_start = clip.start_sample;
                    let clip_end = clip.start_sample.saturating_add(clip.duration_samples);
                    let overlaps = block_end > clip_start && block_start < clip_end;
                    if clip_count == 0 {
                        first_clip = format!(
                            "{} range={}..{} offset={:.3}s gain={:.3} read_rate={:.3} stretch={:.3} backend={:?} overlaps={}",
                            clip.id,
                            clip_start,
                            clip_end,
                            clip.offset_seconds,
                            clip.gain,
                            clip.source_read_rate,
                            clip.effective_time_ratio,
                            clip.stretch_backend,
                            overlaps
                        );
                    }
                    clip_count += 1;
                    if overlaps {
                        overlapping += 1;
                    }
                }
                eprintln!(
                    "[SphereAudio callback] clipRoute track={} block={}..{} clips={} overlapping={} preInsertPeakL={:.6} preInsertPeakR={:.6} firstClip={}",
                    track_id,
                    block_start,
                    block_end,
                    clip_count,
                    overlapping,
                    input_peak_l,
                    input_peak_r,
                    first_clip
                );
            }
            jobs.push(ChainJob {
                track_index,
                audible: true,
            });
        }
        run_track_chains(&mut runtime.tracks, &jobs, frames, transport);
        for job in &jobs {
            let track_index = job.track_index;
            if !job.audible {
                let track = &mut runtime.tracks[track_index];
                track.block_l[..frames].fill(0.0);
                track.block_r[..frames].fill(0.0);
                continue;
            }
            let emit = !source_output_silenced(runtime, track_index, block_beat);
            finish_track_block(
                runtime,
                track_index,
                frames,
                output,
                channels,
                block_beat,
                emit,
            );
            if let Some(tap) = track_taps
                .as_deref_mut()
                .and_then(|taps| taps.get_mut(track_index))
            {
                for frame in 0..frames {
                    tap[frame * 2] = runtime.tracks[track_index].block_l[frame];
                    tap[frame * 2 + 1] = runtime.tracks[track_index].block_r[frame];
                }
            }
            // Audio Jam publish tap: this track's post-fader signal, the same one
            // an export would capture. Atomics into a preallocated ring, and one
            // `Option` check for every track that is not being shared.
            publish_track_to_jam(runtime, track_index, frames, jam);
            stage_track_to_render_capture(runtime, track_index, frames);
            // Track loopback tap: the same post-fader signal, kept for whichever
            // audio track reads this one as its input on the next callback. One
            // `bool` check for every track nobody is listening to, which is almost
            // all of them.
            capture_track_loopback(runtime, track_index, frames);
        }
    }
    runtime.audio_graph.pass1_source_indices = pass1_indices;

    // ── Pass 2: routing tracks (bus / return / group) ───────────────────
    // Input = the accumulated send receive buffer. Process inserts → fader and
    // sum to the master output. Solo is ignored for routing tracks so soloing
    // a *source* track still lets its send reach the return. Order comes from
    // the precomputed topological sort in `RuntimeAudioGraph`.
    //
    // A batch here is a run of consecutive routing tracks none of which feeds
    // another: a bus summing into a group waits for the bus's batch to finish.
    let pass2_indices = std::mem::take(&mut runtime.audio_graph.pass2_routing_indices);
    let mut child_channels_summed = 0usize;
    let mut cursor = 0;
    while cursor < pass2_indices.len() {
        jobs.clear();
        while cursor < pass2_indices.len() && (parallel || jobs.is_empty()) {
            let track_index = pass2_indices[cursor];
            if jobs
                .iter()
                .any(|job| feeds_track(&runtime.tracks, job.track_index, track_index))
            {
                break;
            }
            cursor += 1;
            // A silenced bus still has to run when it is delay-compensated, so
            // its ring keeps the signal it will emit on unmute. VSTi multi-out
            // strips are the routing tracks that also obey solo.
            if routing_output_silenced(runtime, track_index, block_beat)
                && !compensation_delay_running(runtime, track_index)
            {
                continue;
            }
            {
                let track = &mut runtime.tracks[track_index];
                track.block_l[..frames].copy_from_slice(&track.recv_l[..frames]);
                track.block_r[..frames].copy_from_slice(&track.recv_r[..frames]);
            }
            jobs.push(ChainJob {
                track_index,
                audible: true,
            });
        }
        for job in &jobs {
            apply_wdw_bus_inserts(runtime, job.track_index, frames, transport);
        }
        run_track_chains(&mut runtime.tracks, &jobs, frames, transport);
        for job in &jobs {
            let track_index = job.track_index;
            let emit = !routing_output_silenced(runtime, track_index, block_beat);
            finish_track_block(
                runtime,
                track_index,
                frames,
                output,
                channels,
                block_beat,
                emit,
            );
            if let Some(tap) = track_taps
                .as_deref_mut()
                .and_then(|taps| taps.get_mut(track_index))
            {
                for frame in 0..frames {
                    tap[frame * 2] = runtime.tracks[track_index].block_l[frame];
                    tap[frame * 2 + 1] = runtime.tracks[track_index].block_r[frame];
                }
            }
            // Audio Jam publish tap: this track's post-fader signal, the same one
            // an export would capture. Atomics into a preallocated ring, and one
            // `Option` check for every track that is not being shared.
            publish_track_to_jam(runtime, track_index, frames, jam);
            stage_track_to_render_capture(runtime, track_index, frames);
            // Track loopback tap for routing tracks: a bus, group or return
            // (the WDW return included) is a loopback source like any other,
            // and this is the only place its post-fader block exists.
            capture_track_loopback(runtime, track_index, frames);
            if is_vsti_output_child_track_id(&runtime.tracks[track_index].id)
                && (runtime.tracks[track_index]
                    .meter_peak_l
                    .max(runtime.tracks[track_index].meter_peak_r)
                    > 0.0001)
            {
                child_channels_summed = child_channels_summed.saturating_add(1);
            }
        }
    }
    runtime.audio_graph.pass2_routing_indices = pass2_indices;
    runtime.audio_graph.chain_jobs = jobs;

    // ── Spatial bus → device ──
    let discrete_channels = mix_spatial_bus(runtime, frames, output, channels);

    // ── Master bus: apply master track inserts on the summed output ──
    // A discrete surround mix bypasses them: the master chain is stereo, and
    // processing two of its channels and not the rest would split the mix.
    if let Some(m_idx) = master_index.filter(|_| discrete_channels.is_none()) {
        let muted = effective_track_muted(&runtime.tracks[m_idx], block_beat);
        if !muted {
            let master = &mut runtime.tracks[m_idx];
            // Copy summed output into master scratch buffer.
            for i in 0..frames {
                let frame = &output[i * channels..i * channels + channels];
                master.block_l[i] = frame[0];
                master.block_r[i] = frame[1];
            }
            apply_track_chain_block(master, frames, transport);
            // Write back, accumulate master meter, apply preview mode.
            for i in 0..frames {
                let (l, r) =
                    apply_preview_mode(master.block_l[i], master.block_r[i], master.preview_mode);
                master.meter_peak_l = master.meter_peak_l.max(l.abs());
                master.meter_peak_r = master.meter_peak_r.max(r.abs());
                master.meter_sum_sq_l += l * l;
                master.meter_sum_sq_r += r * r;
                let out = &mut output[i * channels..i * channels + channels];
                out[0] = l;
                out[1] = r;
            }
            if let Some(tap) = track_taps.and_then(|taps| taps.get_mut(m_idx)) {
                for frame in 0..frames {
                    tap[frame * 2] = master.block_l[frame];
                    tap[frame * 2 + 1] = master.block_r[frame];
                }
            }
        } else {
            for frame in output.chunks_mut(channels) {
                frame[0] = 0.0;
                frame[1] = 0.0;
            }
        }
    }

    // Final master volume — gain and nothing else. The output stage does not
    // limit or clip: a mix that runs hot leaves here hot, and reads hot on the
    // meter, instead of being quietly reshaped on its way to the device.
    // Playback and offline export use the same block ramp so master automation
    // follows the same envelope.
    // Every channel the mix occupies: the stereo pair, or a discrete
    // surround layout's full width.
    let mix_width = discrete_channels.unwrap_or(2).min(channels);
    if channels == 2 && mix_width == 2 {
        // The common stereo device: one contiguous interleaved run.
        let out = &mut output[..frames * 2];
        if runtime.fader_smoothing {
            let start = runtime.smoothed_master_gain;
            let inc = (master_volume - start) / frames as f32;
            simd::ramp_interleaved_stereo(out, start, inc);
        } else {
            simd::scale(out, master_volume);
        }
        runtime.smoothed_master_gain = master_volume;
    } else if runtime.fader_smoothing {
        let start = runtime.smoothed_master_gain;
        let inc = (master_volume - start) / frames as f32;
        for i in 0..frames {
            let g = start + inc * i as f32;
            let out = &mut output[i * channels..i * channels + channels];
            for sample in &mut out[..mix_width] {
                *sample *= g;
            }
        }
        runtime.smoothed_master_gain = master_volume;
    } else {
        runtime.smoothed_master_gain = master_volume;
        for i in 0..frames {
            let out = &mut output[i * channels..i * channels + channels];
            for sample in &mut out[..mix_width] {
                *sample *= master_volume;
            }
        }
    }
    // Audio Jam multitrack tap. Last, so every shared track — source, bus and
    // master alike — has its post-fader block for this instant, and the whole
    // stream advances on one head.
    publish_multitrack_to_jam(runtime, frames, jam);

    if crate::forensic_trace::forensic_trace_enabled() {
        static MASTER_INPUT_LOG_SEQ: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(0);
        let process_seq = MASTER_INPUT_LOG_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if process_seq.is_multiple_of(30) {
            let mut peak_l = 0.0f32;
            let mut peak_r = 0.0f32;
            for i in 0..frames {
                let frame = &output[i * channels..i * channels + channels];
                peak_l = peak_l.max(frame[0].abs());
                peak_r = peak_r.max(frame[1].abs());
            }
            eprintln!(
                "[MASTER INPUT]\nprocess_seq={process_seq}\nnum_child_channels_summed={child_channels_summed}\nmaster_peak_l={peak_l:.6}\nmaster_peak_r={peak_r:.6}\naudio_device_write_peak_l={peak_l:.6}\naudio_device_write_peak_r={peak_r:.6}"
            );
            if child_channels_summed > 0 && peak_l.max(peak_r) <= 0.0001 {
                eprintln!(
                    "[ROUTING ERROR]\nprocess_seq={process_seq}\nreason=child mixer channels had signal but master/audio device output is zero"
                );
            }
        }
    }

    frames as u64
}

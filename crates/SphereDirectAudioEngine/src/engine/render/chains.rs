//! Insert chains: MIDI scheduling, automation, bypass, WDW buses and per-insert processing.

use super::*;

/// Whether an inaudible track still has to run its chain: it hosts an
/// instrument, which must keep running while its audio is thrown away.
///
/// Mute and solo silence a track's **output**, not its instrument. Skipping the
/// chain outright meant the block's MIDI was never delivered: a note that
/// started while the track was muted (or while another track was soloed) was
/// dropped for good, and a note already sounding froze mid-voice instead of
/// playing on underneath. Running the chain and clearing the block before
/// sends, fader, meters, and routing keeps the notes moving with the transport
/// while nothing reaches the mix — so lifting the mute or solo drops back in
/// mid-phrase instead of restarting or staying silent.
///
/// Tracks with no instrument route hold no note state, so they stay skipped and
/// cost nothing — unless a compensation or send delay is running, in which case
/// the pass keeps rendering them and [`finish_track_block`] advances those
/// rings without emitting. The pass queues an inaudible instrument's chain with
/// the others ([`ChainJob::audible`] `false`) and clears its block afterwards.
#[inline]
pub(super) fn hosts_instrument(track: &RuntimeTrack) -> bool {
    track.midi_instrument_insert_ix.is_some()
        || track.soundfont_player.is_some()
        || track.solfege_engine.is_some()
}

/// Run the insert chain of every track in `jobs` for this block.
///
/// With multi-core processing on, the chains are spread over the worker pool
/// and the calling thread; otherwise, or when the pool is busy, they run here
/// in order. Either way each chain touches only its own track — the callers
/// only batch tracks that do not feed each other — so the result is the same.
pub(super) fn run_track_chains(
    tracks: &mut [RuntimeTrack],
    jobs: &[ChainJob],
    frames: usize,
    transport: RuntimeTransportContext,
) {
    struct Batch {
        tracks: *mut RuntimeTrack,
        len: usize,
        jobs: *const ChainJob,
        frames: usize,
        transport: RuntimeTransportContext,
    }

    unsafe fn run(ctx: *const (), item: usize) {
        // SAFETY: `ctx` is the `Batch` below, alive until `dispatch` returns;
        // `item < jobs.len()` and every job names a distinct track, so each
        // call has the only reference to its track.
        let batch = unsafe { &*(ctx as *const Batch) };
        let index = unsafe { (*batch.jobs.add(item)).track_index };
        if index < batch.len {
            let track = unsafe { &mut *batch.tracks.add(index) };
            apply_track_chain_block(track, batch.frames, batch.transport);
        }
    }

    debug_assert!(
        jobs.iter()
            .enumerate()
            .all(|(i, a)| jobs[..i].iter().all(|b| b.track_index != a.track_index)),
        "a track may appear in a chain batch only once"
    );
    let batch = Batch {
        tracks: tracks.as_mut_ptr(),
        len: tracks.len(),
        jobs: jobs.as_ptr(),
        frames,
        transport,
    };
    // SAFETY: the tracks are distinct (asserted above, guaranteed by the
    // callers), `RuntimeTrack` is `Send` (checked below), and `batch` outlives
    // the call.
    let ran =
        unsafe { crate::parallel::dispatch(jobs.len(), &batch as *const Batch as *const (), run) };
    if !ran {
        for job in jobs {
            if let Some(track) = tracks.get_mut(job.track_index) {
                apply_track_chain_block(track, frames, transport);
            }
        }
    }
}

// A chain batch moves `&mut RuntimeTrack` to worker threads.
const _: fn() = || {
    fn assert_send<T: Send>() {}
    assert_send::<RuntimeTrack>();
    assert_send::<RuntimeTransportContext>();
};

/// Schedule one device callback's MIDI events, splitting only the scheduler
/// range at loop boundaries. The graph/bridge render still runs once for the
/// full device block; offsets are absolute within that callback.
///
/// Returns `Some(loop_start)` when the block ended exactly on a loop boundary
/// and the caller should reset MIDI after rendering so the next callback starts
/// from the loop start.
pub fn schedule_midi_render_block(
    runtime: &mut RuntimeProject,
    base_sample: u64,
    frames: u64,
    loop_bounds: Option<crate::transport::LoopBounds>,
) -> Option<u64> {
    if frames == 0 {
        return None;
    }
    let mut segment_sample = crate::transport::normalize_loop_position(base_sample, loop_bounds);
    let mut remaining = frames;
    let mut callback_offset = 0u64;
    let mut end_reset = None;
    while remaining > 0 {
        let segment_frames = crate::transport::segment_frames_until_loop_wrap(
            segment_sample,
            remaining,
            loop_bounds,
        );
        runtime.schedule_midi_block_with_offset(
            segment_sample,
            segment_frames,
            callback_offset.min(u32::MAX as u64) as u32,
        );
        callback_offset = callback_offset.saturating_add(segment_frames);
        remaining -= segment_frames;
        let (next_sample, wrapped) =
            crate::transport::advance_loop_position(segment_sample, segment_frames, loop_bounds);
        if wrapped {
            if remaining > 0 {
                runtime.loop_wrap_midi_playback(
                    next_sample,
                    callback_offset.min(u32::MAX as u64) as u32,
                );
            } else {
                // The wrap falls on this block's end. Its note-offs go into
                // *this* block, on its last sample: released after rendering,
                // they sat in a list the next callback clears before anything
                // reads it, so an in-process instrument never heard them and
                // every pass left its notes stuck, piling voices up until the
                // next chord could not sound in full.
                runtime.loop_wrap_midi_playback(
                    next_sample,
                    callback_offset.saturating_sub(1).min(u32::MAX as u64) as u32,
                );
                end_reset = Some(next_sample);
            }
        }
        segment_sample = next_sample;
    }
    end_reset
}

#[inline]
pub fn is_master_output(output: &str) -> bool {
    output.is_empty() || output == "master" || output == "none"
}

#[inline]
pub fn apply_track_chain_at_beat(
    mut l: f32,
    mut r: f32,
    track: &mut RuntimeTrack,
    beat: f64,
) -> (f32, f32) {
    if !track.inserts.is_empty() && !track.callback_insert_log_done {
        track.callback_insert_log_done = true;
        if callback_debug_enabled() {
            eprintln!(
                "[SphereAudio callback] track={} inserts={}",
                track.id,
                track.inserts.len()
            );
        }
    }
    for insert in &mut track.inserts {
        let processed = apply_insert(l, r, insert);
        l = processed.0;
        r = processed.1;
    }
    let automation = track.automation_values_at_beat(beat);
    let volume = automation.volume.unwrap_or(track.volume);
    let pan = automation.pan.unwrap_or(track.pan);
    let (pan_l, pan_r) = pan_gains(pan);
    (l * volume * pan_l, r * volume * pan_r)
}

/// Evaluate this track's resolved plugin-parameter automation lanes at `beat`
/// and push changed normalized values to the matching inserts.
///
/// Realtime-safe: `plugin_param_automation` is pre-resolved off the audio
/// thread, so this does no allocation, no string parsing, and no id lookups.
/// Each binding holds the last value it emitted and only pushes on change, so
/// the lock-free param ring is not flooded with identical values every block.
/// Bridged inserts use the wait-free `push_param` ring; in-process VST3 inserts
/// queue the value via `set_param`.
#[inline]
pub(super) fn apply_plugin_param_automation(track: &mut RuntimeTrack, beat: f64) {
    if track.plugin_param_automation.is_empty() {
        return;
    }
    // Disjoint field borrows: bindings (mut) + lanes (read) + inserts (mut).
    let crate::runtime::RuntimeTrack {
        automation_lanes,
        inserts,
        plugin_param_automation,
        ..
    } = track;
    for binding in plugin_param_automation.iter_mut() {
        let Some(lane) = automation_lanes.get(binding.lane_ix) else {
            continue;
        };
        let Some(value) = lane.evaluate_normalized(beat) else {
            continue;
        };
        let value = value.clamp(0.0, 1.0);
        if (value - binding.last_value).abs() <= crate::runtime::PLUGIN_PARAM_AUTOMATION_EPS {
            continue;
        }
        let Some(insert) = inserts.get_mut(binding.insert_ix) else {
            continue;
        };
        // Recorded as sent only once it was: a value dropped here (bypassed
        // insert, full ring, no sink yet) is tried again next block rather
        // than lost until the lane happens to move.
        if !insert.active() {
            continue;
        }
        let sent = match insert.kind_tag {
            crate::runtime::RuntimeInsertKind::ExternalBridge => insert
                .bridge_sink
                .as_ref()
                .is_some_and(|sink| sink.push_param(binding.param_id, value, 0)),
            _ => match insert.vst3.as_mut() {
                Some(vst3) => {
                    vst3.set_param(binding.param_id, value as f64);
                    true
                }
                None => false,
            },
        };
        if sent {
            binding.last_value = value;
        }
    }
}

/// Set each insert's [`RuntimeInsert::automation_bypassed`] from its bypass
/// lane at `beat`, or clear it when stopped (`None`). Index lookups only.
#[inline]
pub(crate) fn apply_insert_bypass_automation(track: &mut RuntimeTrack, beat: Option<f64>) {
    let crate::runtime::RuntimeTrack {
        automation_lanes,
        inserts,
        ..
    } = track;
    for insert in inserts.iter_mut() {
        let Some(lane_ix) = insert.bypass_lane else {
            continue;
        };
        insert.automation_bypass = beat
            .and_then(|beat| automation_lanes.get(lane_ix)?.evaluate_normalized(beat))
            .map(|value| value >= 0.5);
    }
}

/// Apply every insert on a channel strip, including external-bridge inserts on
/// the master bus. Each bridge insert uses its build/command-time cached
/// `bridge_sink` (no per-block `HashMap<String, _>` lookup).
pub fn apply_track_chain_block(
    track: &mut RuntimeTrack,
    frames: usize,
    transport: RuntimeTransportContext,
) {
    if !track.inserts.is_empty() && !track.callback_insert_log_done {
        track.callback_insert_log_done = true;
        if callback_debug_enabled() {
            eprintln!(
                "[SphereAudio callback] track={} inserts={} blockFrames={}",
                track.id,
                track.inserts.len(),
                frames
            );
        }
    }
    // Sample plugin-parameter automation for this block and push the resolved
    // normalized values to the matching inserts before they process, so the
    // bridged host applies them on the same block it renders. Realtime-safe:
    // pre-resolved bindings, ring push / set_param only, no allocation. Only
    // while playing so manual edits / the plugin editor own the value when the
    // transport is stopped.
    //
    // Read at the beat of the audio entering the chain, not the block's: a
    // bus is fed audio its sources' plug-ins have already delayed.
    let chain_beat = track.automation_clock.chain_beat(transport.ppq_position);
    if transport.playing {
        apply_plugin_param_automation(track, chain_beat);
    }
    // Insert bypass automation, the same way: read while playing, and the
    // insert's own switch stands while stopped.
    apply_insert_bypass_automation(track, transport.playing.then_some(chain_beat));

    render_ara_block(track, frames, transport);
    render_soundfont_instrument_block(track, frames);
    render_solfege_instrument_block(track, frames);

    let instrument_ix = track.midi_instrument_insert_ix;
    let midi_events = &track.midi_block_events;
    // Resolved with the routing indices, never searched here.
    let wdw_live = track.wdw_return_index.is_some();
    for (ix, insert) in track.inserts.iter_mut().enumerate() {
        // Wet/dry/wet inserts stay owned by this track (params, host, editor)
        // and run later, on the split's return. Skipping them here is what
        // takes them out of the main chain -- only while that return is live.
        // With the split gone (its send off or removed, its return deleted)
        // they stay in this chain: a plug-in left on the W bus keeps playing
        // instead of going silent.
        if wdw_live && insert.signal_bus == crate::runtime::SIGNAL_BUS_WDW {
            continue;
        }
        let midi = instrument_ix
            .filter(|&i| i == ix)
            .map(|_| midi_events.as_slice());
        process_chain_insert(
            &mut track.block_l[..frames],
            &mut track.block_r[..frames],
            insert,
            ix,
            midi,
            transport,
        );
    }
    silence_nonfinite_block(&mut track.block_l[..frames], &mut track.block_r[..frames]);
}

/// Run one insert on `block_*`. The chain and the wet/dry/wet return share
/// this so a plug-in moved onto the return still goes through the same host.
pub(super) fn process_chain_insert(
    block_l: &mut [f32],
    block_r: &mut [f32],
    insert: &mut RuntimeInsert,
    ix: usize,
    midi: Option<&[crate::vst3_processor::Vst3MidiEvent]>,
    transport: RuntimeTransportContext,
) {
    // What this one insert costs, so a plug-in's own editor can say what it
    // is spending. Two clock reads per insert per block, no allocation and
    // no lock -- the same instrument the callback already applies to itself.
    let started = std::time::Instant::now();
    if insert.kind_tag == crate::runtime::RuntimeInsertKind::ExternalBridge {
        // Arc clone (refcount bump only) so the sink can be borrowed
        // alongside the &mut insert.
        let bridge_sink = insert.bridge_sink.clone();
        apply_external_bridge_insert_block(
            block_l,
            block_r,
            insert,
            midi,
            bridge_sink.as_deref(),
            ix,
            transport,
        );
    } else {
        apply_insert_block(block_l, block_r, insert, midi, transport);
    }
    publish_insert_cpu(insert, started.elapsed());
    // Audioguard: an insert writing NaN / infinity is silenced at the
    // insert, so the next one in the chain never sees it, and bypassed
    // when it keeps doing it.
    if insert.active() && crate::audioguard::process_level().protects() {
        insert.guard.check_output(block_l, block_r);
    }
}

/// The track whose wet/dry/wet inserts run on `return_index`. A scan of a
/// resolved index per track, no allocation and no id lookup: the same
/// [`RuntimeTrack::wdw_return_index`] that takes them out of that track's
/// own chain, so an insert runs in exactly one place.
pub(super) fn wdw_source_index(tracks: &[RuntimeTrack], return_index: usize) -> Option<usize> {
    tracks
        .iter()
        .position(|track| track.wdw_return_index == Some(return_index))
}

/// Run the source track's wet/dry/wet inserts on the return's block.
///
/// Called after the return has copied its receive buffer and before its own
/// inserts, on the audio thread, one return at a time. The source track is
/// finished for this block, so borrowing its inserts here does not alias a
/// chain that is still running.
pub(super) fn apply_wdw_bus_inserts(
    runtime: &mut RuntimeProject,
    return_index: usize,
    frames: usize,
    transport: RuntimeTransportContext,
) {
    let Some(src) = wdw_source_index(&runtime.tracks, return_index) else {
        return;
    };
    if src == return_index || src >= runtime.tracks.len() {
        return;
    }
    let (src_track, ret) = two_mut(&mut runtime.tracks, src, return_index);
    let frames = frames.min(ret.block_l.len()).min(ret.block_r.len());
    if frames == 0 {
        return;
    }
    for (ix, insert) in src_track.inserts.iter_mut().enumerate() {
        if insert.signal_bus != crate::runtime::SIGNAL_BUS_WDW {
            continue;
        }
        process_chain_insert(
            &mut ret.block_l[..frames],
            &mut ret.block_r[..frames],
            insert,
            ix,
            None,
            transport,
        );
    }
    silence_nonfinite_block(&mut ret.block_l[..frames], &mut ret.block_r[..frames]);
}

/// Replace a block holding NaN or infinity with silence, at the channel that
/// produced it.
///
/// One such sample from a plug-in or instrument poisons every sum it reaches:
/// the bus it feeds becomes NaN, then the master, and the device plays NaN as
/// silence — the whole project goes quiet, with the meters reading empty
/// because `max` skips NaN. Soloing any *other* channel cuts the bad one out of
/// the mix and the sound comes back, and un-soloing puts it back in and the
/// project goes silent again. Clearing it here costs that one channel one
/// block instead. Realtime-safe: a scan and, rarely, two slice fills.
#[inline]
pub(crate) fn silence_nonfinite_block(block_l: &mut [f32], block_r: &mut [f32]) -> bool {
    let finite = simd::all_finite(block_l) && simd::all_finite(block_r);
    if !finite {
        block_l.fill(0.0);
        block_r.fill(0.0);
    }
    !finite
}

/// Fold one insert's block time into its smoothed meter.
///
/// A single block is far too jumpy to read -- a plug-in that allocates on one
/// block and coasts on the next would swing the readout wildly -- so this is an
/// exponential average with a short attack. Store-only from the audio thread;
/// the control side reads the same atomic.
#[inline]
pub(super) fn publish_insert_cpu(
    insert: &crate::runtime::RuntimeInsert,
    elapsed: std::time::Duration,
) {
    use std::sync::atomic::Ordering;
    const SMOOTHING: f32 = 0.15;
    let micros = elapsed.as_micros().min(u32::MAX as u128) as f32;
    let previous = insert.cpu_us.load(Ordering::Relaxed) as f32;
    let smoothed = previous + (micros - previous) * SMOOTHING;
    insert
        .cpu_us
        .store(smoothed.round().max(0.0) as u32, Ordering::Relaxed);
}

#[inline]
pub fn apply_insert(l: f32, r: f32, insert: &mut RuntimeInsert) -> (f32, f32) {
    if insert.kind_tag == crate::runtime::RuntimeInsertKind::NativePlugin {
        if !insert.active() {
            if !insert.callback_process_log_done {
                insert.callback_process_log_done = true;
                // params lookup only inside the once-per-insert log branch.
                let format = insert
                    .params
                    .get("format")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                eprintln!(
                    "[SphereAudio callback] insert={} format={} bypass=true beforePeakL={:.6} beforePeakR={:.6} afterPeakL={:.6} afterPeakR={:.6}",
                    insert.id,
                    format,
                    l.abs(),
                    r.abs(),
                    l.abs(),
                    r.abs()
                );
            }
            return (l, r);
        }
        if let Some(vst3) = insert.vst3.as_mut() {
            let processed = vst3.process_stereo_sample(l, r);
            let (out_l, out_r) = processed.unwrap_or((l, r));
            if !insert.callback_process_log_done {
                insert.callback_process_log_done = true;
                let format = insert
                    .params
                    .get("format")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                eprintln!(
                    "[SphereAudio callback] insert={} format={} processorHandle=0x{:x} bypass=false processOk={} beforePeakL={:.6} beforePeakR={:.6} afterPeakL={:.6} afterPeakR={:.6}",
                    insert.id,
                    format,
                    vst3.handle_value(),
                    processed.is_some(),
                    l.abs(),
                    r.abs(),
                    out_l.abs(),
                    out_r.abs()
                );
            }
            return (out_l, out_r);
        }
        if !insert.callback_process_log_done {
            insert.callback_process_log_done = true;
            let format = insert
                .params
                .get("format")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            eprintln!(
                "[SphereAudio callback] insert={} format={} processorHandle=0x0 bypass=false processOk=false beforePeakL={:.6} beforePeakR={:.6} afterPeakL={:.6} afterPeakR={:.6}",
                insert.id,
                format,
                l.abs(),
                r.abs(),
                l.abs(),
                r.abs()
            );
        }
        return (l, r);
    }

    let plugin_id = canonical_plugin_id(&insert.kind);
    process_stereo_sample(
        plugin_id,
        insert.enabled,
        &insert.params,
        &mut insert.dsp,
        l,
        r,
    )
}

pub fn apply_insert_block(
    block_l: &mut [f32],
    block_r: &mut [f32],
    insert: &mut RuntimeInsert,
    midi_events: Option<&[crate::vst3_processor::Vst3MidiEvent]>,
    transport: RuntimeTransportContext,
) {
    if block_l.is_empty() || block_r.is_empty() {
        return;
    }
    if insert.kind_tag != crate::runtime::RuntimeInsertKind::NativePlugin {
        for i in 0..block_l.len().min(block_r.len()) {
            let (l, r) = apply_insert(block_l[i], block_r[i], insert);
            block_l[i] = l;
            block_r[i] = r;
        }
        return;
    }

    // Diagnostic-only: peak folds feed the once-per-insert process log and the
    // silent-block counter; skipped entirely once that log has fired so the
    // steady-state path stays branch + DSP only. The params "format" lookup
    // happens only inside the log branches.
    let diag = !insert.callback_process_log_done;
    let (before_peak_l, before_peak_r) = if diag {
        (
            block_l
                .iter()
                .fold(0.0f32, |peak, sample| peak.max(sample.abs())),
            block_r
                .iter()
                .fold(0.0f32, |peak, sample| peak.max(sample.abs())),
        )
    } else {
        (0.0, 0.0)
    };

    if !insert.active() {
        if !insert.callback_process_log_done {
            insert.callback_process_log_done = true;
            let format = insert
                .params
                .get("format")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            eprintln!(
                "[SphereAudio callback] insert={} format={} bypass=true blockFrames={} beforePeakL={:.6} beforePeakR={:.6} afterPeakL={:.6} afterPeakR={:.6}",
                insert.id,
                format,
                block_l.len().min(block_r.len()),
                before_peak_l,
                before_peak_r,
                before_peak_l,
                before_peak_r
            );
        }
        return;
    }

    let Some(vst3) = insert.vst3.as_mut() else {
        if !insert.callback_process_log_done {
            insert.callback_process_log_done = true;
            let format = insert
                .params
                .get("format")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            eprintln!(
                "[SphereAudio callback] insert={} format={} processorHandle=0x0 bypass=false processOk=false blockFrames={} beforePeakL={:.6} beforePeakR={:.6} afterPeakL={:.6} afterPeakR={:.6}",
                insert.id,
                format,
                block_l.len().min(block_r.len()),
                before_peak_l,
                before_peak_r,
                before_peak_l,
                before_peak_r
            );
        }
        return;
    };

    // Guard: if the underlying C++ processor was destroyed (e.g., Arc dropped
    // on another thread racing with this callback), bypass and log once.
    if !vst3.is_processor_valid() {
        if !insert.callback_process_log_done {
            insert.callback_process_log_done = true;
            let format = insert
                .params
                .get("format")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            eprintln!(
                "[SphereAudio callback] insert={} format={} processorHandle=0x{:x} INVALID/DESTROYED bypass=true — insert bypassed to prevent use-after-free",
                insert.id,
                format,
                vst3.handle_value()
            );
        }
        return;
    }

    let frames = block_l.len().min(block_r.len());
    if insert.scratch_l.len() < frames {
        insert.scratch_l.resize(frames, 0.0);
        insert.scratch_r.resize(frames, 0.0);
    }
    insert.scratch_l[..frames].fill(0.0);
    insert.scratch_r[..frames].fill(0.0);

    // Real transport ProcessContext for this block, immediately before the
    // plugin processes it (same thread, no race with process()).
    vst3.set_process_context(&transport);

    let handle = vst3.handle_value();
    let process_ok = if let Some(events) = midi_events.filter(|e| !e.is_empty()) {
        vst3.process_stereo_block_with_midi(
            &block_l[..frames],
            &block_r[..frames],
            &mut insert.scratch_l[..frames],
            &mut insert.scratch_r[..frames],
            events,
        )
    } else {
        vst3.process_stereo_block(
            &block_l[..frames],
            &block_r[..frames],
            &mut insert.scratch_l[..frames],
            &mut insert.scratch_r[..frames],
        )
    };
    if process_ok {
        block_l[..frames].copy_from_slice(&insert.scratch_l[..frames]);
        block_r[..frames].copy_from_slice(&insert.scratch_r[..frames]);
    }

    if diag && before_peak_l <= 0.000001 && before_peak_r <= 0.000001 {
        insert.silent_process_blocks = insert.silent_process_blocks.saturating_add(1);
    }

    if diag
        && (before_peak_l > 0.000001
            || before_peak_r > 0.000001
            || insert.silent_process_blocks >= 200)
    {
        insert.callback_process_log_done = true;
        let format = insert
            .params
            .get("format")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        let after_peak_l = block_l[..frames]
            .iter()
            .fold(0.0f32, |peak, sample| peak.max(sample.abs()));
        let after_peak_r = block_r[..frames]
            .iter()
            .fold(0.0f32, |peak, sample| peak.max(sample.abs()));
        eprintln!(
            "[SphereAudio callback] insert={} format={} processorHandle=0x{:x} bypass=false processOk={} blockFrames={} silentBlocks={} beforePeakL={:.6} beforePeakR={:.6} afterPeakL={:.6} afterPeakR={:.6}",
            insert.id,
            format,
            handle,
            process_ok,
            frames,
            insert.silent_process_blocks,
            before_peak_l,
            before_peak_r,
            after_peak_l,
            after_peak_r
        );
    }
}

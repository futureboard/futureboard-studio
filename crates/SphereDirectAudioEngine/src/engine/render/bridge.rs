//! Bridged (out-of-process) inserts: MIDI to the sink and the bridged block.

use super::*;

pub(super) fn push_vst3_midi_to_sink(
    sink: &dyn crate::plugin_bridge::PluginBridgeSink,
    events: &[crate::vst3_processor::Vst3MidiEvent],
    instance_id: &str,
) {
    let verbose = crate::runtime::midi_verbose_enabled();
    for ev in events {
        crate::runtime::push_vst3_midi_event_to_sink(sink, ev, instance_id, verbose);
    }
}

/// Apply a bridged insert's freshly-read output to the track block, honoring the
/// realtime **bypass policy** when the host produced no fresh block (`got == 0`,
/// e.g. its service thread is stalled behind a plugin load or editor open):
///
/// * an **effect** replaces the dry block only when the host delivered a **full**
///   block (`got == frames`). A partial wet|dry splice clicks and reads as a
///   stutter / doubled image in both live playback and offline WAV/MP3 bounce —
///   so a short read is treated like a miss and dry passes through;
/// * an **instrument** adds only the `0..got` frames it actually received, so a
///   not-ready instrument contributes silence.
///
/// Returns the `(L, R)` output peaks for diagnostics. Wait-free: only slice
/// copies/adds over `got` frames, no allocation, no locks — safe to call from
/// the audio callback. `got` must be `<=` every slice length (the caller sizes
/// `scratch` to `frames` and reads at most `frames`).
#[inline]
pub(crate) fn apply_bridge_insert_output(
    is_effect: bool,
    got: usize,
    block_l: &mut [f32],
    block_r: &mut [f32],
    scratch_l: &[f32],
    scratch_r: &[f32],
) -> (f32, f32) {
    let need_peaks = crate::forensic_trace::engine_midi_verbose_enabled()
        || plugin_restore_debug_enabled()
        || crate::runtime::midi_verbose_enabled();
    let mut peak_l = 0.0f32;
    let mut peak_r = 0.0f32;
    let frames = block_l.len().min(block_r.len());
    // Effects: all-or-nothing. Partial replace left a wet head and dry tail.
    let effect_frames = if is_effect && got >= frames && frames > 0 {
        frames
    } else {
        0
    };
    if is_effect && effect_frames > 0 {
        block_l[..effect_frames].copy_from_slice(&scratch_l[..effect_frames]);
        block_r[..effect_frames].copy_from_slice(&scratch_r[..effect_frames]);
        if need_peaks {
            peak_l = scratch_l[..effect_frames]
                .iter()
                .fold(0.0f32, |p, s| p.max(s.abs()));
            peak_r = scratch_r[..effect_frames]
                .iter()
                .fold(0.0f32, |p, s| p.max(s.abs()));
        }
    } else if !is_effect {
        let n = got.min(frames);
        for i in 0..n {
            block_l[i] += scratch_l[i];
            block_r[i] += scratch_r[i];
            if need_peaks {
                peak_l = peak_l.max(scratch_l[i].abs());
                peak_r = peak_r.max(scratch_r[i].abs());
            }
        }
    }
    (peak_l, peak_r)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_external_bridge_insert_block(
    block_l: &mut [f32],
    block_r: &mut [f32],
    insert: &mut RuntimeInsert,
    midi_events: Option<&[crate::vst3_processor::Vst3MidiEvent]>,
    bridge_sink: Option<&dyn crate::plugin_bridge::PluginBridgeSink>,
    slot_index: usize,
    transport: RuntimeTransportContext,
) {
    let frames = block_l.len().min(block_r.len());
    if frames == 0 || !insert.active() {
        return;
    }
    let Some(sink) = bridge_sink else {
        if plugin_restore_debug_enabled() && insert.bridge_missed_blocks == 0 {
            eprintln!(
                "[AudioGraph] processing insert skipped instance={} reason=no_bridge_sink",
                insert.id
            );
        }
        return;
    };
    if plugin_restore_debug_enabled() && insert.bridge_missed_blocks == 0 {
        let input_peak = block_l[..frames]
            .iter()
            .chain(block_r[..frames].iter())
            .fold(0.0f32, |p, s| p.max(s.abs()));
        eprintln!(
            "[BridgeProcess] track=<chain> slot={slot_index} instance={} input_peak={input_peak:.6}",
            insert.id
        );
    }

    // Clip MIDI for bridged plugins is pushed in schedule_midi_block. Preview
    // MIDI is pushed in drain_commands. Non-bridge inserts still use midi_block_events.
    if let Some(events) = midi_events.filter(|e| !e.is_empty()) {
        let verbose = crate::runtime::midi_verbose_enabled();
        if verbose {
            eprintln!(
                "[plugin-dsp-midi-write] instance={} events={}",
                insert.id,
                events.len()
            );
        }
        push_vst3_midi_to_sink(sink, events, &insert.id);
    }

    // `params["role"]` resolved at build time — no params-map read per block.
    let is_effect = insert.bridge_is_effect;

    if insert.scratch_l.len() < frames || insert.scratch_r.len() < frames {
        // Scratches are pre-sized at graph build / `resolve_bridge_sinks`.
        // Growing here would allocate on the audio thread — skip this insert.
        return;
    }

    // One-block handshake ownership (critical):
    //   1. read previous wet  — proves the host finished the last cycle and
    //      released `audio_in` (it copies input before publishing `done_seq`);
    //   2. write next dry     — only now is overwriting `audio_in` safe;
    //   3. apply wet to block;
    //   4. request next       — host may read `audio_in` after this.
    // The old write→read→request order raced the host on the single `audio_in`
    // buffer. Offline export almost always lost that race (near-zero gap between
    // request and the next write), tearing wet blocks into stutter / overlap.
    // Live hits the same bug whenever the host overruns the device period.
    let got = if insert.vsti_output_children.is_empty() {
        // Default single-track path (unchanged): fold the selected channels into
        // this track's stereo.
        sink.read_output_for_channels(
            &mut insert.scratch_l[..frames],
            &mut insert.scratch_r[..frames],
            frames,
            &insert.bridge_enabled_output_channels,
        )
    } else {
        // Multi-out path: ONE freshness-guarded read of the whole plugin block
        // into `scratch_multi` (a second sink read would see the guard return
        // 0). Every active VST3 output bus routes to an explicit child mixer
        // track, including bus 0. With explicit children present the parent
        // instrument track does not receive a fallback downmix.
        let channels = (sink.plugin_output_channels() as usize)
            .clamp(1, crate::runtime::MAX_VSTI_OUTPUT_CHANNELS as usize);
        let needed = frames * channels;
        // `resolve_bridge_sinks` reserves `scratch_multi`'s *capacity* on the
        // control thread for every insert with `vsti_output_children`, so
        // `resize` below only ever adjusts `.len()` within that reserved
        // capacity — it does not allocate on the audio thread.
        // `scatter_vsti_output_children` recovers the channel stride from
        // `scratch_multi.len() / frames`, so the resize must still land at
        // exactly `needed`; skip the read entirely (leaving length/capacity
        // untouched) in the otherwise-unreached case where capacity falls
        // short, rather than growing here.
        if insert.scratch_multi.capacity() < needed {
            0
        } else {
            insert.scratch_multi.resize(needed, 0.0);
            let (got, _) =
                sink.read_output_multichannel(&mut insert.scratch_multi[..needed], frames);
            // A plug-in whose main output has no child strip of its own (a
            // multi-out built-in giving strips only to the extra outputs its
            // pads use) keeps that output on the parent track; otherwise the
            // parent gets nothing, and bus 0 plays on its child like the rest.
            let main_on_parent = !insert
                .vsti_output_children
                .iter()
                .any(|child| child.bus_index == 0);
            if main_on_parent {
                let right = if channels > 1 { 1 } else { 0 };
                for i in 0..got {
                    insert.scratch_l[i] = insert.scratch_multi[i * channels];
                    insert.scratch_r[i] = insert.scratch_multi[i * channels + right];
                }
            } else {
                insert.scratch_l[..got].fill(0.0);
                insert.scratch_r[..got].fill(0.0);
            }
            got
        }
    };

    // A miss can mean the host still owns the previous request's input buffer.
    // In that case bypass this callback but leave the in-flight block untouched;
    // overwriting or publishing a second request would race the host's raw read.
    let can_publish_request = got > 0 || !sink.request_in_flight();
    if is_effect && can_publish_request {
        // Current dry (pre-apply). Host will process this after `request_block`.
        sink.write_input(&block_l[..frames], &block_r[..frames], frames);
    }

    // Multi-out diagnostic: which plugin output channels actually carry signal
    // this block, and which the engine is folding. Tells separate-out silence
    // apart from "the plugin only writes its main bus" without a debugger.
    // `FUTUREBOARD_PLUGIN_BRIDGE_DEBUG=1`, throttled to ~every 256 blocks.
    if bridge_debug_enabled() && got > 0 {
        static MULTIOUT_PEAK_LOG: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(0);
        if MULTIOUT_PEAK_LOG
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .is_multiple_of(256)
        {
            let mut ch_peaks = [0.0f32; 32];
            for (i, peak) in ch_peaks.iter_mut().enumerate() {
                *peak = sink.output_channel_peak((i as u8) + 1);
            }
            eprintln!(
                "[Bridge] multiout instance={} enabled_channels={:?} channel_peaks={:?}",
                insert.id, insert.bridge_enabled_output_channels, ch_peaks
            );
        }
    }

    // Missed-deadline accounting: `read_output` returns 0 when the host has
    // not produced a fresh block (its service thread is stalled behind an
    // editor open/close or a plugin load). The block below then bypasses the
    // insert (effect keeps the dry signal, instrument contributes silence) —
    // stale output is never replayed. A few misses are normal on startup and
    // when resuming from pause, so only log once a stall is established.
    const BRIDGE_MISS_LOG_THRESHOLD: u32 = 8;
    insert.guard.note_bridge_block(got == 0);
    if got == 0 {
        insert.bridge_missed_blocks = insert.bridge_missed_blocks.saturating_add(1);
        if plugin_restore_debug_enabled()
            && (insert.bridge_missed_blocks == 1
                || insert.bridge_missed_blocks == BRIDGE_MISS_LOG_THRESHOLD
                || insert.bridge_missed_blocks.is_multiple_of(1024))
        {
            eprintln!(
                "[Bridge] missed/bypass instance_id={} missed_blocks={}",
                insert.id, insert.bridge_missed_blocks
            );
        }
        // Stall accounting stays in `bridge_missed_blocks`; stderr from the
        // audio callback only exists under the bridge debug flag (realtime
        // rules — stdio can block the callback).
        if bridge_debug_enabled()
            && (insert.bridge_missed_blocks == BRIDGE_MISS_LOG_THRESHOLD
                || insert.bridge_missed_blocks.is_multiple_of(1024))
        {
            if is_effect {
                eprintln!(
                    "[AudioEngine] plugin missed deadline; bypassing to dry signal instance={} missed_blocks={}",
                    insert.id, insert.bridge_missed_blocks
                );
            } else {
                eprintln!(
                    "[VSTi] missed bridge block; output silence instance={} missed_blocks={}",
                    insert.id, insert.bridge_missed_blocks
                );
            }
        }
    } else {
        if plugin_restore_debug_enabled() {
            let out_peak = insert.scratch_l[..got]
                .iter()
                .chain(insert.scratch_r[..got].iter())
                .fold(0.0f32, |p, s| p.max(s.abs()));
            eprintln!(
                "[BridgeProcess] track=<chain> slot={slot_index} instance={} fresh output_peak={out_peak:.6} frames={got}",
                insert.id
            );
        }
        if bridge_debug_enabled() && insert.bridge_missed_blocks >= BRIDGE_MISS_LOG_THRESHOLD {
            if is_effect {
                eprintln!(
                    "[AudioEngine] plugin host recovered instance={} missed_blocks={}",
                    insert.id, insert.bridge_missed_blocks
                );
            } else {
                eprintln!(
                    "[VSTi] recovered after missed blocks={} instance={}",
                    insert.bridge_missed_blocks, insert.id
                );
            }
        }
        insert.bridge_missed_blocks = 0;
    }

    let (out_peak_l, out_peak_r) = apply_bridge_insert_output(
        is_effect,
        got,
        block_l,
        block_r,
        &insert.scratch_l,
        &insert.scratch_r,
    );
    if crate::forensic_trace::engine_midi_verbose_enabled()
        && (out_peak_l > 0.0001 || out_peak_r > 0.0001)
    {
        eprintln!(
            "[SphereAudio] external_bridge output_peak_l={:.6} output_peak_r={:.6}",
            out_peak_l, out_peak_r
        );
        eprintln!(
            "[plugin-host-dsp] response_peak_l={:.6} response_peak_r={:.6}",
            out_peak_l, out_peak_r
        );
    }

    if can_publish_request {
        // Publish the real transport ProcessContext for this block before kicking
        // the host, so the bridged plugin sees true tempo/position/playing instead
        // of the old hardcoded stub. Wait-free atomic stores.
        sink.set_transport(&transport);

        // Drive the host DSP handshake: MIDI was already pushed to the shared ring.
        if plugin_restore_debug_enabled() && insert.bridge_missed_blocks == 0 {
            eprintln!(
                "[Bridge] request block instance_id={} frames={frames}",
                insert.id
            );
        }
        sink.request_block(frames as u32);
    }
}

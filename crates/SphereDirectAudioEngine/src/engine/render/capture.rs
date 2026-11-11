//! Render capture, jam publishing and loopback taps.

use super::*;

/// Stage one track's post-fader block for a realtime render, the same signal
/// an offline stem takes. One `Option` check when nothing is being rendered.
#[inline]
pub(super) fn stage_track_to_render_capture(
    runtime: &RuntimeProject,
    track_index: usize,
    frames: usize,
) {
    let Some(capture) = runtime.render_capture.as_deref() else {
        return;
    };
    let track = &runtime.tracks[track_index];
    capture.stage_track(track_index, &track.block_l, &track.block_r, frames);
}

/// Feed one track's post-fader block to its Audio Jam publish slot.
///
/// Realtime-safe by construction: no allocation, no lock, no key lookup — the
/// slot index was resolved on the control thread and shipped as a command.
#[inline]
pub(super) fn publish_track_to_jam(
    runtime: &RuntimeProject,
    track_index: usize,
    frames: usize,
    jam: Option<&crate::jam_bus::JamAudioBus>,
) {
    let Some(bus) = jam else {
        return;
    };
    let Some(track) = runtime.tracks.get(track_index) else {
        return;
    };
    let Some(slot) = track.jam_publish_slot else {
        return;
    };
    let Some(slot) = bus.publish(slot as usize) else {
        return;
    };
    slot.write_planar(&track.block_l, &track.block_r, frames, runtime.sample_rate);
}

/// Keep this track's post-fader block for a track that loops it back as input.
///
/// Realtime-safe in the steady state: two `copy_from_slice`s into buffers that
/// were grown on the first block a source published and never shrink. A track
/// nobody reads pays one `bool`.
#[inline]
pub(super) fn capture_track_loopback(
    runtime: &mut RuntimeProject,
    track_index: usize,
    frames: usize,
) {
    let Some(track) = runtime.tracks.get_mut(track_index) else {
        return;
    };
    if !track.loopback_publish {
        // Release the buffers of a source nobody reads any more. Done here
        // rather than on the control thread because the control thread does not
        // own the graph the callback is holding, and a `Vec` that is already
        // empty costs nothing to clear.
        if !track.loopback_out_l.is_empty() {
            track.loopback_out_l.clear();
            track.loopback_out_r.clear();
        }
        return;
    }
    if track.loopback_out_l.len() < frames {
        track.loopback_out_l.resize(frames, 0.0);
        track.loopback_out_r.resize(frames, 0.0);
    }
    let frames = frames.min(track.block_l.len()).min(track.block_r.len());
    track.loopback_out_l[..frames].copy_from_slice(&track.block_l[..frames]);
    track.loopback_out_r[..frames].copy_from_slice(&track.block_r[..frames]);
}

/// Assemble one block of the Audio Jam multitrack stream.
///
/// Every shared track has already rendered its post-fader block by the time
/// this runs, so the whole stream is staged and published in one pass at the
/// end of the graph rather than a pair at a time as each track finishes. That
/// ordering is the point: one head advance means one capture base and one
/// sequence for the whole arrangement, which is what lets a receiving Studio
/// drop the take onto its timeline as a single aligned multitrack recording.
///
/// Realtime-safe: bounds-checked indexing and atomic stores into rings that
/// were allocated when the bus was built. Pairs nobody staged are zeroed by
/// [`crate::jam_bus::JamPublishSlot::commit`], so a track that stopped sharing
/// leaves silence rather than a repeating block.
#[inline]
pub(super) fn publish_multitrack_to_jam(
    runtime: &RuntimeProject,
    frames: usize,
    jam: Option<&crate::jam_bus::JamAudioBus>,
) {
    let Some(bus) = jam else {
        return;
    };
    let Some(slot) = bus.multitrack_publish() else {
        return;
    };
    for track in runtime.tracks.iter() {
        let Some(pair) = track.jam_multitrack_pair else {
            continue;
        };
        slot.stage_pair(pair as usize, &track.block_l, &track.block_r, frames);
    }
    slot.commit(frames, runtime.sample_rate);
}

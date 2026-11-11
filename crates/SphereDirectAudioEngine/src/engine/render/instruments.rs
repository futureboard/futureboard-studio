//! Instrument tracks: ARA, SoundFont and Solfege renderers.

use super::*;

/// Mix this track's ARA playback renderers into `block_*`.
///
/// An ARA renderer generates the audio for the playback regions assigned to it,
/// reading the source samples out of band through the host callbacks in
/// `SphereAraHost`. It is therefore fed silence rather than the track signal,
/// and the clips it owns were skipped by the clip loop.
///
/// Realtime rules: the renderer list and its buffers are built on the control
/// thread; this only fills, calls, and sums. Buffers grow once to the largest
/// block seen, exactly like `block_*` and `recv_*` above.
pub(super) fn render_ara_block(
    track: &mut RuntimeTrack,
    frames: usize,
    transport: RuntimeTransportContext,
) {
    if frames == 0 || track.ara_renderers.is_empty() {
        return;
    }
    if track.ara_l.len() < frames {
        track.ara_l.resize(frames, 0.0);
        track.ara_r.resize(frames, 0.0);
        track.ara_silence.resize(frames, 0.0);
    }
    // The silence buffer is only ever read, so it is filled once here rather
    // than trusted to stay zero after a plug-in wrote through its input pointer.
    track.ara_silence[..frames].fill(0.0);

    for index in 0..track.ara_renderers.len() {
        // Split the borrow: the renderer is mutated while the track's own
        // buffers are written.
        let (renderers, out_l, out_r, silence, block_l, block_r) = (
            &mut track.ara_renderers,
            &mut track.ara_l,
            &mut track.ara_r,
            &track.ara_silence,
            &mut track.block_l,
            &mut track.block_r,
        );
        let renderer = &mut renderers[index];
        if !renderer.processor.is_processor_valid() {
            continue;
        }
        out_l[..frames].fill(0.0);
        out_r[..frames].fill(0.0);
        // ARA rendering is transport-driven: the plug-in maps the process
        // context's project position onto its playback regions, so the context
        // has to be published before every block, not only while playing.
        renderer.processor.set_process_context(&transport);
        if !renderer.processor.process_stereo_block(
            &silence[..frames],
            &silence[..frames],
            &mut out_l[..frames],
            &mut out_r[..frames],
        ) {
            continue;
        }
        for frame in 0..frames {
            block_l[frame] += out_l[frame];
            block_r[frame] += out_r[frame];
        }
    }
}

pub(super) fn render_soundfont_instrument_block(track: &mut RuntimeTrack, frames: usize) {
    if frames == 0
        || track
            .soundfont_player
            .as_ref()
            .and_then(|soundfont| soundfont.player.as_ref())
            .is_none()
    {
        // A track with no player sounds nothing; leaving a stale count here
        // would keep the meter lit after the instrument was removed.
        track.active_voices = 0;
        return;
    }

    let mut cursor = 0usize;
    for index in 0..track.midi_block_events.len() {
        let event = track.midi_block_events[index];
        let offset = (event.sample_offset as usize).min(frames);
        if offset > cursor {
            render_soundfont_segment(track, cursor, offset);
            cursor = offset;
        }
        if let Some(player) = track
            .soundfont_player
            .as_mut()
            .and_then(|soundfont| soundfont.player.as_mut())
        {
            match event.kind {
                1 => {
                    let velocity = (event.velocity.clamp(0.0, 1.0) * 127.0).round() as u8;
                    let _ = player.note_on(
                        event.channel.min(15),
                        event.pitch.min(127),
                        velocity.max(1),
                    );
                }
                0 => {
                    let _ = player.note_off(event.channel.min(15), event.pitch.min(127));
                }
                2 => {
                    // `pitch` carries the controller number here, including the
                    // engine's out-of-band numbers for pitch bend and channel
                    // pressure — clamping it to 127 would turn a bend lane into
                    // a random CC, so the player does the translation.
                    let value = (event.velocity.clamp(0.0, 1.0) * 127.0).round() as u8;
                    let _ = player.controller(event.channel.min(15), event.pitch, value);
                }
                _ => {}
            }
        }
    }
    if cursor < frames {
        render_soundfont_segment(track, cursor, frames);
    }
    // Read after the last segment so the count describes the block that was
    // just produced. One field read on the synth — no allocation, no lock.
    track.active_voices = track
        .soundfont_player
        .as_ref()
        .and_then(|soundfont| soundfont.player.as_ref())
        .map(|player| player.active_voice_count() as u32)
        .unwrap_or(0);
}

pub(super) fn render_soundfont_segment(track: &mut RuntimeTrack, start: usize, end: usize) {
    if end <= start {
        return;
    }
    let Some(soundfont) = track.soundfont_player.as_mut() else {
        return;
    };
    let Some(player) = soundfont.player.as_mut() else {
        return;
    };
    let len = end - start;
    track.soundfont_l[..len].fill(0.0);
    track.soundfont_r[..len].fill(0.0);
    if player
        .render(&mut track.soundfont_l[..len], &mut track.soundfont_r[..len])
        .is_err()
    {
        return;
    }
    for i in 0..len {
        track.block_l[start + i] += track.soundfont_l[i];
        track.block_r[start + i] += track.soundfont_r[i];
    }
}

pub(super) fn render_solfege_instrument_block(track: &mut RuntimeTrack, frames: usize) {
    if frames == 0 || track.solfege_engine.is_none() {
        return;
    }
    // Two ordered streams — note/controller events and continuous-pitch
    // targets — merged into one forward walk so the block is split at every
    // event, in time order, whichever list it came from. Both lists are already
    // sorted by sample offset (the scheduler emits them from one sorted event
    // list), so this is a merge, not a sort: no allocation, no comparison
    // sort, nothing that would be unsafe in the audio callback.
    let midi_count = track.midi_block_events.len();
    let pitch_count = track.solfege_pitch_events.len();
    let articulation_count = track.solfege_articulation_events.len();
    let mut midi_ix = 0usize;
    let mut pitch_ix = 0usize;
    let mut articulation_ix = 0usize;
    let mut cursor = 0usize;
    while midi_ix < midi_count || pitch_ix < pitch_count || articulation_ix < articulation_count {
        let midi_at = (midi_ix < midi_count)
            .then(|| (track.midi_block_events[midi_ix].sample_offset as usize).min(frames));
        let pitch_at = (pitch_ix < pitch_count)
            .then(|| (track.solfege_pitch_events[pitch_ix].0 as usize).min(frames));
        let articulation_at = (articulation_ix < articulation_count)
            .then(|| (track.solfege_articulation_events[articulation_ix].0 as usize).min(frames));

        // Tie-breaking at one offset decides whether a note is heard correctly:
        // an articulation chooses the recording, so it must arrive *before* the
        // note-on that resolves it; a pitch target addresses a voice, so it must
        // arrive *after*. Hence articulation, then MIDI, then pitch.
        let earliest = [articulation_at, midi_at, pitch_at]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(frames);
        if earliest > cursor {
            render_solfege_segment(track, cursor, earliest);
            cursor = earliest;
        }

        if articulation_at == Some(earliest) {
            let (_, note_id, articulation) = track.solfege_articulation_events[articulation_ix];
            articulation_ix += 1;
            if let Some(solfege) = track.solfege_engine.as_mut() {
                solfege.handle_articulation_event(note_id, articulation);
            }
        } else if midi_at == Some(earliest) {
            let event = track.midi_block_events[midi_ix];
            midi_ix += 1;
            if let Some(solfege) = track.solfege_engine.as_mut() {
                solfege.handle_midi_event(event);
            }
        } else {
            let (_, note_id, hz) = track.solfege_pitch_events[pitch_ix];
            pitch_ix += 1;
            if let Some(solfege) = track.solfege_engine.as_mut() {
                solfege.handle_pitch_event(note_id, hz);
            }
        }
    }
    if cursor < frames {
        render_solfege_segment(track, cursor, frames);
    }
}

pub(super) fn render_solfege_segment(track: &mut RuntimeTrack, start: usize, end: usize) {
    if end <= start {
        return;
    }
    let len = end - start;
    // Both scratch planes are preallocated to the callback capacity. Keeping
    // the bank stereo avoids folding phase-opposed source material to mono,
    // which can sound thin or dirty on sustained notes.
    let output_l = &mut track.soundfont_l[..len];
    let output_r = &mut track.soundfont_r[..len];
    output_l.fill(0.0);
    output_r.fill(0.0);
    if let Some(solfege) = track.solfege_engine.as_mut() {
        solfege.render_segment_stereo(output_l, output_r);
    }
    for i in 0..len {
        track.block_l[start + i] += output_l[i];
        track.block_r[start + i] += output_r[i];
    }
}

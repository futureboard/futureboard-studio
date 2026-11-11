//! Runtime MIDI: events, sources, clips, tracks, block buffers and sending to plug-ins.

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeMidiEventKind {
    NoteOff,
    NoteOn,
    /// MIDI controller change (CC / pitch-bend / aftertouch). Uses
    /// `cc_number` / `cc_value` rather than `pitch` / `velocity`.
    ControlChange,
    /// Continuous sounding pitch for a note already playing, in Hz
    /// (`pitch_hz`). Distinct from a pitch-bend `ControlChange`: it is an
    /// absolute frequency for one voice rather than a channel-wide bend over a
    /// semitone range, which is what a drawn pitch curve and a bowed-string
    /// glide actually are. Only continuous-pitch instruments consume it; a
    /// plain MIDI instrument ignores it and hears the notes unchanged.
    Pitch,
    /// Which recorded articulation the *next* note-on should play, in
    /// `cc_number`. Sorted to land immediately before its own note-on, because
    /// a sampled instrument chooses its source at note-on and an articulation
    /// arriving afterwards would apply to the following note.
    Articulation,
}

#[derive(Debug, Clone)]
pub struct RuntimeMidiEvent {
    /// Absolute project sample at which the event fires (precomputed from the
    /// snapshot BPM at build time, mirroring how audio clips resolve to
    /// samples — keeps scheduling deterministic and lock-free in the callback).
    pub sample: u64,
    /// Absolute project beat. This is the canonical musical position; `sample`
    /// is rebuilt from it when the project tempo changes.
    pub beat: f64,
    pub kind: RuntimeMidiEventKind,
    pub pitch: u8,
    pub velocity: u8,
    pub channel: u8,
    pub note_id: u64,
    /// For `ControlChange`: VST3 controller number (`0..=127` CC, `128`
    /// aftertouch, `129` pitch bend). Unused for note events.
    pub cc_number: u16,
    /// For `ControlChange`: normalized value `0.0..=1.0`. Unused for notes.
    pub cc_value: f32,
    /// For `Pitch`: the sounding frequency in Hz. Unused by every other kind.
    pub pitch_hz: f32,
    /// Index into [`RuntimeMidiTrack::sources`]: the MIDI track this event
    /// comes from when that track plays this instrument. `None` for the
    /// instrument track's own clips.
    pub source: Option<u16>,
}

/// A MIDI track that plays another track's instrument (MIDI Out → Instrument).
/// Its notes are merged into the instrument's schedule; kept here so its own
/// mute and solo still apply.
#[derive(Debug, Clone, Default)]
pub struct RuntimeMidiSource {
    pub track_id: String,
    /// Resolved like [`RuntimeMidiTrack::track_index`]; `None` when missing.
    pub track_index: Option<usize>,
}

/// Structural per-clip representation, retained for logging / future reuse.
#[derive(Debug, Clone)]
pub struct RuntimeMidiClip {
    pub id: String,
    pub track_id: String,
    pub start_beat: f64,
    pub end_beat: f64,
    pub events: Vec<RuntimeMidiEvent>,
}

/// Build-time MPE transport state shared by every MIDI clip that resolves to
/// the same output track. MPE member channels are a track-wide resource: a
/// fresh allocator per clip lets overlapping clips reuse the same channel and
/// cross-wire their note expression.
pub(super) struct MpeBuildState {
    pub(super) allocator: MpeChannelAllocator,
    pub(super) active: Vec<(u64, f64)>,
    pub(super) member_channel_start: u8,
    pub(super) member_channel_end: u8,
}

impl MpeBuildState {
    pub(super) fn new(zone: MpeZone) -> Self {
        Self {
            allocator: MpeChannelAllocator::new(zone.member_channel_start, zone.member_channel_end),
            active: Vec::new(),
            member_channel_start: zone.member_channel_start,
            member_channel_end: zone.member_channel_end,
        }
    }
}

/// Per-track merged + sorted event list with a playback cursor and active-note
/// set. Scheduling reads `events[cursor..]` each block; `cursor` is repositioned
/// on seek/play. `active` prevents stuck notes across stop/seek.
#[derive(Debug, Clone, Default)]
pub struct RuntimeMidiTrack {
    pub track_id: String,
    /// [`Self::track_id`] resolved to a track index at build time
    /// ([`RuntimeProject::resolve_indices`]); `None` when the track is missing.
    pub track_index: Option<usize>,
    pub events: Vec<RuntimeMidiEvent>,
    pub cursor: usize,
    /// Currently-sounding (channel, pitch) pairs since the last NoteOn.
    pub active: Vec<(u8, u8)>,
    /// UI preview notes currently held independently of transport playback.
    pub preview_active: Vec<(u8, u8)>,
    /// MIDI tracks routed into this one ([`RuntimeMidiEvent::source`]).
    pub sources: Vec<RuntimeMidiSource>,
    /// A block-event list the control thread preallocated for this schedule's
    /// densest block ([`midi_block_event_capacity`]), for a running graph whose
    /// track list is smaller: installing the schedule swaps it in
    /// ([`RuntimeProject::adopt_midi_block_event_buffers`]) so the callback
    /// never grows the list. Empty when the base capacity is enough.
    pub block_events_spare: Vec<Vst3MidiEvent>,
}

/// Every in-process instrument track's per-block event list holds at least
/// this many events.
pub(super) const MIDI_BLOCK_EVENT_BASE_CAPACITY: usize = 256;

/// Block the per-block MIDI lists are sized for: the longest block the graph's
/// track buffers take. Sizing for it rather than for the open device keeps the
/// lists right for every path that schedules blocks — any device buffer, a
/// device that hands over a longer block than it announced, offline export —
/// at the cost of a few kilobytes on dense tracks only.
pub(super) const MIDI_BLOCK_WINDOW: usize = DEFAULT_AUDIO_BLOCK_CAPACITY;

/// Events one MIDI track can queue for its in-process instrument in a single
/// render block of `block_frames`: the densest `block_frames` window of
/// `events` (sorted by sample) twice over — a loop-wrap block carries the end
/// of one pass and the start of the next — plus a note-off for every sounding
/// note (128, the `active` capacity), the stop/seek controller panic (four
/// per channel) and a little room for live preview and program changes.
pub(crate) fn midi_block_event_capacity(events: &[RuntimeMidiEvent], block_frames: usize) -> usize {
    let window = block_frames.clamp(1, DEFAULT_AUDIO_BLOCK_CAPACITY) as u64;
    let mut densest = 0usize;
    let mut start = 0usize;
    for end in 0..events.len() {
        while events[end].sample.saturating_sub(events[start].sample) >= window {
            start += 1;
        }
        densest = densest.max(end - start + 1);
    }
    (2 * densest + 128 + 16 * 4 + 32).max(MIDI_BLOCK_EVENT_BASE_CAPACITY)
}

/// Times the callback had to grow an instrument track's per-block MIDI list
/// because a graph reached it without the list being sized off the audio
/// thread. Every path that hands a graph or a schedule to the callback sizes
/// it, so this should stay zero; it is logged off the audio thread by
/// [`crate::engine::EngineInner::log_callback_diagnostics`] so a path that
/// does not shows up instead of hiding.
pub(super) static MIDI_BLOCK_LIST_GROWTHS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Total [`MIDI_BLOCK_LIST_GROWTHS`].
pub fn midi_block_list_growths() -> u64 {
    MIDI_BLOCK_LIST_GROWTHS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Queue `event` for a track's in-process instrument this block. Within the
/// capacity reserved off the audio thread this never allocates. A full list
/// still takes the event — losing a note-off would leave a note stuck — but
/// the growth is counted and reported, never silent.
#[inline]
pub(super) fn push_midi_block_event(events: &mut Vec<Vst3MidiEvent>, event: Vst3MidiEvent) -> bool {
    if events.len() == events.capacity() {
        MIDI_BLOCK_LIST_GROWTHS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    events.push(event);
    true
}

/// A project's MIDI schedule on its own: what a note edit changes, swapped
/// into the running graph by [`RuntimeProject::replace_midi_schedule`].
#[derive(Debug, Clone, Default)]
pub struct RuntimeMidiData {
    pub midi_clips: Vec<RuntimeMidiClip>,
    pub midi_tracks: Vec<RuntimeMidiTrack>,
}

impl RuntimeMidiData {
    /// Control thread: give each track the block-event list its densest block
    /// needs, when that is more than the base capacity every graph track
    /// starts with. See [`RuntimeMidiTrack::block_events_spare`].
    pub fn prepare_block_event_buffers(&mut self) {
        for track in &mut self.midi_tracks {
            let need = midi_block_event_capacity(&track.events, MIDI_BLOCK_WINDOW);
            track.block_events_spare = if need > MIDI_BLOCK_EVENT_BASE_CAPACITY {
                Vec::with_capacity(need)
            } else {
                Vec::new()
            };
        }
    }

    /// Control thread: re-time this schedule for `tempo_map` (see
    /// [`retime_midi_lists`]).
    pub fn retime(&mut self, tempo_map: &RuntimeTempoMapSnapshot, sample_rate: u32) {
        retime_midi_lists(
            &mut self.midi_clips,
            &mut self.midi_tracks,
            tempo_map,
            sample_rate.max(1) as f64,
        );
    }

    /// Control thread: a copy ready to be swapped into a running graph. A
    /// plain clone keeps no spare capacity, and the callback pushes into each
    /// track's sounding-note lists, so they get the capacity
    /// [`build_midi_runtime`] gives them.
    pub fn realtime_copy(&self) -> Self {
        let mut copy = self.clone();
        for track in &mut copy.midi_tracks {
            track.active.reserve(128);
            track.preview_active.reserve(128);
        }
        copy
    }
}

/// Whether `events` (the unplayed part of a schedule) end the note on
/// `channel`/`pitch` before starting it again.
pub(super) fn note_off_is_pending(events: &[RuntimeMidiEvent], channel: u8, pitch: u8) -> bool {
    for event in events {
        if event.channel != channel || event.pitch != pitch {
            continue;
        }
        match event.kind {
            RuntimeMidiEventKind::NoteOff => return true,
            RuntimeMidiEventKind::NoteOn => return false,
            _ => {}
        }
    }
    false
}

/// One track's program selection on one MIDI channel, bound to the track
/// whose instrument receives it.
#[derive(Debug, Clone)]
pub struct RuntimeMidiProgram {
    /// The receiving track, for carrying `sent_to` across a graph swap.
    pub track_id: String,
    pub track_index: usize,
    pub program: crate::types::EngineMidiProgram,
    /// The destination it was last sent to ([`midi_program_route`]), so a
    /// rebuilt graph or a restarted transport does not send it again — a
    /// program change makes some instruments reload — while a plug-in
    /// reloaded behind a new sink does get it.
    pub sent_to: Option<usize>,
}

/// Where a track's program selection would go this block, as an identity to
/// compare with the last send: the bridged instrument's sink, or the track's
/// own in-process instrument. `None` when there is nothing to receive it yet
/// — a bridged instrument still loading has no sink, and sending into the
/// in-process list for it would mark the program sent to nobody.
pub(super) fn midi_program_route(track: &RuntimeTrack) -> Option<usize> {
    if let Some(insert) = track
        .midi_instrument_insert_ix
        .and_then(|ix| track.inserts.get(ix))
    {
        return match (&insert.bridge_sink, insert.kind_tag) {
            (Some(sink), _) => Some(Arc::as_ptr(sink) as *const () as usize),
            (None, RuntimeInsertKind::ExternalBridge) => None,
            (None, _) => Some(1),
        };
    }
    (track.soundfont_player.is_some() || track.solfege_engine.is_some()).then_some(1)
}

pub(super) static MIDI_WRITE_SEQ: AtomicU32 = AtomicU32::new(0);

/// VST3's controller number for a program change (`kCtrlProgramChange`). The
/// engine's MIDI events carry it as a controller change with the program in
/// the value, like aftertouch (128) and pitch bend (129); each destination
/// turns it back into a MIDI program change (`0xC0`).
pub const VST3_CTRL_PROGRAM_CHANGE: u8 = 130;

pub fn push_vst3_midi_event_to_sink(
    sink: &dyn crate::plugin_bridge::PluginBridgeSink,
    ev: &Vst3MidiEvent,
    instance_id: &str,
    verbose: bool,
) {
    let channel = ev.channel & 0x0F;
    let seq = MIDI_WRITE_SEQ.fetch_add(1, Ordering::Relaxed);
    match ev.kind {
        1 => {
            let vel = (ev.velocity.clamp(0.0, 1.0) * 127.0).round() as u8;
            if vel > 0 {
                sink.push_midi(0x90 | channel, ev.pitch, vel, ev.sample_offset);
                if verbose {
                    eprintln!("[plugin-dsp-midi-write] seq={seq} instance={instance_id} events=1");
                    eprintln!(
                        "[plugin-dsp-midi-write] note_on pitch={} offset={} ch={channel}",
                        ev.pitch, ev.sample_offset
                    );
                }
            } else {
                sink.push_midi(0x80 | channel, ev.pitch, 0, ev.sample_offset);
                if verbose {
                    eprintln!("[plugin-dsp-midi-write] seq={seq} instance={instance_id} events=1");
                    eprintln!(
                        "[plugin-dsp-midi-write] note_off pitch={} offset={} ch={channel}",
                        ev.pitch, ev.sample_offset
                    );
                }
            }
        }
        0 => {
            let vel = (ev.velocity.clamp(0.0, 1.0) * 127.0).round() as u8;
            sink.push_midi(0x80 | channel, ev.pitch, vel, ev.sample_offset);
            if verbose {
                eprintln!("[plugin-dsp-midi-write] seq={seq} instance={instance_id} events=1");
                eprintln!(
                    "[plugin-dsp-midi-write] note_off pitch={} offset={} ch={channel}",
                    ev.pitch, ev.sample_offset
                );
            }
        }
        2 => {
            let val = (ev.velocity.clamp(0.0, 1.0) * 127.0).round() as u8;
            // The shared event ABI uses VST3 controller numbers 128/129 for
            // channel pressure/pitch bend, but a bridge sink carries raw MIDI
            // bytes. Do not emit an impossible CC number (128/129): translate
            // these two out-of-band values back to their MIDI status bytes.
            if ev.pitch == 128 {
                sink.push_midi(0xD0 | channel, val, 0, ev.sample_offset);
            } else if ev.pitch == 129 {
                let bend = (ev.velocity.clamp(0.0, 1.0) * 16_383.0).round() as u16;
                sink.push_midi(
                    0xE0 | channel,
                    (bend & 0x7F) as u8,
                    (bend >> 7) as u8,
                    ev.sample_offset,
                );
            } else if ev.pitch == VST3_CTRL_PROGRAM_CHANGE {
                sink.push_midi(0xC0 | channel, val, 0, ev.sample_offset);
            } else {
                sink.push_midi(0xB0 | channel, ev.pitch.min(127), val, ev.sample_offset);
            }
            if verbose {
                let kind = match ev.pitch {
                    128 => "channel_pressure",
                    129 => "pitch_bend",
                    VST3_CTRL_PROGRAM_CHANGE => "program_change",
                    _ => "cc",
                };
                eprintln!(
                    "[plugin-dsp-midi-write] seq={seq} instance={instance_id} events=1 {kind}={} val={val}",
                    ev.pitch,
                );
            }
        }
        _ => {}
    }
}

/// First native VST3 insert that should receive scheduled MIDI for this track.
pub(super) fn find_midi_instrument_insert_ix(
    inserts: &[RuntimeInsert],
    track_type: &str,
) -> Option<usize> {
    inserts.iter().enumerate().find_map(|(ix, insert)| {
        if insert_accepts_midi_events(insert, track_type) {
            Some(ix)
        } else {
            None
        }
    })
}

#[inline]
pub(super) fn insert_accepts_midi_events(insert: &RuntimeInsert, track_type: &str) -> bool {
    if !insert.enabled {
        return false;
    }
    let is_bridge = insert.kind.eq_ignore_ascii_case("external-bridge-plugin");
    if !is_bridge && insert.vst3.is_none() {
        return false;
    }
    // A bridged insert the snapshot already resolved as an effect (registry
    // kind, or any insert on a built-in-instrument track) processes audio; it
    // is never the track's note destination, whatever the track type.
    if is_bridge && insert.bridge_is_effect {
        return false;
    }
    let ty = track_type.to_ascii_lowercase();
    if ty == "instrument" || ty == "midi" {
        return true;
    }
    let cat = insert
        .params
        .get("category")
        .or_else(|| insert.params.get("pluginCategory"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let cat_lc = cat.to_ascii_lowercase();
    if cat_lc.contains("instrument") || cat_lc.contains("synth") {
        return true;
    }
    insert
        .params
        .get("acceptsMidi")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

pub(super) fn push_all_notes_off_for_track(
    project: &mut RuntimeProject,
    track_index: Option<usize>,
    active: &[(u8, u8)],
    sample_offset: u32,
    controllers: bool,
) {
    let Some(ti) = track_index.filter(|&ti| ti < project.tracks.len()) else {
        return;
    };
    let instrument_ix = project.tracks[ti].midi_instrument_insert_ix;
    if instrument_ix.is_none()
        && project.tracks[ti].soundfont_player.is_none()
        && project.tracks[ti].solfege_engine.is_none()
    {
        return;
    }
    let sink = instrument_ix.and_then(|ix| {
        project.tracks[ti]
            .inserts
            .get(ix)
            .and_then(|insert| insert.bridge_sink.clone())
    });
    if let Some(sink) = sink {
        if midi_engine_debug_enabled() {
            eprintln!(
                "[midi-playback] transport_stop panic instance={} old_notes={}",
                instrument_ix
                    .and_then(|ix| project.tracks[ti].inserts.get(ix))
                    .map(|insert| insert.id.as_str())
                    .unwrap_or("soundfont-player"),
                active.len()
            );
        }
        for &(channel, pitch) in active {
            sink.push_midi(0x80 | (channel & 0x0F), pitch, 0, sample_offset);
        }
        if controllers {
            for ch in 0u8..16 {
                sink.push_midi(0xB0 | (ch & 0x0F), 64, 0, sample_offset);
                sink.push_midi(0xB0 | (ch & 0x0F), 123, 0, sample_offset);
                sink.push_midi(0xB0 | (ch & 0x0F), 120, 0, sample_offset);
            }
        }
        return;
    }
    let events = &mut project.tracks[ti].midi_block_events;
    for &(channel, pitch) in active {
        push_midi_block_event(
            events,
            Vst3MidiEvent::note_off(sample_offset, channel, pitch, 0.0),
        );
    }
    if !controllers {
        return;
    }
    for channel in 0..16 {
        for controller in [64, 123, 120, 121] {
            push_midi_block_event(
                events,
                Vst3MidiEvent::control_change(sample_offset, channel, controller, 0.0),
            );
        }
    }
}

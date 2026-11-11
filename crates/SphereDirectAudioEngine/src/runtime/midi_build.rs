//! Building MIDI schedules and the project tempo map.

use super::*;

pub fn build_tempo_map_from_points(
    default_bpm: f64,
    points: &[crate::types::EngineTempoPointSnapshot],
) -> RuntimeTempoMapSnapshot {
    if points.is_empty() {
        RuntimeTempoMapSnapshot::static_tempo(default_bpm)
    } else {
        TempoMap::from_points(
            default_bpm,
            points
                .iter()
                .map(|p| TempoPoint {
                    beat: p.beat,
                    bpm: p.bpm,
                    curve: crate::tempo_map::TempoCurve::from_tag(p.curve),
                    tension: crate::tempo_map::clamp_tension(p.tension),
                })
                .collect(),
        )
        .into_snapshot()
    }
}

pub fn build_project_tempo_map(snapshot: &EngineProjectSnapshot) -> RuntimeTempoMapSnapshot {
    build_tempo_map_from_points(snapshot.bpm, &snapshot.tempo_points)
}

/// Emit the MIDI 1.0 RPN setup that corresponds to the track's selected MPE
/// zone. The values are stored as normalized VST3 controller values in the
/// runtime event, then converted by the bridge at dispatch time. Sending this
/// at the clip boundary makes the selected pitch ranges effective for both
/// realtime playback and offline rendering.
pub(super) fn push_mpe_configuration_events(
    events: &mut Vec<RuntimeMidiEvent>,
    zone: MpeZone,
    beat: f64,
    sample: u64,
) {
    fn push_cc(
        events: &mut Vec<RuntimeMidiEvent>,
        channel: u8,
        controller: u16,
        value: f32,
        beat: f64,
        sample: u64,
    ) {
        events.push(RuntimeMidiEvent {
            sample,
            beat,
            kind: RuntimeMidiEventKind::ControlChange,
            pitch: 0,
            velocity: 0,
            channel,
            note_id: 0,
            cc_number: controller,
            cc_value: value.clamp(0.0, 1.0),
            pitch_hz: 0.0,
            source: None,
        });
    }

    fn push_rpn_range(
        events: &mut Vec<RuntimeMidiEvent>,
        channel: u8,
        range: f32,
        beat: f64,
        sample: u64,
    ) {
        let range = if range.is_finite() {
            range.clamp(0.01, 127.99)
        } else {
            2.0
        };
        let msb = range.floor().clamp(0.0, 127.0) as u8;
        let lsb = ((range.fract() * 100.0).round() as u8).min(99);
        push_cc(events, channel, 101, 0.0, beat, sample);
        push_cc(events, channel, 100, 0.0, beat, sample);
        push_cc(events, channel, 6, f32::from(msb) / 127.0, beat, sample);
        push_cc(events, channel, 38, f32::from(lsb) / 127.0, beat, sample);
    }

    push_rpn_range(
        events,
        zone.manager_channel,
        zone.manager_pitch_range,
        beat,
        sample,
    );
    for channel in zone.member_channels() {
        push_rpn_range(events, channel, zone.member_pitch_range, beat, sample);
    }
    // RPN 0,6 on the manager channel selects the zone's member count. The
    // manager channel itself identifies the zone (channel 1 = Lower, channel
    // 16 = Upper); the RPN LSB is always 6. Sending 0/1 here would select
    // unrelated RPNs and causes hardware/plugins to ignore the member count.
    push_cc(events, zone.manager_channel, 101, 0.0, beat, sample);
    push_cc(events, zone.manager_channel, 100, 6.0 / 127.0, beat, sample);
    push_cc(
        events,
        zone.manager_channel,
        6,
        zone.member_channels().len().min(15) as f32 / 127.0,
        beat,
        sample,
    );
}

/// Makes every key (channel, pitch) sound one note at a time. Two notes of
/// the same pitch that overlap (a chord tone held into the next chord, a
/// legato tail, notes drawn over each other) used to reach the instrument as
/// on, on, off, off: the first note-off then silenced the note still meant to
/// sound, and the key went quiet early. The later note retriggers the key
/// instead (an off right before its on), and only the last note-off of the
/// overlap releases it. Runs when the schedule is built, never on the audio
/// thread; `events` must already be sorted.
pub(super) fn resolve_overlapping_notes(events: Vec<RuntimeMidiEvent>) -> Vec<RuntimeMidiEvent> {
    let mut held: HashMap<(u8, u8), u32> = HashMap::new();
    let mut out = Vec::with_capacity(events.len());
    for event in events {
        let key = (event.channel, event.pitch);
        match event.kind {
            RuntimeMidiEventKind::NoteOn => {
                let count = held.entry(key).or_insert(0);
                if *count > 0 {
                    out.push(RuntimeMidiEvent {
                        kind: RuntimeMidiEventKind::NoteOff,
                        velocity: 0,
                        ..event.clone()
                    });
                }
                *count += 1;
                out.push(event);
            }
            RuntimeMidiEventKind::NoteOff => {
                let count = held.entry(key).or_insert(0);
                *count = count.saturating_sub(1);
                if *count == 0 {
                    out.push(event);
                }
            }
            _ => out.push(event),
        }
    }
    out
}

pub(super) fn sort_midi_events(events: &mut [RuntimeMidiEvent]) {
    events.sort_by(midi_event_order);
}

pub(super) fn midi_event_order(a: &RuntimeMidiEvent, b: &RuntimeMidiEvent) -> std::cmp::Ordering {
    a.sample
        .cmp(&b.sample)
        .then(midi_event_priority(a.kind).cmp(&midi_event_priority(b.kind)))
}

/// Recompute every event's sample from its beat under `tempo_map` and restore
/// the schedule order. Allocation-free, so the callback can use it as the
/// fallback when no re-timed schedule was prepared for it.
///
/// The lists were sorted under a previous map, and beat -> sample is monotonic
/// under every map (tempo is clamped positive), so re-timing can only reorder
/// events that share a sample before or after it. A stable insertion sort
/// therefore costs one pass plus those few local swaps, and never needs the
/// buffer `sort_by` takes.
pub(super) fn retime_midi_lists(
    clips: &mut [RuntimeMidiClip],
    tracks: &mut [RuntimeMidiTrack],
    tempo_map: &RuntimeTempoMapSnapshot,
    sample_rate: f64,
) {
    for clip in clips {
        retime_midi_events(&mut clip.events, tempo_map, sample_rate);
    }
    for track in tracks {
        retime_midi_events(&mut track.events, tempo_map, sample_rate);
    }
}

pub(super) fn retime_midi_events(
    events: &mut [RuntimeMidiEvent],
    tempo_map: &RuntimeTempoMapSnapshot,
    sample_rate: f64,
) {
    for event in events.iter_mut() {
        event.sample = tempo_map.samples_at_beat(event.beat, sample_rate);
    }
    // Stable insertion sort, in place.
    for i in 1..events.len() {
        let mut j = i;
        while j > 0 && midi_event_order(&events[j - 1], &events[j]).is_gt() {
            events.swap(j - 1, j);
            j -= 1;
        }
    }
}

/// Ordering at one sample is part of the transport contract. Expression reset
/// messages must precede a replacement NoteOn, while an old NoteOff must land
/// before both. This also keeps articulation selection ahead of its note.
pub(super) fn midi_event_priority(kind: RuntimeMidiEventKind) -> u8 {
    match kind {
        RuntimeMidiEventKind::NoteOff => 0,
        RuntimeMidiEventKind::ControlChange => 1,
        RuntimeMidiEventKind::Articulation => 2,
        RuntimeMidiEventKind::NoteOn => 3,
        RuntimeMidiEventKind::Pitch => 4,
    }
}

pub(super) fn mpe_voice_release_beat(
    note_end: f64,
    clip_end: f64,
    sustain_changes: &[(f64, bool)],
) -> f64 {
    let sustain_down = sustain_changes
        .iter()
        .rfind(|(beat, _)| *beat <= note_end)
        .is_some_and(|(_, down)| *down);
    if !sustain_down {
        return note_end;
    }
    sustain_changes
        .iter()
        .find(|(beat, down)| *beat > note_end && !*down)
        .map_or(clip_end, |(beat, _)| *beat)
}

/// Apply a note event to the active-note set (NoteOn inserts, NoteOff removes).
#[inline]
pub(super) fn apply_active(active: &mut Vec<(u8, u8)>, ev: &RuntimeMidiEvent) {
    let key = (ev.channel, ev.pitch);
    match ev.kind {
        RuntimeMidiEventKind::NoteOn => {
            if !active.contains(&key) {
                active.push(key);
            }
        }
        RuntimeMidiEventKind::NoteOff => {
            active.retain(|k| *k != key);
        }
        // Controller, continuous-pitch and articulation events shape a note;
        // none of them starts or ends one.
        RuntimeMidiEventKind::ControlChange
        | RuntimeMidiEventKind::Pitch
        | RuntimeMidiEventKind::Articulation => {}
    }
}

/// Convert snapshot MIDI clips into structural [`RuntimeMidiClip`]s and merged
/// per-track [`RuntimeMidiTrack`] schedules. Note starts are clip-relative and
/// converted to absolute project beats/samples here (outside the audio
/// callback). Events are sorted by sample, with NoteOff before NoteOn at the
/// same sample to avoid retrigger glitches / stuck notes.
/// A project's MIDI schedule with no track resolved yet. It needs no graph, so
/// a caller can build it without holding the graph's lock — the part of a note
/// edit that scales with every note in the project — and resolve the tracks
/// afterwards with [`resolve_midi_schedule_tracks`].
pub fn build_unresolved_midi_schedule(
    snapshot: &EngineProjectSnapshot,
    sample_rate: u32,
) -> RuntimeMidiData {
    let tempo_map = build_project_tempo_map(snapshot);
    let (midi_clips, midi_tracks) =
        build_midi_runtime(&snapshot.midi_clips, &tempo_map, sample_rate);
    RuntimeMidiData {
        midi_clips,
        midi_tracks,
    }
}

/// Point each MIDI track of `data` at the graph track of the same id, given
/// the graph's ids in track-index order.
pub fn resolve_midi_schedule_tracks(data: &mut RuntimeMidiData, track_ids: &[String]) {
    for midi_track in &mut data.midi_tracks {
        midi_track.track_index = track_ids.iter().position(|id| *id == midi_track.track_id);
        for source in &mut midi_track.sources {
            source.track_index = track_ids.iter().position(|id| *id == source.track_id);
        }
    }
}

#[cfg(test)]
pub(super) fn shared_midi_clips(
    clips: &[EngineMidiClipSnapshot],
) -> Vec<std::sync::Arc<EngineMidiClipSnapshot>> {
    clips.iter().cloned().map(std::sync::Arc::new).collect()
}

pub(super) fn build_midi_runtime(
    snapshot_clips: &[std::sync::Arc<EngineMidiClipSnapshot>],
    tempo_map: &RuntimeTempoMapSnapshot,
    sample_rate: u32,
) -> (Vec<RuntimeMidiClip>, Vec<RuntimeMidiTrack>) {
    let sr = sample_rate.max(1) as f64;
    let mut clips: Vec<RuntimeMidiClip> = Vec::with_capacity(snapshot_clips.len());
    let mut by_track: HashMap<String, Vec<RuntimeMidiEvent>> = HashMap::new();
    let mut mpe_states: HashMap<String, MpeBuildState> = HashMap::new();
    let mut configured_mpe_zones: HashMap<String, MpeZone> = HashMap::new();
    let mut mpe_track_end_beats: HashMap<String, f64> = HashMap::new();
    let mut sustain_changes_by_track: HashMap<String, Vec<(f64, bool)>> = HashMap::new();
    let mut stolen_note_cutoffs: HashMap<u64, u64> = HashMap::new();
    let mut next_runtime_note_id = 1_u64;
    // Per output track: the MIDI tracks routed into it, in first-seen order.
    let mut sources_by_track: HashMap<String, Vec<RuntimeMidiSource>> = HashMap::new();

    // Sustain is a track-wide controller state. Build one absolute timeline
    // before allocating voices so a pedal change in a neighboring/overlapping
    // clip cannot make an MPE member channel look reusable too early.
    for clip in snapshot_clips {
        let clip_end = clip.start_beat + clip.length_beats.max(0.0);
        mpe_track_end_beats
            .entry(clip.track_id.clone())
            .and_modify(|end| *end = end.max(clip_end))
            .or_insert(clip_end);
        let changes = sustain_changes_by_track
            .entry(clip.track_id.clone())
            .or_default();
        for lane in clip.controllers.iter().filter(|lane| lane.controller == 64) {
            changes.extend(
                lane.points
                    .iter()
                    .map(|point| (clip.start_beat + point.beat.max(0.0), point.value >= 0.5)),
            );
        }
    }
    for changes in sustain_changes_by_track.values_mut() {
        changes.sort_by(|a, b| a.0.total_cmp(&b.0));
    }

    // Allocation is a track-wide transport concern. Process clips in musical
    // order so a note in an earlier overlapping clip occupies a member
    // channel before a later clip asks for one, independent of file order.
    let mut clip_order: Vec<usize> = (0..snapshot_clips.len()).collect();
    clip_order.sort_by(|a, b| {
        snapshot_clips[*a]
            .track_id
            .cmp(&snapshot_clips[*b].track_id)
            .then_with(|| {
                snapshot_clips[*a]
                    .start_beat
                    .total_cmp(&snapshot_clips[*b].start_beat)
            })
            .then_with(|| snapshot_clips[*a].id.cmp(&snapshot_clips[*b].id))
    });

    for clip_index in clip_order {
        let clip = &snapshot_clips[clip_index];
        let has_expression = clip
            .notes
            .iter()
            .any(|note| !note.expression.sanitized().is_empty());
        let mpe_enabled = clip.mpe.should_use_mpe(has_expression);
        let mut events: Vec<RuntimeMidiEvent> =
            Vec::with_capacity(clip.notes.len() * if mpe_enabled { 5 } else { 2 });
        let clip_end_beat = clip.start_beat + clip.length_beats.max(0.0);
        let sustain_changes = sustain_changes_by_track
            .get(&clip.track_id)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let release_horizon = mpe_track_end_beats
            .get(&clip.track_id)
            .copied()
            .unwrap_or(clip_end_beat);
        let mut note_indices: Vec<usize> = clip
            .notes
            .iter()
            .enumerate()
            .filter(|(_, note)| note.length_beats > 0.0)
            .map(|(index, _)| index)
            .collect();
        // Allocation is a transport concern, so assign member channels in
        // musical order rather than depending on the serialized note order.
        note_indices.sort_by(|a, b| {
            clip.notes[*a]
                .start_beat
                .total_cmp(&clip.notes[*b].start_beat)
                .then_with(|| clip.notes[*a].id.cmp(&clip.notes[*b].id))
        });
        if mpe_enabled {
            if let Some(zone) = clip.mpe.zone() {
                let state = mpe_states
                    .entry(clip.track_id.clone())
                    .or_insert_with(|| MpeBuildState::new(zone));
                // Track-level settings normally make every clip agree. If a
                // hand-authored snapshot violates that invariant, reset the
                // allocator at the new zone boundary rather than assigning a
                // channel outside the selected zone.
                if state.member_channel_start != zone.member_channel_start
                    || state.member_channel_end != zone.member_channel_end
                {
                    *state = MpeBuildState::new(zone);
                }
                // Configuration is track-wide. Repeating the RPN burst at
                // every clip boundary can retune a live device mid-note and
                // creates needless controller traffic; resend only when the
                // effective zone actually changes.
                if configured_mpe_zones
                    .get(&clip.track_id)
                    .is_none_or(|configured| *configured != zone)
                {
                    push_mpe_configuration_events(
                        &mut events,
                        zone,
                        clip.start_beat,
                        tempo_map.samples_at_beat(clip.start_beat, sr),
                    );
                    configured_mpe_zones.insert(clip.track_id.clone(), zone);
                }
            }
        }
        let mut deferred_mpe_note_offs = Vec::new();
        for note_index in note_indices {
            let note = &clip.notes[note_index];
            let expression = note.expression.sanitized();
            let runtime_note_id = next_runtime_note_id;
            next_runtime_note_id = next_runtime_note_id.wrapping_add(1).max(1);
            let pitch = note.pitch.min(127);
            let velocity = note.velocity.clamp(1, 127);
            let abs_start = clip.start_beat + note.start_beat.max(0.0);
            let abs_end = abs_start + note.length_beats;
            let on_sample = tempo_map.samples_at_beat(abs_start, sr);
            let off_sample = tempo_map.samples_at_beat(abs_end, sr);
            let release_velocity = expression
                .release_velocity
                .map(|value| (value.clamp(0.0, 1.0) * 127.0).round() as u8)
                .unwrap_or(0);
            let channel = if mpe_enabled {
                let state = mpe_states
                    .get_mut(&clip.track_id)
                    .expect("MPE build state is initialized before note scheduling");
                // Ended voices are reusable before allocation. This is done
                // at graph-build time, never in the audio callback.
                for (note_id, end) in &state.active {
                    if *end <= abs_start {
                        state.allocator.release(*note_id);
                    }
                }
                state.active.retain(|(_, end)| *end > abs_start);
                let allocation = state
                    .allocator
                    .allocate(runtime_note_id, pitch, velocity)
                    .expect("MPE allocator has at least one member channel");
                if let Some(stolen_note) = allocation.stolen_note {
                    stolen_note_cutoffs.insert(stolen_note, on_sample);
                    state.active.retain(|(note_id, _)| *note_id != stolen_note);
                    let stolen_pitch = allocation.stolen_pitch.unwrap_or(pitch);
                    events.push(RuntimeMidiEvent {
                        sample: on_sample,
                        beat: abs_start,
                        kind: RuntimeMidiEventKind::NoteOff,
                        pitch: stolen_pitch,
                        velocity: 0,
                        channel: allocation.channel,
                        note_id: stolen_note,
                        cc_number: 0,
                        cc_value: 0.0,
                        pitch_hz: 0.0,
                        source: None,
                    });
                }
                if allocation.reset_required {
                    // Reset all channel-wide MPE state before the replacement
                    // NoteOn so the previous voice cannot leak its gesture.
                    for (cc_number, cc_value) in [(129_u16, 0.5_f32), (128, 0.0), (74, 0.0)] {
                        events.push(RuntimeMidiEvent {
                            sample: on_sample,
                            beat: abs_start,
                            kind: RuntimeMidiEventKind::ControlChange,
                            pitch: 0,
                            velocity: 0,
                            channel: allocation.channel,
                            note_id: runtime_note_id,
                            cc_number,
                            cc_value,
                            pitch_hz: 0.0,
                            source: None,
                        });
                    }
                }
                for (cc_number, curve) in [
                    (129_u16, &expression.pitch),
                    (128_u16, &expression.pressure),
                    (74_u16, &expression.timbre),
                ] {
                    for point in &curve.points {
                        if !point.position.is_finite()
                            || point.position < 0.0
                            || point.position >= note.length_beats as f32
                        {
                            continue;
                        }
                        let beat = abs_start + point.position as f64;
                        events.push(RuntimeMidiEvent {
                            sample: tempo_map.samples_at_beat(beat, sr),
                            beat,
                            kind: RuntimeMidiEventKind::ControlChange,
                            pitch: 0,
                            velocity: 0,
                            channel: allocation.channel,
                            note_id: runtime_note_id,
                            cc_number,
                            cc_value: if cc_number == 129 {
                                (point.value.clamp(-1.0, 1.0) + 1.0) * 0.5
                            } else {
                                point.value.clamp(0.0, 1.0)
                            },
                            pitch_hz: 0.0,
                            source: None,
                        });
                    }
                }
                // A key release during sustain does not free the member
                // channel. Keep it reserved until the next pedal-up event so
                // a later note cannot inherit its pitch/pressure/timbre state.
                let voice_release =
                    mpe_voice_release_beat(abs_end, release_horizon, sustain_changes);
                state.active.push((runtime_note_id, voice_release));
                allocation.channel
            } else {
                note.channel.min(15)
            };
            if let Some(articulation) = note.articulation {
                events.push(RuntimeMidiEvent {
                    sample: on_sample,
                    beat: abs_start,
                    kind: RuntimeMidiEventKind::Articulation,
                    pitch,
                    velocity: 0,
                    channel,
                    note_id: if mpe_enabled {
                        runtime_note_id
                    } else {
                        note.id
                    },
                    cc_number: articulation,
                    cc_value: 0.0,
                    pitch_hz: 0.0,
                    source: None,
                });
            }
            events.push(RuntimeMidiEvent {
                sample: on_sample,
                beat: abs_start,
                kind: RuntimeMidiEventKind::NoteOn,
                pitch,
                velocity,
                channel,
                note_id: if mpe_enabled {
                    runtime_note_id
                } else {
                    note.id
                },
                cc_number: 0,
                cc_value: 0.0,
                pitch_hz: 0.0,
                source: None,
            });
            let note_off = RuntimeMidiEvent {
                sample: off_sample,
                beat: abs_end,
                kind: RuntimeMidiEventKind::NoteOff,
                pitch,
                velocity: release_velocity,
                channel,
                note_id: if mpe_enabled {
                    runtime_note_id
                } else {
                    note.id
                },
                cc_number: 0,
                cc_value: 0.0,
                pitch_hz: 0.0,
                source: None,
            };
            if mpe_enabled {
                deferred_mpe_note_offs.push((runtime_note_id, note_off));
            } else {
                events.push(note_off);
            }
            // The note's drawn/derived pitch trajectory. Scheduled on the same
            // timeline as its own note events so a seek, a loop or a tempo
            // change moves the curve with the note it belongs to. Points
            // outside `[on, off)` are dropped: a pitch target for a voice that
            // is not sounding would either be ignored or, worse, land on the
            // next note that reuses the voice.
            for point in &note.pitch_points {
                if !point.hz.is_finite() || point.hz <= 0.0 {
                    continue;
                }
                let abs_beat = clip.start_beat + point.beat.max(0.0);
                if abs_beat < abs_start || abs_beat >= abs_end {
                    continue;
                }
                events.push(RuntimeMidiEvent {
                    sample: tempo_map.samples_at_beat(abs_beat, sr),
                    beat: abs_beat,
                    kind: RuntimeMidiEventKind::Pitch,
                    pitch,
                    velocity: 0,
                    channel,
                    note_id: if mpe_enabled {
                        runtime_note_id
                    } else {
                        note.id
                    },
                    cc_number: 0,
                    cc_value: 0.0,
                    pitch_hz: point.hz,
                    source: None,
                });
            }
        }
        for (_note_id, note_off) in deferred_mpe_note_offs {
            events.push(note_off);
        }
        // Controller points → ControlChange events (block-level value).
        for lane in &clip.controllers {
            let channel = lane.channel.min(15);
            for point in &lane.points {
                let abs_beat = clip.start_beat + point.beat.max(0.0);
                let sample = tempo_map.samples_at_beat(abs_beat, sr);
                events.push(RuntimeMidiEvent {
                    sample,
                    beat: abs_beat,
                    kind: RuntimeMidiEventKind::ControlChange,
                    pitch: 0,
                    velocity: 0,
                    channel,
                    note_id: 0,
                    cc_number: lane.controller,
                    cc_value: point.value.clamp(0.0, 1.0),
                    pitch_hz: 0.0,
                    source: None,
                });
            }
        }
        // Sort by sample; NoteOff before NoteOn at the same sample.
        sort_midi_events(&mut events);
        if let Some(source_id) = clip
            .source_track_id
            .as_ref()
            .filter(|id| **id != clip.track_id)
        {
            let sources = sources_by_track.entry(clip.track_id.clone()).or_default();
            let index = match sources.iter().position(|s| s.track_id == *source_id) {
                Some(index) => index,
                None => {
                    sources.push(RuntimeMidiSource {
                        track_id: source_id.clone(),
                        track_index: None,
                    });
                    sources.len() - 1
                }
            };
            let source = u16::try_from(index).ok();
            for event in &mut events {
                event.source = source;
            }
        }
        let end_beat = clip_end_beat;
        clips.push(RuntimeMidiClip {
            id: clip.id.clone(),
            track_id: clip.track_id.clone(),
            start_beat: clip.start_beat,
            end_beat,
            events,
        });
    }

    // A stolen note may belong to an earlier clip. Apply cutoffs after every
    // clip has been built so its future expression and note-off events are
    // removed from both the structural clip and the merged output schedule.
    for clip in &mut clips {
        clip.events.retain(|event| {
            stolen_note_cutoffs
                .get(&event.note_id)
                .is_none_or(|cutoff| {
                    event.sample < *cutoff
                        || (event.sample == *cutoff && event.kind == RuntimeMidiEventKind::NoteOff)
                })
        });
        by_track
            .entry(clip.track_id.clone())
            .or_default()
            .extend(clip.events.iter().cloned());
    }

    let mut midi_tracks: Vec<RuntimeMidiTrack> = by_track
        .into_iter()
        .map(|(track_id, mut events)| {
            sort_midi_events(&mut events);
            let events = resolve_overlapping_notes(events);
            let active = Vec::with_capacity(128); // bound growth out of the audio callback
            RuntimeMidiTrack {
                sources: sources_by_track.remove(&track_id).unwrap_or_default(),
                track_id,
                track_index: None, // resolved by RuntimeProject::resolve_indices
                events,
                cursor: 0,
                active,
                preview_active: Vec::with_capacity(128),
                block_events_spare: Vec::new(),
            }
        })
        .collect();
    midi_tracks.sort_by(|a, b| a.track_id.cmp(&b.track_id));

    (clips, midi_tracks)
}

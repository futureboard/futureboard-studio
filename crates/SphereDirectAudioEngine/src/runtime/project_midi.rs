//! RuntimeProject: MIDI schedules, tempo maps and per-block MIDI.

use super::*;

impl RuntimeProject {
    /// Send each track's bank and program to its instrument where it has not
    /// been sent yet: on the first block after it is set or changed, and when
    /// the instrument behind it is replaced. Runs every callback, playing or
    /// not, so choosing a patch is heard at once. Allocation-free: the events
    /// go into the bridge ring or the track's preallocated block list.
    pub fn flush_midi_programs(&mut self) {
        for index in 0..self.midi_programs.len() {
            let entry = &self.midi_programs[index];
            let track_index = entry.track_index;
            let Some(track) = self.tracks.get(track_index) else {
                continue;
            };
            let Some(route) = midi_program_route(track) else {
                continue;
            };
            if entry.sent_to == Some(route) {
                continue;
            }
            let program = entry.program;
            let channel = program.channel & 0x0F;
            let events = [
                program.bank_msb.map(|msb| {
                    Vst3MidiEvent::control_change(0, channel, 0, f32::from(msb) / 127.0)
                }),
                program.bank_lsb.map(|lsb| {
                    Vst3MidiEvent::control_change(0, channel, 32, f32::from(lsb) / 127.0)
                }),
                Some(Vst3MidiEvent::control_change(
                    0,
                    channel,
                    u16::from(VST3_CTRL_PROGRAM_CHANGE),
                    f32::from(program.program) / 127.0,
                )),
            ];
            let track = &mut self.tracks[track_index];
            let sink = track
                .midi_instrument_insert_ix
                .and_then(|ix| track.inserts.get(ix))
                .and_then(|insert| insert.bridge_sink.clone());
            for event in events.into_iter().flatten() {
                match &sink {
                    Some(sink) => push_vst3_midi_event_to_sink(sink.as_ref(), &event, "", false),
                    None => {
                        if track.midi_block_events.len() < track.midi_block_events.capacity() {
                            track.midi_block_events.push(event);
                        }
                    }
                }
            }
            self.midi_programs[index].sent_to = Some(route);
        }
    }

    /// Keeps, from the graph this one replaces, which program selections its
    /// instruments already have — the same selection to the same destination
    /// is not sent again. Called on the audio thread at the swap; compares
    /// ids in place and allocates nothing.
    pub fn inherit_midi_programs_sent(&mut self, previous: &RuntimeProject) {
        for entry in &mut self.midi_programs {
            entry.sent_to = previous
                .midi_programs
                .iter()
                .find(|old| old.track_id == entry.track_id && old.program == entry.program)
                .and_then(|old| old.sent_to);
        }
    }

    /// Build the MIDI schedule for `snapshot` against this graph's tracks, for
    /// [`Self::replace_midi_schedule`]. Control thread only: it allocates.
    pub fn build_midi_schedule(&self, snapshot: &EngineProjectSnapshot) -> RuntimeMidiData {
        let mut data = build_unresolved_midi_schedule(snapshot, self.sample_rate);
        resolve_midi_schedule_tracks(&mut data, &self.track_ids());
        data
    }

    /// This graph's track ids, in track-index order.
    pub fn track_ids(&self) -> Vec<String> {
        self.tracks.iter().map(|track| track.id.clone()).collect()
    }

    /// Store a MIDI schedule in a graph that is not playing (the control
    /// thread's copy). Sends nothing to any instrument.
    pub fn install_midi_schedule(&mut self, data: RuntimeMidiData) {
        self.midi_clips = data.midi_clips;
        self.midi_tracks = data.midi_tracks;
        for track_index in self
            .midi_tracks
            .iter()
            .filter_map(|track| track.track_index)
        {
            if let Some(slot) = self.audio_graph.active_source_mask.get_mut(track_index) {
                *slot = true;
            }
        }
    }

    /// Swap in a new MIDI schedule without touching anything else in the
    /// graph: instruments, plug-ins, voices, PDC lines and stretchers all keep
    /// running. This is the whole cost of a note edit.
    ///
    /// A note that is sounding right now keeps sounding when the new schedule
    /// still ends it (its note-off comes before any new start of the same key);
    /// otherwise — the note was deleted, moved, or shortened to before the
    /// playhead — it is released here. `next` receives the schedule that was
    /// playing, for the caller to retire off the audio thread.
    ///
    /// Realtime-safe: swaps, in-place scans, and pushes within capacities that
    /// [`build_midi_runtime`] reserved.
    pub fn replace_midi_schedule(&mut self, next: &mut RuntimeMidiData, position_sample: u64) {
        std::mem::swap(&mut self.midi_clips, &mut next.midi_clips);
        std::mem::swap(&mut self.midi_tracks, &mut next.midi_tracks);
        self.adopt_midi_block_event_buffers();
        for midi_track in &mut self.midi_tracks {
            midi_track.cursor = midi_track
                .events
                .partition_point(|ev| ev.sample < position_sample);
            // A track that just got its first MIDI has to join the pass.
            if let Some(slot) = midi_track
                .track_index
                .and_then(|ti| self.audio_graph.active_source_mask.get_mut(ti))
            {
                *slot = true;
            }
        }
        for old in next.midi_tracks.iter_mut() {
            let mut released = std::mem::take(&mut old.active);
            if let Some(new) = self
                .midi_tracks
                .iter_mut()
                .find(|track| track.track_id == old.track_id)
            {
                let pending = &new.events[new.cursor..];
                let active = &mut new.active;
                released.retain(|&(channel, pitch)| {
                    let keep = note_off_is_pending(pending, channel, pitch)
                        && active.len() < active.capacity();
                    if keep {
                        active.push((channel, pitch));
                    }
                    !keep
                });
                // Keys held from the editor belong to the user, not to the
                // schedule: they stay down.
                for &held in &old.preview_active {
                    if new.preview_active.len() < new.preview_active.capacity() {
                        new.preview_active.push(held);
                    }
                }
                old.preview_active.clear();
            }
            push_all_notes_off_for_track(self, old.track_index, &released, 0, false);
            old.active = released;
        }
    }

    /// Reposition every MIDI track's cursor to the first event at/after
    /// `position_sample` and clear active notes (emitting note-offs so the
    /// destination never gets a stuck note). Called on seek / play-from.
    pub fn reset_midi_playback(&mut self, position_sample: u64) {
        self.reset_midi_playback_with_offset(position_sample, 0);
    }

    /// Like [`Self::reset_midi_playback`], but note-off panic events are placed
    /// at `sample_offset` within the current callback block. Used when the
    /// render kernel wraps a loop in the middle of a device block.
    pub fn reset_midi_playback_with_offset(&mut self, position_sample: u64, sample_offset: u32) {
        self.all_notes_off_with_offset("seek/play", sample_offset, true);
        self.rewind_midi_cursors(position_sample);
    }

    /// The transport wrapped from the loop end back to `position_sample`, at
    /// `sample_offset` in the current block. Only the notes still sounding
    /// are released, with note-offs: no sustain / all-sound-off / all-notes-
    /// off controllers. Those land on the same sample as the chord that
    /// starts the next pass, and an instrument that applies them after the
    /// notes (VST3 turns them into parameter changes, whose order against
    /// same-sample events is the plug-in's choice) killed part of that chord
    /// on every pass. A seek or a stop still sends the full panic.
    pub fn loop_wrap_midi_playback(&mut self, position_sample: u64, sample_offset: u32) {
        self.all_notes_off_with_offset("loop", sample_offset, false);
        self.rewind_midi_cursors(position_sample);
    }

    pub(super) fn rewind_midi_cursors(&mut self, position_sample: u64) {
        for mt in &mut self.midi_tracks {
            // Binary search: first event with sample >= position.
            mt.cursor = mt.events.partition_point(|ev| ev.sample < position_sample);
        }
        // A note-off silences a voice; it does not clear a recurrent model's
        // memory of the audio that came before the jump. Without this the first
        // block after a seek is coloured by a passage the listener never heard,
        // which is heard as a swell or a thump at the new position.
        for track in &mut self.tracks {
            if let Some(solfege) = track.solfege_engine.as_mut() {
                solfege.reset_state();
            }
        }
        if midi_engine_debug_enabled() {
            eprintln!(
                "[DAUx MIDI] reset_midi_playback pos={}sa tracks={}",
                position_sample,
                self.midi_tracks.len()
            );
        }
    }

    /// Rebuild MIDI event sample positions from canonical beat positions after a
    /// tempo-map change. Returns the sample position that preserves the current
    /// musical playhead beat under the new map.
    ///
    /// Control-thread / offline form: the replaced map is dropped here. The
    /// callback uses [`Self::install_tempo_change`].
    pub fn apply_tempo_map(
        &mut self,
        mut tempo_map: RuntimeTempoMapSnapshot,
        position_sample: u64,
    ) -> u64 {
        self.install_tempo_map(&mut tempo_map, None, position_sample)
    }

    /// Apply a tempo change prepared by the control thread. Realtime-safe:
    /// every replaced part (map, metronome map, schedule) goes back into
    /// `change` for the caller to retire off the audio thread, and the
    /// metronome copy is left in `change.metronome_map` for the caller to swap
    /// into the metronome. Returns the new transport position.
    pub fn install_tempo_change(
        &mut self,
        change: &mut crate::command::TempoMapChange,
        position_sample: u64,
    ) -> u64 {
        // A schedule timed at another rate than this graph runs at is no use.
        let retimed = change
            .midi
            .as_mut()
            .filter(|_| change.sample_rate == self.sample_rate);
        self.install_tempo_map(&mut change.map, retimed, position_sample)
    }

    /// Swap `tempo_map` in (the previous map comes back through it) and move
    /// the MIDI schedule onto it: by swapping in `retimed` when it is this
    /// graph's schedule already re-timed for the map, otherwise by re-timing
    /// the schedule in place. Neither path allocates or frees.
    pub(super) fn install_tempo_map(
        &mut self,
        tempo_map: &mut RuntimeTempoMapSnapshot,
        retimed: Option<&mut RuntimeMidiData>,
        position_sample: u64,
    ) -> u64 {
        // Identical segments mean an identical beat<->sample mapping, so the
        // rebuild below would recompute every event to the value it already
        // holds. `revision` is bumped per rebuild, so two structurally
        // identical maps built independently compare unequal; compare the
        // segments, which are the mapping.
        if self.tempo_map.segments == tempo_map.segments {
            return position_sample;
        }
        let sr = self.sample_rate.max(1) as f64;
        let current_beat = self.tempo_map.beat_at_samples(position_sample, sr);
        self.all_notes_off("tempo_change");
        std::mem::swap(&mut self.tempo_map, tempo_map);
        let next_position = self.tempo_map.samples_at_beat(current_beat, sr);
        match retimed.filter(|data| self.midi_schedule_matches(data)) {
            Some(data) => {
                std::mem::swap(&mut self.midi_clips, &mut data.midi_clips);
                std::mem::swap(&mut self.midi_tracks, &mut data.midi_tracks);
                self.adopt_midi_block_event_buffers();
            }
            None => retime_midi_lists(
                &mut self.midi_clips,
                &mut self.midi_tracks,
                &self.tempo_map,
                sr,
            ),
        }
        for mt in &mut self.midi_tracks {
            mt.cursor = mt.events.partition_point(|ev| ev.sample < next_position);
            mt.active.clear();
        }
        next_position
    }

    /// Whether `data` is this graph's MIDI schedule (same tracks, routed to
    /// the same graph tracks, same events), differing at most in timing. A
    /// tempo change re-timed off the audio thread is only swapped in when it
    /// is; anything else (a schedule edit that has not landed yet, a preview
    /// track the callback added) re-times in place instead.
    pub(super) fn midi_schedule_matches(&self, data: &RuntimeMidiData) -> bool {
        self.midi_clips.len() == data.midi_clips.len()
            && self.midi_tracks.len() == data.midi_tracks.len()
            && self
                .midi_tracks
                .iter()
                .zip(&data.midi_tracks)
                .all(|(live, next)| {
                    live.track_index == next.track_index
                        && live.events.len() == next.events.len()
                        && live.track_id == next.track_id
                })
    }

    /// Static-tempo shortcut used by legacy `SetBpm` commands.
    pub fn set_static_midi_tempo(&mut self, bpm: f64, position_sample: u64) -> u64 {
        self.apply_tempo_map(RuntimeTempoMapSnapshot::static_tempo(bpm), position_sample)
    }

    /// Emit note-off for all active notes on every MIDI track and clear the
    /// active set. Called on stop/seek to prevent stuck notes.
    pub fn all_notes_off(&mut self, reason: &str) {
        self.all_notes_off_with_offset(reason, 0, true);
    }

    /// `controllers`: also send sustain-off, all-sound-off and all-notes-off
    /// on every channel (stop, seek), not only the active notes' note-offs.
    pub(super) fn all_notes_off_with_offset(
        &mut self,
        reason: &str,
        sample_offset: u32,
        controllers: bool,
    ) {
        let debug = midi_engine_debug_enabled();
        if debug && reason.contains("seek") {
            for mt in &self.midi_tracks {
                if mt.active.is_empty() {
                    continue;
                }
                if let Some(ti) = mt.track_index {
                    if let Some(ix) = self.tracks[ti].midi_instrument_insert_ix {
                        if let Some(instance) = self.tracks[ti].inserts.get(ix) {
                            eprintln!(
                                "[midi-playback] seek panic old_notes={} instance={}",
                                mt.active.len(),
                                instance.id
                            );
                        }
                    }
                }
            }
        }
        // Runs on the audio thread (stop/seek/mute/solo/graph swap): take each
        // active list out by swap (no clone, capacity preserved) and hand the
        // note-offs to the track's instrument route.
        for mt_ix in 0..self.midi_tracks.len() {
            let mut active = std::mem::take(&mut self.midi_tracks[mt_ix].active);
            if debug {
                eprintln!(
                    "[MidiPanic] track={} reason={} active_notes_cleared={}",
                    self.midi_tracks[mt_ix].track_id,
                    reason,
                    active.len()
                );
            }
            let track_index = self.midi_tracks[mt_ix].track_index;
            push_all_notes_off_for_track(self, track_index, &active, sample_offset, controllers);
            active.clear();
            self.midi_tracks[mt_ix].active = active;
            self.midi_tracks[mt_ix].preview_active.clear();
        }
    }

    /// Schedule the MIDI events that fall inside `[base_sample, base_sample +
    /// frames)`. Runs once per audio block from the callback. No heap
    /// allocation on the steady-state path (event lists are preallocated; the
    /// active-note Vec is reserved at build time).
    pub fn schedule_midi_block(&mut self, base_sample: u64, frames: u64) {
        self.schedule_midi_block_with_offset(base_sample, frames, 0);
    }

    pub fn schedule_midi_block_with_offset(
        &mut self,
        base_sample: u64,
        frames: u64,
        callback_offset: u32,
    ) {
        if self.midi_tracks.is_empty() || frames == 0 {
            return;
        }
        let block_end = base_sample.saturating_add(frames);
        let debug = midi_engine_debug_enabled();
        let verbose = crate::forensic_trace::engine_midi_verbose_enabled();
        let trace = crate::forensic_trace::engine_midi_trace_enabled();
        let vst3_debug = vst3_midi_debug_enabled();
        let sr = self.sample_rate.max(1) as f64;
        let heartbeat = trace && crate::forensic_trace::scheduler_heartbeat_due();
        for mt in &mut self.midi_tracks {
            let mut scheduled = 0u32;
            // Instrument route from build-time indices + the cached bridge
            // sink — no id lookups, String clones, or Vec collects per block.
            let track_ix = mt.track_index.filter(|&ti| ti < self.tracks.len());
            let instrument_ix = track_ix.and_then(|ti| self.tracks[ti].midi_instrument_insert_ix);
            let bridge_sink = track_ix.zip(instrument_ix).and_then(|(ti, ix)| {
                self.tracks[ti]
                    .inserts
                    .get(ix)
                    .and_then(|insert| insert.bridge_sink.clone())
            });
            if trace {
                // Trace-only diagnostics. The clip-overlap scan (and its Vec)
                // is allowed here because the flag is off in production.
                let bpm = self
                    .tempo_map
                    .bpm_at_beat(self.tempo_map.beat_at_samples(base_sample, sr));
                let overlapping: Vec<_> = self
                    .midi_clips
                    .iter()
                    .filter(|c| {
                        c.track_id == mt.track_id
                            && block_end > self.tempo_map.samples_at_beat(c.start_beat, sr)
                            && base_sample < self.tempo_map.samples_at_beat(c.end_beat, sr)
                    })
                    .collect();
                let block_has_note = overlapping.iter().any(|c| {
                    c.events.iter().any(|ev| {
                        ev.sample >= base_sample
                            && ev.sample < block_end
                            && matches!(ev.kind, RuntimeMidiEventKind::NoteOn)
                    })
                });
                if block_has_note || heartbeat {
                    eprintln!(
                        "[midi-scheduler] playing=true bpm={bpm:.1} sr={} block_start={base_sample} block_end={block_end}",
                        self.sample_rate
                    );
                    for clip in &overlapping {
                        eprintln!(
                            "[midi-scheduler] track={} clip={} overlaps=true",
                            mt.track_id, clip.id
                        );
                    }
                }
                if bridge_sink.is_some() {
                    if let Some((ti, ix)) = track_ix.zip(instrument_ix) {
                        let instance_id = &self.tracks[ti].inserts[ix].id;
                        eprintln!(
                            "[instrument-route] track={} instrument_instance={}",
                            mt.track_id, instance_id
                        );
                        eprintln!("[instrument-route] plugin_instance_id={instance_id}");
                        eprintln!("[instrument-route] route_ok=true");
                    }
                }
            }
            while mt.cursor < mt.events.len() && mt.events[mt.cursor].sample < block_end {
                let ev = mt.events[mt.cursor].clone();
                mt.cursor += 1;
                if ev.sample < base_sample {
                    apply_active(&mut mt.active, &ev);
                    continue;
                }
                let offset = callback_offset.saturating_add((ev.sample - base_sample) as u32);
                if matches!(ev.kind, RuntimeMidiEventKind::NoteOn)
                    && track_ix.is_some_and(|ti| {
                        midi_source_silenced(&self.tracks, &mt.sources, ev.source, ti, ev.beat)
                    })
                {
                    continue;
                }
                apply_active(&mut mt.active, &ev);
                if let Some(ti) = track_ix {
                    let has_soundfont = self.tracks[ti].soundfont_player.is_some();
                    let has_solfege = self.tracks[ti].solfege_engine.is_some();
                    // Continuous pitch has no VST3 wire representation and no
                    // meaning for a sampler or a bridged plugin, so it leaves
                    // the MIDI stream here and travels on its own list to the
                    // one instrument that can act on it.
                    if matches!(ev.kind, RuntimeMidiEventKind::Articulation) {
                        if has_solfege {
                            let track = &mut self.tracks[ti];
                            if track.solfege_articulation_events.len()
                                < track.solfege_articulation_events.capacity()
                            {
                                track.solfege_articulation_events.push((
                                    offset,
                                    solfege_note_id(ev.channel, ev.pitch),
                                    ev.cc_number,
                                ));
                            } else if let Some(solfege) = track.solfege_engine.as_ref() {
                                // Full list. Staying inside the capacity keeps
                                // this allocation-free, but the event is gone,
                                // so say so instead of losing it silently.
                                solfege.engine.metrics().record_dropped_articulation_event();
                            }
                        }
                        continue;
                    }
                    if matches!(ev.kind, RuntimeMidiEventKind::Pitch) {
                        if has_solfege {
                            let track = &mut self.tracks[ti];
                            if track.solfege_pitch_events.len()
                                < track.solfege_pitch_events.capacity()
                            {
                                track.solfege_pitch_events.push((
                                    offset,
                                    solfege_note_id(ev.channel, ev.pitch),
                                    ev.pitch_hz,
                                ));
                            } else if let Some(solfege) = track.solfege_engine.as_ref() {
                                solfege.engine.metrics().record_dropped_pitch_event();
                            }
                        }
                        continue;
                    }
                    if instrument_ix.is_some() || has_soundfont || has_solfege {
                        let vel = ev.velocity as f32 / 127.0;
                        let midi_ev = match ev.kind {
                            RuntimeMidiEventKind::NoteOn => {
                                Vst3MidiEvent::note_on(offset, ev.channel, ev.pitch, vel)
                            }
                            RuntimeMidiEventKind::NoteOff => {
                                Vst3MidiEvent::note_off(offset, ev.channel, ev.pitch, vel)
                            }
                            RuntimeMidiEventKind::ControlChange => Vst3MidiEvent::control_change(
                                offset,
                                ev.channel,
                                ev.cc_number,
                                ev.cc_value,
                            ),
                            // Both filtered out immediately above.
                            RuntimeMidiEventKind::Pitch | RuntimeMidiEventKind::Articulation => {
                                unreachable!()
                            }
                        };
                        if let Some((ix, sink)) = instrument_ix.zip(bridge_sink.as_deref()) {
                            push_vst3_midi_event_to_sink(
                                sink,
                                &midi_ev,
                                &self.tracks[ti].inserts[ix].id,
                                verbose,
                            );
                            if trace {
                                let abs = ev.sample;
                                let instance_id = &self.tracks[ti].inserts[ix].id;
                                match ev.kind {
                                    RuntimeMidiEventKind::NoteOn => eprintln!(
                                        "[midi-schedule] sample_rate={} event_ppq={:.6} event_sample={abs} offset={offset} note_on pitch={} instance={instance_id}",
                                        self.sample_rate, ev.beat, ev.pitch
                                    ),
                                    RuntimeMidiEventKind::NoteOff => eprintln!(
                                        "[midi-schedule] sample_rate={} event_ppq={:.6} event_sample={abs} offset={offset} note_off pitch={} instance={instance_id}",
                                        self.sample_rate, ev.beat, ev.pitch
                                    ),
                                    _ => {}
                                }
                            }
                        } else {
                            push_midi_block_event(&mut self.tracks[ti].midi_block_events, midi_ev);
                        }
                    } else if vst3_debug {
                        eprintln!(
                            "[VST3 MIDI] skip track={} reason=no_instrument_insert",
                            mt.track_id
                        );
                    }
                }
                if debug {
                    match ev.kind {
                        RuntimeMidiEventKind::NoteOn => eprintln!(
                            "[DAUx MIDI] note_on ch={} pitch={} vel={} offset={}",
                            ev.channel, ev.pitch, ev.velocity, offset
                        ),
                        RuntimeMidiEventKind::NoteOff => eprintln!(
                            "[DAUx MIDI] note_off ch={} pitch={} offset={}",
                            ev.channel, ev.pitch, offset
                        ),
                        RuntimeMidiEventKind::ControlChange => eprintln!(
                            "[DAUx MIDI] cc ch={} ctrl={} value={:.3} offset={}",
                            ev.channel, ev.cc_number, ev.cc_value, offset
                        ),
                        RuntimeMidiEventKind::Pitch => eprintln!(
                            "[DAUx MIDI] pitch ch={} note={} hz={:.3} offset={}",
                            ev.channel, ev.pitch, ev.pitch_hz, offset
                        ),
                        RuntimeMidiEventKind::Articulation => eprintln!(
                            "[DAUx MIDI] articulation ch={} note={} id={} offset={}",
                            ev.channel, ev.pitch, ev.cc_number, offset
                        ),
                    }
                }
                scheduled += 1;
            }
            if debug && scheduled > 0 {
                let bs = self.tempo_map.beat_at_samples(base_sample, sr);
                let be = self.tempo_map.beat_at_samples(block_end, sr);
                eprintln!(
                    "[DAUx MIDI] block beat={:.3}..{:.3} track={} events={} active={}",
                    bs,
                    be,
                    mt.track_id,
                    scheduled,
                    mt.active.len()
                );
                eprintln!(
                    "[DAUx MIDI] block events={} track={}",
                    scheduled, mt.track_id
                );
            }
            if vst3_debug {
                if let Some(ti) = track_ix {
                    if let Some(ix) = instrument_ix {
                        eprintln!(
                            "[VST3 MIDI] instrument insert track={} insert_ix={} block_events={}",
                            mt.track_id,
                            ix,
                            self.tracks[ti].midi_block_events.len()
                        );
                    }
                }
            }
        }
    }

    #[inline]
    pub fn active_clip_count_at_sample(&self, project_sample: u64) -> usize {
        self.clips
            .iter()
            .filter(|clip| {
                project_sample >= clip.start_sample
                    && project_sample < clip.start_sample.saturating_add(clip.duration_samples)
            })
            .count()
    }

    /// Deliver pending `midi_block_events` to instrument VST3 inserts when the
    /// transport is stopped but stop/seek queued note-offs must still reach the
    /// plugin (prevents stuck notes).
    pub fn flush_vst3_midi_inserts(&mut self, frames: usize) {
        if frames == 0 {
            return;
        }
        for track in &mut self.tracks {
            if track.midi_block_events.is_empty() {
                continue;
            }
            let insert_ix = match track.midi_instrument_insert_ix {
                Some(ix) => ix,
                None => {
                    track.midi_block_events.clear();
                    continue;
                }
            };
            // Read in place and cleared after: taking the list left the track
            // with a capacity-0 list (every later event regrew it) and freed
            // the taken one, both on the audio thread.
            if track.block_l.len() >= frames && track.block_r.len() >= frames {
                track.block_l[..frames].fill(0.0);
                track.block_r[..frames].fill(0.0);
                let insert = &mut track.inserts[insert_ix];
                if let Some(vst3) = insert
                    .vst3
                    .as_mut()
                    .filter(|vst3| vst3.is_processor_valid())
                {
                    if insert.scratch_l.len() < frames {
                        insert.scratch_l.resize(frames, 0.0);
                        insert.scratch_r.resize(frames, 0.0);
                    }
                    insert.scratch_l[..frames].fill(0.0);
                    insert.scratch_r[..frames].fill(0.0);
                    let _ = vst3.process_stereo_block_with_midi(
                        &insert.scratch_l[..frames],
                        &insert.scratch_r[..frames],
                        &mut track.block_l[..frames],
                        &mut track.block_r[..frames],
                        &track.midi_block_events,
                    );
                }
            }
            track.midi_block_events.clear();
        }
    }
}

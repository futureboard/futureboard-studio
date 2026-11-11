//! Project snapshots: loading a project, MIDI schedules, clip gain and SoundFont changes, and the project structure key.

use super::*;

/// Identity of everything in `snapshot` except its MIDI clips, plus the build
/// parameters a graph depends on. Two snapshots with the same key build the
/// same graph apart from the MIDI schedule. The MIDI is taken out for the
/// serialize and put back, so nothing is cloned.
///
/// Object keys are written sorted: insert params are a `HashMap`, whose order
/// differs between two maps holding the same entries, and a key that changed
/// with it would never match.
pub(super) fn project_structure_key(
    snapshot: &mut EngineProjectSnapshot,
    output_sample_rate: u32,
    pdc_enabled: bool,
) -> Option<String> {
    let midi = std::mem::take(&mut snapshot.midi_clips);
    // A Soundfont Player's parts are applied live (see
    // `AudioEngine::apply_soundfont_channel_changes`), so they are not structure.
    let parts: Vec<_> = snapshot
        .tracks
        .iter_mut()
        .map(|track| {
            std::mem::replace(
                &mut track.soundfont_channels,
                sphere_soundfont_player::default_channels(),
            )
        })
        .collect();
    // Nor is clip gain (see `AudioEngine::apply_clip_gain_changes`): a
    // rebuild for it cut every sounding note and restarted every stretcher
    // at the end of each gain drag.
    let gains: Vec<f32> = snapshot
        .clips
        .iter_mut()
        .map(|clip| std::mem::replace(&mut clip.gain, 1.0))
        .collect();
    let value = serde_json::to_value(&*snapshot);
    snapshot.midi_clips = midi;
    for (track, parts) in snapshot.tracks.iter_mut().zip(parts) {
        track.soundfont_channels = parts;
    }
    for (clip, gain) in snapshot.clips.iter_mut().zip(gains) {
        clip.gain = gain;
    }
    let mut key = format!("{output_sample_rate}:{pdc_enabled}:");
    write_canonical_json(&value.ok()?, &mut key);
    Some(key)
}

pub(super) fn write_canonical_json(value: &serde_json::Value, out: &mut String) {
    use serde_json::Value;
    match value {
        Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
            out.push('{');
            for (index, (name, item)) in entries.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(name.clone()).to_string());
                out.push(':');
                write_canonical_json(item, out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical_json(item, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

impl EngineInner {
    /// The MIDI-only half of [`Self::load_project`]: build the new schedule
    /// against the running graph's tracks and swap it in on the audio thread.
    pub(super) fn replace_midi_schedule(
        &self,
        snapshot: EngineProjectSnapshot,
    ) -> Result<(), SphereAudioError> {
        let mut current_project = self.project.lock();
        // Built and copied outside the graph lock. The UI thread takes that
        // lock for its meter poll every frame, so building a whole project's
        // schedule under it froze the editor after every note drawn, for as
        // long as the build took. Only the id lookup and the store hold it.
        let (sample_rate, track_ids) = {
            let mirror = self.runtime.lock();
            (mirror.sample_rate, mirror.track_ids())
        };
        let mut schedule = crate::runtime::build_unresolved_midi_schedule(&snapshot, sample_rate);
        crate::runtime::resolve_midi_schedule_tracks(&mut schedule, &track_ids);
        let mut mirror_copy = schedule.clone();
        {
            let mut mirror = self.runtime.lock();
            // `load_project` runs one at a time, so the graph cannot have been
            // rebuilt meanwhile; if it somehow was, resolve against it now.
            if mirror.sample_rate != sample_rate
                || !mirror
                    .tracks
                    .iter()
                    .map(|track| &track.id)
                    .eq(track_ids.iter())
            {
                schedule = mirror.build_midi_schedule(&snapshot);
                mirror_copy = schedule.clone();
            }
            // The mirror seeds a stream opened later, so it plays this too.
            // Stored, not swapped: a swap releases notes, and the mirror
            // shares its plug-in bridge sinks with the live graph.
            mirror.install_midi_schedule(mirror_copy);
        }
        *current_project = Some(snapshot);
        // A denser schedule brings its own larger per-block event lists, so the
        // callback never grows one (see `RuntimeMidiTrack::block_events_spare`).
        schedule.prepare_block_event_buffers();
        let result = self.send_command(EngineCommand::ReplaceMidi(Box::new(schedule)));
        drop(current_project);
        match result {
            Ok(()) | Err(SphereAudioError::EngineNotOpen) => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Sends the running graph the parts of every multitimbral Soundfont
    /// Player whose parts differ from the loaded project's.
    ///
    /// Parts are left out of the structure key, so an edit to one never
    /// rebuilds the graph: a rebuild would cut every sounding note to move one
    /// channel's level. Whatever changed them — an edit, an undo, a reopened
    /// project — they reach the player here, as a live command.
    pub(super) fn apply_soundfont_channel_changes(&self, snapshot: &EngineProjectSnapshot) {
        let changed: Vec<(usize, sphere_soundfont_player::SoundfontChannels)> = {
            let project = self.project.lock();
            let Some(project) = project.as_ref() else {
                return;
            };
            snapshot
                .tracks
                .iter()
                .enumerate()
                .filter(|(_, track)| track.builtin_soundfont_player)
                .filter_map(|(index, track)| {
                    let previous = project.tracks.iter().find(|t| t.id == track.id)?;
                    (previous.soundfont_channels != track.soundfont_channels)
                        .then_some((index, track.soundfont_channels))
                })
                .collect()
        };
        for (track_index, channels) in changed {
            self.runtime
                .lock()
                .update_soundfont_channels(track_index, &channels);
            let _ = self.send_command(EngineCommand::SetSoundfontChannels {
                track_index,
                channels,
            });
        }
    }

    /// Sends the running graph the gain of every clip whose gain differs
    /// from the loaded project's.
    ///
    /// Clip gain is left out of the structure key, so committing a gain
    /// drag, or undoing one, never rebuilds the graph: it reaches the clip
    /// here, as a live command, like the drag itself did.
    pub(super) fn apply_clip_gain_changes(&self, snapshot: &EngineProjectSnapshot) {
        let changed: Vec<(&str, f32)> = {
            let project = self.project.lock();
            let Some(project) = project.as_ref() else {
                return;
            };
            snapshot
                .clips
                .iter()
                .filter_map(|clip| {
                    let previous = project.clips.iter().find(|c| c.id == clip.id)?;
                    (previous.gain != clip.gain).then_some((clip.id.as_str(), clip.gain))
                })
                .collect()
        };
        for (clip_id, gain) in changed {
            let _ = self.set_clip_gain(clip_id, gain);
        }
    }

    pub fn load_project(&self, snapshot: EngineProjectSnapshot) -> Result<(), SphereAudioError> {
        let insert_param_revision = self.insert_param_revision();
        self.load_project_as_of(snapshot, insert_param_revision)
    }

    /// [`Self::load_project`] for a snapshot taken when the plug-in parameter
    /// journal stood at `insert_param_revision`: every parameter edit made
    /// after that is carried into the new graph (#365).
    pub fn load_project_as_of(
        &self,
        mut snapshot: EngineProjectSnapshot,
        insert_param_revision: u64,
    ) -> Result<(), SphereAudioError> {
        let input_state_revision_at_start = self.input_state_revision.load(Ordering::Acquire);
        let old_state = AudioEngineState::from_u8(
            self.shared
                .engine_state
                .swap(AudioEngineState::LoadingProject as u8, Ordering::Relaxed),
        );
        eprintln!(
            "[AudioEngineState] old={old_state:?} new=LoadingProject source=load_project playing={}",
            self.shared.playing.load(Ordering::Relaxed)
        );
        let output_sample_rate = self.shared.sample_rate.load(Ordering::Relaxed).max(1);
        // A note edit changes the MIDI and nothing else. Rebuilding the whole
        // graph for it re-read every SoundFont from disk and, at the swap,
        // silenced every sounding note and restarted every stretcher and PDC
        // line -- once per note drawn. Swap the schedule instead.
        let structure_key =
            project_structure_key(&mut snapshot, output_sample_rate, self.pdc_enabled());
        if structure_key.is_some() && *self.structure_key.lock() == structure_key {
            self.shared
                .engine_state
                .store(old_state as u8, Ordering::Relaxed);
            self.apply_soundfont_channel_changes(&snapshot);
            self.apply_clip_gain_changes(&snapshot);
            return self.replace_midi_schedule(snapshot);
        }
        // A full build from here on; until it lands, nothing matches.
        *self.structure_key.lock() = None;
        let previously_active_paths: Vec<String> = self
            .project
            .lock()
            .as_ref()
            .into_iter()
            .flat_map(|project| project.clips.iter())
            .filter_map(|clip| clip.media_path.as_deref())
            .filter(|path| !path.is_empty())
            .map(str::to_owned)
            .collect();

        // Log how many clips have paths before building runtime.
        let clips_with_path = snapshot
            .clips
            .iter()
            .filter(|c| {
                c.media_path
                    .as_deref()
                    .map(|p| !p.is_empty())
                    .unwrap_or(false)
            })
            .count();
        eprintln!(
            "[SphereAudio] load_project: id='{}' tracks={} snapshot_clips={} clips_with_path={}",
            snapshot.project_id,
            snapshot.tracks.len(),
            snapshot.clips.len(),
            clips_with_path,
        );

        if clips_with_path == 0 && !snapshot.clips.is_empty() {
            eprintln!(
                "[SphereAudio] ⚠ WARNING: all {} clips have null/empty mediaPath — no audio will play!",
                snapshot.clips.len()
            );
        }

        // Extract existing VST3 processors from the current runtime so they can
        // be reused in the new build if the same insert ID + plugin path + class_id
        // + sample_rate still match.  This prevents spurious processor destruction
        // (and editor HWND invalidation) when just reloading the project.
        let mut existing_vst3: HashMap<String, crate::vst3_processor::Vst3RuntimeProcessor> = {
            let mut current = self.runtime.lock();
            let mut map = HashMap::new();
            for track in &mut current.tracks {
                for insert in &mut track.inserts {
                    if let Some(vst3) = insert.vst3.take() {
                        vst3.set_destroy_reason("replaced-by-load-project");
                        map.insert(insert.id.clone(), vst3);
                    }
                }
            }
            map
        };

        let mut runtime = {
            let mut audio_cache = self.audio_cache.lock();
            match RuntimeProject::build(
                &snapshot,
                output_sample_rate,
                &mut audio_cache,
                Some(&mut existing_vst3),
                self.pdc_enabled(),
            ) {
                Ok(project) => {
                    // Keep recently removed sources warm for delete/undo. Opening
                    // a large mapped or streaming source again can take long
                    // enough to make undo appear stuck. The inactive LRU remains
                    // bounded, while every source still used by the project is
                    // retained independently of that limit.
                    let active_paths: HashSet<String> = snapshot
                        .clips
                        .iter()
                        .filter_map(|c| c.media_path.as_deref())
                        .filter(|p| !p.is_empty())
                        .map(str::to_owned)
                        .collect();
                    let mut inactive_lru = self.inactive_audio_cache_lru.lock();
                    inactive_lru.retain(|path| !active_paths.contains(path));
                    for path in &previously_active_paths {
                        if !active_paths.contains(path)
                            && audio_cache.contains_key(path)
                            && !inactive_lru.contains(path)
                        {
                            inactive_lru.push_back(path.clone());
                        }
                    }
                    while inactive_lru.len() > MAX_INACTIVE_AUDIO_CACHE_ENTRIES {
                        if let Some(path) = inactive_lru.pop_front() {
                            if !active_paths.contains(&path) {
                                audio_cache.remove(&path);
                            }
                        }
                    }
                    project
                }
                Err(e) => {
                    let mut current = self.runtime.lock();
                    for track in &mut current.tracks {
                        for insert in &mut track.inserts {
                            if insert.vst3.is_none() {
                                if let Some(vst3) = existing_vst3.remove(&insert.id) {
                                    insert.vst3 = Some(vst3);
                                }
                            }
                        }
                    }
                    // Failed build must not leave the engine wedged in
                    // LoadingProject (permanent silence) — restore.
                    self.shared
                        .engine_state
                        .store(old_state as u8, Ordering::Relaxed);
                    eprintln!(
                        "[AudioEngineState] old=LoadingProject new={old_state:?} source=load_project_failed"
                    );
                    return Err(e.into());
                }
            }
        };
        // Jam routes resolve against the live bus, which `RuntimeProject::build`
        // has no access to. Doing it here rather than inside the build keeps the
        // offline exporter free of live network audio.
        runtime.resolve_jam_inputs(&snapshot, &self.shared.jam_bus);
        runtime.resolve_loopback_inputs(&snapshot);

        // Processors left in existing_vst3 had no matching insert in the new
        // snapshot — they are dropped here with reason="replaced-by-load-project".
        drop(existing_vst3);

        eprintln!(
            "[SphereAudio] RuntimeProject built: {} runtime clips from {} snapshot clips (sr={}) graph_nodes={} pass2={} rejected_routes={}",
            runtime.clips.len(),
            snapshot.clips.len(),
            output_sample_rate,
            runtime.audio_graph.nodes.len(),
            runtime.audio_graph.pass2_routing_indices.len(),
            runtime.audio_graph.rejected_routes.len(),
        );

        // Build initial track states from snapshot.
        let mut tracks = self.tracks.lock();
        tracks.clear();
        for t in &snapshot.tracks {
            let mut ts = TrackState::new(&t.id);
            ts.volume = t.volume.clamp(0.0, 2.0);
            ts.pan = t.pan.clamp(-1.0, 1.0);
            ts.muted = t.muted;
            ts.solo = t.solo;
            tracks.push(ts);
        }
        drop(tracks);

        for t in &snapshot.tracks {
            let track_clips = runtime.clips.iter().filter(|c| c.track_id == t.id).count();
            // Tag 0 is TrackVolume (see `EngineAutomationTargetSnapshot`). An
            // enabled lane with points makes the runtime ignore `volume`
            // entirely (`engine/render.rs`, `automation.volume.unwrap_or`), so
            // a fader that appears to do nothing has to be explainable from
            // this line rather than from reading the render pass.
            //
            // The point count is part of the report, not a filter on it. This
            // read `enabled && !points.is_empty()` and so printed `off` for the
            // enabled-but-empty lane — which was the one case that silently
            // overrode the fader, until `RuntimeAutomationLane::evaluate_normalized`
            // learned to skip empty lanes. A diagnostic that hides the state it
            // exists to expose is worse than none.
            let volume_lane = t
                .automation_lanes
                .iter()
                .find(|lane| lane.target.tag == 0 && lane.enabled);
            let volume_automation = match volume_lane {
                Some(lane) if !lane.points.is_empty() => {
                    format!("on(fader ignored, points={})", lane.points.len())
                }
                Some(_) => "enabled-but-empty(fader wins)".to_string(),
                None => "off".to_string(),
            };
            eprintln!(
                "[SphereAudio] track '{}' type={} clips={} volume={:.2} pan={:.2} muted={} solo={} volume_automation={}",
                t.id, t.track_type, track_clips, t.volume, t.pan, t.muted, t.solo, volume_automation
            );
        }
        // Initialise the graveyard drop-threads on this (control) thread so the
        // callback's first graph swap is a cheap atomic enqueue, never a
        // channel allocation + thread spawn on the audio thread.
        prime_audio_thread_resources();

        // Serialize the project store and callback command with incremental
        // input edits. If one committed while this graph was being built, carry
        // its newer arm/monitor/route fields into both the snapshot and runtime
        // before `LoadProject` is enqueued.
        let mut current_project = self.project.lock();
        if self.input_state_revision.load(Ordering::Acquire) != input_state_revision_at_start {
            if let Some(current) = current_project
                .as_ref()
                .filter(|current| current.project_id == snapshot.project_id)
            {
                for (track_index, next_track) in snapshot.tracks.iter_mut().enumerate() {
                    let Some(current_track) = current
                        .tracks
                        .iter()
                        .find(|track| track.id == next_track.id)
                    else {
                        continue;
                    };
                    next_track.armed = current_track.armed;
                    next_track.input_monitor = current_track.input_monitor;
                    next_track.input_source = current_track.input_source.clone();
                    runtime.update_track_input_state(
                        track_index,
                        current_track.armed,
                        current_track.input_monitor,
                        RuntimeTrackInputSource::from_channels(
                            &current_track.input_source.channels,
                        ),
                    );
                }
                // `from_channels` read a loopback route as the hardware
                // channels it carries; resolve those against the track list
                // again, as the build did.
                runtime.resolve_loopback_inputs(&snapshot);
            }
        }
        if let Err(error) = self.sync_live_input_stream(&snapshot) {
            let message = format!("Live input unavailable: {error}");
            eprintln!("[SphereAudio] {message}");
            self.status.lock().last_error = Some(message);
        }
        // Carry live ARA renderers into the rebuilt graph. A project sync must
        // not tear down bound ARA instances: the binding, the document graph,
        // and any analysis the plug-in has done all belong to the instance, and
        // re-creating it would throw them away and restart analysis.
        {
            let mirror = self.ara_renderers.lock();
            // Lent to an offline render: the rebuilt graph must not take them
            // back while the export processes them.
            if !mirror.is_empty() && !self.ara_lent.load(Ordering::SeqCst) {
                for track in runtime.tracks.iter_mut() {
                    if let Some(renderers) = mirror.get(&track.id) {
                        track.ara_renderers = renderers.clone();
                        track.ara_latency_samples = renderers
                            .iter()
                            .map(|renderer| renderer.latency_samples)
                            .sum();
                        track.plugin_latency_samples = track
                            .plugin_latency_samples
                            .saturating_add(track.ara_latency_samples);
                    }
                }
            }
        }
        // Carry the plugin-bridge sinks (Stage 3b) into the rebuilt graph and
        // re-cache the per-insert handles here, on the control thread. The
        // realtime `LoadProject` arm used to clone the map and re-resolve every
        // insert itself — heap work in the same callback as Play. The mirror is
        // maintained in lockstep with `SetPluginBridgeSink`, so it is the same
        // set the callback's graph holds. Must happen before the runtime mirror
        // below is stored, or that mirror loses the sinks.
        runtime.plugin_bridge_sinks = self.plugin_bridge_sinks.lock().clone();
        runtime.resolve_bridge_sinks();
        // The same for the Control Room: a rebuilt graph starts from a default
        // monitor, and nothing re-sends the configuration after a sync — so
        // every sync used to put Master back on the default output pair and
        // drop the Control Room's source, level and Listen taps.
        self.monitor_mirror.lock().apply_to(&mut runtime);
        // A realtime render in progress records the rebuilt graph too, with
        // its lanes mapped to where the tracks now are.
        if let Some((capture, track_ids)) = self.render_capture.lock().as_ref() {
            capture.map_tracks(&render_capture_indices(&snapshot, track_ids));
            runtime.render_capture = Some(capture.clone());
        }
        // So does a loopback take: a project sync mid-take (the auto-save
        // before Record is enough for one) must not drop it.
        if let Some((recorder, track_ids)) = self.loopback_record.lock().as_ref() {
            recorder.map_destinations(&render_capture_indices(&snapshot, track_ids));
            runtime.loopback_record = Some(recorder.clone());
        }
        // The callback applies this graph's tempo map wholesale, so record it as
        // sent: the trailing `set_tempo_map` below then costs nothing instead of
        // rebuilding every MIDI event list on the audio thread after every sync.
        *self.last_sent_tempo_map.lock() = runtime.tempo_map.clone();
        // Size the tracks' block buffers for the open device here, so the
        // callback does not grow them on the first blocks after the swap.
        let device_block = self.status.lock().buffer_size as usize;
        runtime.reserve_track_blocks(device_block.max(1024));
        // Plug-in parameter edits made since the snapshot was taken went to
        // the running graph; carry them into this one. The journal lock is
        // held until the graph is queued, so a concurrent edit either lands
        // before the replay (queued against the old graph, replayed into this
        // one) or after the swap (resolved against this graph) (#365).
        let mut param_journal = self.insert_param_journal.lock();
        param_journal.replay_into(&mut runtime, insert_param_revision, output_sample_rate);
        *self.runtime.lock() = runtime.clone();
        // The metronome's copy of the tempo map travels with the graph, so the
        // swap hands it over instead of cloning it on the audio thread.
        runtime.metronome_tempo_map = Some(runtime.tempo_map.clone());
        *current_project = Some(snapshot.clone());
        let load_result = self.send_command(EngineCommand::LoadProject(Box::new(runtime)));
        drop(param_journal);
        if matches!(load_result, Ok(()) | Err(SphereAudioError::EngineNotOpen)) {
            *self.structure_key.lock() = structure_key;
        }
        drop(current_project);

        match load_result {
            Ok(()) => eprintln!("[SphereAudio] LoadProject command sent to audio callback"),
            Err(SphereAudioError::EngineNotOpen) => {
                eprintln!(
                    "[SphereAudio] ⚠ WARNING: no audio stream open — runtime stored, \
                     will apply on next openDevice/openDaux"
                );
                // No callback will run the graph-swap transition, so this is
                // the only place that can leave a consistent state. It follows
                // the transport rather than always parking in `Paused`: a
                // stream that reopens under a running transport would otherwise
                // find the engine silenced with no command coming to lift it.
                let resumed = if self.shared.playing.load(Ordering::Relaxed) {
                    AudioEngineState::Running
                } else {
                    AudioEngineState::Paused
                };
                self.shared
                    .engine_state
                    .store(resumed as u8, Ordering::Relaxed);
                eprintln!(
                    "[AudioEngineState] old=LoadingProject new={resumed:?} source=load_project_no_stream"
                );
            }
            Err(e) => {
                self.shared
                    .engine_state
                    .store(old_state as u8, Ordering::Relaxed);
                eprintln!(
                    "[AudioEngineState] old=LoadingProject new={old_state:?} source=load_project_send_failed"
                );
                return Err(e);
            }
        }

        let _ = self.set_tempo_map(snapshot.bpm, snapshot.tempo_points.clone());
        // The project snapshot carries one signature, not the marker list — the
        // time-signature map reaches the engine from the UI as a real map.
        // Pushing the project signature unconditionally replaced that whole map
        // with a single point, so every project sync (including the dirty-poll
        // sync during playback) flattened the meter and moved the metronome's
        // accents. Seed the static signature only while no marker map is
        // installed; once one is, it stays authoritative until the UI replaces
        // it (which it does, including when the last marker is deleted).
        if !self.has_time_signature_markers() {
            let _ = self.set_time_signature(snapshot.time_signature[0], snapshot.time_signature[1]);
        }

        Ok(())
    }
}

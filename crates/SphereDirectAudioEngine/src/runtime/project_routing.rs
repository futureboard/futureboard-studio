//! RuntimeProject: resolving routes, jam and loopback inputs, bridge sinks and ARA renderers.

use super::*;

impl RuntimeProject {
    /// Bind every jam-routed track to its slot in the engine's jam bus.
    ///
    /// Called on the live path only, immediately after [`Self::build`]. The
    /// offline exporter deliberately does not call it: a bounce is a render of
    /// the project, and a remote performer playing at the moment somebody hit
    /// Export is not part of it.
    pub fn resolve_jam_inputs(
        &mut self,
        snapshot: &EngineProjectSnapshot,
        bus: &crate::jam_bus::JamAudioBus,
    ) {
        for (index, track) in snapshot.tracks.iter().enumerate() {
            let Some(runtime_track) = self.tracks.get_mut(index) else {
                break;
            };
            let Some(stream_id) = track.input_source.jam_stream_id() else {
                continue;
            };
            runtime_track.input_source = match bus.bind_input(stream_id) {
                Some(slot) => RuntimeTrackInputSource::Jam {
                    slot: slot as u32,
                    mode: crate::jam_bus::JamChannelMode::from_channels(
                        &track.input_source.channels,
                    ),
                },
                // Every slot is taken. The track captures nothing and says so,
                // rather than being pointed at somebody else's stream.
                None => RuntimeTrackInputSource::None,
            };
        }
    }

    /// Resolve every `trk:` input route to the source track's index, and mark
    /// exactly the tracks something reads.
    ///
    /// Both halves have to happen together: a track publishes its post-fader
    /// block *because* another track reads it, so the publish flags cannot be
    /// decided one track at a time. Run after every project sync and after any
    /// input-route change, on the control thread.
    ///
    /// A route naming a track that does not exist, or naming the destination
    /// itself, resolves to [`RuntimeTrackInputSource::None`] — the track
    /// captures nothing and says so, rather than falling back to whatever
    /// hardware channels the route happens to carry, or building a feedback
    /// loop nobody asked for.
    pub fn resolve_loopback_inputs(&mut self, snapshot: &EngineProjectSnapshot) {
        for track in self.tracks.iter_mut() {
            track.loopback_publish = false;
        }
        let position_of = |id: &str| {
            snapshot
                .tracks
                .iter()
                .position(|candidate| candidate.id == id)
        };
        // Every loopback route in the project as `(source, destination)`, so a
        // route can be checked against the others as well as against outputs
        // and sends. Read from the snapshot because the loop below rewrites the
        // runtime routes as it goes.
        let loopback_edges: Vec<(usize, usize)> = snapshot
            .tracks
            .iter()
            .enumerate()
            .filter_map(|(index, track)| {
                let source = position_of(track.input_source.loopback_track_id()?)?;
                Some((source, index))
            })
            .collect();
        let mut sources: Vec<usize> = Vec::new();
        for (index, track) in snapshot.tracks.iter().enumerate() {
            if index >= self.tracks.len() {
                break;
            }
            let Some(source_id) = track.input_source.loopback_track_id() else {
                continue;
            };
            // A source the destination already feeds — through its output, a
            // send, a multi-out child or another loopback — closes a loop: the
            // destination would hear its own output a block later, forever.
            // With monitoring on that is runaway feedback, so the route
            // resolves to nothing, exactly like a route to itself.
            let source_index = position_of(source_id).filter(|source| {
                *source != index
                    && *source < self.tracks.len()
                    && !self.audio_reaches(index, *source, &loopback_edges)
            });
            self.tracks[index].input_source = match source_index {
                Some(source) => {
                    sources.push(source);
                    RuntimeTrackInputSource::Loopback {
                        source_index: source as u32,
                        mode: crate::jam_bus::JamChannelMode::from_channels(
                            &track.input_source.channels,
                        ),
                    }
                }
                None => RuntimeTrackInputSource::None,
            };
        }
        for source in sources {
            self.tracks[source].loopback_publish = true;
        }
    }

    /// Whether audio leaving track `from` arrives at track `to`: through an
    /// output, a send, a VSTi multi-out child, or a loopback route in
    /// `loopback_edges` (`(source, destination)` pairs). Control thread only;
    /// it allocates.
    pub(super) fn audio_reaches(
        &self,
        from: usize,
        to: usize,
        loopback_edges: &[(usize, usize)],
    ) -> bool {
        let mut seen = vec![false; self.tracks.len()];
        let mut stack = vec![from];
        while let Some(at) = stack.pop() {
            if at == to {
                return true;
            }
            if at >= self.tracks.len() || std::mem::replace(&mut seen[at], true) {
                continue;
            }
            let track = &self.tracks[at];
            stack.extend(track.output_track_index);
            stack.extend(
                track
                    .sends
                    .iter()
                    .filter_map(|send| send.return_track_index),
            );
            for insert in &track.inserts {
                stack.extend(
                    insert
                        .vsti_output_children
                        .iter()
                        .filter_map(|child| child.dest_track_index),
                );
            }
            stack.extend(
                loopback_edges
                    .iter()
                    .filter(|(source, _)| *source == at)
                    .map(|(_, destination)| *destination),
            );
        }
        false
    }

    /// Resolve every cross-entity id reference (clip→track, send→track,
    /// track→output, MIDI track→track) to an index. Called once at build time
    /// on the worker thread; track order is fixed for the life of a runtime
    /// snapshot, so the audio callback only ever reads the precomputed indices.
    pub fn resolve_indices(&mut self) {
        let track_indices: HashMap<String, usize> = self
            .tracks
            .iter()
            .enumerate()
            .map(|(index, track)| (track.id.clone(), index))
            .collect();
        for i in 0..self.clips.len() {
            let ix = track_indices.get(&self.clips[i].track_id).copied();
            self.clips[i].track_index = ix;
        }
        for i in 0..self.midi_tracks.len() {
            let ix = track_indices.get(&self.midi_tracks[i].track_id).copied();
            self.midi_tracks[i].track_index = ix;
            for source in &mut self.midi_tracks[i].sources {
                source.track_index = track_indices.get(&source.track_id).copied();
            }
        }
        // Control Room source: resolve the selected bus/track to an index once,
        // here, so the audio callback branches on `Option<usize>` instead of
        // hashing a string every block. An id that no longer exists resolves to
        // `None`, which the callback treats as "fall back to the master bus"
        // rather than as silence.
        {
            use crate::monitor::{MonitorSource, TapStage};
            let (index, stage) = match &self.monitor.source {
                MonitorSource::MasterBus | MonitorSource::HardwareInput(_) => (None, None),
                MonitorSource::Bus(id) => {
                    (track_indices.get(id).copied(), Some(TapStage::PostFader))
                }
                MonitorSource::TrackAfterFader(id) => {
                    (track_indices.get(id).copied(), Some(TapStage::PostFader))
                }
                MonitorSource::TrackPreFader(id) => {
                    (track_indices.get(id).copied(), Some(TapStage::PreFader))
                }
            };
            self.monitor.source_track_index = index;
            self.monitor.source_stage = stage.filter(|_| index.is_some());
        }
        // Main-output and send-return targets. Shared with latency planning,
        // which routes on these indices, so both see one rule for which id means
        // "straight to master" and which resolves to a track.
        resolve_latency_routing_indices(&mut self.tracks);
        for i in 0..self.tracks.len() {
            // Multi-out (Slice 1): resolve each bridged instrument insert's child
            // "Out Ch" route destination track. Almost always empty.
            for n in 0..self.tracks[i].inserts.len() {
                for c in 0..self.tracks[i].inserts[n].vsti_output_children.len() {
                    let dest_ix = track_indices
                        .get(&self.tracks[i].inserts[n].vsti_output_children[c].dest_track_id)
                        .copied();
                    self.tracks[i].inserts[n].vsti_output_children[c].dest_track_index = dest_ix;
                }
            }
            // Pre-resolve plugin-parameter automation lanes to compact index +
            // numeric-param-id bindings so the audio callback never parses a
            // string param id or searches inserts by id per block.
            let bindings = build_plugin_param_bindings(&self.tracks[i]);
            self.tracks[i].plugin_param_automation = bindings;
            // Send and insert-bypass lanes, likewise resolved to indices.
            resolve_send_and_bypass_lanes(&mut self.tracks[i]);
        }
        // Reverse of the child routes resolved above: for each child "Out Ch"
        // strip, the instrument track that produces it. Soloing the main VSTi
        // track must sound every one of its separate-output channels, and the
        // render pass tests that per block — so it is resolved here, not
        // searched on the audio thread.
        let mut vsti_output_parent_indices = vec![None; self.tracks.len()];
        for (source_index, track) in self.tracks.iter().enumerate() {
            for insert in &track.inserts {
                for child in &insert.vsti_output_children {
                    if let Some(dest_index) = child
                        .dest_track_index
                        .filter(|&dest| dest < vsti_output_parent_indices.len())
                    {
                        vsti_output_parent_indices[dest_index] = Some(source_index);
                    }
                }
            }
        }
        self.vsti_output_parent_indices = vsti_output_parent_indices;

        let mut active_source_mask = vec![false; self.tracks.len()];
        for track_index in self.clips.iter().filter_map(|clip| clip.track_index) {
            active_source_mask[track_index] = true;
        }
        for track_index in self
            .midi_tracks
            .iter()
            .filter_map(|track| track.track_index)
        {
            active_source_mask[track_index] = true;
        }
        for &track_index in &self.audio_graph.pass1_source_indices {
            let track = &self.tracks[track_index];
            active_source_mask[track_index] |= !track.inserts.is_empty()
                || track.soundfont_player.is_some()
                || track.solfege_engine.is_some()
                // An ARA plug-in is the track's source: its clips are skipped by
                // the clip loop because the plug-in renders them itself, so the
                // track has to stay in the pass on its own account.
                || !track.ara_renderers.is_empty();
        }
        self.audio_graph.active_source_mask = active_source_mask;
        self.resolve_bridge_sinks();
    }

    /// Cache each external-bridge insert's realtime sink from
    /// [`Self::plugin_bridge_sinks`] onto the insert itself, so the block path
    /// reads `insert.bridge_sink` instead of doing a `HashMap<String, _>`
    /// lookup. Re-run whenever the sink map changes (LoadProject preserves the
    /// map across graph swaps; SetPluginBridgeSink installs/removes entries).
    /// Arc clones only — no allocation.
    /// Replace one track's ARA playback renderers.
    ///
    /// Called from the callback's command drain, so it does no allocation: the
    /// new list arrived fully built from the control thread and the old one goes
    /// to the graveyard, because dropping the last handle to a bound ARA
    /// instance destroys a C++ VST3 processor.
    ///
    /// PDC follows the renderer set here rather than waiting for the next graph
    /// build, which is why the ARA share of the latency total is tracked apart
    /// from the insert share.
    pub fn set_ara_renderers(&mut self, track_id: &str, renderers: Vec<RuntimeAraRenderer>) {
        let Some(track) = self.tracks.iter_mut().find(|track| track.id == track_id) else {
            crate::graveyard::retire_ara_renderers(renderers);
            return;
        };
        let previous = std::mem::replace(&mut track.ara_renderers, renderers);
        track.plugin_latency_samples = track
            .plugin_latency_samples
            .saturating_sub(track.ara_latency_samples);
        track.ara_latency_samples = track
            .ara_renderers
            .iter()
            .map(|renderer| renderer.latency_samples)
            .sum();
        track.plugin_latency_samples = track
            .plugin_latency_samples
            .saturating_add(track.ara_latency_samples);
        crate::graveyard::retire_ara_renderers(previous);
    }

    pub fn resolve_bridge_sinks(&mut self) {
        let sinks = &self.plugin_bridge_sinks;
        let min_scratch =
            DEFAULT_AUDIO_BLOCK_CAPACITY.max(crate::plugin_bridge::MAX_BRIDGE_BLOCK_FRAMES);
        // Worst-case interleaved multi-out read: every bridge channel enabled.
        let min_scratch_multi = min_scratch * MAX_VSTI_OUTPUT_CHANNELS as usize;
        for track in &mut self.tracks {
            for insert in &mut track.inserts {
                if insert.kind_tag == RuntimeInsertKind::ExternalBridge {
                    insert.bridge_sink = sinks.get(&insert.id).cloned();
                    // Pre-size on the control thread so the audio callback never
                    // has to grow scratch for a bridged insert mid-block.
                    if insert.scratch_l.len() < min_scratch {
                        insert.scratch_l.resize(min_scratch, 0.0);
                        insert.scratch_r.resize(min_scratch, 0.0);
                    }
                    // Only inserts actually routing to child "Out Ch" tracks read
                    // through `scratch_multi` (see `apply_external_bridge_insert_block`);
                    // pre-reserving every bridge insert would waste ~1 MiB each on
                    // the common single-track-fold path.
                    //
                    // Reserve *capacity* only, never touch `.len()`: the audio
                    // thread's `resize(needed, 0.0)` must land `scratch_multi` at
                    // exactly `frames * channels`, which `scatter_vsti_output_children`
                    // relies on to recover the channel stride. Reserving capacity
                    // here makes that `resize` allocation-free without disturbing
                    // that invariant.
                    if !insert.vsti_output_children.is_empty() {
                        let len = insert.scratch_multi.len();
                        if insert.scratch_multi.capacity() < min_scratch_multi {
                            insert
                                .scratch_multi
                                .reserve(min_scratch_multi.saturating_sub(len));
                        }
                    }
                }
            }
        }
    }
}

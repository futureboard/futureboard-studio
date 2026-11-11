//! RuntimeProject: building the runtime project from a snapshot.

use super::*;

impl RuntimeProject {
    /// Build a RuntimeProject from a snapshot.
    ///
    /// `existing_vst3` — if provided, VST3 processors from a previous runtime
    /// whose insert ID + plugin path + class_id + sample_rate still match are
    /// REUSED (taken out of the map) rather than recreated.  This keeps the
    /// same C++ processor alive across project reloads so editor windows stay
    /// valid.  Any entries left in the map after build were not matched and will
    /// be dropped by the caller (triggering `sphere_daux_vst3_destroy`).
    pub fn build(
        snapshot: &EngineProjectSnapshot,
        output_sample_rate: u32,
        decoded_by_path: &mut HashMap<String, Arc<ClipAudioSource>>,
        mut existing_vst3: Option<&mut HashMap<String, Vst3RuntimeProcessor>>,
        pdc_enabled: bool,
    ) -> Result<Self, GraphValidationError> {
        let output_sample_rate = output_sample_rate.max(1);
        let beats_per_second = snapshot.bpm.max(1.0) / 60.0;
        // Audio clips are timed at the base tempo while it is the whole map;
        // under tempo markers they are placed and, when locked to the tempo,
        // stretched through the same map MIDI and the metronome play from.
        let clip_tempo_map = build_project_tempo_map(snapshot);
        let clip_tempo_map =
            (!tempo_map_is_flat_at(&clip_tempo_map, snapshot.bpm)).then_some(&clip_tempo_map);
        let mut clips = Vec::new();
        // Media path of each entry in `clips`, for `assign_stream_lanes`.
        let mut clip_paths: Vec<&str> = Vec::new();
        let mut skipped_no_path = 0u32;
        let mut skipped_decode_err = 0u32;
        let mut loaded_from_cache = 0u32;
        let mut loaded_fresh = 0u32;
        let graph_debug = std::env::var_os("FUTUREBOARD_AUDIO_GRAPH_DEBUG").is_some();

        for clip in &snapshot.clips {
            let Some(path) = clip.media_path.as_deref().filter(|p| !p.trim().is_empty()) else {
                if graph_debug {
                    eprintln!(
                        "[SphereAudio] clip '{}' (track={}) — no mediaPath, skipping",
                        clip.id, clip.track_id
                    );
                }
                skipped_no_path += 1;
                continue;
            };

            let source = match decoded_by_path.get(path) {
                Some(existing) => {
                    if graph_debug {
                        eprintln!(
                            "[SphereAudio] clip '{}' — cache hit: '{path}' ({} frames)",
                            clip.id,
                            existing.frames()
                        );
                    }
                    loaded_from_cache += 1;
                    Arc::clone(existing)
                }
                None => match open_clip_audio_source(path) {
                    Ok(source) => {
                        if graph_debug {
                            eprintln!(
                                "[SphereAudio] clip '{}' — opened: '{path}' {} frames @ {}Hz {} ch ({})",
                                clip.id,
                                source.frames(),
                                source.sample_rate(),
                                source.channels(),
                                if source.is_streaming() {
                                    "stream"
                                } else if source.is_mapped() {
                                    "mmap"
                                } else {
                                    "memory"
                                }
                            );
                        }
                        loaded_fresh += 1;
                        let source = Arc::new(source);
                        decoded_by_path.insert(path.to_string(), Arc::clone(&source));
                        source
                    }
                    Err(e) => {
                        skipped_decode_err += 1;
                        if graph_debug {
                            eprintln!(
                                "[SphereAudio] clip '{}' — decode FAILED '{path}': {e}",
                                clip.id
                            );
                        }
                        continue;
                    }
                },
            };

            let Some(runtime_clip) = build_clip_runtime(
                clip,
                Arc::clone(&source),
                beats_per_second,
                output_sample_rate,
                clip_tempo_map,
            ) else {
                skipped_decode_err += 1;
                continue;
            };
            clips.push(runtime_clip);
            clip_paths.push(path);
        }
        loaded_fresh += assign_stream_lanes(
            &mut clips,
            &clip_paths,
            decoded_by_path,
            &mut open_clip_audio_source,
        );
        publish_stream_clip_spans(&clips, output_sample_rate);

        if skipped_no_path > 0 || skipped_decode_err > 0 || loaded_fresh > 0 {
            eprintln!(
                "[SphereAudio] RuntimeProject built: {} clips ready ({} cached, {} decoded), \
                 {} skipped (no path), {} decode errors",
                clips.len(),
                loaded_from_cache,
                loaded_fresh,
                skipped_no_path,
                skipped_decode_err,
            );
        }

        // Use an explicit loop so we can mutably borrow existing_vst3 on each insert.
        let mut tracks: Vec<RuntimeTrack> = Vec::with_capacity(snapshot.tracks.len());
        for t in &snapshot.tracks {
            let mut inserts: Vec<RuntimeInsert> = Vec::with_capacity(t.inserts.len());
            for insert in &t.inserts {
                // Both external module formats take the in-process native path;
                // `Vst3RuntimeProcessor` resolves which bridge to use from this
                // same `format` param.
                let is_native_module_plugin = insert.kind.eq_ignore_ascii_case("native-plugin")
                    && insert
                        .params
                        .get("format")
                        .and_then(Value::as_str)
                        .and_then(crate::plugin_backend::PluginModuleFormat::from_label)
                        .is_some();

                let vst3 = if is_native_module_plugin {
                    let new_path = insert
                        .params
                        .get("modulePath")
                        .or_else(|| insert.params.get("path"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    let new_class_id = insert
                        .params
                        .get("classId")
                        .or_else(|| insert.params.get("class_id"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .trim()
                        .to_string();

                    // Try to reuse an existing processor matching insert ID +
                    // plugin path + class_id + sample_rate.
                    let reused: Option<Vst3RuntimeProcessor> =
                        if let Some(ref mut map) = existing_vst3 {
                            let can_reuse = map
                                .get(&insert.id)
                                .map(|e| {
                                    e.plugin_path()
                                        .map(|p| p == new_path.as_str())
                                        .unwrap_or(false)
                                        && e.class_id()
                                            .map(|c| c == new_class_id.as_str())
                                            .unwrap_or(false)
                                        && e.sample_rate() == output_sample_rate
                                        && e.is_ready()
                                })
                                .unwrap_or(false);
                            if can_reuse {
                                map.remove(&insert.id)
                            } else {
                                None
                            }
                        } else {
                            None
                        };

                    let reused_flag = reused.is_some();
                    let processor = reused.or_else(|| {
                        Vst3RuntimeProcessor::from_params(&insert.params, output_sample_rate)
                    });
                    if graph_debug {
                        let processor_handle =
                            processor.as_ref().map(|p| p.handle_value()).unwrap_or(0);
                        eprintln!(
                            "[SphereAudio] native VST3 insert track='{}' insert='{}' pluginInstanceId='{}' reused={} ready={} processorHandle=0x{:x} path='{}'",
                            t.id,
                            insert.id,
                            insert
                                .params
                                .get("pluginInstanceId")
                                .and_then(Value::as_str)
                                .unwrap_or(&insert.id),
                            reused_flag,
                            processor.as_ref().map(|p| p.is_ready()).unwrap_or(false),
                            processor_handle,
                            insert
                                .params
                                .get("path")
                                .and_then(Value::as_str)
                                .unwrap_or(""),
                        );
                    }
                    processor
                } else {
                    None
                };

                inserts.push(RuntimeInsert {
                    id: insert.id.clone(),
                    kind: insert.kind.clone(),
                    kind_tag: RuntimeInsertKind::from_kind(&insert.kind),
                    enabled: insert.enabled,
                    bypass_lane: None,
                    automation_bypass: None,
                    bridge_is_effect: insert
                        .params
                        .get("role")
                        .and_then(Value::as_str)
                        .map(|role| role.eq_ignore_ascii_case("effect"))
                        .unwrap_or(false),
                    bridge_is_builtin: insert
                        .params
                        .get("format")
                        .and_then(Value::as_str)
                        .map(|format| format.eq_ignore_ascii_case("builtin"))
                        .unwrap_or(false),
                    bridge_enabled_output_channels: bridge_enabled_output_channels_from_params(
                        &insert.params,
                    ),
                    bridge_sink: None,
                    params: std::sync::Arc::new(insert.params.clone()),
                    dsp: InsertDspState::new(
                        canonical_plugin_id(&insert.kind),
                        &insert.params,
                        output_sample_rate,
                    ),
                    vst3,
                    cpu_us: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
                    guard: Default::default(),
                    callback_process_log_done: false,
                    silent_process_blocks: 0,
                    bridge_missed_blocks: 0,
                    scratch_l: vec![0.0; DEFAULT_AUDIO_BLOCK_CAPACITY],
                    scratch_r: vec![0.0; DEFAULT_AUDIO_BLOCK_CAPACITY],
                    // Populated by the snapshot in a later slice; empty here so
                    // the default single-track fold path is unchanged.
                    vsti_output_children: vsti_output_children_from_params(&insert.params),
                    scratch_multi: Vec::new(),
                    chain_latency_samples: 0,
                    signal_bus: signal_bus_from_params(&insert.params),
                });
            }

            let soundfont_player = RuntimeSoundfontPlayer::from_snapshot(t, output_sample_rate);
            let solfege_engine = RuntimeSolfegeEngine::from_snapshot(t, output_sample_rate);
            // A built-in instrument owns the track's notes; the inserts around
            // it are effects and must never be handed the MIDI stream.
            let midi_instrument_insert_ix =
                if soundfont_player.is_some() || solfege_engine.is_some() {
                    None
                } else {
                    find_midi_instrument_insert_ix(&inserts, &t.track_type)
                };
            // The built-in player's decimation delay at an oversampled render
            // quality is real path latency, so it has to be in the track's
            // reported figure from the first graph build — not only after a
            // later sample-rate rebuild or latency refresh.
            let soundfont_latency_samples = soundfont_player
                .as_ref()
                .map(RuntimeSoundfontPlayer::latency_samples)
                .unwrap_or(0);

            // Seed the fader smoother at the build-time target so the first
            // realtime block plays at the correct level (no startup ramp).
            let init_volume = t.volume.clamp(0.0, 2.0);
            let init_pan = t.pan.clamp(-1.0, 1.0);
            let (init_pan_l, init_pan_r) = if init_pan < 0.0 {
                (1.0, 1.0 + init_pan)
            } else {
                (1.0 - init_pan, 1.0)
            };
            tracks.push(RuntimeTrack {
                spatial: None,
                spatial_params: Default::default(),
                listen: crate::monitor::ListenMode::Off,
                id: t.id.clone(),
                track_type: t.track_type.clone(),
                active_voices: 0,
                volume: init_volume,
                pan: init_pan,
                muted: t.muted,
                solo: t.solo,
                record_armed: t.armed,
                monitor_enabled: t.input_monitor,
                // A jam route resolves to a bus slot, which `build` has no
                // access to; the live engine fills it in through
                // `resolve_jam_inputs` right after this. Leaving it unroutable
                // until then is what stops a jam track from picking up the
                // hardware channels its route happens to name. A loopback
                // route is the same: `resolve_loopback_inputs` resolves it once
                // the whole track list is known.
                input_source: if t.input_source.is_jam() || t.input_source.is_loopback() {
                    RuntimeTrackInputSource::None
                } else {
                    RuntimeTrackInputSource::from_channels(&t.input_source.channels)
                },
                // A publish is session state, not project state, so a rebuilt
                // graph starts unshared and the control thread re-applies what
                // is live. Carrying it in the snapshot would make reopening a
                // project start broadcasting.
                jam_publish_slot: None,
                // Set by `resolve_loopback_inputs` once every track's input
                // route is known: a track publishes only because some other
                // track reads it, which `build` cannot see one track at a time.
                loopback_publish: false,
                loopback_out_l: Vec::new(),
                loopback_out_r: Vec::new(),
                jam_multitrack_pair: None,
                preview_mode: RuntimePreviewMode::from_str(&t.preview_mode),
                output_track_id: t.output_track_id.clone(),
                output_track_index: None, // resolved below in resolve_indices
                inserts,
                sends: t
                    .sends
                    .iter()
                    .map(|send| RuntimeSend {
                        id: send.id.clone(),
                        return_track_id: send.return_track_id.clone(),
                        return_track_index: None, // resolved below in resolve_indices
                        level: send.level.clamp(0.0, 2.0),
                        enabled: send.enabled,
                        pre_fader: send.pre_fader,
                        wdw: send.wdw,
                        level_lane: None,
                        enabled_lane: None,
                        pdc: Default::default(),
                    })
                    .collect(),
                automation_lanes: t
                    .automation_lanes
                    .iter()
                    .map(RuntimeAutomationLane::from_snapshot)
                    .collect(),
                // Resolved below in resolve_indices once insert ids are known.
                plugin_param_automation: Vec::new(),
                automation_clock: Default::default(),
                meter: Arc::new(RuntimeTrackMeter::default()),
                meter_peak_l: 0.0,
                meter_peak_r: 0.0,
                meter_sum_sq_l: 0.0,
                meter_sum_sq_r: 0.0,
                callback_insert_log_done: false,
                callback_clip_route_log_done: false,
                block_l: vec![0.0; DEFAULT_AUDIO_BLOCK_CAPACITY],
                block_r: vec![0.0; DEFAULT_AUDIO_BLOCK_CAPACITY],
                recv_l: vec![0.0; DEFAULT_AUDIO_BLOCK_CAPACITY],
                recv_r: vec![0.0; DEFAULT_AUDIO_BLOCK_CAPACITY],
                soundfont_l: vec![0.0; DEFAULT_AUDIO_BLOCK_CAPACITY],
                soundfont_r: vec![0.0; DEFAULT_AUDIO_BLOCK_CAPACITY],
                ara_renderers: Vec::new(),
                ara_l: vec![0.0; DEFAULT_AUDIO_BLOCK_CAPACITY],
                ara_r: vec![0.0; DEFAULT_AUDIO_BLOCK_CAPACITY],
                ara_silence: vec![0.0; DEFAULT_AUDIO_BLOCK_CAPACITY],
                ara_latency_samples: 0,
                midi_block_events: Vec::with_capacity(MIDI_BLOCK_EVENT_BASE_CAPACITY),
                solfege_pitch_events: Vec::with_capacity(1024),
                solfege_articulation_events: Vec::with_capacity(256),
                midi_instrument_insert_ix,
                soundfont_player,
                solfege_engine,
                plugin_latency_samples: soundfont_latency_samples,
                pdc_delay_l: Vec::new(),
                pdc_delay_r: Vec::new(),
                pdc_write_pos: 0,
                smoothed_gain_l: init_volume * init_pan_l,
                smoothed_gain_r: init_volume * init_pan_r,
                wdw_return_index: None,
            });
        }
        let has_solo = tracks.iter().any(|t| t.solo);
        if std::env::var_os("FUTUREBOARD_AUDIO_GRAPH_DEBUG").is_some() {
            let master_insert_count = tracks
                .iter()
                .find(|track| track.track_type == "master")
                .map(|track| track.inserts.len())
                .unwrap_or(0);
            eprintln!("[SphereAudio] RuntimeMaster inserts={master_insert_count}");
            let track_indices: HashMap<&str, usize> = tracks
                .iter()
                .enumerate()
                .map(|(index, track)| (track.id.as_str(), index))
                .collect();
            let mut clip_counts = vec![0usize; tracks.len()];
            for clip in &clips {
                if let Some(index) = track_indices.get(clip.track_id.as_str()) {
                    clip_counts[*index] += 1;
                }
            }
            for (track_index, track) in tracks.iter().enumerate() {
                eprintln!(
                    "[SphereAudio] RuntimeTrack track={} clips={} inserts={}",
                    track.id,
                    clip_counts[track_index],
                    track.inserts.len()
                );
                for insert in &track.inserts {
                    let format = insert
                        .params
                        .get("format")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let path = insert
                        .params
                        .get("modulePath")
                        .or_else(|| insert.params.get("path"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let class_id = insert
                        .params
                        .get("classId")
                        .or_else(|| insert.params.get("class_id"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    eprintln!(
                        "[SphereAudio] RuntimeInsert id={} format={} path={} classId={} bypass={}",
                        insert.id, format, path, class_id, !insert.enabled
                    );
                }
            }
        }

        // Phase 3 routing graph trace. Logged here on the build (worker)
        // thread — never in the audio callback. Reports node kinds, each
        // track's sends, and any sends that will be rejected at render time
        // (cycle-safe rule: source→routing only, routing→later-routing only).
        if std::env::var_os("FUTUREBOARD_ROUTING_DEBUG").is_some() {
            let is_routing = |ty: &str| ty == "bus" || ty == "return";
            eprintln!("[routing] graph nodes={}", tracks.len());
            for (idx, track) in tracks.iter().enumerate() {
                eprintln!(
                    "[routing] node[{idx}] track={} type={} sends={}",
                    track.id,
                    track.track_type,
                    track.sends.len()
                );
                for send in &track.sends {
                    let target_idx = tracks.iter().position(|t| t.id == send.return_track_id);
                    let target_routing = target_idx
                        .map(|t| is_routing(&tracks[t].track_type))
                        .unwrap_or(false);
                    let source_routing = is_routing(&track.track_type);
                    // Accepted when: target is a routing track, AND if the
                    // source is itself routing the target must come later in
                    // the array (forward-only) to stay acyclic.
                    let accepted = target_routing
                        && match (source_routing, target_idx) {
                            (true, Some(t)) => t > idx,
                            (false, Some(_)) => true,
                            _ => false,
                        };
                    eprintln!(
                        "[routing]   send id={} -> {} target_idx={:?} level={:.3} enabled={} {}",
                        send.id,
                        send.return_track_id,
                        target_idx,
                        send.level,
                        send.enabled,
                        if accepted {
                            "ACCEPT"
                        } else {
                            "REJECT(cycle-unsafe)"
                        }
                    );
                }
            }
        }

        // ── MIDI runtime build (Phase 2) ────────────────────────────────────
        let tempo_map = build_project_tempo_map(snapshot);
        let (midi_clips, midi_tracks) =
            build_midi_runtime(&snapshot.midi_clips, &tempo_map, output_sample_rate);
        let samples_per_beat = if snapshot.bpm > 0.0 {
            output_sample_rate as f64 * 60.0 / snapshot.bpm
        } else {
            0.0
        };
        if graph_debug {
            eprintln!(
                "[transport] sample_rate={} bpm={} samples_per_beat={:.0}",
                output_sample_rate, snapshot.bpm, samples_per_beat
            );
        }

        if crate::forensic_trace::engine_midi_trace_enabled() {
            for track in &snapshot.tracks {
                let track_clip_count = snapshot
                    .midi_clips
                    .iter()
                    .filter(|c| c.track_id == track.id)
                    .count();
                crate::forensic_trace::log_runtime_midi_track_summary(&track.id, track_clip_count);
            }
            for clip in &snapshot.midi_clips {
                crate::forensic_trace::log_runtime_midi_clip(
                    &clip.track_id,
                    clip,
                    samples_per_beat,
                    |beat| tempo_map.samples_at_beat(beat, output_sample_rate as f64),
                );
            }
        }

        if midi_engine_debug_enabled() {
            let total_events: usize = midi_clips.iter().map(|c| c.events.len()).sum();
            for c in &midi_clips {
                eprintln!(
                    "[DAUx MIDI] RuntimeMidiClip id={} track={} notes={} events={} beats={:.3}..{:.3}",
                    c.id,
                    c.track_id,
                    c.events.len() / 2,
                    c.events.len(),
                    c.start_beat,
                    c.end_beat
                );
            }
            eprintln!(
                "[DAUx MIDI] RuntimeProject midi_clips={} midi_events={} midi_tracks={} samples_per_beat={:.2}",
                midi_clips.len(),
                total_events,
                midi_tracks.len(),
                samples_per_beat
            );
        }

        let audio_graph = match plan_runtime_audio_graph(&tracks) {
            Ok(graph) => graph,
            Err(err) => {
                if let Some(map) = existing_vst3 {
                    for track in &mut tracks {
                        for insert in &mut track.inserts {
                            if let Some(vst3) = insert.vst3.take() {
                                map.insert(insert.id.clone(), vst3);
                            }
                        }
                    }
                }
                return Err(err);
            }
        };

        if std::env::var_os("FUTUREBOARD_ROUTING_DEBUG").is_some() {
            eprintln!(
                "[routing] graph nodes={} pass1={} pass2={} rejected={}",
                audio_graph.nodes.len(),
                audio_graph.pass1_source_indices.len(),
                audio_graph.pass2_routing_indices.len(),
                audio_graph.rejected_routes.len(),
            );
        }

        for (idx, track) in tracks.iter_mut().enumerate() {
            track.plugin_latency_samples =
                crate::latency_graph::strip_plugin_latency_samples(track);
            let _ = idx;
        }

        let pdc_active = pdc_enabled
            && !std::env::var_os("FUTUREBOARD_PDC").is_some_and(|v| v == "0" || v == "false");
        let latency_graph = plan_runtime_latency_graph(&mut tracks, &audio_graph, pdc_active);

        if std::env::var_os("FUTUREBOARD_ROUTING_DEBUG").is_some() {
            eprintln!(
                "[latency] max_path={} master_plugin={} pdc_enabled={}",
                latency_graph.max_path_latency_samples,
                latency_graph.master_plugin_latency,
                pdc_active
            );
            for (idx, track) in tracks.iter().enumerate() {
                eprintln!(
                    "[latency] track={} plugin={} output={} pdc_delay={}",
                    track.id,
                    latency_graph.track_plugin_latency[idx],
                    latency_graph.track_output_latency[idx],
                    latency_graph.track_pdc_delay[idx],
                );
            }
        }

        let mut project = Self {
            monitor: {
                // Preallocate the Control Room scratch alongside the per-track
                // block buffers so the device callback never resizes it.
                let mut monitor = RuntimeMonitor::default();
                monitor.ensure_block_capacity(DEFAULT_AUDIO_BLOCK_CAPACITY);
                monitor.simulator = Some(Box::new(solfege_spatialaudio::ListeningSimulator::new(
                    output_sample_rate,
                    DEFAULT_AUDIO_BLOCK_CAPACITY,
                )));
                monitor
            },
            sample_rate: output_sample_rate,
            tracks,
            clips,
            has_solo,
            tempo_map,
            metronome_tempo_map: None,
            midi_clips,
            midi_tracks,
            // Filled by `resolve_indices()` below.
            vsti_output_parent_indices: Vec::new(),
            audio_graph,
            latency_graph,
            pdc_enabled: pdc_active,
            // Set by whoever renders the graph: the live callback from the
            // open stream, the exporter from its block size.
            bridge_period_frames: 0,
            latency_replan_count: 0,
            // Realtime default: smooth fader/pan drags. The offline exporter
            // overrides this to `false` right after `build()` for a deterministic,
            // byte-identical bounce.
            fader_smoothing: true,
            smoothed_master_gain: 1.0,
            // Installed by the control thread after build; never carried in a
            // freshly built project (preserved across reloads in drain_commands).
            plugin_bridge_sinks: std::collections::HashMap::new(),
            bridge_editor_active: std::collections::HashSet::new(),
            midi_programs: snapshot
                .tracks
                .iter()
                .enumerate()
                .flat_map(|(track_index, track)| {
                    track
                        .midi_programs
                        .iter()
                        .map(move |program| RuntimeMidiProgram {
                            track_id: track.id.clone(),
                            track_index,
                            program: program.sanitized(),
                            sent_to: None,
                        })
                })
                .collect(),
            render_capture: None,
            loopback_record: None,
            spatial: RuntimeSpatial::from_snapshot(&snapshot.spatial, output_sample_rate),
        };
        // Resolve cross-entity indices once, on this worker thread, so the
        // audio callback never does an id lookup per block.
        project.resolve_indices();
        project.apply_spatial_sources(&snapshot.spatial);
        project.ensure_spatial();
        // Size the PDC rings here rather than inline above, so a freshly built
        // project starts with the same reserved headroom every other
        // control-thread path leaves behind — without it the first bridge
        // latency the callback observes would have nowhere to grow into.
        project.ensure_pdc_delay_capacity();
        // The instrument tracks' per-block MIDI lists, sized for the schedule
        // (every graph, live or offline, starts with lists that hold it).
        project.reserve_midi_block_events();
        Ok(project)
    }
}

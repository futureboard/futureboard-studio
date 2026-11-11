//! Building the output stream and its audio callback.

use super::*;

// ── Audio callback builder ────────────────────────────────────────────────────

pub(super) fn output_stream_candidates(
    device: &cpal::Device,
    requested: &JsDeviceOpenConfig,
) -> Result<Vec<OutputStreamCandidate>, String> {
    let default_supported = device.default_output_config().map_err(|e| e.to_string())?;
    let default_config = default_supported.config();
    let sample_format = default_supported.sample_format();

    let mut candidates = Vec::new();

    if requested.sample_rate.is_some() || requested.buffer_size.is_some() {
        let mut requested_config = default_config.clone();

        if let Some(sample_rate) = requested.sample_rate {
            requested_config.sample_rate = cpal::SampleRate(sample_rate);
        }
        if let Some(buffer_size) = requested.buffer_size {
            requested_config.buffer_size = BufferSize::Fixed(buffer_size);
        }

        candidates.push(OutputStreamCandidate {
            config: requested_config,
            sample_format,
            label: "requested",
        });
    }

    candidates.push(OutputStreamCandidate {
        config: default_config,
        sample_format,
        label: "default",
    });

    Ok(candidates)
}

pub(super) fn reported_buffer_size(config: &cpal::StreamConfig) -> u32 {
    match config.buffer_size {
        BufferSize::Fixed(frames) => frames,
        BufferSize::Default => 0,
    }
}

/// Compact, log-friendly mode token for the audio-device sample-rate decision.
pub(super) fn sample_rate_mode_label(backend: &BackendKind) -> &'static str {
    match backend {
        BackendKind::Auto => "AUTO",
        BackendKind::WasapiShared => "WASAPI_SHARED",
        BackendKind::WasapiExclusive => "WASAPI_EXCLUSIVE",
        BackendKind::WdmKs => "WDM_KS",
        BackendKind::DauxDirect => "DAUX_DIRECT",
        BackendKind::Asio => "ASIO",
        BackendKind::CoreAudio => "COREAUDIO",
        BackendKind::Alsa => "ALSA",
        BackendKind::MmeFallback => "MME",
    }
}

/// Returns `Some((requested, active))` when a *specific* rate was requested
/// (`requested > 0`) and the opened stream runs at a different rate. `None`
/// otherwise (no specific request, or the rates agree). Control-thread only —
/// the realtime callback must never call this.
pub(crate) fn sample_rate_mismatch(requested: u32, active: u32) -> Option<(u32, u32)> {
    if requested > 0 && active > 0 && requested != active {
        Some((requested, active))
    } else {
        None
    }
}

/// Emit the visible sample-rate decision after a stream opens (control thread).
/// Shared/Auto/WDM-KS mismatch is *allowed* (Windows resamples) but must be
/// visible. Exclusive-mode mismatch means an implicit fallback happened and is
/// surfaced as a warning string the caller can store in `last_daux_error`.
pub(super) fn log_sample_rate_decision(
    requested: u32,
    active: u32,
    backend: &BackendKind,
) -> Option<String> {
    let (req, act) = sample_rate_mismatch(requested, active)?;
    let mode = sample_rate_mode_label(backend);
    eprintln!("[audio-device] requested_sample_rate={req} active_sample_rate={act} mode={mode}");
    eprintln!("[audio-device] sample rate mismatch: using active device rate for timing");
    if matches!(backend, BackendKind::WasapiExclusive) {
        // Exclusive mode could not honor the exact rate and fell back to the
        // native device rate — a real degradation the user should see.
        let msg = format!(
            "Exclusive mode could not open at {req} Hz; the device is running at {act} Hz. \
             Timing uses the active device rate."
        );
        eprintln!("[audio-device] WARNING: {msg}");
        Some(msg)
    } else {
        None
    }
}

//
pub(super) fn build_output_stream(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    sample_format: SampleFormat,
    cmd_rx: Receiver<EngineCommand>,
    shared: Arc<SharedState>,
    initial_runtime: RuntimeProject,
) -> Result<cpal::Stream, String> {
    match sample_format {
        SampleFormat::I8 => {
            build_output_stream_typed::<i8>(device, config, cmd_rx, shared, initial_runtime)
        }
        SampleFormat::I16 => {
            build_output_stream_typed::<i16>(device, config, cmd_rx, shared, initial_runtime)
        }
        SampleFormat::I32 => {
            build_output_stream_typed::<i32>(device, config, cmd_rx, shared, initial_runtime)
        }
        SampleFormat::I64 => {
            build_output_stream_typed::<i64>(device, config, cmd_rx, shared, initial_runtime)
        }
        SampleFormat::U8 => {
            build_output_stream_typed::<u8>(device, config, cmd_rx, shared, initial_runtime)
        }
        SampleFormat::U16 => {
            build_output_stream_typed::<u16>(device, config, cmd_rx, shared, initial_runtime)
        }
        SampleFormat::U32 => {
            build_output_stream_typed::<u32>(device, config, cmd_rx, shared, initial_runtime)
        }
        SampleFormat::U64 => {
            build_output_stream_typed::<u64>(device, config, cmd_rx, shared, initial_runtime)
        }
        SampleFormat::F32 => {
            build_output_stream_typed::<f32>(device, config, cmd_rx, shared, initial_runtime)
        }
        SampleFormat::F64 => {
            build_output_stream_typed::<f64>(device, config, cmd_rx, shared, initial_runtime)
        }
        format => Err(format!("unsupported output sample format: {format}")),
    }
}

pub(super) fn build_output_stream_typed<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    cmd_rx: Receiver<EngineCommand>,
    shared: Arc<SharedState>,
    initial_runtime: RuntimeProject,
) -> Result<cpal::Stream, String>
where
    T: SizedSample + Sample + FromSample<f32> + crate::dsp::dither::DitheredOutput,
{
    let output_sample_rate = config.sample_rate.0;
    let sr = output_sample_rate as f64;

    // Word-length reduction for integer device formats, same as the DAUx path:
    // `from_sample` truncates toward zero, which is a bias plus correlated
    // distortion at the device word length. Floats pass through untouched.
    let mut dither = crate::dsp::dither::OutputDither::new();

    // Oscillator state — local to the audio callback (no lock needed).
    let mut osc_l = SineOscillator::new(440.0, sr);
    let mut osc_r = SineOscillator::new(440.0, sr);
    let mut osc_freq = 440.0f32;
    let mut osc_on = false;

    // Local playback state.
    let mut playing_local = false;
    let mut render_path_logged = false;
    let mut block_scratch: Vec<f32> = Vec::new();
    let mut metronome = crate::backend::render::LocalAudioState::new(sr);
    let mut audition = AuditionPlayer::default();

    // Meter state with peak hold.
    let mut prev_peak_l = 0.0f32;
    let mut prev_peak_r = 0.0f32;

    let ch = config.channels as usize;
    let mut runtime = initial_runtime;
    runtime.sample_rate = output_sample_rate;
    // Often a clone of the control thread's mirror, whose per-block MIDI lists
    // came back empty: size them here, not in the callback.
    runtime.reserve_midi_block_events();

    let stream = device
        .build_output_stream::<T, _, _>(
            config,
            move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
                // Dropout watchdog: time the whole callback (atomics only). The
                // legacy in-process callback previously had no timing — only the
                // DAUx backend did — so realtime dropouts went undetected here.
                let cb_started = std::time::Instant::now();

                // Publish the output play-out latency (callback → DAC) so a live
                // recording can shift its take earlier by the real round-trip.
                // cpal already computes these instants; this is one atomic store.
                let out_ts = info.timestamp();
                if let Some(delay) = out_ts.playback.duration_since(&out_ts.callback) {
                    shared
                        .output_latency_secs
                        .store(f32_store(delay.as_secs_f32()), Ordering::Relaxed);
                }
                if ch > 0 {
                    for track in &mut runtime.tracks {
                        track.midi_block_events.clear();
                        track.solfege_pitch_events.clear();
                        track.solfege_articulation_events.clear();
                    }
                }

                // ── 1. Drain command queue ───────────────────────────────────
                // Runs first so commands take effect from the start of this block.
                let frames_for_flush = if ch > 0 {
                    data.len().checked_div(ch).unwrap_or(0)
                } else {
                    0
                };
                while let Ok(cmd) = cmd_rx.try_recv() {
                    match cmd {
                        EngineCommand::LoadProject(next_runtime) => {
                            if callback_debug_enabled() {
                                eprintln!(
                                    "[SphereAudio callback] LoadProject: {} tracks, {} clips (sr={})",
                                    next_runtime.tracks.len(),
                                    next_runtime.clips.len(),
                                    output_sample_rate,
                                );
                            }
                            // Release notes on the current graph before swapping
                            // it out, otherwise pending note-offs would be
                            // dropped with the retired VST3 processors.
                            runtime.all_notes_off("project_load");
                            runtime.flush_vst3_midi_inserts(frames_for_flush);
                            // Swap in the new graph and retire the old one off
                            // the realtime thread — its destructor frees
                            // buffers / munmaps sources / destroys VST3 handles
                            // and must not run here. See `crate::graveyard`.
                            // Swapped through the command's box rather than
                            // moved out of it: moving out frees the box here.
                            let mut old = next_runtime;
                            std::mem::swap(&mut runtime, &mut *old);
                            runtime.sample_rate = output_sample_rate;
                            // The plugin-bridge sinks (Stage 3b) arrive already
                            // carried and resolved: `load_project` copies its
                            // control-thread mirror into the graph and resolves
                            // the per-insert handles before sending it. Cloning
                            // the map and re-resolving (which can grow insert
                            // scratch) here was heap work on the audio thread.
                            //
                            // The open-editor set moves across by swap rather
                            // than a clone (a set plus a String per open
                            // editor); the new graph's empty set leaves with
                            // `old` through the graveyard.
                            std::mem::swap(
                                &mut runtime.bridge_editor_active,
                                &mut old.bridge_editor_active,
                            );
                            runtime.inherit_midi_programs_sent(&old);
                            // The panic pushed into the preserved sinks above
                            // still needs flushing through the new graph.
                            // Keep the live master-fader ramp continuous across
                            // media/control graph swaps (clip mute/gain sync).
                            runtime.smoothed_master_gain =
                                f32_load(shared.master_volume.load(Ordering::Relaxed));
                            let pos = shared.position_samples.load(Ordering::Relaxed);
                            // Through the copy `load_project` packed with the
                            // graph: no clone or free on this thread.
                            std::mem::swap(
                                &mut runtime.metronome_tempo_map,
                                &mut old.metronome_tempo_map,
                            );
                            metronome.follow_graph_tempo_map(
                                &runtime.tempo_map,
                                &mut old.metronome_tempo_map,
                                pos,
                                output_sample_rate,
                            );
                            crate::graveyard::retire(old);
                            if crate::runtime::midi_engine_debug_enabled() {
                                let notes: usize =
                                    runtime.midi_clips.iter().map(|c| c.events.len() / 2).sum();
                                eprintln!(
                                    "[DAUx MIDI] snapshot clips={} notes={}",
                                    runtime.midi_clips.len(),
                                    notes
                                );
                            }
                            // Position MIDI cursors for the current transport
                            // position so a load mid-playback stays in sync.
                            let midi_pos = shared.position_samples.load(Ordering::Relaxed);
                            runtime.reset_midi_playback(midi_pos);
                            // Transport/audio-graph separation, same rule the
                            // DAUx path states: a graph swap must never change
                            // the user's transport state. `load_project` parks
                            // the engine in `LoadingProject` on the control
                            // thread, and this is the only place that can lift
                            // it — without this the state stayed `LoadingProject`
                            // for the rest of the session, which lies to the UI
                            // and to every diagnostic that reads it.
                            let was_playing = shared.playing.load(Ordering::Relaxed);
                            playing_local = was_playing;
                            let previous_state = crate::engine::AudioEngineState::from_u8(
                                shared.engine_state.load(Ordering::Relaxed),
                            );
                            let swapped_state = if was_playing {
                                crate::engine::AudioEngineState::Running
                            } else {
                                crate::engine::AudioEngineState::Paused
                            };
                            shared
                                .engine_state
                                .store(swapped_state as u8, Ordering::Relaxed);
                            if callback_debug_enabled() || command_debug_enabled() {
                                eprintln!(
                                    "[AudioEngineState] old={previous_state:?} new={swapped_state:?} source=graph_swap was_playing={was_playing}"
                                );
                            }
                        }
                        EngineCommand::ReplaceMidi(mut next) => {
                            let pos = shared.position_samples.load(Ordering::Relaxed);
                            runtime.replace_midi_schedule(&mut next, pos);
                            crate::graveyard::retire_midi(next);
                        }
                        EngineCommand::SetTestTone { enabled, frequency } => {
                            osc_on = enabled;
                            osc_freq = frequency;
                            osc_l.set_frequency(frequency as f64);
                            osc_r.set_frequency(frequency as f64);
                        }
                        EngineCommand::StartAudition { token, source } => {
                            // Reject a decode the user has already moved past;
                            // the source leaves through the graveyard so this
                            // block never frees it on the audio thread.
                            if token
                                == shared.audition_request_token.load(Ordering::Relaxed)
                            {
                                audition.start(source, output_sample_rate);
                            } else {
                                crate::graveyard::retire_audio_file(source);
                            }
                        }
                        EngineCommand::StopAudition => {
                            audition.stop(output_sample_rate);
                        }
                        EngineCommand::StartTransport => {
                            let pos = shared.position_samples.load(Ordering::Relaxed);
                            let active_clips = runtime.active_clip_count_at_sample(pos);
                            if command_debug_enabled() {
                                eprintln!(
                                    "[SphereAudio callback] StartTransport: position={}sa ({:.3}s), active_clip_count={}, scheduled_clip_count={}",
                                    pos,
                                    pos as f64 / output_sample_rate as f64,
                                    active_clips,
                                    runtime.clips.len(),
                                );
                            }
                            playing_local = true;
                            shared.playing.store(true, Ordering::Relaxed);
                            // Position MIDI cursors at the start beat and clear
                            // any stale active notes so play-from is clean.
                            runtime.reset_midi_playback(pos);
                            // Clear stale PDC delay-line audio so the compensated
                            // (lower-latency / audio) tracks start settled and stay
                            // aligned with plugin/VSTi-latency tracks from the first
                            // audible block — parity with the DAUx backend and with
                            // offline export's fresh-runtime + warmup start. Without
                            // this, the legacy realtime callback replays pre-stop
                            // audio out of the delay rings, desyncing audio vs VSTi
                            // tracks at the start of every play (export was fine
                            // because it builds a fresh zeroed runtime). Realtime-safe
                            // zero-fill; runs only on Start.
                            runtime.reset_pdc_delay_lines();
                            // Starting playback is the authoritative end of any
                            // ruler scrub. Never inherit a stale suspended flag
                            // when mouse-up occurred outside the ruler.
                            metronome.prepare_metronome_for_transport_start(
                                pos,
                                output_sample_rate,
                            );
                        }
                        EngineCommand::StopTransport => {
                            if command_debug_enabled() {
                                eprintln!("[SphereAudio callback] StopTransport");
                            }
                            playing_local = false;
                            shared.playing.store(false, Ordering::Relaxed);
                            // Release held notes so nothing is left stuck.
                            runtime.all_notes_off("stop");
                            // Drop latency-compensated pre-stop audio immediately so
                            // Stop cuts at the command (parity with the DAUx path).
                            runtime.reset_pdc_delay_lines();
                        }
                        EngineCommand::Seek { position_seconds } => {
                            let sr_local = shared.sample_rate.load(Ordering::Relaxed) as f64;
                            let pos = (position_seconds * sr_local) as u64;
                            if command_debug_enabled() {
                                eprintln!(
                                    "[SphereAudio callback] Seek -> {:.3}s ({}sa)",
                                    position_seconds, pos
                                );
                            }
                            shared.position_samples.store(pos, Ordering::Relaxed);
                            metronome.reset_metronome_schedule(pos, output_sample_rate);
                            // Re-seek MIDI cursors + flush held notes.
                            runtime.reset_midi_playback(pos);
                            // A seek repositions the playhead; the PDC delay rings
                            // still hold audio from the pre-seek position. Clear them
                            // so the compensated tracks refill from the new position
                            // and stay aligned with plugin/VSTi-latency tracks (same
                            // reset the DAUx backend and export already do).
                            runtime.reset_pdc_delay_lines();
                        }
                        EngineCommand::SetMetronomeEnabled(enabled) => {
                            let pos = shared.position_samples.load(Ordering::Relaxed);
                            shared
                                .metronome_enabled
                                .store(enabled, Ordering::Relaxed);
                            metronome.set_metronome_enabled(enabled, pos, output_sample_rate);
                        }
                        EngineCommand::SetMetronomeSuspended(suspended) => {
                            metronome.set_metronome_suspended(suspended);
                        }
                        EngineCommand::SetMetronomeVoice { volume, sound } => {
                            metronome.set_metronome_voice(
                                volume,
                                crate::backend::render::MetronomeSound::from_code(sound),
                            );
                        }
                        EngineCommand::StartCountIn {
                            beats,
                            beats_per_bar,
                            samples_per_beat,
                        } => {
                            metronome.begin_count_in(beats, beats_per_bar, samples_per_beat);
                        }
                        EngineCommand::CancelCountIn => {
                            metronome.cancel_count_in();
                            shared.count_in_remaining_samples.store(0, Ordering::Relaxed);
                        }
                        EngineCommand::SetBpm(bpm) => {
                            let pos = shared.position_samples.load(Ordering::Relaxed);
                            transport::store_f64_bits(&shared.bpm_bits, bpm);
                            let map = crate::tempo_map::RuntimeTempoMapSnapshot::static_tempo(bpm);
                            metronome.set_tempo_map(map.clone(), pos, output_sample_rate);
                            let next_pos = runtime.apply_tempo_map(map, pos);
                            shared.position_samples.store(next_pos, Ordering::Relaxed);
                        }
                        EngineCommand::SetTempoMap(mut change) => {
                            // Prepared on the control thread: swaps only (see
                            // the DAUx arm in `backend::render`).
                            let pos = shared.position_samples.load(Ordering::Relaxed);
                            let next_pos = runtime.install_tempo_change(&mut change, pos);
                            shared.position_samples.store(next_pos, Ordering::Relaxed);
                            metronome.swap_tempo_map(
                                &mut change.metronome_map,
                                next_pos,
                                output_sample_rate,
                            );
                            crate::graveyard::retire_tempo_change(change);
                        }
                        EngineCommand::SetTimeSignatureMap(mut change) => {
                            let pos = shared.position_samples.load(Ordering::Relaxed);
                            metronome.swap_time_signature_map(
                                &mut change,
                                pos,
                                output_sample_rate,
                            );
                            if let Some(pt) = metronome.time_signature_map.points().first() {
                                shared
                                    .time_sig_num
                                    .store(pt.numerator.max(1) as u32, Ordering::Relaxed);
                                shared
                                    .time_sig_den
                                    .store(pt.denominator.max(1) as u32, Ordering::Relaxed);
                            }
                            crate::graveyard::retire_time_signature_map(change);
                        }
                        EngineCommand::SetLoop {
                            enabled,
                            start_seconds,
                            end_seconds,
                        } => {
                            let sr_local = shared.sample_rate.load(Ordering::Relaxed) as f64;
                            let start = (start_seconds.max(0.0) * sr_local) as u64;
                            let end = (end_seconds.max(0.0) * sr_local) as u64;
                            shared.loop_enabled.store(enabled, Ordering::Relaxed);
                            shared
                                .loop_start_samples
                                .store(start, Ordering::Relaxed);
                            shared.loop_end_samples.store(end, Ordering::Relaxed);
                        }
                        EngineCommand::SetMasterVolume { value } => {
                            shared
                                .master_volume
                                .store(f32_store(value), Ordering::Relaxed);
                        }
                        EngineCommand::SetMonitorSource { source } => {
                            runtime.monitor.source = source;
                            runtime.resolve_indices();
                        }
                        EngineCommand::SetMonitorControl { control } => {
                            runtime.monitor.control = control;
                            shared.monitor_gain.store(
                                f32_store(control.effective_gain()),
                                Ordering::Relaxed,
                            );
                        }
                        EngineCommand::SetMonitorOutput { target } => {
                            runtime.monitor.output = target;
                        }
                        EngineCommand::SetListeningSimulation { settings } => {
                            if let Some(simulator) = runtime.monitor.simulator.as_deref_mut() {
                                simulator.configure(settings);
                            }
                        }
                        EngineCommand::SetHardwareOutputOwnership {
                            owner,
                            master,
                            monitor,
                        } => {
                            runtime.monitor.hardware_owner = owner;
                            runtime.monitor.master_output = master;
                            if let Some((left, _right)) = monitor {
                                runtime.monitor.output.left_channel = left;
                            }
                        }
                        EngineCommand::SetTrackSpatial {
                            track_index,
                            params,
                        } => {
                            runtime.update_track_spatial(track_index, params);
                        }
                        EngineCommand::SetClipGain { clip_index, gain } => {
                            runtime.update_clip_gain(clip_index, gain);
                        }
                        EngineCommand::SetTrackSendLevel {
                            track_index,
                            send_index,
                            level,
                        } => {
                            runtime.update_track_send_level(track_index, send_index, level);
                        }
                        EngineCommand::SetTrackListen {
                            track_index,
                            listen,
                        } => {
                            if let Some(track) = runtime.tracks.get_mut(track_index) {
                                track.listen = listen;
                            }
                        }
                        EngineCommand::ClearAllListen => {
                            for track in runtime.tracks.iter_mut() {
                                track.listen = crate::monitor::ListenMode::Off;
                            }
                        }
                        EngineCommand::SetAraRenderers {
                            track_id,
                            renderers,
                        } => {
                            runtime.set_ara_renderers(&track_id, renderers);
                        }
                        EngineCommand::SetPluginBridgeSink { insert_id, sink } => {
                            match sink {
                                Some(sink) => {
                                    runtime.plugin_bridge_sinks.insert(insert_id, sink);
                                }
                                None => {
                                    runtime.plugin_bridge_sinks.remove(&insert_id);
                                }
                            }
                            // Re-cache per-insert sink handles for the block path.
                            runtime.resolve_bridge_sinks();
                        }
                        EngineCommand::CommandBarrier { ack } => {
                            // Wait-free ack: every command sent before this one
                            // has now been applied to the callback's runtime.
                            ack.store(true, Ordering::Release);
                        }
                        EngineCommand::SetBridgeEditorActive(mut change) => {
                            std::mem::swap(&mut runtime.bridge_editor_active, &mut *change);
                            crate::graveyard::retire_bridge_editor_active(change);
                        }
                        EngineCommand::SetTrackVolume { track_index, value } => {
                            let applied = runtime.update_track_volume_at(track_index, value);
                            if callback_debug_enabled() {
                                eprintln!(
                                    "[SphereAudio callback] SetTrackVolume track_index={track_index} linear={value:.4} applied={applied}"
                                );
                            }
                        }
                        EngineCommand::SetTrackPan { track_index, value } => {
                            runtime.update_track_pan_at(track_index, value);
                        }
                        EngineCommand::SetTrackMute { track_index, muted } => {
                            if callback_debug_enabled() {
                                eprintln!("[SphereAudio callback] SetTrackMute track_index={track_index} muted={muted}");
                            }
                            // No note-off: mute silences the output, and the
                            // render pass keeps the instrument running under
                            // it, so unmuting drops back in mid-note instead
                            // of waiting for the next note-on.
                            runtime.update_track_mute_at(track_index, muted);
                        }
                        EngineCommand::SetTrackSolo { track_index, solo } => {
                            // Same rule as mute: soloing another track must not
                            // cut this one's sounding notes.
                            runtime.update_track_solo_at(track_index, solo);
                        }
                        EngineCommand::SetTrackInputState {
                            track_index,
                            record_armed,
                            monitor_enabled,
                            input_source,
                        } => runtime.update_track_input_state(
                            track_index,
                            record_armed,
                            monitor_enabled,
                            input_source,
                        ),
                        EngineCommand::SetTrackLoopbackPublish {
                            track_index,
                            publish,
                        } => {
                            runtime.update_track_loopback_publish(track_index, publish);
                        }
                        EngineCommand::SetTrackJamPublish { track_index, slot } => {
                            runtime.update_track_jam_publish(track_index, slot);
                        }
                        EngineCommand::SetJamMultitrackPairs { pairs } => {
                            runtime.apply_jam_multitrack_pairs(&pairs);
                        }
                        // This callback path does not record a realtime render
                        // (the DAUx callback does); it still takes the
                        // reference so the capture is released in step.
                        EngineCommand::SetRenderCapture(capture) => {
                            runtime.render_capture = capture;
                        }
                        // Nor a loopback take: same reference rule.
                        EngineCommand::SetLoopbackRecord(recorder) => {
                            runtime.loopback_record = recorder;
                        }
                        EngineCommand::SetTrackPreviewMode { track_id, value } => {
                            runtime.update_track_preview_mode(&track_id, RuntimePreviewMode::from_code(value));
                        }
                        EngineCommand::SetSoundfontChannels {
                            track_index,
                            channels,
                        } => {
                            runtime.update_soundfont_channels(track_index, &channels);
                        }
                        EngineCommand::SetInsertParam {
                            track_index,
                            insert_index,
                            param_id,
                            value,
                            mut change,
                        } => {
                            runtime.update_insert_param_at(
                                track_index,
                                insert_index,
                                param_id,
                                value,
                                change.as_deref_mut(),
                            );
                            if let Some(change) = change {
                                crate::graveyard::retire_insert_param_change(change);
                            }
                        }
                        EngineCommand::MidiPreviewNoteOn {
                            track_id,
                            channel,
                            pitch,
                            velocity,
                        } => {
                            runtime.midi_preview_note_on(&track_id, channel, pitch, velocity);
                        }
                        EngineCommand::MidiPreviewNoteOff {
                            track_id,
                            channel,
                            pitch,
                        } => {
                            runtime.midi_preview_note_off(&track_id, channel, pitch);
                        }
                        EngineCommand::MidiPreviewControlChange {
                            track_id,
                            channel,
                            controller,
                            value,
                        } => {
                            runtime.midi_preview_control_change(
                                &track_id,
                                channel,
                                controller,
                                value,
                            );
                        }
                        EngineCommand::MidiPreviewAllNotesOff { track_id } => {
                            runtime.midi_preview_all_notes_off(&track_id);
                        }
                        EngineCommand::PluginPreviewNoteOn {
                            track_id,
                            plugin_instance_id,
                            channel,
                            pitch,
                            velocity,
                        } => {
                            runtime.bridge_preview_note_on(
                                &track_id,
                                &plugin_instance_id,
                                channel,
                                pitch,
                                velocity,
                            );
                        }
                        EngineCommand::PluginPreviewNoteOff {
                            track_id,
                            plugin_instance_id,
                            channel,
                            pitch,
                        } => {
                            runtime.bridge_preview_note_off(
                                &track_id,
                                &plugin_instance_id,
                                channel,
                                pitch,
                            );
                        }
                        EngineCommand::PluginPreviewControlChange {
                            track_id,
                            plugin_instance_id,
                            channel,
                            controller,
                            value,
                        } => {
                            runtime.bridge_preview_control_change(
                                &track_id,
                                &plugin_instance_id,
                                channel,
                                controller,
                                value,
                            );
                        }
                        EngineCommand::PluginPreviewAllNotesOff {
                            track_id,
                            plugin_instance_id,
                        } => {
                            runtime.bridge_preview_all_notes_off(&track_id, &plugin_instance_id);
                        }
                        EngineCommand::PluginPreviewPitchBend {
                            track_id,
                            plugin_instance_id,
                            channel,
                            value,
                        } => {
                            runtime.bridge_preview_pitch_bend(
                                &track_id,
                                &plugin_instance_id,
                                channel,
                                value,
                            );
                        }
                    }
                }
                metronome.follow_record_pdc_bypass(&mut runtime, &shared, playing_local);

                // ── 2. Read shared control state (Relaxed OK for audio) ──────
                let master_vol = f32_load(shared.master_volume.load(Ordering::Relaxed));
                // Re-sync local osc flags from atomics in case they changed externally.
                let tone_on = shared.test_tone_enabled.load(Ordering::Relaxed);
                let tone_freq = f32_load(shared.test_tone_freq.load(Ordering::Relaxed));
                if tone_freq != osc_freq {
                    osc_freq = tone_freq;
                    osc_l.set_frequency(tone_freq as f64);
                    osc_r.set_frequency(tone_freq as f64);
                }
                let gen_tone = tone_on || osc_on;

                // ── 3. Fill output buffer ────────────────────────────────────
                let mut peak_l = 0.0f32;
                let mut peak_r = 0.0f32;
                let mut sum_sq_l = 0.0f32;
                let mut sum_sq_r = 0.0f32;
                let mut frames = 0u64;
                let loop_bounds = if playing_local {
                    transport::active_loop_bounds(&shared)
                } else {
                    None
                };
                let raw_base_sample = shared.position_samples.load(Ordering::Relaxed);
                let base_sample = transport::normalize_loop_position(raw_base_sample, loop_bounds);
                if base_sample != raw_base_sample {
                    shared.position_samples.store(base_sample, Ordering::Relaxed);
                    runtime.reset_midi_playback(base_sample);
                    metronome.reset_metronome_schedule(base_sample, output_sample_rate);
                }
                runtime.begin_meter_block();
                // Bank/program selections not yet sent, playing or not.
                runtime.flush_midi_programs();

                // MIDI scheduling — once per block when playing.
                let mut end_loop_midi_reset = None;
                if playing_local && ch > 0 {
                    let frames_needed = data.len().checked_div(ch).unwrap_or(0) as u64;
                    end_loop_midi_reset = schedule_midi_render_block(
                        &mut runtime,
                        base_sample,
                        frames_needed,
                        loop_bounds,
                    );
                }

                // Park the Browser preview playhead every block.
                shared.publish_audition_position(audition.position_seconds());
                // Diagnostics only. Nothing below gates rendering on these any
                // more: the graph runs every block the engine renders.
                if !playing_local && callback_debug_enabled() {
                    let active_notes: u32 = runtime
                        .midi_tracks
                        .iter()
                        .map(|mt| mt.preview_active.len() as u32)
                        .sum();
                    if active_notes != metronome.prev_logged_preview_notes {
                        metronome.prev_logged_preview_notes = active_notes;
                        eprintln!(
                            "[RealtimeGraph] transport=stopped active_preview_notes={active_notes} rendering=true"
                        );
                    }
                }

                if ch >= 2 {
                    let frames_needed = data.len() / ch;
                    let scratch_len = frames_needed * ch;
                    if block_scratch.len() < scratch_len {
                        block_scratch.resize(scratch_len, 0.0);
                    }
                    let scratch = &mut block_scratch[..scratch_len];
                    // Bridged-insert compensation follows the configured
                    // period, not this callback's size (#377).
                    runtime.bridge_period_frames = shared.bridge_period_frames_for_block(
                        frames_needed.min(u32::MAX as usize) as u32,
                    );
                    frames = render_project_block_interleaved(
                        &mut runtime,
                        base_sample,
                        master_vol,
                        scratch,
                        ch,
                        playing_local,
                        shared.time_sig_num.load(Ordering::Relaxed),
                        shared.time_sig_den.load(Ordering::Relaxed),
                        loop_bounds,
                    );
                    audition.mix_into(scratch, ch);
                    if !render_path_logged {
                        render_path_logged = true;
                        if callback_debug_enabled() {
                            eprintln!(
                                "[SphereAudio callback] renderPath=legacy-block frames={} channels={} tracks={}",
                                frames,
                                ch,
                                runtime.tracks.len()
                            );
                        }
                    }
                    if gen_tone {
                        for frame in scratch.chunks_mut(ch) {
                            let tone_l = osc_l.next_sample() * TEST_TONE_AMPLITUDE * master_vol;
                            let tone_r = osc_r.next_sample() * TEST_TONE_AMPLITUDE * master_vol;
                            frame[0] += tone_l;
                            frame[1] += tone_r;
                        }
                    }
                    let metronome_graph_max_samples =
                        crate::backend::render::metronome_graph_max_latency_samples(&runtime);
                    let metronome_delay_samples =
                        crate::backend::render::metronome_compensation_delay_samples(&runtime);
                    let mut segment_sample = base_sample;
                    let mut callback_offset = 0usize;
                    let mut remaining = frames;
                    while remaining > 0 {
                        let segment_frames = transport::segment_frames_until_loop_wrap(
                            segment_sample,
                            remaining,
                            loop_bounds,
                        );
                        for i in 0..segment_frames as usize {
                            let frame = &mut scratch[(callback_offset + i) * ch
                                ..(callback_offset + i) * ch + ch];
                            let click = metronome.metronome_sample(
                                segment_sample + i as u64,
                                (callback_offset + i) as u64,
                                output_sample_rate,
                                playing_local,
                                metronome_graph_max_samples,
                                metronome_delay_samples,
                            );
                            if click != 0.0 {
                                frame[0] += click * master_vol;
                                frame[1] += click * master_vol;
                            }
                        }
                        callback_offset += segment_frames as usize;
                        remaining -= segment_frames;
                        if remaining == 0 {
                            break;
                        }
                        let (next_sample, wrapped) = transport::advance_loop_position(
                            segment_sample,
                            segment_frames,
                            loop_bounds,
                        );
                        if wrapped {
                            metronome.reset_metronome_schedule(next_sample, output_sample_rate);
                        }
                        segment_sample = next_sample;
                    }
                    // (Legacy NAPI path) Software monitoring is handled by the
                    // DAUx/cpal render kernel via the input ring; the old
                    // sample-and-hold monitor was removed (warble).
                    // Stereo and wider layouts always take this block kernel.
                    // Mono stays on the per-sample path.
                    for (out_frame, frame) in data.chunks_mut(ch).zip(scratch.chunks(ch)) {
                        let l = frame[0];
                        let r = frame[1];
                        out_frame[0] = T::dithered_from_f32(l, &mut dither);
                        out_frame[1] = T::dithered_from_f32(r, &mut dither);
                        for extra in out_frame.iter_mut().skip(2) {
                            *extra = T::dithered_from_f32(0.0, &mut dither);
                        }
                        peak_l = peak_l.max(l.abs());
                        peak_r = peak_r.max(r.abs());
                        sum_sq_l += l * l;
                        sum_sq_r += r * r;
                    }
                } else if ch == 1 {
                    let metronome_graph_max_samples =
                        crate::backend::render::metronome_graph_max_latency_samples(&runtime);
                    let metronome_delay_samples =
                        crate::backend::render::metronome_compensation_delay_samples(&runtime);
                    for sample in data.iter_mut() {
                        let tone = if gen_tone {
                            osc_l.next_sample() * TEST_TONE_AMPLITUDE * master_vol
                        } else {
                            0.0
                        };
                        let (project_l, project_r) = if playing_local {
                            render_project_sample(&mut runtime, base_sample + frames, master_vol)
                        } else {
                            (0.0, 0.0)
                        };
                        let click = metronome.metronome_sample(
                            base_sample + frames,
                            frames,
                            output_sample_rate,
                            playing_local,
                            metronome_graph_max_samples,
                            metronome_delay_samples,
                        ) * master_vol;
                        let value = tone + (project_l + project_r) * 0.5 + click;
                        *sample = T::dithered_from_f32(value, &mut dither);
                        peak_l = peak_l.max(value.abs());
                        sum_sq_l += value * value;
                        frames += 1;
                    }
                }

                if shared.live_input_active.load(Ordering::Relaxed) {
                    let input_l = f32_load(shared.live_input_l.load(Ordering::Relaxed));
                    let input_r = f32_load(shared.live_input_r.load(Ordering::Relaxed));
                    let source_pair = shared.monitor_source_pair();
                    runtime.accumulate_live_input_meters(input_l, input_r, source_pair);
                }

                // ── 4. Compute meters (no allocation) ────────────────────────
                let rms_l = if frames > 0 {
                    (sum_sq_l / frames as f32).sqrt()
                } else {
                    0.0
                };
                let (pk_r, rms_r) = if ch >= 2 {
                    (
                        peak_r,
                        if frames > 0 {
                            (sum_sq_r / frames as f32).sqrt()
                        } else {
                            0.0
                        },
                    )
                } else {
                    (peak_l, rms_l)
                };
                runtime.end_meter_block(frames);

                prev_peak_l = smooth_peak(prev_peak_l, peak_l, PEAK_DECAY);
                prev_peak_r = smooth_peak(prev_peak_r, pk_r, PEAK_DECAY);

                shared
                    .peak_l
                    .store(f32_store(prev_peak_l), Ordering::Relaxed);
                shared
                    .peak_r
                    .store(f32_store(prev_peak_r), Ordering::Relaxed);
                shared.rms_l.store(f32_store(rms_l), Ordering::Relaxed);
                shared.rms_r.store(f32_store(rms_r), Ordering::Relaxed);

                // Advance position counter.
                if playing_local && ch > 0 {
                    shared.note_output_block(base_sample, frames);
                    let (next_position, _) =
                        transport::advance_loop_position(base_sample, frames, loop_bounds);
                    shared.position_samples.store(next_position, Ordering::Relaxed);
                    if let Some(reset_sample) = end_loop_midi_reset {
                        runtime.loop_wrap_midi_playback(reset_sample, 0);
                        metronome.reset_metronome_schedule(reset_sample, output_sample_rate);
                    }
                }

                // Voice load for the transport meter. Summed here rather
                // than inside the render pass because this is the one place
                // that owns the whole runtime for the block and also holds
                // `shared` — one relaxed store per callback, O(tracks) with no
                // allocation.
                let voices: u32 = runtime.tracks.iter().map(|track| track.active_voices).sum();
                shared.active_voices.store(voices, Ordering::Relaxed);

                // ── Dropout watchdog: publish timing + classify this block ────
                shared.output_cb_count.fetch_add(1, Ordering::Relaxed);
                let elapsed_us = cb_started.elapsed().as_micros().min(u32::MAX as u128) as u32;
                let block_frames = data.len().checked_div(ch).unwrap_or(0);
                crate::engine::record_output_callback_timing(
                    &shared,
                    elapsed_us,
                    block_frames,
                    output_sample_rate,
                    true,
                );
            },
            move |err| {
                eprintln!("[SphereAudio] Stream error: {err}");
            },
            None,
        )
        .map_err(|e| e.to_string())?;

    Ok(stream)
}

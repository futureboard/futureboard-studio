//! The recording API: starting and stopping recording on every input path.

use super::*;

impl EngineInner {
    pub(super) fn find_input_device_for_active_backend(
        &self,
        device_id: Option<&str>,
    ) -> Result<cpal::Device, SphereAudioError> {
        let backend = self.daux_config.lock().backend.clone();
        match backend {
            // An ASIO driver is one duplex session. Opening a second input
            // stream would re-prepare the shared buffer set and kill the
            // output side — capture always taps the session input callback.
            BackendKind::Asio => Err(SphereAudioError::NativeError(
                "ASIO input is captured by the active duplex session; \
                 no separate input device is ever opened"
                    .into(),
            )),
            _ => recording::find_input_device(device_id),
        }
    }

    /// Live capabilities of the open ASIO session (None for other backends or
    /// while no stream is open). Device enumeration overlays these onto the
    /// registry-derived driver list so only the *active* driver reports real
    /// channel counts — installed-but-idle drivers are never instantiated just
    /// to fill a dropdown.
    pub fn asio_session_caps(&self) -> Option<crate::backend::AsioSessionCaps> {
        self.asio_caps.lock().clone()
    }

    /// Begin writing armed tracks to disk. WASAPI-family backends open a
    /// dedicated capture stream; the ASIO backend installs a record sink into
    /// the already-running duplex session (the driver/stream is never touched).
    pub fn start_recording(&self, config: JsStartRecordingConfig) -> Result<(), SphereAudioError> {
        // Decided as the take arms. A take started while the transport already
        // runs keeps the compensation it is being heard with: turning it off
        // mid-play would jump every compensated track by its delay.
        let bypass_pdc = self
            .shared
            .record_pdc_bypass_enabled
            .load(Ordering::Relaxed)
            && !self.shared.playing.load(Ordering::Relaxed);
        self.start_recording_session(config)?;
        self.shared
            .record_pdc_bypass
            .store(bypass_pdc, Ordering::Relaxed);
        Ok(())
    }

    pub(super) fn start_recording_session(
        &self,
        config: JsStartRecordingConfig,
    ) -> Result<(), SphereAudioError> {
        let mut guard = self.recording.lock();
        if guard.is_some() {
            return Err(SphereAudioError::NativeError(
                "A recording session is already active".to_string(),
            ));
        }
        // The last take's files are still closing. Its stop holds the one
        // pending slot, and a second stop would overwrite it, losing that take;
        // finalizing it also re-syncs the input stream under a running take.
        // The caller collects it first (`poll_stop_recording`).
        if self.pending_stop.lock().is_some() {
            return Err(SphereAudioError::NativeError(
                "The previous take is still being saved".to_string(),
            ));
        }
        let monitor_mix = config.monitor_mix;

        // Tracks whose input is another track are recorded from the render
        // callback, not from a capture device, so they take neither the ASIO
        // nor the capture-stream path below — and work with no interface open.
        // One take is one source family: the two run on different callbacks
        // with different clocks, and one writer cannot interleave them.
        let loopback_tracks = config
            .tracks
            .iter()
            .filter(|track| track.loopback_source_track_id.is_some())
            .count();
        if loopback_tracks > 0 {
            if loopback_tracks != config.tracks.len() {
                return Err(SphereAudioError::InvalidConfig(
                    "armed tracks mix hardware inputs and track inputs; record them in separate takes"
                        .to_string(),
                ));
            }
            self.start_recording_loopback(config, &mut guard)?;
            drop(guard);
            self.warm_output_for_monitoring();
            return Ok(());
        }

        #[cfg(all(target_os = "windows", feature = "asio"))]
        if self.daux_config.lock().backend == BackendKind::Asio {
            return self.start_recording_asio(config, monitor_mix, &mut guard);
        }

        // Preferred path: attach the take to the capture stream that is already
        // running. Nothing in the device path moves, so Record does not stutter
        // and neither does Stop.
        match self.start_recording_live_tap(&config, monitor_mix, &mut guard) {
            LiveTapStart::Started => {
                drop(guard);
                self.warm_output_for_monitoring();
                return Ok(());
            }
            LiveTapStart::Failed(error) => {
                drop(guard);
                return Err(error);
            }
            // No usable live stream — fall through and open one for the take.
            LiveTapStart::Unavailable => {}
        }

        // Single-capture-stream invariant: the recording stream becomes the sole
        // input device client during a take (it feeds the monitor ring, preview
        // ring, and file writer). Stop the standalone monitor stream so two
        // WASAPI shared clients don't contend on the same endpoint — a likely
        // source of jitter while recording.
        self.stop_live_input_stream();
        let session = self
            .find_input_device_for_active_backend(config.input_device_id.as_deref())
            .and_then(|device| {
                let preferred_period = {
                    let st = self.status.lock();
                    if st.buffer_size > 0 {
                        Some(st.buffer_size)
                    } else {
                        self.daux_config.lock().buffer_size.filter(|&p| p > 0)
                    }
                };
                recording::start_recording_with_device(
                    config,
                    Arc::clone(&self.shared),
                    monitor_mix,
                    device,
                    preferred_period,
                )
            });
        match session {
            Ok(session) => {
                if recording_debug_enabled() {
                    eprintln!(
                        "[recording] session started tracks={} start_beat={:.3}",
                        session.track_count, session.start_beat
                    );
                }
                *guard = Some(session);
                drop(guard);
                // Ensure the output stream runs so monitoring is audible.
                self.warm_output_for_monitoring();
                Ok(())
            }
            Err(e) => {
                drop(guard);
                // Restore standalone monitoring from the last snapshot on failure.
                if let Some(snapshot) = self.project.lock().clone() {
                    let _ = self.sync_live_input_stream(&snapshot);
                }
                Err(e)
            }
        }
    }

    /// Start a take of tracks whose input is another track (`trk:` routes).
    ///
    /// The recorder goes into the graph the render callback runs, which needs
    /// an output stream and nothing else. Each destination must actually be
    /// resolved to its source in this graph: a route the engine refused (the
    /// source is gone, or the destination already feeds it) would record
    /// silence, so it is refused here with the reason instead.
    pub(super) fn start_recording_loopback(
        &self,
        config: JsStartRecordingConfig,
        guard: &mut Option<RecordingSession>,
    ) -> Result<(), SphereAudioError> {
        let sample_rate = self.shared.sample_rate.load(Ordering::Relaxed);
        if sample_rate == 0 {
            return Err(SphereAudioError::NativeError(
                "recording a track's output needs a running audio output; open an audio device first"
                    .to_string(),
            ));
        }
        let track_ids: Vec<String> = config
            .tracks
            .iter()
            .map(|track| track.track_id.clone())
            .collect();
        let indices = {
            let project = self.project.lock();
            let snapshot = project.as_ref().ok_or_else(|| {
                SphereAudioError::InvalidConfig("no project is loaded".to_string())
            })?;
            for track in &config.tracks {
                let routed = snapshot
                    .tracks
                    .iter()
                    .find(|candidate| candidate.id == track.track_id)
                    .and_then(|candidate| candidate.input_source.loopback_track_id());
                if routed != track.loopback_source_track_id.as_deref() {
                    return Err(SphereAudioError::InvalidConfig(format!(
                        "{}: the engine has not applied this track's input yet; re-arm the track",
                        track.name
                    )));
                }
            }
            render_capture_indices(snapshot, &track_ids)
        };
        {
            let runtime = self.runtime.lock();
            for (track, index) in config.tracks.iter().zip(&indices) {
                let resolved = index
                    .and_then(|index| runtime.tracks.get(index))
                    .is_some_and(|candidate| candidate.input_source.is_loopback());
                if !resolved {
                    return Err(SphereAudioError::InvalidConfig(format!(
                        "{}: its input track cannot be recorded — it no longer exists, or {} \
                         already feeds it, which would be a feedback loop",
                        track.name, track.name
                    )));
                }
            }
        }

        let (session, recorder) =
            recording::start_recording_loopback_tap(config, Arc::clone(&self.shared), sample_rate)?;
        let recorder = Arc::new(recorder);
        recorder.map_destinations(&indices);
        // Registered before it is sent, so a project sync from here on builds
        // its graph with the recorder in it.
        *self.loopback_record.lock() = Some((recorder.clone(), track_ids));
        self.runtime.lock().loopback_record = Some(recorder.clone());
        if let Err(error) = self.send_command(EngineCommand::SetLoopbackRecord(Some(recorder))) {
            self.runtime.lock().loopback_record = None;
            let recorder = self.loopback_record.lock().take();
            // Unwind the armed pipeline; the take never ran.
            let _ = recording::stop_recording(session);
            drop(recorder);
            return Err(match error {
                SphereAudioError::EngineNotOpen => SphereAudioError::NativeError(
                    "recording a track's output needs a running audio output; open an audio device first"
                        .to_string(),
                ),
                other => other,
            });
        }
        if recording_debug_enabled() {
            eprintln!(
                "[recording] session started on the track loopback tap tracks={} start_beat={:.3}",
                session.track_count, session.start_beat
            );
        }
        *guard = Some(session);
        Ok(())
    }

    /// Try to run the take on the live input stream that is already open.
    ///
    /// Declines — rather than failing — when there is no live stream, or when
    /// the take needs an input channel the open stream does not carry. Those
    /// are both cases the own-stream path can still serve, and a take that can
    /// be recorded must not be refused because the preferred route was busy.
    pub(super) fn start_recording_live_tap(
        &self,
        config: &JsStartRecordingConfig,
        monitor_mix: bool,
        guard: &mut Option<RecordingSession>,
    ) -> LiveTapStart {
        let (channels, sample_rate) = {
            let live = self.live_input.lock();
            let Some(handle) = live.as_ref() else {
                return LiveTapStart::Unavailable;
            };
            // A previous take's sink may still be waiting to be dropped.
            handle.tap.drain_trash();
            (handle.channels, handle.sample_rate)
        };
        if channels == 0 || sample_rate == 0 {
            return LiveTapStart::Unavailable;
        }
        // The open stream cannot carry a channel this take needs. Decline and
        // let the own-stream path negotiate a configuration that can.
        if config
            .tracks
            .iter()
            .flat_map(|track| track.input_channels.iter())
            .any(|channel| *channel as usize >= channels)
        {
            if recording_debug_enabled() {
                eprintln!(
                    "[recording] live-stream tap declined: take needs a channel beyond the open \
                     stream's {channels}"
                );
            }
            return LiveTapStart::Unavailable;
        }

        let (session, sink) = match recording::start_recording_live_tap(
            config.clone(),
            Arc::clone(&self.shared),
            monitor_mix,
            channels,
            sample_rate,
        ) {
            Ok(pair) => pair,
            // A real failure — no armed track, an unwritable Recordings folder,
            // a bad channel selection. The own-stream path would fail the same
            // way, so report it rather than retrying into the same error.
            Err(error) => return LiveTapStart::Failed(error),
        };

        let installed = {
            let live = self.live_input.lock();
            match live.as_ref() {
                Some(handle) => handle.tap.install(sink).is_ok(),
                None => false,
            }
        };
        if !installed {
            // The stream went away between the two locks, or its callback has
            // stopped draining. Unwind the armed pipeline (it finalizes and
            // reports; the results are discarded because no take ever ran).
            let _ = recording::stop_recording(session);
            return LiveTapStart::Unavailable;
        }

        if recording_debug_enabled() {
            eprintln!(
                "[recording] session started on the live-stream tap tracks={} start_beat={:.3}",
                session.track_count, session.start_beat
            );
        }
        *guard = Some(session);
        LiveTapStart::Started
    }

    /// ASIO take: build the writer pipeline, then hand the record sink to the
    /// session's input callback over its bounded command queue. Monitoring,
    /// the stream, and the driver are untouched — recording never depends on
    /// monitoring being enabled, and starting a take never recreates streams.
    #[cfg(all(target_os = "windows", feature = "asio"))]
    pub(super) fn start_recording_asio(
        &self,
        config: JsStartRecordingConfig,
        monitor_mix: bool,
        guard: &mut Option<RecordingSession>,
    ) -> Result<(), SphereAudioError> {
        use crate::backend::asio_session::AsioInputCommand;

        let caps = self.asio_caps.lock().clone().ok_or_else(|| {
            SphereAudioError::NativeError(
                "ASIO recording requires an open ASIO stream — apply the audio settings first"
                    .into(),
            )
        })?;
        if caps.input_channels == 0 {
            let detail = self
                .status
                .lock()
                .last_daux_error
                .clone()
                .unwrap_or_else(|| {
                    format!("ASIO driver '{}' has no usable input channels", caps.driver)
                });
            return Err(SphereAudioError::NativeError(detail));
        }

        let (session, sink) = recording::start_recording_asio_tap(
            config,
            Arc::clone(&self.shared),
            monitor_mix,
            caps.input_channels,
            caps.sample_rate,
            caps.buffer_size,
        )?;

        let install_result = {
            let stream_guard = self.active_stream.lock();
            match stream_guard.as_ref().and_then(|s| s.as_asio_duplex()) {
                None => Err(SphereAudioError::NativeError(
                    "ASIO session is not open".into(),
                )),
                Some(handle) => {
                    // Dispose sinks from a previous take before installing.
                    handle.drain_trashed_sinks();
                    handle
                        .input_cmd_tx
                        .try_send(AsioInputCommand::SetRecordSink(Box::new(sink)))
                        .map_err(|_| {
                            SphereAudioError::NativeError(
                                "ASIO input command queue is full; cannot start recording".into(),
                            )
                        })
                }
            }
        };

        if let Err(error) = install_result {
            // Unwind the already-armed writer pipeline (finalizes and reports;
            // results are discarded because the take never started).
            let _ = recording::stop_recording(session);
            return Err(error);
        }

        *guard = Some(session);
        self.warm_output_for_monitoring();
        Ok(())
    }

    /// Stop the active recording, finalize WAV files, and return per-track results.
    /// Finalize the active take and hand back its results.
    ///
    /// Takes the session out of the engine, so the next Record always builds a
    /// new one: new take id, new file, new clip. There is no path that reuses a
    /// writer, and a second call without a second Record reports "no active
    /// recording session" rather than quietly appending to the last take.
    /// Detach the take and wait for its files, on this thread.
    ///
    /// The behaviour every caller had before the stop was split. Kept because a
    /// shutdown path and a test both genuinely want to block; the UI does not,
    /// and uses `begin_stop_recording` + `poll_stop_recording` instead.
    pub fn stop_recording(&self) -> Result<Vec<JsRecordingResult>, SphereAudioError> {
        self.begin_stop_recording()?;
        self.wait_stop_recording()
    }

    /// Detach the take. Returns as soon as capture has stopped; the files are
    /// closed on the writer's thread and collected by [`Self::poll_stop_recording`].
    pub fn begin_stop_recording(&self) -> Result<(), SphereAudioError> {
        let session = self.recording.lock().take().ok_or_else(|| {
            SphereAudioError::NativeError("No active recording session".to_string())
        })?;
        // Belt and braces with `pause`: whichever arrives first, the writer has
        // stopped consuming input before a single sample of finalization work.
        session.recording_active.store(false, Ordering::Relaxed);
        self.shared.recording_active.store(false, Ordering::Relaxed);
        let pdc_bypassed = self.shared.record_pdc_bypass.swap(false, Ordering::Relaxed);
        if recording_debug_enabled() {
            eprintln!(
                "[recording] session finalizing after {:.3}s",
                session.started_at.elapsed().as_secs_f64()
            );
        }

        // ASIO tap: detach the record sink from the session input callback so
        // the callback releases its channel senders on the next block. The
        // writer additionally exits on the session's stop flag, so a stalled
        // driver still finalizes the take.
        #[cfg(all(target_os = "windows", feature = "asio"))]
        let asio_tap = session.is_asio_tap();
        #[cfg(all(target_os = "windows", feature = "asio"))]
        if asio_tap {
            if let Some(handle) = self
                .active_stream
                .lock()
                .as_ref()
                .and_then(|s| s.as_asio_duplex())
            {
                let _ = handle
                    .input_cmd_tx
                    .try_send(crate::backend::asio_session::AsioInputCommand::ClearRecordSink);
            }
        }

        // Live-stream tap: detach the record fan-out. The callback parks the
        // sink (its sender with it) until the take has finalized, so the disk
        // writer ends on the stop flag, not on a disconnect. The stream itself
        // keeps running, so monitoring never drops out and the
        // end of a take is silent in the way it should be.
        let live_tap = session.is_live_stream_tap();
        if live_tap {
            if let Some(handle) = self.live_input.lock().as_ref() {
                handle.tap.clear();
            }
        }

        // Loopback take: take the recorder out of the graph. Its `active` flag
        // (the session's) is already down, so it records nothing more; the
        // control thread keeps its reference until the files are closed, so
        // the callback never drops the last one.
        let loopback_tap = session.is_loopback_tap();
        let loopback_recorder = if loopback_tap {
            let recorder = self.loopback_record.lock().take();
            self.runtime.lock().loopback_record = None;
            let _ = self.send_command(EngineCommand::SetLoopbackRecord(None));
            recorder.map(|(recorder, _)| recorder)
        } else {
            None
        };

        // How far after its declared start the take really began capturing.
        // Read before the session is consumed.
        let armed_start_sample = session.armed_start_sample;
        let first_capture_sample = session.first_capture_sample.load(Ordering::Relaxed);

        let pending = recording::begin_stop_recording(session);
        *self.pending_stop.lock() = Some(PendingStopContext {
            pending,
            loopback_recorder,
            placement: TakePlacement {
                armed_start_sample,
                first_capture_sample,
                live_tap,
                loopback_tap,
                pdc_bypassed,
                #[cfg(all(target_os = "windows", feature = "asio"))]
                asio_tap,
            },
        });
        Ok(())
    }

    /// Results of a detached take, or `None` while the writer is still going.
    ///
    /// Never blocks. The caller polls this from wherever it already ticks; the
    /// take's files close on the writer's thread and the UI keeps drawing.
    pub fn poll_stop_recording(&self) -> Option<Result<Vec<JsRecordingResult>, SphereAudioError>> {
        let ready = {
            let mut slot = self.pending_stop.lock();
            let context = slot.as_mut()?;
            match context.pending.poll() {
                None => return None,
                Some(Err(error)) => {
                    *slot = None;
                    return Some(Err(error));
                }
                Some(Ok(results)) => {
                    let context = slot.take().expect("checked above");
                    (results, context)
                }
            }
        };
        let (results, context) = ready;
        Some(Ok(self.finish_stop_recording(results, context.placement())))
    }

    /// Block until a detached take's files are closed.
    ///
    /// The behaviour `stop_recording` always had, for callers with nothing else
    /// to do — shutdown, and tests that want the files on the next line.
    pub fn wait_stop_recording(&self) -> Result<Vec<JsRecordingResult>, SphereAudioError> {
        let Some(context) = self.pending_stop.lock().take() else {
            return Ok(Vec::new());
        };
        let placement = context.placement();
        let results = context.pending.wait()?;
        Ok(self.finish_stop_recording(results, placement))
    }

    /// Everything that happens once a take's files are closed: release the
    /// taps, place the audio against the tempo map, re-sync monitoring.
    ///
    /// Identical whichever way the caller waited, which is why it is one
    /// function — a take placed differently depending on how the stop was
    /// awaited would be the worst possible outcome of this split.
    pub(super) fn finish_stop_recording(
        &self,
        mut results: Vec<JsRecordingResult>,
        placement: TakePlacement,
    ) -> Vec<JsRecordingResult> {
        let live_tap = placement.live_tap;
        let armed_start_sample = placement.armed_start_sample;
        let first_capture_sample = placement.first_capture_sample;
        #[cfg(all(target_os = "windows", feature = "asio"))]
        let asio_tap = placement.asio_tap;

        if live_tap {
            if let Some(handle) = self.live_input.lock().as_ref() {
                handle.tap.drain_trash();
            }
        }

        #[cfg(all(target_os = "windows", feature = "asio"))]
        if asio_tap {
            if let Some(handle) = self
                .active_stream
                .lock()
                .as_ref()
                .and_then(|s| s.as_asio_duplex())
            {
                handle.drain_trashed_sinks();
            }
        }

        let runtime = self.runtime.lock();
        let buffer_frames = self.status.lock().buffer_size;
        let sample_rate = runtime.sample_rate.max(1) as f64;
        // A take is armed a block or two — or a whole count-in — before the
        // first sample it captures. Placing it on the beat it was *asked* for
        // puts the audio that far early; placing it on the beat it actually
        // started on is what makes two takes cut against each other. Resolved
        // through the tempo map, so a marker between the two lands right.
        let capture_offset_beats =
            if first_capture_sample != u64::MAX && first_capture_sample > armed_start_sample {
                runtime
                    .tempo_map
                    .beat_at_samples(first_capture_sample, sample_rate)
                    - runtime
                        .tempo_map
                        .beat_at_samples(armed_start_sample, sample_rate)
            } else {
                0.0
            };
        // Round-trip compensation, applied here and only here: the latency
        // measured during the take (`latency_seconds`: output play-out plus
        // input capture) when the backend timestamped it, else driver-reported
        // latencies when the ASIO session provides them, else the 2×buffer
        // estimate. The UI used to subtract `latency_seconds` a second time on
        // top of this.
        let asio_round_trip = self.asio_caps.lock().as_ref().map(|caps| {
            caps.input_latency_samples
                .saturating_add(caps.output_latency_samples)
        });
        let round_trip_buffer = match asio_round_trip {
            Some(latency) if latency > 0 => latency,
            _ => buffer_frames.saturating_mul(2),
        };
        // How far behind the render position the performer heard the mix,
        // which is how far the click is held back to sound with it. Bypassed,
        // nothing waited for the slowest path: the mix came out as soon as
        // the master inserts let it through. Compensated, every track waited
        // for the slowest path and then went through the master inserts. That
        // does not depend on the armed track: subtracting only the armed
        // track's own plug-in latency put a take recorded while playing late
        // by the rest of the compensation.
        let path_samples = if placement.loopback_tap {
            // A loopback take is the source's post-fader block, taken before
            // the master inserts: compensated, it is held back by the graph's
            // own delay and no more; bypassed, by nothing the graph adds.
            if placement.pdc_bypassed {
                0
            } else {
                crate::backend::render::metronome_graph_max_latency_samples(&runtime)
            }
        } else if placement.pdc_bypassed {
            runtime.latency_graph.master_plugin_latency
        } else {
            crate::backend::render::metronome_compensation_delay_samples(&runtime)
        };
        for result in &mut results {
            if !runtime
                .tracks
                .iter()
                .any(|track| track.id == result.track_id)
            {
                continue;
            }
            let measured_round_trip = (result.latency_seconds * sample_rate).round() as u64;
            // Nothing went out through a converter and back in: the take was
            // recorded from the graph itself.
            let round_trip = if placement.loopback_tap {
                result.latency_seconds = 0.0;
                0
            } else if measured_round_trip > 0 {
                measured_round_trip
            } else {
                round_trip_buffer as u64
            };
            let offset_samples = round_trip.saturating_add(path_samples as u64);
            // A duration at the take. `beat_at_samples(offset)` alone would
            // convert it at the tempo of beat 0.
            let place_beat = result.start_beat + capture_offset_beats;
            let offset_beats =
                runtime
                    .tempo_map
                    .beats_for_sample_span(place_beat, offset_samples, sample_rate);
            result.start_beat = (place_beat - offset_beats).max(0.0);
        }
        drop(runtime);

        // Re-sync the monitoring input stream against the current routing.
        //
        // After an own-stream take this re-opens the stream the take took away.
        // After a tapped take it is nearly always free — `sync_live_input_stream`
        // keeps the open stream when the device is unchanged — but it still has
        // to run, because the user may have disarmed the last armed track
        // during the take and the stream should close with it.
        if let Some(snapshot) = self.project.lock().clone() {
            let _ = self.sync_live_input_stream(&snapshot);
        }
        results
    }

    /// Snapshot of current recording state (for UI status polling).
    pub fn get_recording_status(&self) -> JsRecordingStatus {
        match self.recording.lock().as_ref() {
            None => JsRecordingStatus::default(),
            Some(s) => JsRecordingStatus {
                active: true,
                duration_seconds: s.started_at.elapsed().as_secs_f64(),
                track_count: s.track_count as u32,
            },
        }
    }
}

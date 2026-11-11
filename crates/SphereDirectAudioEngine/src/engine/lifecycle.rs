//! Device open and close, stream start and stop.

use super::*;

impl EngineInner {
    pub fn get_version(&self) -> String {
        ENGINE_VERSION.to_string()
    }

    pub fn get_status(&self) -> JsSphereAudioStatus {
        let st = self.status.lock().clone();
        let sample_rate = self.shared.sample_rate.load(Ordering::Relaxed).max(1);
        let position_samples = self.shared.playback_position();
        JsSphereAudioStatus {
            available: true,
            running: st.running,
            stream_open: st.stream_open,
            transport_playing: self.shared.playing.load(Ordering::Relaxed),
            position_seconds: position_samples as f64 / sample_rate as f64,
            version: ENGINE_VERSION.to_string(),
            backend_name: cpal::default_host().id().name().to_string(),
            sample_rate: st.sample_rate,
            buffer_size: st.buffer_size,
            input_device: st.input_device,
            output_device: st.output_device,
            last_error: st.last_error,
        }
    }

    pub fn list_input_devices(&self) -> Vec<JsAudioDeviceInfo> {
        device::list_input_devices()
    }

    pub fn list_output_devices(&self) -> Vec<JsAudioDeviceInfo> {
        device::list_output_devices()
    }

    /// Open (or re-open) an audio output stream.
    /// Closes any existing stream first.
    pub fn open_device(&self, config: JsDeviceOpenConfig) -> Result<(), SphereAudioError> {
        self.prepare_stream_open();

        let (dev, dev_name) = device::resolve_output_device(config.output_device_id.as_deref())
            .map_err(SphereAudioError::DeviceNotFound)?;

        let candidates =
            output_stream_candidates(&dev, &config).map_err(SphereAudioError::StreamOpenFailed)?;

        let mut last_error = None;
        let mut selected = None;

        for candidate in candidates {
            let (tx, rx) = bounded::<EngineCommand>(512);
            let shared_cb = Arc::clone(&self.shared);
            shared_cb
                .sample_rate
                .store(candidate.config.sample_rate.0, Ordering::Relaxed);
            let initial_runtime = self
                .project
                .lock()
                .as_ref()
                .map(|snapshot| {
                    let mut audio_cache = self.audio_cache.lock();
                    match RuntimeProject::build(
                        snapshot,
                        candidate.config.sample_rate.0,
                        &mut audio_cache,
                        None,
                        self.pdc_enabled(),
                    ) {
                        Ok(mut runtime) => {
                            runtime.resolve_jam_inputs(snapshot, &self.shared.jam_bus);
                            runtime.resolve_loopback_inputs(snapshot);
                            runtime
                        }
                        Err(e) => {
                            eprintln!(
                                "[SphereAudio] open_device: invalid routing graph ({e}), keeping previous runtime"
                            );
                            let mut runtime = self.runtime.lock().clone();
                            runtime.sample_rate = candidate.config.sample_rate.0;
                            runtime
                        }
                    }
                })
                .unwrap_or_else(|| {
                    let mut runtime = self.runtime.lock().clone();
                    runtime.sample_rate = candidate.config.sample_rate.0;
                    runtime
                });
            *self.runtime.lock() = initial_runtime.clone();

            match build_output_stream(
                &dev,
                &candidate.config,
                candidate.sample_format,
                rx,
                shared_cb,
                initial_runtime,
            ) {
                Ok(stream) => {
                    selected = Some((candidate, tx, stream));
                    break;
                }
                Err(e) => {
                    last_error = Some(format!("{} config failed: {e}", candidate.label));
                }
            }
        }

        let (selected_config, tx, stream) = selected.ok_or_else(|| {
            SphereAudioError::StreamOpenFailed(
                last_error.unwrap_or_else(|| "no stream config candidates available".to_string()),
            )
        })?;

        let sample_rate = selected_config.config.sample_rate.0;
        let buffer_size = reported_buffer_size(&selected_config.config);
        eprintln!("[audio-engine] active_sample_rate={sample_rate} block_size={buffer_size}");

        // Store the stream and sender.
        *self.stream.lock() = Some(stream);
        *self.cmd_tx.lock() = Some(tx);

        // The callback's metronome/tempo/meter state starts at its defaults.
        self.reseed_stream_transport_state();

        // Update status.
        let mut st = self.status.lock();
        st.stream_open = true;
        st.running = false;
        st.sample_rate = sample_rate;
        st.buffer_size = buffer_size;
        // Bridged-insert compensation follows the stream's period, not the
        // size of each callback; 0 lets the stream resolve it on its first
        // block, and a steady different size the device delivers replaces it.
        self.shared.set_stream_bridge_period(buffer_size);
        st.output_device = Some(dev_name);
        st.last_error = None;

        Ok(())
    }

    /// Stop and close the audio stream.
    pub fn close_device(&self) {
        self.close_device_inner();
    }

    /// Control-thread preparation for a new output stream, before any backend
    /// builds one: close the previous stream, and make sure everything the new
    /// callback reaches through a process-wide lazy static already exists —
    /// some backends start calling back while the stream is still being built.
    pub(super) fn prepare_stream_open(&self) {
        self.close_device_inner();
        prime_audio_thread_resources();
    }

    pub(super) fn close_device_inner(&self) {
        // Preserve the input ring's SPSC contract across backend switches: the
        // standalone capture callback must be gone before a duplex ASIO producer
        // can start writing into the same ring.
        self.stop_live_input_stream();
        // Drop active DAUx stream (stops WASAPI exclusive thread or cpal stream).
        *self.active_stream.lock() = None;
        // Drop legacy cpal stream path.
        *self.stream.lock() = None;
        *self.cmd_tx.lock() = None;
        *self.asio_caps.lock() = None;

        let mut st = self.status.lock();
        st.stream_open = false;
        st.running = false;
    }

    /// Push the control thread's transport-clock and metronome state into a
    /// freshly opened stream.
    ///
    /// Every backend builds its `LocalAudioState` from scratch — metronome off,
    /// static 120 BPM, static 4/4 — and nothing else re-pushed it, so changing
    /// the audio device silently stopped the click (and moved the accents) until
    /// the next Play. All control-thread sends; the tempo map is gated by
    /// `apply_tempo_map`'s identity check on the other side, so re-seeding a map
    /// the new graph already carries costs the callback nothing.
    pub(super) fn reseed_stream_transport_state(&self) {
        let tempo_map = self.last_sent_tempo_map.lock().clone();
        let time_signature_map = self.last_sent_time_signature_map.lock().clone();
        let metronome_enabled = self.shared.metronome_enabled.load(Ordering::Relaxed);
        let metronome_gain = f32_load(self.shared.metronome_gain.load(Ordering::Relaxed));
        let metronome_sound = self.shared.metronome_sound.load(Ordering::Relaxed);
        // The new graph was built from the project, so it already plays this
        // map: no schedule to re-time, only the metronome to seed.
        let _ = self.send_command(EngineCommand::SetTempoMap(Box::new(
            crate::command::TempoMapChange {
                metronome_map: tempo_map.clone(),
                map: tempo_map,
                midi: None,
                sample_rate: 0,
            },
        )));
        crate::graveyard::prime();
        let _ = self.send_command(EngineCommand::SetTimeSignatureMap(Box::new(
            time_signature_map,
        )));
        let _ = self.send_command(EngineCommand::SetMetronomeVoice {
            volume: metronome_gain,
            sound: metronome_sound,
        });
        let _ = self.send_command(EngineCommand::SetMetronomeEnabled(metronome_enabled));
        // A new stream cannot be inside a scrub, but saying so explicitly costs
        // one queued command and removes a way for a reopen to come up silent.
        let _ = self.send_command(EngineCommand::SetMetronomeSuspended(false));
    }

    /// A device open without the device: the control-thread steps
    /// `open_device` takes around building its stream — the preparation before
    /// it, the graph the callback starts with, the command channel it reads and
    /// the re-seed after it. Returns the callback's end of the channel and its
    /// graph, for a test to play the callback.
    #[cfg(test)]
    pub(crate) fn open_test_stream(
        &self,
        sample_rate: u32,
    ) -> (crossbeam_channel::Receiver<EngineCommand>, RuntimeProject) {
        self.prepare_stream_open();
        self.shared
            .sample_rate
            .store(sample_rate, Ordering::Relaxed);
        let mut initial_runtime = self.get_initial_runtime(Some(sample_rate));
        // What every backend does with it before its first block.
        initial_runtime.retarget_sample_rate(sample_rate);
        let (tx, rx) = bounded::<EngineCommand>(512);
        *self.cmd_tx.lock() = Some(tx);
        self.reseed_stream_transport_state();
        (rx, initial_runtime)
    }

    /// Start audio playback (calls `stream.play()`).
    pub fn start(&self) -> Result<(), SphereAudioError> {
        {
            let st = self.status.lock();
            if st.stream_open && st.running {
                return Ok(());
            }
        }

        // Try DAUx active stream first — drop `active_stream` lock before `play()`.
        let daux_play = {
            let guard = self.active_stream.lock();
            guard.as_ref().map(|stream| stream.play())
        };
        if let Some(result) = daux_play {
            result.map_err(SphereAudioError::StreamStartFailed)?;
            self.shared.playing.store(false, Ordering::Relaxed); // transport starts paused
            self.status.lock().running = true;
            return Ok(());
        }
        // Legacy cpal path.
        let play_result = {
            let guard = self.stream.lock();
            guard
                .as_ref()
                .ok_or(SphereAudioError::EngineNotOpen)?
                .play()
                .map_err(|e| SphereAudioError::StreamStartFailed(e.to_string()))
        };
        play_result?;
        self.shared.playing.store(false, Ordering::Relaxed); // transport starts paused
        self.status.lock().running = true;
        Ok(())
    }

    /// Stop audio playback (calls `stream.pause()`).
    pub fn stop(&self) {
        if let Some(stream) = self.active_stream.lock().as_ref() {
            let _ = stream.pause();
        } else if let Some(stream) = self.stream.lock().as_ref() {
            let _ = stream.pause();
        }
        self.shared.playing.store(false, Ordering::Relaxed);
        self.status.lock().running = false;
    }

    /// Explicit shutdown for application exit — do not rely on `Drop` alone.
    /// Stops transport, pauses the device stream, and releases realtime resources.
    pub fn shutdown(&self) {
        self.shared
            .engine_state
            .store(AudioEngineState::ClosingProject as u8, Ordering::Relaxed);
        eprintln!("[AudioEngine] state -> ClosingProject");
        self.shared.playing.store(false, Ordering::Relaxed);
        let _ = self.send_command(EngineCommand::StopTransport);
        self.stop();
        self.close_device_inner();
    }
}

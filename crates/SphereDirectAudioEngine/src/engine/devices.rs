//! DAUx backends, ASIO routing and driver events, input test and latency information.

use super::*;

impl EngineInner {
    /// List all available DAUx backends on this platform.
    pub fn list_daux_backends(&self) -> Vec<JsDauxBackendInfo> {
        list_available_backends()
            .into_iter()
            .map(|b| JsDauxBackendInfo {
                id: b.id,
                name: b.name,
                available: b.available,
                is_default: b.is_default,
                description: b.description,
            })
            .collect()
    }

    /// Open (or re-open) a stream using the full DAUx config.
    /// This is the preferred method; `open_device()` is kept for backward compat.
    ///
    /// On failure the stream stays closed — the caller should call `open_daux`
    /// again with a safe fallback config.  Use `open_daux_safe` if you want the
    /// engine to handle the fallback automatically.
    pub fn open_daux(&self, config: JsDauxConfig) -> Result<(), SphereAudioError> {
        self.prepare_stream_open();

        // Sanitize before open so a persisted Exclusive ASIO id cannot reach
        // the ASIO arm on Community / unentitled builds — fall back to Auto
        // (WASAPI Shared on Windows) instead of failing closed with no stream.
        let requested = BackendKind::from_id(&config.backend_id);
        let backend = BackendKind::sanitize_for_current_platform(requested.clone());
        if backend != requested {
            eprintln!(
                "[DAUx] open_daux: backend {} unavailable; falling back to {}",
                requested.id(),
                backend.id()
            );
        }
        let daux_cfg = DauxDeviceConfig {
            backend: backend.clone(),
            output_device_id: config.output_device_id.filter(|s| !s.is_empty()),
            input_device_id: None,
            sample_rate: config.sample_rate.filter(|&v| v > 0),
            buffer_size: config.buffer_size.filter(|&v| v > 0),
            mmcss_priority: config.mmcss_priority,
            safe_mode: config.safe_mode,
        };

        *self.daux_config.lock() = daux_cfg.clone();
        self.glitch_counter.store(0, Ordering::Relaxed);
        // Counters and the measured latency describe one open stream; a reopen
        // (device or buffer-size change) must not inherit the previous one's.
        self.shared.device_xruns.store(0, Ordering::Relaxed);
        self.shared
            .output_latency_secs
            .store(f32_store(0.0), Ordering::Relaxed);
        self.shared
            .record_input_latency_secs
            .store(f32_store(0.0), Ordering::Relaxed);
        // Until a backend that reports its own latencies (ASIO) is open, the
        // cpal callback measurement is the source again.
        self.shared
            .latency_from_driver
            .store(false, Ordering::Relaxed);

        let initial_runtime = self.get_initial_runtime(None);

        // Non-fatal open diagnostics (e.g. ASIO input degraded) applied after
        // the generic error-clearing below so they stay visible in Settings.
        #[allow(unused_mut)] // mutated only by the cfg-gated ASIO arm
        let mut deferred_open_warning: Option<String> = None;

        let stream = match backend {
            BackendKind::WasapiShared => {
                let handle = cpal_backend::open_wasapi_shared(
                    &daux_cfg,
                    Arc::clone(&self.shared),
                    initial_runtime,
                    Arc::clone(&self.glitch_counter),
                )?;
                let sr = handle.sample_rate;
                let bs = handle.buffer_size;
                let dev_name = handle.device_name.clone();
                let stream = ActiveStream::Cpal(handle);
                self.commit_stream_open(sr, bs, dev_name, "DAUx WASAPI Shared".into());
                stream
            }
            #[cfg(target_os = "windows")]
            BackendKind::WasapiExclusive => {
                let handle = wasapi_exclusive::open(
                    &daux_cfg,
                    Arc::clone(&self.shared),
                    initial_runtime,
                    Arc::clone(&self.glitch_counter),
                )?;
                let sr = handle.sample_rate;
                let bs = handle.buffer_size;
                let dev_name = handle.device_name.clone();
                let stream = ActiveStream::WasapiExclusive(handle);
                self.commit_stream_open(sr, bs, dev_name, "DAUx WASAPI Exclusive".into());
                stream
            }
            #[cfg(target_os = "windows")]
            BackendKind::WdmKs => {
                let handle = wdm_ks::open(
                    &daux_cfg,
                    Arc::clone(&self.shared),
                    initial_runtime,
                    Arc::clone(&self.glitch_counter),
                )?;
                let sr = handle.sample_rate;
                let bs = handle.buffer_size;
                let dev_name = handle.device_name.clone();
                let stream = ActiveStream::WdmKs(handle);
                self.commit_stream_open(sr, bs, dev_name, "DAUx WDM-KS".into());
                stream
            }
            #[cfg(all(target_os = "windows", feature = "asio"))]
            BackendKind::Asio => {
                let host = crate::backend::asio_host()?;
                let handle = crate::backend::asio_session::open_duplex(
                    host,
                    &daux_cfg,
                    Arc::clone(&self.shared),
                    initial_runtime,
                    Arc::clone(&self.glitch_counter),
                )?;
                let sr = handle.sample_rate;
                let bs = handle.buffer_size;
                let dev_name = handle.device_name.clone();
                deferred_open_warning = handle.input_warning.clone();
                *self.asio_caps.lock() = Some(handle.caps.clone());
                self.publish_asio_latencies(&handle.caps);
                let stream = ActiveStream::AsioDuplex(Box::new(handle));
                self.commit_stream_open(sr, bs, dev_name, "DAUx ASIO".into());
                stream
            }
            #[cfg(not(all(target_os = "windows", feature = "asio")))]
            BackendKind::Asio => {
                return Err(SphereAudioError::BackendUnavailable(
                    "this build does not include the DAUx ASIO backend".into(),
                ));
            }
            #[cfg(not(target_os = "windows"))]
            BackendKind::WdmKs => {
                return Err(SphereAudioError::BackendUnavailable(
                    "DAUx WDM-KS is only available on Windows".into(),
                ));
            }
            BackendKind::DauxDirect => {
                return Err(SphereAudioError::BackendUnavailable(
                    "DAUx Direct is no longer supported".into(),
                ));
            }
            _ => {
                let handle = cpal_backend::open(
                    &daux_cfg,
                    Arc::clone(&self.shared),
                    initial_runtime,
                    Arc::clone(&self.glitch_counter),
                )?;
                let sr = handle.sample_rate;
                let bs = handle.buffer_size;
                let dev_name = handle.device_name.clone();
                let backend_name = handle.backend_name.clone();
                let stream = ActiveStream::Cpal(handle);
                self.commit_stream_open(sr, bs, dev_name, backend_name);
                stream
            }
        };

        // Clear any previous error / device-lost flag on success.
        self.status.lock().last_daux_error = deferred_open_warning;
        self.shared.device_lost.store(false, Ordering::Relaxed);

        // Surface any divergence between the requested rate and the rate the
        // device actually opened at. Shared/Auto: allowed (logged + shown in
        // Settings). Exclusive: an implicit fallback happened — make it a
        // visible warning so the user isn't silently off-pitch.
        let active_sr = self.shared.sample_rate.load(Ordering::Relaxed);
        let requested_sr = daux_cfg.sample_rate.unwrap_or(0);
        if let Some(exclusive_warning) =
            log_sample_rate_decision(requested_sr, active_sr, &daux_cfg.backend)
        {
            self.status.lock().last_daux_error = Some(exclusive_warning);
        }

        *self.active_stream.lock() = Some(stream);

        // The new callback's metronome/tempo/time-signature state starts at its
        // defaults; hand it what the control thread is actually holding.
        self.reseed_stream_transport_state();

        // Re-derive input routing after every backend/device reopen. ASIO
        // restores atomic routing on its persistent callback; other backends
        // recreate the standalone capture stream when an armed/monitored track
        // needs one.
        if let Some(snapshot) = self.project.lock().clone() {
            if let Err(error) = self.sync_live_input_stream(&snapshot) {
                eprintln!("[DAUx] input routing restore failed: {error}");
            }
        }

        Ok(())
    }

    /// Re-open the audio device after a device-loss event using the last-known
    /// good DAUx config. Returns `Ok(true)` if a recovery was performed,
    /// `Ok(false)` if no recovery was needed (device not lost). On failure the
    /// `device_lost` flag stays set so the UI keeps showing DeviceLost.
    pub fn recover_daux(&self) -> Result<bool, SphereAudioError> {
        if !self.shared.device_lost.load(Ordering::Relaxed) {
            return Ok(false);
        }
        let cfg = {
            let prev = self.daux_config.lock().clone();
            JsDauxConfig {
                backend_id: prev.backend.id().to_string(),
                output_device_id: prev.output_device_id.clone(),
                sample_rate: prev.sample_rate,
                buffer_size: prev.buffer_size,
                mmcss_priority: prev.mmcss_priority,
                safe_mode: prev.safe_mode,
            }
        };
        // `open_daux` clears `device_lost` on success.
        self.open_daux(cfg)?;
        Ok(true)
    }

    /// Whether applying `new` would require a controlled device restart versus
    /// the currently-open config. In this engine every device-shaping change
    /// (backend, device, sample rate, buffer size, MMCSS, safe mode) reopens the
    /// stream, so the UI should mark those settings "restart required".
    pub fn daux_requires_restart(&self, new: &JsDauxConfig) -> bool {
        let cur = self.daux_config.lock();
        let norm = |s: &Option<String>| s.clone().filter(|v| !v.is_empty());
        let norm_u32 = |v: Option<u32>| v.filter(|&n| n > 0);
        new.backend_id != cur.backend.id()
            || norm(&new.output_device_id) != cur.output_device_id
            || norm_u32(new.sample_rate) != cur.sample_rate
            || norm_u32(new.buffer_size) != cur.buffer_size
            || new.mmcss_priority != cur.mmcss_priority
            || new.safe_mode != cur.safe_mode
    }

    /// Safe variant: tries `new_config`, and on failure restores the previous
    /// working config.  Returns `Ok(())` if the new config succeeded, or
    /// `Err(message)` describing why exclusive failed (after restoring the
    /// previous backend).  The engine always ends up with an open stream.
    pub fn open_daux_safe(&self, new_config: JsDauxConfig) -> Result<(), SphereAudioError> {
        let attempted_backend_name = BackendKind::from_id(&new_config.backend_id).display_name();
        // Save the previous working config before closing.
        let previous_config = {
            let prev = self.daux_config.lock().clone();
            JsDauxConfig {
                backend_id: prev.backend.id().to_string(),
                output_device_id: prev.output_device_id.clone(),
                sample_rate: prev.sample_rate,
                buffer_size: prev.buffer_size,
                mmcss_priority: prev.mmcss_priority,
                safe_mode: prev.safe_mode,
            }
        };
        let had_previous_stream = self.active_stream.lock().is_some();

        // Stop transport so playback doesn't resume mid-switch.
        self.shared.playing.store(false, Ordering::Relaxed);

        match self.open_daux(new_config) {
            Ok(()) => Ok(()),
            Err(open_err) => {
                let err_msg = open_err.to_string();
                eprintln!("[DAUx] open_daux_safe: failed ({err_msg}), attempting fallback");

                // Store the error before trying to restore.
                self.status.lock().last_daux_error = Some(err_msg.clone());

                // Attempt to restore the previous working config.
                if had_previous_stream {
                    match self.open_daux(previous_config) {
                        Ok(()) => {
                            eprintln!("[DAUx] open_daux_safe: previous backend restored");
                            let restore_msg = format!(
                                "{attempted_backend_name} backend switch failed: {err_msg}. Reverted to previous backend."
                            );
                            self.status.lock().last_daux_error = Some(restore_msg.clone());
                            Err(SphereAudioError::StreamOpenFailed(restore_msg))
                        }
                        Err(restore_err) => {
                            eprintln!("[DAUx] open_daux_safe: fallback also failed: {restore_err}");
                            let combined = format!(
                                "{attempted_backend_name} backend switch failed: {err_msg}. Fallback also failed: {restore_err}"
                            );
                            self.status.lock().last_daux_error = Some(combined.clone());
                            Err(SphereAudioError::StreamOpenFailed(combined))
                        }
                    }
                } else {
                    Err(open_err)
                }
            }
        }
    }

    #[cfg(all(target_os = "windows", feature = "asio"))]
    pub(super) fn validate_asio_input_source(
        &self,
        track_id: &str,
        input_source: &EngineTrackInputSourceSnapshot,
        caps: &crate::backend::AsioSessionCaps,
    ) -> Result<(), SphereAudioError> {
        if input_source.channels.is_empty() {
            return Ok(());
        }
        let failure = if caps.input_channels == 0 {
            Some(format!(
                "ASIO driver '{}' has no usable input channels",
                caps.driver
            ))
        } else if let Some(device_id) = input_source
            .device_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .filter(|id| *id != caps.driver)
        {
            Some(format!(
                "track '{track_id}' input device '{device_id}' is not the active ASIO driver '{}'",
                caps.driver
            ))
        } else {
            input_source
                .channels
                .iter()
                .copied()
                .find(|channel| *channel >= caps.input_channels)
                .map(|channel| {
                    format!(
                        "track '{track_id}' input channel {} is unavailable on ASIO driver '{}' ({} input channel(s))",
                        channel.saturating_add(1),
                        caps.driver,
                        caps.input_channels
                    )
                })
        };
        if let Some(message) = failure {
            eprintln!("[DAUx ASIO] {message}");
            self.status.lock().last_error = Some(message.clone());
            return Err(SphereAudioError::InvalidConfig(message));
        }
        Ok(())
    }

    /// Apply track input routing to the persistent ASIO session.
    ///
    /// Everything here is an atomic store into `SharedState` — the session's
    /// input callback re-reads the monitor channel indices every block and the
    /// ring's active flag gates the output-side monitor mix. No streams are
    /// created, no buffers touched, no project reload issued.
    #[cfg(all(target_os = "windows", feature = "asio"))]
    pub(super) fn sync_asio_input_routing(
        &self,
        snapshot: &EngineProjectSnapshot,
        caps: &crate::backend::AsioSessionCaps,
    ) -> Result<(), SphereAudioError> {
        let active_tracks = || {
            snapshot.tracks.iter().filter(|track| {
                track.track_type == "audio"
                    && (track.armed || track.input_monitor)
                    && !track.input_source.channels.is_empty()
                    && !track.input_source.is_internal()
            })
        };
        for track in active_tracks() {
            if let Err(error) =
                self.validate_asio_input_source(&track.id, &track.input_source, caps)
            {
                self.stop_live_input_stream();
                return Err(error);
            }
        }
        // The shared monitor ring currently carries one pair. A monitored route
        // must win over an armed-only route so record arm never changes what the
        // user hears while per-track monitor buses are prepared separately.
        let desired_track = active_tracks()
            .find(|track| track.input_monitor)
            .or_else(|| active_tracks().next());
        let monitor_any = snapshot.tracks.iter().any(|track| {
            track.track_type == "audio"
                && track.input_monitor
                && !track.input_source.channels.is_empty()
                && !track.input_source.is_internal()
        });
        self.shared
            .monitor_enabled_any
            .store(monitor_any, Ordering::Relaxed);

        let Some(track) = desired_track else {
            // Nothing armed/monitored: deactivate the ring (input keeps
            // running for meters) and clear the monitor meter state.
            self.stop_live_input_stream();
            return Ok(());
        };

        let (left, right) = monitor_channel_pair(&track.input_source.channels).unwrap_or((0, 0));

        self.shared.set_monitor_source_pair(left, right);
        self.shared
            .monitor_enabled_any
            .store(monitor_any, Ordering::Relaxed);
        self.shared
            .input_ring
            .set_active(true, caps.input_channels, caps.sample_rate);
        self.shared
            .monitor_shared_clock
            .store(true, Ordering::Relaxed);
        self.shared.live_input_active.store(true, Ordering::Relaxed);
        self.warm_output_for_monitoring();
        Ok(())
    }

    /// Resume the (already-open) output stream so `fill_output_f32` runs while
    /// monitoring/armed, without advancing the transport. No-op when no stream
    /// is open yet — the next explicit `start()` will pick it up.
    pub(super) fn warm_output_for_monitoring(&self) {
        let already_running = self.status.lock().running;
        if already_running {
            return;
        }
        match self.start() {
            Ok(()) => eprintln!("[AudioDevice] output stream warmed for monitoring"),
            Err(SphereAudioError::EngineNotOpen) => {}
            Err(e) => eprintln!("[AudioDevice] could not warm output for monitoring: {e}"),
        }
    }

    /// Start an input-level test on `device_id` (or the default input). Opens a
    /// capture stream whose callback tracks the running peak; poll it with
    /// [`Self::get_input_test_level`] and stop with [`Self::stop_input_test`].
    /// Independent of recording and of the output device.
    pub fn start_input_test(&self, device_id: Option<String>) -> Result<(), SphereAudioError> {
        // Replace any existing test stream.
        *self.input_test.lock() = None;
        self.input_test_uses_session
            .store(false, std::sync::atomic::Ordering::Relaxed);

        // ASIO: the session input is already running — the "test" is just a
        // read of the session-wide peak. Never open a second stream.
        if self.daux_config.lock().backend == BackendKind::Asio {
            let caps = self.asio_caps.lock().clone();
            return match caps {
                Some(caps) if caps.input_channels > 0 => {
                    self.shared
                        .session_input_peak
                        .store(f32_store(0.0), Ordering::Relaxed);
                    self.input_test_uses_session
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                    Ok(())
                }
                Some(caps) => Err(SphereAudioError::NativeError(format!(
                    "ASIO driver '{}' has no usable input channels",
                    caps.driver
                ))),
                None => Err(SphereAudioError::NativeError(
                    "open the ASIO stream before testing its input".into(),
                )),
            };
        }

        let device = self.find_input_device_for_active_backend(device_id.as_deref())?;
        let default_cfg = device.default_input_config().map_err(|e| {
            SphereAudioError::NativeError(format!("Input device config error: {e}"))
        })?;
        let preferred_period = {
            let st = self.status.lock();
            if st.buffer_size > 0 {
                Some(st.buffer_size)
            } else {
                self.daux_config.lock().buffer_size.filter(|&p| p > 0)
            }
        };
        let preferred_sample_rate = {
            let out_sr = self.shared.sample_rate.load(Ordering::Relaxed);
            (out_sr > 0).then_some(out_sr)
        };
        let candidates = crate::device::input_stream_config_candidates(
            &default_cfg,
            preferred_period,
            preferred_sample_rate,
        );

        let peak = Arc::new(AtomicU32::new(0));
        let mut last_error = None;
        let mut opened = None;
        for (label, stream_config) in candidates {
            let cb_peak = Arc::clone(&peak);
            #[cfg(target_os = "linux")]
            let rt_announcer = crate::backend::cpal_backend::spawn_capture_rt_promoter();
            #[cfg(target_os = "linux")]
            let mut rt_announced = false;
            match device.build_input_stream::<f32, _, _>(
                &stream_config,
                move |data: &[f32], _info| {
                    #[cfg(target_os = "linux")]
                    if !rt_announced {
                        rt_announcer.announce_current_thread();
                        rt_announced = true;
                    }
                    let mut block_peak = 0.0f32;
                    for &s in data {
                        let a = s.abs();
                        if a > block_peak {
                            block_peak = a;
                        }
                    }
                    // Keep the running max until the UI polls (swap-resets it).
                    let mut cur = cb_peak.load(Ordering::Relaxed);
                    loop {
                        if block_peak <= f32::from_bits(cur) {
                            break;
                        }
                        match cb_peak.compare_exchange_weak(
                            cur,
                            block_peak.to_bits(),
                            Ordering::Relaxed,
                            Ordering::Relaxed,
                        ) {
                            Ok(_) => break,
                            Err(c) => cur = c,
                        }
                    }
                },
                |err| eprintln!("[SphereAudio] Input test stream error: {err}"),
                None,
            ) {
                Ok(stream) => {
                    if let Err(e) = stream.play() {
                        last_error = Some(format!("{label} play failed: {e}"));
                        continue;
                    }
                    eprintln!(
                        "[SphereAudio] input test open via '{label}' buffer={:?}",
                        stream_config.buffer_size
                    );
                    opened = Some(stream);
                    break;
                }
                Err(e) => last_error = Some(format!("{label}: {e}")),
            }
        }
        let stream = opened.ok_or_else(|| {
            SphereAudioError::NativeError(format!(
                "Cannot open input test stream: {}",
                last_error.unwrap_or_else(|| "no candidates".into())
            ))
        })?;

        *self.input_test.lock() = Some(InputTestHandle {
            _stream: stream,
            peak,
        });
        Ok(())
    }

    /// Read (and reset) the peak input level since the last poll, `0.0..=1.0`.
    /// Returns `0.0` when no input test is active.
    pub fn get_input_test_level(&self) -> f32 {
        if self
            .input_test_uses_session
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return f32::from_bits(self.shared.session_input_peak.swap(0, Ordering::Relaxed));
        }
        self.input_test
            .lock()
            .as_ref()
            .map(|h| f32::from_bits(h.peak.swap(0, Ordering::Relaxed)))
            .unwrap_or(0.0)
    }

    /// Stop and release the input-level test stream.
    pub fn stop_input_test(&self) {
        *self.input_test.lock() = None;
        self.input_test_uses_session
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }

    /// Aggregate latency report (Phase V — reporting only).
    ///
    /// Sums each track's enabled native-plugin insert latencies (queried from
    /// the live VST3 processors on the control-side runtime copy) plus the
    /// device buffer latency. Full plug-in delay compensation is Phase W; this
    /// is the data the UI displays and the basis (`max_track_samples`) PDC will
    /// later use.
    pub fn get_latency_info(&self) -> crate::types::JsLatencyInfo {
        use crate::latency_graph::strip_plugin_latency_samples;
        use crate::types::{JsLatencyInfo, JsTrackLatency};
        let runtime = self.runtime.lock();
        let (sample_rate, buffer_frames) = {
            let st = self.status.lock();
            (st.sample_rate.max(1), st.buffer_size)
        };
        let to_ms = |samples: u32| samples as f64 / sample_rate as f64 * 1000.0;
        let pdc_enabled = self.pdc_enabled();
        // The period delay compensation charges each bridged insert, so the
        // report and the plan agree (`bridged_insert_latency_samples`).
        let bridge_period_frames = self.shared.bridge_period_frames.load(Ordering::Relaxed);

        let max_path_samples = runtime.latency_graph.max_path_latency_samples;
        let mut tracks = Vec::new();
        let mut master_samples = 0u32;
        let mut max_track_plugin_samples = 0u32;
        for (idx, track) in runtime.tracks.iter().enumerate() {
            let mut samples: i64 = 0;
            for insert in &track.inserts {
                if !insert.enabled {
                    continue;
                }
                if let Some(vst3) = insert.vst3.as_ref() {
                    if vst3.is_ready() {
                        samples += vst3.get_latency_samples().max(0) as i64;
                    }
                } else if insert.kind.eq_ignore_ascii_case("external-bridge-plugin") {
                    // Bridged plugin: the host-reported plugin latency plus the
                    // one-block bridge handshake at the configured period, the
                    // same figure delay compensation plans with.
                    if let Some(sink) = runtime.plugin_bridge_sinks.get(&insert.id) {
                        samples += crate::plugin_bridge::bridged_insert_latency_samples(
                            sink.reported_latency_samples(),
                            bridge_period_frames,
                        ) as i64;
                    }
                }
            }
            let plugin_samples = if samples > 0 {
                samples as u32
            } else {
                strip_plugin_latency_samples(track)
            };
            let path_samples = max_path_samples.saturating_sub(
                runtime
                    .latency_graph
                    .track_pdc_delay
                    .get(idx)
                    .copied()
                    .unwrap_or(0),
            );
            let pdc_delay_samples = runtime
                .latency_graph
                .track_pdc_delay
                .get(idx)
                .copied()
                .unwrap_or(0);
            if track.track_type == "master" {
                master_samples = plugin_samples;
            } else {
                max_track_plugin_samples = max_track_plugin_samples.max(plugin_samples);
                tracks.push(JsTrackLatency {
                    track_id: track.id.clone(),
                    plugin_samples,
                    plugin_ms: to_ms(plugin_samples),
                    path_samples,
                    path_ms: to_ms(path_samples),
                    pdc_delay_samples,
                    pdc_delay_ms: to_ms(pdc_delay_samples),
                });
            }
        }

        JsLatencyInfo {
            sample_rate,
            buffer_frames,
            buffer_ms: to_ms(buffer_frames),
            tracks,
            master_samples,
            master_ms: to_ms(master_samples),
            max_path_samples,
            max_path_ms: to_ms(max_path_samples),
            pdc_enabled,
            max_track_samples: max_track_plugin_samples,
        }
    }

    /// Publish an ASIO session's driver-reported latencies as the engine's
    /// input/output latency. `ASIOGetLatencies` covers the whole path —
    /// buffers plus the converters' own delay — which is what monitoring
    /// displays and take compensation need; the cpal block timestamp does not.
    #[cfg(all(target_os = "windows", feature = "asio"))]
    pub(super) fn publish_asio_latencies(&self, caps: &crate::backend::AsioSessionCaps) {
        let sample_rate = caps.sample_rate.max(1) as f32;
        self.shared.record_input_latency_secs.store(
            f32_store(caps.input_latency_samples as f32 / sample_rate),
            Ordering::Relaxed,
        );
        self.shared.output_latency_secs.store(
            f32_store(caps.output_latency_samples as f32 / sample_rate),
            Ordering::Relaxed,
        );
        self.shared
            .latency_from_driver
            .store(true, Ordering::Relaxed);
        eprintln!(
            "[DAUx ASIO] latency in={} out={} samples @ {} Hz ({:.2} ms round trip)",
            caps.input_latency_samples,
            caps.output_latency_samples,
            caps.sample_rate,
            (caps.input_latency_samples + caps.output_latency_samples) as f64 / sample_rate as f64
                * 1000.0
        );
    }

    /// Service the ASIO driver's asynchronous notifications. Control thread
    /// only; called from the status poll so nothing here touches a callback.
    ///
    /// * `latenciesChanged` — re-query `ASIOGetLatencies` and republish, so the
    ///   figures (and the next take's compensation) follow a driver whose
    ///   internal buffering moved without a buffer-size change.
    /// * `resetRequest` — the driver wants its buffers disposed and recreated
    ///   (typically after the user changed the buffer size in its control
    ///   panel). Ignoring it kept streaming through buffers the driver had
    ///   already resized on its side: wrong block size, stale latencies, and
    ///   input that lagged the output by whatever the panel had just added.
    ///   The session is reopened with the same config, which re-snaps the
    ///   buffer size to the driver's new constraints.
    #[cfg(all(target_os = "windows", feature = "asio"))]
    pub(super) fn service_asio_driver_events(&self) {
        let (latencies_changed, reset_requested, refreshed) = {
            let stream_guard = self.active_stream.lock();
            let Some(handle) = stream_guard.as_ref().and_then(|s| s.as_asio_duplex()) else {
                return;
            };
            // A resync is the driver saying it lost data: the buffer switch
            // returned too late for the hardware. Counted as a dropout like
            // any other, with its own reason, so a crackle the driver heard
            // does not show as zero dropouts.
            let resyncs = handle.take_resyncs();
            if resyncs > 0 {
                self.shared
                    .dropout_count
                    .fetch_add(resyncs, Ordering::Relaxed);
                self.shared
                    .device_xruns
                    .fetch_add(resyncs, Ordering::Relaxed);
                self.shared.dropout_last_reason.store(
                    crate::engine::DropoutReason::DriverResync as u8,
                    Ordering::Relaxed,
                );
            }
            let reset_requested = handle.take_reset_request();
            let latencies_changed = handle.take_latencies_changed();
            let refreshed = if latencies_changed && !reset_requested {
                handle.refresh_latencies()
            } else {
                None
            };
            (latencies_changed, reset_requested, refreshed)
        };

        if let Some((input, output)) = refreshed {
            let mut caps_guard = self.asio_caps.lock();
            if let Some(caps) = caps_guard.as_mut() {
                caps.input_latency_samples = input;
                caps.output_latency_samples = output;
                let caps = caps.clone();
                drop(caps_guard);
                self.publish_asio_latencies(&caps);
            }
        } else if latencies_changed && !reset_requested {
            eprintln!("[DAUx ASIO] latenciesChanged received but ASIOGetLatencies failed");
        }

        if reset_requested {
            eprintln!("[DAUx ASIO] driver requested a reset; reopening the session");
            // Reuse the device-loss recovery: it reopens with the last-known
            // config and clears the flag on success. On failure the flag stays
            // set and the UI shows DeviceLost, which is the honest state of a
            // driver that has withdrawn its buffers.
            self.shared.device_lost.store(true, Ordering::Relaxed);
            match self.recover_daux() {
                Ok(_) => eprintln!("[DAUx ASIO] session reopened after driver reset"),
                Err(error) => eprintln!("[DAUx ASIO] reopen after driver reset failed: {error}"),
            }
        }
    }

    #[cfg(not(all(target_os = "windows", feature = "asio")))]
    pub(super) fn service_asio_driver_events(&self) {}

    /// Return the current DAUx status (backend, device, latency, glitches).
    pub fn get_daux_status(&self) -> JsDauxStatus {
        // The status poll is the control thread's regular visit, so it is where
        // the driver's pending notifications get acted on.
        self.service_asio_driver_events();
        let st = self.status.lock().clone();
        let daux_cfg = self.daux_config.lock().clone();
        let glitch_count = self.glitch_counter.load(Ordering::Relaxed) as f64;
        let device_xruns = self.shared.device_xruns.load(Ordering::Relaxed) as f64;
        let mmcss_active = self.shared.mmcss_active.load(Ordering::Relaxed);
        let device_lost = self.shared.device_lost.load(Ordering::Relaxed);
        let running = self.shared.playing.load(Ordering::Relaxed);
        let device_state = if device_lost {
            "DeviceLost"
        } else if !st.stream_open {
            "Closed"
        } else if running {
            "Running"
        } else {
            "Ready"
        }
        .to_string();

        let backend_id = daux_cfg.backend.id().to_string();
        let backend_name = if let Some(stream) = self.active_stream.lock().as_ref() {
            stream.backend_name().to_string()
        } else {
            daux_cfg.backend.display_name().to_string()
        };

        // The device's measured callback→playout delay is the real figure and
        // covers the whole ring; the block-size estimate only ever described
        // one period, which under-reports wherever the ring holds several
        // (ALSA). Fall back to the estimate until the first callback lands.
        let measured_latency_secs =
            f32_load(self.shared.output_latency_secs.load(Ordering::Relaxed));
        let estimated_latency_ms = if measured_latency_secs > 0.0 {
            measured_latency_secs as f64 * 1000.0
        } else if st.sample_rate > 0 {
            st.buffer_size as f64 / st.sample_rate as f64 * 1000.0
        } else {
            0.0
        };
        // Input latency is known only where the backend reports it (ASIO) or a
        // recording stream has measured it; `0` means "not measured", never
        // "zero delay", so a round trip is only stated when both halves are.
        let input_latency_ms = f32_load(
            self.shared
                .record_input_latency_secs
                .load(Ordering::Relaxed),
        ) as f64
            * 1000.0;
        let round_trip_latency_ms = if input_latency_ms > 0.0 {
            input_latency_ms + estimated_latency_ms
        } else {
            0.0
        };

        JsDauxStatus {
            backend_id,
            backend_name,
            output_device: st.output_device,
            sample_rate: st.sample_rate,
            requested_sample_rate: daux_cfg.sample_rate.unwrap_or(0),
            buffer_size: st.buffer_size,
            estimated_latency_ms,
            input_latency_ms,
            round_trip_latency_ms,
            glitch_count,
            device_xruns,
            mmcss_active,
            last_error: st.last_daux_error.clone(),
            device_lost,
            device_state,
        }
    }
}

//! The live input stream that feeds monitoring and recording.

use super::*;

impl EngineInner {
    /// Re-derive the standalone capture stream from `snapshot`.
    ///
    /// Takes the recording lock itself. A caller that already holds it must go
    /// through [`Self::sync_live_input_stream_locked`] instead — `recording` is
    /// a `parking_lot::Mutex`, which is not reentrant, so locking it a second
    /// time on the same thread is a deadlock rather than a nested read. That is
    /// what hung the app on pressing R: `update_track_input_state_inner` holds
    /// the lock for the whole route change, and the take-owns-the-device guard
    /// added inside this function locked it again. The UI thread parked on its
    /// own guard and never came back, so the window stopped drawing before a
    /// single line of this function's tracing could print.
    pub(super) fn sync_live_input_stream(
        &self,
        snapshot: &EngineProjectSnapshot,
    ) -> Result<(), SphereAudioError> {
        let recording = self.recording.lock();
        self.sync_live_input_stream_locked(snapshot, &recording)
    }

    /// The body of [`Self::sync_live_input_stream`], for callers that already
    /// hold the recording lock.
    ///
    /// `recording` is taken by reference rather than as a `bool` on purpose:
    /// the only way to produce one is to be holding the guard, so the call
    /// cannot be made from a context that would deadlock on it. The invariant
    /// is in the signature instead of in a comment somebody has to remember.
    pub(super) fn sync_live_input_stream_locked(
        &self,
        snapshot: &EngineProjectSnapshot,
        recording: &Option<RecordingSession>,
    ) -> Result<(), SphereAudioError> {
        // Single-capture-stream invariant. While a take is running, the
        // recording stream *is* the live input: it owns the endpoint, and it is
        // what sets `live_input_active` and fills `input_ring` — see
        // `recording::start_recording_with_config`.
        //
        // Re-opening a standalone stream here would put a second client on the
        // same device, and `stop_live_input_stream` on the way would reset the
        // very ring the recorder is filling and clear `monitor_enabled_any`
        // under it. That is why a take could record nothing: recording needs an
        // armed track, an armed track makes `wants_live_input` true, and any
        // project sync during the take — the auto-save before Record is enough
        // to cause one — walked straight into this.
        //
        // Declining costs nothing: `stop_recording` restores this stream from
        // the last snapshot as soon as the take is finalized.
        if !live_input_sync_may_touch_stream(recording.is_some()) {
            if recording_debug_enabled() {
                eprintln!("[recording] live-input sync declined: the take owns the capture device");
            }
            return Ok(());
        }
        // Jam- and loopback-routed tracks are excluded throughout: they take no
        // hardware input, so they must not open a capture stream, must not pin
        // a device, and must not count as a conflicting route against a track
        // that does. A loopback route used to be let through here, so arming
        // or monitoring a track fed by another track tried to open an input
        // device called `trk:<id>` — which fails, and the failure rolled the
        // Input choice back (#394).
        let mut monitored_route: Option<(&str, &EngineTrackInputSourceSnapshot)> = None;
        for track in snapshot.tracks.iter().filter(|track| {
            track.track_type == "audio"
                && track.input_monitor
                && !track.input_source.channels.is_empty()
                && !track.input_source.is_internal()
        }) {
            if let Some((other_track_id, other_source)) = monitored_route {
                if other_source.device_id != track.input_source.device_id
                    || other_source.channels != track.input_source.channels
                {
                    return Err(SphereAudioError::InvalidConfig(format!(
                        "software monitoring routes conflict: track '{}' and track '{}' use different hardware inputs",
                        other_track_id, track.id
                    )));
                }
            } else {
                monitored_route = Some((track.id.as_str(), &track.input_source));
            }
        }

        // ASIO: the duplex session's persistent input already feeds the ring.
        // Route changes are atomic stores — never a stream open/close, which
        // would dispose the shared buffer set and kill playback. During stream
        // startup/restart the backend is selected before session capabilities
        // are published; defer routing in that window. `open_daux` restores it
        // from the project as soon as the duplex session is installed.
        if self.daux_config.lock().backend == BackendKind::Asio {
            #[cfg(all(target_os = "windows", feature = "asio"))]
            if let Some(caps) = self.asio_caps.lock().clone() {
                return self.sync_asio_input_routing(snapshot, &caps);
            }

            self.stop_live_input_stream();
            return Ok(());
        }

        self.shared
            .monitor_shared_clock
            .store(false, Ordering::Relaxed);

        // The first armed/monitored audio track with a routable input source
        // drives which device + channels the shared capture stream uses.
        let desired_track = snapshot.tracks.iter().find(|track| {
            track.track_type == "audio"
                && (track.armed || track.input_monitor)
                && !track.input_source.channels.is_empty()
                && !track.input_source.is_internal()
        });

        // A jam send bound to the Live Input pair is a request for the interface,
        // exactly as an armed track is. Without this the capture stream is never
        // opened, the input ring never goes active, and the tap in the render
        // callback has nothing to publish — the room hears silence and nothing
        // on screen says why. It is why "send my mic" only ever worked when a
        // track happened to be armed for something else.
        let jam_wants_live_input = self.shared.jam_bus.live_input_publish().is_some();
        let wants_live_input = desired_track.is_some() || jam_wants_live_input;

        // Any track that explicitly wants monitoring (not just record-arm) —
        // gates whether the render callback mixes the input bus to the output.
        let monitor_any = snapshot.tracks.iter().any(|track| {
            track.track_type == "audio"
                && track.input_monitor
                && !track.input_source.channels.is_empty()
                && !track.input_source.is_internal()
        });
        self.shared
            .monitor_enabled_any
            .store(monitor_any, Ordering::Relaxed);

        if !wants_live_input {
            self.stop_live_input_stream();
            return Ok(());
        }

        // Device resolution order: the track's own pinned device, then the
        // global Preferences input device, then the system default.
        let desired_device = desired_track
            .and_then(|track| track.input_source.device_id.clone())
            .filter(|device_id| !device_id.trim().is_empty())
            .or_else(|| {
                snapshot
                    .preferred_input_device
                    .clone()
                    .filter(|d| !d.trim().is_empty())
            });

        // Which device channels feed the L/R input bus.
        let (mon_l_ch, mon_r_ch) = desired_track
            .and_then(|track| monitor_channel_pair(&track.input_source.channels))
            .unwrap_or((0, 1));
        self.shared.set_monitor_source_pair(mon_l_ch, mon_r_ch);

        eprintln!(
            "[Engine] applied input device = {:?} (preferred={:?}) monitor_any={}",
            desired_device, snapshot.preferred_input_device, monitor_any
        );

        // Reuse the existing stream when the device is unchanged — only the
        // monitor flag/channels may have moved (already stored above).
        if self
            .live_input
            .lock()
            .as_ref()
            .is_some_and(|handle| handle.device_id == desired_device)
        {
            self.shared.live_input_active.store(true, Ordering::Relaxed);
            self.warm_output_for_monitoring();
            return Ok(());
        }

        self.stop_live_input_stream();
        let device = self.find_input_device_for_active_backend(desired_device.as_deref())?;
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
        let device_label = device
            .name()
            .unwrap_or_else(|_| desired_device.clone().unwrap_or_else(|| "default".into()));

        // Try ALSA multi-period / matched-rate candidates first so live monitor
        // shares geometry with the open output stream.
        let mut last_error = None;
        let mut opened: Option<(
            cpal::StreamConfig,
            cpal::Stream,
            crate::capture_tap::CaptureTapControl,
        )> = None;
        for (label, stream_config) in candidates {
            let channel_count = stream_config.channels.max(1) as usize;
            let input_sr = stream_config.sample_rate.0;
            self.shared
                .input_ring
                .set_active(true, channel_count as u32, input_sr);

            let shared = Arc::clone(&self.shared);
            #[cfg(target_os = "linux")]
            let rt_announcer = crate::backend::cpal_backend::spawn_capture_rt_promoter();
            #[cfg(target_os = "linux")]
            let mut rt_announced = false;
            // The record fan-out for whatever take is attached, if any. Built
            // per candidate because only the stream that actually opens keeps
            // its half.
            let (tap_control, mut tap) = crate::capture_tap::capture_tap();

            match device.build_input_stream::<f32, _, _>(
                &stream_config,
                move |data: &[f32], info| {
                    #[cfg(target_os = "linux")]
                    if !rt_announced {
                        rt_announcer.announce_current_thread();
                        rt_announced = true;
                    }
                    tap.drain_commands();
                    let frames = data.len() / channel_count.max(1);
                    shared.input_cb_count.fetch_add(1, Ordering::Relaxed);
                    // Capture latency (ADC → callback) for the take's
                    // round-trip compensation. Published from this stream now
                    // that this stream is the one a take reads from.
                    if tap.has_sink() {
                        let stamps = info.timestamp();
                        if let Some(delay) = stamps.callback.duration_since(&stamps.capture) {
                            shared
                                .record_input_latency_secs
                                .store(f32_store(delay.as_secs_f32()), Ordering::Relaxed);
                        }
                    }
                    shared
                        .input_frames_received
                        .fetch_add(frames as u64, Ordering::Relaxed);
                    let (mon_l_ch, mon_r_ch) = shared.monitor_source_pair();
                    let mon_l_ch = mon_l_ch as usize;
                    let mon_r_ch = mon_r_ch as usize;
                    let mut peak_l = 0.0f32;
                    let mut peak_r = 0.0f32;
                    let mut last_l = 0.0f32;
                    let mut last_r = 0.0f32;
                    for frame in data.chunks(channel_count) {
                        let first = frame.first().copied().unwrap_or(0.0);
                        // Unclamped: monitoring hands on what the interface
                        // captured, so a hot input reads hot instead of
                        // arriving pre-clipped.
                        let l = frame.get(mon_l_ch).copied().unwrap_or(first);
                        let r = frame.get(mon_r_ch).copied().unwrap_or(l);
                        last_l = l;
                        last_r = r;
                        peak_l = peak_l.max(l.abs());
                        peak_r = peak_r.max(r.abs());
                        shared.input_ring.write_stereo(l, r);
                    }
                    shared
                        .live_input_l
                        .store(f32_store(last_l), Ordering::Relaxed);
                    shared
                        .live_input_r
                        .store(f32_store(last_r), Ordering::Relaxed);
                    atomic_max_f32_bits(&shared.live_input_peak_l, peak_l);
                    atomic_max_f32_bits(&shared.live_input_peak_r, peak_r);
                    shared.live_input_active.store(true, Ordering::Relaxed);
                    // Feed the attached take, if there is one. A no-op — one
                    // `Option` check — for the whole time there is not.
                    tap.capture_block(data, channel_count, &shared);
                },
                |err| eprintln!("[SphereAudio] Live input stream error: {err}"),
                None,
            ) {
                Ok(stream) => {
                    if let Err(e) = stream.play() {
                        last_error = Some(format!("{label} play failed: {e}"));
                        self.shared.input_ring.set_active(false, 0, 0);
                        continue;
                    }
                    eprintln!(
                        "[AudioDevice] opening input stream: device='{device_label}' \
                         candidate='{label}' sample_rate={input_sr} channels={channel_count} \
                         buffer={:?} monitor_channels=({mon_l_ch},{mon_r_ch})",
                        stream_config.buffer_size
                    );
                    let output_sr = self.shared.sample_rate.load(Ordering::Relaxed);
                    if output_sr != 0 && input_sr != 0 && output_sr != input_sr {
                        eprintln!(
                            "[AudioDevice] ⚠ input sample_rate ({input_sr}) != output sample_rate \
                             ({output_sr}); monitored audio will be pitch-shifted until resampling \
                             is added"
                        );
                    }
                    opened = Some((stream_config, stream, tap_control));
                    break;
                }
                Err(e) => {
                    last_error = Some(format!("{label}: {e}"));
                    self.shared.input_ring.set_active(false, 0, 0);
                }
            }
        }

        let (stream_config, stream, tap) = opened.ok_or_else(|| {
            self.shared.input_ring.set_active(false, 0, 0);
            SphereAudioError::NativeError(format!(
                "Cannot open live input stream: {}",
                last_error.unwrap_or_else(|| "no candidates".into())
            ))
        })?;
        eprintln!("[AudioDevice] input stream started: device='{device_label}'");

        *self.live_input.lock() = Some(LiveInputHandle {
            _stream: stream,
            device_id: desired_device,
            channels: stream_config.channels.max(1) as usize,
            sample_rate: stream_config.sample_rate.0,
            tap,
        });
        self.shared.live_input_active.store(true, Ordering::Relaxed);

        // Make sure the output render callback is actually running so per-track
        // input meters and the monitor mix update even before transport play.
        self.warm_output_for_monitoring();
        Ok(())
    }

    pub(super) fn stop_live_input_stream(&self) {
        *self.live_input.lock() = None;
        self.shared
            .live_input_active
            .store(false, Ordering::Relaxed);
        self.shared
            .monitor_shared_clock
            .store(false, Ordering::Relaxed);
        self.shared.input_ring.set_active(false, 0, 0);
        self.shared
            .monitor_enabled_any
            .store(false, Ordering::Relaxed);
        self.shared
            .live_input_l
            .store(f32_store(0.0), Ordering::Relaxed);
        self.shared
            .live_input_r
            .store(f32_store(0.0), Ordering::Relaxed);
        self.shared
            .live_input_peak_l
            .store(f32_store(0.0), Ordering::Relaxed);
        self.shared
            .live_input_peak_r
            .store(f32_store(0.0), Ordering::Relaxed);
        self.shared
            .input_bus_peak_l
            .store(f32_store(0.0), Ordering::Relaxed);
        self.shared
            .input_bus_peak_r
            .store(f32_store(0.0), Ordering::Relaxed);
    }
}

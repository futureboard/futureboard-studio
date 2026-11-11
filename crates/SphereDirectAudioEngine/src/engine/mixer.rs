//! Track and master parameters, the control room and listen bus, track inputs.

use super::*;

impl EngineInner {
    pub fn set_master_volume(&self, value: f32) -> Result<(), SphereAudioError> {
        self.shared
            .master_volume
            .store(f32_store(value.clamp(0.0, 2.0)), Ordering::Relaxed);
        Ok(())
    }

    /// Set the linear monitor gain applied to the input bus before it reaches
    /// master. A relaxed atomic store from the control thread; the realtime
    /// monitor path reads it once per block (see `backend::render`), so this is
    /// allocation- and lock-free on both sides.
    pub fn set_monitor_gain(&self, value: f32) -> Result<(), SphereAudioError> {
        self.shared
            .monitor_gain
            .store(f32_store(value.clamp(0.0, 2.0)), Ordering::Relaxed);
        Ok(())
    }

    //
    // Every setter here affects monitoring only. None of them touch the master
    // mix state, and none are reachable from the offline exporter, which
    // renders the graph directly instead of going through the device callback.

    /// Select what the Control Room monitors when no Listen is engaged.
    /// Defaults to [`crate::monitor::MonitorSource::MasterBus`]; a hardware
    /// input is only ever used when explicitly selected here.
    pub fn set_monitor_source(
        &self,
        source: crate::monitor::MonitorSource,
    ) -> Result<(), SphereAudioError> {
        self.monitor_mirror.lock().source = source.clone();
        self.send_command(EngineCommand::SetMonitorSource { source })
    }

    /// Set monitor gain, mute, dim, and mono in one atomic update so a mono
    /// toggle can never land a block apart from its level change.
    pub fn set_monitor_control(
        &self,
        control: crate::monitor::MonitorControl,
    ) -> Result<(), SphereAudioError> {
        self.monitor_mirror.lock().control = control;
        self.send_command(EngineCommand::SetMonitorControl { control })
    }

    /// Set the Control Room's listening simulation (Virtual Speaker).
    pub fn set_listening_simulation(
        &self,
        settings: solfege_spatialaudio::SimulationSettings,
    ) -> Result<(), SphereAudioError> {
        self.monitor_mirror.lock().simulation = settings;
        self.send_command(EngineCommand::SetListeningSimulation { settings })
    }

    /// Select the hardware output pair the Control Room feeds.
    pub fn set_monitor_output(
        &self,
        target: crate::monitor::MonitorOutputTarget,
    ) -> Result<(), SphereAudioError> {
        self.monitor_mirror.lock().output = target.clone();
        self.send_command(EngineCommand::SetMonitorOutput { target })
    }

    /// Publish which stage owns the physical output write, plus the resolved
    /// Master and effective monitoring channel pairs.
    ///
    /// Everything expensive — connection lookup, port resolution, and the
    /// ownership decision itself — has already happened on the control thread;
    /// this hands the callback plain integers.
    pub fn set_hardware_output_ownership(
        &self,
        owner: crate::monitor::HardwareOutputOwner,
        master: Option<(u16, u16)>,
        monitor: Option<(u16, u16)>,
    ) -> Result<(), SphereAudioError> {
        self.monitor_mirror
            .lock()
            .set_hardware_output_ownership(owner, master, monitor);
        self.send_command(EngineCommand::SetHardwareOutputOwnership {
            owner,
            master,
            monitor,
        })
    }

    /// Set one channel's Listen (PFL/AFL) state. The stable track id is
    /// resolved to an index here, off the audio thread.
    pub fn set_track_listen(
        &self,
        track_id: &str,
        listen: crate::monitor::ListenMode,
    ) -> Result<(), SphereAudioError> {
        let track_index = {
            let project = self.project.lock();
            project
                .as_ref()
                .and_then(|p| p.tracks.iter().position(|t| t.id == track_id))
        };
        {
            let mut mirror = self.monitor_mirror.lock();
            if listen == crate::monitor::ListenMode::Off {
                mirror.listen.remove(track_id);
            } else {
                mirror.listen.insert(track_id.to_string(), listen);
            }
        }
        let Some(track_index) = track_index else {
            return Ok(());
        };
        self.send_command(EngineCommand::SetTrackListen {
            track_index,
            listen,
        })
    }

    /// Move one channel in the spatial mix's room, live. The control thread's
    /// copies (the stored snapshot, the graph mirror) are updated too, so a
    /// later rebuild or device reopen keeps the position.
    pub fn set_track_spatial(
        &self,
        track_id: &str,
        params: solfege_spatialaudio::SourceParams,
    ) -> Result<(), SphereAudioError> {
        let params = params.sanitized();
        let track_index = {
            let mut project = self.project.lock();
            let Some(project) = project.as_mut() else {
                return Ok(());
            };
            match project
                .spatial
                .sources
                .iter_mut()
                .find(|source| source.track_id == track_id)
            {
                Some(source) => source.params = params,
                None => project
                    .spatial
                    .sources
                    .push(crate::types::EngineSpatialSource {
                        track_id: track_id.to_string(),
                        params,
                    }),
            }
            project.tracks.iter().position(|t| t.id == track_id)
        };
        let Some(track_index) = track_index else {
            return Ok(());
        };
        self.runtime
            .lock()
            .update_track_spatial(track_index, params);
        self.send_command(EngineCommand::SetTrackSpatial {
            track_index,
            params,
        })
    }

    /// Set one clip's gain (linear, 0..4), live: the running graph ramps to
    /// it over a few milliseconds, so it can follow a drag. The stored
    /// snapshot and the graph mirror take it too, so the next sync matches
    /// and a later rebuild keeps it. A clip not in the graph (muted, or not
    /// built yet) is left to the next sync.
    pub fn set_clip_gain(&self, clip_id: &str, gain: f32) -> Result<(), SphereAudioError> {
        let gain = if gain.is_finite() {
            gain.clamp(0.0, 4.0)
        } else {
            return Ok(());
        };
        if let Some(project) = self.project.lock().as_mut() {
            if let Some(clip) = project.clips.iter_mut().find(|clip| clip.id == clip_id) {
                clip.gain = gain;
            }
        }
        let clip_index = {
            let mut runtime = self.runtime.lock();
            let Some(clip_index) = runtime.clips.iter().position(|clip| clip.id == clip_id) else {
                return Ok(());
            };
            runtime.update_clip_gain(clip_index, gain);
            clip_index
        };
        self.send_command(EngineCommand::SetClipGain { clip_index, gain })
    }

    /// Set one send's level (linear, 0..2), live, so a send fader can follow
    /// a drag without a project sync per pointer sample. The stored snapshot
    /// and the graph mirror take it too, so the next sync matches and a later
    /// rebuild or device reopen keeps it.
    ///
    /// `Ok(false)` when the graph has no such send yet (a send added since the
    /// last sync): the caller has to leave that one to a project sync.
    pub fn set_track_send_level(
        &self,
        track_id: &str,
        send_id: &str,
        level: f32,
    ) -> Result<bool, SphereAudioError> {
        let level = if level.is_finite() {
            level.clamp(0.0, 2.0)
        } else {
            return Ok(false);
        };
        if let Some(project) = self.project.lock().as_mut() {
            if let Some(send) = project
                .tracks
                .iter_mut()
                .find(|track| track.id == track_id)
                .and_then(|track| track.sends.iter_mut().find(|send| send.id == send_id))
            {
                send.level = level;
            }
        }
        let (track_index, send_index) = {
            let mut runtime = self.runtime.lock();
            let Some(track_index) = runtime.tracks.iter().position(|t| t.id == track_id) else {
                return Ok(false);
            };
            let Some(send_index) = runtime.tracks[track_index]
                .sends
                .iter()
                .position(|send| send.id == send_id)
            else {
                return Ok(false);
            };
            runtime.update_track_send_level(track_index, send_index, level);
            (track_index, send_index)
        };
        self.send_command(EngineCommand::SetTrackSendLevel {
            track_index,
            send_index,
            level,
        })
        .map(|()| true)
    }

    /// Clear Listen on every channel — the Control Room returns to its
    /// selected source, normally the master bus.
    pub fn clear_all_listen(&self) -> Result<(), SphereAudioError> {
        self.monitor_mirror.lock().listen.clear();
        self.send_command(EngineCommand::ClearAllListen)
    }

    pub fn update_track_input_state(
        &self,
        track_id: &str,
        record_armed: bool,
        monitor_enabled: bool,
        input_source: EngineTrackInputSourceSnapshot,
    ) -> Result<(), SphereAudioError> {
        self.update_track_input_state_inner(
            track_id,
            record_armed,
            monitor_enabled,
            Some(input_source),
        )
    }

    pub fn update_track_input_flags(
        &self,
        track_id: &str,
        record_armed: bool,
        monitor_enabled: bool,
    ) -> Result<(), SphereAudioError> {
        self.update_track_input_state_inner(track_id, record_armed, monitor_enabled, None)
    }

    pub(super) fn update_track_input_state_inner(
        &self,
        track_id: &str,
        record_armed: bool,
        monitor_enabled: bool,
        explicit_input_source: Option<EngineTrackInputSourceSnapshot>,
    ) -> Result<(), SphereAudioError> {
        // Serialize with start/stop recording. A non-ASIO take owns the only
        // capture stream; route changes must never race it and open a second
        // client. ASIO uses the same fail-closed policy for one predictable UI
        // contract while a take is active.
        let recording_guard = self.recording.lock();
        if recording_guard.is_some() {
            return Err(SphereAudioError::InvalidConfig(
                "track input routing cannot change during an active recording".to_string(),
            ));
        }

        let mut project = self.project.lock();
        let snapshot = project
            .as_mut()
            .ok_or_else(|| SphereAudioError::InvalidConfig("no project is loaded".to_string()))?;
        let track_index = snapshot
            .tracks
            .iter()
            .position(|track| track.id == track_id)
            .ok_or_else(|| {
                SphereAudioError::InvalidConfig(format!("track '{track_id}' was not found"))
            })?;
        let input_source = explicit_input_source
            .unwrap_or_else(|| snapshot.tracks[track_index].input_source.clone());
        if input_source.channels.len() > 2 {
            return Err(SphereAudioError::InvalidConfig(format!(
                "track '{track_id}' input route has {} channels; only mono or stereo routes are supported",
                input_source.channels.len()
            )));
        }
        if matches!(input_source.channels.as_slice(), [left, right] if left == right) {
            return Err(SphereAudioError::InvalidConfig(format!(
                "track '{track_id}' stereo input route uses channel {} twice",
                input_source.channels[0].saturating_add(1)
            )));
        }
        // A jam or loopback route names no driver channel, so there is nothing
        // to validate against one — and a `trk:` id is never the ASIO driver.
        #[cfg(all(target_os = "windows", feature = "asio"))]
        if !input_source.is_internal() {
            if let Some(caps) = self.asio_caps.lock().clone() {
                self.validate_asio_input_source(track_id, &input_source, &caps)?;
            }
        }

        let old_armed = snapshot.tracks[track_index].armed;
        let old_monitor = snapshot.tracks[track_index].input_monitor;
        let old_source = snapshot.tracks[track_index].input_source.clone();
        snapshot.tracks[track_index].armed = record_armed;
        snapshot.tracks[track_index].input_monitor = monitor_enabled;
        snapshot.tracks[track_index].input_source = input_source.clone();

        // `recording_guard` is still held — see `sync_live_input_stream_locked`.
        if let Err(error) = self.sync_live_input_stream_locked(snapshot, &recording_guard) {
            snapshot.tracks[track_index].armed = old_armed;
            snapshot.tracks[track_index].input_monitor = old_monitor;
            snapshot.tracks[track_index].input_source = old_source;
            let _ = self.sync_live_input_stream_locked(snapshot, &recording_guard);
            return Err(error);
        }

        // A `trk:` route names a track, not a device, so it cannot go through
        // `from_route` — that would read the hardware channels the route
        // happens to carry. Resolving it needs the whole track list, which is
        // also where the publish flags come from, so both are done together
        // below.
        let next_runtime_source = if input_source.is_loopback() {
            RuntimeTrackInputSource::None
        } else {
            RuntimeTrackInputSource::from_route(
                &input_source.channels,
                self.jam_slot_for(&input_source),
            )
        };
        let publishers_before: Vec<bool> = {
            let runtime = self.runtime.lock();
            (0..snapshot.tracks.len())
                .map(|index| runtime.track_loopback_publish(index))
                .collect()
        };
        self.runtime.lock().update_track_input_state(
            track_index,
            record_armed,
            monitor_enabled,
            next_runtime_source,
        );
        // Re-resolve every loopback route against the edited project, then tell
        // the callback about the tracks whose publish flag actually moved. The
        // destination's own route rides the `SetTrackInputState` below.
        let (next_runtime_source, publish_changes) = {
            let mut runtime = self.runtime.lock();
            runtime.resolve_loopback_inputs(snapshot);
            let resolved = runtime.track_input_source(track_index);
            let changes: Vec<(usize, bool)> = publishers_before
                .iter()
                .enumerate()
                .filter_map(|(index, before)| {
                    let now = runtime.track_loopback_publish(index);
                    (now != *before).then_some((index, now))
                })
                .collect();
            (resolved.unwrap_or(next_runtime_source), changes)
        };
        for (index, publish) in publish_changes {
            let _ = self.send_command(EngineCommand::SetTrackLoopbackPublish {
                track_index: index,
                publish,
            });
        }
        let command = EngineCommand::SetTrackInputState {
            track_index,
            record_armed,
            monitor_enabled,
            input_source: next_runtime_source,
        };
        let result = match self.send_command(command) {
            Ok(()) | Err(SphereAudioError::EngineNotOpen) => {
                self.input_state_revision.fetch_add(1, Ordering::Release);
                Ok(())
            }
            Err(error) => {
                snapshot.tracks[track_index].armed = old_armed;
                snapshot.tracks[track_index].input_monitor = old_monitor;
                snapshot.tracks[track_index].input_source = old_source.clone();
                self.runtime.lock().update_track_input_state(
                    track_index,
                    old_armed,
                    old_monitor,
                    RuntimeTrackInputSource::from_route(
                        &old_source.channels,
                        self.jam_slot_for(&old_source),
                    ),
                );
                let _ = self.sync_live_input_stream_locked(snapshot, &recording_guard);
                Err(error)
            }
        };
        drop(project);
        drop(recording_guard);
        result
    }

    pub fn update_track_param(
        &self,
        track_id: &str,
        param_id: &str,
        value: f64,
    ) -> Result<(), SphereAudioError> {
        if track_id == "__master__" && param_id == "volume" {
            return self.set_master_volume(value as f32);
        }

        if !matches!(param_id, "volume" | "pan" | "muted" | "solo") {
            return self.update_track_param_other(track_id, param_id, value);
        }
        // Resolved here, against the mirror of the graph the callback holds
        // (kept in lockstep with every `LoadProject`), so the callback looks
        // nothing up by id and frees no string. A track the graph does not
        // have is a no-op, as the id lookup was.
        let Some(track_index) = self
            .runtime
            .lock()
            .tracks
            .iter()
            .position(|track| track.id == track_id)
        else {
            return Ok(());
        };
        let live = match param_id {
            "volume" => self.send_command(EngineCommand::SetTrackVolume {
                track_index,
                value: value as f32,
            }),
            "pan" => self.send_command(EngineCommand::SetTrackPan {
                track_index,
                value: value as f32,
            }),
            "muted" => self.send_command(EngineCommand::SetTrackMute {
                track_index,
                muted: value != 0.0,
            }),
            _ => self.send_command(EngineCommand::SetTrackSolo {
                track_index,
                solo: value != 0.0,
            }),
        };
        if live.is_ok() {
            self.mirror_live_track_control(track_id, param_id, value);
        }
        live
    }

    /// Keep the control thread's copies of the graph in step with a live
    /// volume / pan / mute / solo change.
    ///
    /// The command reaches only the callback's graph. The control thread's
    /// mirror (`runtime`) is what a device reopen starts the callback from,
    /// and the stored snapshot (`project`) is what later edits are reconciled
    /// against; both kept the value from the last full project sync. A mute or
    /// solo made since then came back on the next reopen — most visibly a
    /// cleared solo returning, which silences every other channel while the
    /// mixer shows nothing soloed.
    pub(super) fn mirror_live_track_control(&self, track_id: &str, param_id: &str, value: f64) {
        {
            let mut runtime = self.runtime.lock();
            match param_id {
                "volume" => {
                    runtime.update_track_volume(track_id, value as f32);
                }
                "pan" => runtime.update_track_pan(track_id, value as f32),
                "muted" => runtime.update_track_mute(track_id, value != 0.0),
                "solo" => runtime.update_track_solo(track_id, value != 0.0),
                _ => return,
            }
        }
        if let Some(project) = self.project.lock().as_mut() {
            if let Some(track) = project.tracks.iter_mut().find(|t| t.id == track_id) {
                match param_id {
                    "volume" => track.volume = value as f32,
                    "pan" => track.pan = value as f32,
                    "muted" => track.muted = value != 0.0,
                    "solo" => track.solo = value != 0.0,
                    _ => {}
                }
            }
        }
    }

    pub(super) fn update_track_param_other(
        &self,
        track_id: &str,
        param_id: &str,
        value: f64,
    ) -> Result<(), SphereAudioError> {
        match param_id {
            "previewMode" => self.send_command(EngineCommand::SetTrackPreviewMode {
                track_id: track_id.into(),
                value: value as f32,
            }),
            other => {
                // Unknown param — log but don't error (UI might send future params)
                eprintln!("[SphereAudio] Unknown track param: '{other}' (track={track_id})");
                Ok(())
            }
        }
    }
}

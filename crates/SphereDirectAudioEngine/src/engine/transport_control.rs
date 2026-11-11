//! Transport: play, pause, seek, tempo and time signature, loop, metronome and count-in.

use super::*;

impl EngineInner {
    pub fn play(&self) -> Result<(), SphereAudioError> {
        if self.shared.playing.load(Ordering::Relaxed) {
            if transport_freeze_debug_enabled() {
                eprintln!("[play-debug engine] play() skipped — transport already playing");
            }
            return Ok(());
        }
        if transport_freeze_debug_enabled() {
            eprintln!("[play-debug engine] play() queuing StartTransport");
        }
        self.send_command(EngineCommand::StartTransport)
    }

    /// Stop the transport.
    ///
    /// Stopping the transport is also the end of input capture. Recording keeps
    /// its own lifecycle — the take is finalized by [`Self::stop_recording`],
    /// which the caller still owns and which is what produces the clip — but no
    /// path may leave the writer consuming input after the transport has
    /// stopped, however it stopped: Space, the Stop button, a project close, a
    /// device change, a control surface, a remote command. Ending capture here
    /// is what makes "the file stops growing at Stop" true for every one of
    /// them instead of only for the paths that remembered to ask.
    pub fn pause(&self) -> Result<(), SphereAudioError> {
        self.end_recording_capture();
        // Eagerly clear the shared transport flag so UI playhead / polls stop
        // at the command without waiting for the next audio callback drain.
        // The audio thread still applies StopTransport (notes-off, PDC clear)
        // when it drains the queue.
        self.shared.playing.store(false, Ordering::Relaxed);
        self.send_command(EngineCommand::StopTransport)
    }

    /// What the transport is doing, as one value rather than as two flags that
    /// can disagree. See [`crate::transport::TransportState`].
    pub fn transport_state(&self) -> crate::transport::TransportState {
        crate::transport::TransportState::from_flags(
            self.shared.playing.load(Ordering::Relaxed),
            self.shared.recording_active.load(Ordering::Relaxed),
        )
    }

    /// Stop the active take consuming input without finalizing it.
    ///
    /// The session stays alive so `stop_recording` can still flush the writer
    /// and hand back the take; it simply stops growing. Split from
    /// `stop_recording` because the two answer different questions: this one is
    /// "the transport stopped", that one is "give me the take".
    pub(super) fn end_recording_capture(&self) {
        let Some(session) = self.recording.lock().as_ref().map(|session| {
            session.recording_active.store(false, Ordering::Relaxed);
            session.started_at
        }) else {
            return;
        };
        self.shared.recording_active.store(false, Ordering::Relaxed);
        if recording_debug_enabled() {
            eprintln!(
                "[recording] capture ended by transport stop after {:.3}s; take awaits finalize",
                session.elapsed().as_secs_f64()
            );
        }
    }

    pub fn seek(&self, position_seconds: f64) -> Result<(), SphereAudioError> {
        // #380: have each disk stream start decoding the seek target now, so
        // the first callback after the seek finds it buffered instead of
        // playing a block of silence. Skipped (never waited for) while a
        // graph build holds the cache.
        if let Some(cache) = self.audio_cache.try_lock() {
            for source in cache.values() {
                if let ClipAudioSource::Streaming(stream) = source.as_ref() {
                    stream.prefetch_timeline(position_seconds);
                }
            }
        }
        self.send_command(EngineCommand::Seek { position_seconds })
    }

    pub fn set_metronome_suspended(&self, suspended: bool) -> Result<(), SphereAudioError> {
        self.send_command(EngineCommand::SetMetronomeSuspended(suspended))
    }

    pub fn set_metronome_enabled(&self, enabled: bool) -> Result<(), SphereAudioError> {
        self.shared
            .metronome_enabled
            .store(enabled, Ordering::Relaxed);
        self.send_command(EngineCommand::SetMetronomeEnabled(enabled))
    }

    /// Start an audible record count-in of `beats` beats at `target_beat`'s
    /// tempo, accented every `beats_per_bar`.
    ///
    /// The transport stays where it is: the count-in is a pre-roll *in place*,
    /// not a rewind. That is what makes it work from bar 1, where there is no
    /// earlier timeline to roll through — the case the old timer-driven
    /// count-in spent as a silent wait.
    ///
    /// The click spacing is resolved here, from the tempo map at the beat the
    /// take will start on, so counting into a slow section counts slowly.
    pub fn start_count_in(
        &self,
        beats: u32,
        beats_per_bar: u32,
        target_beat: f64,
    ) -> Result<(), SphereAudioError> {
        let sample_rate = {
            let rate = self.shared.sample_rate.load(Ordering::Relaxed);
            if rate > 0 {
                rate as f64
            } else {
                crate::native::DEFAULT_SAMPLE_RATE as f64
            }
        };
        let map = self.last_sent_tempo_map.lock().clone();
        let target_beat = target_beat.max(0.0);
        // One beat *at the take's position*, in samples: the difference between
        // the beat before it and the beat itself, which is exact under a ramp
        // where `60 / bpm` would not be.
        let samples_per_beat = map
            .samples_at_beat(target_beat + 1.0, sample_rate)
            .saturating_sub(map.samples_at_beat(target_beat, sample_rate))
            .max(1);
        self.shared.count_in_remaining_samples.store(
            samples_per_beat.saturating_mul(beats.max(1) as u64),
            Ordering::Relaxed,
        );
        self.send_command(EngineCommand::StartCountIn {
            beats,
            beats_per_bar,
            samples_per_beat,
        })
    }

    /// Abandon a count-in in progress.
    pub fn cancel_count_in(&self) -> Result<(), SphereAudioError> {
        self.shared
            .count_in_remaining_samples
            .store(0, Ordering::Relaxed);
        self.send_command(EngineCommand::CancelCountIn)
    }

    /// Samples left in the count-in, `0` when none is running.
    pub fn count_in_remaining_samples(&self) -> u64 {
        self.shared
            .count_in_remaining_samples
            .load(Ordering::Relaxed)
    }

    /// Click level and timbre (Settings → Recording → Metronome).
    ///
    /// `volume` is the persisted 0..1 control position. The persisted default
    /// (0.8) maps to unity — the click level Studio has always had — so wiring
    /// this control up changes nothing until the user moves it, and the top of
    /// the slider is headroom rather than a new default.
    pub fn set_metronome_voice(
        &self,
        volume: f32,
        sound_label: &str,
    ) -> Result<(), SphereAudioError> {
        let sound = crate::backend::render::MetronomeSound::from_label(sound_label);
        let gain = (volume.max(0.0) / METRONOME_UNITY_VOLUME).clamp(0.0, 2.0);
        self.shared
            .metronome_gain
            .store(f32_store(gain), Ordering::Relaxed);
        self.shared
            .metronome_sound
            .store(sound.code(), Ordering::Relaxed);
        self.send_command(EngineCommand::SetMetronomeVoice {
            volume: gain,
            sound: sound.code(),
        })
    }

    pub fn set_bpm(&self, bpm: f64) -> Result<(), SphereAudioError> {
        self.set_tempo_map(bpm, Vec::new())
    }

    /// Seek by musical position, converted through the engine's own tempo map.
    ///
    /// The engine owns beat<->time for playback, MIDI and the metronome grid, so
    /// a caller computing `beat * 60 / bpm` lands somewhere else entirely as
    /// soon as the project has one tempo point — and the click then falls off
    /// the grid it was seeked to.
    pub fn seek_beats(&self, beat: f64) -> Result<(), SphereAudioError> {
        let seconds = self
            .last_sent_tempo_map
            .lock()
            .seconds_at_beat(beat.max(0.0));
        self.seek(seconds)
    }

    /// Replace the authoritative tempo map used for playback, metronome, and
    /// beat/time/sample conversion. `points` empty = static tempo at `default_bpm`.
    pub fn set_tempo_map(
        &self,
        default_bpm: f64,
        points: Vec<crate::types::EngineTempoPointSnapshot>,
    ) -> Result<(), SphereAudioError> {
        let snapshot = crate::runtime::build_tempo_map_from_points(default_bpm, &points);
        transport::store_f64_bits(&self.shared.bpm_bits, default_bpm);
        // Held to the send, like `load_project` and `replace_midi_schedule`
        // hold it across their mirror update and send: the schedule re-timed
        // below is then the one the callback's graph holds when this arrives.
        let mut project = self.project.lock();
        if let Some(project) = project.as_mut() {
            project.bpm = default_bpm;
            project.tempo_points = points;
        }
        // Play, every project sync, and every settings change push a map,
        // almost always the same one — so gate on what was actually sent last.
        // (Not on the mirror's map: a stream opened later rebuilds its graph
        // from the project, so the mirror cannot say what the callback plays.)
        {
            let mut last_sent = self.last_sent_tempo_map.lock();
            if last_sent.segments == snapshot.segments {
                return Ok(());
            }
            *last_sent = snapshot.clone();
        }
        // Re-time the MIDI schedule here, on the control thread: the callback
        // used to recompute and re-sort every event list (allocating the sort
        // buffer) and clone and free tempo maps, in one block, mid-playback.
        // Built from the mirror, which holds the same schedule as the running
        // graph; the callback swaps it in, or re-times its own in place without
        // allocating if the two have drifted apart.
        let (mut mirror_midi, mirror_rate) = {
            let mirror = self.runtime.lock();
            (
                crate::runtime::RuntimeMidiData {
                    midi_clips: mirror.midi_clips.clone(),
                    midi_tracks: mirror.midi_tracks.clone(),
                },
                mirror.sample_rate,
            )
        };
        let live_rate = match self.shared.sample_rate.load(Ordering::Relaxed) {
            0 => mirror_rate,
            rate => rate,
        };
        mirror_midi.retime(&snapshot, mirror_rate);
        let mut realtime_midi = mirror_midi.realtime_copy();
        if live_rate != mirror_rate {
            realtime_midi.retime(&snapshot, live_rate);
        }
        realtime_midi.prepare_block_event_buffers();
        {
            let mut mirror = self.runtime.lock();
            mirror.tempo_map = snapshot.clone();
            mirror.midi_clips = mirror_midi.midi_clips;
            mirror.midi_tracks = mirror_midi.midi_tracks;
        }
        prime_audio_thread_resources();
        let result = self.send_command(EngineCommand::SetTempoMap(Box::new(
            crate::command::TempoMapChange {
                metronome_map: snapshot.clone(),
                map: snapshot,
                midi: Some(realtime_midi),
                sample_rate: live_rate,
            },
        )));
        drop(project);
        result
    }

    /// Install a single project-wide signature, replacing any marker map.
    ///
    /// This is the *static* signature path; a project with time-signature
    /// markers reaches the engine through [`Self::set_time_signature_map`].
    pub fn set_time_signature(
        &self,
        numerator: u32,
        denominator: u32,
    ) -> Result<(), SphereAudioError> {
        self.shared
            .time_sig_num
            .store(numerator.max(1), Ordering::Relaxed);
        self.shared
            .time_sig_den
            .store(denominator.max(1), Ordering::Relaxed);
        let snapshot = crate::time_signature_map::RuntimeTimeSignatureMapSnapshot::static_sig(
            numerator.clamp(1, 64) as u16,
            denominator.clamp(1, 64) as u16,
        );
        {
            let mut last_sent = self.last_sent_time_signature_map.lock();
            if *last_sent == snapshot {
                return Ok(());
            }
            *last_sent = snapshot.clone();
        }
        crate::graveyard::prime();
        self.send_command(EngineCommand::SetTimeSignatureMap(Box::new(snapshot)))
    }

    pub fn set_time_signature_map(
        &self,
        points: Vec<crate::time_signature_map::RuntimeTimeSignaturePointSnapshot>,
    ) -> Result<(), SphereAudioError> {
        let snapshot =
            crate::time_signature_map::RuntimeTimeSignatureMapSnapshot::from_points(points);
        if let Some(pt) = snapshot.points().first() {
            self.shared
                .time_sig_num
                .store(pt.numerator.max(1) as u32, Ordering::Relaxed);
            self.shared
                .time_sig_den
                .store(pt.denominator.max(1) as u32, Ordering::Relaxed);
        }
        {
            let mut last_sent = self.last_sent_time_signature_map.lock();
            if *last_sent == snapshot {
                return Ok(());
            }
            *last_sent = snapshot.clone();
        }
        crate::graveyard::prime();
        self.send_command(EngineCommand::SetTimeSignatureMap(Box::new(snapshot)))
    }

    /// Whether the control thread is holding a real marker map (more than one
    /// signature). Used by `load_project` to decide whether the project's single
    /// static signature is news or would flatten a map the UI already pushed.
    pub(super) fn has_time_signature_markers(&self) -> bool {
        self.last_sent_time_signature_map.lock().points().len() > 1
    }

    pub fn set_loop(
        &self,
        enabled: bool,
        start_seconds: f64,
        end_seconds: f64,
    ) -> Result<(), SphereAudioError> {
        self.send_command(EngineCommand::SetLoop {
            enabled,
            start_seconds,
            end_seconds,
        })
    }

    /// Read the current transport/clock snapshot for UI polling.
    pub fn transport_snapshot(&self) -> RuntimeTransportSnapshot {
        RuntimeTransportSnapshot::from_shared(&self.shared, &self.tempo_map())
    }

    pub(super) fn tempo_map(&self) -> TempoMap {
        self.project
            .lock()
            .as_ref()
            .map(tempo_map_from_project_snapshot)
            .unwrap_or_else(|| {
                TempoMap::static_tempo(transport::f64_from_bits(
                    self.shared.bpm_bits.load(Ordering::Relaxed),
                ))
            })
    }
}

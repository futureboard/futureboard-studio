//! Jam publishing and render capture.

use super::*;

impl EngineInner {
    /// Start or stop sharing one track or bus over Audio Jam.
    ///
    /// Returns the bus slot the track now feeds, or `None` when sharing was
    /// stopped. The caller hands that slot to the jam client, which pulls from
    /// it on its own thread; the audio callback only ever writes into it.
    ///
    /// A publish is session state rather than project state: reopening a
    /// project does not resume broadcasting, and starting one does not rebuild
    /// the graph.
    pub fn set_track_jam_publish(
        &self,
        track_id: &str,
        enabled: bool,
    ) -> Result<Option<u32>, SphereAudioError> {
        let track_index = {
            let project = self.project.lock();
            let snapshot = project.as_ref().ok_or_else(|| {
                SphereAudioError::InvalidConfig("no project is loaded".to_string())
            })?;
            snapshot
                .tracks
                .iter()
                .position(|track| track.id == track_id)
                .ok_or_else(|| {
                    SphereAudioError::InvalidConfig(format!("track '{track_id}' was not found"))
                })?
        };

        let key = crate::jam_bus::publish_key_track(track_id);
        let slot = if enabled {
            match self.shared.jam_bus.bind_publish(&key) {
                Some(index) => Some(index as u32),
                None => {
                    return Err(SphereAudioError::InvalidConfig(
                        "every Audio Jam publish slot is in use".to_string(),
                    ))
                }
            }
        } else {
            self.shared.jam_bus.release_publish(&key);
            None
        };

        self.runtime
            .lock()
            .update_track_jam_publish(track_index, slot);
        match self.send_command(EngineCommand::SetTrackJamPublish { track_index, slot }) {
            // A closed engine is not a failure here: the runtime mirror already
            // carries the binding, and the next graph the callback receives will
            // have it.
            Ok(()) | Err(SphereAudioError::EngineNotOpen) => Ok(slot),
            Err(error) => {
                if enabled {
                    self.shared.jam_bus.release_publish(&key);
                    self.runtime
                        .lock()
                        .update_track_jam_publish(track_index, None);
                }
                Err(error)
            }
        }
    }

    /// Share a set of tracks as one Audio Jam multitrack stream, or stop.
    ///
    /// `track_ids` is the layout, in order: the first fills channels 1-2, the
    /// second 3-4, and so on. Passing an empty list stops the stream and
    /// releases the slot.
    ///
    /// Returns the number of channels the stream carries, which is what the
    /// caller announces to the room. The engine claims the wide publish slot
    /// and starts assembling blocks into it; announcing the stream is the jam
    /// client's half, and the two are separate calls so a failure in either
    /// never leaves the other running.
    /// Start a realtime render capture of the master and `track_ids`, in
    /// that lane order, holding up to `capacity_frames` unread frames. It
    /// records nothing until [`crate::render_capture::RenderCapture::set_recording`]
    /// and the transport is playing. Replaces a capture still installed.
    pub fn begin_render_capture(
        &self,
        track_ids: &[String],
        capacity_frames: usize,
    ) -> Result<Arc<crate::render_capture::RenderCapture>, SphereAudioError> {
        let indices = {
            let project = self.project.lock();
            let snapshot = project.as_ref().ok_or_else(|| {
                SphereAudioError::InvalidConfig("no project is loaded".to_string())
            })?;
            let indices = render_capture_indices(snapshot, track_ids);
            if let Some(missing) = track_ids
                .iter()
                .zip(&indices)
                .find_map(|(id, index)| index.is_none().then_some(id))
            {
                return Err(SphereAudioError::InvalidConfig(format!(
                    "track '{missing}' was not found"
                )));
            }
            indices
        };
        let sample_rate = self.shared.sample_rate.load(Ordering::Relaxed).max(1);
        let capture = Arc::new(crate::render_capture::RenderCapture::new(
            track_ids.len(),
            capacity_frames,
            sample_rate,
        ));
        capture.map_tracks(&indices);
        *self.render_capture.lock() = Some((capture.clone(), track_ids.to_vec()));
        self.runtime.lock().render_capture = Some(capture.clone());
        match self.send_command(EngineCommand::SetRenderCapture(Some(capture.clone()))) {
            Ok(()) => Ok(capture),
            Err(error) => {
                self.end_render_capture();
                Err(error)
            }
        }
    }

    /// Remove the realtime render capture. Waits (bounded) for the callback to
    /// let go of it before this thread drops the last reference.
    pub fn end_render_capture(&self) {
        let Some((capture, _)) = self.render_capture.lock().take() else {
            return;
        };
        capture.set_recording(false);
        self.runtime.lock().render_capture = None;
        let _ = self.send_command(EngineCommand::SetRenderCapture(None));
        let _ = self.wait_for_command_barrier(std::time::Duration::from_secs(2));
        drop(capture);
    }

    /// Frames between the transport and the master output it produces: the
    /// plug-in delay compensation the live graph applies. A realtime render
    /// skips this many frames from where it starts playing, as an offline
    /// render does, so both files line up with the timeline.
    pub fn render_latency_frames(&self) -> u64 {
        let runtime = self.runtime.lock();
        crate::export::live_render_latency_frames(&runtime.latency_graph, runtime.pdc_enabled)
    }

    pub fn set_multitrack_jam_publish(
        &self,
        track_ids: &[String],
    ) -> Result<usize, SphereAudioError> {
        if track_ids.len() > crate::jam_bus::MAX_MULTITRACK_PAIRS {
            return Err(SphereAudioError::InvalidConfig(format!(
                "an Audio Jam multitrack stream carries at most {} tracks",
                crate::jam_bus::MAX_MULTITRACK_PAIRS
            )));
        }

        let indices: Vec<usize> = {
            let project = self.project.lock();
            let snapshot = project.as_ref().ok_or_else(|| {
                SphereAudioError::InvalidConfig("no project is loaded".to_string())
            })?;
            track_ids
                .iter()
                .map(|track_id| {
                    snapshot
                        .tracks
                        .iter()
                        .position(|track| &track.id == track_id)
                        .ok_or_else(|| {
                            SphereAudioError::InvalidConfig(format!(
                                "track '{track_id}' was not found"
                            ))
                        })
                })
                .collect::<Result<Vec<_>, _>>()?
        };

        // Release first, always: the layout is fixed for the life of a claim,
        // so changing which tracks are shared is a fresh claim rather than an
        // edit of the live one.
        self.shared
            .jam_bus
            .release_publish(crate::jam_bus::PUBLISH_KEY_MULTITRACK);

        let mut pairs = [crate::jam_bus::NO_JAM_PAIR; crate::jam_bus::MAX_MULTITRACK_PAIRS];
        for (pair, index) in indices.iter().enumerate() {
            pairs[pair] = *index as u32;
        }

        let channels = indices.len() * 2;
        if channels > 0
            && self
                .shared
                .jam_bus
                .bind_multitrack_publish(channels)
                .is_none()
        {
            return Err(SphereAudioError::InvalidConfig(
                "the Audio Jam multitrack slot is already in use".to_string(),
            ));
        }

        self.runtime.lock().update_jam_multitrack_pairs(&indices);
        match self.send_command(EngineCommand::SetJamMultitrackPairs { pairs }) {
            // A closed engine is not a failure: the runtime mirror already
            // carries the assignment, and the next graph the callback receives
            // will have it.
            Ok(()) | Err(SphereAudioError::EngineNotOpen) => Ok(channels),
            Err(error) => {
                self.shared
                    .jam_bus
                    .release_publish(crate::jam_bus::PUBLISH_KEY_MULTITRACK);
                self.runtime.lock().update_jam_multitrack_pairs(&[]);
                Err(error)
            }
        }
    }

    /// Claim (or find) the jam bus slot a track's route names.
    ///
    /// Binding here rather than when the stream is first seen means a route
    /// restored from a project file reserves its slot immediately, so a
    /// performer who joins later lands in the slot the track is already
    /// pointing at. `None` for a hardware route, and also when every slot is
    /// taken — in which case the track resolves to no input and reports
    /// silence rather than being routed to somebody else's stream.
    pub(super) fn jam_slot_for(&self, source: &EngineTrackInputSourceSnapshot) -> Option<u32> {
        let stream_id = source.jam_stream_id()?;
        self.shared
            .jam_bus
            .bind_input(stream_id)
            .map(|index| index as u32)
    }
}

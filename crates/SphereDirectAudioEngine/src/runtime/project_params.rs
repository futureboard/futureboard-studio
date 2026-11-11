//! RuntimeProject: live track, send, spatial, jam and insert parameter updates.

use super::*;

impl RuntimeProject {
    #[inline]
    /// Returns `false` when no track carries `track_id` — the value is then
    /// dropped, which is what a fader that moves without changing the sound
    /// looks like from the outside. The callers log the answer under their
    /// debug flags; this stays allocation- and IO-free for the audio thread.
    pub fn update_track_volume(&mut self, track_id: &str, volume: f32) -> bool {
        if let Some(track) = self.tracks.iter_mut().find(|t| t.id == track_id) {
            track.volume = volume.clamp(0.0, 2.0);
            return true;
        }
        false
    }

    #[inline]
    pub fn update_track_pan(&mut self, track_id: &str, pan: f32) {
        if let Some(track) = self.tracks.iter_mut().find(|t| t.id == track_id) {
            track.pan = pan.clamp(-1.0, 1.0);
        }
    }

    #[inline]
    pub fn update_track_mute(&mut self, track_id: &str, muted: bool) {
        if let Some(track) = self.tracks.iter_mut().find(|t| t.id == track_id) {
            track.muted = muted;
        }
    }

    /// Live fader by resolved index; `false` when the graph has no such track.
    #[inline]
    pub fn update_track_volume_at(&mut self, track_index: usize, volume: f32) -> bool {
        match self.tracks.get_mut(track_index) {
            Some(track) => {
                track.volume = volume.clamp(0.0, 2.0);
                true
            }
            None => false,
        }
    }

    #[inline]
    pub fn update_track_pan_at(&mut self, track_index: usize, pan: f32) {
        if let Some(track) = self.tracks.get_mut(track_index) {
            track.pan = pan.clamp(-1.0, 1.0);
        }
    }

    #[inline]
    pub fn update_track_mute_at(&mut self, track_index: usize, muted: bool) {
        if let Some(track) = self.tracks.get_mut(track_index) {
            track.muted = muted;
        }
    }

    #[inline]
    pub fn update_track_solo_at(&mut self, track_index: usize, solo: bool) {
        if let Some(track) = self.tracks.get_mut(track_index) {
            track.solo = solo;
            self.has_solo = self.tracks.iter().any(|t| t.solo);
        }
    }

    #[inline]
    /// Place each channel as the snapshot says; a channel it does not list
    /// stays front and centre.
    pub fn apply_spatial_sources(&mut self, snapshot: &crate::types::EngineSpatialSnapshot) {
        for source in &snapshot.sources {
            if let Some(track) = self.tracks.iter_mut().find(|t| t.id == source.track_id) {
                track.spatial_params = source.params.sanitized();
            }
        }
    }

    /// Give every channel that sums into the master its spatialiser for the
    /// graph's format, and take them away from the rest. Must run after
    /// [`Self::resolve_indices`], which decides where each channel goes.
    /// Allocates; control thread only.
    pub fn ensure_spatial(&mut self) {
        let format = self.spatial.format;
        let sample_rate = self.spatial.sample_rate;
        for track in self.tracks.iter_mut() {
            let to_master = track.track_type != "master" && track.output_track_index.is_none();
            if !format.is_spatial() || !to_master {
                track.spatial = None;
                continue;
            }
            let keep = matches!(
                (&track.spatial, format),
                (
                    Some(solfege_spatialaudio::SpatialSource::Binaural(_)),
                    solfege_spatialaudio::SpatialFormat::Binaural
                )
            ) || matches!(
                (&track.spatial, format),
                (Some(solfege_spatialaudio::SpatialSource::Surround(source)), solfege_spatialaudio::SpatialFormat::Surround(layout))
                    if source.layout() == layout
            );
            if !keep {
                track.spatial = solfege_spatialaudio::SpatialSource::for_format(
                    format,
                    sample_rate,
                    DEFAULT_AUDIO_BLOCK_CAPACITY,
                );
            }
        }
    }

    /// Move one channel in the room. Realtime-safe: a copy.
    #[inline]
    /// New parts for the multitimbral Soundfont Player on `track_index`.
    pub fn update_soundfont_channels(&mut self, track_index: usize, channels: &SoundfontChannels) {
        if let Some(player) = self
            .tracks
            .get_mut(track_index)
            .and_then(|track| track.soundfont_player.as_mut())
        {
            player.set_channels(channels);
        }
    }

    /// Sets the gain `clip_index` plays toward (linear, clamped to 0..4).
    pub fn update_clip_gain(&mut self, clip_index: usize, gain: f32) {
        if let Some(clip) = self.clips.get_mut(clip_index) {
            clip.gain_target = gain.clamp(0.0, 4.0);
        }
    }

    /// Live send level (linear, 0..2) by resolved indices; an index the graph
    /// does not have is ignored.
    #[inline]
    pub fn update_track_send_level(&mut self, track_index: usize, send_index: usize, level: f32) {
        if let Some(send) = self
            .tracks
            .get_mut(track_index)
            .and_then(|track| track.sends.get_mut(send_index))
        {
            send.level = level.clamp(0.0, 2.0);
        }
    }

    pub fn update_track_spatial(
        &mut self,
        track_index: usize,
        params: solfege_spatialaudio::SourceParams,
    ) {
        if let Some(track) = self.tracks.get_mut(track_index) {
            track.spatial_params = params.sanitized();
        }
    }

    pub fn update_track_solo(&mut self, track_id: &str, solo: bool) {
        if let Some(track) = self.tracks.iter_mut().find(|t| t.id == track_id) {
            track.solo = solo;
            self.has_solo = self.tracks.iter().any(|t| t.solo);
        }
    }

    #[inline]
    pub fn update_track_input_state(
        &mut self,
        track_index: usize,
        record_armed: bool,
        monitor_enabled: bool,
        input_source: RuntimeTrackInputSource,
    ) {
        if let Some(track) = self.tracks.get_mut(track_index) {
            track.record_armed = record_armed;
            track.monitor_enabled = monitor_enabled;
            track.input_source = input_source;
        }
    }

    /// Point a track's post-fader output at an Audio Jam publish slot.
    #[inline]
    /// Set whether a track keeps its post-fader block for a loopback reader.
    ///
    /// Clearing it also drops the buffers on the next block the callback
    /// renders; see `capture_track_loopback`.
    pub fn update_track_loopback_publish(&mut self, track_index: usize, publish: bool) {
        if let Some(track) = self.tracks.get_mut(track_index) {
            track.loopback_publish = publish;
        }
    }

    /// Whether a track is keeping its output for a loopback reader.
    pub fn track_loopback_publish(&self, track_index: usize) -> bool {
        self.tracks
            .get(track_index)
            .is_some_and(|track| track.loopback_publish)
    }

    pub fn update_track_jam_publish(&mut self, track_index: usize, slot: Option<u32>) {
        if let Some(track) = self.tracks.get_mut(track_index) {
            track.jam_publish_slot = slot;
        }
    }

    /// A track's resolved input source, for the control thread.
    pub fn track_input_source(&self, track_index: usize) -> Option<RuntimeTrackInputSource> {
        self.tracks.get(track_index).map(|track| track.input_source)
    }

    /// Which jam publish slot a track feeds, for the control thread.
    pub fn track_jam_publish(&self, track_index: usize) -> Option<u32> {
        self.tracks.get(track_index)?.jam_publish_slot
    }

    /// Assign the channel pairs of the Audio Jam multitrack stream.
    ///
    /// `pairs[k]` is the track index that fills pair `k`; the assignment is
    /// replaced wholesale rather than edited, because a stream's layout is
    /// announced once and every receiver decodes against it. Sharing a
    /// different set of tracks means republishing, and this is the control
    /// thread's half of that.
    pub fn update_jam_multitrack_pairs(&mut self, pairs: &[usize]) {
        for track in self.tracks.iter_mut() {
            track.jam_multitrack_pair = None;
        }
        for (pair, track_index) in pairs.iter().enumerate() {
            if pair >= crate::jam_bus::MAX_MULTITRACK_PAIRS {
                break;
            }
            if let Some(track) = self.tracks.get_mut(*track_index) {
                track.jam_multitrack_pair = Some(pair as u32);
            }
        }
    }

    /// Which multitrack pair a track fills, for the control thread.
    pub fn track_jam_multitrack_pair(&self, track_index: usize) -> Option<u32> {
        self.tracks.get(track_index)?.jam_multitrack_pair
    }

    /// Apply the pair assignment carried by
    /// [`crate::command::EngineCommand::SetJamMultitrackPairs`].
    ///
    /// The callback's half of [`Self::update_jam_multitrack_pairs`], taking the
    /// fixed sentinel-terminated array the command carries rather than a slice
    /// the audio thread would have to free.
    #[inline]
    pub fn apply_jam_multitrack_pairs(
        &mut self,
        pairs: &[u32; crate::jam_bus::MAX_MULTITRACK_PAIRS],
    ) {
        for track in self.tracks.iter_mut() {
            track.jam_multitrack_pair = None;
        }
        for (pair, track_index) in pairs.iter().enumerate() {
            if *track_index == crate::jam_bus::NO_JAM_PAIR {
                continue;
            }
            if let Some(track) = self.tracks.get_mut(*track_index as usize) {
                track.jam_multitrack_pair = Some(pair as u32);
            }
        }
    }

    #[inline]
    pub fn update_track_preview_mode(&mut self, track_id: &str, mode: RuntimePreviewMode) {
        if let Some(track) = self.tracks.iter_mut().find(|t| t.id == track_id) {
            track.preview_mode = mode;
        }
    }

    #[inline]
    pub fn update_insert_param_at(
        &mut self,
        track_index: usize,
        insert_index: usize,
        param_id: crate::command::ResolvedParamId,
        value: f32,
        change: Option<&mut crate::command::InsertParamChange>,
    ) {
        let Some(track) = self.tracks.get_mut(track_index) else {
            return;
        };
        let Some(insert) = track.inserts.get_mut(insert_index) else {
            return;
        };

        if let Some(change) = change {
            if let Some(ref mut new_params) = change.params {
                std::mem::swap(&mut insert.params, new_params);
            }
            if let Some(ref mut new_dsp) = change.dsp {
                // The control thread's copy never runs audio: adopt its
                // settings but keep this insert's running filter memory, or
                // every edit (even Output or Mix) resets the EQ (#366).
                insert.dsp.adopt_settings(new_dsp);
            }
        }

        match param_id {
            crate::command::ResolvedParamId::Bypass => {
                insert.enabled = value >= 0.5;
                if insert.enabled {
                    insert.guard.rearm();
                }
            }
            crate::command::ResolvedParamId::Numeric(wire_param_id) => {
                if insert.kind_tag == RuntimeInsertKind::ExternalBridge {
                    if insert.bridge_is_builtin {
                        if let Some(sink) = insert.bridge_sink.as_ref() {
                            sink.push_param(wire_param_id, value, 0);
                        }
                    } else if let Some(sink) = insert.bridge_sink.as_ref() {
                        sink.push_param(wire_param_id, value.clamp(0.0, 1.0), 0);
                    }
                } else if let Some(vst3) = insert.vst3.as_mut() {
                    vst3.set_param(wire_param_id, value as f64);
                    insert.callback_process_log_done = false;
                    insert.silent_process_blocks = 0;
                }
            }
            crate::command::ResolvedParamId::Named(key) => {
                if insert.kind_tag == RuntimeInsertKind::ExternalBridge {
                    if let Ok(wire_id) = key.as_str().parse::<u32>() {
                        if insert.bridge_is_builtin {
                            if let Some(sink) = insert.bridge_sink.as_ref() {
                                sink.push_param(wire_id, value, 0);
                            }
                        } else if let Some(sink) = insert.bridge_sink.as_ref() {
                            sink.push_param(wire_id, value.clamp(0.0, 1.0), 0);
                        }
                    }
                } else if let Some(vst3) = insert.vst3.as_mut() {
                    if let Ok(vst3_id) = key.as_str().parse::<u32>() {
                        vst3.set_param(vst3_id, value as f64);
                        insert.callback_process_log_done = false;
                        insert.silent_process_blocks = 0;
                    }
                }
            }
        }
    }

    #[inline]
    pub fn update_insert_param(
        &mut self,
        track_id: &str,
        insert_id: &str,
        param_id: &str,
        value: f32,
    ) {
        let Some(track_index) = self.tracks.iter().position(|t| t.id == track_id) else {
            return;
        };
        let Some(insert_index) = self.tracks[track_index]
            .inserts
            .iter()
            .position(|i| i.id == insert_id)
        else {
            return;
        };
        let resolved = if param_id == "enabled" {
            crate::command::ResolvedParamId::Bypass
        } else if let Ok(wire_param_id) = param_id.parse::<u32>() {
            crate::command::ResolvedParamId::Numeric(wire_param_id)
        } else {
            crate::command::ResolvedParamId::Named(crate::command::ParamKey::new(param_id))
        };
        self.update_insert_param_at(track_index, insert_index, resolved, value, None);
    }

    pub fn set_bridge_editor_active(&mut self, track_id: &str, active: bool) {
        if active {
            self.bridge_editor_active.insert(track_id.to_string());
        } else {
            self.bridge_editor_active.remove(track_id);
        }
    }

    pub fn has_bridge_editor_active(&self) -> bool {
        !self.bridge_editor_active.is_empty()
    }

    /// Whether any track is rendered by an ARA plug-in.
    ///
    /// ARA has no host-to-plug-in transport call: the plug-in reads the host's
    /// position and playing state out of the process context it is handed per
    /// block, and that is the only channel there is. A host that stops calling
    /// the instance the moment the transport stops therefore freezes the
    /// plug-in's idea of the transport at the last block it saw — its editor
    /// playhead stops following seeks, and its own play button keeps toggling
    /// against a stale "still playing" state. The graph now runs every block
    /// the engine renders, so that context arrives on its own; this stays as
    /// the query the ARA plumbing asks about bound renderers.
    ///
    /// Realtime-safe: a length check per track, no allocation, no locking.
    pub fn has_ara_renderers(&self) -> bool {
        self.tracks
            .iter()
            .any(|track| !track.ara_renderers.is_empty())
    }
}

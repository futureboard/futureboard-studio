use super::*;

impl ClipState {
    /// Clip-id prefix of a track's live audio recording preview
    /// (`layout::audio_recording_preview_clip_id`).
    pub const AUDIO_RECORDING_PREVIEW_ID_PREFIX: &'static str = "__recording_preview__:";
    /// Clip-id prefix of a track's live MIDI recording preview
    /// (`recording_ops::midi_recording_preview_clip_id`).
    pub const MIDI_RECORDING_PREVIEW_ID_PREFIX: &'static str = "__recording_midi_preview__:";

    /// Whether `clip_id` names a UI-only recording preview clip. Those are
    /// drawn while a take records and replaced by the committed take at Stop;
    /// they must never reach the project file, which a save during recording
    /// would otherwise write as an empty ghost clip.
    pub fn is_recording_preview_clip_id(clip_id: &str) -> bool {
        clip_id.starts_with(Self::AUDIO_RECORDING_PREVIEW_ID_PREFIX)
            || clip_id.starts_with(Self::MIDI_RECORDING_PREVIEW_ID_PREFIX)
    }

    /// See [`Self::is_recording_preview_clip_id`].
    pub fn is_recording_preview_clip(&self) -> bool {
        Self::is_recording_preview_clip_id(&self.id)
    }
}

impl TimelineState {
    pub fn insert_recorded_clip(
        &mut self,
        track_id: &str,
        source_path: String,
        clip_name: String,
        start_beat: f32,
        duration_seconds: f64,
        bpm: f32,
    ) -> String {
        let duration_beats = (duration_seconds.max(0.0) * bpm.max(1.0) as f64 / 60.0) as f32;
        self.insert_audio_clip_with_duration(
            track_id.to_string(),
            source_path,
            clip_name,
            start_beat,
            duration_beats.max(0.01),
            Some(duration_seconds),
        )
    }

    /// Give a just-recorded clip the format its recorder wrote, so its source
    /// window is real frames from the start instead of waiting for a waveform
    /// import that may never run ("Generate waveform after record" off).
    /// Until then it had no rate, trims before a decode fell back to 1 Hz, and
    /// a reopen reset its window. Its length follows the tempo map and stretch
    /// ratio like every decoded clip. Returns `false` if nothing was seeded.
    pub fn seed_recorded_clip_source(
        &mut self,
        clip_id: &str,
        sample_rate: u32,
        duration_seconds: f64,
    ) -> bool {
        if sample_rate == 0 || !(duration_seconds > 0.0) {
            return false;
        }
        let frames = (duration_seconds * sample_rate as f64).round() as u64;
        let Some(clip) = self.recorded_clip_mut(clip_id) else {
            return false;
        };
        clip.stretch.original_sample_rate = sample_rate;
        clip.stretch.project_sample_rate = sample_rate;
        clip.stretch.original_duration_samples = frames;
        clip.stretch.source_start_samples = 0;
        clip.stretch.source_end_samples = frames;
        let length = self.find_clip(clip_id).and_then(|(_, clip)| {
            Some((self.audio_clip_end_beat(clip)? - clip.start_beat as f64) as f32)
        });
        if let (Some(length), Some(clip)) = (length, self.recorded_clip_mut(clip_id)) {
            clip.duration_beats = length;
        }
        true
    }

    fn recorded_clip_mut(&mut self, clip_id: &str) -> Option<&mut ClipState> {
        self.tracks
            .iter_mut()
            .flat_map(|track| track.clips.iter_mut())
            .find(|clip| clip.id == clip_id)
    }

    // ── Realtime recording preview clip (Part 1) ─────────────────────────
    //
    // A temporary, UI-only clip drawn while a take is recording. It has no
    // source path so it is never sent to the engine or persisted; the
    // arrangement renderer lays it out like any clip, and `waveform_canvas`
    // draws its streamed peaks from the recording-preview registry.

    /// Create (or replace) the live recording preview clip on `track_id`.
    pub fn begin_recording_preview_clip(&mut self, clip_id: &str, track_id: &str, start_beat: f32) {
        self.remove_recording_preview_clip(clip_id);
        let clip = ClipState {
            id: clip_id.to_string(),
            name: "Recording…".to_string(),
            start_beat: start_beat.max(0.0),
            duration_beats: 0.01,
            source_duration_seconds: None,
            offset_beats: 0.0,
            gain: 1.0,
            clip_type: ClipType::Audio {
                file_id: String::new(),
                source_path: None,
            },
            muted: false,
            audio_import: AudioImportState::Pending,
            stretch: AudioClipStretchState::default(),
        };
        if let Some(track) = self.tracks.iter_mut().find(|t| t.id == track_id) {
            track.clips.push(clip);
        }
    }

    /// Create an in-memory MIDI take that is visible while recording. Like the
    /// audio recording preview, this is never persisted or sent to the engine;
    /// it is replaced by the committed take when recording stops.
    pub fn begin_midi_recording_preview_clip(
        &mut self,
        clip_id: &str,
        track_id: &str,
        start_beat: f32,
    ) {
        self.remove_recording_preview_clip(clip_id);
        let clip = ClipState {
            id: clip_id.to_string(),
            name: "MIDI Recording…".to_string(),
            start_beat: start_beat.max(0.0),
            duration_beats: MIN_NOTE_BEATS,
            source_duration_seconds: None,
            offset_beats: 0.0,
            gain: 1.0,
            clip_type: ClipType::Midi {
                notes: Vec::new(),
                controller_lanes: Vec::new(),
                sysex_events: Vec::new(),
                articulations: Vec::new(),
            },
            muted: false,
            audio_import: AudioImportState::default(),
            stretch: AudioClipStretchState::default(),
        };
        if let Some(track) = self.tracks.iter_mut().find(|track| track.id == track_id) {
            track.clips.push(clip);
        }
    }

    /// Replace the visible MIDI take snapshot as notes arrive. The caller owns
    /// the mutable recording take; this method only mirrors it for rendering.
    pub fn update_midi_recording_preview_clip(
        &mut self,
        clip_id: &str,
        duration_beats: f32,
        notes: Vec<MidiNoteState>,
    ) -> bool {
        for track in &mut self.tracks {
            if let Some(clip) = track.clips.iter_mut().find(|clip| clip.id == clip_id) {
                let next_duration = duration_beats.max(MIN_NOTE_BEATS);
                let mut changed = (clip.duration_beats - next_duration).abs() > f32::EPSILON;
                clip.duration_beats = next_duration;
                if let ClipType::Midi {
                    notes: preview_notes,
                    ..
                } = &mut clip.clip_type
                {
                    if *preview_notes != notes {
                        *preview_notes = notes;
                        changed = true;
                    }
                }
                return changed;
            }
        }
        false
    }

    /// Grow the preview clip as recording proceeds. Returns `true` if changed.
    pub fn set_recording_preview_clip_length(
        &mut self,
        clip_id: &str,
        duration_beats: f32,
    ) -> bool {
        let next = duration_beats.max(0.01);
        for track in &mut self.tracks {
            if let Some(c) = track.clips.iter_mut().find(|c| c.id == clip_id) {
                if (c.duration_beats - next).abs() > f32::EPSILON {
                    c.duration_beats = next;
                    return true;
                }
                return false;
            }
        }
        false
    }

    /// Remove the preview clip (take finished / cancelled). Returns `true` if
    /// a clip was removed.
    pub fn remove_recording_preview_clip(&mut self, clip_id: &str) -> bool {
        let mut removed = false;
        for track in &mut self.tracks {
            let before = track.clips.len();
            track.clips.retain(|c| c.id != clip_id);
            removed |= track.clips.len() != before;
        }
        if removed {
            self.selection.selected_clip_ids.retain(|id| id != clip_id);
        }
        removed
    }
}

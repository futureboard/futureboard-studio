use super::*;

#[derive(Debug, Clone, PartialEq)]
pub enum MidiSysExKind {
    Normal,
    Escaped,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MidiSysExEvent {
    pub kind: MidiSysExKind,
    pub tick: u64,
    pub beat: f32,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ClipType {
    Audio {
        file_id: String,
        /// Absolute path to the decoded source file, if this clip was created
        /// by importing a real audio file. Used as the waveform cache key.
        source_path: Option<String>,
    },
    Midi {
        notes: Vec<MidiNoteState>,
        /// MIDI controller (CC / pitch-bend / pressure) lanes for this clip.
        controller_lanes: Vec<MidiControllerLane>,
        /// Imported SysEx payloads preserved for future playback/export support.
        ///
        /// TODO(midi-export/playback): carry these through the engine/export
        /// snapshot once Futureboard has an explicit SysEx scheduling path.
        sysex_events: Vec<MidiSysExEvent>,
        /// Direction articulation events (timeline-based; active until the
        /// next event). Per-note articulations live on [`MidiNoteState`].
        articulations: Vec<MidiArticulationEvent>,
    },
    /// A reference video placed on the Video track. Carries no samples and no
    /// notes — the clip only says *which* file is on screen and, through
    /// `start_beat`/`offset_beats`, *where in it* the playhead is. Frames are
    /// decoded on demand by the Video Player window, never stored here.
    Video {
        /// Project asset id, shared with the video import bookkeeping.
        file_id: String,
        /// Absolute path to the source file. `None` while the clip is a
        /// placeholder whose media has not been resolved yet.
        source_path: Option<String>,
    },
}

/// Background import/decode state for a real audio file (waveform + engine).
#[derive(Debug, Clone, PartialEq)]
pub enum AudioImportState {
    Pending,
    Probing,
    Decoding { progress: f32 },
    GeneratingPeaks { progress: f32 },
    Ready,
    Failed { message: String },
}

impl Default for AudioImportState {
    fn default() -> Self {
        Self::Pending
    }
}

/// An ARA plug-in attached to a track.
///
/// ARA is a track-level processor, the way an insert is: the plug-in takes the
/// whole track and every audio clip on it becomes one of its playback regions.
/// This identifies the plug-in well enough to re-instantiate it on load; its
/// edits are not here — they live in the ARA document archive stored per
/// (plug-in, track) on the project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AraTrackBinding {
    /// Catalog id of the ARA plug-in (`RegistryPlugin::id`).
    pub plugin_id: String,
    /// VST3 module path, so the plug-in can be re-instantiated on load.
    pub plugin_path: String,
    /// VST3 audio-module class id inside that module.
    pub class_id: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClipState {
    pub id: String,
    pub name: String,
    pub start_beat: f32,
    pub duration_beats: f32,
    pub source_duration_seconds: Option<f64>,
    pub offset_beats: f32,
    pub gain: f32,
    pub clip_type: ClipType,
    pub muted: bool,
    /// Populated for imported audio clips; drives clip chrome + waveform UI.
    pub audio_import: AudioImportState,
    /// Non-destructive clip-level time-stretch / pitch state. Present on every
    /// clip; only meaningful for audio clips (MIDI clips carry a default `Off`
    /// instance). See [`AudioClipStretchState`].
    pub stretch: AudioClipStretchState,
}

impl ClipState {
    /// Stable key for an imported audio clip's waveform peaks and import state.
    ///
    /// Keyed on the asset id (`file_id`), **not** the on-disk path, so the
    /// waveform binding survives a later change of `source_path` (e.g. copying
    /// the source into the project folder). Returns `None` for clips with no
    /// real source (placeholder / live-recording preview).
    pub fn audio_asset_key(&self) -> Option<&str> {
        match &self.clip_type {
            ClipType::Audio {
                file_id,
                source_path: Some(_),
            } if !file_id.is_empty() => Some(file_id.as_str()),
            _ => None,
        }
    }

    /// Source frames this audio clip consumes per second of timeline, at a
    /// constant project tempo of `60 / seconds_per_beat`. Off clips read 1:1;
    /// a clip stretched to twice its length reads half as fast.
    fn source_frames_per_timeline_second(&self, seconds_per_beat: f32) -> f64 {
        let rate = self.stretch.source_sample_rate().max(1) as f64;
        let project_bpm = 60.0 / seconds_per_beat.max(f32::EPSILON) as f64;
        let ratio = self.stretch.effective_time_ratio(project_bpm);
        let ratio = if ratio.is_finite() && ratio > 1.0e-6 {
            ratio
        } else {
            1.0
        };
        rate / ratio
    }

    /// Whether an edge trim can move this clip's source window. Warp clips pin
    /// source samples to beats with markers, so their window is owned by the
    /// warp editor, not by edge trims.
    fn audio_trim_follows_length(&self) -> bool {
        matches!(self.clip_type, ClipType::Audio { .. })
            && !matches!(self.stretch.mode, StretchMode::Warp)
    }

    /// Reconcile an audio clip's source trim window with its current
    /// `duration_beats`, using right-edge trim semantics: `source_start_samples`
    /// stays fixed and `source_end_samples` follows the clip length, clamped to
    /// the available media. When the media length is known, `duration_beats` is
    /// snapped back to the clamped source window so the waveform's source→pixel
    /// scale is preserved — the waveform crops/reveals, it never stretches
    /// (spec §4/§5).
    ///
    /// A stretched clip trims the same way through its ratio: the window moves
    /// by `length / ratio`, so a trim never changes how fast the clip plays.
    /// No-op for MIDI and Warp clips.
    ///
    /// This is the single source of truth shared by right-edge drag resize
    /// ([`TimelineState::resize_clip_with_bypass`]) and the inspector Length
    /// field ([`TimelineState::set_clip_length`]), so both trim identically.
    pub(crate) fn reconcile_audio_trim_to_length(
        &mut self,
        seconds_per_beat: f32,
        min_len_beats: f32,
    ) {
        if !self.audio_trim_follows_length() {
            return;
        }
        let frames_per_second = self.source_frames_per_timeline_second(seconds_per_beat);
        let source_start = self.stretch.source_start_samples;
        let source_end = source_start.saturating_add(
            ((self.duration_beats as f64 * seconds_per_beat as f64) * frames_per_second)
                .round()
                .max(0.0) as u64,
        );
        self.stretch.apply_trim(source_start, source_end);
        if self.stretch.original_duration_samples > 0 {
            self.stretch.source_end_samples = self
                .stretch
                .source_end_samples
                .min(self.stretch.original_duration_samples);
            let source_len = self.stretch.source_end_samples.saturating_sub(source_start);
            self.duration_beats = ((source_len as f64 / frames_per_second)
                / seconds_per_beat as f64)
                .max(min_len_beats as f64) as f32;
        }
    }
}

/// Shortest an audio clip may become. Below this the two edge handles overlap
/// and the clip stops being a usable target.
pub const MIN_AUDIO_CLIP_BEATS: f32 = 0.25;

/// Which edge of a clip an edge-resize gesture is dragging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipEdge {
    Left,
    Right,
}

/// Highest numeric suffix among existing `clip-N` ids, plus one. Exposed so
/// callers that mint several clip ids in a row (e.g. multi-track MIDI import)
/// can reserve a run of ids locally instead of re-scanning `tracks` — clips
/// built but not yet attached to a track are invisible to that scan.
pub fn next_clip_id_number(tracks: &[TrackState]) -> u32 {
    let mut n = 0u32;
    for t in tracks {
        for c in &t.clips {
            if let Some(rest) = c.id.strip_prefix("clip-") {
                if let Ok(v) = rest.parse::<u32>() {
                    if v > n {
                        n = v;
                    }
                }
            }
        }
    }
    n + 1
}

/// How many beats `seconds` of played audio covers for `clip`, starting where
/// the clip starts.
///
/// This is the whole of "a tempo change must not move the audio". A clip that
/// is *not* locked to the project tempo plays for a fixed wall-clock length, so
/// its beat span is whatever that length covers under the tempo map from its
/// own start — a ramp inside the clip shortens the bar count without touching a
/// sample of audio. A clip that *is* locked (Tempo Sync / Warp) is defined in
/// beats instead: its length already folds the tempo in through the stretch
/// ratio, so it converts at the project tempo and keeps its bar count.
fn audio_clip_beats_for_seconds(
    clip: &ClipState,
    seconds: f64,
    tempo: &DirectAudio::TempoMap,
    project_bpm: f64,
) -> f64 {
    if clip.stretch.follows_project_tempo() {
        return seconds * project_bpm.max(1.0) / 60.0;
    }
    let start_beat = clip.start_beat.max(0.0) as f64;
    let start_seconds = tempo.seconds_at_beat(start_beat);
    (tempo.beat_at_seconds(start_seconds + seconds.max(0.0)) - start_beat).max(0.0)
}

impl TimelineState {
    /// Re-derive every audio clip's musical length from the audio it actually
    /// plays, at the current project tempo. Returns `true` when anything moved.
    ///
    /// `duration_beats` is the coordinate the whole arrangement is built on —
    /// hit-testing, snapping, neighbour layout and the engine snapshot all read
    /// it — while an audio clip is *drawn* from its source window in seconds.
    /// Those two agree right after a trim and diverge the moment the tempo
    /// moves, which is how a clip ends up drawn one length and grabbable
    /// another.
    ///
    /// Deriving rather than storing also settles what a tempo change means for
    /// each stretch mode with no branch at all:
    /// `effective_duration_samples_for_project_bpm` already folds the mode's
    /// ratio in, so a Tempo Sync clip keeps its bar count (its wall-clock
    /// length moves with the tempo) while an Off / Manual / Resample clip keeps
    /// its wall-clock length (its bar count moves). It is idempotent, so undo
    /// restores the lengths by re-running it instead of carrying a second
    /// snapshot alongside the tempo one.
    ///
    /// Clips whose source window has not been decoded yet are left alone: there
    /// is nothing authoritative to derive from, and guessing would shrink a
    /// pending import to the minimum length.
    pub fn reconcile_audio_clip_lengths(&mut self) -> bool {
        // Every path that changes the tempo or the BPM ends here, which makes
        // this the natural place to bring the resolved map back up to date.
        self.refresh_tempo_cache();
        let project_bpm = self.bpm.max(1.0) as f64;
        // One map for the whole pass, and the same map the transport plays.
        let tempo = self.resolved_tempo_map();
        let mut changed = false;
        for track in &mut self.tracks {
            for clip in &mut track.clips {
                if !matches!(clip.clip_type, ClipType::Audio { .. }) {
                    continue;
                }
                let Some(seconds) = clip.stretch.played_seconds_for_project_bpm(project_bpm) else {
                    continue;
                };
                let beats = audio_clip_beats_for_seconds(clip, seconds, &tempo, project_bpm) as f32;
                let beats = beats.max(MIN_AUDIO_CLIP_BEATS);
                if (clip.duration_beats - beats).abs() > 1.0e-4 {
                    clip.duration_beats = beats;
                    changed = true;
                }
            }
        }
        changed
    }

    /// Beat the audio in `clip` finishes on, derived from what it actually
    /// plays. `None` while the source window is still undecoded — there is
    /// nothing authoritative to derive from yet.
    ///
    /// The single answer shared by the model ([`Self::reconcile_audio_clip_lengths`])
    /// and by the drawing, so a clip is never grabbable at one length and
    /// painted at another.
    pub fn audio_clip_end_beat(&self, clip: &ClipState) -> Option<f64> {
        let project_bpm = self.bpm.max(1.0) as f64;
        let seconds = clip.stretch.played_seconds_for_project_bpm(project_bpm)?;
        // The cached map: this runs once per audio clip per frame, and
        // rebuilding it here made the arrangement's frame cost scale with the
        // project's tempo-marker count.
        let tempo = self.resolved_tempo_map();
        let beats = audio_clip_beats_for_seconds(clip, seconds, &tempo, project_bpm);
        Some(clip.start_beat as f64 + beats.max(MIN_AUDIO_CLIP_BEATS as f64))
    }

    pub fn next_clip_id(&self) -> String {
        format!("clip-{}", next_clip_id_number(&self.tracks))
    }

    /// Deterministic id one past `id`, used when a single operation inserts two
    /// fresh clips (e.g. a split) before either is committed, so [`next_clip_id`]
    /// alone would hand back the same number twice.
    pub fn next_clip_id_after(&self, id: &str) -> String {
        id.strip_prefix("clip-")
            .and_then(|rest| rest.parse::<u32>().ok())
            .map(|n| format!("clip-{}", n + 1))
            .unwrap_or_else(|| format!("{id}-split"))
    }

    /// Minimum length (beats) either side of an audio-clip split must keep. A
    /// hair-thin fragment is never useful and rounds toward a zero-length clip.
    pub const MIN_CLIP_SPLIT_BEATS: f32 = 0.25;

    /// Build the two abutting clips a split of `clip` at `split_beat` (absolute
    /// timeline beats) would produce: `(left, right)`. Pure — the caller records
    /// the undoable edit. `None` when `clip` is not audio or `split_beat` lands
    /// within [`MIN_CLIP_SPLIT_BEATS`] of either edge.
    ///
    /// The two clips receive abutting source windows so playback, waveform
    /// rendering, and later edge trims all agree on the actual cut point.
    pub fn plan_audio_clip_split(
        &self,
        clip: &ClipState,
        split_beat: f32,
    ) -> Option<(ClipState, ClipState)> {
        if !matches!(clip.clip_type, ClipType::Audio { .. }) {
            return None;
        }
        let clip_start = clip.start_beat;
        let clip_end = clip.start_beat + clip.duration_beats;
        if split_beat <= clip_start + Self::MIN_CLIP_SPLIT_BEATS
            || split_beat >= clip_end - Self::MIN_CLIP_SPLIT_BEATS
        {
            return None;
        }

        let left_len = split_beat - clip_start;
        let right_len = clip_end - split_beat;
        let left_id = self.next_clip_id();
        let right_id = self.next_clip_id_after(&left_id);

        let mut left = self.clone_clip_for_insert(clip, left_id, clip.name.clone(), clip_start);
        left.duration_beats = left_len;

        let mut right =
            self.clone_clip_for_insert(clip, right_id, format!("{} Split", clip.name), split_beat);
        right.duration_beats = right_len;
        right.offset_beats = clip.offset_beats + left_len;
        if matches!(clip.clip_type, ClipType::Audio { .. }) {
            // Split the source window where the audio under the split point
            // lives. Through the stretch ratio, so a stretched clip's halves
            // keep playing at the same speed instead of each re-deriving the
            // whole take's length.
            // The split point's time into the clip comes through the tempo map,
            // the axis the waveform is drawn on, so under tempo automation the
            // halves meet on the audio under the razor line.
            let frames_per_second = clip.source_frames_per_timeline_second(self.seconds_per_beat());
            let split_seconds = self.clip_local_seconds_at_beat(clip, split_beat as f64);
            let split_samples = (split_seconds * frames_per_second).round().max(0.0) as u64;
            let source_start = clip.stretch.source_start_samples;
            let source_end = if clip.stretch.source_end_samples > source_start {
                clip.stretch.source_end_samples
            } else {
                clip.stretch.original_duration_samples
            };
            if source_end > source_start {
                let source_split = source_start.saturating_add(split_samples).min(source_end);
                left.stretch.apply_trim(source_start, source_split);
                right.stretch.apply_trim(source_split, source_end);
                // Each half keeps only the warp markers that fall inside it.
                let split_at = split_beat as f64;
                left.stretch
                    .warp_markers
                    .retain(|marker| marker.timeline_beat < split_at);
                right
                    .stretch
                    .warp_markers
                    .retain(|marker| marker.timeline_beat > split_at);
            }
            // The outer fades stay where they were; the cut itself gets none.
            // Copying them put a fade-out and a fade-in — a dip — on every cut.
            left.stretch.fade_out_ms = 0.0;
            right.stretch.fade_in_ms = 0.0;
        }

        Some((left, right))
    }

    /// Length of a clip in beats, if it exists.
    pub fn clip_duration_beats(&self, clip_id: &str) -> Option<f32> {
        for track in &self.tracks {
            if let Some(clip) = track.clips.iter().find(|c| c.id == clip_id) {
                return Some(clip.duration_beats);
            }
        }
        None
    }

    /// Clips intersecting a beat range on any track.
    pub fn clips_intersecting_beats(&self, start: f32, end: f32) -> Vec<String> {
        let (lo, hi) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        let mut ids = Vec::new();
        for track in &self.tracks {
            for clip in &track.clips {
                let clip_end = clip.start_beat + clip.duration_beats;
                if clip.start_beat < hi && clip_end > lo {
                    ids.push(clip.id.clone());
                }
            }
        }
        ids
    }

    pub fn rename_clip(&mut self, clip_id: &str, name: &str) -> bool {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return false;
        }
        for track in &mut self.tracks {
            if let Some(clip) = track.clips.iter_mut().find(|clip| clip.id == clip_id) {
                if clip.name != trimmed {
                    clip.name = trimmed.to_string();
                    return true;
                }
                return false;
            }
        }
        false
    }

    pub fn set_clip_start(&mut self, clip_id: &str, start_beat: f32) -> bool {
        let start_beat = start_beat.max(0.0);
        for track in &mut self.tracks {
            if let Some(clip) = track.clips.iter_mut().find(|clip| clip.id == clip_id) {
                if (clip.start_beat - start_beat).abs() > 0.0001 {
                    clip.start_beat = start_beat;
                    return true;
                }
                return false;
            }
        }
        false
    }

    /// Raw duration setter. This is a low-level primitive shared by the stretch
    /// command machinery (which sets a *stretched* length and owns the source↔
    /// length coupling itself) and undo/redo, so it deliberately does **not**
    /// touch the audio source trim window. UI that means "trim the clip" must use
    /// [`Self::set_clip_length_trimming`] instead.
    pub fn set_clip_length(&mut self, clip_id: &str, duration_beats: f32) -> bool {
        for track in &mut self.tracks {
            if let Some(clip) = track.clips.iter_mut().find(|clip| clip.id == clip_id) {
                let min_len = match &clip.clip_type {
                    ClipType::Midi { notes, .. } => {
                        let last_note_end = notes
                            .iter()
                            .map(|note| note.start.max(0.0) + note.duration.max(MIN_NOTE_BEATS))
                            .fold(0.0_f32, f32::max);
                        MIN_MIDI_CLIP_BEATS.max(last_note_end)
                    }
                    ClipType::Audio { .. } | ClipType::Video { .. } => 0.25,
                };
                let duration_beats = duration_beats.max(min_len);
                if (clip.duration_beats - duration_beats).abs() > 0.0001 {
                    clip.duration_beats = duration_beats;
                    return true;
                }
                return false;
            }
        }
        false
    }

    /// User-facing "set clip length" (inspector Length field). Behaves like a
    /// right-edge resize: for an audio clip it reconciles the source trim
    /// window (through the stretch ratio) so the waveform crops/reveals instead
    /// of stretching to fit the new width (spec §4/§5). MIDI and Warp clips fall
    /// back to the raw [`Self::set_clip_length`] behaviour. Returns `true` when
    /// anything changed. UI-mutating only — the caller records undo / marks dirty.
    pub fn set_clip_length_trimming(&mut self, clip_id: &str, duration_beats: f32) -> bool {
        let seconds_per_beat = self.seconds_per_beat();
        let project_rate = self.project_sample_rate;
        for track in &mut self.tracks {
            if let Some(clip) = track.clips.iter_mut().find(|clip| clip.id == clip_id) {
                clip.assume_project_rate_until_decoded(project_rate);
                let min_len = match &clip.clip_type {
                    ClipType::Midi { notes, .. } => {
                        let last_note_end = notes
                            .iter()
                            .map(|note| note.start.max(0.0) + note.duration.max(MIN_NOTE_BEATS))
                            .fold(0.0_f32, f32::max);
                        MIN_MIDI_CLIP_BEATS.max(last_note_end)
                    }
                    ClipType::Audio { .. } | ClipType::Video { .. } => 0.25,
                };
                let duration_beats = duration_beats.max(min_len);
                let before = (
                    clip.duration_beats,
                    clip.stretch.source_start_samples,
                    clip.stretch.source_end_samples,
                );
                clip.duration_beats = duration_beats;
                clip.reconcile_audio_trim_to_length(seconds_per_beat, min_len);
                return (before.0 - clip.duration_beats).abs() > 0.0001
                    || before.1 != clip.stretch.source_start_samples
                    || before.2 != clip.stretch.source_end_samples;
            }
        }
        false
    }

    pub fn set_clip_muted(&mut self, clip_id: &str, muted: bool) -> bool {
        for track in &mut self.tracks {
            if let Some(clip) = track.clips.iter_mut().find(|clip| clip.id == clip_id) {
                if clip.muted != muted {
                    clip.muted = muted;
                    return true;
                }
                return false;
            }
        }
        false
    }

    pub fn set_clip_gain(&mut self, clip_id: &str, gain: f32) -> bool {
        let gain = gain.clamp(0.0, 4.0);
        for track in &mut self.tracks {
            if let Some(clip) = track.clips.iter_mut().find(|clip| clip.id == clip_id) {
                if (clip.gain - gain).abs() > 0.0001 {
                    clip.gain = gain;
                    return true;
                }
                return false;
            }
        }
        false
    }

    /// Read-only access to a clip's non-destructive stretch/pitch state.
    pub fn clip_stretch(&self, clip_id: &str) -> Option<&AudioClipStretchState> {
        for track in &self.tracks {
            if let Some(clip) = track.clips.iter().find(|clip| clip.id == clip_id) {
                return Some(&clip.stretch);
            }
        }
        None
    }

    /// Replace a clip's stretch/pitch state. Returns `true` when it changed.
    /// UI-mutating only — the caller marks the project dirty / records undo.
    pub fn set_clip_stretch(&mut self, clip_id: &str, stretch: AudioClipStretchState) -> bool {
        let mut stretch = stretch;
        stretch.sanitize_in_place();
        for track in &mut self.tracks {
            if let Some(clip) = track.clips.iter_mut().find(|clip| clip.id == clip_id) {
                if clip.stretch != stretch {
                    clip.stretch = stretch;
                    return true;
                }
                return false;
            }
        }
        false
    }

    pub fn find_clip(&self, clip_id: &str) -> Option<(&TrackState, &ClipState)> {
        for t in &self.tracks {
            if let Some(c) = t.clips.iter().find(|c| c.id == clip_id) {
                return Some((t, c));
            }
        }
        None
    }

    pub fn delete_clip(&mut self, clip_id: &str) {
        for track in &mut self.tracks {
            if let Some(index) = track.clips.iter().position(|clip| clip.id == clip_id) {
                track.clips.remove(index);
                self.selection.selected_clip_ids.retain(|id| id != clip_id);
                self.selection.selected_track_id = Some(track.id.clone());
                // A take is a pointer to a clip, and this clip has gone. A take
                // row pointing at nothing is a control that does nothing.
                track.takes.retain(|take| take.clip_id != clip_id);
                if track.takes.is_empty() {
                    track.takes_expanded = false;
                }
                return;
            }
        }
    }

    pub fn duplicate_clip(&mut self, clip_id: &str) {
        let Some((track_id, duplicate)) = self.build_clip_duplicate_after(clip_id) else {
            return;
        };
        let duplicate_id = duplicate.id.clone();
        if let Some(track) = self.tracks.iter_mut().find(|track| track.id == track_id) {
            if let Some(index) = track.clips.iter().position(|clip| clip.id == clip_id) {
                track.clips.insert(index + 1, duplicate);
            } else {
                track.clips.push(duplicate);
            }
            self.selection.selected_track_id = Some(track.id.clone());
            self.selection.selected_clip_ids = vec![duplicate_id];
        }
    }

    pub fn build_clip_duplicate_after(&self, clip_id: &str) -> Option<(String, ClipState)> {
        for track in &self.tracks {
            if let Some(clip) = track.clips.iter().find(|clip| clip.id == clip_id) {
                let raw_start = clip.start_beat + clip.duration_beats;
                let start_beat = self.snap_beats(raw_start).max(0.0);
                let duplicate = self.clone_clip_for_insert(
                    clip,
                    self.next_clip_id(),
                    format!("{} Copy", clip.name),
                    start_beat,
                );
                return Some((track.id.clone(), duplicate));
            }
        }
        None
    }

    pub fn clone_clip_for_insert(
        &self,
        clip: &ClipState,
        id: String,
        name: String,
        start_beat: f32,
    ) -> ClipState {
        let mut cloned = clip.clone();
        cloned.id = id;
        cloned.name = name;
        cloned.start_beat = start_beat.max(0.0);
        cloned.clip_type = match &clip.clip_type {
            ClipType::Audio {
                file_id,
                source_path,
            } => ClipType::Audio {
                file_id: file_id.clone(),
                source_path: source_path.clone(),
            },
            ClipType::Midi {
                notes,
                controller_lanes,
                sysex_events,
                articulations,
            } => ClipType::Midi {
                notes: notes
                    .iter()
                    .map(|note| {
                        let mut cloned = MidiNoteState::new(
                            note.pitch,
                            note.start,
                            note.duration,
                            note.velocity,
                        );
                        cloned.release_velocity = note.release_velocity;
                        cloned.muted = note.muted;
                        cloned.channel = note.channel;
                        cloned.articulation = note.articulation;
                        // Per-note pitch performance is part of the note, not
                        // decoration on it: duplicating, alt-dragging or pasting
                        // a clip must carry the drawn curve with the notes or
                        // the copy silently plays a different phrase. Fresh
                        // point ids, so editing the copy cannot disturb the
                        // original's selection or undo entries.
                        cloned.pitch_curve = note
                            .pitch_curve
                            .as_ref()
                            .map(super::PitchCurve::cloned_with_new_ids);
                        cloned.expression = note.expression.clone();
                        cloned
                    })
                    .collect(),
                controller_lanes: controller_lanes
                    .iter()
                    .map(|lane| MidiControllerLane {
                        kind: lane.kind,
                        points: lane
                            .points
                            .iter()
                            .map(|point| MidiControllerPoint::new(point.beat, point.value))
                            .collect(),
                        visible: lane.visible,
                        height: lane.height,
                        collapsed: lane.collapsed,
                    })
                    .collect(),
                sysex_events: sysex_events.clone(),
                // Fresh transient ids for the duplicate — never reuse source ids.
                articulations: articulations
                    .iter()
                    .map(|event| MidiArticulationEvent::new(event.beat, event.articulation))
                    .collect(),
            },
            // A duplicated video clip references the same media; only its
            // placement differs.
            ClipType::Video {
                file_id,
                source_path,
            } => ClipType::Video {
                file_id: file_id.clone(),
                source_path: source_path.clone(),
            },
        };
        cloned
    }

    pub fn move_clip_to_track(&mut self, clip_id: &str, target_track_id: &str, start_beat: f32) {
        self.move_clip_to_track_with_options(clip_id, target_track_id, start_beat, true)
    }

    /// Move a clip. When `snap` is false the provided `start_beat` is used as-is
    /// (Shift-bypass and multi-selection peers that already share one anchor delta).
    pub fn move_clip_to_track_with_options(
        &mut self,
        clip_id: &str,
        target_track_id: &str,
        start_beat: f32,
        snap: bool,
    ) {
        let start_beat = if snap {
            self.snap_beats(start_beat).max(0.0)
        } else {
            start_beat.max(0.0)
        };
        let mut moved_clip = None;
        let mut source_track_id = None;

        for track in &mut self.tracks {
            if let Some(index) = track.clips.iter().position(|clip| clip.id == clip_id) {
                let mut clip = track.clips.remove(index);
                clip.start_beat = start_beat;
                moved_clip = Some(clip);
                source_track_id = Some(track.id.clone());
                break;
            }
        }

        let Some(clip) = moved_clip else {
            return;
        };

        let target_id = if self.tracks.iter().any(|track| track.id == target_track_id) {
            target_track_id.to_string()
        } else {
            source_track_id.unwrap_or_else(|| target_track_id.to_string())
        };

        if let Some(track) = self.tracks.iter_mut().find(|track| track.id == target_id) {
            track.clips.push(clip);
            self.selection.selected_track_id = Some(track.id.clone());
            self.selection.selected_clip_ids = vec![clip_id.to_string()];
        }
    }

    /// Stretch an audio clip by dragging one edge to `new_edge_beat` — the
    /// Stretch tool's gesture. The source window stays exactly as it is and
    /// the stretch ratio changes so that window fills the new length; the
    /// opposite edge stays fixed.
    ///
    /// A clip that was not already speed-stretched becomes a Speed clip and
    /// keeps its pitch choice (an `Off` or Tempo clip keeps pitch). A Warp clip
    /// stays Warp and its markers scale with it, so they stay on the audio
    /// they were placed on.
    ///
    /// UI-mutating only — the caller records the undo step on drop. Returns
    /// `true` when the gesture applies (an audio clip with a decoded source
    /// window), whether or not this particular move changed anything; `false`
    /// tells the caller to fall back to an ordinary edge resize.
    pub fn stretch_clip_edge(&mut self, clip_id: &str, edge: ClipEdge, new_edge_beat: f32) -> bool {
        let seconds_per_beat = self.seconds_per_beat();
        let project_bpm = self.bpm.max(1.0) as f64;
        let Some(clip) = self
            .tracks
            .iter_mut()
            .flat_map(|track| track.clips.iter_mut())
            .find(|clip| clip.id == clip_id)
        else {
            return false;
        };
        if !matches!(clip.clip_type, ClipType::Audio { .. }) {
            return false;
        }
        let rate = clip.stretch.source_sample_rate();
        let source_len = clip.stretch.source_len_samples();
        if rate == 0 || source_len == 0 {
            return false;
        }
        let old_start = clip.start_beat;
        let old_len = clip.duration_beats.max(MIN_AUDIO_CLIP_BEATS);
        let old_end = old_start + old_len;
        let requested_len = match edge {
            ClipEdge::Right => new_edge_beat - old_start,
            ClipEdge::Left => old_end - new_edge_beat.max(0.0),
        }
        .max(MIN_AUDIO_CLIP_BEATS);

        let source_seconds = source_len as f64 / rate as f64;
        let requested_ratio = requested_len as f64 * seconds_per_beat as f64 / source_seconds;
        let mut next = match clip.stretch.timing() {
            StretchTiming::Speed | StretchTiming::Warp => clip.stretch.clone(),
            StretchTiming::Off | StretchTiming::Tempo => {
                clip.stretch.with_timing(StretchTiming::Speed, project_bpm)
            }
        };
        next.set_stretch_ratio(requested_ratio);
        next.clip_timeline_duration_beats = 0.0;
        // The ratio is clamped, so derive the length from what was accepted.
        let new_len = (source_seconds * next.stretch_ratio / seconds_per_beat as f64)
            .max(MIN_AUDIO_CLIP_BEATS as f64) as f32;
        let new_start = match edge {
            ClipEdge::Right => old_start,
            ClipEdge::Left => (old_end - new_len).max(0.0),
        };
        if next.mode == StretchMode::Warp {
            let (old_start, old_len) = (old_start as f64, old_len as f64);
            let (new_start_f, new_len_f) = (new_start as f64, new_len as f64);
            for marker in &mut next.warp_markers {
                let t = (marker.timeline_beat - old_start) / old_len.max(f64::EPSILON);
                marker.timeline_beat = (new_start_f + t * new_len_f).max(0.0);
            }
        }
        clip.stretch = next;
        clip.start_beat = new_start;
        clip.duration_beats = new_len;
        true
    }

    /// Resize a clip by dragging one edge to `new_edge_beat` (absolute beats).
    /// Audio clips are intentionally not snapped here: normal edge resize is a
    /// source trim, not a musical quantize/stretch operation. MIDI clips keep
    /// grid snapping because note boundaries are beat-authored. The opposite
    /// edge stays fixed. Enforces a minimum length
    /// and, for MIDI clips, never shrinks below the last note end. Left-edge
    /// resizes re-offset clip-local notes so they keep their absolute position,
    /// clamping so the earliest note never crosses clip-local beat 0.
    ///
    /// UI-mutating only — the caller marks the project dirty once on commit.
    /// Returns `true` when a matching clip was found.
    pub fn resize_clip(&mut self, clip_id: &str, edge: ClipEdge, new_edge_beat: f32) -> bool {
        self.resize_clip_with_bypass(clip_id, edge, new_edge_beat, false)
    }

    /// Variant used by active pointer gestures. Shift bypass applies only to
    /// MIDI edge snapping; audio source trimming remains intentionally free.
    pub fn resize_clip_with_bypass(
        &mut self,
        clip_id: &str,
        edge: ClipEdge,
        new_edge_beat: f32,
        bypass_snap: bool,
    ) -> bool {
        let is_audio_clip = self
            .find_clip(clip_id)
            .is_some_and(|(_, clip)| matches!(clip.clip_type, ClipType::Audio { .. }));
        let edge_beat = if is_audio_clip {
            new_edge_beat.max(0.0)
        } else {
            self.snap_beats_with_bypass(new_edge_beat, bypass_snap)
                .max(0.0)
        };
        let seconds_per_beat = self.seconds_per_beat();
        let project_rate = self.project_sample_rate;
        let Some(track) = self
            .tracks
            .iter_mut()
            .find(|t| t.clips.iter().any(|c| c.id == clip_id))
        else {
            return false;
        };
        let Some(clip) = track.clips.iter_mut().find(|c| c.id == clip_id) else {
            return false;
        };
        // No 1 Hz trim math for a clip whose file is not decoded yet.
        clip.assume_project_rate_until_decoded(project_rate);

        let is_midi = matches!(clip.clip_type, ClipType::Midi { .. });
        let min_len = if is_midi {
            MIN_MIDI_CLIP_BEATS
        } else {
            MIN_AUDIO_CLIP_BEATS
        };
        // Clip-local end of the furthest note — the floor for any MIDI shrink.
        let last_note_end = if let ClipType::Midi { notes, .. } = &clip.clip_type {
            notes
                .iter()
                .map(|n| n.start.max(0.0) + n.duration.max(MIN_NOTE_BEATS))
                .fold(0.0_f32, f32::max)
        } else {
            0.0
        };

        match edge {
            ClipEdge::Right => {
                // Right edge moves; start fixed. Cannot shrink below the last
                // note end or the minimum length.
                let dur = (edge_beat - clip.start_beat)
                    .max(min_len)
                    .max(last_note_end);
                clip.duration_beats = dur;
                // Right-edge drag = source trim: keep source_start, follow the new
                // length with source_end (clamped to media). Shared with the
                // inspector Length field so both crop/reveal, never stretch.
                clip.reconcile_audio_trim_to_length(seconds_per_beat, min_len);
            }
            ClipEdge::Left => {
                let old_start = clip.start_beat;
                let old_right = old_start + clip.duration_beats;
                // Keep the right edge fixed; clamp the new start to [0, right-min].
                let mut new_start = edge_beat.min(old_right - min_len).max(0.0);
                // Audio: never reveal earlier than the start of the available
                // media. Bounding the timeline start by the source that actually
                // exists keeps the source window and clip width locked in
                // lockstep, so revealing left uncovers real audio instead of
                // stretching the waveform (spec §4). Stretched clips reveal
                // through their ratio.
                let trims_source = clip.audio_trim_follows_length();
                let frames_per_second = clip.source_frames_per_timeline_second(seconds_per_beat);
                if trims_source {
                    let max_reveal_beats = clip.stretch.source_start_samples as f64
                        / (frames_per_second * seconds_per_beat as f64).max(f64::MIN_POSITIVE);
                    let min_new_start = (old_start as f64 - max_reveal_beats).max(0.0) as f32;
                    new_start = new_start.max(min_new_start);
                }
                // Trimming from the left must not push the earliest note < 0.
                if let ClipType::Midi { notes, .. } = &clip.clip_type {
                    if let Some(min_local) = notes.iter().map(|n| n.start).reduce(f32::min) {
                        let max_start = (old_start + min_local).max(0.0);
                        new_start = new_start.min(max_start);
                    }
                }
                let delta = old_start - new_start;
                if let ClipType::Midi { notes, .. } = &mut clip.clip_type {
                    for note in notes.iter_mut() {
                        note.start = (note.start + delta).max(0.0);
                    }
                }
                clip.start_beat = new_start;
                clip.duration_beats = (old_right - new_start).max(min_len);
                if trims_source {
                    let trim_delta_samples = ((new_start - old_start) as f64
                        * seconds_per_beat as f64
                        * frames_per_second)
                        .round() as i64;
                    let current_start = clip.stretch.source_start_samples as i64;
                    let next_start = (current_start + trim_delta_samples).max(0) as u64;
                    clip.stretch
                        .apply_trim(next_start, clip.stretch.source_end_samples.max(next_start));
                }
            }
        }

        if midi_debug_enabled() {
            eprintln!(
                "[midi] resize_clip clip={} edge={:?} start={:.3} len={:.3}",
                clip_id, edge, clip.start_beat, clip.duration_beats
            );
        }
        true
    }
}

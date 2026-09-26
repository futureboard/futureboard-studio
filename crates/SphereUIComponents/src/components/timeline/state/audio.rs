use super::*;

impl TimelineState {
    /// Drop a clip onto the timeline. `drop_x` and `drop_y` are in the track
    /// area coordinate system (header_width and ruler_height already stripped).
    /// Imports a clip with unknown metadata. The 2-bar duration is a temporary
    /// placeholder and must be replaced by DirectAudioEngine metadata.
    pub fn import_audio_at(
        &mut self,
        source_path: String,
        clip_name: String,
        drop_x: f32,
        drop_y: f32,
    ) -> String {
        eprintln!(
            "[import] drop path={} clip={} drop_x={:.1} drop_y={:.1}",
            source_path, clip_name, drop_x, drop_y
        );
        // Resolve target track: an existing lane under drop_y, otherwise create one.
        let track_id = match self.track_index_at_y(drop_y) {
            Some(idx) if matches!(self.tracks[idx].track_type, TrackType::Audio) => {
                self.tracks[idx].id.clone()
            }
            _ => self.create_audio_track(),
        };

        // Resolve start beat with snap.
        let raw_beats = self.x_to_beats(drop_x.max(0.0));
        let start_beat = self.snap_beats(raw_beats).max(0.0);

        self.insert_audio_clip(track_id, source_path, clip_name, start_beat)
    }

    pub fn import_audio_to_selected_or_new_track(
        &mut self,
        source_path: String,
        clip_name: String,
    ) -> String {
        let track_id = self
            .selected_audio_track_id()
            .unwrap_or_else(|| self.create_audio_track());
        eprintln!(
            "[import] browser path={} clip={} resolved_track_id={}",
            source_path, clip_name, track_id
        );
        let start_beat = self.snap_beats(self.x_to_beats(0.0)).max(0.0);
        self.insert_audio_clip(track_id, source_path, clip_name, start_beat)
    }

    pub(crate) fn insert_audio_clip_with_duration(
        &mut self,
        track_id: String,
        source_path: String,
        clip_name: String,
        start_beat: f32,
        duration_beats: f32,
        source_duration_seconds: Option<f64>,
    ) -> String {
        let track_id = if self.tracks.iter().any(|track| track.id == track_id) {
            track_id
        } else {
            eprintln!(
                "[recording] target track id={track_id} missing; creating fallback audio track"
            );
            self.create_audio_track()
        };

        let clip_id = self.next_clip_id();
        let new_clip = ClipState {
            id: clip_id.clone(),
            name: clip_name,
            start_beat: start_beat.max(0.0),
            duration_beats,
            source_duration_seconds,
            offset_beats: 0.0,
            gain: 1.0,
            clip_type: ClipType::Audio {
                file_id: source_path.clone(),
                source_path: Some(source_path),
            },
            muted: false,
            audio_import: AudioImportState::Pending,
            stretch: AudioClipStretchState::default(),
        };

        if let ClipType::Audio {
            source_path: Some(path),
            ..
        } = &new_clip.clip_type
        {
            eprintln!(
                "[Timeline] created audio clip clip_id={clip_id} source={path} start_beat={:.3} duration_beats={:.3}",
                new_clip.start_beat, new_clip.duration_beats
            );
        }

        if let Some(track) = self.tracks.iter_mut().find(|track| track.id == track_id) {
            track.clips.push(new_clip);
        }
        self.selection.selected_track_id = Some(track_id);
        self.selection.selected_clip_ids = vec![clip_id.clone()];
        clip_id
    }

    fn insert_audio_clip(
        &mut self,
        track_id: String,
        source_path: String,
        clip_name: String,
        start_beat: f32,
    ) -> String {
        let track_id = if self.tracks.iter().any(|track| track.id == track_id) {
            track_id
        } else {
            eprintln!(
                "[import] target track id={} missing; creating fallback audio track",
                track_id
            );
            self.create_audio_track()
        };

        let duration_beats = AUDIO_IMPORT_PLACEHOLDER_BEATS;
        eprintln!(
            "[audio-import] WARNING using fallback duration because metadata is pending: path={} duration_beats=8.0",
            source_path
        );
        self.insert_audio_clip_with_duration(
            track_id,
            source_path,
            clip_name,
            start_beat,
            duration_beats,
            None,
        )
    }

    /// Apply decoded source metadata to every clip sharing `asset_key`
    /// (`ClipState::audio_asset_key`, i.e. the clip's `file_id`). Keyed on the
    /// asset id rather than the path so it still matches after a clip's
    /// `source_path` is rewritten (e.g. copy-into-project).
    ///
    /// Only a clip still exactly as a drop created it (the pending-length
    /// placeholder) adopts the file's length. Every other clip — trimmed,
    /// split, recorded, imported from another DAW, or reopened from a project —
    /// keeps its start and length; the metadata only makes its source window
    /// real frames at the file's rate. Returns `true` when any clip changed.
    pub fn update_audio_clip_metadata(
        &mut self,
        asset_key: &str,
        format: &str,
        sample_rate: u32,
        channels: u16,
        total_frames: u64,
        duration_seconds: f64,
    ) -> bool {
        self.apply_decoded_audio_source(
            asset_key,
            format,
            sample_rate,
            channels,
            total_frames,
            duration_seconds,
            true,
        )
    }

    /// A reopened project's peaks came from the on-disk cache, so no decode
    /// runs: apply the format those peaks were built from. This sets the source
    /// duration and rates only; no clip's length is taken from the file, not
    /// even a placeholder's (a saved clip's length is what the user left).
    pub fn apply_cached_audio_source_format(
        &mut self,
        asset_key: &str,
        sample_rate: u32,
        channels: u16,
        total_frames: u64,
        duration_seconds: f64,
    ) -> bool {
        self.apply_decoded_audio_source(
            asset_key,
            "cached-peaks",
            sample_rate,
            channels,
            total_frames,
            duration_seconds,
            false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_decoded_audio_source(
        &mut self,
        asset_key: &str,
        format: &str,
        sample_rate: u32,
        channels: u16,
        total_frames: u64,
        duration_seconds: f64,
        adopt_placeholder_length: bool,
    ) -> bool {
        if duration_seconds <= 0.0 || sample_rate == 0 {
            return false;
        }
        let project_bpm = self.bpm.max(1.0) as f64;
        let seconds_per_beat = self.seconds_per_beat() as f64;
        let tempo = self.resolved_tempo_map();
        let source = DecodedAudioSource {
            sample_rate,
            total_frames,
            duration_seconds,
        };
        let mut changed = false;
        let mut matched = false;
        let mut adopted: Vec<(usize, usize)> = Vec::new();
        for (track_index, track) in self.tracks.iter_mut().enumerate() {
            for (clip_index, clip) in track.clips.iter_mut().enumerate() {
                if clip.audio_asset_key() != Some(asset_key) {
                    continue;
                }
                matched = true;
                let before = (
                    clip.stretch.source_start_samples,
                    clip.stretch.source_end_samples,
                );
                let adopt = clip.apply_decoded_source(
                    source,
                    seconds_per_beat,
                    &tempo,
                    project_bpm,
                    adopt_placeholder_length,
                );
                if adopt {
                    adopted.push((track_index, clip_index));
                }
                changed |= before
                    != (
                        clip.stretch.source_start_samples,
                        clip.stretch.source_end_samples,
                    );
            }
        }
        // A fresh import takes the file's length, through the tempo map and the
        // stretch ratio like every decoded clip (`audio_clip_end_beat`), not
        // the project's first tempo.
        for (track_index, clip_index) in adopted {
            let clip = &self.tracks[track_index].clips[clip_index];
            let Some(end_beat) = self.audio_clip_end_beat(clip) else {
                continue;
            };
            let beats = (end_beat - clip.start_beat as f64) as f32;
            if (clip.duration_beats - beats).abs() > 0.001 {
                self.tracks[track_index].clips[clip_index].duration_beats = beats;
                changed = true;
            }
        }
        if matched {
            self.log_audio_meta(
                asset_key,
                format,
                sample_rate,
                channels,
                total_frames,
                duration_seconds,
            );
            self.log_audio_import(self.seconds_to_beats(duration_seconds));
        }
        changed
    }

    /// Retarget the stable asset id (`file_id`) for every clip sharing `old_key`.
    /// Used after copy-into-project so the cache key matches the saved project
    /// relative path. Returns `true` if any clip changed.
    pub fn retarget_audio_asset_id(&mut self, old_key: &str, new_key: &str) -> bool {
        if old_key == new_key {
            return false;
        }
        let mut changed = false;
        for track in &mut self.tracks {
            for clip in &mut track.clips {
                if clip.audio_asset_key() != Some(old_key) {
                    continue;
                }
                if let ClipType::Audio { file_id, .. } = &mut clip.clip_type {
                    if file_id.as_str() != new_key {
                        *file_id = new_key.to_string();
                        changed = true;
                    }
                }
            }
        }
        changed
    }

    /// Point every clip sharing `asset_key` at a new resolvable `source_path`
    /// (e.g. after copying the source into the project folder). Returns `true`
    /// if any clip changed.
    pub fn retarget_audio_source(&mut self, asset_key: &str, new_source_path: &str) -> bool {
        let mut changed = false;
        for track in &mut self.tracks {
            for clip in &mut track.clips {
                if clip.audio_asset_key() != Some(asset_key) {
                    continue;
                }
                if let ClipType::Audio { source_path, .. } = &mut clip.clip_type {
                    if source_path.as_deref() != Some(new_source_path) {
                        *source_path = Some(new_source_path.to_string());
                        changed = true;
                    }
                }
            }
        }
        changed
    }

    /// Set the import state on every clip sharing `asset_key` (the `file_id`).
    pub fn set_audio_import_for_asset(&mut self, asset_key: &str, state: AudioImportState) {
        for track in &mut self.tracks {
            for clip in &mut track.clips {
                if clip.audio_asset_key() == Some(asset_key) {
                    clip.audio_import = state.clone();
                }
            }
        }
    }

    pub fn audio_source_duration_seconds(&self, asset_key: &str) -> Option<f64> {
        self.tracks.iter().find_map(|track| {
            track.clips.iter().find_map(|clip| {
                if clip.audio_asset_key() == Some(asset_key) {
                    return clip.source_duration_seconds;
                }
                None
            })
        })
    }

    fn log_audio_meta(
        &self,
        source_path: &str,
        format: &str,
        sample_rate: u32,
        channels: u16,
        total_frames: u64,
        duration_seconds: f64,
    ) {
        eprintln!("[audio-meta] path={}", source_path);
        eprintln!("[audio-meta] format={}", format);
        eprintln!("[audio-meta] sample_rate={}", sample_rate);
        eprintln!("[audio-meta] channels={}", channels);
        eprintln!("[audio-meta] total_frames={}", total_frames);
        eprintln!("[audio-meta] duration_seconds={:.6}", duration_seconds);
    }

    fn log_audio_import(&self, duration_beats: f32) {
        let bars_4_4 = duration_beats / 4.0;
        eprintln!("[audio-import] bpm={:.3}", self.bpm);
        eprintln!("[audio-import] duration_beats={:.6}", duration_beats);
        eprintln!("[audio-import] bars_4_4={:.6}", bars_4_4);
    }
}

/// Length a dropped audio clip shows until its file's metadata arrives.
pub(crate) const AUDIO_IMPORT_PLACEHOLDER_BEATS: f32 = 8.0;

/// Format of a decoded audio file, as the import probe or the peak cache
/// reports it.
#[derive(Debug, Clone, Copy)]
struct DecodedAudioSource {
    sample_rate: u32,
    total_frames: u64,
    duration_seconds: f64,
}

/// Wall-clock seconds `clip`'s timeline length plays for: the inverse of the
/// beat conversion `audio_clip_end_beat` uses. A clip locked to the project
/// tempo is defined in beats at the project tempo; any other clip covers its
/// beats under the tempo map from its own start.
fn audio_clip_played_seconds(
    clip: &ClipState,
    tempo: &DirectAudio::TempoMap,
    project_bpm: f64,
) -> f64 {
    let beats = clip.duration_beats.max(0.0) as f64;
    if clip.stretch.follows_project_tempo() {
        return beats * 60.0 / project_bpm.max(1.0);
    }
    let start_beat = clip.start_beat.max(0.0) as f64;
    (tempo.seconds_at_beat(start_beat + beats) - tempo.seconds_at_beat(start_beat)).max(0.0)
}

/// Multiply every stored source position by `factor`, converting them from
/// the rate they were written in to another.
fn rescale_source_positions(stretch: &mut AudioClipStretchState, factor: f64) {
    let scale = |value: u64| (value as f64 * factor).round().max(0.0) as u64;
    stretch.source_start_samples = scale(stretch.source_start_samples);
    stretch.source_end_samples = scale(stretch.source_end_samples);
    stretch.original_duration_samples = scale(stretch.original_duration_samples);
    for marker in &mut stretch.warp_markers {
        marker.source_sample = scale(marker.source_sample);
    }
}

impl ClipState {
    /// Still exactly as a drop created it: never decoded, never trimmed, split
    /// or offset, at the placeholder length. Only such a clip may take its
    /// length from the file.
    fn is_untouched_import_placeholder(&self) -> bool {
        self.stretch.original_sample_rate == 0
            && self.source_duration_seconds.is_none()
            && self.offset_beats == 0.0
            && self.stretch.source_start_samples == 0
            && self.stretch.source_end_samples == 0
            && self.stretch.warp_markers.is_empty()
            && (self.duration_beats - AUDIO_IMPORT_PLACEHOLDER_BEATS).abs() < 1.0e-4
    }

    /// Store source positions in the project rate while this audio clip's file
    /// has not been decoded. Trim math used to fall back to 1 Hz here, so the
    /// window it wrote was really in seconds; stored as project-rate frames the
    /// engine reads them in the same unit, and the first decode rescales them
    /// to the file's own rate. Positions already written at 1 Hz are converted.
    pub(crate) fn assume_project_rate_until_decoded(&mut self, project_rate: u32) {
        if !matches!(self.clip_type, ClipType::Audio { .. })
            || self.stretch.source_sample_rate() > 0
            || project_rate == 0
        {
            return;
        }
        rescale_source_positions(&mut self.stretch, project_rate as f64);
        self.stretch.project_sample_rate = project_rate;
    }

    /// Apply a decoded file's format to this clip. Returns `true` when the clip
    /// is an untouched placeholder that should now take the file's length
    /// (only when `adopt_placeholder_length`); its window then spans the file.
    ///
    /// Otherwise the clip's start and length never change. A clip decoded for
    /// the first time has its stored positions — written at the rate the trim
    /// math assumed then (1 Hz before, the project rate now) — rescaled to the
    /// file's rate, so it keeps playing the same audio. A clip whose window was
    /// never set gets one from where it plays now: its legacy beat offset (at
    /// the project tempo, as the engine reads it) or its left-trim start, for
    /// the length it covers on the timeline through the tempo map and its
    /// stretch ratio.
    fn apply_decoded_source(
        &mut self,
        source: DecodedAudioSource,
        seconds_per_beat: f64,
        tempo: &DirectAudio::TempoMap,
        project_bpm: f64,
        adopt_placeholder_length: bool,
    ) -> bool {
        let rate = source.sample_rate;
        let first_decode = self.stretch.original_sample_rate == 0;
        let adopt = adopt_placeholder_length && self.is_untouched_import_placeholder();
        self.source_duration_seconds = Some(source.duration_seconds);

        if adopt {
            self.stretch.original_sample_rate = rate;
            self.stretch.project_sample_rate = rate;
            self.stretch.original_duration_samples = source.total_frames;
            self.stretch.source_start_samples = 0;
            self.stretch.source_end_samples = source.total_frames;
            return true;
        }

        if first_decode {
            // The unit the positions were written in: 1 Hz when no rate was
            // known at all, the project rate once trims assume it.
            let written_at = self.stretch.source_sample_rate().max(1) as f64;
            let factor = rate as f64 / written_at;
            if (factor - 1.0).abs() > f64::EPSILON {
                rescale_source_positions(&mut self.stretch, factor);
            }
            self.stretch.original_duration_samples = source.total_frames;
        } else {
            self.stretch.original_duration_samples = self
                .stretch
                .original_duration_samples
                .max(source.total_frames);
        }
        self.stretch.original_sample_rate = rate;
        self.stretch.project_sample_rate = rate;

        let window_unset = self.stretch.source_end_samples <= self.stretch.source_start_samples;
        if window_unset && matches!(self.clip_type, ClipType::Audio { .. }) {
            let start = if self.stretch.source_start_samples > 0 {
                self.stretch.source_start_samples
            } else {
                (self.offset_beats.max(0.0) as f64 * seconds_per_beat * rate as f64)
                    .round()
                    .max(0.0) as u64
            };
            let ratio = self.stretch.effective_time_ratio(project_bpm);
            let ratio = if ratio.is_finite() && ratio > 1.0e-6 {
                ratio
            } else {
                1.0
            };
            let played_seconds = audio_clip_played_seconds(self, tempo, project_bpm);
            let length = (played_seconds * rate as f64 / ratio).round().max(0.0) as u64;
            let start = start.min(source.total_frames);
            let end = start.saturating_add(length).min(source.total_frames);
            if end > start {
                self.stretch.source_start_samples = start;
                self.stretch.source_end_samples = end;
            }
        } else {
            self.stretch.source_end_samples =
                self.stretch.source_end_samples.min(source.total_frames);
        }
        false
    }
}

#[cfg(test)]
mod metadata_tests {
    use super::*;
    use std::path::PathBuf;

    const KEY: &str = "C:/a/take.wav";

    fn state_at(bpm: f32) -> TimelineState {
        let mut state = TimelineState::default();
        state.tracks.clear();
        state.bpm = bpm;
        state.project_sample_rate = 48_000;
        state.snap_to_grid = false;
        state
    }

    fn clip(state: &TimelineState, id: &str) -> ClipState {
        state.find_clip(id).expect("clip").1.clone()
    }

    fn window(clip: &ClipState) -> (u64, u64) {
        (
            clip.stretch.source_start_samples,
            clip.stretch.source_end_samples,
        )
    }

    /// A drop still at its placeholder length takes the file's length.
    #[test]
    fn a_fresh_placeholder_adopts_the_file_length() {
        let mut state = state_at(90.0);
        let id = state.import_audio_at(KEY.to_string(), "take".to_string(), 0.0, 1.0e9);
        assert_eq!(
            clip(&state, &id).duration_beats,
            AUDIO_IMPORT_PLACEHOLDER_BEATS
        );

        assert!(state.update_audio_clip_metadata(KEY, "wav", 48_000, 2, 192_000, 4.0));
        let clip = clip(&state, &id);
        // 4 s at 90 BPM.
        assert!((clip.duration_beats - 6.0).abs() < 1.0e-3);
        assert_eq!(window(&clip), (0, 192_000));
        assert_eq!(clip.source_duration_seconds, Some(4.0));
    }

    /// The adopted length goes through the tempo map, like every decoded
    /// clip's length, not through the project's first tempo.
    #[test]
    fn an_adopted_length_follows_the_tempo_map() {
        let mut state = state_at(120.0);
        state.add_tempo_point(2.0, 60.0);
        let id = state.import_audio_at(KEY.to_string(), "take".to_string(), 0.0, 1.0e9);
        state.update_audio_clip_metadata(KEY, "wav", 48_000, 2, 144_000, 3.0);
        // Beats 0–2 take 1 s at 120 BPM; the other 2 s cover 2 beats at 60.
        assert!((clip(&state, &id).duration_beats - 4.0).abs() < 1.0e-3);
    }

    /// A trim written while the rates were unknown used 1 Hz math, so its
    /// stored positions are seconds. The first metadata converts them to
    /// frames instead of resetting the window to the whole file.
    #[test]
    fn a_legacy_one_hertz_left_trim_keeps_its_audible_offset() {
        let mut state = state_at(120.0);
        let id =
            state.insert_recorded_clip("t", KEY.to_string(), "take".to_string(), 2.0, 3.0, 120.0);
        {
            let (_, clip) = state.find_clip(&id).unwrap();
            assert_eq!(clip.stretch.source_sample_rate(), 0);
        }
        // What an older build saved after a left trim of 1 s before metadata:
        // start == end == 1 (seconds, at 1 Hz).
        for track in &mut state.tracks {
            for clip in &mut track.clips {
                clip.stretch.source_start_samples = 1;
                clip.stretch.source_end_samples = 1;
            }
        }
        let before = clip(&state, &id);

        state.update_audio_clip_metadata(KEY, "wav", 44_100, 2, 176_400, 4.0);
        let after = clip(&state, &id);
        assert_eq!(after.start_beat, before.start_beat);
        assert_eq!(after.duration_beats, before.duration_beats);
        // Plays from 1 s for the clip's 3 s (6 beats at 120 BPM).
        assert_eq!(window(&after), (44_100, 44_100 + 3 * 44_100));
    }

    /// A left trim before metadata now stores project-rate frames; the first
    /// decode rescales them to the file's own rate, same audio, same clip.
    #[test]
    fn a_left_trim_before_metadata_survives_the_first_decode() {
        let mut state = state_at(120.0);
        let id =
            state.insert_recorded_clip("t", KEY.to_string(), "take".to_string(), 0.0, 4.0, 120.0);
        assert!(state.resize_clip(&id, ClipEdge::Left, 2.0));
        let trimmed = clip(&state, &id);
        assert_eq!(trimmed.start_beat, 2.0);
        assert_eq!(trimmed.duration_beats, 6.0);
        // 1 s into the take, in project-rate frames, not 1 (Hz).
        assert_eq!(trimmed.stretch.source_start_samples, 48_000);
        assert_eq!(trimmed.stretch.project_sample_rate, 48_000);

        state.update_audio_clip_metadata(KEY, "wav", 44_100, 2, 176_400, 4.0);
        let decoded = clip(&state, &id);
        assert_eq!(decoded.start_beat, 2.0);
        assert_eq!(decoded.duration_beats, 6.0);
        assert_eq!(window(&decoded), (44_100, 176_400));
    }

    /// A right trim before metadata no longer writes a 1 Hz window.
    #[test]
    fn a_right_trim_before_metadata_is_kept_in_frames() {
        let mut state = state_at(120.0);
        let id = state.import_audio_at(KEY.to_string(), "take".to_string(), 0.0, 1.0e9);
        assert!(state.resize_clip(&id, ClipEdge::Right, 3.0));
        let trimmed = clip(&state, &id);
        assert_eq!(window(&trimmed), (0, 72_000), "1.5 s at the project rate");

        state.update_audio_clip_metadata(KEY, "wav", 44_100, 2, 441_000, 10.0);
        let decoded = clip(&state, &id);
        assert!(
            (decoded.duration_beats - 3.0).abs() < 1.0e-4,
            "no reset to the file"
        );
        assert_eq!(window(&decoded), (0, 66_150), "1.5 s at the file rate");
    }

    /// Both halves of a clip split before its metadata arrived keep their
    /// place and length; each gets the part of the file it plays.
    #[test]
    fn split_halves_keep_their_offset_and_length() {
        let mut state = state_at(120.0);
        let id = state.import_audio_at(KEY.to_string(), "take".to_string(), 0.0, 1.0e9);
        let original = clip(&state, &id);
        let (left, right) = state.plan_audio_clip_split(&original, 3.0).expect("split");
        state.delete_clip(&id);
        let (left_id, right_id) = (left.id.clone(), right.id.clone());
        state.tracks[0].clips.push(left);
        state.tracks[0].clips.push(right);

        state.update_audio_clip_metadata(KEY, "wav", 48_000, 2, 480_000, 10.0);
        let left = clip(&state, &left_id);
        let right = clip(&state, &right_id);
        assert_eq!((left.start_beat, left.duration_beats), (0.0, 3.0));
        assert_eq!((right.start_beat, right.duration_beats), (3.0, 5.0));
        assert_eq!(window(&left), (0, 72_000));
        assert_eq!(window(&right), (72_000, 192_000));
    }

    /// A decoded, trimmed clip is never widened back to the whole file.
    #[test]
    fn a_trimmed_clip_is_never_widened() {
        let mut state = state_at(120.0);
        let id = state.import_audio_at(KEY.to_string(), "take".to_string(), 0.0, 1.0e9);
        state.update_audio_clip_metadata(KEY, "wav", 48_000, 2, 192_000, 4.0);
        assert!(state.resize_clip(&id, ClipEdge::Right, 2.0));
        let trimmed = clip(&state, &id);

        assert!(!state.update_audio_clip_metadata(KEY, "wav", 48_000, 2, 192_000, 4.0));
        let again = clip(&state, &id);
        assert_eq!(again.duration_beats, trimmed.duration_beats);
        assert_eq!(window(&again), window(&trimmed));
    }

    /// A clip imported from another DAW arrives with only a beat offset and a
    /// length. Opening it runs the metadata pass, which used to grow every
    /// trimmed clip to the full file.
    #[test]
    fn a_daw_imported_trimmed_clip_keeps_its_length() {
        use crate::project::{
            apply_to_timeline, ClipSource, FutureboardProject, ProjectAsset, ProjectClip,
            ProjectTrack, ProjectTrackType, TrackRouting,
        };
        let mut project = FutureboardProject::new("Imported");
        project.settings.bpm = 120.0;
        project.assets.push(ProjectAsset {
            id: "cubase-1".to_string(),
            original_filename: "take.wav".to_string(),
            relative_path: None,
            absolute_path: Some(PathBuf::from(KEY)),
            duration_secs: Some(10.0),
            sample_rate: Some(48_000),
            channels: Some(2),
            source_fingerprint: None,
            waveform_peak_relative_path: None,
            duration_samples: Some(480_000),
        });
        let mut track = ProjectTrack {
            id: "t1".to_string(),
            name: "Audio 1".to_string(),
            track_type: ProjectTrackType::Audio,
            ara: None,
            timebase: TrackTimebase::Musical.to_tag(),
            parent_group_id: None,
            group_collapsed: false,
            color_hex: "#56C7C9".to_string(),
            volume_norm: 1.0,
            pan: 0.0,
            muted: false,
            solo: false,
            record_arm: false,
            input_monitor: crate::project::InputMonitorMode::Off,
            routing: TrackRouting::default(),
            inserts: Vec::new(),
            automation_lanes: Vec::new(),
            clips: Vec::new(),
            row_height_px: None,
            soundfont: None,
            volume_automation_read: true,
            solfege: None,
            takes: Vec::new(),
            takes_expanded: false,
        };
        track.clips.push(ProjectClip {
            id: "c1".to_string(),
            name: "take".to_string(),
            start_beat: 8.0,
            duration_beats: 4.0,
            offset_beats: 2.0,
            gain: 1.0,
            muted: false,
            source: ClipSource::Audio {
                asset_id: "cubase-1".to_string(),
                source_path: Some(PathBuf::from(KEY)),
            },
            stretch: AudioClipStretchState::default(),
        });
        project.tracks.push(track);
        let mut state = TimelineState::default();
        let _ = apply_to_timeline(&project, &mut state);

        state.update_audio_clip_metadata("cubase-1", "wav", 48_000, 2, 480_000, 10.0);
        let clip = clip(&state, "c1");
        assert_eq!((clip.start_beat, clip.duration_beats), (8.0, 4.0));
        // From 1 s (2 beats) for 2 s, as the engine played the beat offset.
        assert_eq!(window(&clip), (48_000, 144_000));
    }

    /// A cache hit on reopen applies the format only; even a clip saved at
    /// the placeholder length keeps the length it was saved with.
    #[test]
    fn a_peak_cache_hit_applies_the_format_but_no_length() {
        let mut state = state_at(120.0);
        let id = state.import_audio_at(KEY.to_string(), "take".to_string(), 0.0, 1.0e9);
        state.apply_cached_audio_source_format(KEY, 48_000, 2, 480_000, 10.0);
        let clip = clip(&state, &id);
        assert_eq!(clip.duration_beats, AUDIO_IMPORT_PLACEHOLDER_BEATS);
        assert_eq!(clip.source_duration_seconds, Some(10.0));
        assert_eq!(clip.stretch.original_sample_rate, 48_000);
        assert_eq!(window(&clip), (0, 192_000), "the 8 beats it already played");
    }

    /// A recorded clip starts with the recorder's real format, so it never
    /// depends on a waveform import to have a window.
    #[test]
    fn a_seeded_recording_has_real_frames_and_a_tempo_map_length() {
        let mut state = state_at(120.0);
        state.add_tempo_point(2.0, 60.0);
        let id =
            state.insert_recorded_clip("t", KEY.to_string(), "take".to_string(), 0.0, 3.0, 120.0);
        assert!(state.seed_recorded_clip_source(&id, 44_100, 3.0));
        let clip = clip(&state, &id);
        assert_eq!(clip.stretch.original_sample_rate, 44_100);
        assert_eq!(window(&clip), (0, 132_300));
        assert!((clip.duration_beats - 4.0).abs() < 1.0e-3);

        // The post-record import then changes nothing.
        assert!(!state.update_audio_clip_metadata(KEY, "wav", 44_100, 2, 132_300, 3.0));
    }
}

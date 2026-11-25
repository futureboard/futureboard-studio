//! Converting an audio clip to MIDI: what audio the clip plays, and where
//! notes heard in it land on the timeline.
//!
//! The converter is handed the clip's source window as the clip plays it
//! (reversed when the clip is) and answers in seconds into that window. Those
//! seconds are placed through the same beat ↔ time mapping playback uses —
//! the tempo map, the clip's stretch, its warp markers — so the notes sit
//! under the audio they came from, and they sound at the pitch the clip is
//! heard at.

use super::*;
use crate::audio_to_midi::ConvertedNote;

/// The shortest note a conversion writes, in beats.
const MIN_NOTE_BEATS: f64 = 1.0 / 64.0;

/// What converting an audio clip reads.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioClipConversionSource {
    pub clip_id: String,
    pub track_id: String,
    pub clip_name: String,
    /// The decoded source file.
    pub path: String,
    /// The clip's source window, in frames at `source_rate`, as stored
    /// (resolve against the file's length with [`Self::frames_in`]).
    pub stretch: AudioClipStretchState,
    /// The clip plays its window backwards.
    pub reverse: bool,
}

impl AudioClipConversionSource {
    /// The frames of a decoded file (`total` frames at `file_rate` Hz) the
    /// clip plays, as a range of that file's frames.
    pub fn frames_in(&self, total: usize, file_rate: u32) -> std::ops::Range<usize> {
        let source_rate = self.stretch.source_sample_rate().max(1) as f64;
        let scale = file_rate.max(1) as f64 / source_rate;
        let total_source = (total as f64 / scale).round() as u64;
        let (start, end) = self.stretch.resolved_source_trim_range(total_source);
        let from = ((start as f64 * scale).round() as usize).min(total);
        let to = ((end as f64 * scale).round() as usize).clamp(from, total);
        from..to
    }
}

/// A warped clip's anchors, `(beats from the clip start, source frame)`, the
/// way the engine pins them (see `warp_split_source_sample`).
fn warp_anchors(clip: &ClipState) -> Vec<(f64, f64)> {
    let clip_start = clip.start_beat.max(0.0) as f64;
    let duration = clip.duration_beats.max(0.0) as f64;
    let start = clip.stretch.source_start_samples as f64;
    let end = clip
        .stretch
        .source_end_samples
        .max(clip.stretch.source_start_samples) as f64;
    let mut anchors = vec![(0.0, start)];
    for marker in &clip.stretch.warp_markers {
        let beat = marker.timeline_beat - clip_start;
        let frame = (marker.source_sample as f64).clamp(start, end);
        let (last_beat, last_frame) = anchors[anchors.len() - 1];
        if beat > last_beat && beat < duration && frame > last_frame {
            anchors.push((beat, frame));
        }
    }
    anchors.push((duration, end));
    anchors
}

/// The beat (from the clip start) a warped clip plays source `frame` at.
fn warp_beat_at_frame(anchors: &[(f64, f64)], frame: f64) -> f64 {
    let segment = anchors
        .windows(2)
        .find(|pair| frame <= pair[1].1)
        .or_else(|| anchors.windows(2).last());
    let Some(pair) = segment else {
        return anchors.first().map_or(0.0, |(beat, _)| *beat);
    };
    let ((b0, f0), (b1, f1)) = (pair[0], pair[1]);
    let span = f1 - f0;
    if span > f64::EPSILON {
        b0 + (b1 - b0) * ((frame - f0) / span)
    } else {
        b0
    }
}

impl TimelineState {
    /// What converting `clip_id` to MIDI reads, or why it cannot be read.
    pub fn audio_clip_conversion_source(
        &self,
        clip_id: &str,
    ) -> Result<AudioClipConversionSource, String> {
        let (track, clip) = self
            .find_clip(clip_id)
            .ok_or_else(|| "The clip is no longer in the project.".to_string())?;
        let ClipType::Audio {
            source_path: Some(path),
            ..
        } = &clip.clip_type
        else {
            return Err("Only an audio clip with a recording or file can be converted.".into());
        };
        if path.is_empty() {
            return Err("The clip has no audio file.".into());
        }
        match &clip.audio_import {
            AudioImportState::Ready => {}
            AudioImportState::Failed { message } => {
                return Err(format!("The clip's audio could not be read: {message}"));
            }
            _ => return Err("The clip's audio is still loading.".into()),
        }
        Ok(AudioClipConversionSource {
            clip_id: clip.id.clone(),
            track_id: track.id.clone(),
            clip_name: clip.name.clone(),
            path: path.clone(),
            stretch: clip.stretch.clone(),
            reverse: clip.stretch.reverse,
        })
    }

    /// The timeline beat `clip` plays `seconds` into its source window at —
    /// seconds of the window as it plays (from its end, for a reversed clip).
    pub fn audio_clip_beat_at_window_seconds(&self, clip: &ClipState, seconds: f64) -> f64 {
        let bpm = self.bpm.max(1.0) as f64;
        if matches!(clip.stretch.mode, StretchMode::Warp) && !clip.stretch.reverse {
            let rate = clip.stretch.source_sample_rate().max(1) as f64;
            let frame = clip.stretch.source_start_samples as f64 + seconds.max(0.0) * rate;
            return clip.start_beat.max(0.0) as f64
                + warp_beat_at_frame(&warp_anchors(clip), frame);
        }
        let ratio = clip.stretch.effective_time_ratio(bpm);
        let ratio = if ratio.is_finite() && ratio > 1.0e-6 {
            ratio
        } else {
            1.0
        };
        self.clip_beat_at_local_seconds(clip, seconds.max(0.0) * ratio)
    }

    /// Semitones the clip's pitch is moved by as it plays: its pitch shift,
    /// or — where speed and pitch go together — its speed.
    pub fn audio_clip_heard_transpose(&self, clip: &ClipState) -> i32 {
        let semitones = if clip.stretch.pitch_linked_to_duration() {
            let speed = clip.stretch.resample_speed_ratio(self.bpm.max(1.0) as f64);
            if speed.is_finite() && speed > 1.0e-6 {
                12.0 * speed.log2()
            } else {
                0.0
            }
        } else {
            clip.stretch.pitch_shift_semitones as f64
        };
        semitones.round() as i32
    }

    /// `notes` heard in `clip_id`'s audio as the notes of a MIDI clip over
    /// the same span: beats from the clip's start, cut to its length, moved
    /// by the pitch the clip is heard at when `transpose` (not for drums,
    /// whose notes name instruments). `None` when the clip is gone.
    pub fn converted_midi_notes(
        &self,
        clip_id: &str,
        notes: &[ConvertedNote],
        transpose: bool,
    ) -> Option<Vec<MidiNoteState>> {
        let (_, clip) = self.find_clip(clip_id)?;
        let clip_start = clip.start_beat.max(0.0) as f64;
        let length = clip.duration_beats.max(0.0) as f64;
        let shift = if transpose {
            self.audio_clip_heard_transpose(clip)
        } else {
            0
        };
        let mut out = Vec::with_capacity(notes.len());
        for note in notes {
            let at = |seconds: f64| self.audio_clip_beat_at_window_seconds(clip, seconds);
            let start = at(note.start_seconds) - clip_start;
            let end = at(note.start_seconds + note.duration_seconds.max(0.0)) - clip_start;
            if start >= length || end <= 0.0 {
                continue;
            }
            let (start, end) = (start.max(0.0), end.min(length));
            let pitch = (note.pitch as i32 + shift).clamp(0, 127) as u8;
            let mut midi = MidiNoteState::new(
                pitch,
                start as f32,
                (end - start).max(MIN_NOTE_BEATS) as f32,
                note.velocity.clamp(1, 127),
            );
            if !note.bend.is_empty() {
                let note_beat = at(note.start_seconds);
                let points = note
                    .bend
                    .iter()
                    .map(|(seconds, cents)| {
                        let beat = at(note.start_seconds + seconds) - note_beat;
                        PitchPoint::new(beat as f32, *cents, PitchSegmentShape::Smooth)
                    })
                    .collect();
                midi.pitch_curve = Some(PitchCurve::from_points(points));
            }
            out.push(midi);
        }
        out.sort_by(|a, b| a.start.total_cmp(&b.start).then(a.pitch.cmp(&b.pitch)));
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(start: f64, duration: f64, pitch: u8) -> ConvertedNote {
        ConvertedNote {
            start_seconds: start,
            duration_seconds: duration,
            pitch,
            velocity: 100,
            bend: Vec::new(),
        }
    }

    /// An audio clip at `start_beat`, `beats` long, ready, over a 48 kHz file.
    fn audio_clip(state: &mut TimelineState, start_beat: f32, beats: f32) -> String {
        let track_id = state.create_audio_track();
        let mut clip = ClipState {
            id: "clip-conv".into(),
            name: "Vocal".into(),
            start_beat,
            duration_beats: beats,
            source_duration_seconds: Some(10.0),
            offset_beats: 0.0,
            gain: 1.0,
            clip_type: ClipType::Audio {
                file_id: "file-1".into(),
                source_path: Some("vocal.wav".into()),
            },
            muted: false,
            audio_import: AudioImportState::Ready,
            stretch: AudioClipStretchState::default(),
        };
        clip.stretch.original_sample_rate = 48_000;
        clip.stretch.original_duration_samples = 480_000;
        clip.stretch.source_start_samples = 0;
        clip.stretch.source_end_samples = 480_000;
        let track = state
            .tracks
            .iter_mut()
            .find(|track| track.id == track_id)
            .unwrap();
        track.clips.push(clip);
        "clip-conv".into()
    }

    #[test]
    fn notes_land_on_the_beats_their_audio_plays_at() {
        let mut state = TimelineState::default();
        state.bpm = 120.0;
        let clip = audio_clip(&mut state, 4.0, 8.0);
        // 120 BPM: half a second a beat.
        let notes = state
            .converted_midi_notes(&clip, &[note(0.0, 0.5, 60), note(1.0, 0.25, 64)], true)
            .unwrap();
        assert_eq!(notes.len(), 2);
        assert!((notes[0].start - 0.0).abs() < 1.0e-4);
        assert!((notes[0].duration - 1.0).abs() < 1.0e-4);
        assert!((notes[1].start - 2.0).abs() < 1.0e-4);
        assert!((notes[1].duration - 0.5).abs() < 1.0e-4);
    }

    #[test]
    fn notes_past_the_clip_are_cut_and_dropped() {
        let mut state = TimelineState::default();
        state.bpm = 120.0;
        let clip = audio_clip(&mut state, 0.0, 2.0);
        let notes = state
            .converted_midi_notes(&clip, &[note(0.75, 1.0, 60), note(3.0, 0.5, 62)], true)
            .unwrap();
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!((notes[0].start - 1.5).abs() < 1.0e-4);
        assert!(
            (notes[0].duration - 0.5).abs() < 1.0e-4,
            "cut at the clip's end"
        );
    }

    #[test]
    fn a_pitch_shifted_clip_transposes_its_notes_but_not_its_drums() {
        let mut state = TimelineState::default();
        state.bpm = 120.0;
        let clip = audio_clip(&mut state, 0.0, 8.0);
        for track in &mut state.tracks {
            for c in &mut track.clips {
                c.stretch.mode = StretchMode::Manual;
                c.stretch.preserve_pitch = true;
                c.stretch.pitch_shift_semitones = 3.0;
            }
        }
        let shifted = state
            .converted_midi_notes(&clip, &[note(0.0, 0.5, 60)], true)
            .unwrap();
        assert_eq!(shifted[0].pitch, 63);
        let drums = state
            .converted_midi_notes(&clip, &[note(0.0, 0.1, 36)], false)
            .unwrap();
        assert_eq!(drums[0].pitch, 36);
    }

    #[test]
    fn a_bent_note_carries_its_curve_in_beats() {
        let mut state = TimelineState::default();
        state.bpm = 120.0;
        let clip = audio_clip(&mut state, 0.0, 8.0);
        let mut bent = note(0.5, 1.0, 67);
        bent.bend = vec![(0.0, -30.0), (0.5, 40.0)];
        let notes = state.converted_midi_notes(&clip, &[bent], true).unwrap();
        let curve = notes[0].pitch_curve.as_ref().expect("a curve");
        assert_eq!(curve.points.len(), 2);
        assert!(
            (curve.points[1].beat - 1.0).abs() < 1.0e-4,
            "half a second is a beat"
        );
        assert_eq!(curve.points[1].cents, 40.0);
    }

    #[test]
    fn a_trimmed_window_is_read_from_the_file() {
        let mut state = TimelineState::default();
        let clip = audio_clip(&mut state, 0.0, 8.0);
        for track in &mut state.tracks {
            for c in &mut track.clips {
                c.stretch.source_start_samples = 48_000;
                c.stretch.source_end_samples = 96_000;
            }
        }
        let source = state.audio_clip_conversion_source(&clip).unwrap();
        assert_eq!(source.frames_in(480_000, 48_000), 48_000..96_000);
        // The same window in a file decoded at another rate.
        assert_eq!(source.frames_in(441_000, 44_100), 44_100..88_200);
    }

    #[test]
    fn a_clip_still_loading_cannot_be_converted() {
        let mut state = TimelineState::default();
        let clip = audio_clip(&mut state, 0.0, 8.0);
        for track in &mut state.tracks {
            for c in &mut track.clips {
                c.audio_import = AudioImportState::Decoding { progress: 0.5 };
            }
        }
        assert!(state.audio_clip_conversion_source(&clip).is_err());
        assert!(state.audio_clip_conversion_source("nope").is_err());
    }

    #[test]
    fn a_warped_clip_places_notes_through_its_markers() {
        let anchors = vec![(0.0, 0.0), (2.0, 48_000.0), (6.0, 96_000.0)];
        // The first second of audio takes two beats, the next four.
        assert!((warp_beat_at_frame(&anchors, 24_000.0) - 1.0).abs() < 1.0e-9);
        assert!((warp_beat_at_frame(&anchors, 72_000.0) - 4.0).abs() < 1.0e-9);
        assert!((warp_beat_at_frame(&anchors, 96_000.0) - 6.0).abs() < 1.0e-9);
    }
}

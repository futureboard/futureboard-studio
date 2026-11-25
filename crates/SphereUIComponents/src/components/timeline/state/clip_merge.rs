//! Merge and Glue: several clips on one track become one clip.
//!
//! **MIDI** clips always merge. Notes, controller points, articulation events
//! and SysEx are stored relative to their clip, so each is moved by its clip's
//! distance from the merged clip's start. What a clip's end cut off is left
//! out, so the merged clip plays exactly what its parts played; a muted part's
//! notes come over muted for the same reason.
//!
//! **Audio** clips merge only when they are one recording cut apart: the same
//! source, end to start on the timeline and in the source, processed alike —
//! the halves of a split, put back together. Anything else would have to be
//! rendered into a new file, which this does not do, so it says why instead.
//!
//! The merged clip keeps the earliest clip's id and name, so a selection, an
//! ARA binding or a take pointing at it still finds it.

use super::*;

/// One track's merge: `merged` replaces every clip in `replaced` (its own id
/// among them).
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedClipMerge {
    pub track_id: String,
    pub merged: ClipState,
    pub replaced: Vec<String>,
}

/// Timeline beats closer than this are the same position.
const SAME_BEAT: f32 = 1.0e-3;

impl TimelineState {
    /// The merges `clip_ids` asks for: on every track with two or more of
    /// them, those clips as one. Pure — the caller applies and records it.
    ///
    /// `Err` says, for the status bar, why nothing can be merged: fewer than
    /// two clips on any one track, or a track whose clips cannot be (audio
    /// that is not one recording, MIDI with audio, video).
    pub fn plan_clip_merge(
        &self,
        clip_ids: &[String],
    ) -> Result<Vec<PlannedClipMerge>, &'static str> {
        let mut per_track: Vec<(String, Vec<&ClipState>)> = Vec::new();
        for track in &self.tracks {
            let mut clips: Vec<&ClipState> = track
                .clips
                .iter()
                .filter(|clip| clip_ids.iter().any(|id| *id == clip.id))
                .collect();
            if clips.len() < 2 {
                continue;
            }
            clips.sort_by(|a, b| a.start_beat.total_cmp(&b.start_beat));
            per_track.push((track.id.clone(), clips));
        }
        if per_track.is_empty() {
            return Err("Select two or more clips on one track to merge them");
        }
        per_track
            .into_iter()
            .map(|(track_id, clips)| {
                let merged = merge_clips(&clips)?;
                Ok(PlannedClipMerge {
                    track_id,
                    merged,
                    replaced: clips.iter().map(|clip| clip.id.clone()).collect(),
                })
            })
            .collect()
    }

    /// The Glue tool's click: `clip_id` and the clip right after it on its
    /// track (the next one to start at or after its end) as one.
    pub fn plan_glue_with_next(&self, clip_id: &str) -> Result<PlannedClipMerge, &'static str> {
        let (track, clip) = self.find_clip(clip_id).ok_or("That clip is gone")?;
        let end = clip.start_beat + clip.duration_beats;
        let next = track
            .clips
            .iter()
            .filter(|other| other.id != clip.id && other.start_beat >= end - SAME_BEAT)
            .min_by(|a, b| a.start_beat.total_cmp(&b.start_beat))
            .ok_or("No clip after this one on its track to glue it to")?;
        let merged = merge_clips(&[clip, next])?;
        Ok(PlannedClipMerge {
            track_id: track.id.clone(),
            merged,
            replaced: vec![clip.id.clone(), next.id.clone()],
        })
    }

    /// Put `plan` into the arrangement: the merged clip where its first part
    /// was, the other parts gone. Returns whether the track was found.
    pub fn apply_clip_merge(&mut self, plan: &PlannedClipMerge) -> bool {
        let Some(track) = self
            .tracks
            .iter_mut()
            .find(|track| track.id == plan.track_id)
        else {
            return false;
        };
        let keep = &plan.merged.id;
        track
            .clips
            .retain(|clip| clip.id == *keep || !plan.replaced.contains(&clip.id));
        match track.clips.iter_mut().find(|clip| clip.id == *keep) {
            Some(slot) => *slot = plan.merged.clone(),
            None => track.clips.push(plan.merged.clone()),
        }
        // A take row pointing at a part that is gone points at nothing.
        track
            .takes
            .retain(|take| take.clip_id == *keep || !plan.replaced.contains(&take.clip_id));
        if track.takes.is_empty() {
            track.takes_expanded = false;
        }
        true
    }
}

/// `clips`, sorted by start, as one clip.
fn merge_clips(clips: &[&ClipState]) -> Result<ClipState, &'static str> {
    let first = clips.first().ok_or("Nothing to merge")?;
    if clips
        .iter()
        .all(|clip| matches!(clip.clip_type, ClipType::Midi { .. }))
    {
        return Ok(merge_midi(clips));
    }
    if clips
        .iter()
        .all(|clip| matches!(clip.clip_type, ClipType::Audio { .. }))
    {
        let mut merged = (*first).clone();
        for next in &clips[1..] {
            merged = glue_audio(&merged, next)?;
        }
        return Ok(merged);
    }
    Err("Only MIDI clips with MIDI clips, or audio clips with audio clips, can be merged")
}

fn merge_midi(clips: &[&ClipState]) -> ClipState {
    let first = clips[0];
    let start = clips
        .iter()
        .map(|clip| clip.start_beat)
        .fold(f32::INFINITY, f32::min);
    let end = clips
        .iter()
        .map(|clip| clip.start_beat + clip.duration_beats)
        .fold(f32::NEG_INFINITY, f32::max);

    let mut notes: Vec<MidiNoteState> = Vec::new();
    let mut lanes: Vec<MidiControllerLane> = Vec::new();
    let mut sysex: Vec<MidiSysExEvent> = Vec::new();
    let mut articulations: Vec<MidiArticulationEvent> = Vec::new();
    for clip in clips {
        let ClipType::Midi {
            notes: clip_notes,
            controller_lanes,
            sysex_events,
            articulations: clip_articulations,
        } = &clip.clip_type
        else {
            continue;
        };
        let shift = clip.start_beat - start;
        let length = clip.duration_beats;
        let heard = |beat: f32| beat < length - SAME_BEAT * 0.5;
        for note in clip_notes.iter().filter(|note| heard(note.start)) {
            let mut moved = note.clone();
            moved.start = note.start + shift;
            // The part's end cut the note off there.
            moved.duration = note.duration.min(length - note.start).max(MIN_NOTE_BEATS);
            moved.muted |= clip.muted;
            notes.push(moved);
        }
        for lane in controller_lanes {
            let points = lane
                .points
                .iter()
                .filter(|point| heard(point.beat))
                .map(|point| MidiControllerPoint {
                    beat: point.beat + shift,
                    ..point.clone()
                });
            match lanes.iter_mut().find(|merged| merged.kind == lane.kind) {
                Some(merged) => merged.points.extend(points),
                None => lanes.push(MidiControllerLane {
                    points: points.collect(),
                    ..lane.clone()
                }),
            }
        }
        sysex.extend(
            sysex_events
                .iter()
                .filter(|event| heard(event.beat))
                .map(|event| MidiSysExEvent {
                    beat: event.beat + shift,
                    ..event.clone()
                }),
        );
        articulations.extend(
            clip_articulations
                .iter()
                .filter(|event| heard(event.beat))
                .map(|event| MidiArticulationEvent {
                    beat: event.beat + shift,
                    ..event.clone()
                }),
        );
    }
    notes.sort_by(|a, b| a.start.total_cmp(&b.start).then(a.pitch.cmp(&b.pitch)));
    for lane in &mut lanes {
        lane.points.sort_by(|a, b| a.beat.total_cmp(&b.beat));
    }
    sysex.sort_by(|a, b| a.beat.total_cmp(&b.beat));
    articulations.sort_by(|a, b| a.beat.total_cmp(&b.beat));

    let mut merged = first.clone();
    merged.start_beat = start;
    merged.duration_beats = (end - start).max(MIN_MIDI_CLIP_BEATS);
    merged.offset_beats = 0.0;
    merged.muted = clips.iter().all(|clip| clip.muted);
    merged.clip_type = ClipType::Midi {
        notes,
        controller_lanes: lanes,
        sysex_events: sysex,
        articulations,
    };
    merged
}

/// `left` and `right` (which starts where `left` ends) as one audio clip,
/// when they are one recording cut apart.
fn glue_audio(left: &ClipState, right: &ClipState) -> Result<ClipState, &'static str> {
    const NOT_ONE_RECORDING: &str =
        "Only audio clips cut from one recording, touching, can be glued — others need a bounce";
    let (
        ClipType::Audio {
            file_id: left_file,
            source_path: left_path,
        },
        ClipType::Audio {
            file_id: right_file,
            source_path: right_path,
        },
    ) = (&left.clip_type, &right.clip_type)
    else {
        return Err(NOT_ONE_RECORDING);
    };
    if left_file.is_empty() || left_file != right_file || left_path != right_path {
        return Err(NOT_ONE_RECORDING);
    }
    let left_end = left.start_beat + left.duration_beats;
    if (right.start_beat - left_end).abs() > SAME_BEAT {
        return Err("Audio clips have to touch to be glued");
    }
    let (a, b) = (&left.stretch, &right.stretch);
    if a.reverse || b.reverse || !a.warp_markers.is_empty() || !b.warp_markers.is_empty() {
        return Err("Reversed or warped audio clips cannot be glued");
    }
    if a.source_end_samples == 0 || a.source_end_samples != b.source_start_samples {
        return Err(NOT_ONE_RECORDING);
    }
    // Everything but the source window and the outer fades has to match, or
    // the glued clip would play one half differently than it did.
    let mut a_rest = a.clone();
    let mut b_rest = b.clone();
    for rest in [&mut a_rest, &mut b_rest] {
        rest.source_start_samples = 0;
        rest.source_end_samples = 0;
        rest.fade_in_ms = 0.0;
        rest.fade_out_ms = 0.0;
        rest.clip_timeline_start_beats = 0.0;
        rest.clip_timeline_duration_beats = 0.0;
        rest.dirty = false;
    }
    if a_rest != b_rest || left.gain != right.gain || left.muted != right.muted {
        return Err("Audio clips with different gain or processing cannot be glued");
    }

    let mut merged = left.clone();
    merged.duration_beats = left.duration_beats + right.duration_beats;
    merged
        .stretch
        .apply_trim(a.source_start_samples, b.source_end_samples);
    merged.stretch.fade_in_ms = a.fade_in_ms;
    merged.stretch.fade_out_ms = b.fade_out_ms;
    Ok(merged)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clip(id: &str, start_beat: f32, duration_beats: f32, clip_type: ClipType) -> ClipState {
        ClipState {
            id: id.to_string(),
            name: id.to_string(),
            start_beat,
            duration_beats,
            source_duration_seconds: None,
            offset_beats: 0.0,
            gain: 1.0,
            clip_type,
            muted: false,
            audio_import: AudioImportState::Pending,
            stretch: AudioClipStretchState::default(),
        }
    }

    fn midi(id: &str, start_beat: f32, duration_beats: f32, notes: &[(u8, f32, f32)]) -> ClipState {
        clip(
            id,
            start_beat,
            duration_beats,
            ClipType::Midi {
                notes: notes
                    .iter()
                    .map(|&(pitch, start, length)| MidiNoteState::new(pitch, start, length, 100))
                    .collect(),
                controller_lanes: Vec::new(),
                sysex_events: Vec::new(),
                articulations: Vec::new(),
            },
        )
    }

    fn take(id: &str, start_beat: f32, file: &str) -> ClipState {
        let mut take = clip(
            id,
            start_beat,
            4.0,
            ClipType::Audio {
                file_id: file.to_string(),
                source_path: Some(format!("/proj/{file}.wav")),
            },
        );
        take.stretch.original_sample_rate = 48_000;
        take.stretch.project_sample_rate = 48_000;
        take.stretch.original_duration_samples = 192_000;
        take.stretch.source_end_samples = 192_000;
        take
    }

    fn state_with(clips: Vec<ClipState>) -> TimelineState {
        let mut state = TimelineState::default();
        state.tracks.clear();
        state.create_midi_track();
        state.tracks[0].clips = clips.into();
        state
    }

    fn ids(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    fn notes(clip: &ClipState) -> Vec<(u8, f32, f32)> {
        let ClipType::Midi { notes, .. } = &clip.clip_type else {
            panic!("a MIDI clip");
        };
        notes
            .iter()
            .map(|n| (n.pitch, n.start, n.duration))
            .collect()
    }

    /// Two MIDI clips with a gap become one clip from the first's start to
    /// the last's end; each note keeps the time it played at, and a note the
    /// first clip's end cut short stays cut.
    #[test]
    fn midi_clips_merge_into_one_that_plays_the_same_notes() {
        let mut state = state_with(vec![
            midi(
                "clip-1",
                4.0,
                4.0,
                &[(60, 0.0, 1.0), (64, 3.0, 2.0), (67, 4.5, 1.0)],
            ),
            midi("clip-2", 12.0, 4.0, &[(72, 1.0, 1.0)]),
        ]);
        let plans = state
            .plan_clip_merge(&ids(&["clip-2", "clip-1"]))
            .expect("two MIDI clips merge");
        assert_eq!(plans.len(), 1);
        let plan = &plans[0];
        assert_eq!(plan.merged.id, "clip-1", "the earliest clip's id is kept");
        assert_eq!(
            (plan.merged.start_beat, plan.merged.duration_beats),
            (4.0, 12.0)
        );
        // 67 started past clip-1's end and was never heard; 64 was cut at 4.
        assert_eq!(
            notes(&plan.merged),
            vec![(60, 0.0, 1.0), (64, 3.0, 1.0), (72, 9.0, 1.0)]
        );

        assert!(state.apply_clip_merge(plan));
        let clips = &state.tracks[0].clips;
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0].id, "clip-1");
    }

    /// The Glue tool joins a clip with the next one after it, not one before.
    #[test]
    fn glue_joins_with_the_next_clip_on_the_track() {
        let state = state_with(vec![
            midi("clip-1", 0.0, 4.0, &[(60, 0.0, 1.0)]),
            midi("clip-2", 4.0, 4.0, &[(62, 0.0, 1.0)]),
            midi("clip-3", 8.0, 4.0, &[(64, 0.0, 1.0)]),
        ]);
        let plan = state
            .plan_glue_with_next("clip-2")
            .expect("a clip after it");
        assert_eq!(plan.replaced, ids(&["clip-2", "clip-3"]));
        assert_eq!(notes(&plan.merged), vec![(62, 0.0, 1.0), (64, 4.0, 1.0)]);
        assert!(
            state.plan_glue_with_next("clip-3").is_err(),
            "nothing after the last"
        );
    }

    /// The two halves of a split glue back into the clip they came from.
    #[test]
    fn split_audio_halves_glue_back_into_one_take() {
        let whole = take("clip-1", 0.0, "vocal");
        let state = state_with(vec![whole.clone()]);
        let (left, right) = state.plan_audio_clip_split(&whole, 2.0).expect("a split");
        let state = state_with(vec![left.clone(), right.clone()]);
        let plans = state
            .plan_clip_merge(&ids(&[&left.id, &right.id]))
            .expect("the halves glue");
        let merged = &plans[0].merged;
        assert_eq!((merged.start_beat, merged.duration_beats), (0.0, 4.0));
        assert_eq!(
            merged.stretch.source_start_samples,
            whole.stretch.source_start_samples
        );
        assert_eq!(
            merged.stretch.source_end_samples,
            whole.stretch.source_end_samples
        );
    }

    /// Audio from two files would need a bounce, so it is refused with a
    /// reason; so is one clip, and a MIDI clip with an audio clip.
    #[test]
    fn what_cannot_be_merged_says_why() {
        let state = state_with(vec![take("clip-1", 0.0, "a"), take("clip-2", 4.0, "b")]);
        assert!(state.plan_clip_merge(&ids(&["clip-1", "clip-2"])).is_err());
        assert!(state.plan_clip_merge(&ids(&["clip-1"])).is_err());
        let mixed = state_with(vec![
            take("clip-1", 0.0, "a"),
            midi("clip-2", 4.0, 4.0, &[(60, 0.0, 1.0)]),
        ]);
        assert!(mixed.plan_clip_merge(&ids(&["clip-1", "clip-2"])).is_err());
    }
}

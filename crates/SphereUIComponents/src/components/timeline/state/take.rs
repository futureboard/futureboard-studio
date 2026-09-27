use super::*;

/// One recorded pass on a track.
///
/// A take is not a second copy of the audio — it *is* an arrangement clip, plus
/// the record of which pass produced it. Keeping the clip as the storage is
/// what makes an inactive take editable, movable and exportable like anything
/// else the moment it is made active again, instead of a special object that
/// only the take list understands.
///
/// Takes that overlap each other are alternates of the same performance: making
/// one active mutes the others it collides with, which is the whole of comping
/// at this level. Takes that do not overlap are simply different parts of the
/// track and all stay active.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackTake {
    pub id: String,
    /// Shown in the take list. Defaults to `Take N`, renameable.
    pub name: String,
    /// The arrangement clip this take produced. A take whose clip has been
    /// deleted is pruned — see [`TimelineState::prune_orphaned_takes`].
    pub clip_id: String,
    /// Whether this take is the one heard. Exactly one of a set of overlapping
    /// takes is active at a time.
    pub active: bool,
    /// Local timestamp of the pass, for the take row's secondary line. Stored
    /// as text because it is a label, never a value anything computes with.
    pub recorded_at: String,
}

impl TrackTake {
    pub fn new(id: impl Into<String>, name: impl Into<String>, clip_id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            clip_id: clip_id.into(),
            active: true,
            recorded_at: String::new(),
        }
    }
}

impl TrackState {
    /// Take rows the header should draw, newest first — the order a player
    /// looks for the pass they just did.
    pub fn takes_newest_first(&self) -> impl Iterator<Item = &TrackTake> {
        self.takes.iter().rev()
    }

    pub fn take(&self, take_id: &str) -> Option<&TrackTake> {
        self.takes.iter().find(|take| take.id == take_id)
    }

    /// How many takes overlap the busiest point on this track — the number the
    /// header's "Takes" badge shows. `1` means every take is its own region and
    /// nothing is being comped.
    pub fn take_stack_depth(&self) -> usize {
        let mut depth = 0usize;
        for take in &self.takes {
            let Some(bounds) = self.take_bounds(take) else {
                continue;
            };
            let overlapping = self
                .takes
                .iter()
                .filter(|other| {
                    self.take_bounds(other)
                        .is_some_and(|other_bounds| ranges_overlap(bounds, other_bounds))
                })
                .count();
            depth = depth.max(overlapping);
        }
        depth
    }

    fn take_bounds(&self, take: &TrackTake) -> Option<(f32, f32)> {
        let clip = self.clips.iter().find(|clip| clip.id == take.clip_id)?;
        Some((
            clip.start_beat,
            clip.start_beat + clip.duration_beats.max(0.0),
        ))
    }
}

/// Whether two half-open beat ranges share any time at all.
fn ranges_overlap(a: (f32, f32), b: (f32, f32)) -> bool {
    a.0 < b.1 && b.0 < a.1
}

/// The loop a recording ran under, in beats: each time the transport wraps
/// from its end back to its start, the performance starts a new take.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RecordLoop {
    pub start: f32,
    pub end: f32,
}

impl RecordLoop {
    /// The loop, when it is on and has room for a take.
    pub fn of(enabled: bool, start: f32, end: f32) -> Option<Self> {
        (enabled && end - start > f32::EPSILON).then_some(Self { start, end })
    }

    fn len(self) -> f32 {
        self.end - self.start
    }
}

/// A pass shorter than this at the end of a loop recording (the moment
/// between the last wrap and Stop) is not kept as a take.
const MIN_PASS_SECONDS: f64 = 0.25;

/// One pass of a recorded audio file: where it lands and which part of the
/// file it plays.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RecordedPass {
    pub start_beat: f32,
    pub source_start_seconds: f64,
    pub source_end_seconds: f64,
    /// It ran the whole loop, rather than being cut short by Stop.
    pub complete: bool,
}

impl TimelineState {
    /// Splits one continuous recorded file into the passes a loop recording
    /// made. The recorder writes straight through every wrap, so the file is
    /// the passes end to end: the first from where recording started to the
    /// loop end, then one whole loop each, then what was played before Stop.
    /// `placed_beat` is where the file's first frame lands (latency already
    /// taken off), and pass boundaries are measured from it in real time,
    /// through the tempo map. Without a loop, or when recording started past
    /// its end, the file is one pass.
    pub fn audio_record_passes(
        &self,
        placed_beat: f32,
        duration_seconds: f64,
        record_loop: Option<RecordLoop>,
    ) -> Vec<RecordedPass> {
        let whole = RecordedPass {
            start_beat: placed_beat,
            source_start_seconds: 0.0,
            source_end_seconds: duration_seconds,
            complete: true,
        };
        let Some(record_loop) = record_loop.filter(|lp| placed_beat < lp.end) else {
            return vec![whole];
        };
        let tempo = self.tempo_lookup();
        let placed_s = tempo.seconds_at_beat(placed_beat as f64);
        let loop_start_s = tempo.seconds_at_beat(record_loop.start as f64);
        let loop_end_s = tempo.seconds_at_beat(record_loop.end as f64);
        let loop_len = loop_end_s - loop_start_s;
        let first_end = loop_end_s - placed_s;
        if loop_len <= 0.0 || duration_seconds <= first_end + MIN_PASS_SECONDS {
            return vec![whole];
        }
        let mut passes = vec![RecordedPass {
            source_end_seconds: first_end,
            ..whole
        }];
        let mut start = first_end;
        while duration_seconds - start >= MIN_PASS_SECONDS {
            let end = (start + loop_len).min(duration_seconds);
            passes.push(RecordedPass {
                start_beat: record_loop.start,
                source_start_seconds: start,
                source_end_seconds: end,
                complete: end - start >= loop_len - 1e-6,
            });
            start = end;
        }
        passes
    }
}

/// Unrolls a transport position that wraps around `record_loop` into one
/// that keeps counting, so notes from every pass of a loop recording can be
/// told apart. `last` and `wraps` are the recorder's running state: a
/// position more than half a loop behind the last one is a wrap. Events can
/// arrive a little out of order; a jump that large cannot be one.
pub fn unwrap_record_beat(
    beat: f32,
    record_loop: Option<RecordLoop>,
    last: &mut f32,
    wraps: &mut u32,
) -> f32 {
    let Some(record_loop) = record_loop else {
        return beat;
    };
    if beat < *last - record_loop.len() * 0.5 {
        *wraps += 1;
    }
    *last = beat;
    beat + *wraps as f32 * record_loop.len()
}

/// One pass of a MIDI loop recording: where its clip starts, how long it is,
/// and its notes relative to that start.
#[derive(Debug, Clone, PartialEq)]
pub struct MidiRecordedPass {
    pub start_beat: f32,
    pub duration_beats: f32,
    pub notes: Vec<MidiNoteState>,
    pub complete: bool,
}

/// Splits a MIDI recording made over a loop into its passes. `notes` are
/// relative to `start_beat` on the unrolled clock ([`unwrap_record_beat`]),
/// and `end` is where recording stopped on it. A note belongs to the pass it
/// starts in, and is cut at the end of that pass; a pass with no notes is no
/// take. Without a loop, or when recording started past its end, it is all
/// one pass.
pub fn midi_record_passes(
    start_beat: f32,
    notes: Vec<MidiNoteState>,
    end: f32,
    record_loop: Option<RecordLoop>,
) -> Vec<MidiRecordedPass> {
    let note_end = notes
        .iter()
        .map(|note| note.start + note.duration)
        .fold(0.0_f32, f32::max);
    let whole = |notes| MidiRecordedPass {
        start_beat,
        duration_beats: end.max(note_end).max(MIN_NOTE_BEATS),
        notes,
        complete: true,
    };
    let Some(record_loop) = record_loop.filter(|lp| start_beat < lp.end) else {
        return vec![whole(notes)];
    };
    let first_end = record_loop.end - start_beat;
    if end <= first_end {
        return vec![whole(notes)];
    }
    // Pass boundaries on the unrolled clock, relative to `start_beat`, with
    // where each pass lands.
    let mut bounds = vec![(0.0, first_end, start_beat, true)];
    let mut from = first_end;
    while from < end {
        let to = from + record_loop.len();
        bounds.push((from, to, record_loop.start, to <= end + 1e-4));
        from = to;
    }
    bounds
        .into_iter()
        .filter_map(|(from, to, place, complete)| {
            let notes: Vec<MidiNoteState> = notes
                .iter()
                .filter(|note| note.start >= from && note.start < to)
                .map(|note| MidiNoteState {
                    start: note.start - from,
                    duration: note.duration.min(to - note.start).max(MIN_NOTE_BEATS),
                    ..note.clone()
                })
                .collect();
            if notes.is_empty() {
                return None;
            }
            let length = if complete {
                to - from
            } else {
                (end - from).max(
                    notes
                        .iter()
                        .map(|note| note.start + note.duration)
                        .fold(0.0_f32, f32::max),
                )
            };
            Some(MidiRecordedPass {
                start_beat: place,
                duration_beats: length.max(MIN_NOTE_BEATS),
                notes,
                complete,
            })
        })
        .collect()
}

impl TimelineState {
    /// Register the clip a recording pass just produced as a take.
    ///
    /// The new take is the active one, and every take it overlaps becomes an
    /// alternate — which is what a second pass over the same bars means. A pass
    /// somewhere else on the track collides with nothing and leaves the rest
    /// alone.
    ///
    /// Returns the new take's id, or `None` when the track or clip is gone.
    pub fn register_recorded_take(
        &mut self,
        track_id: &str,
        clip_id: &str,
        recorded_at: String,
    ) -> Option<String> {
        let take_id = self.next_take_id(track_id);
        let track = self.tracks.iter_mut().find(|track| track.id == track_id)?;
        if !track.clips.iter().any(|clip| clip.id == clip_id) {
            return None;
        }
        let name = format!("Take {}", track.takes.len() + 1);
        track.takes.push(TrackTake {
            id: take_id.clone(),
            name,
            clip_id: clip_id.to_string(),
            active: true,
            recorded_at,
        });
        // A track that has never been comped shows no take lane; the second
        // overlapping pass is what makes one worth opening.
        if track.take_stack_depth() > 1 {
            track.takes_expanded = true;
        }
        self.set_active_take(track_id, &take_id);
        Some(take_id)
    }

    /// Register the clips one recording made on `track_id`, in pass order, as
    /// takes: one per pass of a loop recording, or the single clip of a
    /// straight one. The last pass that ran the whole loop is the one heard —
    /// a pass cut short by Stop is kept, but would otherwise silence the full
    /// pass before it. More than one pass opens the take lanes.
    ///
    /// Returns the new take ids, in pass order.
    pub fn register_recorded_passes(
        &mut self,
        track_id: &str,
        passes: &[(String, bool)],
        recorded_at: &str,
    ) -> Vec<String> {
        let taken: Vec<(String, bool)> = passes
            .iter()
            .filter_map(|(clip_id, complete)| {
                self.register_recorded_take(track_id, clip_id, recorded_at.to_string())
                    .map(|take_id| (take_id, *complete))
            })
            .collect();
        if let Some((heard, _)) = taken.iter().rev().find(|(_, complete)| *complete) {
            let heard = heard.clone();
            self.set_active_take(track_id, &heard);
        }
        if taken.len() > 1 {
            if let Some(track) = self.tracks.iter_mut().find(|track| track.id == track_id) {
                track.takes_expanded = true;
            }
        }
        taken.into_iter().map(|(take_id, _)| take_id).collect()
    }

    /// Make `take_id` the take that is heard, muting every take it overlaps and
    /// unmuting itself.
    ///
    /// Mute is the mechanism because it is the one the engine, the export and
    /// the mixer already agree on: an inactive take is a muted clip, not a clip
    /// in a parallel universe the renderer has to learn about.
    ///
    /// Returns `true` when anything changed.
    pub fn set_active_take(&mut self, track_id: &str, take_id: &str) -> bool {
        let Some(track) = self.tracks.iter_mut().find(|track| track.id == track_id) else {
            return false;
        };
        let Some(target) = track.take(take_id).cloned() else {
            return false;
        };
        let Some(target_bounds) = track
            .clips
            .iter()
            .find(|clip| clip.id == target.clip_id)
            .map(|clip| {
                (
                    clip.start_beat,
                    clip.start_beat + clip.duration_beats.max(0.0),
                )
            })
        else {
            return false;
        };

        // Resolve the collision set first: the mutable walk below cannot also
        // be reading clip bounds off the same track.
        let colliding: Vec<(String, String)> = track
            .takes
            .iter()
            .filter(|take| take.id != target.id)
            .filter_map(|take| {
                let clip = track.clips.iter().find(|clip| clip.id == take.clip_id)?;
                let bounds = (
                    clip.start_beat,
                    clip.start_beat + clip.duration_beats.max(0.0),
                );
                ranges_overlap(target_bounds, bounds)
                    .then(|| (take.id.clone(), take.clip_id.clone()))
            })
            .collect();

        let mut changed = false;
        for take in &mut track.takes {
            let should_be_active =
                take.id == target.id || !colliding.iter().any(|(id, _)| *id == take.id);
            if take.active != should_be_active {
                take.active = should_be_active;
                changed = true;
            }
        }
        let muted_clips: Vec<String> = colliding.into_iter().map(|(_, clip)| clip).collect();
        for clip in &mut track.clips {
            let should_be_muted = muted_clips.iter().any(|id| id == &clip.id);
            // Only clips this take list owns: a clip that is not a take keeps
            // whatever mute the user gave it.
            let is_take_clip = track.takes.iter().any(|take| take.clip_id == clip.id);
            if !is_take_clip {
                continue;
            }
            if clip.muted != should_be_muted {
                clip.muted = should_be_muted;
                changed = true;
            }
        }
        changed
    }

    /// Delete a take *and the clip it holds*. There is nothing else it is.
    pub fn delete_take(&mut self, track_id: &str, take_id: &str) -> bool {
        let Some(track) = self.tracks.iter_mut().find(|track| track.id == track_id) else {
            return false;
        };
        let Some(index) = track.takes.iter().position(|take| take.id == take_id) else {
            return false;
        };
        let removed = track.takes.remove(index);
        track.clips.retain(|clip| clip.id != removed.clip_id);
        if track.takes.is_empty() {
            track.takes_expanded = false;
        }
        // Deleting the take that was heard leaves its slot silent, so promote
        // the most recent surviving take that covered the same ground.
        if removed.active {
            if let Some(next) = track.takes.last().map(|take| take.id.clone()) {
                self.set_active_take(track_id, &next);
            }
        }
        true
    }

    pub fn rename_take(&mut self, track_id: &str, take_id: &str, name: String) -> bool {
        let name = name.trim().to_string();
        if name.is_empty() {
            return false;
        }
        let Some(track) = self.tracks.iter_mut().find(|track| track.id == track_id) else {
            return false;
        };
        let Some(take) = track.takes.iter_mut().find(|take| take.id == take_id) else {
            return false;
        };
        if take.name == name {
            return false;
        }
        take.name = name;
        true
    }

    pub fn toggle_takes_expanded(&mut self, track_id: &str) -> bool {
        let Some(track) = self.tracks.iter_mut().find(|track| track.id == track_id) else {
            return false;
        };
        track.takes_expanded = !track.takes_expanded;
        true
    }

    /// Drop takes whose clip no longer exists.
    ///
    /// A take is a pointer to a clip, and a clip can be deleted from the
    /// arrangement like any other. Called after edits that remove clips, so the
    /// take list never offers a row that would do nothing.
    pub fn prune_orphaned_takes(&mut self) -> bool {
        let mut changed = false;
        for track in &mut self.tracks {
            let before = track.takes.len();
            track
                .takes
                .retain(|take| track.clips.iter().any(|clip| clip.id == take.clip_id));
            if track.takes.len() != before {
                changed = true;
            }
            if track.takes.is_empty() && track.takes_expanded {
                track.takes_expanded = false;
            }
        }
        changed
    }

    /// An id no take on `track_id` is using.
    fn next_take_id(&self, track_id: &str) -> String {
        let used: Vec<&str> = self
            .tracks
            .iter()
            .find(|track| track.id == track_id)
            .map(|track| track.takes.iter().map(|take| take.id.as_str()).collect())
            .unwrap_or_default();
        let mut n = used.len() as u32 + 1;
        loop {
            let candidate = format!("{track_id}-take-{n}");
            if !used.iter().any(|id| *id == candidate) {
                return candidate;
            }
            n += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track_with_clips(bounds: &[(f32, f32)]) -> TimelineState {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let track_id = state.create_audio_track();
        let track = state
            .tracks
            .iter_mut()
            .find(|track| track.id == track_id)
            .expect("track");
        for (index, (start, len)) in bounds.iter().enumerate() {
            track.clips.push(ClipState {
                id: format!("clip-{index}"),
                name: format!("Clip {index}"),
                start_beat: *start,
                duration_beats: *len,
                source_duration_seconds: None,
                offset_beats: 0.0,
                gain: 1.0,
                clip_type: ClipType::Audio {
                    file_id: String::new(),
                    source_path: None,
                },
                muted: false,
                audio_import: AudioImportState::Ready,
                stretch: AudioClipStretchState::default(),
            });
        }
        state
    }

    fn track_id(state: &TimelineState) -> String {
        state.tracks[0].id.clone()
    }

    /// The second pass over the same bars is an alternate, not a layer. Playing
    /// both at once is the failure this exists to prevent.
    #[test]
    fn a_second_overlapping_take_mutes_the_first() {
        let mut state = track_with_clips(&[(0.0, 8.0), (0.0, 8.0)]);
        let id = track_id(&state);
        let first = state
            .register_recorded_take(&id, "clip-0", String::new())
            .expect("first take");
        let second = state
            .register_recorded_take(&id, "clip-1", String::new())
            .expect("second take");

        let track = &state.tracks[0];
        assert!(!track.take(&first).unwrap().active);
        assert!(track.take(&second).unwrap().active);
        assert!(track.clips[0].muted, "the earlier take is still audible");
        assert!(!track.clips[1].muted);
    }

    /// Two passes on different parts of the track are not alternates of each
    /// other, and muting one because the other arrived would silence a part of
    /// the arrangement nobody asked to replace.
    #[test]
    fn takes_that_do_not_overlap_all_stay_active() {
        let mut state = track_with_clips(&[(0.0, 4.0), (8.0, 4.0)]);
        let id = track_id(&state);
        let first = state
            .register_recorded_take(&id, "clip-0", String::new())
            .unwrap();
        let second = state
            .register_recorded_take(&id, "clip-1", String::new())
            .unwrap();

        let track = &state.tracks[0];
        assert!(track.take(&first).unwrap().active);
        assert!(track.take(&second).unwrap().active);
        assert!(!track.clips[0].muted);
        assert!(!track.clips[1].muted);
        assert_eq!(track.take_stack_depth(), 1, "nothing is being comped");
    }

    #[test]
    fn choosing_an_earlier_take_puts_it_back_and_mutes_the_later_one() {
        let mut state = track_with_clips(&[(0.0, 8.0), (0.0, 8.0)]);
        let id = track_id(&state);
        let first = state
            .register_recorded_take(&id, "clip-0", String::new())
            .unwrap();
        let second = state
            .register_recorded_take(&id, "clip-1", String::new())
            .unwrap();

        assert!(state.set_active_take(&id, &first));
        let track = &state.tracks[0];
        assert!(track.take(&first).unwrap().active);
        assert!(!track.take(&second).unwrap().active);
        assert!(!track.clips[0].muted);
        assert!(track.clips[1].muted);
    }

    /// A take *is* its clip, so deleting one deletes the audio from the
    /// arrangement — and the slot it leaves must not stay silent.
    #[test]
    fn deleting_the_active_take_removes_its_clip_and_promotes_another() {
        let mut state = track_with_clips(&[(0.0, 8.0), (0.0, 8.0)]);
        let id = track_id(&state);
        let first = state
            .register_recorded_take(&id, "clip-0", String::new())
            .unwrap();
        let second = state
            .register_recorded_take(&id, "clip-1", String::new())
            .unwrap();

        assert!(state.delete_take(&id, &second));
        let track = &state.tracks[0];
        assert_eq!(track.takes.len(), 1);
        assert_eq!(track.clips.len(), 1);
        assert!(track.take(&first).unwrap().active);
        assert!(!track.clips[0].muted, "the surviving take is still muted");
    }

    /// A clip deleted from the arrangement takes its take row with it — a row
    /// that points at nothing would be a control that does nothing.
    #[test]
    fn a_take_whose_clip_was_deleted_is_pruned() {
        let mut state = track_with_clips(&[(0.0, 8.0)]);
        let id = track_id(&state);
        state
            .register_recorded_take(&id, "clip-0", String::new())
            .unwrap();
        state.tracks[0].clips.clear();
        assert!(state.prune_orphaned_takes());
        assert!(state.tracks[0].takes.is_empty());
        assert!(!state.tracks[0].takes_expanded);
    }

    /// The lane opens itself the moment there is something to choose between,
    /// and not before — one take is not a comp.
    #[test]
    fn the_take_lane_opens_on_the_first_real_alternative() {
        let mut state = track_with_clips(&[(0.0, 8.0), (0.0, 8.0)]);
        let id = track_id(&state);
        state
            .register_recorded_take(&id, "clip-0", String::new())
            .unwrap();
        assert!(!state.tracks[0].takes_expanded);
        state
            .register_recorded_take(&id, "clip-1", String::new())
            .unwrap();
        assert!(state.tracks[0].takes_expanded);
        assert_eq!(state.tracks[0].take_stack_depth(), 2);
    }

    #[test]
    fn take_ids_are_unique_per_track() {
        let mut state = track_with_clips(&[(0.0, 4.0), (4.0, 4.0)]);
        let id = track_id(&state);
        let a = state
            .register_recorded_take(&id, "clip-0", String::new())
            .unwrap();
        let b = state
            .register_recorded_take(&id, "clip-1", String::new())
            .unwrap();
        assert_ne!(a, b);
    }

    fn note(start: f32, duration: f32) -> MidiNoteState {
        MidiNoteState {
            start,
            duration,
            ..MidiNoteState::new(60, 0.0, 1.0, 100)
        }
    }

    /// 120 bpm, a loop over beats 4..8 (2 s), recorded from beat 2 (3 s
    /// before the loop end) through two whole passes and a short third one.
    #[test]
    fn a_loop_recording_file_splits_into_one_pass_per_wrap() {
        let mut state = TimelineState::default();
        state.bpm = 120.0;
        state.tempo_map.points.clear();
        let record_loop = RecordLoop::of(true, 4.0, 8.0);
        // Beat 2 to the loop end is 3 s; then 2 s a pass; 1 s of a third.
        let passes = state.audio_record_passes(2.0, 3.0 + 2.0 + 2.0 + 1.0, record_loop);
        assert_eq!(passes.len(), 4);
        assert_eq!(passes[0].start_beat, 2.0);
        assert!((passes[0].source_end_seconds - 3.0).abs() < 1e-6);
        for pass in &passes[1..] {
            assert_eq!(pass.start_beat, 4.0);
        }
        assert!((passes[1].source_start_seconds - 3.0).abs() < 1e-6);
        assert!((passes[2].source_end_seconds - 7.0).abs() < 1e-6);
        assert!(passes[1].complete && passes[2].complete);
        assert!(!passes[3].complete);

        // Without a loop, or started past it, the file is one pass.
        assert_eq!(state.audio_record_passes(2.0, 8.0, None).len(), 1);
        assert_eq!(state.audio_record_passes(9.0, 8.0, record_loop).len(), 1);
        // Stopped before the first wrap: one pass.
        assert_eq!(state.audio_record_passes(2.0, 2.5, record_loop).len(), 1);
    }

    #[test]
    fn the_last_complete_loop_pass_is_the_one_heard() {
        let mut state = track_with_clips(&[(4.0, 4.0), (4.0, 4.0), (4.0, 1.0)]);
        let id = track_id(&state);
        let passes = [
            ("clip-0".to_string(), true),
            ("clip-1".to_string(), true),
            ("clip-2".to_string(), false),
        ];
        let takes = state.register_recorded_passes(&id, &passes, "12:00");
        assert_eq!(takes.len(), 3);
        let track = &state.tracks[0];
        let heard: Vec<bool> = track.takes.iter().map(|take| take.active).collect();
        assert_eq!(heard, [false, true, false]);
        assert!(track.clips[2].muted && track.clips[0].muted && !track.clips[1].muted);
        assert!(track.takes_expanded);
    }

    #[test]
    fn midi_notes_of_each_wrap_become_their_own_pass() {
        let record_loop = RecordLoop::of(true, 4.0, 8.0);
        // The recorder's clock, fed the transport as it wraps: 6, 7.5, then
        // 4.5 after the wrap (pass two), 4.8 after the next.
        let (mut last, mut wraps) = (4.0, 0);
        let mut unroll = |beat| unwrap_record_beat(beat, record_loop, &mut last, &mut wraps) - 4.0;
        let a = unroll(6.0);
        let b = unroll(7.5);
        let c = unroll(4.5);
        let d = unroll(7.0);
        let e = unroll(4.8);
        assert_eq!((a, b, c, d), (2.0, 3.5, 4.5, 7.0));
        assert!((e - 8.8).abs() < 1e-5);

        // A note held over the wrap is cut there.
        let notes = vec![note(a, 1.0), note(b, 1.0), note(c, 0.5), note(e, 0.5)];
        let passes = midi_record_passes(4.0, notes, 9.75, record_loop);
        assert_eq!(passes.len(), 3);
        assert!(passes.iter().all(|pass| pass.start_beat == 4.0));
        assert_eq!(passes[0].notes.len(), 2);
        assert!((passes[0].notes[1].duration - 0.5).abs() < 1e-6);
        assert!((passes[1].notes[0].start - 0.5).abs() < 1e-6);
        assert!((passes[2].notes[0].start - 0.8).abs() < 1e-5);
        assert!(passes[0].complete && passes[1].complete && !passes[2].complete);

        // A straight recording keeps its notes where they are.
        let straight = midi_record_passes(4.0, vec![note(9.0, 1.0)], 10.0, None);
        assert_eq!(straight.len(), 1);
        assert_eq!(straight[0].notes[0].start, 9.0);
    }

    /// Split, a take stays a take: each piece is one, and undo gives back
    /// the whole clip as the take it was.
    #[test]
    fn a_split_take_keeps_its_take_through_split_and_undo() {
        use crate::components::edit::{ClipSnapshot, EditCommand};
        let mut state = track_with_clips(&[(0.0, 8.0), (0.0, 8.0)]);
        let id = track_id(&state);
        state.register_recorded_take(&id, "clip-0", String::new());
        state.register_recorded_take(&id, "clip-1", String::new());
        let snapshot = ClipSnapshot::capture(&state, "clip-1").unwrap();
        assert!(snapshot.take.is_some());
        let mut left = snapshot.clip.clone();
        left.id = "left".to_string();
        left.duration_beats = 4.0;
        let mut right = snapshot.clip.clone();
        right.id = "right".to_string();
        right.start_beat = 4.0;
        right.duration_beats = 4.0;
        let command = EditCommand::ReplaceClipWithClips {
            clips: vec![(id.clone(), left), (id.clone(), right)],
            snapshot,
        };
        command.execute(&mut state);
        let pieces: Vec<&str> = state.tracks[0]
            .takes
            .iter()
            .map(|t| t.clip_id.as_str())
            .collect();
        assert_eq!(pieces, ["clip-0", "left", "right"]);
        // Choosing the first take mutes both pieces of the second.
        let first = state.tracks[0].takes[0].id.clone();
        state.set_active_take(&id, &first);
        assert!(state.tracks[0]
            .clips
            .iter()
            .filter(|c| c.id != "clip-0")
            .all(|c| c.muted));

        command.undo(&mut state);
        let back: Vec<&str> = state.tracks[0]
            .takes
            .iter()
            .map(|t| t.clip_id.as_str())
            .collect();
        assert_eq!(back, ["clip-0", "clip-1"]);
    }

    #[test]
    fn a_take_moved_to_another_track_is_a_plain_clip_there() {
        let mut state = track_with_clips(&[(0.0, 8.0)]);
        let id = track_id(&state);
        let other = state.create_audio_track();
        state.register_recorded_take(&id, "clip-0", String::new());
        state.move_clip_to_track_with_options("clip-0", &other, 0.0, false);
        assert!(state.tracks[0].takes.is_empty());
        assert!(state.tracks[1].takes.is_empty());
    }
}

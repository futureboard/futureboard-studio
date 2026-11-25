//! Clips cut at the edges of an arrangement time range
//! ([`TimelineState::arrangement_range`]): the pure planners the range edits
//! — delete, copy, cut and move — are built from.
//!
//! Nothing here changes the project. The timeline applies the planned pieces
//! as one undo step (see `Timeline::delete_arrangement_range`).

use super::*;

/// A range edge this close (beats) to a clip's own edge is that edge: a range
/// never cuts off, or leaves behind, a sliver of a clip.
pub const RANGE_EDGE_BEATS: f32 = 1.0e-3;

/// What either side of a range cut keeps at least. Smaller than
/// [`RANGE_EDGE_BEATS`], so a cut [`TimelineState::plan_clip_range_pieces`]
/// has decided to make is never refused.
const RANGE_SPLIT_MIN_BEATS: f32 = RANGE_EDGE_BEATS * 0.5;

/// One clip cut at a range's edges: the part before the range, the part
/// inside it and the part after it. `left` / `right` are `None` where the
/// range reaches past the clip's edge.
#[derive(Debug, Clone, PartialEq)]
pub struct ClipRangePieces {
    pub left: Option<ClipState>,
    pub inside: ClipState,
    pub right: Option<ClipState>,
}

/// A clip a range edit acts on: on a track the range covers, drawn in the
/// track's lane, and overlapping the range by more than [`RANGE_EDGE_BEATS`].
#[derive(Debug, Clone, PartialEq)]
pub struct RangeClipTarget {
    pub track_id: String,
    /// The clip's index in its track's clip list.
    pub index: usize,
    pub clip_id: String,
}

impl TimelineState {
    /// Build the two abutting clips a split of the MIDI clip `clip` at
    /// `split_beat` (absolute timeline beats) would produce: `(left, right)`.
    /// Pure — the caller records the undoable edit. `None` when `clip` is not
    /// MIDI or `split_beat` lands within [`MIN_MIDI_CLIP_BEATS`] of either
    /// edge.
    ///
    /// Every event goes to the piece it starts in, re-based to that piece's
    /// start. A note that sounds across the cut is cut off there in the left
    /// piece and not repeated in the right one. Controller points, SysEx and
    /// articulation events are partitioned by time the same way. Both pieces
    /// get fresh note and point ids, as any copy of a clip does.
    pub fn plan_midi_clip_split(
        &self,
        clip: &ClipState,
        split_beat: f32,
    ) -> Option<(ClipState, ClipState)> {
        self.plan_midi_clip_split_keeping(clip, split_beat, MIN_MIDI_CLIP_BEATS)
    }

    /// [`Self::plan_midi_clip_split`] with `min_beats` kept either side of the
    /// cut instead of [`MIN_MIDI_CLIP_BEATS`].
    pub fn plan_midi_clip_split_keeping(
        &self,
        clip: &ClipState,
        split_beat: f32,
        min_beats: f32,
    ) -> Option<(ClipState, ClipState)> {
        if !matches!(clip.clip_type, ClipType::Midi { .. }) {
            return None;
        }
        let clip_start = clip.start_beat;
        let clip_end = clip.start_beat + clip.duration_beats;
        if split_beat <= clip_start + min_beats || split_beat >= clip_end - min_beats {
            return None;
        }
        // The cut in clip-local beats, the time base of every MIDI event.
        let cut = split_beat - clip_start;
        let left_id = self.next_clip_id();
        let right_id = self.next_clip_id_after(&left_id);

        let mut left = self.clone_clip_for_insert(clip, left_id, clip.name.clone(), clip_start);
        left.duration_beats = cut;
        if let ClipType::Midi {
            notes,
            controller_lanes,
            sysex_events,
            articulations,
        } = &mut left.clip_type
        {
            notes.retain(|note| note.start < cut);
            for note in notes.iter_mut() {
                note.duration = note.duration.min(cut - note.start).max(MIN_NOTE_BEATS);
            }
            for lane in controller_lanes.iter_mut() {
                lane.points.retain(|point| point.beat < cut);
            }
            sysex_events.retain(|event| event.beat < cut);
            articulations.retain(|event| event.beat < cut);
        }

        let mut right = self.clone_clip_for_insert(
            clip,
            right_id,
            derived_clip_name(&clip.name, "Split"),
            split_beat,
        );
        right.duration_beats = clip_end - split_beat;
        if let ClipType::Midi {
            notes,
            controller_lanes,
            sysex_events,
            articulations,
        } = &mut right.clip_type
        {
            notes.retain(|note| note.start >= cut);
            for note in notes.iter_mut() {
                note.start = (note.start - cut).max(0.0);
            }
            for lane in controller_lanes.iter_mut() {
                lane.points.retain(|point| point.beat >= cut);
                for point in lane.points.iter_mut() {
                    point.beat = (point.beat - cut).max(0.0);
                }
            }
            *sysex_events = std::mem::take(sysex_events)
                .into_iter()
                .filter(|event| event.beat >= cut)
                .map(|event| {
                    let beat = event.beat - cut;
                    event.at_beat(beat)
                })
                .collect();
            articulations.retain(|event| event.beat >= cut);
            for event in articulations.iter_mut() {
                event.beat = (event.beat - cut).max(0.0);
            }
        }
        Some((left, right))
    }

    /// Split an audio or MIDI clip at `split_beat`, keeping `min_beats` either
    /// side; `None` for any other clip or a cut too close to an edge.
    fn plan_clip_split_keeping(
        &self,
        clip: &ClipState,
        split_beat: f32,
        min_beats: f32,
    ) -> Option<(ClipState, ClipState)> {
        match clip.clip_type {
            ClipType::Audio { .. } => {
                self.plan_audio_clip_split_keeping(clip, split_beat, min_beats)
            }
            ClipType::Midi { .. } => self.plan_midi_clip_split_keeping(clip, split_beat, min_beats),
            ClipType::Video { .. } => None,
        }
    }

    /// `clip` cut at `start_beat` and `end_beat`: an ordinary split at each
    /// range edge inside it ([`Self::plan_audio_clip_split_keeping`],
    /// [`Self::plan_midi_clip_split_keeping`]), so every piece plays what it
    /// played before. `None` when the clip is neither audio nor MIDI or
    /// overlaps the range by no more than [`RANGE_EDGE_BEATS`].
    ///
    /// The pieces keep the clip's name and are numbered `clip-{next_id}` on,
    /// advancing `next_id`, so several clips can be planned before any is
    /// inserted. Pure apart from remembering where each piece came from
    /// (`clip_lineage`).
    pub fn plan_clip_range_pieces(
        &self,
        clip: &ClipState,
        start_beat: f32,
        end_beat: f32,
        next_id: &mut u32,
    ) -> Option<ClipRangePieces> {
        if !matches!(
            clip.clip_type,
            ClipType::Audio { .. } | ClipType::Midi { .. }
        ) {
            return None;
        }
        let clip_start = clip.start_beat;
        let clip_end = clip.start_beat + clip.duration_beats;
        let lo = start_beat.min(end_beat).max(clip_start);
        let hi = start_beat.max(end_beat).min(clip_end);
        if hi - lo <= RANGE_EDGE_BEATS {
            return None;
        }
        let mut rest = clip.clone();
        let mut left = None;
        if lo > clip_start + RANGE_EDGE_BEATS {
            let (before, after) = self.plan_clip_split_keeping(&rest, lo, RANGE_SPLIT_MIN_BEATS)?;
            left = Some(before);
            rest = after;
        }
        let mut right = None;
        if hi < clip_end - RANGE_EDGE_BEATS {
            let (inside, after) = self.plan_clip_split_keeping(&rest, hi, RANGE_SPLIT_MIN_BEATS)?;
            rest = inside;
            right = Some(after);
        }
        let mut pieces = ClipRangePieces {
            left,
            inside: rest,
            right,
        };

        // A split cuts the source window from its start; a reversed clip
        // plays it from its end, so each piece's window is the mirror image
        // within the clip's own.
        if matches!(clip.clip_type, ClipType::Audio { .. }) && clip.stretch.reverse {
            let window_start = clip.stretch.source_start_samples;
            let window_end = if clip.stretch.source_end_samples > window_start {
                clip.stretch.source_end_samples
            } else {
                clip.stretch.original_duration_samples
            };
            if window_end > window_start {
                let mirror = |frame: u64| {
                    (window_start + window_end)
                        .saturating_sub(frame)
                        .clamp(window_start, window_end)
                };
                for piece in pieces.pieces_mut() {
                    let (from, to) = (
                        piece.stretch.source_start_samples,
                        piece.stretch.source_end_samples,
                    );
                    piece.stretch.apply_trim(mirror(to), mirror(from));
                }
            }
        }

        for piece in pieces.pieces_mut() {
            let id = format!("clip-{}", *next_id);
            *next_id += 1;
            self.clip_lineage.record(&id, &clip.id);
            piece.id = id;
            piece.name = clip.name.clone();
        }
        Some(pieces)
    }

    /// The tracks a range edit covers: the range's own, or the selected
    /// track for a range that names none.
    pub fn range_scope_track_ids(&self, range: &TimelineRangeSelection) -> Vec<String> {
        if range.track_ids.is_empty() {
            self.selection.selected_track_id.iter().cloned().collect()
        } else {
            range.track_ids.clone()
        }
    }

    /// The clips a range edit acts on, track by track in arrangement order:
    /// audio and MIDI clips drawn in a covered track's lane (not a hidden
    /// take, not on a frozen track) that overlap the range.
    pub fn range_clip_targets(&self, range: &TimelineRangeSelection) -> Vec<RangeClipTarget> {
        let scope = self.range_scope_track_ids(range);
        let (lo, hi) = range.as_f32_range();
        let mut targets = Vec::new();
        if hi - lo <= RANGE_EDGE_BEATS {
            return targets;
        }
        for track in self
            .tracks
            .iter()
            .filter(|track| scope.contains(&track.id) && !track.is_frozen())
        {
            for (index, clip) in track.clips.iter().enumerate() {
                if !matches!(
                    clip.clip_type,
                    ClipType::Audio { .. } | ClipType::Midi { .. }
                ) || track.is_hidden_take_clip(&clip.id)
                {
                    continue;
                }
                let clip_end = clip.start_beat + clip.duration_beats;
                if clip.start_beat.max(lo) + RANGE_EDGE_BEATS < clip_end.min(hi) {
                    targets.push(RangeClipTarget {
                        track_id: track.id.clone(),
                        index,
                        clip_id: clip.id.clone(),
                    });
                }
            }
        }
        targets
    }

    /// The part of each clip inside `range`, as a copy would take it: `(track,
    /// index of the source clip, piece)`. Changes nothing in the project.
    pub fn plan_range_copy(
        &self,
        range: &TimelineRangeSelection,
    ) -> Vec<(String, usize, ClipState)> {
        let (lo, hi) = range.as_f32_range();
        let mut next_id = next_clip_id_number(&self.tracks);
        self.range_clip_targets(range)
            .into_iter()
            .filter_map(|target| {
                let (_, clip) = self.find_clip(&target.clip_id)?;
                let pieces = self.plan_clip_range_pieces(clip, lo, hi, &mut next_id)?;
                Some((target.track_id, target.index, pieces.inside))
            })
            .collect()
    }
}

impl ClipRangePieces {
    /// Every piece, in time order.
    pub fn pieces_mut(&mut self) -> impl Iterator<Item = &mut ClipState> {
        self.left
            .iter_mut()
            .chain(std::iter::once(&mut self.inside))
            .chain(self.right.iter_mut())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A MIDI clip at beat 4, 8 beats long, with notes at clip beats 0, 1.5
    /// (crossing beat 2), 2 and 6, a controller lane and an articulation.
    fn midi_clip(state: &TimelineState) -> ClipState {
        let mut clip = ClipState {
            id: "clip-1".into(),
            name: "Keys".into(),
            start_beat: 4.0,
            duration_beats: 8.0,
            source_duration_seconds: None,
            offset_beats: 0.0,
            gain: 1.0,
            clip_type: ClipType::Midi {
                notes: vec![
                    MidiNoteState::new(60, 0.0, 1.0, 100),
                    MidiNoteState::new(62, 1.5, 2.0, 90),
                    MidiNoteState::new(64, 2.0, 1.0, 80),
                    MidiNoteState::new(65, 6.0, 1.0, 70),
                ],
                controller_lanes: vec![MidiControllerLane {
                    kind: MidiControllerKind::PitchBend,
                    points: vec![
                        MidiControllerPoint::new(0.5, 0.2),
                        MidiControllerPoint::new(2.0, 0.6),
                        MidiControllerPoint::new(5.0, 0.9),
                    ],
                    visible: true,
                    height: 40.0,
                    collapsed: false,
                }],
                sysex_events: vec![
                    MidiSysExEvent::from_message(3.0, &[0xF0, 0x7E, 0xF7]).expect("sysex")
                ],
                articulations: vec![MidiArticulationEvent::new(1.0, ArticulationId::Staccato)],
            },
            muted: false,
            audio_import: AudioImportState::default(),
            stretch: AudioClipStretchState::default(),
        };
        clip.id = state.next_clip_id();
        clip
    }

    fn notes(clip: &ClipState) -> Vec<(u8, f32, f32)> {
        match &clip.clip_type {
            ClipType::Midi { notes, .. } => notes
                .iter()
                .map(|note| (note.pitch, note.start, note.duration))
                .collect(),
            _ => panic!("not MIDI"),
        }
    }

    #[test]
    fn a_midi_split_gives_each_note_to_the_piece_it_starts_in() {
        let state = TimelineState::default();
        let clip = midi_clip(&state);
        // Clip beat 2 = timeline beat 6.
        let (left, right) = state.plan_midi_clip_split(&clip, 6.0).expect("split");

        assert_eq!((left.start_beat, left.duration_beats), (4.0, 2.0));
        assert_eq!((right.start_beat, right.duration_beats), (6.0, 6.0));
        // The note across the cut is cut off there and not repeated.
        assert_eq!(notes(&left), vec![(60, 0.0, 1.0), (62, 1.5, 0.5)]);
        // A note on the cut starts the right piece; times are re-based.
        assert_eq!(notes(&right), vec![(64, 0.0, 1.0), (65, 4.0, 1.0)]);
        let ClipType::Midi {
            controller_lanes: left_lanes,
            sysex_events: left_sysex,
            articulations: left_arts,
            notes: left_notes,
        } = &left.clip_type
        else {
            unreachable!()
        };
        let ClipType::Midi {
            controller_lanes: right_lanes,
            sysex_events: right_sysex,
            articulations: right_arts,
            notes: right_notes,
        } = &right.clip_type
        else {
            unreachable!()
        };
        let beats = |lane: &MidiControllerLane| -> Vec<f32> {
            lane.points.iter().map(|point| point.beat).collect()
        };
        assert_eq!(beats(&left_lanes[0]), vec![0.5]);
        assert_eq!(beats(&right_lanes[0]), vec![0.0, 3.0]);
        assert!(left_sysex.is_empty());
        assert_eq!(right_sysex.len(), 1);
        assert_eq!(right_sysex[0].beat, 1.0);
        assert_eq!(right_sysex[0].tick, 960);
        assert_eq!(left_arts.len(), 1);
        assert!(right_arts.is_empty());
        // Fresh ids: no note or point is shared with the source or between
        // the pieces.
        let ClipType::Midi {
            notes: source_notes,
            ..
        } = &clip.clip_type
        else {
            unreachable!()
        };
        for note in left_notes.iter().chain(right_notes) {
            assert!(source_notes.iter().all(|source| source.id != note.id));
        }
        assert_ne!(left.id, right.id);
    }

    #[test]
    fn a_midi_split_near_an_edge_or_of_another_clip_is_refused() {
        let state = TimelineState::default();
        let clip = midi_clip(&state);
        assert!(state.plan_midi_clip_split(&clip, 4.05).is_none());
        assert!(state.plan_midi_clip_split(&clip, 11.95).is_none());
        assert!(state.plan_midi_clip_split(&clip, 2.0).is_none());
        let mut audio = clip.clone();
        audio.clip_type = ClipType::Audio {
            file_id: "f".into(),
            source_path: None,
        };
        assert!(state.plan_midi_clip_split(&audio, 6.0).is_none());
    }

    #[test]
    fn range_pieces_cover_the_clip_without_gaps() {
        let state = TimelineState::default();
        let clip = midi_clip(&state);
        let mut next_id = 50;
        let pieces = state
            .plan_clip_range_pieces(&clip, 6.0, 9.0, &mut next_id)
            .expect("pieces");
        let left = pieces.left.as_ref().expect("left");
        let right = pieces.right.as_ref().expect("right");
        assert_eq!((left.start_beat, left.duration_beats), (4.0, 2.0));
        assert_eq!(
            (pieces.inside.start_beat, pieces.inside.duration_beats),
            (6.0, 3.0)
        );
        assert_eq!((right.start_beat, right.duration_beats), (9.0, 3.0));
        assert_eq!(notes(&pieces.inside), vec![(64, 0.0, 1.0)]);
        assert_eq!(notes(right), vec![(65, 1.0, 1.0)]);
        assert_eq!(
            [&left.id, &pieces.inside.id, &right.id],
            ["clip-50", "clip-51", "clip-52"]
        );
        assert_eq!(next_id, 53);
        assert!([left, &pieces.inside, right]
            .iter()
            .all(|piece| piece.name == clip.name));

        // A range past an edge cuts on one side only; one outside the clip
        // gives nothing.
        let pieces = state
            .plan_clip_range_pieces(&clip, 0.0, 8.0, &mut next_id)
            .expect("pieces");
        assert!(pieces.left.is_none());
        assert_eq!(pieces.inside.duration_beats, 4.0);
        assert!(pieces.right.is_some());
        assert!(state
            .plan_clip_range_pieces(&clip, 12.0, 16.0, &mut next_id)
            .is_none());
    }

    #[test]
    fn a_reversed_clips_range_pieces_play_the_mirrored_window() {
        let state = TimelineState::default();
        let mut clip = ClipState {
            id: "clip-1".into(),
            name: "Vox".into(),
            start_beat: 0.0,
            duration_beats: 8.0,
            source_duration_seconds: Some(4.0),
            offset_beats: 0.0,
            gain: 1.0,
            clip_type: ClipType::Audio {
                file_id: "f".into(),
                source_path: Some("vox.wav".into()),
            },
            muted: false,
            audio_import: AudioImportState::Ready,
            stretch: AudioClipStretchState::default(),
        };
        clip.stretch.original_sample_rate = 48_000;
        clip.stretch.project_sample_rate = 48_000;
        clip.stretch.original_duration_samples = 192_000;
        clip.stretch.source_end_samples = 192_000;
        clip.stretch.reverse = true;
        let mut next_id = 2;
        let pieces = state
            .plan_clip_range_pieces(&clip, 2.0, 4.0, &mut next_id)
            .expect("pieces");
        // 120 bpm: 2 beats = 1 s = 48 000 frames. Played backwards, the
        // second beat pair is the second-to-last second of the file.
        let window = |piece: &ClipState| {
            (
                piece.stretch.source_start_samples,
                piece.stretch.source_end_samples,
            )
        };
        assert_eq!(window(pieces.left.as_ref().unwrap()), (144_000, 192_000));
        assert_eq!(window(&pieces.inside), (96_000, 144_000));
        assert_eq!(window(pieces.right.as_ref().unwrap()), (0, 96_000));
    }
}

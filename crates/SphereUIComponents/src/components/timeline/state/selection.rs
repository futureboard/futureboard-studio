use super::*;

#[derive(Debug, Clone, PartialEq)]
pub struct TimelineSelection {
    pub selected_track_id: Option<String>,
    /// Ordered multi-track selection. `selected_track_id` remains the primary
    /// track used by the Inspector and single-target commands.
    pub selected_track_ids: Vec<String>,
    /// Stable anchor used by Shift-click range selection.
    pub track_selection_anchor_id: Option<String>,
    pub selected_clip_ids: Vec<String>,
    /// Shared Song Text selection used by the ruler and all panel/window views.
    pub selected_song_text_event_ids: Vec<SongTextEventId>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TimelineRangeSelection {
    pub start_beat: f64,
    pub end_beat: f64,
    pub track_ids: Vec<String>,
}

impl TimelineRangeSelection {
    pub fn new(start_beat: f64, end_beat: f64, track_ids: Vec<String>) -> Self {
        let (start_beat, end_beat) = if start_beat <= end_beat {
            (start_beat, end_beat)
        } else {
            (end_beat, start_beat)
        };
        Self {
            start_beat,
            end_beat,
            track_ids,
        }
    }

    pub fn as_f32_range(&self) -> (f32, f32) {
        (self.start_beat as f32, self.end_beat as f32)
    }
}

/// An arrangement marquee in arrangement space: `left` / `right` in lane x (the
/// beat transform the clips are drawn with, horizontal scroll included),
/// `top` / `bottom` in content y (0 at the top of the first track row).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MarqueeRect {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

impl MarqueeRect {
    /// The rectangle spanned by two corners, in either order.
    pub fn from_corners(a: (f32, f32), b: (f32, f32)) -> Self {
        Self {
            left: a.0.min(b.0),
            top: a.1.min(b.1),
            right: a.0.max(b.0),
            bottom: a.1.max(b.1),
        }
    }

    fn as_tuple(&self) -> (f32, f32, f32, f32) {
        (self.left, self.top, self.right, self.bottom)
    }
}

/// What an arrangement marquee encloses: the track rows it crosses and the
/// clips whose drawn rectangles it touches, both in arrangement order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MarqueeHits {
    pub track_ids: Vec<String>,
    pub clip_ids: Vec<String>,
}

/// How a left press on a clip changes the clip selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipSelectOp {
    /// Select only this clip.
    Replace,
    /// Add the clip, or take it out when it is already selected.
    Toggle,
}

/// When a press on a clip applies its selection change: at the press, or only
/// on a click (a release without a drag).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipPressPlan {
    pub on_press: Option<ClipSelectOp>,
    pub on_click: Option<ClipSelectOp>,
}

/// Plan a left press on a clip.
///
/// A press on a clip that is already selected leaves the selection alone
/// until the release: dragging it then moves everything selected with it (a
/// whole marquee selection), while a click still narrows the selection to that
/// clip, or with the additive modifier takes it out. A press on a clip that is
/// not selected selects it at once, so the drag that follows moves it.
pub fn clip_press_plan(already_selected: bool, additive: bool) -> ClipPressPlan {
    let op = if additive {
        ClipSelectOp::Toggle
    } else {
        ClipSelectOp::Replace
    };
    if already_selected {
        ClipPressPlan {
            on_press: None,
            on_click: Some(op),
        }
    } else {
        ClipPressPlan {
            on_press: Some(op),
            on_click: None,
        }
    }
}

impl TimelineState {
    /// What `rect` encloses, hit-tested against exactly what is drawn.
    ///
    /// Tracks are the rows the rectangle crosses (a track's automation lanes
    /// count as its row). Clips are those whose drawn rectangle
    /// ([`Self::clip_lane_rect`]) it touches, so a clip narrower than its
    /// minimum drawn width is hit where it is painted, and the pad bands above
    /// and below a clip are lane, not clip. Rows that are not drawn — mixer-only
    /// Bus / Return channels, VSTi multi-out children, children of a collapsed
    /// group — are never hit, so a later "delete selected tracks" cannot reach
    /// a track nobody could see.
    pub fn marquee_hits(&self, rows: &TrackRowLayout, rect: MarqueeRect) -> MarqueeHits {
        let mut hits = MarqueeHits::default();
        let first = rows
            .rows
            .partition_point(|row| row.y + row.block_height() <= rect.top);
        for row in &rows.rows[first..] {
            if row.y >= rect.bottom {
                break;
            }
            if row.height <= 0.0 {
                continue;
            }
            let Some(track) = self.tracks.get(row.index) else {
                continue;
            };
            if is_arrangement_hidden_track(track)
                || !(rect.top < row.y + row.block_height() && rect.bottom > row.y)
            {
                continue;
            }
            hits.track_ids.push(track.id.clone());
            for clip in &track.clips {
                let drawn = self.clip_lane_rect(clip, row.height);
                let drawn = (
                    drawn.left,
                    row.y + drawn.top,
                    drawn.right(),
                    row.y + drawn.bottom(),
                );
                if crate::components::edit::rects_intersect(rect.as_tuple(), drawn) {
                    hits.clip_ids.push(clip.id.clone());
                }
            }
        }
        hits
    }

    pub fn selected_range_track_ids(&self) -> Vec<String> {
        match self.selection.selected_track_id.as_ref() {
            Some(primary)
                if self.selection.selected_track_ids.is_empty()
                    || !self.selection.selected_track_ids.contains(primary) =>
            {
                vec![primary.clone()]
            }
            Some(_) => self.selection.selected_track_ids.clone(),
            None => Vec::new(),
        }
    }

    pub fn track_ids_between(&self, a: &str, b: &str) -> Vec<String> {
        let Some(a_index) = self.tracks.iter().position(|track| track.id == a) else {
            return Vec::new();
        };
        let Some(b_index) = self.tracks.iter().position(|track| track.id == b) else {
            return vec![a.to_string()];
        };
        let (lo, hi) = if a_index <= b_index {
            (a_index, b_index)
        } else {
            (b_index, a_index)
        };
        self.tracks[lo..=hi]
            .iter()
            .map(|track| track.id.clone())
            .collect()
    }

    /// Commit what a marquee enclosed: the tracks it crossed and the clips it
    /// touched.
    ///
    /// Tracks are part of the result, not a side effect of the clips. A band
    /// pulled across five tracks that leaves four unselected is the gesture
    /// failing, and a band pulled across empty lanes has no clips to hit at all
    /// yet still says exactly which tracks the user meant.
    ///
    /// `anchor` is the track the drag started on. It becomes the primary and
    /// the anchor for a following Shift-click, rather than whichever track
    /// happens to be topmost — extending a selection should continue from where
    /// the cursor actually went down.
    ///
    /// Additive keeps what was already selected and adds to it; replace does
    /// what its name says.
    pub fn apply_marquee_selection(
        &mut self,
        track_ids: &[String],
        clip_ids: Vec<String>,
        anchor: &str,
        additive: bool,
    ) {
        if additive {
            for clip_id in clip_ids {
                if !self.selection.selected_clip_ids.contains(&clip_id) {
                    self.selection.selected_clip_ids.push(clip_id);
                }
            }
            for track_id in track_ids {
                if !self.selection.selected_track_ids.contains(track_id) {
                    self.selection.selected_track_ids.push(track_id.clone());
                }
            }
            if self.selection.selected_track_id.is_none() {
                self.selection.selected_track_id = Some(anchor.to_string());
            }
        } else {
            self.selection.selected_clip_ids = clip_ids;
            self.selection.selected_track_ids = track_ids.to_vec();
            self.selection.selected_track_id = track_ids
                .iter()
                .find(|id| id.as_str() == anchor)
                .cloned()
                .or_else(|| track_ids.first().cloned());
        }
        if !track_ids.is_empty() {
            self.selection.track_selection_anchor_id = Some(anchor.to_string());
        }
    }

    pub fn select_track(&mut self, track_id: &str) {
        self.selection.selected_track_id = Some(track_id.to_string());
        self.selection.selected_track_ids = vec![track_id.to_string()];
        self.selection.track_selection_anchor_id = Some(track_id.to_string());
        self.selection.selected_clip_ids.clear();
        self.selection.selected_song_text_event_ids.clear();
        self.arrangement_range = None;
    }

    pub fn select_track_with_modifiers(&mut self, track_id: &str, additive: bool, range: bool) {
        match self.selection.selected_track_id.clone() {
            Some(primary) if !self.selection.selected_track_ids.contains(&primary) => {
                self.selection.selected_track_ids = vec![primary];
            }
            None => self.selection.selected_track_ids.clear(),
            _ => {}
        }
        if range {
            let anchor = self
                .selection
                .track_selection_anchor_id
                .as_deref()
                .or(self.selection.selected_track_id.as_deref())
                .unwrap_or(track_id)
                .to_string();
            let range_ids = self.track_ids_between(&anchor, track_id);
            if additive {
                for id in range_ids {
                    if !self.selection.selected_track_ids.contains(&id) {
                        self.selection.selected_track_ids.push(id);
                    }
                }
            } else {
                self.selection.selected_track_ids = range_ids;
            }
            self.selection.selected_track_id = Some(track_id.to_string());
            self.selection.track_selection_anchor_id = Some(anchor);
        } else if additive {
            if let Some(index) = self
                .selection
                .selected_track_ids
                .iter()
                .position(|id| id == track_id)
            {
                self.selection.selected_track_ids.remove(index);
                if self.selection.selected_track_id.as_deref() == Some(track_id) {
                    self.selection.selected_track_id =
                        self.selection.selected_track_ids.last().cloned();
                }
            } else {
                self.selection.selected_track_ids.push(track_id.to_string());
                self.selection.selected_track_id = Some(track_id.to_string());
            }
            self.selection.track_selection_anchor_id = self.selection.selected_track_id.clone();
        } else {
            self.selection.selected_track_id = Some(track_id.to_string());
            self.selection.selected_track_ids = vec![track_id.to_string()];
            self.selection.track_selection_anchor_id = Some(track_id.to_string());
        }
        self.selection.selected_clip_ids.clear();
        self.selection.selected_song_text_event_ids.clear();
        self.arrangement_range = None;
    }

    pub fn is_track_selected(&self, track_id: &str) -> bool {
        match self.selection.selected_track_id.as_ref() {
            Some(primary)
                if self.selection.selected_track_ids.is_empty()
                    || !self.selection.selected_track_ids.contains(primary) =>
            {
                primary == track_id
            }
            Some(_) => self
                .selection
                .selected_track_ids
                .iter()
                .any(|id| id == track_id),
            None => false,
        }
    }

    pub fn select_clip(&mut self, clip_id: &str) {
        self.selection.selected_clip_ids = vec![clip_id.to_string()];
        self.selection.selected_song_text_event_ids.clear();
        self.arrangement_range = None;
        if let Some(track) = self
            .tracks
            .iter()
            .find(|t| t.clips.iter().any(|c| c.id == clip_id))
        {
            self.selection.selected_track_id = Some(track.id.clone());
            self.selection.selected_track_ids = vec![track.id.clone()];
            self.selection.track_selection_anchor_id = Some(track.id.clone());
        }
    }

    pub fn select_clip_additive(&mut self, clip_id: &str) {
        self.selection.selected_song_text_event_ids.clear();
        self.arrangement_range = None;
        if let Some(pos) = self
            .selection
            .selected_clip_ids
            .iter()
            .position(|id| id == clip_id)
        {
            self.selection.selected_clip_ids.remove(pos);
        } else {
            self.selection.selected_clip_ids.push(clip_id.to_string());
        }
        if let Some(track) = self
            .tracks
            .iter()
            .find(|t| t.clips.iter().any(|c| c.id == clip_id))
        {
            // Extend the track selection rather than collapse it: an additive
            // click inside a cross-track marquee selection must not throw away
            // the tracks the marquee spanned.
            if !self.selection.selected_track_ids.contains(&track.id) {
                self.selection.selected_track_ids.push(track.id.clone());
            }
            self.selection.selected_track_id = Some(track.id.clone());
            self.selection.track_selection_anchor_id = Some(track.id.clone());
        }
    }

    pub fn select_song_text_event(&mut self, id: &str, additive: bool) {
        self.selection.selected_clip_ids.clear();
        self.arrangement_range = None;
        if additive {
            if let Some(index) = self
                .selection
                .selected_song_text_event_ids
                .iter()
                .position(|selected| selected == id)
            {
                self.selection.selected_song_text_event_ids.remove(index);
            } else {
                self.selection
                    .selected_song_text_event_ids
                    .push(id.to_string());
            }
        } else {
            self.selection.selected_song_text_event_ids = vec![id.to_string()];
        }
    }

    pub fn selected_song_text_event(&self) -> Option<&SongTextEvent> {
        let id = self.selection.selected_song_text_event_ids.first()?;
        self.song_text_event(id)
    }

    pub fn clear_song_text_selection(&mut self) {
        self.selection.selected_song_text_event_ids.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bug this covers: a marquee pulled across several tracks selected the
    /// clips it touched but left the *tracks* alone — only the topmost one came
    /// out selected — so "select across tracks" did not work, and a band across
    /// empty lanes selected nothing at all.
    #[test]
    fn a_marquee_selects_every_track_it_crossed() {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let first = state.create_audio_track();
        let second = state.create_audio_track();
        let third = state.create_audio_track();
        let spanned = vec![first.clone(), second.clone(), third.clone()];

        // Dragged from the middle track outwards, with no clips under the band.
        state.apply_marquee_selection(&spanned, Vec::new(), &second, false);

        assert_eq!(state.selection.selected_track_ids, spanned);
        assert!(
            state.selection.selected_clip_ids.is_empty(),
            "no clips were under the band"
        );
        // The primary and the anchor stay where the drag started, so a Shift
        // click afterwards extends from the cursor rather than from the top.
        assert_eq!(
            state.selection.selected_track_id.as_deref(),
            Some(second.as_str())
        );
        assert_eq!(
            state.selection.track_selection_anchor_id.as_deref(),
            Some(second.as_str())
        );
    }

    #[test]
    fn a_replacing_marquee_drops_what_was_selected_before() {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let first = state.create_audio_track();
        let second = state.create_audio_track();

        state.select_track(&first);
        state.selection.selected_clip_ids = vec!["stale-clip".to_string()];

        state.apply_marquee_selection(
            &[second.clone()],
            vec!["fresh-clip".to_string()],
            &second,
            false,
        );

        assert_eq!(state.selection.selected_track_ids, vec![second.clone()]);
        assert_eq!(
            state.selection.selected_clip_ids,
            vec!["fresh-clip".to_string()]
        );
    }

    /// Ctrl-drag adds to what is already selected — tracks included — and never
    /// lists the same track twice however many times it is crossed.
    #[test]
    fn an_additive_marquee_unions_tracks_without_duplicating_them() {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let first = state.create_audio_track();
        let second = state.create_audio_track();
        let third = state.create_audio_track();

        state.select_track(&first);
        state.apply_marquee_selection(
            &[second.clone(), third.clone()],
            vec!["clip-b".to_string()],
            &second,
            true,
        );
        state.apply_marquee_selection(
            &[first.clone(), second.clone()],
            vec!["clip-b".to_string(), "clip-a".to_string()],
            &first,
            true,
        );

        assert_eq!(
            state.selection.selected_track_ids,
            vec![first.clone(), second.clone(), third.clone()]
        );
        assert_eq!(
            state.selection.selected_clip_ids,
            vec!["clip-b".to_string(), "clip-a".to_string()],
            "a clip already selected must not be listed twice"
        );
        // Additive keeps the primary it had.
        assert_eq!(
            state.selection.selected_track_id.as_deref(),
            Some(first.as_str())
        );
    }

    /// An anchor outside the band (the drag left the arrangement) still has to
    /// produce a primary, or the selection has tracks and no focus.
    #[test]
    fn a_marquee_whose_anchor_is_not_in_the_span_still_has_a_primary() {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let first = state.create_audio_track();
        let second = state.create_audio_track();

        state.apply_marquee_selection(&[second.clone()], Vec::new(), &first, false);

        assert_eq!(
            state.selection.selected_track_id.as_deref(),
            Some(second.as_str())
        );
    }

    #[test]
    fn ctrl_toggles_tracks_and_shift_selects_anchor_range() {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let first = state.create_audio_track();
        let second = state.create_audio_track();
        let third = state.create_audio_track();

        state.select_track(&first);
        state.select_track_with_modifiers(&third, true, false);
        assert_eq!(
            state.selection.selected_track_ids,
            vec![first.clone(), third.clone()]
        );

        state.select_track_with_modifiers(&first, true, false);
        assert_eq!(state.selection.selected_track_ids, vec![third.clone()]);

        state.select_track(&first);
        state.select_track_with_modifiers(&third, false, true);
        assert_eq!(
            state.selection.selected_track_ids,
            vec![first, second, third]
        );
    }

    /// `count` MIDI tracks, each holding one clip from beat 2 to beat 6.
    fn stacked_midi_clips(count: usize) -> (TimelineState, Vec<String>, Vec<String>) {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let mut track_ids = Vec::new();
        let mut clip_ids = Vec::new();
        for _ in 0..count {
            let track_id = state.create_midi_track();
            let clip = state.build_midi_clip(&track_id, 2.0, 4.0).expect("clip");
            clip_ids.push(clip.id.clone());
            state
                .tracks
                .iter_mut()
                .find(|track| track.id == track_id)
                .expect("track")
                .clips
                .push(clip);
            track_ids.push(track_id);
        }
        (state, track_ids, clip_ids)
    }

    fn hits(state: &TimelineState, rect: MarqueeRect) -> MarqueeHits {
        state.marquee_hits(&state.track_row_layout(), rect)
    }

    /// A vertical-only drag through a column of clips selects every one of
    /// them and every track it crossed; the old row-quantized marquee needed
    /// horizontal travel before it hit anything.
    #[test]
    fn a_vertical_marquee_selects_clips_stacked_across_tracks() {
        let (state, track_ids, clip_ids) = stacked_midi_clips(3);
        let x = state.beats_to_x(4.0);
        let found = hits(&state, MarqueeRect::from_corners((x, 30.0), (x, 180.0)));
        assert_eq!(found.track_ids, track_ids);
        assert_eq!(found.clip_ids, clip_ids);
    }

    #[test]
    fn a_marquee_hits_a_clip_where_it_is_drawn() {
        let (mut state, track_ids, clip_ids) = stacked_midi_clips(1);
        // A clip far shorter than its 10 px minimum drawn width.
        state.tracks[0].clips[0].duration_beats = 0.02;
        let drawn = state.clip_lane_rect(&state.tracks[0].clips[0], DEFAULT_TRACK_HEIGHT);
        assert!((drawn.width - CLIP_MIN_DRAWN_WIDTH).abs() < 1.0e-4);
        let model_right = state.beats_to_x(2.02);
        assert!(model_right + 3.0 < drawn.right());

        // Touching only the drawn part past the model end still hits it.
        let past_model_end = MarqueeRect::from_corners(
            (drawn.right() - 2.0, 20.0),
            (drawn.right() + 40.0, 40.0),
        );
        assert_eq!(hits(&state, past_model_end).clip_ids, clip_ids);

        // The pad bands above and below the clip are lane, not clip: the track
        // is crossed but the clip is not touched.
        let pad_above =
            MarqueeRect::from_corners((drawn.left - 5.0, 1.0), (drawn.right() + 5.0, 6.0));
        let found = hits(&state, pad_above);
        assert_eq!(found.track_ids, track_ids);
        assert!(found.clip_ids.is_empty());
        let pad_below = MarqueeRect::from_corners(
            (drawn.left - 5.0, DEFAULT_TRACK_HEIGHT - 6.0),
            (drawn.right() + 5.0, DEFAULT_TRACK_HEIGHT - 1.0),
        );
        assert!(hits(&state, pad_below).clip_ids.is_empty());

        // One pixel into the drawn body is a hit.
        let edge = MarqueeRect::from_corners(
            (drawn.left - 20.0, CLIP_LANE_PAD - 3.0),
            (drawn.left + 1.0, CLIP_LANE_PAD + 1.0),
        );
        assert_eq!(hits(&state, edge).clip_ids, clip_ids);
    }

    /// Dragging below the last row keeps every row down to the last one; it
    /// used to fall back to the row the drag started on.
    #[test]
    fn a_marquee_past_the_last_row_reaches_the_last_row() {
        let (state, track_ids, _) = stacked_midi_clips(3);
        let total = state.track_row_layout().total_height;
        let found = hits(
            &state,
            MarqueeRect::from_corners((0.0, DEFAULT_TRACK_HEIGHT + 20.0), (10.0, total + 400.0)),
        );
        assert_eq!(found.track_ids, track_ids[1..].to_vec());
    }

    /// Rows that are not drawn — Bus/Return channels, VSTi multi-out children,
    /// children of a collapsed group — never enter the selection, however the
    /// rectangle spans them.
    #[test]
    fn a_marquee_never_selects_hidden_rows_or_their_clips() {
        use crate::components::timeline::timeline_state::{
            vsti_output_child_track_id, CreateTrackOptions, InputMonitorMode,
        };
        let (mut state, track_ids, clip_ids) = stacked_midi_clips(3);
        let group_id = state.create_track(CreateTrackOptions {
            name: "Group".to_string(),
            track_type: TrackType::Group,
            color: crate::theme::Colors::track_color_for_index(0),
            volume: 1.0,
            pan: 0.0,
            armed: false,
            input_monitor: InputMonitorMode::Off,
        });
        // The middle track goes into the group, which is then collapsed.
        assert!(state.assign_track_to_group(&track_ids[1], &group_id));
        assert_eq!(state.toggle_group_collapsed(&group_id), Some(true));
        let bus_id = state.create_bus_track(&[]);
        // A VSTi multi-out child row, with a clip that must stay out of reach.
        let child_index = state.tracks.len();
        let child_source = state.create_midi_track();
        let child_clip = state.build_midi_clip(&child_source, 2.0, 4.0).expect("clip");
        state.tracks[child_index].id = vsti_output_child_track_id("insert-1", 1);
        state.tracks[child_index].clips.push(child_clip);

        let found = hits(
            &state,
            MarqueeRect::from_corners((0.0, 0.0), (2_000.0, 5_000.0)),
        );
        assert!(!found.track_ids.contains(&track_ids[1]), "collapsed child");
        assert!(!found.clip_ids.contains(&clip_ids[1]), "collapsed child's clip");
        assert!(!found.track_ids.contains(&bus_id), "mixer-only bus");
        assert!(!found
            .track_ids
            .iter()
            .any(|id| crate::components::timeline::timeline_state::is_vsti_output_child_track_id(id)));
        assert_eq!(found.clip_ids, vec![clip_ids[0].clone(), clip_ids[2].clone()]);
        assert!(found.track_ids.contains(&track_ids[0]));
        assert!(found.track_ids.contains(&track_ids[2]));
        assert!(found.track_ids.contains(&group_id));
    }

    /// The rectangle is free pixels: the grid has nothing to say about what
    /// it encloses.
    #[test]
    fn marquee_hits_do_not_depend_on_snap() {
        let (mut state, _, _) = stacked_midi_clips(2);
        let rect = MarqueeRect::from_corners(
            (state.beats_to_x(5.93), 12.0),
            (state.beats_to_x(9.0), DEFAULT_TRACK_HEIGHT + 30.0),
        );
        state.snap_to_grid = true;
        let snapped = hits(&state, rect);
        state.snap_to_grid = false;
        assert_eq!(snapped, hits(&state, rect));
        assert_eq!(snapped.clip_ids.len(), 2);
    }

    #[test]
    fn pressing_a_selected_clip_defers_the_selection_change_to_the_click() {
        assert_eq!(
            clip_press_plan(false, false),
            ClipPressPlan {
                on_press: Some(ClipSelectOp::Replace),
                on_click: None
            }
        );
        assert_eq!(
            clip_press_plan(true, false),
            ClipPressPlan {
                on_press: None,
                on_click: Some(ClipSelectOp::Replace)
            }
        );
        assert_eq!(
            clip_press_plan(false, true),
            ClipPressPlan {
                on_press: Some(ClipSelectOp::Toggle),
                on_click: None
            }
        );
        assert_eq!(
            clip_press_plan(true, true),
            ClipPressPlan {
                on_press: None,
                on_click: Some(ClipSelectOp::Toggle)
            }
        );
    }

    /// An additive clip click inside a cross-track selection keeps the tracks.
    #[test]
    fn an_additive_clip_click_extends_the_track_selection() {
        let (mut state, track_ids, clip_ids) = stacked_midi_clips(3);
        state.apply_marquee_selection(
            &track_ids[..2],
            clip_ids[..2].to_vec(),
            &track_ids[0],
            false,
        );
        state.select_clip_additive(&clip_ids[2]);
        assert_eq!(state.selection.selected_track_ids, track_ids);
        assert_eq!(state.selection.selected_clip_ids, clip_ids);
        assert_eq!(
            state.selection.selected_track_id.as_deref(),
            Some(track_ids[2].as_str())
        );
        state.select_clip_additive(&clip_ids[0]);
        assert_eq!(state.selection.selected_clip_ids, clip_ids[1..].to_vec());
        assert_eq!(state.selection.selected_track_ids, track_ids);
    }
}

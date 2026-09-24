//! Global Chord Track: the project's harmony as beat spans under the ruler.
//!
//! One chord event covers `[start_beat, start_beat + length_beats)`. Events
//! never overlap: placing chords (a drop from the Chord Generator, a move, a
//! resize) trims or removes whatever it covers, so the lane always reads as a
//! single left-to-right chord chart — the same model Studio One and Cubase
//! use for their chord tracks.

use super::*;
use sphere_midi_service::chords::Chord;

pub const CHORD_TRACK_HEIGHT: f32 = 38.0;
pub const CHORD_TRACK_HEIGHT_COLLAPSED: f32 = 22.0;
/// Shortest chord the lane keeps (a sixteenth at 4/4).
pub const MIN_CHORD_BEATS: f64 = 0.25;

/// One chord on the Chord Track.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChordTrackEvent {
    pub id: u64,
    pub start_beat: f64,
    pub length_beats: f64,
    pub chord: Chord,
    /// Spell the chord with flats (the key it was written in prefers them).
    pub flats: bool,
}

impl ChordTrackEvent {
    pub fn end_beat(&self) -> f64 {
        self.start_beat + self.length_beats
    }

    pub fn name(&self) -> String {
        self.chord.name(self.flats)
    }
}

/// A chord being placed: chord, spelling and length. The start is decided by
/// where it lands.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChordPlacement {
    pub chord: Chord,
    pub flats: bool,
    pub length_beats: f64,
}

/// Transient ghost of an external drop (the Chord Generator window) over the
/// Chord Track or a track lane. View state only; never persisted.
#[derive(Debug, Clone, PartialEq)]
pub struct ChordDropPreview {
    pub target: ChordDropTarget,
    pub start_beat: f64,
    pub chords: Vec<ChordPlacement>,
}

/// Where a dragged progression would land.
#[derive(Debug, Clone, PartialEq)]
pub enum ChordDropTarget {
    /// Onto the Chord Track.
    ChordTrack,
    /// As a MIDI clip on this MIDI/instrument track.
    Track { track_id: String },
    /// As a MIDI clip on a new MIDI track at the end of the track list.
    NewTrack,
}

impl ChordDropPreview {
    pub fn total_beats(&self) -> f64 {
        self.chords.iter().map(|c| c.length_beats).sum()
    }
}

impl TimelineState {
    pub fn chord_track_height(&self) -> f32 {
        if !self.show_chord_track {
            return 0.0;
        }
        self.global_lane_height(GlobalLaneKind::Chord)
    }

    pub fn show_chord_track_lane(&mut self) {
        self.show_chord_track = true;
    }

    pub fn hide_chord_track_lane(&mut self) {
        self.show_chord_track = false;
        self.selected_chord_event_id = None;
    }

    fn next_chord_event_id(&self) -> u64 {
        self.chord_events.iter().map(|e| e.id).max().unwrap_or(0) + 1
    }

    /// Clear `[start, end)` on the lane: events inside are removed, events
    /// straddling an edge are trimmed (one straddling both is split).
    fn clear_chord_range(&mut self, start: f64, end: f64, keep: Option<u64>) {
        let mut next_id = self.next_chord_event_id();
        let mut out = Vec::with_capacity(self.chord_events.len() + 1);
        for event in self.chord_events.drain(..) {
            if Some(event.id) == keep || event.end_beat() <= start || event.start_beat >= end {
                out.push(event);
                continue;
            }
            if event.start_beat < start {
                let mut head = event;
                head.length_beats = start - event.start_beat;
                if head.length_beats >= MIN_CHORD_BEATS {
                    out.push(head);
                }
            }
            if event.end_beat() > end {
                let mut tail = event;
                tail.start_beat = end;
                tail.length_beats = event.end_beat() - end;
                if event.start_beat < start {
                    // The head kept the original id.
                    tail.id = next_id;
                    next_id += 1;
                }
                if tail.length_beats >= MIN_CHORD_BEATS {
                    out.push(tail);
                }
            }
        }
        self.chord_events = out;
    }

    fn sort_chord_events(&mut self) {
        self.chord_events
            .sort_by(|a, b| a.start_beat.total_cmp(&b.start_beat).then(a.id.cmp(&b.id)));
    }

    /// Lay `chords` end to end from `start_beat`, replacing whatever they
    /// cover. Returns the new events' ids.
    pub fn place_chords(&mut self, start_beat: f64, chords: &[ChordPlacement]) -> Vec<u64> {
        let start = start_beat.max(0.0);
        let total: f64 = chords
            .iter()
            .map(|c| c.length_beats.max(MIN_CHORD_BEATS))
            .sum();
        if chords.is_empty() {
            return Vec::new();
        }
        self.clear_chord_range(start, start + total, None);
        let mut id = self.next_chord_event_id();
        let mut beat = start;
        let mut ids = Vec::with_capacity(chords.len());
        for placement in chords {
            let length = placement.length_beats.max(MIN_CHORD_BEATS);
            self.chord_events.push(ChordTrackEvent {
                id,
                start_beat: beat,
                length_beats: length,
                chord: placement.chord,
                flats: placement.flats,
            });
            ids.push(id);
            id += 1;
            beat += length;
        }
        self.sort_chord_events();
        ids
    }

    pub fn chord_event(&self, id: u64) -> Option<&ChordTrackEvent> {
        self.chord_events.iter().find(|e| e.id == id)
    }

    /// The chord sounding at `beat`.
    pub fn chord_event_at(&self, beat: f64) -> Option<u64> {
        self.chord_events
            .iter()
            .find(|e| beat >= e.start_beat && beat < e.end_beat())
            .map(|e| e.id)
    }

    /// Move or resize one event to `[start, end)`, clearing what it now covers.
    pub fn set_chord_event_range(&mut self, id: u64, start: f64, end: f64) -> bool {
        let start = start.max(0.0);
        let end = end.max(start + MIN_CHORD_BEATS);
        let Some(index) = self.chord_events.iter().position(|e| e.id == id) else {
            return false;
        };
        let current = self.chord_events[index];
        if (current.start_beat - start).abs() < 1.0e-9 && (current.end_beat() - end).abs() < 1.0e-9
        {
            return false;
        }
        self.clear_chord_range(start, end, Some(id));
        if let Some(event) = self.chord_events.iter_mut().find(|e| e.id == id) {
            event.start_beat = start;
            event.length_beats = end - start;
        }
        self.sort_chord_events();
        true
    }

    pub fn delete_chord_event(&mut self, id: u64) -> bool {
        let before = self.chord_events.len();
        self.chord_events.retain(|e| e.id != id);
        if self.selected_chord_event_id == Some(id) {
            self.selected_chord_event_id = None;
        }
        self.chord_events.len() != before
    }

    pub fn select_chord_event(&mut self, id: u64) {
        self.selected_chord_event_id = Some(id);
    }

    pub fn clear_chord_selection(&mut self) {
        self.selected_chord_event_id = None;
    }

    /// Where a progression released at window point `(x, y)` would land, and
    /// on which (snapped) beat. `None` outside the arrangement — over the
    /// ruler, another conductor lane, or beyond the lanes' edges.
    ///
    /// Rule: the Chord Track takes chords; a MIDI or instrument track row
    /// takes a MIDI clip; any other row, or the space below the tracks, makes
    /// a new MIDI track. The drop hint and the drop both resolve through here.
    pub fn resolve_chord_drop(&self, x: f32, y: f32) -> Option<(ChordDropTarget, f64)> {
        let lane_origin = self.lane_origin_x();
        let lanes_right = lane_origin + self.viewport.viewport_width;
        if x < lane_origin - HEADER_WIDTH || x > lanes_right {
            return None;
        }
        // Over a header column the drop starts at the playhead; over lanes,
        // at the snapped beat under the pointer.
        let beat = if x < lane_origin {
            self.transport.playhead_beats.max(0.0) as f64
        } else {
            let lane_x = self.lane_x_from_window_x(x);
            (self.snap_beats(self.x_to_beat(lane_x) as f32).max(0.0)) as f64
        };
        let chrome = crate::shell_metrics::APP_CHROME_HEIGHT;
        if self.show_chord_track {
            let top = chrome + RULER_HEIGHT + self.global_lane_top(GlobalLaneKind::Chord);
            if y >= top && y < top + self.chord_track_height() {
                return Some((ChordDropTarget::ChordTrack, beat));
            }
        }
        let content_top = chrome + self.arrangement_content_top();
        if y < content_top || y > content_top + self.viewport.viewport_height.max(1.0) {
            return None;
        }
        let target = self
            .track_index_at_y(y - content_top)
            .and_then(|index| self.tracks.get(index))
            .filter(|track| matches!(track.track_type, TrackType::Midi | TrackType::Instrument))
            .map(|track| ChordDropTarget::Track {
                track_id: track.id.clone(),
            })
            .unwrap_or(ChordDropTarget::NewTrack);
        Some((target, beat))
    }

    pub fn chord_lane_header_subtitle(&self) -> String {
        match self.chord_events.len() {
            0 => "Drop chords here".to_string(),
            1 => "1 chord".to_string(),
            n => format!("{n} chords"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sphere_midi_service::chords::ChordQuality;

    fn placement(root: u8, beats: f64) -> ChordPlacement {
        ChordPlacement {
            chord: Chord::new(root, ChordQuality::Major),
            flats: false,
            length_beats: beats,
        }
    }

    fn spans(state: &TimelineState) -> Vec<(f64, f64, u8)> {
        state
            .chord_events
            .iter()
            .map(|e| (e.start_beat, e.end_beat(), e.chord.root))
            .collect()
    }

    #[test]
    fn placing_lays_chords_end_to_end() {
        let mut state = TimelineState::default();
        state.place_chords(4.0, &[placement(0, 4.0), placement(7, 4.0)]);
        assert_eq!(spans(&state), vec![(4.0, 8.0, 0), (8.0, 12.0, 7)]);
    }

    #[test]
    fn placing_over_existing_chords_trims_and_splits_them() {
        let mut state = TimelineState::default();
        state.place_chords(0.0, &[placement(0, 16.0)]);
        state.place_chords(4.0, &[placement(9, 4.0)]);
        assert_eq!(
            spans(&state),
            vec![(0.0, 4.0, 0), (4.0, 8.0, 9), (8.0, 16.0, 0)]
        );
        let ids: std::collections::HashSet<u64> = state.chord_events.iter().map(|e| e.id).collect();
        assert_eq!(ids.len(), 3, "split halves get distinct ids");
    }

    #[test]
    fn moving_an_event_clears_what_it_lands_on() {
        let mut state = TimelineState::default();
        let ids = state.place_chords(0.0, &[placement(0, 4.0), placement(7, 4.0)]);
        assert!(state.set_chord_event_range(ids[0], 6.0, 10.0));
        assert_eq!(spans(&state), vec![(4.0, 6.0, 7), (6.0, 10.0, 0)]);
        assert_eq!(state.chord_event_at(7.0), Some(ids[0]));
    }

    #[test]
    fn drop_resolution_follows_the_lane_under_the_pointer() {
        let mut state = TimelineState::default();
        state.viewport.viewport_width = 800.0;
        state.viewport.viewport_height = 600.0;
        state.show_chord_track_lane();
        let x = state.lane_origin_x() + 10.0;
        let chrome = crate::shell_metrics::APP_CHROME_HEIGHT;
        let lane_top = chrome + RULER_HEIGHT + state.global_lane_top(GlobalLaneKind::Chord);
        assert_eq!(
            state.resolve_chord_drop(x, lane_top + 4.0).map(|r| r.0),
            Some(ChordDropTarget::ChordTrack)
        );
        // Over the ruler: nothing.
        assert!(state.resolve_chord_drop(x, chrome + 2.0).is_none());
        // Below every track: a new MIDI track.
        let below = chrome + state.arrangement_content_top() + 590.0;
        assert_eq!(
            state.resolve_chord_drop(x, below).map(|r| r.0),
            Some(ChordDropTarget::NewTrack)
        );
        // Right of the lanes: nothing.
        assert!(state
            .resolve_chord_drop(state.lane_origin_x() + 900.0, below)
            .is_none());
    }

    #[test]
    fn delete_clears_selection() {
        let mut state = TimelineState::default();
        let ids = state.place_chords(0.0, &[placement(0, 4.0)]);
        state.select_chord_event(ids[0]);
        assert!(state.delete_chord_event(ids[0]));
        assert!(state.chord_events.is_empty());
        assert_eq!(state.selected_chord_event_id, None);
    }
}

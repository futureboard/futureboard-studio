//! The Chord Generator's timeline: chord regions on a Chords lane and
//! performance-pattern regions on Comp and Bass lanes, in beats from the
//! timeline's start.
//!
//! Regions in one lane never overlap. Placing, moving or stretching a region
//! overwrites whatever it covers in its lane, the way an arrangement editor
//! does: a region it lands inside is split around it, one it half covers is
//! trimmed, one it covers is removed. Pure data — the window owns gestures,
//! preview and undo around it.

use sphere_midi_service::chords::Chord;
use sphere_midi_service::performance::{
    BassPattern, ChordSpan, CompPattern, PatternSpan, PATTERN_BEATS,
};

pub const BEATS_PER_BAR: f64 = PATTERN_BEATS;
/// The timeline never shows fewer bars than this.
pub const MIN_BARS: u32 = 4;
/// Nor grows past this.
pub const MAX_BARS: u32 = 32;
/// Shortest region, beats.
pub const MIN_LENGTH: f64 = 1.0;
/// Undo depth.
const HISTORY: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lane {
    Chords,
    Comp,
    Bass,
}

impl Lane {
    pub const ALL: [Lane; 3] = [Lane::Chords, Lane::Comp, Lane::Bass];

    pub fn label(self) -> &'static str {
        match self {
            Lane::Chords => "Chords",
            Lane::Comp => "Comp",
            Lane::Bass => "Bass",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Content {
    Chord {
        chord: Chord,
        /// Kept when Generate rewrites the progression.
        locked: bool,
    },
    Comp(CompPattern),
    Bass(BassPattern),
}

impl Content {
    pub fn lane(&self) -> Lane {
        match self {
            Content::Chord { .. } => Lane::Chords,
            Content::Comp(_) => Lane::Comp,
            Content::Bass(_) => Lane::Bass,
        }
    }

    pub fn chord(&self) -> Option<Chord> {
        match self {
            Content::Chord { chord, .. } => Some(*chord),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Region {
    pub id: u64,
    pub start: f64,
    pub length: f64,
    pub content: Content,
}

impl Region {
    pub fn end(&self) -> f64 {
        self.start + self.length
    }

    pub fn lane(&self) -> Lane {
        self.content.lane()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Arrangement {
    regions: Vec<Region>,
    bars: u32,
    next_id: u64,
}

impl Default for Arrangement {
    fn default() -> Self {
        Self {
            regions: Vec::new(),
            bars: MIN_BARS * 2,
            next_id: 1,
        }
    }
}

impl Arrangement {
    pub fn bars(&self) -> u32 {
        self.bars
    }

    pub fn total_beats(&self) -> f64 {
        self.bars as f64 * BEATS_PER_BAR
    }

    /// Show `bars` bars, but never hide a region.
    pub fn set_bars(&mut self, bars: u32) {
        let needed = (self.content_end() / BEATS_PER_BAR).ceil() as u32;
        self.bars = bars.clamp(MIN_BARS, MAX_BARS).max(needed);
    }

    /// Regions of `lane`, in time order.
    pub fn lane(&self, lane: Lane) -> impl Iterator<Item = &Region> {
        self.regions.iter().filter(move |r| r.lane() == lane)
    }

    pub fn find(&self, id: u64) -> Option<&Region> {
        self.regions.iter().find(|r| r.id == id)
    }

    pub fn is_empty(&self) -> bool {
        self.regions.is_empty()
    }

    /// End of the last region, beats (0 when empty).
    pub fn content_end(&self) -> f64 {
        self.regions.iter().map(Region::end).fold(0.0, f64::max)
    }

    /// End of the last chord, beats (0 without chords).
    pub fn chords_end(&self) -> f64 {
        self.lane(Lane::Chords).map(Region::end).fold(0.0, f64::max)
    }

    /// The region of `lane` sounding at `beat`.
    pub fn region_at(&self, lane: Lane, beat: f64) -> Option<&Region> {
        self.lane(lane)
            .find(|r| r.start <= beat + 1e-9 && beat < r.end() - 1e-9)
    }

    /// Place `content` over `[start, start + length)`, overwriting what its
    /// lane holds there. The span is clamped to the timeline's maximum
    /// length and the timeline grows to show it. Returns the new region's id,
    /// or `None` when nothing of it fits.
    pub fn place(&mut self, start: f64, length: f64, content: Content) -> Option<u64> {
        let id = self.next_id;
        self.next_id += 1;
        self.insert(Region {
            id,
            start,
            length,
            content,
        })
        .then_some(id)
    }

    /// Place a run of regions back to back from `start`.
    pub fn place_sequence(&mut self, start: f64, items: &[(Content, f64)]) -> Vec<u64> {
        let mut at = start;
        let mut ids = Vec::with_capacity(items.len());
        for &(content, length) in items {
            if let Some(id) = self.place(at, length, content) {
                ids.push(id);
            }
            at += length;
        }
        ids
    }

    /// Move a region to start at `start`, overwriting what it lands on.
    pub fn move_region(&mut self, id: u64, start: f64) -> bool {
        let Some(region) = self.take(id) else {
            return false;
        };
        self.insert(Region { start, ..region })
    }

    /// Give a region a new length, overwriting what it grows over.
    pub fn resize_region(&mut self, id: u64, length: f64) -> bool {
        let Some(region) = self.take(id) else {
            return false;
        };
        self.insert(Region {
            length: length.max(MIN_LENGTH),
            ..region
        })
    }

    pub fn remove(&mut self, id: u64) -> bool {
        self.take(id).is_some()
    }

    /// Replace a region's content with another of the same lane.
    pub fn set_content(&mut self, id: u64, content: Content) -> bool {
        match self.regions.iter_mut().find(|r| r.id == id) {
            Some(region) if region.lane() == content.lane() => {
                region.content = content;
                true
            }
            _ => false,
        }
    }

    /// Remove every region of `lane` for which `remove` holds.
    pub fn clear_lane(&mut self, lane: Lane, remove: impl Fn(&Region) -> bool) {
        self.regions.retain(|r| r.lane() != lane || !remove(r));
    }

    /// Shift every chord by `semitones` (a key change moves the whole song).
    pub fn transpose(&mut self, semitones: i32) {
        let shift = |pc: u8| (pc as i32 + semitones).rem_euclid(12) as u8;
        for region in &mut self.regions {
            if let Content::Chord { chord, .. } = &mut region.content {
                chord.root = shift(chord.root);
                chord.bass = chord.bass.map(shift);
            }
        }
    }

    pub fn chord_spans(&self) -> Vec<ChordSpan> {
        self.lane(Lane::Chords)
            .filter_map(|r| {
                r.content.chord().map(|chord| ChordSpan {
                    chord,
                    start: r.start,
                    length: r.length,
                })
            })
            .collect()
    }

    pub fn comp_spans(&self) -> Vec<PatternSpan<CompPattern>> {
        self.lane(Lane::Comp)
            .filter_map(|r| match r.content {
                Content::Comp(pattern) => Some(PatternSpan {
                    pattern,
                    start: r.start,
                    length: r.length,
                }),
                _ => None,
            })
            .collect()
    }

    pub fn bass_spans(&self) -> Vec<PatternSpan<BassPattern>> {
        self.lane(Lane::Bass)
            .filter_map(|r| match r.content {
                Content::Bass(pattern) => Some(PatternSpan {
                    pattern,
                    start: r.start,
                    length: r.length,
                }),
                _ => None,
            })
            .collect()
    }

    fn take(&mut self, id: u64) -> Option<Region> {
        let index = self.regions.iter().position(|r| r.id == id)?;
        Some(self.regions.remove(index))
    }

    /// Insert a region (keeping its id), clamped to the timeline's maximum
    /// and overwriting its lane under it.
    fn insert(&mut self, region: Region) -> bool {
        let limit = MAX_BARS as f64 * BEATS_PER_BAR;
        let start = region.start.max(0.0);
        let end = (region.start + region.length).min(limit);
        if end - start < MIN_LENGTH - 1e-9 {
            return false;
        }
        let lane = region.lane();
        let mut kept = Vec::with_capacity(self.regions.len() + 1);
        for r in self.regions.drain(..) {
            if r.lane() != lane || r.end() <= start + 1e-9 || r.start >= end - 1e-9 {
                kept.push(r);
                continue;
            }
            // Keep what sticks out on either side.
            if r.start < start - 1e-9 {
                kept.push(Region {
                    length: start - r.start,
                    ..r
                });
            }
            if r.end() > end + 1e-9 {
                let id = self.next_id;
                self.next_id += 1;
                kept.push(Region {
                    id: if r.start < start - 1e-9 { id } else { r.id },
                    start: end,
                    length: r.end() - end,
                    ..r
                });
            }
        }
        kept.push(Region {
            start,
            length: end - start,
            ..region
        });
        kept.sort_by(|a, b| a.start.total_cmp(&b.start));
        self.regions = kept;
        let needed = (self.content_end() / BEATS_PER_BAR).ceil() as u32;
        self.bars = self.bars.max(needed).min(MAX_BARS);
        true
    }
}

/// Undo/redo over whole arrangements (they are small).
#[derive(Debug, Default)]
pub struct History {
    past: Vec<Arrangement>,
    future: Vec<Arrangement>,
}

impl History {
    /// Record the state before an edit.
    pub fn record(&mut self, before: Arrangement) {
        if self.past.last() == Some(&before) {
            return;
        }
        self.past.push(before);
        if self.past.len() > HISTORY {
            self.past.remove(0);
        }
        self.future.clear();
    }

    pub fn undo(&mut self, current: &mut Arrangement) -> bool {
        match self.past.pop() {
            Some(previous) => {
                self.future.push(std::mem::replace(current, previous));
                true
            }
            None => false,
        }
    }

    pub fn redo(&mut self, current: &mut Arrangement) -> bool {
        match self.future.pop() {
            Some(next) => {
                self.past.push(std::mem::replace(current, next));
                true
            }
            None => false,
        }
    }
}

/// Snap `beat` to the grid.
pub fn snap(beat: f64, grid: f64) -> f64 {
    (beat / grid).round() * grid
}

#[cfg(test)]
mod tests {
    use super::*;
    use sphere_midi_service::chords::ChordQuality;

    fn chord(root: u8) -> Content {
        Content::Chord {
            chord: Chord::new(root, ChordQuality::Major),
            locked: false,
        }
    }

    fn spans(a: &Arrangement, lane: Lane) -> Vec<(f64, f64)> {
        a.lane(lane).map(|r| (r.start, r.end())).collect()
    }

    #[test]
    fn a_sequence_lays_chords_back_to_back() {
        let mut a = Arrangement::default();
        a.place_sequence(0.0, &[(chord(0), 4.0), (chord(7), 4.0), (chord(9), 2.0)]);
        assert_eq!(
            spans(&a, Lane::Chords),
            vec![(0.0, 4.0), (4.0, 8.0), (8.0, 10.0)]
        );
    }

    #[test]
    fn placing_over_a_region_splits_trims_and_removes() {
        let mut a = Arrangement::default();
        a.place_sequence(0.0, &[(chord(0), 8.0), (chord(7), 2.0), (chord(9), 4.0)]);
        // Two beats in the middle of the first chord.
        a.place(2.0, 2.0, chord(5));
        assert_eq!(
            spans(&a, Lane::Chords),
            vec![
                (0.0, 2.0),
                (2.0, 4.0),
                (4.0, 8.0),
                (8.0, 10.0),
                (10.0, 14.0)
            ]
        );
        // Over the end of one chord, all of the next, the start of a third.
        a.place(7.0, 4.0, chord(2));
        assert_eq!(
            spans(&a, Lane::Chords),
            vec![
                (0.0, 2.0),
                (2.0, 4.0),
                (4.0, 7.0),
                (7.0, 11.0),
                (11.0, 14.0)
            ]
        );
    }

    #[test]
    fn lanes_do_not_overwrite_each_other() {
        let mut a = Arrangement::default();
        a.place(0.0, 8.0, chord(0));
        a.place(0.0, 8.0, Content::Comp(CompPattern::Strum));
        a.place(0.0, 8.0, Content::Bass(BassPattern::Root));
        assert_eq!(a.chord_spans().len(), 1);
        assert_eq!(a.comp_spans().len(), 1);
        assert_eq!(a.bass_spans().len(), 1);
    }

    #[test]
    fn moving_a_chord_keeps_its_id_and_overwrites_its_new_place() {
        let mut a = Arrangement::default();
        let ids = a.place_sequence(0.0, &[(chord(0), 4.0), (chord(7), 4.0)]);
        assert!(a.move_region(ids[0], 6.0));
        let moved = a.find(ids[0]).unwrap();
        assert_eq!((moved.start, moved.end()), (6.0, 10.0));
        // The G it landed on keeps its first two beats.
        assert_eq!(spans(&a, Lane::Chords), vec![(4.0, 6.0), (6.0, 10.0)]);
    }

    #[test]
    fn resizing_has_a_floor_and_grows_the_timeline() {
        let mut a = Arrangement::default();
        let id = a.place(0.0, 4.0, chord(0)).unwrap();
        a.resize_region(id, 0.25);
        assert_eq!(a.find(id).unwrap().length, MIN_LENGTH);
        a.resize_region(id, 40.0);
        assert_eq!(a.bars(), 10);
    }

    #[test]
    fn nothing_is_placed_past_the_maximum() {
        let mut a = Arrangement::default();
        let limit = MAX_BARS as f64 * BEATS_PER_BAR;
        assert!(a.place(limit, 4.0, chord(0)).is_none());
        let id = a.place(limit - 2.0, 4.0, chord(0)).unwrap();
        assert_eq!(a.find(id).unwrap().end(), limit);
        assert_eq!(a.bars(), MAX_BARS);
    }

    #[test]
    fn the_timeline_never_hides_a_region() {
        let mut a = Arrangement::default();
        a.place(20.0, 4.0, chord(0));
        a.set_bars(MIN_BARS);
        assert_eq!(a.bars(), 6);
    }

    #[test]
    fn transposing_moves_roots_and_slash_basses() {
        let mut a = Arrangement::default();
        let mut c_over_e = Chord::new(0, ChordQuality::Major);
        c_over_e.bass = Some(4);
        a.place(
            0.0,
            4.0,
            Content::Chord {
                chord: c_over_e,
                locked: false,
            },
        );
        a.transpose(-3);
        let chord = a.chord_spans()[0].chord;
        assert_eq!((chord.root, chord.bass), (9, Some(1)));
    }

    #[test]
    fn undo_and_redo_walk_the_history() {
        let mut a = Arrangement::default();
        let mut history = History::default();
        history.record(a.clone());
        a.place(0.0, 4.0, chord(0));
        history.record(a.clone());
        a.place(4.0, 4.0, chord(7));
        assert!(history.undo(&mut a));
        assert_eq!(a.chord_spans().len(), 1);
        assert!(history.undo(&mut a));
        assert!(a.is_empty());
        assert!(!history.undo(&mut a));
        assert!(history.redo(&mut a));
        assert_eq!(a.chord_spans().len(), 1);
    }
}

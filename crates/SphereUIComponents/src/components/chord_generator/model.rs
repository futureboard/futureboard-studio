//! The Chord Generator's song: a sequence of sections, each a run of chords
//! played back to back, with Comp and Bass pattern regions under it saying
//! how the chords are performed.
//!
//! The sequence has no gaps. A chord's place is its order, and its start is
//! the sum of the lengths before it, so moving or resizing one moves the
//! chords after it. A section is just a group of chords; it is never empty
//! (removing its last chord removes it). Pattern regions are placed in beats
//! and never overlap within their lane; placing, moving or stretching one
//! overwrites whatever it covers, the way an arrangement editor does. A
//! pattern region that ran to the end of the chords keeps running to their
//! end when the sequence grows or shrinks.
//!
//! Pure data — the window owns gestures, preview and undo around it. Ids are
//! stable across edits and undo.

use sphere_midi_service::chords::Chord;
use sphere_midi_service::performance::{
    BassPattern, ChordSpan, CompPattern, PatternSpan, PATTERN_BEATS,
};

pub const BEATS_PER_BAR: f64 = PATTERN_BEATS;
/// The sequence view never shows fewer bars than this.
pub const MIN_BARS: u32 = 4;
/// Nor grows past this; nothing is placed beyond it.
pub const MAX_BARS: u32 = 32;
/// Shortest chord or pattern region, beats.
pub const MIN_LENGTH: f64 = 1.0;
/// Undo depth.
const HISTORY: usize = 100;
const EPS: f64 = 1e-9;
/// Stands in for moved chords while they are taken out and put back.
const SENTINEL: u64 = u64::MAX;

fn limit() -> f64 {
    MAX_BARS as f64 * BEATS_PER_BAR
}

/// One chord of a section.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Slot {
    pub id: u64,
    pub chord: Chord,
    pub beats: f64,
    /// Kept when Fill rewrites its section.
    pub locked: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Section {
    pub id: u64,
    pub slots: Vec<Slot>,
}

impl Section {
    pub fn beats(&self) -> f64 {
        self.slots.iter().map(|s| s.beats).sum()
    }
}

/// Where chords go in the sequence.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Insert {
    /// A new section at this index of the sequence (`len` appends).
    NewSection(usize),
    /// Into `section`, before its chord at `index` (`len` appends).
    Into { section: u64, index: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lane {
    Comp,
    Bass,
}

impl Lane {
    pub const ALL: [Lane; 2] = [Lane::Comp, Lane::Bass];

    pub fn label(self) -> &'static str {
        match self {
            Lane::Comp => "Comp",
            Lane::Bass => "Bass",
        }
    }

    /// The pattern a new region of this lane starts with.
    pub fn default_pattern(self) -> Pattern {
        match self {
            Lane::Comp => Pattern::Comp(CompPattern::Sustain),
            Lane::Bass => Pattern::Bass(BassPattern::Root),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Pattern {
    Comp(CompPattern),
    Bass(BassPattern),
}

impl Pattern {
    pub fn lane(self) -> Lane {
        match self {
            Pattern::Comp(_) => Lane::Comp,
            Pattern::Bass(_) => Lane::Bass,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Pattern::Comp(p) => p.label(),
            Pattern::Bass(p) => p.label(),
        }
    }

    pub fn onsets(self) -> Vec<f64> {
        match self {
            Pattern::Comp(p) => p.onsets(),
            Pattern::Bass(p) => p.onsets(),
        }
    }
}

/// A pattern placed on its lane.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Region {
    pub id: u64,
    pub start: f64,
    pub length: f64,
    pub pattern: Pattern,
}

impl Region {
    pub fn end(&self) -> f64 {
        self.start + self.length
    }

    pub fn lane(&self) -> Lane {
        self.pattern.lane()
    }
}

/// A chord with its place in the sequence.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placed {
    pub section_index: usize,
    pub slot: Slot,
    pub start: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Arrangement {
    sections: Vec<Section>,
    regions: Vec<Region>,
    bars: u32,
    next_id: u64,
}

impl Default for Arrangement {
    fn default() -> Self {
        Self {
            sections: Vec::new(),
            regions: Vec::new(),
            bars: MIN_BARS * 2,
            next_id: 1,
        }
    }
}

impl Arrangement {
    // ── Reading ─────────────────────────────────────────────────────────────

    pub fn sections(&self) -> &[Section] {
        &self.sections
    }

    pub fn bars(&self) -> u32 {
        self.bars
    }

    pub fn total_beats(&self) -> f64 {
        self.bars as f64 * BEATS_PER_BAR
    }

    pub fn is_empty(&self) -> bool {
        self.sections.is_empty() && self.regions.is_empty()
    }

    pub fn chord_count(&self) -> usize {
        self.sections.iter().map(|s| s.slots.len()).sum()
    }

    /// End of the last chord, beats.
    pub fn chords_end(&self) -> f64 {
        self.sections.iter().map(Section::beats).sum()
    }

    /// End of the last chord or pattern, beats.
    pub fn content_end(&self) -> f64 {
        self.regions
            .iter()
            .map(Region::end)
            .fold(self.chords_end(), f64::max)
    }

    /// Every chord in order, with where it starts.
    pub fn placed(&self) -> Vec<Placed> {
        let mut at = 0.0;
        let mut out = Vec::with_capacity(self.chord_count());
        for (section_index, section) in self.sections.iter().enumerate() {
            for slot in &section.slots {
                out.push(Placed {
                    section_index,
                    slot: *slot,
                    start: at,
                });
                at += slot.beats;
            }
        }
        out
    }

    pub fn find_slot(&self, id: u64) -> Option<Placed> {
        self.placed().into_iter().find(|p| p.slot.id == id)
    }

    /// Index of the section holding chord `id`.
    pub fn section_of(&self, id: u64) -> Option<usize> {
        self.sections
            .iter()
            .position(|s| s.slots.iter().any(|slot| slot.id == id))
    }

    pub fn section_index(&self, id: u64) -> Option<usize> {
        self.sections.iter().position(|s| s.id == id)
    }

    /// `(start, length)` of section `id`.
    pub fn section_span(&self, id: u64) -> Option<(f64, f64)> {
        let index = self.section_index(id)?;
        let start = self.sections[..index].iter().map(Section::beats).sum();
        Some((start, self.sections[index].beats()))
    }

    /// The chord sounding at `beat`.
    pub fn slot_at(&self, beat: f64) -> Option<Slot> {
        self.placed()
            .into_iter()
            .find(|p| p.start <= beat + EPS && beat < p.start + p.slot.beats - EPS)
            .map(|p| p.slot)
    }

    /// The chord after chord `id`, across sections.
    pub fn next_chord(&self, id: u64) -> Option<Chord> {
        let placed = self.placed();
        let index = placed.iter().position(|p| p.slot.id == id)?;
        placed.get(index + 1).map(|p| p.slot.chord)
    }

    /// Where an [`Insert`] lands, in beats.
    pub fn insert_beat(&self, at: Insert) -> f64 {
        match at {
            Insert::NewSection(index) => self.sections[..index.min(self.sections.len())]
                .iter()
                .map(Section::beats)
                .sum(),
            Insert::Into { section, index } => {
                let Some(i) = self.section_index(section) else {
                    return self.chords_end();
                };
                let before: f64 = self.sections[..i].iter().map(Section::beats).sum();
                let within: f64 = self.sections[i].slots[..index.min(self.sections[i].slots.len())]
                    .iter()
                    .map(|s| s.beats)
                    .sum();
                before + within
            }
        }
    }

    pub fn chord_spans(&self) -> Vec<ChordSpan> {
        self.placed()
            .into_iter()
            .map(|p| ChordSpan {
                chord: p.slot.chord,
                start: p.start,
                length: p.slot.beats,
            })
            .collect()
    }

    /// Regions of `lane`, in time order.
    pub fn lane(&self, lane: Lane) -> impl Iterator<Item = &Region> {
        self.regions.iter().filter(move |r| r.lane() == lane)
    }

    pub fn find_region(&self, id: u64) -> Option<&Region> {
        self.regions.iter().find(|r| r.id == id)
    }

    /// The region of `lane` playing at `beat`.
    pub fn region_at(&self, lane: Lane, beat: f64) -> Option<&Region> {
        self.lane(lane)
            .find(|r| r.start <= beat + EPS && beat < r.end() - EPS)
    }

    pub fn comp_spans(&self) -> Vec<PatternSpan<CompPattern>> {
        self.lane(Lane::Comp)
            .filter_map(|r| match r.pattern {
                Pattern::Comp(pattern) => Some(PatternSpan {
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
            .filter_map(|r| match r.pattern {
                Pattern::Bass(pattern) => Some(PatternSpan {
                    pattern,
                    start: r.start,
                    length: r.length,
                }),
                _ => None,
            })
            .collect()
    }

    // ── View length ─────────────────────────────────────────────────────────

    /// Show `bars` bars, but never hide anything.
    pub fn set_bars(&mut self, bars: u32) {
        let needed = (self.content_end() / BEATS_PER_BAR - EPS).ceil().max(0.0) as u32;
        self.bars = bars.clamp(MIN_BARS, MAX_BARS).max(needed);
    }

    fn grow_to_fit(&mut self) {
        let needed = (self.content_end() / BEATS_PER_BAR - EPS).ceil().max(0.0) as u32;
        self.bars = self.bars.max(needed).min(MAX_BARS);
    }

    // ── Chord edits ─────────────────────────────────────────────────────────

    /// Put `chords` (each with its length) at `at`. Chords that would run
    /// past the maximum are left out; the last one that fits is shortened.
    /// Returns the new chords' ids.
    pub fn insert_chords(&mut self, at: Insert, chords: &[(Chord, f64)]) -> Vec<u64> {
        self.chords_edit(|this| {
            let mut room = limit() - this.chords_end();
            let mut slots = Vec::with_capacity(chords.len());
            for &(chord, beats) in chords {
                let beats = beats.max(MIN_LENGTH).min(room);
                if beats < MIN_LENGTH - EPS {
                    break;
                }
                room -= beats;
                slots.push(Slot {
                    id: this.fresh_id(),
                    chord,
                    beats,
                    locked: false,
                });
            }
            let ids = slots.iter().map(|s| s.id).collect();
            if !slots.is_empty() {
                this.put(at, slots);
            }
            ids
        })
    }

    /// Move chords `ids` (in sequence order) to `at`, or copy them there
    /// with new ids when `duplicate`. Moved chords keep their ids. Returns
    /// the ids of the chords now at `at`.
    pub fn move_chords(&mut self, ids: &[u64], at: Insert, duplicate: bool) -> Vec<u64> {
        self.chords_edit(|this| {
            let mut taken: Vec<Slot> = this
                .placed()
                .into_iter()
                .map(|p| p.slot)
                .filter(|s| ids.contains(&s.id))
                .collect();
            if taken.is_empty() {
                return Vec::new();
            }
            if duplicate {
                let mut room = limit() - this.chords_end();
                taken.retain(|s| {
                    let fits = s.beats <= room + EPS;
                    if fits {
                        room -= s.beats;
                    }
                    fits
                });
                if taken.is_empty() {
                    return Vec::new();
                }
                for slot in &mut taken {
                    slot.id = this.fresh_id();
                }
            }
            // Mark the destination, take the moved chords out, then put them
            // where the mark ended up: indices stay right whatever was
            // removed before the destination.
            this.put(
                at,
                vec![Slot {
                    id: SENTINEL,
                    chord: taken[0].chord,
                    beats: 0.0,
                    locked: false,
                }],
            );
            if !duplicate {
                for section in &mut this.sections {
                    section.slots.retain(|s| !ids.contains(&s.id));
                }
                this.sections.retain(|s| !s.slots.is_empty());
            }
            let new_ids = taken.iter().map(|s| s.id).collect();
            for section in &mut this.sections {
                if let Some(mark) = section.slots.iter().position(|s| s.id == SENTINEL) {
                    section.slots.splice(mark..=mark, taken);
                    break;
                }
            }
            new_ids
        })
    }

    /// Give chord `id` a new length, as far as the maximum allows.
    pub fn resize_chord(&mut self, id: u64, beats: f64) -> bool {
        self.chords_edit(|this| {
            let total = this.chords_end();
            let Some(slot) = this.slot_mut(id) else {
                return false;
            };
            let room = limit() - (total - slot.beats);
            slot.beats = beats.max(MIN_LENGTH).min(room);
            true
        })
    }

    pub fn remove_chords(&mut self, ids: &[u64]) -> bool {
        self.chords_edit(|this| {
            let before = this.chord_count();
            for section in &mut this.sections {
                section.slots.retain(|s| !ids.contains(&s.id));
            }
            this.sections.retain(|s| !s.slots.is_empty());
            this.chord_count() != before
        })
    }

    pub fn set_chord(&mut self, id: u64, chord: Chord, locked: bool) -> bool {
        match self.slot_mut(id) {
            Some(slot) => {
                slot.chord = chord;
                slot.locked = locked;
                true
            }
            None => false,
        }
    }

    pub fn set_locked(&mut self, id: u64, locked: bool) -> bool {
        match self.slot_mut(id) {
            Some(slot) => {
                slot.locked = locked;
                true
            }
            None => false,
        }
    }

    /// Move section `id` to sequence index `to` (an index of the sequence as
    /// it is now; `len` moves it to the end), or put a copy there with new
    /// ids when `duplicate`. Returns the id of the section now at `to`.
    pub fn move_section(&mut self, id: u64, to: usize, duplicate: bool) -> Option<u64> {
        self.chords_edit(|this| {
            let from = this.section_index(id)?;
            let to = to.min(this.sections.len());
            if duplicate {
                let mut copy = this.sections[from].clone();
                if copy.beats() > limit() - this.chords_end() + EPS {
                    return None;
                }
                copy.id = this.fresh_id();
                for slot in &mut copy.slots {
                    slot.id = this.fresh_id();
                }
                let new_id = copy.id;
                this.sections.insert(to, copy);
                Some(new_id)
            } else {
                let section = this.sections.remove(from);
                let to = if to > from { to - 1 } else { to };
                this.sections.insert(to, section);
                Some(id)
            }
        })
    }

    pub fn remove_section(&mut self, id: u64) -> bool {
        self.chords_edit(|this| {
            let before = this.sections.len();
            this.sections.retain(|s| s.id != id);
            this.sections.len() != before
        })
    }

    /// Write `chords` into section `id`'s unlocked chords, in order (cycling
    /// when there are fewer chords than places), keeping their lengths. With
    /// no such section, a new one is appended with a bar per chord. Returns
    /// the section's id.
    pub fn fill_section(&mut self, id: Option<u64>, chords: &[Chord]) -> Option<u64> {
        if chords.is_empty() {
            return None;
        }
        if let Some(index) = id.and_then(|id| self.section_index(id)) {
            let section = &mut self.sections[index];
            for (i, slot) in section.slots.iter_mut().enumerate() {
                if !slot.locked {
                    slot.chord = chords[i % chords.len()];
                }
            }
            return Some(section.id);
        }
        let items: Vec<(Chord, f64)> = chords.iter().map(|&c| (c, BEATS_PER_BAR)).collect();
        let at = Insert::NewSection(self.sections.len());
        let ids = self.insert_chords(at, &items);
        ids.first()
            .and_then(|&first| self.section_of(first).map(|index| self.sections[index].id))
    }

    /// Shift every chord by `semitones` (a key change moves the whole song).
    pub fn transpose(&mut self, semitones: i32) {
        let shift = |pc: u8| (pc as i32 + semitones).rem_euclid(12) as u8;
        for section in &mut self.sections {
            for slot in &mut section.slots {
                slot.chord.root = shift(slot.chord.root);
                slot.chord.bass = slot.chord.bass.map(shift);
            }
        }
    }

    pub fn clear(&mut self) {
        self.sections.clear();
        self.regions.clear();
    }

    // ── Pattern edits ───────────────────────────────────────────────────────

    /// Place `pattern` over `[start, start + length)`, overwriting what its
    /// lane holds there. Returns the new region's id, or `None` when none of
    /// it fits.
    pub fn place_pattern(&mut self, start: f64, length: f64, pattern: Pattern) -> Option<u64> {
        let id = self.fresh_id();
        self.insert_region(Region {
            id,
            start,
            length,
            pattern,
        })
        .then_some(id)
    }

    /// Move a region to start at `start`, overwriting what it lands on.
    pub fn move_region(&mut self, id: u64, start: f64) -> bool {
        let Some(region) = self.take_region(id) else {
            return false;
        };
        self.insert_region(Region { start, ..region })
    }

    /// Give a region a new length, overwriting what it grows over.
    pub fn resize_region(&mut self, id: u64, length: f64) -> bool {
        let Some(region) = self.take_region(id) else {
            return false;
        };
        self.insert_region(Region {
            length: length.max(MIN_LENGTH),
            ..region
        })
    }

    pub fn remove_region(&mut self, id: u64) -> bool {
        self.take_region(id).is_some()
    }

    /// Replace a region's pattern with another of the same lane.
    pub fn set_pattern(&mut self, id: u64, pattern: Pattern) -> bool {
        match self.regions.iter_mut().find(|r| r.id == id) {
            Some(region) if region.lane() == pattern.lane() => {
                region.pattern = pattern;
                true
            }
            _ => false,
        }
    }

    /// The empty stretch of `lane` around `beat`, within the chords (at
    /// least a bar when there are none): `(start, length)`, or `None` when a
    /// region is already there.
    pub fn gap_at(&self, lane: Lane, beat: f64) -> Option<(f64, f64)> {
        if self.region_at(lane, beat).is_some() {
            return None;
        }
        let start = self
            .lane(lane)
            .map(Region::end)
            .filter(|&end| end <= beat + EPS)
            .fold(0.0, f64::max);
        let next = self
            .lane(lane)
            .map(|r| r.start)
            .filter(|&s| s > beat)
            .fold(f64::INFINITY, f64::min);
        let end = self
            .chords_end()
            .max(start + BEATS_PER_BAR)
            .min(next)
            .min(limit());
        (end - start >= MIN_LENGTH - EPS).then_some((start, end - start))
    }

    // ── Internals ───────────────────────────────────────────────────────────

    fn fresh_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn slot_mut(&mut self, id: u64) -> Option<&mut Slot> {
        self.sections
            .iter_mut()
            .flat_map(|s| s.slots.iter_mut())
            .find(|s| s.id == id)
    }

    /// Insert `slots` at `at`. A target section that no longer exists
    /// becomes a new section at the end.
    fn put(&mut self, at: Insert, slots: Vec<Slot>) {
        match at {
            Insert::Into { section, index } => {
                if let Some(i) = self.section_index(section) {
                    let target = &mut self.sections[i].slots;
                    let index = index.min(target.len());
                    target.splice(index..index, slots);
                    return;
                }
                let id = self.fresh_id();
                self.sections.push(Section { id, slots });
            }
            Insert::NewSection(index) => {
                let id = self.fresh_id();
                let index = index.min(self.sections.len());
                self.sections.insert(index, Section { id, slots });
            }
        }
    }

    /// Run a sequence edit, then let pattern regions that ran to the end of
    /// the chords follow the new end.
    fn chords_edit<R>(&mut self, edit: impl FnOnce(&mut Self) -> R) -> R {
        let old_end = self.chords_end();
        let result = edit(self);
        let new_end = self.chords_end();
        if (new_end - old_end).abs() > EPS && old_end > EPS {
            self.follow_chords_end(old_end, new_end);
        }
        self.grow_to_fit();
        result
    }

    fn follow_chords_end(&mut self, old_end: f64, new_end: f64) {
        let followers: Vec<Region> = self
            .regions
            .iter()
            .copied()
            .filter(|r| (r.end() - old_end).abs() < 1e-6)
            .collect();
        for region in followers {
            // Not into the next region of its lane.
            let next = self
                .lane(region.lane())
                .map(|r| r.start)
                .filter(|&s| s > region.start + EPS && s >= old_end - EPS)
                .fold(f64::INFINITY, f64::min);
            let end = new_end.min(next);
            if let Some(r) = self.regions.iter_mut().find(|r| r.id == region.id) {
                r.length = end - r.start;
            }
        }
        self.regions.retain(|r| r.length >= MIN_LENGTH - EPS);
    }

    fn take_region(&mut self, id: u64) -> Option<Region> {
        let index = self.regions.iter().position(|r| r.id == id)?;
        Some(self.regions.remove(index))
    }

    /// Insert a region (keeping its id), clamped to the maximum and
    /// overwriting its lane under it.
    fn insert_region(&mut self, region: Region) -> bool {
        let start = region.start.max(0.0);
        let end = (region.start + region.length).min(limit());
        if end - start < MIN_LENGTH - EPS {
            return false;
        }
        let lane = region.lane();
        let mut kept = Vec::with_capacity(self.regions.len() + 1);
        for r in std::mem::take(&mut self.regions) {
            if r.lane() != lane || r.end() <= start + EPS || r.start >= end - EPS {
                kept.push(r);
                continue;
            }
            // Keep what sticks out on either side.
            if r.start < start - EPS {
                kept.push(Region {
                    length: start - r.start,
                    ..r
                });
            }
            if r.end() > end + EPS {
                let id = if r.start < start - EPS {
                    self.fresh_id()
                } else {
                    r.id
                };
                kept.push(Region {
                    id,
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
        self.grow_to_fit();
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

    fn major(root: u8) -> Chord {
        Chord::new(root, ChordQuality::Major)
    }

    fn bar(root: u8) -> (Chord, f64) {
        (major(root), BEATS_PER_BAR)
    }

    /// Roots per section.
    fn roots(a: &Arrangement) -> Vec<Vec<u8>> {
        a.sections()
            .iter()
            .map(|s| s.slots.iter().map(|slot| slot.chord.root).collect())
            .collect()
    }

    fn two_sections() -> (Arrangement, Vec<u64>, Vec<u64>) {
        let mut a = Arrangement::default();
        let first = a.insert_chords(Insert::NewSection(0), &[bar(0), bar(9), bar(5), bar(7)]);
        let second = a.insert_chords(Insert::NewSection(1), &[bar(2), bar(7)]);
        (a, first, second)
    }

    #[test]
    fn chords_lay_back_to_back_across_sections() {
        let (a, _, _) = two_sections();
        assert_eq!(roots(&a), vec![vec![0, 9, 5, 7], vec![2, 7]]);
        let starts: Vec<f64> = a.placed().iter().map(|p| p.start).collect();
        assert_eq!(starts, vec![0.0, 4.0, 8.0, 12.0, 16.0, 20.0]);
        assert_eq!(a.chords_end(), 24.0);
        assert_eq!(a.section_span(a.sections()[1].id), Some((16.0, 8.0)));
    }

    #[test]
    fn inserting_goes_into_a_section_or_makes_one() {
        let (mut a, _, _) = two_sections();
        let second = a.sections()[1].id;
        a.insert_chords(
            Insert::Into {
                section: second,
                index: 1,
            },
            &[bar(4)],
        );
        assert_eq!(roots(&a), vec![vec![0, 9, 5, 7], vec![2, 4, 7]]);
        a.insert_chords(Insert::NewSection(1), &[bar(11)]);
        assert_eq!(roots(&a), vec![vec![0, 9, 5, 7], vec![11], vec![2, 4, 7]]);
        assert_eq!(a.insert_beat(Insert::NewSection(2)), 20.0);
        assert_eq!(
            a.insert_beat(Insert::Into {
                section: second,
                index: 2
            }),
            28.0
        );
    }

    #[test]
    fn moving_chords_keeps_their_ids_and_drops_emptied_sections() {
        let (mut a, first, second) = two_sections();
        // Am from the first section to the end of the second.
        let target = a.sections()[1].id;
        let moved = a.move_chords(
            &[first[1]],
            Insert::Into {
                section: target,
                index: 2,
            },
            false,
        );
        assert_eq!(moved, vec![first[1]]);
        assert_eq!(roots(&a), vec![vec![0, 5, 7], vec![2, 7, 9]]);
        // Both chords of the second section out to a new first section: the
        // second section empties and goes.
        let ids = [second[0], second[1], first[1]];
        a.move_chords(&ids, Insert::NewSection(0), false);
        assert_eq!(roots(&a), vec![vec![2, 7, 9], vec![0, 5, 7]]);
        assert_eq!(a.sections().len(), 2);
    }

    #[test]
    fn moving_within_a_section_lands_where_aimed() {
        let (mut a, first, _) = two_sections();
        let section = a.sections()[0].id;
        // C to just before G (index 3 counted with C still in place).
        a.move_chords(&[first[0]], Insert::Into { section, index: 3 }, false);
        assert_eq!(roots(&a)[0], vec![9, 5, 0, 7]);
    }

    #[test]
    fn duplicating_copies_with_new_ids() {
        let (mut a, first, _) = two_sections();
        let copies = a.move_chords(&[first[0], first[1]], Insert::NewSection(2), true);
        assert_eq!(copies.len(), 2);
        assert!(copies.iter().all(|id| !first.contains(id)));
        assert_eq!(roots(&a), vec![vec![0, 9, 5, 7], vec![2, 7], vec![0, 9]]);
    }

    #[test]
    fn sections_reorder_and_duplicate() {
        let (mut a, _, _) = two_sections();
        let (s0, s1) = (a.sections()[0].id, a.sections()[1].id);
        a.move_section(s0, 2, false);
        assert_eq!(a.sections()[0].id, s1);
        assert_eq!(a.sections()[1].id, s0);
        let copy = a.move_section(s1, 0, true).unwrap();
        assert_ne!(copy, s1);
        assert_eq!(roots(&a), vec![vec![2, 7], vec![2, 7], vec![0, 9, 5, 7]]);
    }

    #[test]
    fn removing_the_last_chord_removes_its_section() {
        let (mut a, _, second) = two_sections();
        a.remove_chords(&second);
        assert_eq!(a.sections().len(), 1);
    }

    #[test]
    fn resizing_has_a_floor_and_a_ceiling() {
        let (mut a, first, _) = two_sections();
        a.resize_chord(first[0], 0.25);
        assert_eq!(a.find_slot(first[0]).unwrap().slot.beats, MIN_LENGTH);
        a.resize_chord(first[0], 1_000.0);
        assert_eq!(a.chords_end(), limit());
        assert_eq!(a.bars(), MAX_BARS);
    }

    #[test]
    fn nothing_is_placed_past_the_maximum() {
        let mut a = Arrangement::default();
        let long: Vec<(Chord, f64)> = (0..40).map(|_| bar(0)).collect();
        let ids = a.insert_chords(Insert::NewSection(0), &long);
        assert_eq!(ids.len(), MAX_BARS as usize);
        assert!(a.insert_chords(Insert::NewSection(1), &[bar(7)]).is_empty());
    }

    #[test]
    fn fill_rewrites_unlocked_chords_and_keeps_lengths() {
        let (mut a, first, _) = two_sections();
        a.resize_chord(first[2], 2.0);
        a.set_locked(first[1], true);
        let section = a.sections()[0].id;
        a.fill_section(Some(section), &[major(2), major(4)]);
        let slots = &a.sections()[0].slots;
        let got: Vec<(u8, f64)> = slots.iter().map(|s| (s.chord.root, s.beats)).collect();
        assert_eq!(got, vec![(2, 4.0), (9, 4.0), (2, 2.0), (4, 4.0)]);
        // Without a section: a new one at the end, a bar a chord.
        let new = a.fill_section(None, &[major(5), major(7)]).unwrap();
        assert_eq!(a.sections().last().unwrap().id, new);
        assert_eq!(a.sections().last().unwrap().beats(), 8.0);
    }

    #[test]
    fn patterns_that_reach_the_end_follow_it() {
        let (mut a, first, second) = two_sections();
        let comp = a
            .place_pattern(0.0, 24.0, Pattern::Comp(CompPattern::Strum))
            .unwrap();
        let bass = a
            .place_pattern(0.0, 8.0, Pattern::Bass(BassPattern::Root))
            .unwrap();
        a.insert_chords(Insert::NewSection(2), &[bar(0)]);
        assert_eq!(a.find_region(comp).unwrap().end(), 28.0);
        // One that stopped short stays as it was.
        assert_eq!(a.find_region(bass).unwrap().end(), 8.0);
        a.remove_chords(&second);
        assert_eq!(a.find_region(comp).unwrap().end(), 20.0);
        a.remove_chords(&first);
        assert_eq!(a.find_region(comp).unwrap().end(), 4.0);
    }

    #[test]
    fn patterns_overwrite_their_own_lane_only() {
        let mut a = Arrangement::default();
        a.place_pattern(0.0, 16.0, Pattern::Comp(CompPattern::Sustain));
        a.place_pattern(0.0, 16.0, Pattern::Bass(BassPattern::Root));
        a.place_pattern(4.0, 4.0, Pattern::Comp(CompPattern::Strum));
        let comp: Vec<(f64, f64)> = a.lane(Lane::Comp).map(|r| (r.start, r.end())).collect();
        assert_eq!(comp, vec![(0.0, 4.0), (4.0, 8.0), (8.0, 16.0)]);
        assert_eq!(a.bass_spans().len(), 1);
    }

    #[test]
    fn a_gap_is_filled_up_to_the_chords_or_the_next_pattern() {
        let (mut a, _, _) = two_sections();
        a.place_pattern(8.0, 4.0, Pattern::Comp(CompPattern::Strum));
        assert_eq!(a.gap_at(Lane::Comp, 2.0), Some((0.0, 8.0)));
        assert_eq!(a.gap_at(Lane::Comp, 13.0), Some((12.0, 12.0)));
        assert_eq!(a.gap_at(Lane::Comp, 9.0), None);
        assert_eq!(a.gap_at(Lane::Bass, 0.0), Some((0.0, 24.0)));
    }

    #[test]
    fn transposing_moves_roots_and_slash_basses() {
        let mut a = Arrangement::default();
        let mut c_over_e = major(0);
        c_over_e.bass = Some(4);
        a.insert_chords(Insert::NewSection(0), &[(c_over_e, 4.0)]);
        a.transpose(-3);
        let chord = a.chord_spans()[0].chord;
        assert_eq!((chord.root, chord.bass), (9, Some(1)));
    }

    #[test]
    fn undo_and_redo_walk_the_history() {
        let mut a = Arrangement::default();
        let mut history = History::default();
        history.record(a.clone());
        a.insert_chords(Insert::NewSection(0), &[bar(0)]);
        history.record(a.clone());
        a.insert_chords(Insert::NewSection(1), &[bar(7)]);
        assert!(history.undo(&mut a));
        assert_eq!(a.chord_count(), 1);
        assert!(history.undo(&mut a));
        assert!(a.is_empty());
        assert!(!history.undo(&mut a));
        assert!(history.redo(&mut a));
        assert_eq!(a.chord_count(), 1);
    }
}

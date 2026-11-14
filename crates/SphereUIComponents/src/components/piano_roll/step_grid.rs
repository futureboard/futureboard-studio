//! The step sequencer's view of a MIDI clip.
//!
//! The step grid is not a second copy of the pattern: the clip's notes are the
//! pattern. Reading them onto the grid ("MIDI → Step") groups each drum lane's
//! notes by the step they start in; writing a step ("Step → MIDI") turns it
//! back into ordinary notes, so a step pattern plays, saves, undoes, exports
//! and opens in the piano roll exactly like any other MIDI clip.
//!
//! A step can repeat — a ratchet — `repeat` times inside its length: that is
//! `repeat` evenly spaced notes, each one sub-step long. A group of notes that
//! is not such a ratchet (a hit played between the lines, or an uneven roll)
//! is shown as **off grid** and left exactly as it is until the user snaps it.
//!
//! Everything here is pure, so the mapping is tested without a window.

use crate::components::timeline::timeline_state::{MidiChannel, MidiNoteState};

/// The repeat counts a step can take, in the order the selector offers them.
pub(crate) const STEP_REPEATS: [u8; 6] = [1, 2, 3, 4, 6, 8];

/// How far from its line a note may start and still count as on it, in
/// beats. A 1/64 triplet is ~0.02 beats, so this tolerates float drift and a
/// sample or two of recorded jitter without swallowing a real off-beat.
const ON_LINE_BEATS: f32 = 0.002;

/// Velocity of a step the user adds.
pub(crate) const STEP_VELOCITY: u8 = 100;

/// One filled step of one lane.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StepCell {
    /// Step index from the clip start.
    pub index: usize,
    /// The notes that make it, in start order.
    pub ids: Vec<u64>,
    /// The loudest of them, which is what the cell shows.
    pub velocity: u8,
    /// How many hits the step plays.
    pub repeat: u8,
    /// The notes are exactly a step (or an even ratchet) on its line.
    pub on_grid: bool,
    /// Every note of the step is muted.
    pub muted: bool,
    /// Channel of the first note, kept when the step is rewritten.
    pub channel: MidiChannel,
}

/// The step `beat` falls in. `None` before the clip start.
pub(crate) fn step_index(beat: f32, step: f32) -> Option<usize> {
    if step <= 0.0 || beat < -ON_LINE_BEATS {
        return None;
    }
    Some(((beat + ON_LINE_BEATS) / step).floor().max(0.0) as usize)
}

/// How many steps a clip of `clip_len` beats holds: every step that starts
/// inside it.
pub(crate) fn step_count(clip_len: f32, step: f32) -> usize {
    if step <= 0.0 || clip_len <= 0.0 {
        return 0;
    }
    ((clip_len - ON_LINE_BEATS) / step).floor().max(0.0) as usize + 1
}

/// The filled steps of lane `pitch`, in step order.
pub(crate) fn lane_cells(notes: &[MidiNoteState], pitch: u8, step: f32) -> Vec<StepCell> {
    let mut lane: Vec<&MidiNoteState> = notes.iter().filter(|n| n.pitch == pitch).collect();
    lane.sort_by(|a, b| a.start.total_cmp(&b.start));
    let mut cells: Vec<StepCell> = Vec::new();
    let mut group: Vec<&MidiNoteState> = Vec::new();
    let mut group_index = None;
    for note in lane {
        let Some(index) = step_index(note.start, step) else {
            continue;
        };
        if group_index != Some(index) && !group.is_empty() {
            cells.push(cell_of(&group, group_index.unwrap_or(0), step));
            group.clear();
        }
        group_index = Some(index);
        group.push(note);
    }
    if let Some(index) = group_index.filter(|_| !group.is_empty()) {
        cells.push(cell_of(&group, index, step));
    }
    cells
}

fn cell_of(group: &[&MidiNoteState], index: usize, step: f32) -> StepCell {
    let count = group.len();
    let sub = step / count as f32;
    let line = index as f32 * step;
    let on_grid = count <= usize::from(u8::MAX)
        && group
            .iter()
            .enumerate()
            .all(|(i, note)| (note.start - (line + i as f32 * sub)).abs() <= ON_LINE_BEATS);
    StepCell {
        index,
        ids: group.iter().map(|note| note.id).collect(),
        velocity: group.iter().map(|note| note.velocity).max().unwrap_or(1),
        repeat: count.min(usize::from(u8::MAX)) as u8,
        on_grid,
        muted: group.iter().all(|note| note.muted),
        channel: group[0].channel,
    }
}

/// The notes one step plays: `repeat` hits spread evenly over the step, each
/// as long as its share of it.
pub(crate) fn step_notes(
    pitch: u8,
    index: usize,
    step: f32,
    repeat: u8,
    velocity: u8,
    channel: MidiChannel,
) -> Vec<MidiNoteState> {
    let repeat = repeat.max(1);
    let sub = step / f32::from(repeat);
    (0..repeat)
        .map(|i| {
            let mut note = MidiNoteState::new(
                pitch,
                index as f32 * step + f32::from(i) * sub,
                sub,
                velocity.clamp(1, 127),
            );
            note.channel = channel;
            note
        })
        .collect()
}

/// Every off-grid note of `notes` moved onto the nearest step line, and the
/// notes that then land on a line already played in their lane — which would
/// sound as one doubled hit — to be removed.
///
/// Returns `(moved, dropped)`: the moved notes keep their ids, so expression
/// and selection travel with them; `dropped` are originals to delete.
pub(crate) fn snap_off_grid(
    notes: &[MidiNoteState],
    step: f32,
) -> (Vec<MidiNoteState>, Vec<MidiNoteState>) {
    let mut pitches: Vec<u8> = notes.iter().map(|n| n.pitch).collect();
    pitches.sort_unstable();
    pitches.dedup();
    let mut moved = Vec::new();
    let mut dropped = Vec::new();
    for pitch in pitches {
        let cells = lane_cells(notes, pitch, step);
        // Lines this lane already plays on grid; a snapped hit that lands on
        // one would double it.
        let mut taken: Vec<usize> = cells
            .iter()
            .filter(|cell| cell.on_grid)
            .map(|cell| cell.index)
            .collect();
        for cell in cells.iter().filter(|cell| !cell.on_grid) {
            for id in &cell.ids {
                let Some(note) = notes.iter().find(|n| n.id == *id) else {
                    continue;
                };
                let index = (note.start.max(0.0) / step).round() as usize;
                if taken.contains(&index) {
                    dropped.push(note.clone());
                    continue;
                }
                taken.push(index);
                let line = index as f32 * step;
                // A note already on its line (beside a stray hit in the same
                // step) stays where it is; it only claims the line.
                if (note.start - line).abs() > ON_LINE_BEATS {
                    let mut snapped = note.clone();
                    snapped.start = line;
                    moved.push(snapped);
                }
            }
        }
    }
    (moved, dropped)
}

/// How many notes of `notes` sit off the step grid.
pub(crate) fn off_grid_count(notes: &[MidiNoteState], step: f32) -> usize {
    let mut pitches: Vec<u8> = notes.iter().map(|n| n.pitch).collect();
    pitches.sort_unstable();
    pitches.dedup();
    pitches
        .into_iter()
        .flat_map(|pitch| lane_cells(notes, pitch, step))
        .filter(|cell| !cell.on_grid)
        .map(|cell| cell.ids.len())
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIXTEENTH: f32 = 0.25;

    fn note(pitch: u8, start: f32, duration: f32) -> MidiNoteState {
        MidiNoteState::new(pitch, start, duration, 100)
    }

    #[test]
    fn a_hit_on_a_line_is_one_step() {
        let notes = vec![note(36, 0.0, 0.25), note(36, 1.0, 0.1), note(38, 0.5, 0.25)];
        let kick = lane_cells(&notes, 36, SIXTEENTH);
        assert_eq!(kick.len(), 2);
        assert_eq!(
            (kick[0].index, kick[0].repeat, kick[0].on_grid),
            (0, 1, true)
        );
        assert_eq!(
            (kick[1].index, kick[1].repeat, kick[1].on_grid),
            (4, 1, true)
        );
        // A note's length does not move it off the grid.
        assert!(lane_cells(&notes, 38, SIXTEENTH)[0].on_grid);
    }

    /// Step → MIDI → Step: every repeat count comes back as itself.
    #[test]
    fn every_repeat_reads_back_as_the_step_it_was_written_as() {
        for repeat in STEP_REPEATS {
            let notes = step_notes(36, 5, SIXTEENTH, repeat, 90, MidiChannel::default());
            assert_eq!(notes.len(), usize::from(repeat));
            let cells = lane_cells(&notes, 36, SIXTEENTH);
            assert_eq!(cells.len(), 1, "×{repeat}");
            let cell = &cells[0];
            assert_eq!(cell.index, 5);
            assert_eq!(cell.repeat, repeat);
            assert!(cell.on_grid, "×{repeat}");
            assert_eq!(cell.velocity, 90);
            // The hits fill the step and nothing past it.
            let end = notes
                .iter()
                .map(|n| n.start + n.duration)
                .fold(0.0_f32, f32::max);
            assert!((end - 6.0 * SIXTEENTH).abs() < 1e-5);
        }
    }

    #[test]
    fn a_hit_between_the_lines_is_off_grid_and_is_not_moved() {
        let notes = vec![note(42, 0.30, 0.1)];
        let cells = lane_cells(&notes, 42, SIXTEENTH);
        assert_eq!(cells[0].index, 1);
        assert!(!cells[0].on_grid);
        assert_eq!(off_grid_count(&notes, SIXTEENTH), 1);
        assert_eq!(notes[0].start, 0.30, "reading never edits");
    }

    #[test]
    fn an_uneven_roll_is_off_grid() {
        let notes = vec![note(38, 0.0, 0.05), note(38, 0.05, 0.05)];
        let cells = lane_cells(&notes, 38, SIXTEENTH);
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0].repeat, 2);
        assert!(!cells[0].on_grid);
    }

    /// Snapping keeps each note's id, lands it on the nearest line, and drops
    /// a hit that would double one already on that line.
    #[test]
    fn snapping_moves_off_grid_hits_and_never_doubles_one() {
        let on = note(36, 0.5, 0.25);
        let near_on = note(36, 0.52, 0.1);
        let late = note(36, 0.80, 0.1);
        let notes = vec![on.clone(), near_on.clone(), late.clone()];
        assert_eq!(off_grid_count(&notes, SIXTEENTH), 3);
        let (moved, dropped) = snap_off_grid(&notes, SIXTEENTH);
        assert_eq!(
            dropped.iter().map(|n| n.id).collect::<Vec<_>>(),
            vec![near_on.id]
        );
        assert_eq!(moved.len(), 1);
        assert_eq!(moved[0].id, late.id);
        assert!((moved[0].start - 0.75).abs() < 1e-6);

        // Applied, the lane is all on the grid: one hit per line.
        let after: Vec<MidiNoteState> = notes
            .iter()
            .filter(|n| !dropped.iter().any(|d| d.id == n.id))
            .map(|n| moved.iter().find(|m| m.id == n.id).unwrap_or(n).clone())
            .collect();
        assert_eq!(off_grid_count(&after, SIXTEENTH), 0);
        assert_eq!(lane_cells(&after, 36, SIXTEENTH).len(), 2);
    }

    #[test]
    fn steps_cover_the_whole_clip() {
        assert_eq!(step_count(4.0, SIXTEENTH), 16);
        assert_eq!(step_count(4.1, SIXTEENTH), 17);
        assert_eq!(step_count(0.0, SIXTEENTH), 0);
        assert_eq!(
            step_index(0.999, SIXTEENTH),
            Some(4),
            "within a hair of a line"
        );
        assert_eq!(step_index(-1.0, SIXTEENTH), None);
    }
}

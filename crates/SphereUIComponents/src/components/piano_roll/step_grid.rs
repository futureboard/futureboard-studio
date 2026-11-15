//! The step sequencer's view of a MIDI clip.
//!
//! The step grid is not a second copy of the pattern: the clip's notes are the
//! pattern. Reading them onto the grid ("MIDI → Step") groups each drum lane's
//! notes into steps; writing a step ("Step → MIDI") turns it back into
//! ordinary notes, so a step pattern plays, saves, undoes, exports and opens in
//! the piano roll exactly like any other MIDI clip.
//!
//! A step belongs to the step line its first hit is nearest to, and remembers
//! how far it sits from that line (its **offset**): a hit nudged early or late
//! for groove stays the step it was. A step can repeat — a ratchet — `repeat`
//! times inside its length: that is `repeat` evenly spaced notes, each one
//! sub-step long. A group that is not evenly spaced (an uneven roll) is shown
//! as **uneven** and left exactly as it is until the user snaps it.
//!
//! Every gesture of the step view is computed here — painting and clearing
//! along a row, erasing, the velocity ramp, the micro-nudge, and the row
//! generators — as pure functions over notes, so the mapping is tested without
//! a window. The editor applies what they return and records one undo entry
//! per gesture.

use rand::{Rng, SeedableRng};

use crate::components::timeline::timeline_state::{MidiChannel, MidiNoteState};

/// The repeat counts a step can take, in the order the selector offers them.
pub(crate) const STEP_REPEATS: [u8; 6] = [1, 2, 3, 4, 6, 8];

/// How far apart two starts may be and still count as the same place, in
/// beats. A 1/64 triplet is ~0.02 beats, so this tolerates float drift and a
/// sample or two of recorded jitter without swallowing a real off-beat.
const ON_LINE_BEATS: f32 = 0.002;

/// How far a step may be nudged off its line, as a share of the step: just
/// under half, so a nudged step can never be mistaken for its neighbour's.
pub(crate) const NUDGE_LIMIT: f32 = 0.49;

/// Velocity of a step the user adds.
pub(crate) const STEP_VELOCITY: u8 = 100;

/// Velocity a velocity ramp gains per logical pixel dragged upwards: a full
/// 1–127 sweep is about 150 px, a comfortable single drag.
pub(crate) const RAMP_VELOCITY_PER_PX: f32 = 127.0 / 150.0;

/// How far "Randomize Velocity" moves each hit, as a share of its velocity.
pub(crate) const RANDOM_VELOCITY_SPREAD: f32 = 0.15;

/// Vertical travel, in logical pixels, that moves a Shift-drag one repeat
/// count up or down: ×1 to ×8 is about 100 px.
pub(crate) const RATCHET_PX_PER_COUNT: f32 = 20.0;

/// The velocity shape of a step's repeated hits, as shares of the step's
/// velocity. A ratchet at one velocity sounds like a machine gun; a roll
/// that builds, dies away or swings sounds played.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum RepeatCurve {
    /// Every hit at the step's velocity.
    #[default]
    Flat,
    /// 35% rising to 100%: a snare or hi-hat build-up.
    RampUp,
    /// 100% falling to 25%: a roll dying away.
    Decay,
    /// 100%, 55%, 80%, 45%, repeating: a swung, accented roll.
    Accent,
}

impl RepeatCurve {
    pub(crate) const ALL: [Self; 4] = [Self::Flat, Self::RampUp, Self::Decay, Self::Accent];

    const ACCENTS: [f32; 4] = [1.0, 0.55, 0.80, 0.45];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Flat => "Flat",
            Self::RampUp => "Ramp Up",
            Self::Decay => "Decay",
            Self::Accent => "Accent",
        }
    }

    pub(crate) fn description(self) -> &'static str {
        match self {
            Self::Flat => "Repeat curve: every hit at the step's velocity",
            Self::RampUp => "Repeat curve: hits rise from 35% to 100% (build-ups)",
            Self::Decay => "Repeat curve: hits fall from 100% to 25% (roll fade-outs)",
            Self::Accent => "Repeat curve: accented groove, 100% 55% 80% 45%",
        }
    }

    /// Share of the step's velocity hit `hit` of `count` plays at.
    pub(crate) fn factor(self, hit: usize, count: usize) -> f32 {
        let t = if count > 1 {
            hit as f32 / (count - 1) as f32
        } else {
            0.0
        };
        match self {
            Self::Flat => 1.0,
            Self::RampUp if count > 1 => 0.35 + 0.65 * t,
            Self::Decay => 1.0 - 0.75 * t,
            Self::Accent => Self::ACCENTS[hit % Self::ACCENTS.len()],
            Self::RampUp => 1.0,
        }
    }

    /// `count` velocities for a step at `velocity`. The loudest is always
    /// `velocity` itself, so reading the step back finds the same velocity.
    pub(crate) fn velocities(self, velocity: u8, count: usize) -> Vec<u8> {
        (0..count)
            .map(|hit| {
                (f32::from(velocity) * self.factor(hit, count))
                    .round()
                    .clamp(1.0, 127.0) as u8
            })
            .collect()
    }

    /// The curve a step's hits were written with, if they match one. Several
    /// match a single hit or a flat pair; Flat wins, as the plainest.
    pub(crate) fn detect(velocities: &[u8]) -> Option<Self> {
        let loudest = velocities.iter().copied().max()?;
        Self::ALL.into_iter().find(|curve| {
            curve
                .velocities(loudest, velocities.len())
                .iter()
                .zip(velocities)
                .all(|(want, have)| want.abs_diff(*have) <= 1)
        })
    }
}

/// The repeat count `notches` steps along [`STEP_REPEATS`] from `from`
/// (positive: more hits), held at ×1 and ×8. A count not in the list (an
/// uneven roll of five) starts from the nearest one above it.
pub(crate) fn repeat_after(from: u8, notches: i32) -> u8 {
    let at = STEP_REPEATS
        .iter()
        .position(|repeat| *repeat >= from)
        .unwrap_or(STEP_REPEATS.len() - 1) as i32;
    let to = (at + notches).clamp(0, STEP_REPEATS.len() as i32 - 1);
    STEP_REPEATS[to as usize]
}

/// One filled step of one lane.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StepCell {
    /// Step index from the clip start.
    pub index: usize,
    /// The notes that make it, in start order.
    pub ids: Vec<u64>,
    /// Each hit's velocity, in start order: the ratchet's slices.
    pub velocities: Vec<u8>,
    /// The loudest hit, which is what a single-hit step shows.
    pub velocity: u8,
    /// How many hits the step plays.
    pub repeat: u8,
    /// Where the first hit sits relative to the step line, in beats
    /// (negative: early). Within ±[`NUDGE_LIMIT`] of a step.
    pub offset: f32,
    /// The hits are evenly spaced across the step — one hit, or a ratchet.
    pub even: bool,
    /// Every note of the step is muted.
    pub muted: bool,
    /// Channel of the first note, kept when the step is rewritten.
    pub channel: MidiChannel,
}

impl StepCell {
    /// Exactly on its line and evenly spaced: nothing to snap.
    pub(crate) fn on_grid(&self) -> bool {
        self.even && self.offset.abs() <= ON_LINE_BEATS
    }
}

/// The step box `beat` falls in — the cell drawn under the pointer. `None`
/// before the clip start.
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
///
/// A step starts at a hit and takes every later hit of the lane less than a
/// step after it; it belongs to the line nearest its first hit. Two groups
/// that land on one line (two stray hits close together) are one uneven step.
pub(crate) fn lane_cells(notes: &[MidiNoteState], pitch: u8, step: f32) -> Vec<StepCell> {
    if step <= 0.0 {
        return Vec::new();
    }
    let mut lane: Vec<&MidiNoteState> = notes
        .iter()
        .filter(|n| n.pitch == pitch && n.start >= -step * 0.5)
        .collect();
    lane.sort_by(|a, b| a.start.total_cmp(&b.start));
    let mut groups: Vec<(usize, Vec<&MidiNoteState>)> = Vec::new();
    for note in lane {
        if let Some((_, group)) = groups.last_mut() {
            if note.start < group[0].start + step - ON_LINE_BEATS {
                group.push(note);
                continue;
            }
        }
        let index = (note.start.max(0.0) / step).round() as usize;
        match groups.last_mut() {
            Some((last, group)) if *last == index => group.push(note),
            _ => groups.push((index, vec![note])),
        }
    }
    groups
        .into_iter()
        .map(|(index, group)| cell_of(&group, index, step))
        .collect()
}

fn cell_of(group: &[&MidiNoteState], index: usize, step: f32) -> StepCell {
    let count = group.len();
    let first = group[0].start;
    let sub = step / count as f32;
    let even = group
        .iter()
        .enumerate()
        .all(|(i, note)| (note.start - (first + i as f32 * sub)).abs() <= ON_LINE_BEATS);
    let velocities: Vec<u8> = group.iter().map(|note| note.velocity).collect();
    StepCell {
        index,
        ids: group.iter().map(|note| note.id).collect(),
        velocity: velocities.iter().copied().max().unwrap_or(1),
        velocities,
        repeat: count.min(usize::from(u8::MAX)) as u8,
        offset: first - index as f32 * step,
        even,
        muted: group.iter().all(|note| note.muted),
        channel: group[0].channel,
    }
}

/// The step of lane `pitch` at `index`, if it is filled.
pub(crate) fn cell_at(
    notes: &[MidiNoteState],
    pitch: u8,
    index: usize,
    step: f32,
) -> Option<StepCell> {
    lane_cells(notes, pitch, step)
        .into_iter()
        .find(|cell| cell.index == index)
}

/// The notes one step plays: `repeat` hits spread evenly over the step from
/// its line, each as long as its share of it, their velocities `curve`
/// shaped from `velocity`.
pub(crate) fn step_notes(
    pitch: u8,
    index: usize,
    step: f32,
    repeat: u8,
    velocity: u8,
    curve: RepeatCurve,
    channel: MidiChannel,
) -> Vec<MidiNoteState> {
    let repeat = repeat.max(1);
    let sub = step / f32::from(repeat);
    curve
        .velocities(velocity.clamp(1, 127), usize::from(repeat))
        .into_iter()
        .enumerate()
        .map(|(i, velocity)| {
            let mut note =
                MidiNoteState::new(pitch, index as f32 * step + i as f32 * sub, sub, velocity);
            note.channel = channel;
            note
        })
        .collect()
}

/// `cell` written again with `repeat` hits shaped by `curve`: same lane,
/// line, nudge, channel, mute and velocity (its loudest hit). New notes; the
/// caller swaps them for the cell's.
pub(crate) fn rewritten_step(
    pitch: u8,
    cell: &StepCell,
    step: f32,
    repeat: u8,
    curve: RepeatCurve,
) -> Vec<MidiNoteState> {
    step_notes(
        pitch,
        cell.index,
        step,
        repeat,
        cell.velocity,
        curve,
        cell.channel,
    )
    .into_iter()
    .map(|mut note| {
        note.start = (note.start + cell.offset).max(0.0);
        note.muted = cell.muted;
        note
    })
    .collect()
}

/// The filled steps holding any of `ids`, with their lanes, in lane then
/// step order.
pub(crate) fn cells_holding(
    notes: &[MidiNoteState],
    ids: &[u64],
    step: f32,
) -> Vec<(u8, StepCell)> {
    let mut pitches: Vec<u8> = notes
        .iter()
        .filter(|n| ids.contains(&n.id))
        .map(|n| n.pitch)
        .collect();
    pitches.sort_unstable();
    pitches.dedup();
    pitches
        .into_iter()
        .flat_map(|pitch| {
            lane_cells(notes, pitch, step)
                .into_iter()
                .filter(|cell| cell.ids.iter().any(|id| ids.contains(id)))
                .map(move |cell| (pitch, cell))
        })
        .collect()
}

/// The selected steps rewritten: each with `repeat` hits (or its own count)
/// and `curve` (or the curve its hits already follow, else Flat). Returns
/// `(removed, added)` for one replace; both empty when nothing would change.
pub(crate) fn rewrite_steps(
    notes: &[MidiNoteState],
    cells: &[(u8, StepCell)],
    step: f32,
    repeat: Option<u8>,
    curve: Option<RepeatCurve>,
) -> (Vec<MidiNoteState>, Vec<MidiNoteState>) {
    let mut removed = Vec::new();
    let mut added = Vec::new();
    for (pitch, cell) in cells {
        let count = repeat.unwrap_or(cell.repeat);
        let shape = curve
            .or_else(|| RepeatCurve::detect(&cell.velocities))
            .unwrap_or_default();
        let next = rewritten_step(*pitch, cell, step, count, shape);
        let before = notes_with(notes, &cell.ids);
        if same_hits(&before, &next) {
            continue;
        }
        removed.extend(before);
        added.extend(next);
    }
    (removed, added)
}

/// Whether two sets of hits play the same: starts, lengths and velocities.
fn same_hits(a: &[MidiNoteState], b: &[MidiNoteState]) -> bool {
    let key = |notes: &[MidiNoteState]| {
        let mut hits: Vec<(i64, i64, u8)> = notes
            .iter()
            .map(|n| {
                (
                    (n.start / ON_LINE_BEATS).round() as i64,
                    (n.duration / ON_LINE_BEATS).round() as i64,
                    n.velocity,
                )
            })
            .collect();
        hits.sort_unstable();
        hits
    };
    key(a) == key(b)
}

/// The notes of `ids` among `notes`, in the order `notes` holds them.
pub(crate) fn notes_with(notes: &[MidiNoteState], ids: &[u64]) -> Vec<MidiNoteState> {
    notes
        .iter()
        .filter(|n| ids.contains(&n.id))
        .cloned()
        .collect()
}

// ── Gestures ───────────────────────────────────────────────────────────────

/// What a press in the step view does while the button is held.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum StepGesture {
    /// Left press on an empty step: fill every empty step the pointer crosses
    /// along that row; filled steps are left alone.
    Paint,
    /// Left press on a filled step: clear it and every filled step the
    /// pointer crosses along that row.
    Clear,
    /// Right press: clear every filled step the pointer crosses, on any row.
    Erase,
    /// Alt + left press: the row's hits from the pressed step to the pointer
    /// take velocities ramped from the pressed step's to the one the pointer's
    /// height sets.
    VelocityRamp { from_velocity: u8 },
    /// Ctrl/Cmd + left press on a filled step: slide that step early or late,
    /// up to just under half a step.
    Nudge { offset: f32 },
    /// Shift + left press: dragging up or down steps the pressed step's
    /// repeat count through [`STEP_REPEATS`], from `from_repeat`, keeping its
    /// curve. A press that never moves gives it the toolbar's count instead.
    Repeat {
        from_repeat: u8,
        curve: RepeatCurve,
        /// Moved far enough to be a drag rather than a click.
        dragged: bool,
    },
}

/// One press-drag-release in the step view.
///
/// The editor applies each move's result to the clip at once, so the change is
/// seen and heard as it is made, and on release records the whole gesture as
/// one undo entry: `removed`/`added` for the steps a paint, clear or erase
/// touched, `prev` (the notes as they were) for a ramp or a nudge, which only
/// change notes in place.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StepDrag {
    pub gesture: StepGesture,
    /// The row the gesture works on (every gesture but Erase stays on it).
    pub pitch: u8,
    /// The step that was pressed.
    pub start_index: usize,
    /// Pointer y at the press, grid space — the ramp's zero.
    pub start_y: f32,
    /// Clip beat at the press — the nudge's zero.
    pub start_beat: f32,
    /// Last step visited on the row, so a fast drag fills every step it
    /// crossed rather than only the ones a mouse event landed in.
    pub last_index: usize,
    /// Notes taken out by this gesture, as they were before it.
    pub removed: Vec<MidiNoteState>,
    /// Notes put in by this gesture, as they are now.
    pub added: Vec<MidiNoteState>,
    /// Ramp and nudge: every note the gesture may change, as it was.
    pub prev: Vec<MidiNoteState>,
    /// The clip the gesture edits.
    pub clip_id: String,
}

impl StepDrag {
    pub(crate) fn new(
        gesture: StepGesture,
        pitch: u8,
        start_index: usize,
        start_y: f32,
        start_beat: f32,
    ) -> Self {
        Self {
            gesture,
            pitch,
            start_index,
            start_y,
            start_beat,
            last_index: start_index,
            removed: Vec::new(),
            added: Vec::new(),
            prev: Vec::new(),
            clip_id: String::new(),
        }
    }

    /// Steps from the last one visited to `index`, inclusive, in drag order.
    pub(crate) fn crossed(&self, index: usize) -> Vec<usize> {
        if index >= self.last_index {
            (self.last_index..=index).collect()
        } else {
            (index..=self.last_index).rev().collect()
        }
    }

    /// Records a step's notes as taken out by this gesture.
    pub(crate) fn take_out(&mut self, notes: Vec<MidiNoteState>) {
        for note in notes {
            // A note this gesture added is simply dropped again.
            if let Some(at) = self.added.iter().position(|n| n.id == note.id) {
                self.added.remove(at);
            } else if !self.removed.iter().any(|n| n.id == note.id) {
                self.removed.push(note);
            }
        }
    }
}

/// The empty steps among `indices` on lane `pitch`, filled: the notes to add.
pub(crate) fn paint_steps(
    notes: &[MidiNoteState],
    pitch: u8,
    indices: &[usize],
    steps: usize,
    step: f32,
    repeat: u8,
    curve: RepeatCurve,
    channel: MidiChannel,
) -> Vec<MidiNoteState> {
    let filled: Vec<usize> = lane_cells(notes, pitch, step)
        .iter()
        .map(|cell| cell.index)
        .collect();
    let mut seen = Vec::new();
    indices
        .iter()
        .copied()
        .filter(|index| *index < steps && !filled.contains(index))
        .filter(|index| {
            let first = !seen.contains(index);
            seen.push(*index);
            first
        })
        .flat_map(|index| step_notes(pitch, index, step, repeat, STEP_VELOCITY, curve, channel))
        .collect()
}

/// The notes of the filled steps among `indices` on lane `pitch`: the notes
/// to take out.
pub(crate) fn clear_steps(
    notes: &[MidiNoteState],
    pitch: u8,
    indices: &[usize],
    step: f32,
) -> Vec<MidiNoteState> {
    let ids: Vec<u64> = lane_cells(notes, pitch, step)
        .into_iter()
        .filter(|cell| indices.contains(&cell.index))
        .flat_map(|cell| cell.ids)
        .collect();
    notes_with(notes, &ids)
}

/// The ramp's end velocity for a pointer at `y`, from `from_velocity` at
/// `start_y`: up is louder.
pub(crate) fn ramp_target(from_velocity: u8, start_y: f32, y: f32) -> u8 {
    (f32::from(from_velocity) + (start_y - y) * RAMP_VELOCITY_PER_PX)
        .round()
        .clamp(1.0, 127.0) as u8
}

/// `row` (the lane's notes as they were before the ramp) with the steps from
/// `from` to `to` ramped linearly from `from_velocity` to `to_velocity`; every
/// other note as it was. All hits of a step take the step's value.
pub(crate) fn ramp_velocities(
    row: &[MidiNoteState],
    pitch: u8,
    step: f32,
    from: usize,
    to: usize,
    from_velocity: u8,
    to_velocity: u8,
) -> Vec<MidiNoteState> {
    let (lo, hi) = (from.min(to), from.max(to));
    let span = (hi - lo) as f32;
    let mut out = row.to_vec();
    for cell in lane_cells(row, pitch, step) {
        if cell.index < lo || cell.index > hi {
            continue;
        }
        // Distance from the pressed step, whichever way the drag went.
        let t = if span == 0.0 {
            1.0
        } else {
            (cell.index as f32 - from as f32).abs() / span
        };
        let velocity = (f32::from(from_velocity)
            + (f32::from(to_velocity) - f32::from(from_velocity)) * t)
            .round()
            .clamp(1.0, 127.0) as u8;
        for note in out.iter_mut().filter(|n| cell.ids.contains(&n.id)) {
            note.velocity = velocity;
        }
    }
    out
}

/// A step's notes (`cell_notes`, as they were) slid so the step sits `offset`
/// beats from its line, clamped to ±[`NUDGE_LIMIT`] of a step. Its hits keep
/// their spacing.
pub(crate) fn nudged(
    cell_notes: &[MidiNoteState],
    cell: &StepCell,
    step: f32,
    offset: f32,
) -> Vec<MidiNoteState> {
    let limit = step * NUDGE_LIMIT;
    let offset = offset.clamp(-limit, limit);
    let shift = offset - cell.offset;
    cell_notes
        .iter()
        .map(|note| {
            let mut moved = note.clone();
            moved.start = (note.start + shift).max(0.0);
            moved
        })
        .collect()
}

/// "Fill Every N Steps": every `every`-th step of lane `pitch`, counted from
/// the clip start, filled where it is empty. Steps already played are kept as
/// they are.
pub(crate) fn fill_every(
    notes: &[MidiNoteState],
    pitch: u8,
    every: usize,
    steps: usize,
    step: f32,
    repeat: u8,
    curve: RepeatCurve,
    channel: MidiChannel,
) -> Vec<MidiNoteState> {
    let every = every.max(1);
    let indices: Vec<usize> = (0..steps).step_by(every).collect();
    paint_steps(notes, pitch, &indices, steps, step, repeat, curve, channel)
}

/// "Randomize Velocity": each note of `row` moved by up to
/// ±[`RANDOM_VELOCITY_SPREAD`] of its own velocity, kept in 1–127.
pub(crate) fn randomized_velocities(row: &[MidiNoteState], seed: u64) -> Vec<MidiNoteState> {
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    row.iter()
        .map(|note| {
            let spread = f32::from(note.velocity) * RANDOM_VELOCITY_SPREAD;
            let delta: f32 = rng.random_range(-spread..=spread);
            let mut out = note.clone();
            out.velocity = (f32::from(note.velocity) + delta).round().clamp(1.0, 127.0) as u8;
            out
        })
        .collect()
}

/// Every off-grid step of `notes` put back on its line. An even step (one hit
/// or a ratchet, nudged or played early or late) slides back whole; an uneven
/// one has each hit moved onto its nearest line, and a hit that would then
/// double one already played on that line is to be removed.
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
        // Lines this lane already plays on; a snapped hit landing on one
        // would double it.
        let mut taken: Vec<usize> = cells
            .iter()
            .filter(|cell| cell.on_grid())
            .map(|cell| cell.index)
            .collect();
        for cell in cells.iter().filter(|cell| !cell.on_grid()) {
            if cell.even {
                // A whole step back onto its own line: its index is its own.
                taken.push(cell.index);
                moved.extend(notes_with(notes, &cell.ids).into_iter().map(|mut note| {
                    note.start = (note.start - cell.offset).max(0.0);
                    note
                }));
                continue;
            }
            for note in notes_with(notes, &cell.ids) {
                let index = (note.start.max(0.0) / step).round() as usize;
                if taken.contains(&index) {
                    dropped.push(note);
                    continue;
                }
                taken.push(index);
                let line = index as f32 * step;
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

/// How many notes of `notes` sit off the step grid — nudged, played early or
/// late, or in an uneven roll.
pub(crate) fn off_grid_count(notes: &[MidiNoteState], step: f32) -> usize {
    let mut pitches: Vec<u8> = notes.iter().map(|n| n.pitch).collect();
    pitches.sort_unstable();
    pitches.dedup();
    pitches
        .into_iter()
        .flat_map(|pitch| lane_cells(notes, pitch, step))
        .filter(|cell| !cell.on_grid())
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

    fn velocity_note(pitch: u8, start: f32, velocity: u8) -> MidiNoteState {
        MidiNoteState::new(pitch, start, 0.25, velocity)
    }

    /// Applies an edit the way the editor does: take `removed` out, put
    /// `added` in.
    fn apply(
        notes: &[MidiNoteState],
        removed: &[MidiNoteState],
        added: &[MidiNoteState],
    ) -> Vec<MidiNoteState> {
        let mut out: Vec<MidiNoteState> = notes
            .iter()
            .filter(|n| !removed.iter().any(|r| r.id == n.id))
            .cloned()
            .collect();
        out.extend(added.iter().cloned());
        out
    }

    #[test]
    fn a_hit_on_a_line_is_one_step() {
        let notes = vec![note(36, 0.0, 0.25), note(36, 1.0, 0.1), note(38, 0.5, 0.25)];
        let kick = lane_cells(&notes, 36, SIXTEENTH);
        assert_eq!(kick.len(), 2);
        assert_eq!(
            (kick[0].index, kick[0].repeat, kick[0].on_grid()),
            (0, 1, true)
        );
        assert_eq!(
            (kick[1].index, kick[1].repeat, kick[1].on_grid()),
            (4, 1, true)
        );
        // A note's length does not move it off the grid.
        assert!(lane_cells(&notes, 38, SIXTEENTH)[0].on_grid());
    }

    /// Step → MIDI → Step: every repeat count comes back as itself, with each
    /// slice's velocity.
    #[test]
    fn every_repeat_reads_back_as_the_step_it_was_written_as() {
        for repeat in STEP_REPEATS {
            let notes = step_notes(
                36,
                5,
                SIXTEENTH,
                repeat,
                90,
                RepeatCurve::Flat,
                MidiChannel::default(),
            );
            assert_eq!(notes.len(), usize::from(repeat));
            let cells = lane_cells(&notes, 36, SIXTEENTH);
            assert_eq!(cells.len(), 1, "×{repeat}");
            let cell = &cells[0];
            assert_eq!(cell.index, 5);
            assert_eq!(cell.repeat, repeat);
            assert!(cell.on_grid(), "×{repeat}");
            assert_eq!(cell.velocities, vec![90; usize::from(repeat)]);
            let end = notes
                .iter()
                .map(|n| n.start + n.duration)
                .fold(0.0_f32, f32::max);
            assert!((end - 6.0 * SIXTEENTH).abs() < 1e-5);
        }
    }

    /// A hit played late stays the step it was meant for, with its offset, and
    /// is never moved by reading it.
    #[test]
    fn a_late_hit_is_its_step_with_an_offset() {
        let notes = vec![note(42, 0.30, 0.1)];
        let cells = lane_cells(&notes, 42, SIXTEENTH);
        assert_eq!(cells[0].index, 1);
        assert!(cells[0].even);
        assert!((cells[0].offset - 0.05).abs() < 1e-6);
        assert!(!cells[0].on_grid());
        assert_eq!(off_grid_count(&notes, SIXTEENTH), 1);
        assert_eq!(notes[0].start, 0.30, "reading never edits");
        // An early hit belongs to the line after it.
        let early = vec![note(42, 0.45, 0.1)];
        let cell = &lane_cells(&early, 42, SIXTEENTH)[0];
        assert_eq!(cell.index, 2);
        assert!((cell.offset + 0.05).abs() < 1e-6);
    }

    #[test]
    fn an_uneven_roll_is_one_uneven_step() {
        let notes = vec![note(38, 0.0, 0.05), note(38, 0.05, 0.05)];
        let cells = lane_cells(&notes, 38, SIXTEENTH);
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0].repeat, 2);
        assert!(!cells[0].even);
    }

    /// A ratchet whose later hits sit past the half-step is still one step.
    #[test]
    fn a_ratchet_never_splits_across_lines() {
        let notes = step_notes(
            38,
            15,
            SIXTEENTH,
            4,
            96,
            RepeatCurve::Flat,
            MidiChannel::default(),
        );
        let cells = lane_cells(&notes, 38, SIXTEENTH);
        assert_eq!(cells.len(), 1);
        assert_eq!((cells[0].index, cells[0].repeat), (15, 4));
    }

    /// Snapping keeps ids: an even step slides back whole, an uneven one has
    /// its hits put on lines and a doubled hit dropped.
    #[test]
    fn snapping_puts_steps_back_and_never_doubles_one() {
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
        let mut after = apply(&notes, &dropped, &[]);
        for note in after.iter_mut() {
            if let Some(m) = moved.iter().find(|m| m.id == note.id) {
                *note = m.clone();
            }
        }
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

    /// Painting fills only empty steps, so dragging over a hit never removes
    /// it, and a fast drag fills every step it crossed.
    #[test]
    fn painting_fills_the_empty_steps_it_crosses_and_keeps_the_rest() {
        let notes = vec![velocity_note(36, 0.5, 70)];
        let mut drag = StepDrag::new(StepGesture::Paint, 36, 0, 0.0, 0.0);
        let crossed = drag.crossed(4);
        assert_eq!(crossed, vec![0, 1, 2, 3, 4]);
        let added = paint_steps(
            &notes,
            36,
            &crossed,
            16,
            SIXTEENTH,
            1,
            RepeatCurve::Flat,
            MidiChannel::default(),
        );
        assert_eq!(added.len(), 4, "step 2 was already played");
        drag.last_index = 4;
        assert_eq!(drag.crossed(2), vec![4, 3, 2]);
        let after = apply(&notes, &[], &added);
        let cells = lane_cells(&after, 36, SIXTEENTH);
        assert_eq!(cells.len(), 5);
        assert_eq!(cells[2].velocity, 70, "the existing hit is untouched");
        // Past the clip's last step nothing is painted.
        assert!(paint_steps(
            &after,
            36,
            &[16, 17],
            16,
            SIXTEENTH,
            1,
            RepeatCurve::Flat,
            MidiChannel::default()
        )
        .is_empty());
    }

    #[test]
    fn clearing_takes_out_whole_steps_including_ratchets() {
        let mut notes = step_notes(
            38,
            3,
            SIXTEENTH,
            4,
            90,
            RepeatCurve::Flat,
            MidiChannel::default(),
        );
        notes.push(velocity_note(38, 0.0, 100));
        let removed = clear_steps(&notes, 38, &[2, 3], SIXTEENTH);
        assert_eq!(removed.len(), 4);
        let after = apply(&notes, &removed, &[]);
        assert_eq!(lane_cells(&after, 38, SIXTEENTH).len(), 1);
    }

    /// A gesture that adds and then takes the same note out leaves no trace in
    /// its undo entry.
    #[test]
    fn a_note_added_then_cleared_in_one_gesture_cancels_out() {
        let mut drag = StepDrag::new(StepGesture::Erase, 36, 0, 0.0, 0.0);
        let fresh = step_notes(
            36,
            0,
            SIXTEENTH,
            1,
            100,
            RepeatCurve::Flat,
            MidiChannel::default(),
        );
        drag.added.extend(fresh.clone());
        let old = velocity_note(36, 1.0, 80);
        drag.take_out(fresh);
        drag.take_out(vec![old.clone()]);
        assert!(drag.added.is_empty());
        assert_eq!(drag.removed, vec![old]);
    }

    /// The ramp runs from the pressed step's velocity to the target, linearly,
    /// over the filled steps between; steps outside it keep their velocity.
    #[test]
    fn a_velocity_ramp_interpolates_across_the_row() {
        let row: Vec<MidiNoteState> = (0..8)
            .map(|i| velocity_note(42, i as f32 * SIXTEENTH, 40))
            .collect();
        let ramped = ramp_velocities(&row, 42, SIXTEENTH, 0, 4, 40, 120);
        let velocities: Vec<u8> = ramped.iter().map(|n| n.velocity).collect();
        assert_eq!(velocities, vec![40, 60, 80, 100, 120, 40, 40, 40]);
        // Ids are kept, so the edit overwrites in place.
        assert!(ramped.iter().zip(&row).all(|(a, b)| a.id == b.id));
        // Dragged leftwards it ramps away from the pressed step the other way.
        let back = ramp_velocities(&row, 42, SIXTEENTH, 4, 0, 40, 120);
        assert_eq!(back[4].velocity, 40);
        assert_eq!(back[0].velocity, 120);
        assert_eq!(ramp_target(64, 100.0, 100.0 - 150.0), 127);
        assert_eq!(ramp_target(64, 100.0, 400.0), 1);
    }

    /// A nudge slides the whole step and stops just short of half a step, so
    /// the step stays the one it was.
    #[test]
    fn a_nudge_slides_a_step_and_stays_within_half_a_step() {
        let notes = step_notes(
            36,
            4,
            SIXTEENTH,
            2,
            100,
            RepeatCurve::Flat,
            MidiChannel::default(),
        );
        let cell = lane_cells(&notes, 36, SIXTEENTH).remove(0);
        let late = nudged(&notes, &cell, SIXTEENTH, 0.05);
        assert!((late[0].start - 1.05).abs() < 1e-6);
        assert!((late[1].start - 1.175).abs() < 1e-6, "spacing kept");
        let far = nudged(&notes, &cell, SIXTEENTH, 10.0);
        let read = lane_cells(&far, 36, SIXTEENTH);
        assert_eq!(read[0].index, 4, "still step 4");
        assert!((read[0].offset - SIXTEENTH * NUDGE_LIMIT).abs() < 1e-5);
        assert!(read[0].even, "a nudged ratchet is still even");
    }

    #[test]
    fn fill_every_n_fills_only_empty_lines() {
        let notes = vec![velocity_note(42, 0.0, 50)];
        let added = fill_every(
            &notes,
            42,
            2,
            16,
            SIXTEENTH,
            1,
            RepeatCurve::Flat,
            MidiChannel::default(),
        );
        assert_eq!(added.len(), 7, "eight lines, one already played");
        let after = apply(&notes, &[], &added);
        let indices: Vec<usize> = lane_cells(&after, 42, SIXTEENTH)
            .iter()
            .map(|c| c.index)
            .collect();
        assert_eq!(indices, vec![0, 2, 4, 6, 8, 10, 12, 14]);
        assert_eq!(
            fill_every(
                &after,
                42,
                4,
                16,
                SIXTEENTH,
                1,
                RepeatCurve::Flat,
                MidiChannel::default()
            )
            .len(),
            0
        );
    }

    #[test]
    fn randomized_velocity_stays_within_fifteen_percent() {
        let row: Vec<MidiNoteState> = (0..64)
            .map(|i| velocity_note(38, i as f32 * SIXTEENTH, 100))
            .collect();
        let out = randomized_velocities(&row, 7);
        assert!(out.iter().all(|n| (85..=115).contains(&n.velocity)));
        assert!(out.iter().any(|n| n.velocity != 100), "something moved");
        assert!(out
            .iter()
            .zip(&row)
            .all(|(a, b)| a.id == b.id && a.start == b.start));
        let loud = vec![velocity_note(38, 0.0, 127)];
        assert!(randomized_velocities(&loud, 3)[0].velocity <= 127);
    }

    #[test]
    fn repeat_curves_shape_the_hits_from_the_step_velocity() {
        assert_eq!(RepeatCurve::Flat.velocities(100, 4), vec![100; 4]);
        assert_eq!(
            RepeatCurve::RampUp.velocities(100, 4),
            vec![35, 57, 78, 100]
        );
        assert_eq!(RepeatCurve::Decay.velocities(100, 4), vec![100, 75, 50, 25]);
        assert_eq!(
            RepeatCurve::Accent.velocities(100, 6),
            vec![100, 55, 80, 45, 100, 55]
        );
        // One hit is the step's velocity whatever the curve.
        for curve in RepeatCurve::ALL {
            assert_eq!(curve.velocities(90, 1), vec![90]);
        }
        // Never below 1, even from a quiet step.
        assert!(RepeatCurve::Decay.velocities(2, 8).iter().all(|v| *v >= 1));
    }

    /// Writing a curve and reading it back finds the curve and the step's
    /// velocity, so rewriting the count keeps the shape and the level.
    #[test]
    fn a_written_curve_reads_back_as_itself() {
        for curve in RepeatCurve::ALL {
            for repeat in [2u8, 3, 4, 6, 8] {
                let notes = step_notes(38, 2, SIXTEENTH, repeat, 96, curve, MidiChannel::default());
                let cell = &lane_cells(&notes, 38, SIXTEENTH)[0];
                assert_eq!(cell.velocity, 96, "{curve:?} ×{repeat}");
                assert_eq!(
                    RepeatCurve::detect(&cell.velocities),
                    Some(curve),
                    "×{repeat}"
                );
            }
        }
        assert_eq!(RepeatCurve::detect(&[100, 20, 90]), None);
    }

    #[test]
    fn the_repeat_count_steps_through_the_list_and_stops_at_its_ends() {
        assert_eq!(repeat_after(1, 1), 2);
        assert_eq!(repeat_after(4, 1), 6);
        assert_eq!(repeat_after(4, -2), 2);
        assert_eq!(repeat_after(8, 3), 8);
        assert_eq!(repeat_after(2, -5), 1);
        // An uneven roll of five counts from ×6.
        assert_eq!(repeat_after(5, 0), 6);
    }

    /// Rewriting keeps a step where it plays (nudge), muted, and as loud;
    /// the count and curve change and nothing else.
    #[test]
    fn rewriting_a_step_keeps_its_nudge_mute_and_level() {
        let mut notes = step_notes(
            36,
            4,
            SIXTEENTH,
            2,
            110,
            RepeatCurve::Flat,
            MidiChannel::default(),
        );
        for note in &mut notes {
            note.start += 0.03;
            note.muted = true;
        }
        let ids: Vec<u64> = notes.iter().map(|n| n.id).collect();
        let cells = cells_holding(&notes, &ids[..1], SIXTEENTH);
        assert_eq!(cells.len(), 1);
        let (removed, added) =
            rewrite_steps(&notes, &cells, SIXTEENTH, Some(4), Some(RepeatCurve::Decay));
        assert_eq!(removed.len(), 2);
        assert_eq!(added.len(), 4);
        let after = apply(&notes, &removed, &added);
        let cell = &lane_cells(&after, 36, SIXTEENTH)[0];
        assert_eq!((cell.index, cell.repeat, cell.velocity), (4, 4, 110));
        assert!((cell.offset - 0.03).abs() < 1e-5);
        assert!(cell.even && cell.muted);
        assert_eq!(
            RepeatCurve::detect(&cell.velocities),
            Some(RepeatCurve::Decay)
        );

        // Only the count given: the curve the hits already follow is kept.
        let cells = cells_holding(&after, &[added[0].id], SIXTEENTH);
        let (_, again) = rewrite_steps(&after, &cells, SIXTEENTH, Some(8), None);
        assert_eq!(
            RepeatCurve::detect(&again.iter().map(|n| n.velocity).collect::<Vec<_>>()),
            Some(RepeatCurve::Decay)
        );
        // Asking for what is already there changes nothing.
        let (removed, added) =
            rewrite_steps(&after, &cells, SIXTEENTH, Some(4), Some(RepeatCurve::Decay));
        assert!(removed.is_empty() && added.is_empty());
    }
}

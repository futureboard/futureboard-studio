//! Note transforms behind the MIDI editor's tools panel: quantize, length,
//! time stretch, transpose, chord voicing and strum.
//!
//! Every function here is a pure, in-place rewrite of the start, duration or
//! pitch of the notes whose ids are in `ids`. None of them adds or removes a
//! note, so the editor records each one as a single `EditMidiNotes` command
//! from before/after snapshots of exactly those ids — undo restores the
//! fields and the note set never changes. Ids and every other note field
//! (velocity, channel, articulation, expression) are left alone.
//!
//! Positions are clip-local beats, as on [`MidiNoteState`]. Each function
//! returns the number of notes it actually changed, so a caller can tell a
//! no-op from an edit.

use std::collections::HashSet;

use super::musical_snap::{MeterBars, MeterGrid};
use super::{MidiNoteState, MidiScale, ScaleKind, MIN_NOTE_BEATS};

impl super::TimelineState {
    /// Set a clip's length outright. Note edits only ever grow a clip to fit
    /// (see `expand_clip_to_contain_notes`); this is the other direction, for
    /// the undo of a tool that grew it and for a cancelled tool preview.
    /// Returns whether the length changed.
    pub fn set_clip_length_beats(&mut self, clip_id: &str, beats: f32) -> bool {
        for track in &mut self.tracks {
            for clip in &mut track.clips {
                if clip.id == clip_id {
                    let beats = beats.max(MIN_NOTE_BEATS);
                    if (clip.duration_beats - beats).abs() > EPS {
                        clip.duration_beats = beats;
                        return true;
                    }
                    return false;
                }
            }
        }
        false
    }
}

/// Notes whose starts sit within this many beats of the first note of a group
/// are one chord: a 64th at 4/4, loose enough for a played chord and tight
/// enough that consecutive sixteenths stay separate.
pub const CHORD_TOLERANCE_BEATS: f32 = 0.0625;

const EPS: f32 = 1.0e-4;

/// Which edge of a note quantize moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuantizeTarget {
    /// Move the note onto the grid, keeping its length.
    Start,
    /// Move the note's end onto the grid, keeping its start.
    End,
    /// Round the length to a whole number of grid steps, keeping the start.
    Length,
}

impl QuantizeTarget {
    pub fn label(self) -> &'static str {
        match self {
            Self::Start => "Starts",
            Self::End => "Ends",
            Self::Length => "Lengths",
        }
    }
}

/// A quantize pass: grid, how far toward it, and the off-beat delay.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QuantizeSpec {
    /// Grid step in beats.
    pub step: f32,
    /// `0..=1`: share of the distance to the grid each note travels. `1` is a
    /// hard quantize; less leaves some of the played feel.
    pub strength: f32,
    /// `0..=1`: delay of every second grid line. `1` puts the off-beat at the
    /// triplet position (2:1), `0` is straight.
    pub swing: f32,
}

impl QuantizeSpec {
    /// The `i`th grid line, with swing applied to the odd ones.
    fn line(&self, i: i64) -> f32 {
        let base = i as f32 * self.step;
        if i.rem_euclid(2) == 1 {
            base + self.swing.clamp(0.0, 1.0) * self.step / 3.0
        } else {
            base
        }
    }

    /// The grid line nearest `beat` (never before zero).
    fn nearest(&self, beat: f32) -> f32 {
        let i0 = (beat / self.step).floor() as i64;
        (i0 - 1..=i0 + 2)
            .filter(|i| *i >= 0)
            .map(|i| self.line(i))
            .min_by(|a, b| (a - beat).abs().total_cmp(&(b - beat).abs()))
            .unwrap_or(0.0)
    }

    /// The first grid line strictly after `beat`.
    fn after(&self, beat: f32) -> f32 {
        let i0 = (beat / self.step).floor() as i64;
        (i0..=i0 + 2)
            .map(|i| self.line(i))
            .find(|line| *line > beat + EPS)
            .unwrap_or(beat + self.step)
    }

    fn pull(&self, from: f32, to: f32) -> f32 {
        from + (to - from) * self.strength.clamp(0.0, 1.0)
    }
}

fn selected<'a>(
    notes: &'a mut [MidiNoteState],
    ids: &'a HashSet<u64>,
) -> impl Iterator<Item = &'a mut MidiNoteState> + 'a {
    notes.iter_mut().filter(move |n| ids.contains(&n.id))
}

fn changed_f32(a: f32, b: f32) -> bool {
    (a - b).abs() > EPS
}

/// Quantize starts, ends or lengths toward `spec`'s grid.
pub fn quantize_notes(
    notes: &mut [MidiNoteState],
    ids: &HashSet<u64>,
    spec: QuantizeSpec,
    target: QuantizeTarget,
) -> usize {
    if spec.step <= 0.0 || spec.strength <= 0.0 {
        return 0;
    }
    let mut changed = 0;
    for note in selected(notes, ids) {
        let (start, duration) = match target {
            QuantizeTarget::Start => {
                let start = spec.pull(note.start, spec.nearest(note.start)).max(0.0);
                (start, note.duration)
            }
            QuantizeTarget::End => {
                let end = note.start + note.duration;
                let mut grid_end = spec.nearest(end);
                if grid_end <= note.start + EPS {
                    grid_end = spec.after(note.start);
                }
                let end = spec.pull(end, grid_end);
                (note.start, (end - note.start).max(MIN_NOTE_BEATS))
            }
            QuantizeTarget::Length => {
                let steps = (note.duration / spec.step).round().max(1.0);
                let duration = spec.pull(note.duration, steps * spec.step);
                (note.start, duration.max(MIN_NOTE_BEATS))
            }
        };
        if changed_f32(start, note.start) || changed_f32(duration, note.duration) {
            note.start = start;
            note.duration = duration;
            changed += 1;
        }
    }
    changed
}

/// Lines a [`quantize_notes_on_grid`] length walk visits at most, so a
/// degenerate grid cannot stall the UI thread.
const MAX_QUANTIZE_LINES: usize = 1 << 16;

/// The grid line nearest clip beat `beat` that is not before the clip start.
/// A clip that starts off a bar line has the bar line before it outside the
/// clip, so a note near its start goes to the first line inside it.
fn grid_nearest_in_clip<M: MeterBars + ?Sized>(grid: MeterGrid<'_, M>, beat: f32) -> f32 {
    let at = f64::from(beat);
    let (below, above) = grid.cell(at);
    let line = if below < -f64::from(EPS) {
        above
    } else {
        grid.snap(at)
    };
    (line as f32).max(0.0)
}

/// The first grid line strictly after clip beat `beat`.
fn grid_after<M: MeterBars + ?Sized>(grid: MeterGrid<'_, M>, beat: f32) -> f32 {
    let at = f64::from(beat);
    let mut line = grid.next_line(grid.line_at_or_before(at));
    for _ in 0..4 {
        if line > at + f64::from(EPS) {
            break;
        }
        line = grid.next_line(line);
    }
    line as f32
}

/// The length nearest `duration` that is a whole number of grid cells (at
/// least one) from `start`, keeping `start`'s place in its cell: the rule
/// Duplicate measures by ([`MeterGrid::duplicate_offset`]), so across a meter
/// change "one bar" is the bar the meter makes it there.
fn grid_length<M: MeterBars + ?Sized>(grid: MeterGrid<'_, M>, start: f32, duration: f32) -> f32 {
    let start = f64::from(start);
    let duration = f64::from(duration);
    let anchor = grid.line_at_or_before(start);
    let into_cell = start - anchor;
    let mut line = grid.next_line(anchor);
    let mut shorter = line + into_cell - start;
    for _ in 0..MAX_QUANTIZE_LINES {
        if duration <= shorter {
            break;
        }
        line = grid.next_line(line);
        let longer = line + into_cell - start;
        if duration <= longer {
            // A tie takes the longer length, as `round` does on one grid.
            return if duration - shorter < longer - duration {
                shorter as f32
            } else {
                longer as f32
            };
        }
        shorter = longer;
    }
    shorter as f32
}

/// [`quantize_notes`] against the editor's meter-aware note grid instead of
/// a fixed step counted from clip beat 0: what "1 Bar" quantizes against.
///
/// Positions are clip-local; `grid` carries the clip's song origin, so the
/// lines are the bar lines drawn over the clip: a clip that starts off a bar
/// line quantizes to the bar lines inside it, and a meter change inside the
/// clip changes the bar length from the change on. Only `spec.strength` is
/// used: the grid supplies the lines, and swing (a delay of every second
/// line) is a subdivision feel with no meaning for whole bars.
pub fn quantize_notes_on_grid<M: MeterBars + ?Sized>(
    notes: &mut [MidiNoteState],
    ids: &HashSet<u64>,
    grid: MeterGrid<'_, M>,
    spec: QuantizeSpec,
    target: QuantizeTarget,
) -> usize {
    if spec.strength <= 0.0 {
        return 0;
    }
    let mut changed = 0;
    for note in selected(notes, ids) {
        let (start, duration) = match target {
            QuantizeTarget::Start => {
                let start = spec
                    .pull(note.start, grid_nearest_in_clip(grid, note.start))
                    .max(0.0);
                (start, note.duration)
            }
            QuantizeTarget::End => {
                let end = note.start + note.duration;
                let mut grid_end = grid_nearest_in_clip(grid, end);
                if grid_end <= note.start + EPS {
                    grid_end = grid_after(grid, note.start);
                }
                let end = spec.pull(end, grid_end);
                (note.start, (end - note.start).max(MIN_NOTE_BEATS))
            }
            QuantizeTarget::Length => {
                let length = grid_length(grid, note.start, note.duration);
                let duration = spec.pull(note.duration, length);
                (note.start, duration.max(MIN_NOTE_BEATS))
            }
        };
        if changed_f32(start, note.start) || changed_f32(duration, note.duration) {
            note.start = start;
            note.duration = duration;
            changed += 1;
        }
    }
    changed
}

/// Give every note the same length.
pub fn set_note_lengths(notes: &mut [MidiNoteState], ids: &HashSet<u64>, length: f32) -> usize {
    let length = length.max(MIN_NOTE_BEATS);
    let mut changed = 0;
    for note in selected(notes, ids) {
        if changed_f32(note.duration, length) {
            note.duration = length;
            changed += 1;
        }
    }
    changed
}

/// Multiply every note's length by `factor`.
pub fn scale_note_lengths(notes: &mut [MidiNoteState], ids: &HashSet<u64>, factor: f32) -> usize {
    if factor <= 0.0 {
        return 0;
    }
    let mut changed = 0;
    for note in selected(notes, ids) {
        let duration = (note.duration * factor).max(MIN_NOTE_BEATS);
        if changed_f32(note.duration, duration) {
            note.duration = duration;
            changed += 1;
        }
    }
    changed
}

/// Stretch each note to the start of the next note (or chord) after it,
/// plus `overlap` beats (negative leaves a gap). The last note is left as is.
pub fn legato_notes(notes: &mut [MidiNoteState], ids: &HashSet<u64>, overlap: f32) -> usize {
    legato_notes_by(notes, ids, |_| overlap)
}

/// [`legato_notes`] with the overlap given per note start, so a caller can
/// express it in milliseconds at the tempo where each note sits.
pub fn legato_notes_by(
    notes: &mut [MidiNoteState],
    ids: &HashSet<u64>,
    overlap: impl Fn(f32) -> f32,
) -> usize {
    let mut starts: Vec<f32> = notes
        .iter()
        .filter(|n| ids.contains(&n.id))
        .map(|n| n.start)
        .collect();
    starts.sort_by(f32::total_cmp);
    let mut changed = 0;
    for note in selected(notes, ids) {
        let threshold = note.start + CHORD_TOLERANCE_BEATS;
        let Some(next) = starts.iter().copied().find(|s| *s > threshold) else {
            continue;
        };
        let duration = (next - note.start + overlap(note.start)).max(MIN_NOTE_BEATS);
        if changed_f32(note.duration, duration) {
            note.duration = duration;
            changed += 1;
        }
    }
    changed
}

/// Trim every note that runs past the start of the next one. `same_pitch`
/// only trims against the next note of the same pitch (mono per key);
/// otherwise against the next note of any pitch (one voice).
pub fn delete_overlaps(notes: &mut [MidiNoteState], ids: &HashSet<u64>, same_pitch: bool) -> usize {
    let mut order: Vec<(f32, u8)> = notes
        .iter()
        .filter(|n| ids.contains(&n.id))
        .map(|n| (n.start, n.pitch))
        .collect();
    order.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut changed = 0;
    for note in selected(notes, ids) {
        let tolerance = if same_pitch {
            EPS
        } else {
            CHORD_TOLERANCE_BEATS
        };
        let next = order
            .iter()
            .filter(|(start, pitch)| {
                *start > note.start + tolerance && (!same_pitch || *pitch == note.pitch)
            })
            .map(|(start, _)| *start)
            .next();
        let Some(next) = next else {
            continue;
        };
        let room = next - note.start;
        if note.start + note.duration > next + EPS && room >= MIN_NOTE_BEATS {
            note.duration = room;
            changed += 1;
        }
    }
    changed
}

/// Move each note by `degrees` steps of `scale` (a chromatic scale moves by
/// semitones). A note off the scale first snaps onto it, so a run of scale
/// steps always lands on scale tones. Notes that would leave 0..=127 stay.
pub fn transpose_in_scale(
    notes: &mut [MidiNoteState],
    ids: &HashSet<u64>,
    scale: MidiScale,
    degrees: i32,
) -> usize {
    if degrees == 0 {
        return 0;
    }
    let mut changed = 0;
    for note in selected(notes, ids) {
        let Some(pitch) = scale_step(scale, note.pitch, degrees) else {
            continue;
        };
        if pitch != note.pitch {
            note.pitch = pitch;
            changed += 1;
        }
    }
    changed
}

fn scale_step(scale: MidiScale, pitch: u8, degrees: i32) -> Option<u8> {
    if scale.kind == ScaleKind::Chromatic {
        let next = pitch as i32 + degrees;
        return (0..=127).contains(&next).then_some(next as u8);
    }
    let mut current = scale.nearest_pitch(pitch) as i32;
    let dir = degrees.signum();
    for _ in 0..degrees.abs() {
        loop {
            current += dir;
            if !(0..=127).contains(&current) {
                return None;
            }
            if scale.contains_pitch(current as u8) {
                break;
            }
        }
    }
    Some(current as u8)
}

/// Mirror the pitches around the middle of their own range: the highest
/// becomes the lowest and the contour turns upside down.
pub fn invert_pitches(notes: &mut [MidiNoteState], ids: &HashSet<u64>) -> usize {
    let pitches = notes
        .iter()
        .filter(|n| ids.contains(&n.id))
        .map(|n| n.pitch);
    let (Some(lo), Some(hi)) = (pitches.clone().min(), pitches.max()) else {
        return 0;
    };
    let axis = lo as i32 + hi as i32;
    let mut changed = 0;
    for note in selected(notes, ids) {
        let pitch = (axis - note.pitch as i32).clamp(0, 127) as u8;
        if pitch != note.pitch {
            note.pitch = pitch;
            changed += 1;
        }
    }
    changed
}

/// Play the passage backwards: each note's span is mirrored inside the span
/// the notes cover together, lengths kept.
pub fn retrograde_notes(notes: &mut [MidiNoteState], ids: &HashSet<u64>) -> usize {
    let mut span: Option<(f32, f32)> = None;
    for note in notes.iter().filter(|n| ids.contains(&n.id)) {
        let end = note.start + note.duration;
        span = Some(match span {
            None => (note.start, end),
            Some((s, e)) => (s.min(note.start), e.max(end)),
        });
    }
    let Some((first, last)) = span else {
        return 0;
    };
    let mut changed = 0;
    for note in selected(notes, ids) {
        let start = (first + last - (note.start + note.duration)).max(0.0);
        if changed_f32(start, note.start) {
            note.start = start;
            changed += 1;
        }
    }
    changed
}

/// The beats the notes cover together: the earliest start to the latest end.
pub fn note_span(notes: &[MidiNoteState], ids: &HashSet<u64>) -> Option<(f32, f32)> {
    notes
        .iter()
        .filter(|n| ids.contains(&n.id))
        .fold(None, |span, note| {
            let end = note.start + note.duration;
            Some(match span {
                None => (note.start, end),
                Some((s, e)) => (f32::min(s, note.start), f32::max(e, end)),
            })
        })
}

/// Which edge of the notes' span stays where it is while they stretch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StretchAnchor {
    /// The first note keeps its start; the passage grows or shrinks to the right.
    Start,
    /// The last note keeps its end; the passage grows or shrinks to the left.
    End,
}

/// The tightest and loosest a stretch goes in one step: 1/64 to 64 times.
pub const MIN_STRETCH_FACTOR: f32 = 1.0 / 64.0;
pub const MAX_STRETCH_FACTOR: f32 = 64.0;

/// A time stretch of a passage: every position is scaled about one edge of
/// the passage's span, every length by the same factor. Shared by the
/// editor's live drag and the committed edit so both put a note in the same
/// place.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StretchSpec {
    /// The passage's span before the stretch, clip-local beats.
    pub first: f32,
    pub last: f32,
    /// New length over old; above 1 slows the passage down.
    pub factor: f32,
    pub anchor: StretchAnchor,
}

impl StretchSpec {
    fn pivot(&self) -> f32 {
        match self.anchor {
            StretchAnchor::Start => self.first,
            StretchAnchor::End => self.last,
        }
    }

    fn factor(&self) -> f32 {
        if self.factor.is_finite() {
            self.factor.clamp(MIN_STRETCH_FACTOR, MAX_STRETCH_FACTOR)
        } else {
            1.0
        }
    }

    /// Where a note at `start` lasting `duration` lands.
    pub fn map(&self, start: f32, duration: f32) -> (f32, f32) {
        let factor = self.factor();
        let pivot = self.pivot();
        (
            (pivot + (start - pivot) * factor).max(0.0),
            (duration * factor).max(MIN_NOTE_BEATS),
        )
    }

    /// The span after the stretch.
    pub fn stretched_span(&self) -> (f32, f32) {
        let factor = self.factor();
        let pivot = self.pivot();
        (
            (pivot + (self.first - pivot) * factor).max(0.0),
            pivot + (self.last - pivot) * factor,
        )
    }
}

/// Time-stretch the notes by `factor` about one edge of their span. Starts
/// and lengths scale together, and so does every note's own expression
/// (pitch curve and expression lanes are timed from the note's start), so a
/// bend lands at the same point of a longer note.
pub fn stretch_notes(
    notes: &mut [MidiNoteState],
    ids: &HashSet<u64>,
    factor: f32,
    anchor: StretchAnchor,
) -> usize {
    if !factor.is_finite() || factor <= 0.0 || (factor - 1.0).abs() < 1.0e-6 {
        return 0;
    }
    let Some((first, last)) = note_span(notes, ids) else {
        return 0;
    };
    let spec = StretchSpec {
        first,
        last,
        factor,
        anchor,
    };
    let factor = spec.factor();
    let mut changed = 0;
    for note in selected(notes, ids) {
        let (start, duration) = spec.map(note.start, note.duration);
        if changed_f32(start, note.start) || changed_f32(duration, note.duration) {
            changed += 1;
        }
        note.start = start;
        note.duration = duration;
        if let Some(curve) = note.pitch_curve.as_mut() {
            for point in &mut curve.points {
                point.beat *= factor;
            }
        }
        let expression = &mut note.expression;
        for curve in [
            &mut expression.pitch,
            &mut expression.pressure,
            &mut expression.timbre,
        ]
        .into_iter()
        .chain(expression.custom.iter_mut().map(|lane| &mut lane.curve))
        {
            for point in &mut curve.points {
                point.position *= factor;
            }
        }
    }
    changed
}

/// The selected notes grouped into chords: indices into `notes`, each group
/// sorted by pitch ascending, groups in time order.
fn chord_groups(notes: &[MidiNoteState], ids: &HashSet<u64>) -> Vec<Vec<usize>> {
    let mut order: Vec<usize> = (0..notes.len())
        .filter(|&i| ids.contains(&notes[i].id))
        .collect();
    order.sort_by(|&a, &b| notes[a].start.total_cmp(&notes[b].start));
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut anchor = f32::NEG_INFINITY;
    for i in order {
        if notes[i].start - anchor > CHORD_TOLERANCE_BEATS || groups.is_empty() {
            anchor = notes[i].start;
            groups.push(vec![i]);
        } else if let Some(group) = groups.last_mut() {
            group.push(i);
        }
    }
    for group in &mut groups {
        group.sort_by_key(|&i| (notes[i].pitch, notes[i].id));
    }
    groups
}

/// A chord voicing step applied to every chord in the selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChordVoicing {
    /// Lowest note up an octave.
    InvertUp,
    /// Highest note down an octave.
    InvertDown,
    /// Second-highest note down an octave.
    Drop2,
    /// Third-highest note down an octave.
    Drop3,
    /// Every note folded to within an octave above the lowest.
    Close,
}

impl ChordVoicing {
    pub fn label(self) -> &'static str {
        match self {
            Self::InvertUp => "Invert Chords Up",
            Self::InvertDown => "Invert Chords Down",
            Self::Drop2 => "Drop 2 Voicing",
            Self::Drop3 => "Drop 3 Voicing",
            Self::Close => "Close Voicing",
        }
    }
}

/// Revoice every chord (two or more notes starting together) in the
/// selection. Single notes are left alone.
pub fn voice_chords(
    notes: &mut [MidiNoteState],
    ids: &HashSet<u64>,
    voicing: ChordVoicing,
) -> usize {
    let groups = chord_groups(notes, ids);
    let mut changed = 0;
    let mut set = |notes: &mut [MidiNoteState], i: usize, pitch: i32| {
        if (0..=127).contains(&pitch) && notes[i].pitch as i32 != pitch {
            notes[i].pitch = pitch as u8;
            changed += 1;
        }
    };
    for group in groups.into_iter().filter(|g| g.len() >= 2) {
        let n = group.len();
        match voicing {
            ChordVoicing::InvertUp => {
                let i = group[0];
                let p = notes[i].pitch as i32 + 12;
                set(notes, i, p);
            }
            ChordVoicing::InvertDown => {
                let i = group[n - 1];
                let p = notes[i].pitch as i32 - 12;
                set(notes, i, p);
            }
            ChordVoicing::Drop2 if n >= 3 => {
                let i = group[n - 2];
                let p = notes[i].pitch as i32 - 12;
                set(notes, i, p);
            }
            ChordVoicing::Drop3 if n >= 4 => {
                let i = group[n - 3];
                let p = notes[i].pitch as i32 - 12;
                set(notes, i, p);
            }
            ChordVoicing::Close => {
                let low = notes[group[0]].pitch as i32;
                for &i in &group[1..] {
                    let p = low + (notes[i].pitch as i32 - low).rem_euclid(12);
                    set(notes, i, p);
                }
            }
            _ => {}
        }
    }
    changed
}

/// Move each note to the nearest pitch whose class is in the chord sounding
/// at its start. `chord_at` answers that for a clip-local beat; notes with no
/// chord over them are left alone. Ties resolve downward.
pub fn conform_to_chords(
    notes: &mut [MidiNoteState],
    ids: &HashSet<u64>,
    chord_at: impl Fn(f32) -> Option<Vec<u8>>,
) -> usize {
    let mut changed = 0;
    for note in selected(notes, ids) {
        let Some(classes) = chord_at(note.start) else {
            continue;
        };
        if classes.is_empty() {
            continue;
        }
        let fits = |p: i32| (0..=127).contains(&p) && classes.contains(&((p % 12) as u8));
        let base = note.pitch as i32;
        let target = (0..=6).find_map(|d| {
            if fits(base - d) {
                Some(base - d)
            } else if fits(base + d) {
                Some(base + d)
            } else {
                None
            }
        });
        if let Some(pitch) = target {
            if pitch != base {
                note.pitch = pitch as u8;
                changed += 1;
            }
        }
    }
    changed
}

/// Spread each chord's starts like a strum: `up` plays from the lowest note,
/// otherwise from the highest. `offset(chord_start, k)` is the delay in beats
/// of the `k`th note played (`k = 0` is not delayed), so the caller can turn
/// milliseconds into beats at the chord's tempo. Note ends stay where they
/// are, as a strummed chord is released together; a note never shrinks below
/// [`MIN_NOTE_BEATS`]. The chord's earliest start is the strum's first note.
pub fn strum_chords(
    notes: &mut [MidiNoteState],
    ids: &HashSet<u64>,
    up: bool,
    offset: impl Fn(f32, usize) -> f32,
) -> usize {
    strum_chords_basic(notes, ids, up, offset)
}

fn strum_chords_basic(
    notes: &mut [MidiNoteState],
    ids: &HashSet<u64>,
    up: bool,
    offset: impl Fn(f32, usize) -> f32,
) -> usize {
    let groups = chord_groups(notes, ids);
    let mut changed = 0;
    for mut group in groups.into_iter().filter(|g| g.len() >= 2) {
        if !up {
            group.reverse();
        }
        let anchor = group
            .iter()
            .map(|&i| notes[i].start)
            .fold(f32::INFINITY, f32::min);
        for (k, &i) in group.iter().enumerate() {
            let note = &mut notes[i];
            let end = note.start + note.duration;
            let start = (anchor + offset(anchor, k).max(0.0)).min(end - MIN_NOTE_BEATS);
            let start = start.max(0.0);
            if changed_f32(start, note.start) {
                note.start = start;
                note.duration = (end - start).max(MIN_NOTE_BEATS);
                changed += 1;
            }
        }
    }
    changed
}

/// A strum pass for the Strum tool window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StrumSpec {
    /// `true` plays from the lowest note up, `false` from the highest down.
    pub up: bool,
    /// Flip the direction chord by chord, like a strumming hand. The first
    /// chord goes the way `up` says.
    pub alternate: bool,
    /// Delay between neighbouring notes with no tension, milliseconds. The
    /// whole stroke lasts `spread_ms * (notes - 1)` whatever the tension.
    pub spread_ms: f32,
    /// `-1..=1`. Above zero the gaps shrink through the chord (the stroke
    /// speeds up), below zero they grow (it slows down), zero is even.
    pub tension: f32,
    /// `true` keeps each note's end where it is (a strummed chord released
    /// together); `false` moves the whole note, keeping its length.
    pub keep_ends: bool,
    /// Velocity added to the last note of the stroke, scaled down to nothing
    /// at the first. Negative makes the stroke fade.
    pub velocity_fade: i32,
}

impl StrumSpec {
    /// Share of the whole stroke reached at `t` in `0..=1`: a power curve
    /// whose exponent the tension sets, so the ends stay fixed and only the
    /// spacing in between bends.
    fn shape(&self, t: f32) -> f32 {
        let exponent = 2f32.powf(-1.5 * self.tension.clamp(-1.0, 1.0));
        t.clamp(0.0, 1.0).powf(exponent)
    }
}

/// Strum every chord in the selection. `beats_after(anchor, ms)` converts a
/// delay at a clip-local beat into beats, so the stroke keeps its length in
/// time across tempo changes. Single notes are left alone.
pub fn strum_chords_with(
    notes: &mut [MidiNoteState],
    ids: &HashSet<u64>,
    spec: StrumSpec,
    beats_after: impl Fn(f32, f32) -> f32,
) -> usize {
    let groups = chord_groups(notes, ids);
    let mut changed = 0;
    let mut stroke = 0usize;
    for mut group in groups.into_iter().filter(|g| g.len() >= 2) {
        let up = if spec.alternate && stroke % 2 == 1 {
            !spec.up
        } else {
            spec.up
        };
        stroke += 1;
        if !up {
            group.reverse();
        }
        let anchor = group
            .iter()
            .map(|&i| notes[i].start)
            .fold(f32::INFINITY, f32::min);
        let last = (group.len() - 1) as f32;
        let total_ms = spec.spread_ms.max(0.0) * last;
        for (k, &i) in group.iter().enumerate() {
            let t = k as f32 / last;
            let delay = beats_after(anchor, total_ms * spec.shape(t)).max(0.0);
            let note = &mut notes[i];
            let before = (note.start, note.duration, note.velocity);
            let end = note.start + note.duration;
            if spec.keep_ends {
                let start = (anchor + delay).min(end - MIN_NOTE_BEATS).max(0.0);
                note.start = start;
                note.duration = (end - start).max(MIN_NOTE_BEATS);
            } else {
                note.start = (anchor + delay).max(0.0);
            }
            if spec.velocity_fade != 0 {
                let add = (spec.velocity_fade as f32 * t).round() as i32;
                note.velocity = (note.velocity as i32 + add).clamp(1, 127) as u8;
            }
            if changed_f32(before.0, note.start)
                || changed_f32(before.1, note.duration)
                || before.2 != note.velocity
            {
                changed += 1;
            }
        }
    }
    changed
}

/// A humanize pass: random, bounded nudges to timing, velocity and length.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HumanizeSpec {
    /// Largest start shift either way, milliseconds.
    pub timing_ms: f32,
    /// Largest velocity change either way.
    pub velocity: u8,
    /// Largest length change either way, percent of the note's length.
    pub length_pct: f32,
    /// Shift the notes of a chord by the same amount, so a chord stays a
    /// chord and only its place in time loosens.
    pub keep_chords: bool,
}

/// Small deterministic generator (xorshift64*): the same seed gives the same
/// humanize, which is what makes it testable. Not for anything that needs
/// real randomness.
struct Jitter(u64);

impl Jitter {
    fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    /// Uniform in `-1.0..=1.0`.
    fn next(&mut self) -> f32 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        let bits = x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 40;
        (bits as f32 / ((1u64 << 24) - 1) as f32) * 2.0 - 1.0
    }
}

/// Humanize the selection. `beats_after(start, ms)` converts a shift at a
/// clip-local beat into beats (negative `ms` gives a negative shift). Every
/// value stays in range: starts at or after zero, velocity in 1..=127, length
/// at least [`MIN_NOTE_BEATS`].
pub fn humanize_notes(
    notes: &mut [MidiNoteState],
    ids: &HashSet<u64>,
    spec: HumanizeSpec,
    seed: u64,
    beats_after: impl Fn(f32, f32) -> f32,
) -> usize {
    let mut rng = Jitter::new(seed);
    // One timing draw per chord when chords are kept, else one per note.
    let groups = if spec.keep_chords {
        chord_groups(notes, ids)
    } else {
        let mut order: Vec<usize> = (0..notes.len())
            .filter(|&i| ids.contains(&notes[i].id))
            .collect();
        order.sort_by(|&a, &b| notes[a].start.total_cmp(&notes[b].start));
        order.into_iter().map(|i| vec![i]).collect()
    };
    let mut changed = 0;
    for group in groups {
        let shift_ms = rng.next() * spec.timing_ms.max(0.0);
        for i in group {
            let note = &mut notes[i];
            let before = (note.start, note.duration, note.velocity);
            if spec.timing_ms > 0.0 {
                note.start = (note.start + beats_after(note.start, shift_ms)).max(0.0);
            }
            if spec.velocity > 0 {
                let delta = (rng.next() * spec.velocity as f32).round() as i32;
                note.velocity = (note.velocity as i32 + delta).clamp(1, 127) as u8;
            }
            if spec.length_pct > 0.0 {
                let factor = 1.0 + rng.next() * spec.length_pct / 100.0;
                note.duration = (note.duration * factor).max(MIN_NOTE_BEATS);
            }
            if changed_f32(before.0, note.start)
                || changed_f32(before.1, note.duration)
                || before.2 != note.velocity
            {
                changed += 1;
            }
        }
    }
    changed
}

/// A length pass for the Length tool window: either scale every length or
/// tie each note to the next, then loosen the result at random.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LengthSpec {
    /// Multiply every length (`1.0` leaves them). Ignored with `legato`.
    pub multiply: f32,
    /// Stretch each note to the next note or chord instead of scaling.
    pub legato: bool,
    /// With `legato`: milliseconds past the next note's start (overlap), or
    /// before it when negative (a gap).
    pub gap_ms: f32,
    /// Random change to each resulting length, either way, percent.
    pub variation_pct: f32,
}

/// Apply a [`LengthSpec`]. `beats_after(start, ms)` converts the legato gap
/// at a note's start into beats. Lengths never drop below
/// [`MIN_NOTE_BEATS`].
pub fn articulate_notes(
    notes: &mut [MidiNoteState],
    ids: &HashSet<u64>,
    spec: LengthSpec,
    seed: u64,
    beats_after: impl Fn(f32, f32) -> f32,
) -> usize {
    let before: Vec<(u64, f32)> = notes
        .iter()
        .filter(|n| ids.contains(&n.id))
        .map(|n| (n.id, n.duration))
        .collect();
    if spec.legato {
        legato_notes_by(notes, ids, |start| beats_after(start, spec.gap_ms));
    } else if changed_f32(spec.multiply, 1.0) {
        scale_note_lengths(notes, ids, spec.multiply.max(0.01));
    }
    if spec.variation_pct > 0.0 {
        let mut rng = Jitter::new(seed);
        let mut order: Vec<usize> = (0..notes.len())
            .filter(|&i| ids.contains(&notes[i].id))
            .collect();
        order.sort_by(|&a, &b| notes[a].start.total_cmp(&notes[b].start));
        for i in order {
            let factor = 1.0 + rng.next() * spec.variation_pct / 100.0;
            notes[i].duration = (notes[i].duration * factor).max(MIN_NOTE_BEATS);
        }
    }
    before
        .iter()
        .filter(|(id, duration)| {
            notes
                .iter()
                .find(|n| n.id == *id)
                .is_some_and(|n| changed_f32(n.duration, *duration))
        })
        .count()
}

/// A velocity pass for the Velocity tool window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VelocityShape {
    /// Added to every velocity.
    pub offset: i32,
    /// `-100..=100`: percent toward the selection's average velocity
    /// (compress), or away from it when negative (expand).
    pub compress: f32,
    /// Largest random change either way.
    pub random: u8,
}

/// Apply a [`VelocityShape`]: compress or expand around the average, then
/// offset, then randomize. Velocities stay in 1..=127.
pub fn shape_velocities(
    notes: &mut [MidiNoteState],
    ids: &HashSet<u64>,
    spec: VelocityShape,
    seed: u64,
) -> usize {
    let mut order: Vec<usize> = (0..notes.len())
        .filter(|&i| ids.contains(&notes[i].id))
        .collect();
    if order.is_empty() {
        return 0;
    }
    order.sort_by(|&a, &b| notes[a].start.total_cmp(&notes[b].start));
    let mean = order.iter().map(|&i| notes[i].velocity as f32).sum::<f32>() / order.len() as f32;
    let pull = spec.compress.clamp(-100.0, 100.0) / 100.0;
    let mut rng = Jitter::new(seed);
    let mut changed = 0;
    for i in order {
        let v = notes[i].velocity as f32;
        let mut next = mean + (v - mean) * (1.0 - pull) + spec.offset as f32;
        if spec.random > 0 {
            next += (rng.next() * spec.random as f32).round();
        }
        let next = next.round().clamp(1.0, 127.0) as u8;
        if next != notes[i].velocity {
            notes[i].velocity = next;
            changed += 1;
        }
    }
    changed
}

/// Ids of notes that repeat another note: same pitch, starting within
/// [`CHORD_TOLERANCE_BEATS`]. Of each set the longest note stays (the first
/// one on a tie) and the rest are returned for deletion.
pub fn find_double_notes(notes: &[MidiNoteState], ids: &HashSet<u64>) -> Vec<u64> {
    let mut order: Vec<&MidiNoteState> = notes.iter().filter(|n| ids.contains(&n.id)).collect();
    order.sort_by(|a, b| {
        a.pitch
            .cmp(&b.pitch)
            .then(a.start.total_cmp(&b.start))
            .then(b.duration.total_cmp(&a.duration))
    });
    let mut doubles = Vec::new();
    let mut keep: Option<&MidiNoteState> = None;
    for note in order {
        match keep {
            Some(k) if k.pitch == note.pitch && note.start - k.start <= CHORD_TOLERANCE_BEATS => {
                if note.duration > k.duration + EPS {
                    doubles.push(k.id);
                    keep = Some(note);
                } else {
                    doubles.push(note.id);
                }
            }
            _ => keep = Some(note),
        }
    }
    doubles
}

/// Ids of notes shorter than `min_beats`.
pub fn find_short_notes(notes: &[MidiNoteState], ids: &HashSet<u64>, min_beats: f32) -> Vec<u64> {
    notes
        .iter()
        .filter(|n| ids.contains(&n.id) && n.duration + EPS < min_beats)
        .map(|n| n.id)
        .collect()
}

/// Ids of notes softer than `min_velocity`.
pub fn find_quiet_notes(notes: &[MidiNoteState], ids: &HashSet<u64>, min_velocity: u8) -> Vec<u64> {
    notes
        .iter()
        .filter(|n| ids.contains(&n.id) && n.velocity < min_velocity)
        .map(|n| n.id)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::timeline::timeline_state::ScaleRoot;

    fn note(pitch: u8, start: f32, duration: f32) -> MidiNoteState {
        MidiNoteState::new(pitch, start, duration, 100)
    }

    fn all(notes: &[MidiNoteState]) -> HashSet<u64> {
        notes.iter().map(|n| n.id).collect()
    }

    fn spec(step: f32) -> QuantizeSpec {
        QuantizeSpec {
            step,
            strength: 1.0,
            swing: 0.0,
        }
    }

    /// Doubling from the start keeps the first note where it is and spreads
    /// the rest out, lengths doubled; halving from the end keeps the last end.
    #[test]
    fn stretch_scales_positions_and_lengths_about_one_edge() {
        let mut notes = vec![note(60, 1.0, 0.5), note(62, 2.0, 1.0), note(64, 8.0, 1.0)];
        let ids: HashSet<u64> = [notes[0].id, notes[1].id].into_iter().collect();
        assert_eq!(note_span(&notes, &ids), Some((1.0, 3.0)));
        assert_eq!(
            stretch_notes(&mut notes, &ids, 2.0, StretchAnchor::Start),
            2
        );
        assert!((notes[0].start - 1.0).abs() < 1e-5 && (notes[0].duration - 1.0).abs() < 1e-5);
        assert!((notes[1].start - 3.0).abs() < 1e-5 && (notes[1].duration - 2.0).abs() < 1e-5);
        // A note outside the selection is untouched.
        assert!((notes[2].start - 8.0).abs() < 1e-5);

        let mut notes = vec![note(60, 0.0, 1.0), note(62, 3.0, 1.0)];
        let ids = all(&notes);
        stretch_notes(&mut notes, &ids, 0.5, StretchAnchor::End);
        assert!((notes[0].start - 2.0).abs() < 1e-5 && (notes[0].duration - 0.5).abs() < 1e-5);
        assert!((notes[1].start - 3.5).abs() < 1e-5);
        assert!((notes[1].start + notes[1].duration - 4.0).abs() < 1e-5);
        assert_eq!(
            stretch_notes(&mut notes, &ids, 1.0, StretchAnchor::Start),
            0
        );
    }

    /// A note's own curves are timed from its start, so they stretch with it.
    #[test]
    fn stretch_carries_the_notes_expression() {
        use super::super::{PitchCurve, PitchPoint, PitchSegmentShape};
        let mut first = note(60, 0.0, 1.0);
        first.pitch_curve = Some(PitchCurve::from_points(vec![PitchPoint::new(
            0.5,
            100.0,
            PitchSegmentShape::default(),
        )]));
        first
            .expression
            .pressure
            .points
            .push(sphere_midi_service::ExpressionPoint::new(0.25, 0.8));
        let mut notes = vec![first, note(62, 1.0, 1.0)];
        let ids = all(&notes);
        stretch_notes(&mut notes, &ids, 3.0, StretchAnchor::Start);
        let curve = notes[0].pitch_curve.as_ref().unwrap();
        assert!((curve.points[0].beat - 1.5).abs() < 1e-5);
        assert!((notes[0].expression.pressure.points[0].position - 0.75).abs() < 1e-5);
        assert!((notes[1].start - 3.0).abs() < 1e-5);
    }

    /// Stretching to the left never pushes a note before the clip's start.
    #[test]
    fn stretch_from_the_end_stops_at_the_clip_start() {
        let spec = StretchSpec {
            first: 1.0,
            last: 3.0,
            factor: 4.0,
            anchor: StretchAnchor::End,
        };
        assert_eq!(spec.map(1.0, 1.0).0, 0.0);
        assert_eq!(spec.stretched_span().0, 0.0);
    }

    #[test]
    fn hard_quantize_moves_starts_and_keeps_lengths() {
        let mut notes = vec![note(60, 0.27, 0.5), note(62, 0.9, 0.3)];
        let ids = all(&notes);
        assert_eq!(
            quantize_notes(&mut notes, &ids, spec(0.25), QuantizeTarget::Start),
            2
        );
        assert!((notes[0].start - 0.25).abs() < 1e-5);
        assert!((notes[1].start - 1.0).abs() < 1e-5);
        assert!((notes[0].duration - 0.5).abs() < 1e-5);
    }

    #[test]
    fn strength_moves_part_of_the_way() {
        let mut notes = vec![note(60, 0.2, 0.5)];
        let ids = all(&notes);
        let half = QuantizeSpec {
            strength: 0.5,
            ..spec(0.25)
        };
        quantize_notes(&mut notes, &ids, half, QuantizeTarget::Start);
        assert!((notes[0].start - 0.225).abs() < 1e-5);
    }

    #[test]
    fn full_swing_puts_the_offbeat_on_the_triplet() {
        let mut notes = vec![note(60, 0.7, 0.25)];
        let ids = all(&notes);
        let swung = QuantizeSpec {
            swing: 1.0,
            ..spec(0.5)
        };
        quantize_notes(&mut notes, &ids, swung, QuantizeTarget::Start);
        assert!((notes[0].start - 0.5 * 4.0 / 3.0).abs() < 1e-5);
    }

    #[test]
    fn end_quantize_never_collapses_a_note() {
        let mut notes = vec![note(60, 0.0, 0.05)];
        let ids = all(&notes);
        quantize_notes(&mut notes, &ids, spec(0.25), QuantizeTarget::End);
        assert!((notes[0].duration - 0.25).abs() < 1e-5);
    }

    #[test]
    fn length_quantize_rounds_to_whole_steps() {
        let mut notes = vec![note(60, 0.1, 0.6), note(62, 0.0, 0.05)];
        let ids = all(&notes);
        quantize_notes(&mut notes, &ids, spec(0.25), QuantizeTarget::Length);
        assert!((notes[0].duration - 0.5).abs() < 1e-5);
        assert!((notes[1].duration - 0.25).abs() < 1e-5);
        assert!((notes[0].start - 0.1).abs() < 1e-5);
    }

    #[test]
    fn unselected_notes_are_untouched() {
        let mut notes = vec![note(60, 0.27, 0.5), note(62, 0.9, 0.3)];
        let ids: HashSet<u64> = [notes[0].id].into_iter().collect();
        quantize_notes(&mut notes, &ids, spec(0.25), QuantizeTarget::Start);
        assert!((notes[1].start - 0.9).abs() < 1e-5);
    }

    #[test]
    fn legato_reaches_the_next_chord_not_its_own() {
        let mut notes = vec![note(60, 0.0, 0.2), note(64, 0.01, 0.2), note(67, 1.0, 0.2)];
        let ids = all(&notes);
        legato_notes(&mut notes, &ids, 0.0);
        assert!((notes[0].duration - 1.0).abs() < 1e-5);
        assert!((notes[1].duration - 0.99).abs() < 1e-5);
        assert!((notes[2].duration - 0.2).abs() < 1e-5);
    }

    #[test]
    fn mono_overlaps_trim_only_the_same_key() {
        let mut notes = vec![note(60, 0.0, 2.0), note(60, 1.0, 1.0), note(62, 0.5, 1.0)];
        let ids = all(&notes);
        assert_eq!(delete_overlaps(&mut notes, &ids, true), 1);
        assert!((notes[0].duration - 1.0).abs() < 1e-5);
        assert!((notes[2].duration - 1.0).abs() < 1e-5);
    }

    #[test]
    fn poly_overlaps_trim_against_any_key() {
        let mut notes = vec![note(60, 0.0, 2.0), note(62, 0.5, 1.0)];
        let ids = all(&notes);
        delete_overlaps(&mut notes, &ids, false);
        assert!((notes[0].duration - 0.5).abs() < 1e-5);
    }

    #[test]
    fn scale_steps_stay_on_the_scale() {
        let c_major = MidiScale {
            root: ScaleRoot::C,
            kind: ScaleKind::Major,
        };
        let mut notes = vec![note(64, 0.0, 1.0), note(61, 1.0, 1.0)];
        let ids = all(&notes);
        transpose_in_scale(&mut notes, &ids, c_major, 1);
        // E -> F (a half step is one degree here), C# snaps to C then -> D.
        assert_eq!(notes[0].pitch, 65);
        assert_eq!(notes[1].pitch, 62);
    }

    #[test]
    fn invert_mirrors_the_range() {
        let mut notes = vec![note(60, 0.0, 1.0), note(64, 1.0, 1.0), note(72, 2.0, 1.0)];
        let ids = all(&notes);
        invert_pitches(&mut notes, &ids);
        assert_eq!(
            notes.iter().map(|n| n.pitch).collect::<Vec<_>>(),
            vec![72, 68, 60]
        );
    }

    #[test]
    fn retrograde_mirrors_spans() {
        let mut notes = vec![note(60, 0.0, 1.0), note(62, 1.0, 2.0)];
        let ids = all(&notes);
        retrograde_notes(&mut notes, &ids);
        assert!((notes[0].start - 2.0).abs() < 1e-5);
        assert!((notes[1].start - 0.0).abs() < 1e-5);
    }

    #[test]
    fn chord_voicings() {
        let triad = || vec![note(60, 0.0, 1.0), note(64, 0.0, 1.0), note(67, 0.0, 1.0)];
        let pitches = |n: &[MidiNoteState]| {
            let mut p: Vec<u8> = n.iter().map(|n| n.pitch).collect();
            p.sort();
            p
        };
        let mut up = triad();
        let ids = all(&up);
        voice_chords(&mut up, &ids, ChordVoicing::InvertUp);
        assert_eq!(pitches(&up), vec![64, 67, 72]);

        let mut down = triad();
        let ids = all(&down);
        voice_chords(&mut down, &ids, ChordVoicing::InvertDown);
        assert_eq!(pitches(&down), vec![55, 60, 64]);

        let mut drop = triad();
        let ids = all(&drop);
        voice_chords(&mut drop, &ids, ChordVoicing::Drop2);
        assert_eq!(pitches(&drop), vec![52, 60, 67]);

        let mut open = vec![note(48, 0.0, 1.0), note(64, 0.0, 1.0), note(79, 0.0, 1.0)];
        let ids = all(&open);
        voice_chords(&mut open, &ids, ChordVoicing::Close);
        assert_eq!(pitches(&open), vec![48, 52, 55]);
    }

    #[test]
    fn a_single_note_is_not_a_chord() {
        let mut notes = vec![note(60, 0.0, 1.0), note(64, 1.0, 1.0)];
        let ids = all(&notes);
        assert_eq!(voice_chords(&mut notes, &ids, ChordVoicing::InvertUp), 0);
    }

    #[test]
    fn conform_snaps_to_chord_tones() {
        let mut notes = vec![note(61, 0.0, 1.0), note(66, 4.0, 1.0)];
        let ids = all(&notes);
        // C major over the first bar, nothing after it.
        conform_to_chords(&mut notes, &ids, |beat| (beat < 4.0).then(|| vec![0, 4, 7]));
        assert_eq!(notes[0].pitch, 60);
        assert_eq!(notes[1].pitch, 66);
    }

    #[test]
    fn strum_up_delays_from_the_bottom_and_keeps_ends() {
        let mut notes = vec![note(67, 0.0, 1.0), note(60, 0.0, 1.0), note(64, 0.0, 1.0)];
        let ids = all(&notes);
        strum_chords(&mut notes, &ids, true, |_, k| k as f32 * 0.1);
        let by_pitch = |p: u8| notes.iter().find(|n| n.pitch == p).unwrap().clone();
        assert!((by_pitch(60).start - 0.0).abs() < 1e-5);
        assert!((by_pitch(64).start - 0.1).abs() < 1e-5);
        assert!((by_pitch(67).start - 0.2).abs() < 1e-5);
        assert!((by_pitch(67).start + by_pitch(67).duration - 1.0).abs() < 1e-5);
    }

    #[test]
    fn strum_down_starts_from_the_top() {
        let mut notes = vec![note(60, 0.0, 1.0), note(64, 0.0, 1.0)];
        let ids = all(&notes);
        strum_chords(&mut notes, &ids, false, |_, k| k as f32 * 0.1);
        assert!((notes[1].start - 0.0).abs() < 1e-5);
        assert!((notes[0].start - 0.1).abs() < 1e-5);
    }

    fn ms_as_beats(_: f32, ms: f32) -> f32 {
        // 120 BPM: one beat is 500 ms.
        ms / 500.0
    }

    fn spec_strum(up: bool) -> StrumSpec {
        StrumSpec {
            up,
            alternate: false,
            spread_ms: 50.0,
            tension: 0.0,
            keep_ends: true,
            velocity_fade: 0,
        }
    }

    #[test]
    fn alternate_strums_change_direction_each_chord() {
        let mut notes = vec![
            note(60, 0.0, 1.0),
            note(64, 0.0, 1.0),
            note(60, 2.0, 1.0),
            note(64, 2.0, 1.0),
        ];
        let ids = all(&notes);
        let spec = StrumSpec {
            alternate: true,
            ..spec_strum(true)
        };
        strum_chords_with(&mut notes, &ids, spec, ms_as_beats);
        // First chord up: the high note is late. Second chord down: the low one is.
        assert!((notes[1].start - 0.1).abs() < 1e-5);
        assert!((notes[0].start - 0.0).abs() < 1e-5);
        assert!((notes[2].start - 2.1).abs() < 1e-5);
        assert!((notes[3].start - 2.0).abs() < 1e-5);
    }

    #[test]
    fn tension_bends_the_spacing_but_not_the_stroke() {
        let chord = || vec![note(60, 0.0, 1.0), note(64, 0.0, 1.0), note(67, 0.0, 1.0)];
        let middle = |tension: f32| {
            let mut notes = chord();
            let ids = all(&notes);
            let spec = StrumSpec {
                tension,
                ..spec_strum(true)
            };
            strum_chords_with(&mut notes, &ids, spec, ms_as_beats);
            assert!((notes[2].start - 0.2).abs() < 1e-5, "{tension}");
            notes[1].start
        };
        assert!((middle(0.0) - 0.1).abs() < 1e-5);
        // Speeding up: the middle note lands past the even midpoint.
        assert!(middle(1.0) > 0.1 + 1e-3);
        assert!(middle(-1.0) < 0.1 - 1e-3);
    }

    #[test]
    fn strum_can_move_whole_notes_and_fade() {
        let mut notes = vec![note(60, 0.0, 1.0), note(64, 0.0, 1.0)];
        let ids = all(&notes);
        let spec = StrumSpec {
            keep_ends: false,
            velocity_fade: -20,
            ..spec_strum(true)
        };
        strum_chords_with(&mut notes, &ids, spec, ms_as_beats);
        assert!((notes[1].duration - 1.0).abs() < 1e-5);
        assert_eq!(notes[1].velocity, 80);
        assert_eq!(notes[0].velocity, 100);
    }

    #[test]
    fn humanize_stays_in_bounds_and_repeats_with_its_seed() {
        let make = || {
            (0..32)
                .map(|i| note(60 + (i % 5) as u8, i as f32 * 0.5, 0.5))
                .collect::<Vec<_>>()
        };
        let spec = HumanizeSpec {
            timing_ms: 20.0,
            velocity: 10,
            length_pct: 10.0,
            keep_chords: false,
        };
        let mut a = make();
        let ids_a = all(&a);
        humanize_notes(&mut a, &ids_a, spec, 7, ms_as_beats);
        let originals = make();
        for (n, o) in a.iter().zip(&originals) {
            assert!((n.start - o.start).abs() <= 0.04 + 1e-5);
            assert!((n.velocity as i32 - 100).abs() <= 10);
            assert!((n.duration / o.duration - 1.0).abs() <= 0.1 + 1e-5);
        }
        let mut b = make();
        let ids_b = all(&b);
        humanize_notes(&mut b, &ids_b, spec, 7, ms_as_beats);
        let starts = |v: &[MidiNoteState]| v.iter().map(|n| n.start).collect::<Vec<_>>();
        assert_eq!(starts(&a), starts(&b));
    }

    #[test]
    fn humanize_keeps_chords_together() {
        let mut notes = vec![note(60, 1.0, 1.0), note(64, 1.0, 1.0), note(67, 1.0, 1.0)];
        let ids = all(&notes);
        let spec = HumanizeSpec {
            timing_ms: 30.0,
            velocity: 0,
            length_pct: 0.0,
            keep_chords: true,
        };
        humanize_notes(&mut notes, &ids, spec, 99, ms_as_beats);
        assert!((notes[0].start - notes[1].start).abs() < 1e-6);
        assert!((notes[1].start - notes[2].start).abs() < 1e-6);
    }

    #[test]
    fn legato_overlap_can_be_per_note() {
        let mut notes = vec![note(60, 0.0, 0.2), note(62, 1.0, 0.2)];
        let ids = all(&notes);
        legato_notes_by(&mut notes, &ids, |_| -0.1);
        assert!((notes[0].duration - 0.9).abs() < 1e-5);
    }

    #[test]
    fn velocity_shape_compresses_offsets_and_clamps() {
        let make = || {
            let mut v = vec![note(60, 0.0, 1.0), note(62, 1.0, 1.0), note(64, 2.0, 1.0)];
            v[0].velocity = 40;
            v[1].velocity = 80;
            v[2].velocity = 120;
            v
        };
        let vel = |v: &[MidiNoteState]| v.iter().map(|n| n.velocity).collect::<Vec<_>>();
        let shape = |offset, compress| VelocityShape {
            offset,
            compress,
            random: 0,
        };

        let mut n = make();
        let ids = all(&n);
        shape_velocities(&mut n, &ids, shape(0, 50.0), 1);
        assert_eq!(vel(&n), vec![60, 80, 100]);

        let mut n = make();
        let ids = all(&n);
        shape_velocities(&mut n, &ids, shape(20, 0.0), 1);
        assert_eq!(vel(&n), vec![60, 100, 127]);

        let mut n = make();
        let ids = all(&n);
        shape_velocities(&mut n, &ids, shape(0, -50.0), 1);
        assert_eq!(vel(&n), vec![20, 80, 127]);
    }

    #[test]
    fn random_velocity_stays_in_its_range() {
        let mut n: Vec<_> = (0..40).map(|i| note(60, i as f32, 0.5)).collect();
        let ids = all(&n);
        let spec = VelocityShape {
            offset: 0,
            compress: 0.0,
            random: 12,
        };
        shape_velocities(&mut n, &ids, spec, 3);
        assert!(n.iter().all(|n| (n.velocity as i32 - 100).abs() <= 12));
        assert!(n.iter().any(|n| n.velocity != 100));
    }

    #[test]
    fn length_tool_scales_or_ties_then_varies() {
        let mut n = vec![note(60, 0.0, 0.5), note(62, 1.0, 0.5)];
        let ids = all(&n);
        let scale = LengthSpec {
            multiply: 2.0,
            legato: false,
            gap_ms: 0.0,
            variation_pct: 0.0,
        };
        assert_eq!(articulate_notes(&mut n, &ids, scale, 1, ms_as_beats), 2);
        assert!((n[0].duration - 1.0).abs() < 1e-5);

        let mut n = vec![note(60, 0.0, 0.2), note(62, 1.0, 0.2)];
        let ids = all(&n);
        let tie = LengthSpec {
            legato: true,
            gap_ms: -50.0,
            ..scale
        };
        articulate_notes(&mut n, &ids, tie, 1, ms_as_beats);
        assert!((n[0].duration - 0.9).abs() < 1e-5);

        let mut n: Vec<_> = (0..20).map(|i| note(60, i as f32, 1.0)).collect();
        let ids = all(&n);
        let vary = LengthSpec {
            multiply: 1.0,
            legato: false,
            gap_ms: 0.0,
            variation_pct: 20.0,
        };
        articulate_notes(&mut n, &ids, vary, 5, ms_as_beats);
        assert!(n.iter().all(|n| (n.duration - 1.0).abs() <= 0.2 + 1e-5));
    }

    #[test]
    fn doubles_keep_the_longest() {
        let notes = vec![
            note(60, 0.0, 0.5),
            note(60, 0.01, 1.0),
            note(60, 1.0, 0.5),
            note(62, 0.0, 0.5),
        ];
        let ids = all(&notes);
        assert_eq!(find_double_notes(&notes, &ids), vec![notes[0].id]);
    }

    #[test]
    fn short_and_quiet_notes_are_found() {
        let mut notes = vec![note(60, 0.0, 0.05), note(62, 1.0, 1.0)];
        notes[1].velocity = 10;
        let ids = all(&notes);
        assert_eq!(find_short_notes(&notes, &ids, 0.125), vec![notes[0].id]);
        assert_eq!(find_quiet_notes(&notes, &ids, 20), vec![notes[1].id]);
    }

    /// Bars of 4/4 to beat 8, then 3/4: bar lines at 0, 4, 8, 11, 14, ...
    fn four_then_three() -> super::super::TimeSignatureMap {
        use super::super::{TimeSignatureMap, TimeSignaturePoint};
        TimeSignatureMap::with_points(vec![
            TimeSignaturePoint::with_id("m0", 0.0, 4, 4),
            TimeSignaturePoint::with_id("m1", 8.0, 3, 4),
        ])
    }

    /// #262: a clip at song beat 2 in 4/4 has its bar lines at clip beats 2,
    /// 6, ... (song 4, 8, ...). A note at clip beat 1 quantizes to clip beat 2
    /// (song beat 4), not clip beat 0 (song beat 2, mid-bar).
    #[test]
    fn bar_quantize_follows_the_bar_lines_of_a_clip_off_the_bar() {
        use super::super::GridStep;
        let map = four_then_three();
        let grid = MeterGrid::new(&map, GridStep::Bar, 2.0);
        let mut notes = vec![note(60, 1.0, 1.0), note(62, 0.5, 1.0), note(64, 5.0, 1.0)];
        let ids = all(&notes);
        assert_eq!(
            quantize_notes_on_grid(&mut notes, &ids, grid, spec(4.0), QuantizeTarget::Start),
            3
        );
        assert_eq!(notes[0].start, 2.0);
        // The bar line before the clip is outside it: the first one inside.
        assert_eq!(notes[1].start, 2.0);
        // Song beat 7 is nearer song beat 8 (clip 6) than song beat 4.
        assert_eq!(notes[2].start, 6.0);
    }

    /// #262: after a 3/4 change at beat 8 the bar lines are 8, 11, 14, so a
    /// note near beat 11 quantizes to 11 (a fixed 4-beat step from beat 0
    /// put it on 12, which is not a bar line).
    #[test]
    fn bar_quantize_follows_a_meter_change() {
        use super::super::GridStep;
        let map = four_then_three();
        let grid = MeterGrid::new(&map, GridStep::Bar, 0.0);
        let mut notes = vec![note(60, 11.6, 1.0), note(62, 13.0, 1.0)];
        let ids = all(&notes);
        quantize_notes_on_grid(&mut notes, &ids, grid, spec(4.0), QuantizeTarget::Start);
        assert_eq!(notes[0].start, 11.0);
        assert_eq!(notes[1].start, 14.0);

        // Ends land on bar lines; an end before the first line after the
        // start goes to that line.
        let mut ends = vec![note(60, 8.0, 2.2), note(62, 8.0, 0.3)];
        let ids = all(&ends);
        quantize_notes_on_grid(&mut ends, &ids, grid, spec(4.0), QuantizeTarget::End);
        assert_eq!(ends[0].duration, 3.0);
        assert_eq!(ends[1].duration, 3.0);

        // Lengths are whole 3/4 bars from a note on the change.
        let mut lengths = vec![note(60, 8.0, 2.0), note(62, 8.0, 5.0), note(64, 4.0, 5.0)];
        let ids = all(&lengths);
        quantize_notes_on_grid(&mut lengths, &ids, grid, spec(4.0), QuantizeTarget::Length);
        assert_eq!(lengths[0].duration, 3.0);
        assert_eq!(lengths[1].duration, 6.0);
        // From beat 4: one bar is 4 beats (to 8), two bars 7 (to 11).
        assert_eq!(lengths[2].duration, 4.0);

        // Strength pulls part of the way.
        let mut soft = vec![note(60, 10.0, 1.0)];
        let ids = all(&soft);
        let half = QuantizeSpec {
            strength: 0.5,
            ..spec(4.0)
        };
        quantize_notes_on_grid(&mut soft, &ids, grid, half, QuantizeTarget::Start);
        assert_eq!(soft[0].start, 10.5);
    }
}

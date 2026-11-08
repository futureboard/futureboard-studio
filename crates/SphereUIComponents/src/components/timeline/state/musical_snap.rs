//! Shared musical snapping used by Timeline, Piano Roll, CC lanes, automation,
//! loop range, and markers.
//!
//! Beat positions are the canonical musical-time unit across editors. Tempo
//! changes must not rewrite stored beat positions — only the seconds mapping
//! changes. Time-signature changes affect bar-aligned snap via `beats_per_bar`.

use super::viewport::SnapDivision;

/// Tick resolution used when converting beats ↔ integer musical ticks.
/// 960 PPQN keeps common straight / triplet / dotted divisions exact.
pub const TICKS_PER_QUARTER: i64 = 960;

/// How a base grid division is further shaped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SnapShape {
    #[default]
    Straight,
    /// Multiply the base step by 1.5 (e.g. dotted 1/8 = 0.75 beats).
    Dotted,
    /// Multiply the base step by 2/3 (e.g. 1/8 triplet = 1/3 beat).
    Triplet,
}

impl SnapShape {
    pub const ALL: [SnapShape; 3] = [SnapShape::Straight, SnapShape::Dotted, SnapShape::Triplet];

    pub fn label(&self) -> &'static str {
        match self {
            SnapShape::Straight => "Straight",
            SnapShape::Dotted => "Dotted",
            SnapShape::Triplet => "Triplet",
        }
    }

    /// Stable id used in the `timeline:set-grid-shape:<id>` command.
    pub fn command_id(&self) -> &'static str {
        match self {
            SnapShape::Straight => "straight",
            SnapShape::Dotted => "dotted",
            SnapShape::Triplet => "triplet",
        }
    }

    pub fn from_command_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|shape| shape.command_id() == id)
    }
}

/// Complete snap configuration for one editor gesture.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MusicalSnap {
    pub enabled: bool,
    pub division: SnapDivision,
    pub shape: SnapShape,
    pub beats_per_bar: f64,
    pub auto_step_beats: f64,
}

impl MusicalSnap {
    pub fn off() -> Self {
        Self {
            enabled: false,
            division: SnapDivision::Off,
            shape: SnapShape::Straight,
            beats_per_bar: 4.0,
            auto_step_beats: 0.25,
        }
    }

    /// Resolve the active grid step in beats. Returns `None` when snapping is
    /// disabled or the division is Off.
    pub fn step_beats(self) -> Option<f64> {
        if !self.enabled || self.division == SnapDivision::Off {
            return None;
        }
        let base = match self.division {
            SnapDivision::Auto => self.auto_step_beats,
            SnapDivision::Bar1 => self.beats_per_bar,
            other => other.step_beats(self.beats_per_bar as f32) as f64,
        };
        if base <= 0.0 {
            return None;
        }
        let stepped = match self.shape {
            SnapShape::Straight => base,
            SnapShape::Dotted => base * 1.5,
            SnapShape::Triplet => base * (2.0 / 3.0),
        };
        Some(stepped.max(f64::EPSILON))
    }
}

/// Convert beats to integer ticks (round-to-nearest, away from zero on .5).
#[inline]
pub fn beats_to_ticks(beats: f64) -> i64 {
    let ticks = beats * TICKS_PER_QUARTER as f64;
    if ticks >= 0.0 {
        (ticks + 0.5).floor() as i64
    } else {
        (ticks - 0.5).ceil() as i64
    }
}

/// Convert integer ticks back to beats.
#[inline]
pub fn ticks_to_beats(ticks: i64) -> f64 {
    ticks as f64 / TICKS_PER_QUARTER as f64
}

/// Snap `beat` to the grid. When `bypass` is true (Shift held), returns the
/// unsnapped beat. Negative / pre-roll beats are preserved (not clamped to 0).
pub fn snap_beat(beat: f64, snap: MusicalSnap, bypass: bool) -> f64 {
    if bypass {
        return beat;
    }
    let Some(step) = snap.step_beats() else {
        return beat;
    };
    (beat / step).round() * step
}

/// Snap while preserving the grab offset between the pointer and the object's
/// origin. `raw_beat` is the pointer musical position; `grab_offset` is
/// `object_origin - pointer_at_press` (usually ≤ 0 for a grab inside the object).
pub fn snap_beat_with_grab_offset(
    raw_beat: f64,
    grab_offset: f64,
    snap: MusicalSnap,
    bypass: bool,
) -> f64 {
    let unsnapped = raw_beat + grab_offset;
    if bypass {
        return unsnapped;
    }
    let Some(step) = snap.step_beats() else {
        return unsnapped;
    };
    // Snap the object origin, not the cursor, so crossing a boundary does not
    // yank the object by the grab-offset distance.
    (unsnapped / step).round() * step
}

/// Snap a resize edge independently of the opposite edge.
pub fn snap_resize_edge(edge_beat: f64, snap: MusicalSnap, bypass: bool) -> f64 {
    snap_beat(edge_beat, snap, bypass)
}

/// Multi-selection move: snap the anchor object's proposed origin, then apply
/// the same delta to every peer so internal spacing is preserved.
pub fn multi_select_move_delta(
    anchor_origin: f64,
    proposed_origin: f64,
    snap: MusicalSnap,
    bypass: bool,
) -> f64 {
    let snapped = snap_beat(proposed_origin, snap, bypass);
    snapped - anchor_origin
}

/// Relative snap: quantize a delta so a group can land halfway between visible
/// grid lines when the chosen resolution permits it.
pub fn snap_relative_delta(delta: f64, snap: MusicalSnap, bypass: bool) -> f64 {
    if bypass {
        return delta;
    }
    let Some(step) = snap.step_beats() else {
        return delta;
    };
    (delta / step).round() * step
}

/// The bar lines of a meter map, the way every editor draws them: bars count
/// from each meter's start, the first meter reaches back to the song start,
/// and a meter change is a bar line of its own even when it cuts the previous
/// meter's last bar short.
pub trait MeterBars {
    /// `(start, end)` of the bar containing song-axis `beat`. A beat before
    /// the song start belongs to the first bar.
    fn bar_span(&self, beat: f64) -> (f64, f64);
}

impl MeterBars for super::time_signature::TimeSignatureMap {
    fn bar_span(&self, beat: f64) -> (f64, f64) {
        let beat = beat.max(0.0);
        let mut start = 0.0;
        let mut bar = 4.0;
        let mut end = f64::INFINITY;
        let mut points: Vec<(f64, f64)> = self
            .points
            .iter()
            .map(|p| {
                let bar = super::time_signature::beats_per_bar_from_sig(
                    p.numerator.max(1),
                    p.denominator,
                );
                (p.beat.max(0.0), bar.max(0.25))
            })
            .collect();
        points.sort_by(|a, b| a.0.total_cmp(&b.0));
        for (i, &(at, bar_beats)) in points.iter().enumerate() {
            // The first meter reaches back to the song start.
            let seg_start = if i == 0 { 0.0 } else { at };
            if i > 0 && beat < seg_start {
                end = seg_start;
                break;
            }
            start = seg_start;
            bar = bar_beats;
        }
        let bar_start = start + ((beat - start) / bar).floor() * bar;
        (bar_start, (bar_start + bar).min(end))
    }
}

/// The step of a [`MeterGrid`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GridStep {
    /// Every bar line of the meter: the "1 Bar" grid.
    Bar,
    /// A fixed step, in quarter beats, counted from the local frame's zero.
    Beats(f64),
}

/// Lines a [`MeterGrid`] walk returns at most, so a degenerate step cannot
/// stall the UI thread.
const MAX_GRID_LINES: usize = 1 << 16;

/// Distance, in beats, under which two grid positions are the same line.
const GRID_EPS: f64 = 1.0e-4;

/// The one meter-aware edit grid: piano-roll notes, controller (CC) lanes and
/// arrangement automation lanes all snap, walk and duplicate through it, so a
/// controller point or automation point lands on the line the note above it
/// lands on.
///
/// Positions are in a local frame that starts `origin` beats into the song —
/// a clip's frame for notes and controller points (`origin` = the clip
/// start), the song axis for automation (`origin` = 0).
///
/// * [`GridStep::Bar`] lines are the meter's bar lines, found on the song
///   axis ([`MeterBars`]): a clip that starts off a bar line still snaps to
///   the bar lines drawn over it, and a meter change inside the clip changes
///   the bar length from the change on.
/// * [`GridStep::Beats`] lines are every `step` beats from the local zero —
///   a clip's fixed grid counts from the clip start, as it always has.
pub struct MeterGrid<'a, M: MeterBars + ?Sized> {
    meter: &'a M,
    step: GridStep,
    origin: f64,
}

impl<M: MeterBars + ?Sized> Clone for MeterGrid<'_, M> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<M: MeterBars + ?Sized> Copy for MeterGrid<'_, M> {}

impl<'a, M: MeterBars + ?Sized> MeterGrid<'a, M> {
    pub fn new(meter: &'a M, step: GridStep, origin: f64) -> Self {
        Self {
            meter,
            step,
            origin,
        }
    }

    pub fn step(&self) -> GridStep {
        self.step
    }

    /// The grid cell holding local `beat`: the line at or before it and the
    /// next line.
    pub fn cell(&self, beat: f64) -> (f64, f64) {
        match self.step {
            GridStep::Bar => {
                let (start, end) = self.meter.bar_span(beat + self.origin);
                (start - self.origin, end - self.origin)
            }
            GridStep::Beats(step) => {
                let step = step.max(GRID_EPS);
                let k = (beat / step).floor();
                (k * step, (k + 1.0) * step)
            }
        }
    }

    /// The line nearest local `beat`; a beat halfway between two lines takes
    /// the earlier one.
    pub fn snap(&self, beat: f64) -> f64 {
        match self.step {
            GridStep::Beats(step) => {
                let step = step.max(GRID_EPS);
                (beat / step).round() * step
            }
            GridStep::Bar => {
                let (below, above) = self.cell(beat);
                if beat - below <= above - beat {
                    below
                } else {
                    above
                }
            }
        }
    }

    /// The line at or before local `beat`. A beat a rounding error short of
    /// a line counts as on it.
    pub fn line_at_or_before(&self, beat: f64) -> f64 {
        let (below, above) = self.cell(beat);
        if above - beat <= GRID_EPS {
            above
        } else {
            below
        }
    }

    /// The line after `line`, which is itself a line of this grid.
    pub fn next_line(&self, line: f64) -> f64 {
        let (_, above) = self.cell(line);
        if above - line <= GRID_EPS {
            // `line` sat a rounding error short of the line it stands for.
            self.cell(above).1
        } else {
            above
        }
    }

    /// `a`, every line strictly between `a` and `b`, and `b`, ascending —
    /// the points a snapped stroke or ramp from `a` to `b` writes. With both
    /// ends on lines that is exactly the lines from one to the other; one
    /// entry when they are the same position.
    pub fn lines_between(&self, a: f64, b: f64) -> Vec<f64> {
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        let mut lines = vec![lo];
        if hi - lo <= GRID_EPS {
            return lines;
        }
        let mut line = self.next_line(self.line_at_or_before(lo));
        while line < hi - GRID_EPS && lines.len() < MAX_GRID_LINES {
            if line > lo + GRID_EPS {
                lines.push(line);
            }
            line = self.next_line(line);
        }
        lines.push(hi);
        lines
    }

    /// How far Duplicate moves the copies of a selection spanning local
    /// `[start, end)`.
    ///
    /// The rule: the copies keep the selection's place in its grid cell and
    /// land on the first such place at or after the selection's end. Walk the
    /// grid from the line at or before `start`, one line at a time — with the
    /// "1 Bar" grid one bar at a time, each bar as long as the meter in force
    /// there makes it — until that line plus the selection's offset into its
    /// cell reaches `end`. Under one meter this is the selection length
    /// rounded up to whole grid steps (never less than one step), so a
    /// four-beat selection in 4/4 moves one bar wherever it starts; across a
    /// meter change it is the place the note grid counts the same number of
    /// bars on, which is not the same number of beats.
    pub fn duplicate_offset(&self, start: f64, end: f64) -> f64 {
        let span = (end - start).max(0.0);
        if let GridStep::Beats(step) = self.step {
            let step = step.max(GRID_EPS);
            let steps = ((span - GRID_EPS) / step).ceil().max(1.0);
            return steps * step;
        }
        let anchor = self.line_at_or_before(start);
        let into_cell = start - anchor;
        let mut line = anchor;
        for _ in 0..MAX_GRID_LINES {
            line = self.next_line(line);
            if line + into_cell >= end - GRID_EPS {
                break;
            }
        }
        line + into_cell - start
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(div: SnapDivision, shape: SnapShape) -> MusicalSnap {
        MusicalSnap {
            enabled: true,
            division: div,
            shape,
            beats_per_bar: 4.0,
            auto_step_beats: 0.25,
        }
    }

    #[test]
    fn shift_bypass_disables_snap() {
        let s = snap(SnapDivision::Div1_4, SnapShape::Straight);
        assert_eq!(snap_beat(1.3, s, true), 1.3);
    }

    #[test]
    fn quarter_snap_rounds_to_nearest() {
        let s = snap(SnapDivision::Div1_4, SnapShape::Straight);
        assert_eq!(snap_beat(1.24, s, false), 1.0);
        assert_eq!(snap_beat(1.26, s, false), 1.0);
        assert_eq!(snap_beat(1.6, s, false), 2.0);
    }

    #[test]
    fn triplet_eighth_step() {
        let s = snap(SnapDivision::Div1_8, SnapShape::Triplet);
        let step = s.step_beats().unwrap();
        assert!((step - (1.0 / 3.0)).abs() < 1e-9);
        let snapped = snap_beat(0.4, s, false);
        assert!((snapped - (1.0 / 3.0)).abs() < 1e-9);
    }

    #[test]
    fn dotted_eighth_step() {
        let s = snap(SnapDivision::Div1_8, SnapShape::Dotted);
        let step = s.step_beats().unwrap();
        assert!((step - 0.75).abs() < 1e-9);
        assert_eq!(snap_beat(0.8, s, false), 0.75);
    }

    #[test]
    fn bar_snap_respects_time_signature() {
        let mut s = snap(SnapDivision::Bar1, SnapShape::Straight);
        s.beats_per_bar = 3.0;
        assert_eq!(snap_beat(2.6, s, false), 3.0);
        assert_eq!(snap_beat(1.2, s, false), 0.0);
    }

    #[test]
    fn negative_preroll_positions_snap() {
        let s = snap(SnapDivision::Div1_4, SnapShape::Straight);
        assert_eq!(snap_beat(-0.6, s, false), -1.0);
        assert_eq!(snap_beat(-0.1, s, false), 0.0);
    }

    #[test]
    fn grab_offset_snaps_object_origin() {
        let s = snap(SnapDivision::Div1_4, SnapShape::Straight);
        // Pointer at 1.3, grabbed 0.2 beats into the object → origin 1.1 → snap 1.0
        let origin = snap_beat_with_grab_offset(1.3, -0.2, s, false);
        assert_eq!(origin, 1.0);
    }

    #[test]
    fn multi_selection_preserves_spacing() {
        let s = snap(SnapDivision::Div1_4, SnapShape::Straight);
        let a = 0.0;
        let b = 1.5;
        let delta = multi_select_move_delta(a, 1.3, s, false);
        assert_eq!(delta, 1.0);
        assert_eq!(a + delta, 1.0);
        assert_eq!(b + delta, 2.5);
    }

    #[test]
    fn ticks_roundtrip_common_divisions() {
        for beats in [0.0, 0.25, 0.5, 1.0, 1.5, 2.0 / 3.0, 0.75] {
            let t = beats_to_ticks(beats);
            let back = ticks_to_beats(t);
            assert!(
                (back - beats).abs() < 1e-12,
                "beats={beats} ticks={t} back={back}"
            );
        }
    }

    #[test]
    fn high_zoom_auto_step_still_snaps() {
        let mut s = snap(SnapDivision::Auto, SnapShape::Straight);
        s.auto_step_beats = 1.0 / 64.0;
        let snapped = snap_beat(0.02, s, false);
        assert!((snapped - (1.0 / 64.0)).abs() < 1e-9 || snapped.abs() < 1e-9);
    }

    #[test]
    fn relative_delta_snap() {
        let s = snap(SnapDivision::Div1_16, SnapShape::Straight);
        assert_eq!(snap_relative_delta(0.3, s, false), 0.25);
        assert_eq!(snap_relative_delta(0.3, s, true), 0.3);
    }
}

#[cfg(test)]
mod grid_menu_id_tests {
    use super::*;

    /// The grid dropdown sends `timeline:set-grid:<id>` / `-shape:<id>`; every
    /// entry it offers has to parse back to itself.
    #[test]
    fn grid_menu_ids_round_trip() {
        for division in SnapDivision::MENU {
            assert_eq!(
                SnapDivision::from_command_id(division.command_id()),
                Some(division)
            );
        }
        assert_eq!(
            SnapDivision::from_command_id("off"),
            Some(SnapDivision::Off)
        );
        for shape in SnapShape::ALL {
            assert_eq!(SnapShape::from_command_id(shape.command_id()), Some(shape));
        }
        assert_eq!(SnapDivision::from_command_id("3"), None);
    }
}

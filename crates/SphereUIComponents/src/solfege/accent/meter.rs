//! Metrical structure: where a note sits in its bar, and what that is worth.
//!
//! A bar is not a list of equal slots. Beat 1 of a 4/4 bar is not beat 2, beat
//! 3 is not beat 2 either, and the eighth between them is weaker again — that
//! hierarchy is what makes a note on a weak subdivision *sound* like it is
//! pushing against something.
//!
//! Nothing here is specific to 4/4. The grid comes from the time signature, and
//! specifically from the same [`TimeSignaturePoint::effective_grouping`] the
//! bar ruler already draws, so a 7/8 the user has regrouped as 3+2+2 is
//! analysed the way it is displayed. A 3/2 bar is six quarter-note beats, not
//! three: bar length is `numerator * 4 / denominator`, and reading the
//! numerator alone folds two bars into one and puts every other downbeat on
//! beat 3.
//!
//! This file is the runtime half of a pair. `neural/accent/meter.py` is the
//! training half, function for function and constant for constant, and
//! `accent::parity` checks the two against a shared fixture — a model trained
//! on one grid and run against another has been given different music from the
//! one it was taught on.

use crate::components::timeline::timeline_state::{
    beats_per_bar_from_sig, denominator_unit_quarter_beats, normalize_time_signature_grouping,
};

/// Strength of each metrical level, strongest first. Halving per level is the
/// usual reading of metrical weight and it keeps the numbers interpretable: a
/// beat is worth half a bar line, an offbeat half a beat.
pub const LEVEL_STRENGTHS: [f32; 6] = [1.0, 0.75, 0.5, 0.25, 0.125, 0.0625];

/// Strength given to a position that matches no level of the grid at all.
pub const OFF_GRID_STRENGTH: f32 = 0.03125;

/// A note counts as "on" a metrical position when it is within half the finest
/// grid step of it.
///
/// Scores are exact and would not need this. Played MIDI is not: a take
/// recorded into the DAW lands a few milliseconds either side of every beat,
/// and a strength function that demanded exactness would return the off-grid
/// floor for every note in it and quietly switch meter off.
pub const GRID_TOLERANCE_FRACTION: f32 = 0.5;

/// How many metrical levels are candidates for a note to *displace* by
/// sustaining across them: bar line, half-bar, and beat. Sustaining across an
/// offbeat eighth is what every quarter note does.
const SYNCOPATION_LEVELS: usize = 3;

/// One time signature, resolved into a metrical grid.
///
/// Positions are in quarter-note beats from the bar line, which is the unit the
/// timeline itself uses.
#[derive(Debug, Clone, PartialEq)]
pub struct Meter {
    pub numerator: u16,
    pub denominator: u16,
    /// Bar length in quarter-note beats.
    pub bar_beats: f32,
    /// One denominator unit in quarter-note beats.
    pub unit_beats: f32,
    /// How many denominator units each beat contains, in order. `[1,1,1,1]`
    /// for 4/4, `[3,3]` for 6/8, `[2,2,3]` for 7/8.
    pub groups: Vec<u16>,
    /// The grid, built once.
    ///
    /// It depends only on the signature and its grouping, and `beat_strength`
    /// is called at least twice per note. Rebuilding it per call — six vectors,
    /// two sorts and a dedup each time — cost 9.3 ms to analyse a thousand
    /// notes; caching it here costs 0.2 ms. That is the difference between a
    /// pass that could not run on a long clip and one that could run anywhere.
    levels: Vec<Vec<f32>>,
    /// Half the finest grid step: how far off a position may be and still count
    /// as on it.
    tolerance: f32,
}

impl Meter {
    /// Resolve a signature and its accent grouping into a grid.
    ///
    /// `grouping` is the project's own, normalized: a single entry means "no
    /// internal grouping", so the beat is the denominator unit and 4/4 has four
    /// of them. More than one entry is a compound or additive meter whose beats
    /// begin at the cumulative group boundaries.
    pub fn new(numerator: u16, denominator: u16, grouping: &[u16]) -> Self {
        let normalized = normalize_time_signature_grouping(numerator, denominator, grouping);
        let groups = if normalized.len() > 1 {
            normalized
        } else {
            vec![1; numerator.max(1) as usize]
        };
        let mut meter = Self {
            numerator,
            denominator,
            bar_beats: beats_per_bar_from_sig(numerator, denominator) as f32,
            unit_beats: denominator_unit_quarter_beats(denominator) as f32,
            groups,
            levels: Vec::new(),
            tolerance: 0.0,
        };
        meter.levels = meter.build_levels();
        meter.tolerance = meter.build_finest_step() * GRID_TOLERANCE_FRACTION;
        meter
    }

    /// A plain signature with the project's default grouping.
    pub fn from_signature(numerator: u16, denominator: u16) -> Self {
        Self::new(numerator, denominator, &[])
    }

    pub fn beat_count(&self) -> usize {
        self.groups.len()
    }

    /// Every group is three denominator units — 6/8, 9/8, 12/8.
    pub fn is_compound(&self) -> bool {
        self.groups.len() > 1 && self.groups.iter().all(|&group| group == 3)
    }

    /// The groups are not all the same length — 5/8, 7/8.
    pub fn is_irregular(&self) -> bool {
        self.groups.iter().any(|&group| group != self.groups[0])
    }

    /// Start of each beat, in quarter-note beats from the bar line.
    pub fn beat_starts(&self) -> Vec<f32> {
        let mut starts = Vec::with_capacity(self.groups.len());
        let mut position = 0.0_f32;
        for &group in &self.groups {
            starts.push(position);
            position += group as f32 * self.unit_beats;
        }
        starts
    }

    /// Grid positions per metrical level, strongest level first.
    ///
    /// Coarser levels are not removed from finer ones; the lookup takes the
    /// strongest level a position matches, so a downbeat that also appears in
    /// the beat level still reads 1.0.
    pub fn levels(&self) -> &[Vec<f32>] {
        &self.levels
    }

    fn build_levels(&self) -> Vec<Vec<f32>> {
        let bar = self.bar_beats;
        let starts = self.beat_starts();

        let bar_line = vec![0.0_f32];
        // A half-bar level exists only where the bar really divides in two:
        // 4/4 and 12/8 have a secondary accent in the middle, 3/4 and 7/8 do
        // not, and inventing one for them would put a stress where players
        // place none.
        let half_bar =
            if self.beat_count() >= 4 && self.beat_count() % 2 == 0 && !self.is_irregular() {
                vec![bar / 2.0]
            } else {
                Vec::new()
            };

        // First division of each beat: into its own denominator units for a
        // compound or additive beat (three eighths under a dotted quarter),
        // into halves for a simple one.
        let mut divisions = Vec::new();
        for (index, &group) in self.groups.iter().enumerate() {
            let start = starts[index];
            let length = group as f32 * self.unit_beats;
            let parts = if group > 1 { group as usize } else { 2 };
            for step in 1..parts {
                divisions.push(start + length * step as f32 / parts as f32);
            }
        }

        let mut previous: Vec<f32> = bar_line
            .iter()
            .chain(half_bar.iter())
            .chain(starts.iter())
            .chain(divisions.iter())
            .copied()
            .collect();
        sort_dedup(&mut previous);

        // Two further halvings.
        let mut finer: Vec<Vec<f32>> = Vec::with_capacity(2);
        for _ in 0..2 {
            let mut extended = previous.clone();
            extended.push(bar);
            let mut midpoints: Vec<f32> = extended
                .windows(2)
                .map(|pair| (pair[0] + pair[1]) / 2.0)
                .collect();
            sort_dedup(&mut midpoints);
            previous.extend_from_slice(&midpoints);
            sort_dedup(&mut previous);
            finer.push(midpoints);
        }

        let mut levels = vec![bar_line, half_bar, starts, divisions];
        levels.extend(finer);
        levels
    }

    /// Spacing of the finest level, in quarter-note beats.
    pub fn finest_step(&self) -> f32 {
        self.tolerance / GRID_TOLERANCE_FRACTION
    }

    fn build_finest_step(&self) -> f32 {
        let mut positions: Vec<f32> = self.levels.iter().flatten().copied().collect();
        positions.push(0.0);
        positions.push(self.bar_beats);
        sort_dedup(&mut positions);
        positions
            .windows(2)
            .map(|pair| pair[1] - pair[0])
            .filter(|gap| *gap > 1.0e-9)
            .fold(self.bar_beats, f32::min)
    }

    /// Metrical weight of a position, in `0..=1`.
    ///
    /// `position_beats` is quarter-note beats from the start of the piece; the
    /// bar position is taken modulo the bar length here rather than by the
    /// caller, so a caller cannot get the modulus wrong for a meter whose bar
    /// is not `numerator` beats long.
    #[allow(
        clippy::neg_cmp_op_on_partial_ord,
        reason = "`!(bar > 0.0)` is deliberately true for a NaN bar length, which reads as off-grid"
    )]
    pub fn beat_strength(&self, position_beats: f32) -> f32 {
        let bar = self.bar_beats;
        if !(bar > 0.0) || !position_beats.is_finite() {
            return OFF_GRID_STRENGTH;
        }
        let position = position_beats.rem_euclid(bar);
        let tolerance = self.tolerance;
        // A note a hair before the bar line belongs to the bar line, not to the
        // last subdivision of the bar before it.
        if bar - position <= tolerance {
            return LEVEL_STRENGTHS[0];
        }
        for (level, positions) in self.levels.iter().enumerate() {
            if positions
                .iter()
                .any(|grid| (position - grid).abs() <= tolerance)
            {
                return LEVEL_STRENGTHS[level.min(LEVEL_STRENGTHS.len() - 1)];
            }
        }
        OFF_GRID_STRENGTH
    }

    /// How much stronger a metrical position this note covers than the one it
    /// starts on.
    ///
    /// Zero for a note that starts on the strongest position it touches — every
    /// note beginning on a downbeat, and every short note inside a beat.
    /// Positive when a note begins somewhere weak and *holds through* somewhere
    /// strong: nothing articulates the strong beat, and the ear hears the note
    /// as displacing it. That is why "strong beat = accent" is not enough on
    /// its own — the emphasised note here is the one *before* the strong beat.
    #[allow(
        clippy::neg_cmp_op_on_partial_ord,
        reason = "the negated comparisons are deliberately true for NaN, so a NaN duration or bar has no syncopation"
    )]
    pub fn syncopation(&self, position_beats: f32, duration_beats: f32) -> f32 {
        let bar = self.bar_beats;
        if !(duration_beats > 0.0) || !(bar > 0.0) || !position_beats.is_finite() {
            return 0.0;
        }
        let start_strength = self.beat_strength(position_beats);
        let tolerance = self.tolerance;
        let end = position_beats + duration_beats;
        let levels = &self.levels;

        let mut strongest = 0.0_f32;
        let first_bar = (position_beats / bar).floor() as i64;
        let last_bar = (end / bar).floor() as i64;
        // A note longer than a few bars is a pedal, not a syncopation, and
        // walking every bar it covers is unbounded work for no answer.
        let last_bar = last_bar.min(first_bar + 8);
        for bar_index in first_bar..=last_bar {
            let origin = bar_index as f32 * bar;
            for (level, positions) in levels.iter().take(SYNCOPATION_LEVELS).enumerate() {
                for grid in positions {
                    let absolute = origin + grid;
                    // Strictly inside: a position the note starts on is not one
                    // it displaces, and one it merely reaches the end of is not
                    // held through.
                    if absolute > position_beats + tolerance && absolute < end - tolerance {
                        strongest = strongest.max(LEVEL_STRENGTHS[level]);
                    }
                }
            }
        }
        (strongest - start_strength).max(0.0)
    }
}

fn sort_dedup(values: &mut Vec<f32>) {
    values.sort_by(f32::total_cmp);
    values.dedup_by(|a, b| (*a - *b).abs() < 1.0e-6);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strengths(numerator: u16, denominator: u16, step: f32) -> Vec<f32> {
        let meter = Meter::from_signature(numerator, denominator);
        let count = (meter.bar_beats / step).round() as usize;
        (0..count)
            .map(|index| meter.beat_strength(index as f32 * step))
            .collect()
    }

    #[test]
    fn four_four_has_a_secondary_accent_in_the_middle_of_the_bar() {
        assert_eq!(
            strengths(4, 4, 0.5),
            vec![1.0, 0.25, 0.5, 0.25, 0.75, 0.25, 0.5, 0.25]
        );
    }

    /// Three beats do not divide in two, so there is no half-bar level to find.
    #[test]
    fn three_four_has_no_half_bar_accent() {
        assert_eq!(strengths(3, 4, 0.5), vec![1.0, 0.25, 0.5, 0.25, 0.5, 0.25]);
    }

    /// The bug this replaces: a 3/2 bar is six quarter beats, and treating the
    /// numerator as the bar length put a downbeat on beat 3 of every other bar.
    #[test]
    fn a_half_note_meter_measures_its_bar_in_quarter_beats() {
        let meter = Meter::from_signature(3, 2);
        assert_eq!(meter.bar_beats, 6.0);
        assert_eq!(meter.beat_strength(0.0), 1.0);
        assert_eq!(meter.beat_strength(2.0), 0.5);
        assert_eq!(meter.beat_strength(4.0), 0.5);
        assert_eq!(meter.beat_strength(6.0), 1.0, "next bar line");
        assert_eq!(meter.beat_strength(3.0), 0.25, "not a beat in 3/2");
    }

    #[test]
    fn a_compound_meter_beats_in_dotted_quarters() {
        let meter = Meter::from_signature(6, 8);
        assert_eq!(meter.bar_beats, 3.0);
        assert!(meter.is_compound());
        assert_eq!(meter.beat_starts(), vec![0.0, 1.5]);
        assert_eq!(meter.beat_strength(1.5), 0.5);
        assert_eq!(
            meter.beat_strength(1.0),
            0.25,
            "the third eighth is a division"
        );
    }

    #[test]
    fn an_additive_meter_beats_where_its_groups_start() {
        let meter = Meter::from_signature(7, 8);
        assert!(meter.is_irregular());
        assert_eq!(meter.beat_starts(), vec![0.0, 1.0, 2.0]);
        assert_eq!(meter.beat_strength(2.0), 0.5);
        assert_eq!(
            meter.beat_strength(1.75),
            0.125,
            "7/8 has no beat between its groups"
        );
    }

    /// The user can regroup a meter in the bar ruler; the analyser must follow
    /// what is displayed rather than a table of its own.
    #[test]
    fn a_regrouped_meter_moves_its_strong_beats() {
        let default = Meter::from_signature(7, 8);
        let regrouped = Meter::new(7, 8, &[3, 2, 2]);
        assert_eq!(regrouped.beat_starts(), vec![0.0, 1.5, 2.5]);
        assert_eq!(regrouped.beat_strength(1.5), 0.5);
        assert_ne!(default.beat_strength(1.5), regrouped.beat_strength(1.5));
    }

    #[test]
    fn a_note_held_across_a_stronger_beat_is_syncopated() {
        let meter = Meter::from_signature(4, 4);
        // A half note on beat 2 covers beat 3, which is stronger than beat 2.
        assert_eq!(meter.syncopation(1.0, 2.0), 0.25);
        // An eighth on the "and" of 2, held through beats 3 and 4.
        assert_eq!(meter.syncopation(1.5, 1.5), 0.5);
        // A quarter on the downbeat displaces nothing.
        assert_eq!(meter.syncopation(0.0, 1.0), 0.0);
        // Neither does an offbeat eighth that ends before the next beat.
        assert_eq!(meter.syncopation(0.5, 0.5), 0.0);
    }

    /// Recorded MIDI never lands exactly on a beat. Without the tolerance every
    /// note of a live take would read as off-grid and meter would stop working
    /// on exactly the material it matters most for.
    #[test]
    fn a_note_a_few_milliseconds_off_still_lands_on_its_beat() {
        let meter = Meter::from_signature(4, 4);
        assert_eq!(meter.beat_strength(2.0 + 0.02), 0.75);
        assert_eq!(meter.beat_strength(2.0 - 0.02), 0.75);
        // But not one a full subdivision away.
        assert_eq!(meter.beat_strength(2.25), 0.125);
    }

    #[test]
    fn a_non_finite_position_reports_off_grid_rather_than_panicking() {
        let meter = Meter::from_signature(4, 4);
        assert_eq!(meter.beat_strength(f32::NAN), OFF_GRID_STRENGTH);
        assert_eq!(meter.syncopation(f32::NAN, 1.0), 0.0);
    }

    /// A whole-note pedal must not walk a thousand bars looking for something
    /// to displace.
    #[test]
    fn syncopation_of_a_very_long_note_terminates() {
        let meter = Meter::from_signature(4, 4);
        assert!(meter.syncopation(0.5, 10_000.0) > 0.0);
    }
}

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

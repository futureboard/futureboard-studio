//! Cutting a sample into slices, and editing the cut. Control side only: the
//! editor works out new slice points here and sends them as wire edits.
//!
//! Positions are fractions of the sample. `min_gap` is the shortest slice the
//! caller allows, as a fraction too, so it can be a fixed time whatever the
//! sample's length.

use crate::{BeatDivision, MAX_SLICES, SlicerParams};

/// `p` cut at `points`: put in order, closer ones than `min_gap` merged, at
/// most [`MAX_SLICES`] kept. Unused entries are reset, so two cuts that play
/// the same compare equal.
pub fn with_points(p: SlicerParams, points: &[f32], min_gap: f32) -> SlicerParams {
    let mut sorted: Vec<f32> = points
        .iter()
        .copied()
        .filter(|point| point.is_finite())
        .map(|point| point.clamp(0.0, 1.0))
        .collect();
    sorted.sort_unstable_by(f32::total_cmp);
    let mut kept: Vec<f32> = Vec::with_capacity(sorted.len().min(MAX_SLICES));
    for point in sorted {
        if kept.len() == MAX_SLICES {
            break;
        }
        if kept.last().is_none_or(|last| point - last >= min_gap) && point < 1.0 {
            kept.push(point);
        }
    }
    let mut next = p;
    next.slices = [1.0; MAX_SLICES];
    next.slices[..kept.len()].copy_from_slice(&kept);
    next.slice_count = kept.len() as u8;
    next
}

/// `count` slices of equal length.
pub fn equal_points(count: usize) -> Vec<f32> {
    let count = count.clamp(1, MAX_SLICES);
    (0..count).map(|i| i as f32 / count as f32).collect()
}

/// A slice every `division` at `bpm`, over a sample `seconds` long.
pub fn beat_points(seconds: f64, bpm: f32, division: BeatDivision) -> Vec<f32> {
    if !(seconds > 0.0 && bpm > 0.0) {
        return vec![0.0];
    }
    let step = 60.0 / (bpm as f64 * division.per_beat() as f64);
    (0..MAX_SLICES)
        .map(|i| i as f64 * step / seconds)
        // A step that lands a hair before the end is the end.
        .take_while(|fraction| *fraction < 1.0 - 1.0e-4)
        .map(|fraction| fraction as f32)
        .collect()
}

/// A slice from the start, then one at each detected hit (`(position,
/// strength)`). With more hits than slices, the strongest are kept; two
/// closer than `min_gap` keep the stronger.
pub fn transient_points(hits: &[(f32, f32)], min_gap: f32) -> Vec<f32> {
    let mut strongest: Vec<(f32, f32)> = hits
        .iter()
        .copied()
        .filter(|(position, strength)| position.is_finite() && strength.is_finite())
        .collect();
    strongest.sort_unstable_by(|a, b| b.1.total_cmp(&a.1));
    let mut kept = vec![0.0_f32];
    for (position, _) in strongest {
        if kept.len() == MAX_SLICES {
            break;
        }
        if kept.iter().all(|point| (point - position).abs() >= min_gap) {
            kept.push(position.clamp(0.0, 1.0));
        }
    }
    kept.sort_unstable_by(f32::total_cmp);
    kept
}

/// The slice playing at `fraction`: the last one starting at or before it.
pub fn slice_at(p: &SlicerParams, fraction: f32) -> Option<usize> {
    p.points().iter().rposition(|point| *point <= fraction)
}

/// The slice point nearest `fraction`, if one is within `tolerance`.
pub fn nearest_point(p: &SlicerParams, fraction: f32, tolerance: f32) -> Option<usize> {
    p.points()
        .iter()
        .enumerate()
        .map(|(index, point)| (index, (point - fraction).abs()))
        .filter(|(_, distance)| *distance <= tolerance)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(index, _)| index)
}

/// `p` with a new slice point at `fraction`, or `None` when every slice is in
/// use or `fraction` is within `min_gap` of a point already there.
pub fn insert_point(p: SlicerParams, fraction: f32, min_gap: f32) -> Option<SlicerParams> {
    let fraction = fraction.clamp(0.0, 1.0);
    let points = p.points();
    if points.len() >= MAX_SLICES
        || fraction >= 1.0
        || points
            .iter()
            .any(|point| (point - fraction).abs() < min_gap)
    {
        return None;
    }
    let mut next: Vec<f32> = points.to_vec();
    next.push(fraction);
    Some(with_points(p, &next, 0.0))
}

/// `p` without slice point `index`; the slice before it takes its audio. The
/// last slice point is never removed.
pub fn remove_point(p: SlicerParams, index: usize) -> Option<SlicerParams> {
    let points = p.points();
    if points.len() <= 1 || index >= points.len() {
        return None;
    }
    let mut next = points.to_vec();
    next.remove(index);
    Some(with_points(p, &next, 0.0))
}

/// `p` with slice point `index` moved towards `fraction`, kept `min_gap` clear
/// of its neighbours so slices keep their order and their keys.
pub fn move_point(p: SlicerParams, index: usize, fraction: f32, min_gap: f32) -> SlicerParams {
    let points = p.points();
    if index >= points.len() {
        return p;
    }
    let low = if index == 0 {
        0.0
    } else {
        points[index - 1] + min_gap
    };
    let high = points
        .get(index + 1)
        .map_or(1.0 - min_gap, |next| next - min_gap);
    let mut next = p;
    next.slices[index] = fraction.clamp(low.min(high), high.max(low));
    next
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cut(points: &[f32]) -> SlicerParams {
        with_points(SlicerParams::default(), points, 0.0)
    }

    #[test]
    fn a_cut_is_ordered_merged_and_capped() {
        let p = with_points(SlicerParams::default(), &[0.5, 0.0, 0.501, 1.0, 0.25], 0.01);
        assert_eq!(p.points(), &[0.0, 0.25, 0.5]);
        let many: Vec<f32> = (0..200).map(|i| i as f32 / 200.0).collect();
        assert_eq!(cut(&many).points().len(), MAX_SLICES);
        assert_eq!(cut(&[0.0, 0.5]), cut(&[0.5, 0.0]));
    }

    #[test]
    fn equal_and_beat_cuts_land_where_expected() {
        assert_eq!(equal_points(4), vec![0.0, 0.25, 0.5, 0.75]);
        // Two bars at 120 BPM are 4 s: eighths are 16 slices of 0.25 s.
        let eighths = beat_points(4.0, 120.0, BeatDivision::Eighth);
        assert_eq!(eighths.len(), 16);
        assert!((eighths[1] - 0.0625).abs() < 1.0e-6);
        assert_eq!(beat_points(4.0, 120.0, BeatDivision::Quarter).len(), 8);
        // Never more than there are keys for.
        assert_eq!(
            beat_points(60.0, 120.0, BeatDivision::Sixteenth).len(),
            MAX_SLICES
        );
        assert_eq!(beat_points(4.0, 0.0, BeatDivision::Eighth), vec![0.0]);
    }

    #[test]
    fn transient_cuts_start_at_zero_and_keep_the_strongest_hits() {
        let hits = [(0.001, 0.9), (0.3, 0.5), (0.302, 0.8), (0.7, 0.2)];
        assert_eq!(transient_points(&hits, 0.01), vec![0.0, 0.302, 0.7]);
        let many: Vec<(f32, f32)> = (1..100).map(|i| (i as f32 / 100.0, i as f32)).collect();
        let kept = transient_points(&many, 0.0);
        assert_eq!(kept.len(), MAX_SLICES);
        // The weakest (earliest) are the ones dropped.
        assert!((kept[1] - 0.37).abs() < 1.0e-6, "{kept:?}");
    }

    #[test]
    fn points_are_found_inserted_removed_and_moved() {
        let p = cut(&[0.0, 0.25, 0.5]);
        assert_eq!(slice_at(&p, 0.3), Some(1));
        assert_eq!(slice_at(&p, 0.9), Some(2));
        assert_eq!(nearest_point(&p, 0.26, 0.02), Some(1));
        assert_eq!(nearest_point(&p, 0.4, 0.02), None);

        let inserted = insert_point(p, 0.4, 0.01).unwrap();
        assert_eq!(inserted.points(), &[0.0, 0.25, 0.4, 0.5]);
        assert!(insert_point(p, 0.255, 0.01).is_none());

        assert_eq!(remove_point(p, 1).unwrap().points(), &[0.0, 0.5]);
        assert!(remove_point(cut(&[0.0]), 0).is_none());

        let moved = move_point(p, 1, 0.9, 0.01);
        assert!((moved.slices[1] - 0.49).abs() < 1.0e-6);
        assert_eq!(move_point(p, 0, -1.0, 0.01).slices[0], 0.0);
    }
}

//! Protocol-neutral, note-owned expression data.
//!
//! MIDI channels are deliberately absent from this model.  A protocol adapter
//! may use a channel while receiving or transmitting a note, but once the
//! event reaches Futureboard it is identified by [`NoteId`] instead.

use serde::{Deserialize, Serialize};

/// Stable identity of a musical note inside a clip.
pub type NoteId = u64;

/// Position in a note-local musical timeline.  The current timeline uses
/// beats; keeping the alias here leaves room for a fixed-point tick type when
/// the project timebase becomes tick-native.
pub type Tick = f32;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum ExpressionInterpolation {
    #[default]
    Linear,
    Step,
    Smooth,
    Bezier,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct ExpressionPoint {
    pub position: Tick,
    pub value: f32,
    #[serde(default)]
    pub interpolation: ExpressionInterpolation,
}

impl ExpressionPoint {
    pub fn new(position: Tick, value: f32) -> Self {
        Self {
            position,
            value,
            interpolation: ExpressionInterpolation::Linear,
        }
    }
}

/// A note-local automation curve.  `push_raw` intentionally does not merge or
/// simplify points; recording uses it until the take is stopped.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ExpressionCurve {
    #[serde(default)]
    pub points: Vec<ExpressionPoint>,
}

impl ExpressionCurve {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_points(mut points: Vec<ExpressionPoint>) -> Self {
        points.sort_by(|a, b| a.position.total_cmp(&b.position));
        Self { points }
    }

    /// Append a raw sample exactly as received from the performer.
    pub fn push_raw(&mut self, point: ExpressionPoint) {
        self.points.push(point);
    }

    /// Add a point for an editor gesture and keep the curve ordered.
    pub fn push(&mut self, point: ExpressionPoint) {
        self.points.push(point);
        self.points
            .sort_by(|a, b| a.position.total_cmp(&b.position));
    }

    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    /// Return a playback-safe copy of a curve. Raw recording points are
    /// intentionally permissive, while project files and realtime snapshots
    /// need finite, note-local positions and bounded controller values.
    pub fn sanitized(&self, min_value: f32, max_value: f32) -> Self {
        let fallback = min_value.clamp(min_value, max_value);
        Self::from_points(
            self.points
                .iter()
                .filter_map(|point| {
                    let position = point
                        .position
                        .is_finite()
                        .then_some(point.position.max(0.0))?;
                    let value = if point.value.is_finite() {
                        point.value.clamp(min_value, max_value)
                    } else {
                        fallback
                    };
                    Some(ExpressionPoint {
                        position,
                        value,
                        interpolation: point.interpolation,
                    })
                })
                .collect(),
        )
    }

    /// Evaluate a curve without allocating.  The caller should use
    /// [`ExpressionCursor`] for repeated forward playback lookups.
    pub fn value_at(&self, position: Tick) -> Option<f32> {
        let first = *self.points.first()?;
        if position <= first.position {
            return Some(first.value);
        }
        for pair in self.points.windows(2) {
            let [a, b] = pair else { unreachable!() };
            if position <= b.position {
                return Some(interpolate(*a, *b, position));
            }
        }
        self.points.last().map(|point| point.value)
    }

    pub fn simplify(&self, tolerance: f32) -> Self {
        simplify_expression_curve(self, tolerance)
    }

    /// Split a note-local curve at `position`, rebasing the right half to the
    /// new note start. A sampled boundary is inserted in both halves so a
    /// split cannot create an expression discontinuity.
    pub fn split_at(&self, position: Tick) -> (Self, Self) {
        if self.points.is_empty() {
            return (Self::default(), Self::default());
        }
        let position = position.max(0.0);
        let boundary = ExpressionPoint {
            position: 0.0,
            value: self.value_at(position).unwrap_or(0.0),
            interpolation: ExpressionInterpolation::Linear,
        };
        let mut left = self
            .points
            .iter()
            .copied()
            .filter(|point| point.position < position)
            .collect::<Vec<_>>();
        if left
            .last()
            .is_none_or(|point| (point.position - position).abs() > f32::EPSILON)
        {
            left.push(ExpressionPoint {
                position,
                value: boundary.value,
                interpolation: boundary.interpolation,
            });
        }
        let mut right = vec![boundary];
        right.extend(
            self.points
                .iter()
                .copied()
                .filter(|point| point.position > position)
                .map(|mut point| {
                    point.position -= position;
                    point
                }),
        );
        (Self::from_points(left), Self::from_points(right))
    }
}

/// Cursor for monotonic playback.  It avoids scanning from the first point on
/// every audio block and contains no heap allocation.
#[derive(Debug, Clone, Copy, Default)]
pub struct ExpressionCursor {
    next_segment: usize,
    last_position: Tick,
}

impl ExpressionCursor {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn value_at(&mut self, curve: &ExpressionCurve, position: Tick) -> Option<f32> {
        if position < self.last_position {
            self.next_segment = 0;
        }
        self.last_position = position;
        let first = *curve.points.first()?;
        if position <= first.position {
            return Some(first.value);
        }
        while self.next_segment + 1 < curve.points.len()
            && position > curve.points[self.next_segment + 1].position
        {
            self.next_segment += 1;
        }
        if self.next_segment + 1 < curve.points.len() {
            Some(interpolate(
                curve.points[self.next_segment],
                curve.points[self.next_segment + 1],
                position,
            ))
        } else {
            curve.points.last().map(|point| point.value)
        }
    }
}

fn interpolate(a: ExpressionPoint, b: ExpressionPoint, position: Tick) -> f32 {
    let span = b.position - a.position;
    if span <= f32::EPSILON {
        return b.value;
    }
    let t = ((position - a.position) / span).clamp(0.0, 1.0);
    match a.interpolation {
        ExpressionInterpolation::Linear => a.value + (b.value - a.value) * t,
        ExpressionInterpolation::Step => a.value,
        ExpressionInterpolation::Smooth | ExpressionInterpolation::Bezier => {
            let smooth_t = t * t * (3.0 - 2.0 * t);
            a.value + (b.value - a.value) * smooth_t
        }
    }
}

/// Ramer–Douglas–Peucker simplification in note-local position/value space.
/// Endpoints are always retained and interpolation metadata is retained from
/// the corresponding source points.
pub fn simplify_expression_curve(curve: &ExpressionCurve, tolerance: f32) -> ExpressionCurve {
    // Recording deliberately appends raw points without sorting. Normalize
    // their order at this boundary so the early-return path for one/two points
    // is safe too; otherwise a delayed MIDI event could make value_at/split_at
    // read the curve backwards forever after the take is committed.
    let ordered = ExpressionCurve::from_points(curve.points.clone());
    if ordered.points.len() <= 2 || tolerance <= 0.0 {
        return ordered;
    }
    let tolerance = tolerance.abs();
    let mut keep = vec![false; ordered.points.len()];
    keep[0] = true;
    let last = keep.len() - 1;
    keep[last] = true;
    simplify_range(
        &ordered.points,
        0,
        ordered.points.len() - 1,
        tolerance,
        &mut keep,
    );
    ExpressionCurve::from_points(
        ordered
            .points
            .iter()
            .zip(keep)
            .filter_map(|(point, keep)| keep.then_some(*point))
            .collect(),
    )
}

fn simplify_range(
    points: &[ExpressionPoint],
    first: usize,
    last: usize,
    tolerance: f32,
    keep: &mut [bool],
) {
    if last <= first + 1 {
        return;
    }
    let a = points[first];
    let b = points[last];
    let mut furthest = None;
    let mut max_distance = tolerance;
    for index in (first + 1)..last {
        let distance = perpendicular_distance(points[index], a, b);
        if distance > max_distance {
            max_distance = distance;
            furthest = Some(index);
        }
    }
    let Some(index) = furthest else { return };
    keep[index] = true;
    simplify_range(points, first, index, tolerance, keep);
    simplify_range(points, index, last, tolerance, keep);
}

fn perpendicular_distance(point: ExpressionPoint, a: ExpressionPoint, b: ExpressionPoint) -> f32 {
    let dx = b.position - a.position;
    let dy = b.value - a.value;
    let length = (dx * dx + dy * dy).sqrt();
    if length <= f32::EPSILON {
        return ((point.position - a.position).powi(2) + (point.value - a.value).powi(2)).sqrt();
    }
    (dy * (point.position - a.position) - dx * (point.value - a.value)).abs() / length
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CustomExpressionLane {
    /// Stable numeric identifier for realtime routing; the label is UI-only.
    pub controller: u16,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub curve: ExpressionCurve,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NoteExpressionLane {
    Pitch,
    Pressure,
    Timbre,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct NoteExpression {
    #[serde(default)]
    pub pitch: ExpressionCurve,
    #[serde(default)]
    pub pressure: ExpressionCurve,
    #[serde(default)]
    pub timbre: ExpressionCurve,
    #[serde(default)]
    pub release_velocity: Option<f32>,
    #[serde(default)]
    pub custom: Vec<CustomExpressionLane>,
}

impl NoteExpression {
    pub fn is_empty(&self) -> bool {
        self.pitch.is_empty()
            && self.pressure.is_empty()
            && self.timbre.is_empty()
            && self.release_velocity.is_none()
            && self.custom.iter().all(|lane| lane.curve.is_empty())
    }

    /// Sanitize all lanes at a trust boundary such as project decode or an
    /// engine snapshot. Pitch is bipolar; pressure, timbre and custom lanes
    /// are unipolar. Invalid release velocity means "unset".
    pub fn sanitized(&self) -> Self {
        Self {
            pitch: self.pitch.sanitized(-1.0, 1.0),
            pressure: self.pressure.sanitized(0.0, 1.0),
            timbre: self.timbre.sanitized(0.0, 1.0),
            release_velocity: self
                .release_velocity
                .filter(|value| value.is_finite())
                .map(|value| value.clamp(0.0, 1.0)),
            custom: self
                .custom
                .iter()
                .map(|lane| CustomExpressionLane {
                    controller: lane.controller,
                    name: lane.name.clone(),
                    curve: lane.curve.sanitized(0.0, 1.0),
                })
                .collect(),
        }
    }

    pub fn simplify(&self, tolerances: ExpressionSimplificationTolerances) -> Self {
        Self {
            pitch: self.pitch.simplify(tolerances.pitch),
            pressure: self.pressure.simplify(tolerances.pressure),
            timbre: self.timbre.simplify(tolerances.timbre),
            release_velocity: self.release_velocity,
            custom: self
                .custom
                .iter()
                .map(|lane| CustomExpressionLane {
                    controller: lane.controller,
                    name: lane.name.clone(),
                    curve: lane.curve.simplify(tolerances.custom),
                })
                .collect(),
        }
    }

    /// Split every note-local lane at `position`. Release velocity belongs to
    /// the final segment because it describes the original note's final key
    /// release, not the synthetic boundary between the two notes.
    pub fn split_at(&self, position: Tick) -> (Self, Self) {
        let (pitch_left, pitch_right) = self.pitch.split_at(position);
        let (pressure_left, pressure_right) = self.pressure.split_at(position);
        let (timbre_left, timbre_right) = self.timbre.split_at(position);
        let mut left = Self {
            pitch: pitch_left,
            pressure: pressure_left,
            timbre: timbre_left,
            release_velocity: None,
            custom: Vec::with_capacity(self.custom.len()),
        };
        let mut right = Self {
            pitch: pitch_right,
            pressure: pressure_right,
            timbre: timbre_right,
            release_velocity: self.release_velocity,
            custom: Vec::with_capacity(self.custom.len()),
        };
        for lane in &self.custom {
            let (left_curve, right_curve) = lane.curve.split_at(position);
            left.custom.push(CustomExpressionLane {
                controller: lane.controller,
                name: lane.name.clone(),
                curve: left_curve,
            });
            right.custom.push(CustomExpressionLane {
                controller: lane.controller,
                name: lane.name.clone(),
                curve: right_curve,
            });
        }
        (left, right)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct ExpressionSimplificationTolerances {
    /// Pitch tolerance in normalized curve units.  The default is calibrated
    /// for a ±48-semitone MPE zone, so it is below one cent and deliberately
    /// conservative for smaller ranges.
    pub pitch: f32,
    pub pressure: f32,
    pub timbre: f32,
    pub custom: f32,
}

impl Default for ExpressionSimplificationTolerances {
    fn default() -> Self {
        Self::for_pitch_range(48.0)
    }
}

impl ExpressionSimplificationTolerances {
    /// Build sensible defaults for an active pitch-bend range. Pitch is kept
    /// to roughly half a cent while pressure/timbre remain at half a percent.
    pub fn for_pitch_range(range_semitones: f32) -> Self {
        let range_semitones = range_semitones.max(0.01);
        Self {
            pitch: 0.5 / (range_semitones * 100.0),
            pressure: 0.005,
            timbre: 0.005,
            custom: 0.005,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct PitchExpressionConfig {
    /// Total positive distance from centre in semitones.  This is metadata,
    /// never baked into note expression points.
    pub range_semitones: f32,
}

impl PitchExpressionConfig {
    pub fn new(range_semitones: f32) -> Self {
        Self {
            range_semitones: range_semitones.max(0.01),
        }
    }

    pub fn normalized_to_semitones(self, value: f32) -> f32 {
        value.clamp(-1.0, 1.0) * self.range_semitones
    }

    pub fn normalized_to_cents(self, value: f32) -> f32 {
        self.normalized_to_semitones(value) * 100.0
    }

    pub fn semitones_to_normalized(self, value: f32) -> f32 {
        (value / self.range_semitones.max(0.01)).clamp(-1.0, 1.0)
    }
}

impl Default for PitchExpressionConfig {
    fn default() -> Self {
        Self::new(2.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curves_support_all_interpolation_modes() {
        for interpolation in [
            ExpressionInterpolation::Linear,
            ExpressionInterpolation::Step,
            ExpressionInterpolation::Smooth,
            ExpressionInterpolation::Bezier,
        ] {
            let curve = ExpressionCurve::from_points(vec![
                ExpressionPoint {
                    position: 0.0,
                    value: 0.0,
                    interpolation,
                },
                ExpressionPoint::new(1.0, 1.0),
            ]);
            assert!(curve.value_at(0.5).is_some());
        }
    }

    #[test]
    fn cursor_reuses_forward_segment_and_resets_on_seek() {
        let curve = ExpressionCurve::from_points(vec![
            ExpressionPoint::new(0.0, 0.0),
            ExpressionPoint::new(1.0, 1.0),
        ]);
        let mut cursor = ExpressionCursor::default();
        assert_eq!(cursor.value_at(&curve, 0.25), Some(0.25));
        assert_eq!(cursor.value_at(&curve, 0.75), Some(0.75));
        assert_eq!(cursor.value_at(&curve, 0.1), Some(0.1));
    }

    #[test]
    fn simplification_keeps_a_gesture_breakpoint() {
        let curve = ExpressionCurve::from_points(vec![
            ExpressionPoint::new(0.0, 0.0),
            ExpressionPoint::new(0.5, 1.0),
            ExpressionPoint::new(1.0, 0.0),
        ]);
        assert_eq!(curve.simplify(0.1).points.len(), 3);
        assert_eq!(curve.simplify(2.0).points.len(), 2);
    }

    #[test]
    fn simplifying_raw_points_restores_note_local_order() {
        let mut curve = ExpressionCurve::default();
        curve.push_raw(ExpressionPoint::new(1.0, 1.0));
        curve.push_raw(ExpressionPoint::new(0.0, 0.0));
        let ordered = curve.simplify(0.0);
        assert_eq!(ordered.points[0].position, 0.0);
        assert_eq!(ordered.points[1].position, 1.0);
    }

    #[test]
    fn sanitizing_expression_drops_invalid_positions_and_bounds_values() {
        let expression = NoteExpression {
            pitch: ExpressionCurve::from_points(vec![
                ExpressionPoint::new(-1.0, -2.0),
                ExpressionPoint::new(f32::NAN, 0.0),
                ExpressionPoint::new(1.0, 2.0),
            ]),
            pressure: ExpressionCurve::from_points(vec![ExpressionPoint::new(0.0, f32::NAN)]),
            release_velocity: Some(f32::INFINITY),
            custom: vec![CustomExpressionLane {
                controller: 74,
                name: "timbre".into(),
                curve: ExpressionCurve::from_points(vec![ExpressionPoint::new(0.0, 2.0)]),
            }],
            ..NoteExpression::default()
        };

        let safe = expression.sanitized();
        assert_eq!(safe.pitch.points.len(), 2);
        assert_eq!(safe.pitch.points[0].position, 0.0);
        assert_eq!(safe.pitch.points[0].value, -1.0);
        assert_eq!(safe.pitch.points[1].value, 1.0);
        assert_eq!(safe.pressure.points[0].value, 0.0);
        assert_eq!(safe.release_velocity, None);
        assert_eq!(safe.custom[0].curve.points[0].value, 1.0);
    }

    #[test]
    fn split_rebases_expression_and_preserves_the_boundary() {
        let curve = ExpressionCurve::from_points(vec![
            ExpressionPoint::new(0.0, 0.0),
            ExpressionPoint::new(2.0, 1.0),
        ]);
        let (left, right) = curve.split_at(1.0);
        assert_eq!(left.points.last().unwrap().position, 1.0);
        assert!((left.points.last().unwrap().value - 0.5).abs() < 0.001);
        assert_eq!(right.points[0].position, 0.0);
        assert!((right.points[0].value - 0.5).abs() < 0.001);
        assert_eq!(right.points[1].position, 1.0);
    }

    #[test]
    fn pitch_range_is_metadata_not_curve_data() {
        let curve_value = 0.5;
        assert_eq!(
            PitchExpressionConfig::new(48.0).normalized_to_semitones(curve_value),
            24.0
        );
        assert_eq!(
            PitchExpressionConfig::new(24.0).normalized_to_semitones(curve_value),
            12.0
        );
    }
}

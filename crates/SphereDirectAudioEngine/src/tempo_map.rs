//! Beat/time conversion for transport and scheduling.
//!
//! The map is a list of tempo markers. Each marker says what the tempo is at its
//! beat and how the tempo travels from there to the next marker: it holds, it
//! ramps linearly, or it eases in and out. Everything downstream — the playhead,
//! the metronome, MIDI event scheduling, clip start samples, the plugin process
//! context — converts through this one map, so a curve drawn in the Tempo lane
//! is a curve the transport actually plays.
//!
//! Curved segments are integrated in closed form rather than stepped: across a
//! linear tempo ramp the elapsed time is `60/k * ln(bpm(b)/bpm0)`, which is
//! exact and exactly invertible. That matters more than it sounds — a stepped
//! approximation drifts, and audio that starts a few milliseconds late after
//! every tempo ramp is precisely the "the waveform moved" failure the
//! arrangement is not allowed to have.

use serde::{Deserialize, Serialize};

/// Minimum/maximum project BPM (matches automation spec).
pub const BPM_MIN: f64 = 20.0;
pub const BPM_MAX: f64 = 999.0;

/// Below this BPM-per-beat slope a ramp is a hold. Far under the smallest tempo
/// change the UI can express over the longest usable span, so the cheap
/// constant-tempo path is taken for every project without curves and never
/// swallows one the user drew.
const RAMP_EPSILON: f64 = 1e-9;

/// Starting pieces of a bent (tensioned) `Linear` segment, before refinement.
const CURVED_STEPS: usize = 16;

/// Linear pieces a `Smooth` segment is built from.
///
/// Smoothstep has no closed-form time integral, so it is approximated by linear
/// ramps, each of which *is* integrated exactly. 16 pieces put the worst-case
/// timing error of a 60→180 BPM ease over four bars well under one sample at
/// 192 kHz, and the segments are built off the audio thread.
const SMOOTH_STEPS: usize = 16;

/// How the tempo travels from one marker to the next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TempoCurve {
    /// Constant until the next marker, then a step change.
    #[default]
    Hold,
    /// Straight line in BPM against beats.
    Linear,
    /// Eased in and out (smoothstep) between the two tempos.
    Smooth,
}

impl TempoCurve {
    pub fn to_tag(self) -> u8 {
        match self {
            TempoCurve::Hold => 0,
            TempoCurve::Linear => 1,
            TempoCurve::Smooth => 2,
        }
    }

    pub fn from_tag(tag: u8) -> Self {
        match tag {
            1 => TempoCurve::Linear,
            2 => TempoCurve::Smooth,
            _ => TempoCurve::Hold,
        }
    }

    /// Interpolation factor for a normalized position across the segment.
    ///
    /// `tension` bends a `Linear` ramp into the same power curve automation
    /// lanes use: `> 0` eases in (slow start, fast finish), `< 0` eases out,
    /// and `+k` / `-k` mirror each other. Hold and Smooth ignore it.
    #[inline]
    pub fn shape(self, tension: f64, t: f64) -> f64 {
        let t = t.clamp(0.0, 1.0);
        match self {
            TempoCurve::Hold => 0.0,
            TempoCurve::Linear => {
                let k = clamp_tension(tension);
                if k.abs() < TENSION_EPSILON {
                    t
                } else {
                    t.powf(2f64.powf(k * 2.5))
                }
            }
            TempoCurve::Smooth => t * t * (3.0 - 2.0 * t),
        }
    }
}

/// Below this a tension is a straight ramp.
const TENSION_EPSILON: f64 = 1e-4;

/// Tension range shared with automation lanes.
pub fn clamp_tension(tension: f64) -> f64 {
    if tension.is_finite() {
        tension.clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

/// A tempo change anchored at a musical beat.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TempoPoint {
    pub beat: f64,
    pub bpm: f64,
    /// How the tempo reaches the *next* marker. Defaulted so projects written
    /// before curves existed load as the step-hold maps they were.
    #[serde(default)]
    pub curve: TempoCurve,
    /// Bend of a `Linear` ramp to the next marker, `-1.0..=1.0`; see
    /// [`TempoCurve::shape`]. Defaulted so older maps load as straight ramps.
    #[serde(default)]
    pub tension: f64,
}

impl TempoPoint {
    pub fn new(beat: f64, bpm: f64, curve: TempoCurve) -> Self {
        Self {
            beat,
            bpm,
            curve,
            tension: 0.0,
        }
    }

    pub fn with_tension(mut self, tension: f64) -> Self {
        self.tension = clamp_tension(tension);
        self
    }

    /// A step-hold marker — the shape every marker had before curves.
    pub fn hold(beat: f64, bpm: f64) -> Self {
        Self::new(beat, bpm, TempoCurve::Hold)
    }
}

/// One piece of the map carrying a single linear tempo law, for O(log n)
/// allocation-free lookup from the audio thread.
///
/// `bpm` is the tempo at `start_beat` and `end_bpm` the tempo at `end_beat`;
/// they are equal on a hold. A `Smooth` marker contributes several of these.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TempoSegment {
    pub start_beat: f64,
    pub end_beat: f64,
    pub start_seconds: f64,
    pub bpm: f64,
    pub end_bpm: f64,
}

impl TempoSegment {
    /// BPM per beat across this segment; `0.0` on a hold or an open end.
    #[inline]
    fn slope(&self) -> f64 {
        let span = self.end_beat - self.start_beat;
        if !span.is_finite() || span <= 0.0 {
            return 0.0;
        }
        let slope = (self.end_bpm - self.bpm) / span;
        if slope.abs() < RAMP_EPSILON {
            0.0
        } else {
            slope
        }
    }

    #[inline]
    pub fn bpm_at(&self, beat: f64) -> f64 {
        let slope = self.slope();
        if slope == 0.0 {
            return self.bpm;
        }
        let beat = beat.clamp(self.start_beat, self.end_beat);
        (self.bpm + slope * (beat - self.start_beat)).clamp(BPM_MIN, BPM_MAX)
    }

    /// Elapsed seconds at `beat`, integrating this segment's tempo law.
    #[inline]
    pub fn seconds_at(&self, beat: f64) -> f64 {
        let delta_beats = (beat - self.start_beat).max(0.0);
        let start_bpm = self.bpm.max(BPM_MIN);
        let slope = self.slope();
        if slope == 0.0 {
            return self.start_seconds + delta_beats * 60.0 / start_bpm;
        }
        // ∫ 60/bpm(b) db with bpm linear in b. The ratio stays positive because
        // both endpoint tempos are clamped to at least BPM_MIN.
        let ratio = (1.0 + slope * delta_beats / start_bpm).max(f64::MIN_POSITIVE);
        self.start_seconds + (60.0 / slope) * ratio.ln()
    }

    /// Inverse of [`Self::seconds_at`] within this segment.
    #[inline]
    pub fn beat_at(&self, seconds: f64) -> f64 {
        let delta_seconds = (seconds - self.start_seconds).max(0.0);
        let start_bpm = self.bpm.max(BPM_MIN);
        let slope = self.slope();
        if slope == 0.0 {
            return self.start_beat + delta_seconds * start_bpm / 60.0;
        }
        self.start_beat + start_bpm * ((slope * delta_seconds / 60.0).exp() - 1.0) / slope
    }

    /// Elapsed seconds at `end_beat`; infinite for the final open segment.
    #[inline]
    fn end_seconds(&self) -> f64 {
        if self.end_beat.is_finite() {
            self.seconds_at(self.end_beat)
        } else {
            f64::INFINITY
        }
    }
}

/// Runtime-ready tempo map snapshot (immutable, built off the audio thread).
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeTempoMapSnapshot {
    pub segments: Vec<TempoSegment>,
    /// Bumped whenever segments are rebuilt so caches can invalidate.
    pub revision: u64,
}

impl Default for RuntimeTempoMapSnapshot {
    fn default() -> Self {
        Self::static_tempo(120.0)
    }
}

impl RuntimeTempoMapSnapshot {
    pub fn static_tempo(bpm: f64) -> Self {
        TempoMap::static_tempo(bpm).into_snapshot()
    }

    pub fn bpm_at_beat(&self, beat: f64) -> f64 {
        let beat = beat.max(0.0);
        segment_at_beat(&self.segments, beat).bpm_at(beat)
    }

    pub fn seconds_at_beat(&self, beat: f64) -> f64 {
        let beat = beat.max(0.0);
        segment_at_beat(&self.segments, beat).seconds_at(beat)
    }

    pub fn beat_at_seconds(&self, seconds: f64) -> f64 {
        beat_at_seconds_in_segments(&self.segments, seconds)
    }

    pub fn samples_at_beat(&self, beat: f64, sample_rate: f64) -> u64 {
        (self.seconds_at_beat(beat) * sample_rate.max(1.0))
            .round()
            .max(0.0) as u64
    }

    pub fn beat_at_samples(&self, samples: u64, sample_rate: f64) -> f64 {
        let seconds = samples as f64 / sample_rate.max(1.0);
        self.beat_at_seconds(seconds)
    }
}

/// Project tempo map: the markers, and the segments they resolve to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TempoMap {
    /// Fallback BPM when `points` is empty.
    pub default_bpm: f64,
    #[serde(default)]
    pub points: Vec<TempoPoint>,
    #[serde(skip)]
    segments: Vec<TempoSegment>,
    #[serde(skip)]
    revision: u64,
}

impl TempoMap {
    pub fn static_tempo(bpm: f64) -> Self {
        let mut map = Self {
            default_bpm: clamp_bpm(bpm),
            points: Vec::new(),
            segments: Vec::new(),
            revision: 0,
        };
        map.rebuild_segments();
        map
    }

    pub fn from_points(default_bpm: f64, mut points: Vec<TempoPoint>) -> Self {
        points.sort_by(|a, b| {
            a.beat
                .partial_cmp(&b.beat)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        points.dedup_by(|a, b| (a.beat - b.beat).abs() < 1e-9);
        for point in &mut points {
            point.bpm = clamp_bpm(point.bpm);
        }
        let mut map = Self {
            default_bpm: clamp_bpm(default_bpm),
            points,
            segments: Vec::new(),
            revision: 0,
        };
        map.rebuild_segments();
        map
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn into_snapshot(self) -> RuntimeTempoMapSnapshot {
        RuntimeTempoMapSnapshot {
            segments: self.segments,
            revision: self.revision,
        }
    }

    pub fn snapshot(&self) -> RuntimeTempoMapSnapshot {
        RuntimeTempoMapSnapshot {
            segments: self.segments.clone(),
            revision: self.revision,
        }
    }

    pub fn segments(&self) -> &[TempoSegment] {
        &self.segments
    }

    pub fn tempo_at_beat(&self, beat: f64) -> f64 {
        self.bpm_at_beat(beat)
    }

    pub fn bpm_at_beat(&self, beat: f64) -> f64 {
        let beat = beat.max(0.0);
        if self.segments.is_empty() {
            return self.default_bpm;
        }
        segment_at_beat(&self.segments, beat).bpm_at(beat)
    }

    pub fn seconds_at_beat(&self, beat: f64) -> f64 {
        let beat = beat.max(0.0);
        if self.segments.is_empty() {
            return beat * 60.0 / self.default_bpm.max(BPM_MIN);
        }
        segment_at_beat(&self.segments, beat).seconds_at(beat)
    }

    pub fn beat_at_seconds(&self, seconds: f64) -> f64 {
        beat_at_seconds_in_segments(&self.segments, seconds)
    }

    pub fn samples_at_beat(&self, beat: f64, sample_rate: f64) -> u64 {
        (self.seconds_at_beat(beat) * sample_rate.max(1.0))
            .round()
            .max(0.0) as u64
    }

    pub fn beat_at_samples(&self, samples: u64, sample_rate: f64) -> f64 {
        let seconds = samples as f64 / sample_rate.max(1.0);
        self.beat_at_seconds(seconds)
    }

    fn rebuild_segments(&mut self) {
        self.segments.clear();
        let mut points: Vec<TempoPoint> = Vec::new();
        if self.points.is_empty() {
            points.push(TempoPoint::hold(0.0, self.default_bpm));
        } else {
            if self.points[0].beat > 0.0 {
                points.push(TempoPoint::hold(0.0, self.default_bpm));
            }
            points.extend(self.points.iter().cloned());
        }

        let mut start_seconds = 0.0;
        for (i, point) in points.iter().enumerate() {
            let Some(next) = points.get(i + 1) else {
                // The last marker holds forever: there is nothing to ramp to.
                self.segments.push(TempoSegment {
                    start_beat: point.beat,
                    end_beat: f64::INFINITY,
                    start_seconds,
                    bpm: point.bpm,
                    end_bpm: point.bpm,
                });
                break;
            };
            let span = next.beat - point.beat;
            if span <= 0.0 {
                continue;
            }
            let bent = point.curve == TempoCurve::Linear
                && clamp_tension(point.tension).abs() >= TENSION_EPSILON;
            let bpm_at_t = |t: f64| {
                clamp_bpm(point.bpm + (next.bpm - point.bpm) * point.curve.shape(point.tension, t))
            };
            // Knots in `t` for the linear pieces this marker contributes.
            let mut knots: Vec<f64> = Vec::new();
            match point.curve {
                TempoCurve::Hold => knots.extend([0.0, 1.0]),
                TempoCurve::Linear if !bent => knots.extend([0.0, 1.0]),
                TempoCurve::Linear => {
                    // A bent ramp: start from pieces of equal tempo change
                    // (`t = u^(1/e)` makes `t^e` uniform in `u`), then split any
                    // piece whose straight chord plays measurably differently
                    // from the curve. The time error of the whole segment then
                    // stays below a sample whatever the tension.
                    let exponent = 2f64.powf(clamp_tension(point.tension) * 2.5);
                    knots.push(0.0);
                    for piece in 0..CURVED_STEPS {
                        let a = (piece as f64 / CURVED_STEPS as f64).powf(1.0 / exponent);
                        let b = ((piece + 1) as f64 / CURVED_STEPS as f64).powf(1.0 / exponent);
                        refine_curved_piece(&bpm_at_t, span, a, b, 0, &mut knots);
                    }
                }
                TempoCurve::Smooth => {
                    knots.extend((0..=SMOOTH_STEPS).map(|i| i as f64 / SMOOTH_STEPS as f64))
                }
            }
            for pair in knots.windows(2) {
                let (t0, t1) = (pair[0], pair[1]);
                if t1 <= t0 {
                    continue;
                }
                let segment = TempoSegment {
                    start_beat: point.beat + span * t0,
                    end_beat: point.beat + span * t1,
                    start_seconds,
                    bpm: bpm_at_t(t0),
                    end_bpm: bpm_at_t(t1),
                };
                start_seconds = segment.end_seconds();
                self.segments.push(segment);
            }
        }
        self.revision = self.revision.wrapping_add(1);
    }
}

/// Seconds one bent piece may drift from the curve it stands in for.
const CURVED_PIECE_TOLERANCE_SECONDS: f64 = 2e-8;
/// Halvings allowed per starting piece.
const CURVED_MAX_DEPTH: u32 = 12;

/// Append the end knots covering `(a, b]` of a bent ramp, halving a piece
/// while its linear chord's elapsed time differs from the curve's (Simpson's
/// rule on 60/bpm) by more than [`CURVED_PIECE_TOLERANCE_SECONDS`].
fn refine_curved_piece(
    bpm_at_t: &impl Fn(f64) -> f64,
    span_beats: f64,
    a: f64,
    b: f64,
    depth: u32,
    knots: &mut Vec<f64>,
) {
    let chord = TempoSegment {
        start_beat: 0.0,
        end_beat: span_beats * (b - a),
        start_seconds: 0.0,
        bpm: bpm_at_t(a),
        end_bpm: bpm_at_t(b),
    };
    let chord_seconds = chord.end_seconds();
    const N: usize = 8;
    let h = (b - a) / N as f64;
    let mut sum = 0.0;
    for i in 0..=N {
        let weight = if i == 0 || i == N {
            1.0
        } else if i % 2 == 1 {
            4.0
        } else {
            2.0
        };
        sum += weight * 60.0 / bpm_at_t(a + h * i as f64);
    }
    let curve_seconds = sum * h * span_beats / 3.0;
    if depth < CURVED_MAX_DEPTH
        && (chord_seconds - curve_seconds).abs() > CURVED_PIECE_TOLERANCE_SECONDS
    {
        let mid = (a + b) * 0.5;
        refine_curved_piece(bpm_at_t, span_beats, a, mid, depth + 1, knots);
        refine_curved_piece(bpm_at_t, span_beats, mid, b, depth + 1, knots);
    } else {
        knots.push(b);
    }
}

fn segment_at_beat(segments: &[TempoSegment], beat: f64) -> TempoSegment {
    if segments.is_empty() {
        return TempoSegment {
            start_beat: 0.0,
            end_beat: f64::INFINITY,
            start_seconds: 0.0,
            bpm: BPM_MIN,
            end_bpm: BPM_MIN,
        };
    }
    let idx = segments
        .partition_point(|seg| seg.start_beat <= beat)
        .saturating_sub(1);
    segments[idx.min(segments.len() - 1)]
}

fn beat_at_seconds_in_segments(segments: &[TempoSegment], seconds: f64) -> f64 {
    let seconds = seconds.max(0.0);
    if segments.is_empty() {
        return 0.0;
    }
    if seconds <= segments[0].start_seconds {
        return 0.0;
    }
    let idx = segments
        .partition_point(|seg| seg.start_seconds <= seconds)
        .saturating_sub(1);
    segments[idx.min(segments.len() - 1)].beat_at(seconds)
}

fn clamp_bpm(bpm: f64) -> f64 {
    bpm.clamp(BPM_MIN, BPM_MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_tempo_conversions() {
        let map = TempoMap::static_tempo(120.0);
        assert!((map.tempo_at_beat(0.0) - 120.0).abs() < 1e-9);
        assert!((map.seconds_at_beat(2.0) - 1.0).abs() < 1e-9);
        assert!((map.beat_at_seconds(1.0) - 2.0).abs() < 1e-9);
        assert!((map.beat_at_seconds(0.0) - 0.0).abs() < 1e-9);
    }

    #[test]
    fn step_tempo_point_changes_bpm() {
        let map = TempoMap::from_points(
            120.0,
            vec![TempoPoint::hold(4.0, 60.0), TempoPoint::hold(8.0, 240.0)],
        );
        assert!((map.tempo_at_beat(0.0) - 120.0).abs() < 1e-9);
        assert!((map.tempo_at_beat(4.0) - 60.0).abs() < 1e-9);
        assert!((map.tempo_at_beat(7.9) - 60.0).abs() < 1e-9);
        assert!((map.tempo_at_beat(8.0) - 240.0).abs() < 1e-9);

        // 4 beats @ 120 BPM = 2s, then 4 beats @ 60 BPM = 4s → beat 8 at 6s.
        assert!((map.seconds_at_beat(8.0) - 6.0).abs() < 1e-6);
        assert!((map.beat_at_seconds(6.0) - 8.0).abs() < 1e-6);
        assert!((map.beat_at_seconds(1.0) - 2.0).abs() < 1e-6);
    }

    fn map_120_160() -> TempoMap {
        TempoMap::from_points(120.0, vec![TempoPoint::hold(4.0, 160.0)])
    }

    #[test]
    fn step_tempo_seconds_and_beats() {
        let map = map_120_160();
        assert!((map.seconds_at_beat(0.0) - 0.0).abs() < 1e-9);
        assert!((map.seconds_at_beat(4.0) - 2.0).abs() < 1e-9);
        assert!((map.seconds_at_beat(8.0) - 3.5).abs() < 1e-9);
        assert!((map.beat_at_seconds(2.0) - 4.0).abs() < 1e-9);
        assert!((map.beat_at_seconds(3.5) - 8.0).abs() < 1e-9);
    }

    #[test]
    fn step_tempo_sample_conversions() {
        let map = map_120_160();
        let sr = 48_000.0;
        assert_eq!(map.samples_at_beat(4.0, sr), 96_000);
        assert_eq!(map.samples_at_beat(8.0, sr), 168_000);
        assert!((map.beat_at_samples(96_000, sr) - 4.0).abs() < 1e-6);
        assert!((map.beat_at_samples(168_000, sr) - 8.0).abs() < 1e-6);
    }

    #[test]
    fn runtime_snapshot_matches_tempo_map() {
        let map = map_120_160();
        let snap = map.snapshot();
        assert_eq!(snap.samples_at_beat(4.0, 48_000.0), 96_000);
        assert_eq!(snap.samples_at_beat(8.0, 48_000.0), 168_000);
    }

    #[test]
    fn bpm_math_roundtrips_across_supported_sample_rates() {
        let bpm = 128.0;
        let map = TempoMap::static_tempo(bpm);
        for sr in [44_100.0, 48_000.0, 88_200.0, 96_000.0, 192_000.0] {
            assert!((map.tempo_at_beat(0.0) - bpm).abs() < 1e-9);
            let samples_per_beat = sr * 60.0 / bpm;
            assert_eq!(
                map.samples_at_beat(1.0, sr),
                samples_per_beat.round() as u64
            );
            let half_sample_ppq = bpm / (sr * 60.0) * 0.5;
            assert!(
                (map.beat_at_samples(samples_per_beat.round() as u64, sr) - 1.0).abs()
                    <= half_sample_ppq + 1e-12,
                "sr={sr}"
            );

            let ppq = 17.25;
            let sample = map.samples_at_beat(ppq, sr);
            let roundtrip = map.beat_at_samples(sample, sr);
            assert!(
                (roundtrip - ppq).abs() <= half_sample_ppq + 1e-12,
                "sr={sr} sample={sample} roundtrip={roundtrip}"
            );
        }

        assert_eq!(map.samples_at_beat(1.0, 48_000.0), 22_500);
        assert_eq!(map.samples_at_beat(1.0, 96_000.0), 45_000);
    }

    #[test]
    fn metronome_click_samples_follow_tempo_map() {
        let snap = map_120_160().snapshot();
        let sr = 48_000.0;
        let expected = [
            (0.0, 0_u64),
            (1.0, 24_000),
            (2.0, 48_000),
            (3.0, 72_000),
            (4.0, 96_000),
            (5.0, 114_000),
            (6.0, 132_000),
            (7.0, 150_000),
            (8.0, 168_000),
        ];
        for (beat, samples) in expected {
            assert_eq!(snap.samples_at_beat(beat, sr), samples, "beat {beat}");
        }
    }

    #[test]
    fn segments_are_sorted_and_cover_origin() {
        let map = TempoMap::from_points(100.0, vec![TempoPoint::hold(2.0, 200.0)]);
        let segs = map.segments();
        assert_eq!(segs.len(), 2);
        assert!((segs[0].start_beat - 0.0).abs() < 1e-9);
        assert!((segs[0].bpm - 100.0).abs() < 1e-9);
        assert!((segs[1].start_beat - 2.0).abs() < 1e-9);
        assert!((segs[1].bpm - 200.0).abs() < 1e-9);
    }

    // ── Curves ────────────────────────────────────────────────────────────────

    fn linear_ramp() -> TempoMap {
        TempoMap::from_points(
            120.0,
            vec![
                TempoPoint::new(0.0, 60.0, TempoCurve::Linear),
                TempoPoint::hold(8.0, 120.0),
            ],
        )
    }

    /// A bent ramp must play what it draws: elapsed time is the integral of
    /// 60/bpm over the curve, and seconds → beat inverts it.
    #[test]
    fn a_bent_ramp_integrates_its_curve_and_inverts() {
        for tension in [-0.8, -0.3, 0.3, 0.8] {
            let map = TempoMap::from_points(
                60.0,
                vec![
                    TempoPoint::new(0.0, 60.0, TempoCurve::Linear).with_tension(tension),
                    TempoPoint::new(16.0, 180.0, TempoCurve::Hold),
                ],
            );
            // Reference: fine numeric integration of the same curve.
            let steps = 200_000;
            let mut expected = 0.0;
            for i in 0..steps {
                let t = (i as f64 + 0.5) / steps as f64;
                let bpm = 60.0 + 120.0 * TempoCurve::Linear.shape(tension, t);
                expected += 16.0 / steps as f64 * 60.0 / bpm;
            }
            // Refinement stays bounded (hundreds of pieces, a few kB).
            assert!(
                map.segments().len() < 2_000,
                "k={tension}: {}",
                map.segments().len()
            );
            let got = map.seconds_at_beat(16.0);
            // Under one sample at 48 kHz across the whole ramp.
            assert!(
                (got - expected).abs() < 1.0 / 48_000.0,
                "k={tension}: {got} vs {expected}"
            );
            // Tension bends the right way: easing in lags a straight ramp.
            let mid = map.bpm_at_beat(8.0);
            if tension > 0.0 {
                assert!(mid < 120.0, "k={tension}: {mid}");
            } else {
                assert!(mid > 120.0, "k={tension}: {mid}");
            }
            for beat in [0.5, 3.0, 8.0, 12.5, 15.9] {
                let back = map.beat_at_seconds(map.seconds_at_beat(beat));
                assert!(
                    (back - beat).abs() < 1e-9,
                    "k={tension} beat={beat}: {back}"
                );
            }
        }
    }

    #[test]
    fn zero_tension_keeps_the_exact_single_piece_ramp() {
        let straight = TempoMap::from_points(
            60.0,
            vec![
                TempoPoint::new(0.0, 60.0, TempoCurve::Linear),
                TempoPoint::new(16.0, 180.0, TempoCurve::Hold),
            ],
        );
        assert_eq!(straight.segments().len(), 2);
    }

    #[test]
    fn linear_curve_interpolates_bpm_between_markers() {
        let map = linear_ramp();
        assert!((map.bpm_at_beat(0.0) - 60.0).abs() < 1e-9);
        assert!((map.bpm_at_beat(4.0) - 90.0).abs() < 1e-9);
        assert!((map.bpm_at_beat(8.0) - 120.0).abs() < 1e-9);
        // Past the last marker the tempo holds.
        assert!((map.bpm_at_beat(64.0) - 120.0).abs() < 1e-9);
    }

    #[test]
    fn linear_curve_time_is_the_closed_form_integral() {
        let map = linear_ramp();
        // ∫₀⁸ 60 / (60 + 7.5·b) db = (60/7.5)·ln(120/60) = 8·ln 2.
        let expected = 8.0 * 2.0_f64.ln();
        assert!(
            (map.seconds_at_beat(8.0) - expected).abs() < 1e-9,
            "{} vs {expected}",
            map.seconds_at_beat(8.0)
        );
        // A hold at either endpoint tempo would bracket it, never match it.
        assert!(map.seconds_at_beat(8.0) < 8.0 * 60.0 / 60.0);
        assert!(map.seconds_at_beat(8.0) > 8.0 * 60.0 / 120.0);
    }

    #[test]
    fn every_curve_roundtrips_beats_through_seconds() {
        for curve in [TempoCurve::Hold, TempoCurve::Linear, TempoCurve::Smooth] {
            let map = TempoMap::from_points(
                120.0,
                vec![
                    TempoPoint::new(0.0, 72.0, curve),
                    TempoPoint::new(16.0, 180.0, curve),
                    TempoPoint::hold(24.0, 96.0),
                ],
            );
            let mut beat = 0.0;
            while beat <= 32.0 {
                let seconds = map.seconds_at_beat(beat);
                let back = map.beat_at_seconds(seconds);
                assert!(
                    (back - beat).abs() < 1e-6,
                    "curve={curve:?} beat={beat} -> {seconds}s -> {back}"
                );
                beat += 0.125;
            }
        }
    }

    #[test]
    fn curved_time_is_strictly_monotonic() {
        let map = TempoMap::from_points(
            120.0,
            vec![
                TempoPoint::new(0.0, 200.0, TempoCurve::Smooth),
                TempoPoint::new(12.0, 40.0, TempoCurve::Linear),
                TempoPoint::hold(20.0, 150.0),
            ],
        );
        let mut previous = -1.0;
        let mut beat = 0.0;
        while beat <= 40.0 {
            let seconds = map.seconds_at_beat(beat);
            assert!(seconds > previous, "beat {beat} went backwards");
            previous = seconds;
            beat += 0.05;
        }
    }

    #[test]
    fn smooth_curve_eases_and_lands_on_both_endpoints() {
        let map = TempoMap::from_points(
            120.0,
            vec![
                TempoPoint::new(0.0, 60.0, TempoCurve::Smooth),
                TempoPoint::hold(8.0, 120.0),
            ],
        );
        assert!((map.bpm_at_beat(0.0) - 60.0).abs() < 1e-9);
        assert!((map.bpm_at_beat(8.0) - 120.0).abs() < 1e-9);
        assert!((map.bpm_at_beat(4.0) - 90.0).abs() < 1.0);
        // Eased: the first eighth moves less than a straight line would.
        let linear_at_1 = 60.0 + (120.0 - 60.0) / 8.0;
        assert!(map.bpm_at_beat(1.0) < linear_at_1);
    }

    #[test]
    fn a_hold_marker_is_unchanged_by_the_curve_support() {
        let holds = TempoMap::from_points(
            120.0,
            vec![TempoPoint::hold(4.0, 60.0), TempoPoint::hold(8.0, 240.0)],
        );
        for beat in [0.0, 1.5, 3.999, 4.0, 6.0, 8.0, 12.0] {
            let expected = if beat < 4.0 {
                beat * 60.0 / 120.0
            } else if beat < 8.0 {
                2.0 + (beat - 4.0) * 60.0 / 60.0
            } else {
                6.0 + (beat - 8.0) * 60.0 / 240.0
            };
            assert!(
                (holds.seconds_at_beat(beat) - expected).abs() < 1e-9,
                "beat {beat}"
            );
        }
    }

    #[test]
    fn curve_tags_round_trip() {
        for curve in [TempoCurve::Hold, TempoCurve::Linear, TempoCurve::Smooth] {
            assert_eq!(TempoCurve::from_tag(curve.to_tag()), curve);
        }
        assert_eq!(TempoCurve::from_tag(200), TempoCurve::Hold);
    }
}

//! Warp markers and segments, and tempo-mapped warping.

use super::*;

#[derive(Debug, Clone)]
pub struct RuntimeWarpMarker {
    pub id: u64,
    pub source_sample: u64,
    pub timeline_beat: f64,
    pub locked: bool,
}

/// One monotonic warp segment in the output-sample domain.
#[derive(Debug, Clone, Copy)]
pub struct RuntimeWarpSegment {
    pub out_start: u64,
    pub out_end: u64,
    pub src_start: f64,
    pub src_end: f64,
}

pub(crate) fn build_warp_segments(
    markers: &[RuntimeWarpMarker],
    clip_start_beat: f64,
    duration_beats: f64,
    duration_samples: u64,
    source_start: u64,
    source_end: u64,
) -> Vec<RuntimeWarpSegment> {
    if markers.is_empty() || duration_samples == 0 || duration_beats <= 0.0 {
        return Vec::new();
    }
    let source_start_f = source_start as f64;
    let source_end_f = source_end.max(source_start + 1) as f64;
    let beat_to_out = |beat: f64| -> u64 {
        let t = ((beat - clip_start_beat) / duration_beats).clamp(0.0, 1.0);
        (t * duration_samples as f64).round() as u64
    };

    let mut points: Vec<(u64, f64)> = Vec::with_capacity(markers.len() + 2);
    points.push((0, source_start_f));
    for marker in markers {
        let out = beat_to_out(marker.timeline_beat);
        let src = (marker.source_sample as f64).clamp(source_start_f, source_end_f);
        if points
            .last()
            .is_some_and(|(prev_out, prev_src)| out > *prev_out && src > *prev_src)
        {
            points.push((out, src));
        }
    }
    let end = (duration_samples, source_end_f);
    if points
        .last()
        .is_some_and(|(prev_out, prev_src)| end.0 > *prev_out && end.1 > *prev_src)
    {
        points.push(end);
    }

    let mut segments = Vec::new();
    for pair in points.windows(2) {
        let (out_a, src_a) = pair[0];
        let (out_b, src_b) = pair[1];
        if out_b <= out_a || src_b <= src_a {
            continue;
        }
        segments.push(RuntimeWarpSegment {
            out_start: out_a,
            out_end: out_b,
            src_start: src_a,
            src_end: src_b,
        });
    }
    segments
}

/// Whether `map` is one flat tempo equal to `bpm` — the case the base-tempo
/// clip arithmetic is exact for.
pub(crate) fn tempo_map_is_flat_at(map: &RuntimeTempoMapSnapshot, bpm: f64) -> bool {
    map.segments
        .iter()
        .all(|segment| (segment.bpm - bpm).abs() < 1.0e-9 && (segment.end_bpm - bpm).abs() < 1.0e-9)
}

/// The (clip-relative beat, source frame) pairs a tempo-locked clip is pinned
/// by: its start and end, and for Warp every marker in between. `None` when
/// the clip has nothing to lock to (Tempo Sync without a source tempo).
pub(super) fn tempo_locked_anchors(
    stretch: &StretchParams,
    markers: &[RuntimeWarpMarker],
    clip_start_beat: f64,
    duration_beats: f64,
    source_start: u64,
    source_end: u64,
    source_sample_rate: u32,
) -> Option<Vec<(f64, f64)>> {
    if duration_beats <= 0.0 || source_end <= source_start {
        return None;
    }
    let start = source_start as f64;
    let end = source_end as f64;
    let mut anchors = vec![(0.0, start)];
    match stretch.mode {
        StretchMode::TempoSync => {
            let source_bpm = stretch
                .source_bpm
                .filter(|bpm| bpm.is_finite() && *bpm > 0.0)? as f64;
            // The source plays its own beats one for one against the project's.
            let frames_per_beat = 60.0 / source_bpm * source_sample_rate.max(1) as f64;
            anchors.push((
                duration_beats,
                (start + duration_beats * frames_per_beat).min(end),
            ));
        }
        StretchMode::Warp => {
            for marker in markers {
                let beat = marker.timeline_beat - clip_start_beat;
                let frame = (marker.source_sample as f64).clamp(start, end);
                let (last_beat, last_frame) = *anchors.last().expect("start anchor");
                if beat > last_beat && beat < duration_beats && frame > last_frame {
                    anchors.push((beat, frame));
                }
            }
            let (_, last_frame) = *anchors.last().expect("start anchor");
            if end > last_frame {
                anchors.push((duration_beats, end));
            }
        }
        _ => return None,
    }
    (anchors.len() >= 2).then_some(anchors)
}

/// Beats between extra break points inside a tempo ramp, where the output is
/// not linear in beats. A sixteenth at 4/4 keeps the straight-line error well
/// under a millisecond for any ramp a project holds.
pub(super) const TEMPO_RAMP_STEP_BEATS: f64 = 0.25;

/// Warp segments for a tempo-locked clip under a changing tempo: each anchor
/// and each tempo change inside the clip becomes a break point, the source
/// position between anchors is linear in beats, and the output position comes
/// from the tempo map. Returns the segments and the clip's output length.
pub(super) fn tempo_mapped_warp_segments(
    map: &RuntimeTempoMapSnapshot,
    clip_start_beat: f64,
    duration_beats: f64,
    start_sample: u64,
    output_sample_rate: u32,
    anchors: &[(f64, f64)],
) -> Option<(Vec<RuntimeWarpSegment>, u64)> {
    let sr = output_sample_rate.max(1) as f64;
    let end_sample = map.samples_at_beat(clip_start_beat + duration_beats, sr);
    let duration_samples = end_sample.checked_sub(start_sample).filter(|d| *d > 0)?;

    let mut beats: Vec<f64> = anchors.iter().map(|(beat, _)| *beat).collect();
    for segment in &map.segments {
        for boundary in [segment.start_beat, segment.end_beat] {
            let rel = boundary - clip_start_beat;
            if rel.is_finite() && rel > 0.0 && rel < duration_beats {
                beats.push(rel);
            }
        }
        if (segment.end_bpm - segment.bpm).abs() > 1.0e-9 {
            let from = segment.start_beat.max(clip_start_beat);
            let to = segment.end_beat.min(clip_start_beat + duration_beats);
            let mut beat = from + TEMPO_RAMP_STEP_BEATS;
            while beat < to {
                beats.push(beat - clip_start_beat);
                beat += TEMPO_RAMP_STEP_BEATS;
            }
        }
    }
    beats.sort_by(f64::total_cmp);
    beats.dedup_by(|a, b| (*a - *b).abs() < 1.0e-9);

    let source_at = |beat: f64| -> f64 {
        let next = anchors
            .partition_point(|(b, _)| *b <= beat)
            .min(anchors.len() - 1);
        let (b1, f1) = anchors[next];
        let (b0, f0) = anchors[next.saturating_sub(1)];
        if b1 <= b0 {
            return f1;
        }
        f0 + (f1 - f0) * ((beat - b0) / (b1 - b0)).clamp(0.0, 1.0)
    };
    let output_at = |beat: f64| -> u64 {
        map.samples_at_beat(clip_start_beat + beat, sr)
            .saturating_sub(start_sample)
            .min(duration_samples)
    };

    let mut segments = Vec::with_capacity(beats.len());
    let mut previous = (output_at(beats[0]), source_at(beats[0]));
    for &beat in &beats[1..] {
        let next = (output_at(beat), source_at(beat));
        if next.0 > previous.0 && next.1 > previous.1 {
            segments.push(RuntimeWarpSegment {
                out_start: previous.0,
                out_end: next.0,
                src_start: previous.1,
                src_end: next.1,
            });
            previous = next;
        }
    }
    (!segments.is_empty()).then_some((segments, duration_samples))
}

/// Smallest output-per-input time ratio across a warp map, with the source
/// measured at the output rate (the rate the stretcher is fed at). `None` for
/// an unwarped clip. Control-thread only: sizes the stretcher's scratch.
pub(crate) fn warp_min_time_ratio(
    segments: &[RuntimeWarpSegment],
    source_sample_rate: f64,
    output_sample_rate: f64,
) -> Option<f32> {
    let rate = output_sample_rate / source_sample_rate.max(1.0);
    segments
        .iter()
        .filter_map(|seg| {
            let out = seg.out_end.saturating_sub(seg.out_start) as f64;
            let input = (seg.src_end - seg.src_start) * rate;
            (out > 0.0 && input > 0.0).then(|| (out / input) as f32)
        })
        .reduce(f32::min)
        .map(|ratio| ratio.clamp(0.05, 20.0))
}

/// Map a clip-relative output sample to a source frame through warp segments.
/// `None` means the caller should use the clip's global read rate.
#[inline]
pub(crate) fn map_warp_source_frame(
    rel: u64,
    duration_samples: u64,
    reverse: bool,
    segments: &[RuntimeWarpSegment],
) -> Option<f64> {
    if segments.is_empty() {
        return None;
    }
    let out = if reverse {
        duration_samples.saturating_sub(1).saturating_sub(rel)
    } else {
        rel
    };
    let mut lo = 0usize;
    let mut hi = segments.len();
    while lo < hi {
        let mid = (lo + hi) / 2;
        if segments[mid].out_end <= out {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    let seg = segments.get(lo).or_else(|| segments.last())?;
    let span = seg.out_end.saturating_sub(seg.out_start).max(1) as f64;
    let t = (out.saturating_sub(seg.out_start) as f64 / span).clamp(0.0, 1.0);
    Some(seg.src_start + (seg.src_end - seg.src_start) * t)
}

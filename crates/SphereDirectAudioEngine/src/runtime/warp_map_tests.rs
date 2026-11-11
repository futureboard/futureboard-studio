
use super::*;

#[test]
fn warp_capacity_ratio_is_the_steepest_segment() {
    // Output 0..2000 covers source 0..1000 (ratio 2.0), then output
    // 2000..3000 covers source 1000..3000 (ratio 0.5): the stretcher must be
    // sized for the 0.5 segment, which consumes four times as much input.
    let segments = [
        RuntimeWarpSegment {
            out_start: 0,
            out_end: 2_000,
            src_start: 0.0,
            src_end: 1_000.0,
        },
        RuntimeWarpSegment {
            out_start: 2_000,
            out_end: 3_000,
            src_start: 1_000.0,
            src_end: 3_000.0,
        },
    ];
    let ratio = warp_min_time_ratio(&segments, 48_000.0, 48_000.0).unwrap();
    assert!((ratio - 0.5).abs() < 1e-6, "ratio={ratio}");
    // A 24 kHz source fed at 48 kHz doubles every input span.
    let ratio = warp_min_time_ratio(&segments, 24_000.0, 48_000.0).unwrap();
    assert!((ratio - 0.25).abs() < 1e-6, "ratio={ratio}");
    assert!(warp_min_time_ratio(&[], 48_000.0, 48_000.0).is_none());
}

#[test]
fn empty_markers_do_not_build_segments() {
    assert!(build_warp_segments(&[], 0.0, 4.0, 48_000, 0, 48_000).is_empty());
}

#[test]
fn midpoint_interpolates_source_between_markers() {
    let markers = [
        RuntimeWarpMarker {
            id: 1,
            source_sample: 0,
            timeline_beat: 0.0,
            locked: false,
        },
        RuntimeWarpMarker {
            id: 2,
            source_sample: 1_000,
            timeline_beat: 2.0,
            locked: false,
        },
    ];
    let segments = build_warp_segments(&markers, 0.0, 4.0, 4_000, 0, 2_000);
    assert!(!segments.is_empty());
    let mid = map_warp_source_frame(1_000, 4_000, false, &segments).unwrap();
    assert!((mid - 500.0).abs() < 1.5, "mid={mid}");
    assert!(map_warp_source_frame(0, 4_000, false, &[]).is_none());
}

#[test]
fn zero_length_and_crossing_segments_are_dropped() {
    let markers = [
        RuntimeWarpMarker {
            id: 1,
            source_sample: 800,
            timeline_beat: 1.0,
            locked: false,
        },
        RuntimeWarpMarker {
            id: 2,
            source_sample: 200,
            timeline_beat: 2.0,
            locked: false,
        },
    ];
    let segments = build_warp_segments(&markers, 0.0, 4.0, 4_000, 0, 1_000);
    for pair in segments.windows(2) {
        assert!(pair[1].out_start >= pair[0].out_end);
        assert!(pair[1].src_start >= pair[0].src_end);
    }
    for segment in &segments {
        assert!(segment.out_end > segment.out_start);
        assert!(segment.src_end > segment.src_start);
    }
}

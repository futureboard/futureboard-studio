use super::*;
use crate::input_ring::WaveformPeak;

fn engine_with_two_tracks_active() -> EngineInner {
    let engine = EngineInner::new();
    engine
        .shared
        .recording_preview_active
        .store(true, Ordering::Relaxed);
    engine
        .shared
        .recording_preview_id
        .store(7, Ordering::Relaxed);
    engine
        .shared
        .recording_preview_start_sample
        .store(1_000, Ordering::Relaxed);
    engine
        .shared
        .recording_preview_sample_rate
        .store(48_000, Ordering::Relaxed);
    engine
        .shared
        .recording_preview_peaks_per_sec
        .store(150, Ordering::Relaxed);
    *engine.shared.recording_preview_track_ids.lock() =
        vec!["track-a".to_string(), "track-b".to_string()];
    engine.shared.preview_rings[0].push(WaveformPeak {
        min: -0.5,
        max: 0.5,
        rms: 0.25,
    });
    engine.shared.preview_rings[1].push(WaveformPeak {
        min: -0.1,
        max: 0.1,
        rms: 0.05,
    });
    engine.shared.preview_rings[1].push(WaveformPeak {
        min: -0.2,
        max: 0.2,
        rms: 0.1,
    });
    engine
}

#[test]
fn recording_preview_tracks_reports_every_armed_track_independently() {
    let engine = engine_with_two_tracks_active();
    let tracks = engine.recording_preview_tracks();
    assert_eq!(tracks.len(), 2);
    assert_eq!(tracks[0].track_id, "track-a");
    assert_eq!(tracks[0].peak_count, 1.0);
    assert_eq!(tracks[1].track_id, "track-b");
    assert_eq!(tracks[1].peak_count, 2.0);
    // Shared take-level metadata is identical across tracks.
    for t in &tracks {
        assert_eq!(t.recording_id, 7.0);
        assert_eq!(t.start_sample, 1_000.0);
        assert_eq!(t.sample_rate, 48_000);
    }
}

#[test]
fn recording_preview_tracks_is_empty_when_take_inactive() {
    let engine = engine_with_two_tracks_active();
    engine
        .shared
        .recording_preview_active
        .store(false, Ordering::Relaxed);
    assert!(engine.recording_preview_tracks().is_empty());
}

#[test]
fn drain_recording_preview_peaks_for_track_is_independent_per_track() {
    let engine = engine_with_two_tracks_active();
    let a = engine.drain_recording_preview_peaks_for_track("track-a", 0.0);
    let b = engine.drain_recording_preview_peaks_for_track("track-b", 0.0);
    assert_eq!(a.len(), 1);
    assert_eq!(a[0].rms, 0.25);
    assert_eq!(b.len(), 2);
    // Compare against the same f32->f64 widening `drain_preview_ring`
    // applies, since 0.1f32 doesn't round-trip to the f64 literal 0.1.
    assert_eq!(b[1].rms, 0.1f32 as f64);
    // Draining one track's ring must not disturb the other's head index.
    assert_eq!(
        engine
            .drain_recording_preview_peaks_for_track("track-a", 1.0)
            .len(),
        0
    );
}

#[test]
fn drain_recording_preview_peaks_for_track_returns_empty_for_unknown_track() {
    let engine = engine_with_two_tracks_active();
    assert!(engine
        .drain_recording_preview_peaks_for_track("no-such-track", 0.0)
        .is_empty());
}

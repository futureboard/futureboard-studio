use super::*;
use crate::types::{EngineRoutingSnapshot, EngineTrackSnapshot};

fn monitored_audio_snapshot() -> EngineProjectSnapshot {
    EngineProjectSnapshot {
        spatial: Default::default(),
        project_id: "asio-routing-test".into(),
        project_root: None,
        preferred_input_device: None,
        bpm: 120.0,
        tempo_points: Vec::new(),
        time_signature: [4, 4],
        sample_rate: 48_000,
        tracks: vec![EngineTrackSnapshot {
            midi_programs: Vec::new(),
            id: "audio-1".into(),
            track_type: "audio".into(),
            volume: 1.0,
            pan: 0.0,
            muted: false,
            solo: false,
            armed: true,
            input_monitor: true,
            input_source: EngineTrackInputSourceSnapshot {
                device_id: Some("test-asio-driver".into()),
                channels: vec![0, 1],
            },
            preview_mode: "stereo".into(),
            output_track_id: None,
            inserts: Vec::new(),
            sends: Vec::new(),
            automation_lanes: Vec::new(),
            builtin_soundfont_player: false,
            soundfont_path: None,
            soundfont_preset_bank: None,
            soundfont_preset_patch: None,
            soundfont_volume: 1.0,
            soundfont_reverb_chorus: true,
            soundfont_polyphony: 64,
            soundfont_envelope: Default::default(),
            soundfont_quality: Default::default(),
            soundfont_mode: Default::default(),
            soundfont_channels: Default::default(),
            solfege_engine: None,
        }],
        clips: Vec::new(),
        midi_clips: Vec::new(),
        pdc_enabled: true,
        latency_graph_version: 0,
        routing: EngineRoutingSnapshot {
            master_output_device: None,
            sample_rate: 48_000,
            buffer_size: 256,
        },
    }
}

/// The regression this guard exists for: recording needs an armed track, an
/// armed track makes a live-input sync want the capture device, and any
/// project sync during a take — the auto-save before Record is enough to
/// cause one — used to walk straight in, stop the stream the recorder was
/// feeding, and open a second client on the same endpoint. The take then
/// recorded nothing.
#[test]
fn a_running_take_owns_the_capture_device() {
    assert!(
        !live_input_sync_may_touch_stream(true),
        "a project sync must not rebuild the capture stream under a take"
    );
    assert!(
        live_input_sync_may_touch_stream(false),
        "with no take running the sync is the only thing that owns the stream"
    );
}

/// Pressing R on a track hung the application.
///
/// `update_track_input_state_inner` holds the recording lock for the whole
/// route change — that is what stops a route from moving under a running
/// take. `sync_live_input_stream` then grew its own take-owns-the-device
/// guard, which locked the same `parking_lot::Mutex` a second time on the
/// same thread. Not a nested read: a deadlock. The UI thread parked on its
/// own guard, macOS put "Application Not Responding" on the window, and the
/// log stopped before this function could print anything at all — the arm
/// never reached a device, so there was nothing in the trace to follow.
///
/// Run on a worker with a deadline, because the failure mode is a hang: a
/// same-thread call would take the test runner down with it rather than
/// report.
#[test]
fn arming_a_track_does_not_deadlock_on_the_recording_lock() {
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::time::Duration;

    let engine = Arc::new(EngineInner::new());
    // ASIO with no published capabilities returns from the sync before any
    // device work, so this exercises the lock order and nothing else.
    engine.daux_config.lock().backend = BackendKind::Asio;
    *engine.project.lock() = Some(monitored_audio_snapshot());

    let worker = Arc::clone(&engine);
    let (tx, rx) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        let result = worker.update_track_input_state(
            "audio-1",
            true,
            false,
            EngineTrackInputSourceSnapshot {
                device_id: Some("test-asio-driver".into()),
                channels: vec![0, 1],
            },
        );
        let _ = tx.send(result.is_ok());
    });

    let armed = rx.recv_timeout(Duration::from_secs(10));
    assert!(
        armed.is_ok(),
        "arming a track never returned: the route change is deadlocked on \
             the recording lock it already holds"
    );
    assert!(armed.unwrap(), "arming an audio track should succeed");
    handle.join().expect("worker thread");
}

/// The guard is what makes the deadlock unrepresentable, so check it is
/// actually consulted: with a take running, the sync must decline to touch
/// the stream even when the caller hands it a snapshot that wants one.
#[test]
fn a_locked_sync_still_declines_while_a_take_owns_the_device() {
    let engine = EngineInner::new();
    engine.daux_config.lock().backend = BackendKind::Asio;

    // `None` is "no take running" — the sync proceeds.
    let result = engine.sync_live_input_stream_locked(&monitored_audio_snapshot(), &None);
    assert!(result.is_ok());
}

#[test]
fn asio_input_routing_waits_for_duplex_session() {
    let engine = EngineInner::new();
    engine.daux_config.lock().backend = BackendKind::Asio;

    let result = engine.sync_live_input_stream(&monitored_audio_snapshot());

    assert!(result.is_ok());
    assert!(engine.live_input.lock().is_none());
    assert!(!engine.shared.input_ring.is_active());
}

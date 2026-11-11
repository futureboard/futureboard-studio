use super::{render_project_block_interleaved, render_project_block_interleaved_with_live_input};
use crate::runtime::RuntimeProject;
use crate::types::{
    EngineInsertSnapshot, EngineProjectSnapshot, EngineRoutingSnapshot,
    EngineTrackInputSourceSnapshot, EngineTrackSnapshot,
};
use std::collections::HashMap;

fn track(id: &str, track_type: &str) -> EngineTrackSnapshot {
    EngineTrackSnapshot {
        midi_programs: Vec::new(),
        id: id.to_string(),
        track_type: track_type.to_string(),
        volume: 1.0,
        pan: 0.0,
        muted: false,
        solo: false,
        armed: false,
        input_monitor: track_type == "audio",
        input_source: if track_type == "audio" {
            EngineTrackInputSourceSnapshot {
                device_id: Some("asio:test".to_string()),
                channels: vec![0, 1],
            }
        } else {
            Default::default()
        },
        preview_mode: "stereo".to_string(),
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
    }
}

fn runtime() -> RuntimeProject {
    let snapshot = EngineProjectSnapshot {
        spatial: Default::default(),
        project_id: "monitor-test".to_string(),
        project_root: None,
        preferred_input_device: None,
        bpm: 120.0,
        tempo_points: Vec::new(),
        time_signature: [4, 4],
        sample_rate: 48_000,
        tracks: vec![track("audio-1", "audio"), track("master", "master")],
        clips: Vec::new(),
        midi_clips: Vec::new(),
        pdc_enabled: true,
        latency_graph_version: 1,
        routing: EngineRoutingSnapshot {
            master_output_device: None,
            sample_rate: 48_000,
            buffer_size: 256,
        },
    };
    RuntimeProject::build(&snapshot, 48_000, &mut HashMap::new(), None, true)
        .expect("monitor runtime")
}

const FRAMES: usize = 64;

fn render_monitored_block(runtime: &mut RuntimeProject, base_sample: u64) -> (f32, f32) {
    let input = [0.5f32; FRAMES];
    let mut output = [0.0f32; FRAMES * 2];
    render_project_block_interleaved_with_live_input(
        runtime,
        base_sample,
        1.0,
        &mut output,
        2,
        false,
        4,
        4,
        None,
        &input,
        &input,
    );
    let mut peak_l = 0.0f32;
    let mut peak_r = 0.0f32;
    for frame in output.chunks(2) {
        peak_l = peak_l.max(frame[0].abs());
        peak_r = peak_r.max(frame[1].abs());
    }
    (peak_l, peak_r)
}

#[test]
fn monitored_input_obeys_track_pan_and_mute() {
    let mut runtime = runtime();
    // Exact per-block gains (no first-block ramp) so pan asserts are strict.
    runtime.fader_smoothing = false;
    runtime.tracks[0].pan = -1.0;
    let (left, right) = render_monitored_block(&mut runtime, 0);
    assert!(left > 0.0);
    assert!(right.abs() < 1.0e-6);

    runtime.tracks[0].muted = true;
    let (muted_l, muted_r) = render_monitored_block(&mut runtime, FRAMES as u64);
    assert!(muted_l.abs() < 1.0e-6);
    assert!(muted_r.abs() < 1.0e-6);
}

/// The channel fader has to attenuate what actually leaves the engine, and
/// `SetTrackVolume` (which is all `update_track_volume` is) has to be what
/// moves it. Reported as "master works, the channel strip does not": the
/// master gain is an atomic applied to the interleaved output, while a track
/// fader travels the command queue into `RuntimeTrack::volume` and is applied
/// in `apply_fader`, so the two halves fail independently.
#[test]
fn track_fader_attenuates_the_rendered_block() {
    let mut runtime = runtime();
    // Exact per-block gain, no first-block ramp, so the ratio is strict.
    runtime.fader_smoothing = false;

    let (unity_l, unity_r) = render_monitored_block(&mut runtime, 0);
    assert!(unity_l > 0.1, "test signal must be audible at unity");

    // -11 dB, the level from the bug report.
    let minus_11_db = 10.0f32.powf(-11.0 / 20.0);
    assert!(
        runtime.update_track_volume("audio-1", minus_11_db),
        "the command must find the track by id"
    );
    let (faded_l, faded_r) = render_monitored_block(&mut runtime, FRAMES as u64);

    let expected_l = unity_l * minus_11_db;
    let expected_r = unity_r * minus_11_db;
    assert!(
        (faded_l - expected_l).abs() < 1.0e-4 && (faded_r - expected_r).abs() < 1.0e-4,
        "fader must scale the output: unity=({unity_l:.6}, {unity_r:.6}) \
             faded=({faded_l:.6}, {faded_r:.6}) expected=({expected_l:.6}, {expected_r:.6})"
    );
}

#[test]
fn unmonitored_track_receives_no_live_input() {
    let mut runtime = runtime();
    runtime.fader_smoothing = false;
    runtime.tracks[0].monitor_enabled = false;
    let (left, right) = render_monitored_block(&mut runtime, 0);
    assert!(left.abs() < 1.0e-6);
    assert!(right.abs() < 1.0e-6);
}

#[test]
fn stress_1000_tracks_and_128_bypassed_inserts_render_only_active_source() {
    let mut tracks: Vec<_> = (0..1_000)
        .map(|index| track(&format!("midi-{index}"), "midi"))
        .collect();
    tracks[500].inserts = (0..128)
        .map(|index| EngineInsertSnapshot {
            id: format!("bypassed-{index}"),
            kind: "gain".to_string(),
            enabled: false,
            params: HashMap::new(),
            state: None,
        })
        .collect();
    tracks.push(track("master", "master"));
    let snapshot = EngineProjectSnapshot {
        spatial: Default::default(),
        project_id: "stress-1k-tracks".to_string(),
        project_root: None,
        preferred_input_device: None,
        bpm: 120.0,
        tempo_points: Vec::new(),
        time_signature: [4, 4],
        sample_rate: 48_000,
        tracks,
        clips: Vec::new(),
        midi_clips: Vec::new(),
        pdc_enabled: true,
        latency_graph_version: 1,
        routing: EngineRoutingSnapshot {
            master_output_device: None,
            sample_rate: 48_000,
            buffer_size: 256,
        },
    };
    let mut runtime = RuntimeProject::build(&snapshot, 48_000, &mut HashMap::new(), None, true)
        .expect("large runtime");
    assert_eq!(runtime.tracks.len(), 1_001);
    assert_eq!(
        runtime
            .audio_graph
            .active_source_mask
            .iter()
            .filter(|active| **active)
            .count(),
        1,
        "only the source with an insert chain should touch audio buffers"
    );

    let mut output = [0.0f32; FRAMES * 2];
    render_project_block_interleaved(&mut runtime, 0, 1.0, &mut output, 2, true, 4, 4, None);
    assert!(output.iter().all(|sample| *sample == 0.0));
}

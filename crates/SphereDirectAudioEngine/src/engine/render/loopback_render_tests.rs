//! Track loopback through the real render kernel: a return (the WDW return a
//! Rodhareist wet/dry/wet split creates) heard on, and recorded by, an audio
//! track whose input is that return.

use super::render_project_block_interleaved_with_live_input;
use crate::runtime::{RuntimeProject, RuntimeTrackInputSource};
use crate::types::{
    EngineProjectSnapshot, EngineRoutingSnapshot, EngineTrackInputSourceSnapshot,
    EngineTrackSnapshot,
};
use std::collections::HashMap;

const FRAMES: usize = 64;
const SRC: usize = 0;
const WDW: usize = 1;
const DEST: usize = 2;

fn track(id: &str, track_type: &str, input: EngineTrackInputSourceSnapshot) -> EngineTrackSnapshot {
    EngineTrackSnapshot {
        midi_programs: Vec::new(),
        id: id.to_string(),
        track_type: track_type.to_string(),
        volume: 1.0,
        pan: 0.0,
        muted: false,
        solo: false,
        armed: track_type == "audio",
        input_monitor: track_type == "audio",
        input_source: input,
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

/// `src` (a monitored hardware input) feeds the `wdw` return through its
/// output; `dest` takes `wdw` as its input.
fn runtime() -> RuntimeProject {
    let mut src = track(
        "src",
        "audio",
        EngineTrackInputSourceSnapshot {
            device_id: Some("asio:test".to_string()),
            channels: vec![0, 1],
        },
    );
    src.output_track_id = Some("wdw".to_string());
    let dest = track(
        "dest",
        "audio",
        EngineTrackInputSourceSnapshot {
            device_id: Some(crate::loopback::loopback_device_id("wdw")),
            channels: vec![0, 1],
        },
    );
    let snapshot = EngineProjectSnapshot {
        spatial: Default::default(),
        project_id: "loopback-render".to_string(),
        project_root: None,
        preferred_input_device: None,
        bpm: 120.0,
        tempo_points: Vec::new(),
        time_signature: [4, 4],
        sample_rate: 48_000,
        tracks: vec![
            src,
            track("wdw", "return", Default::default()),
            dest,
            track("master", "master", Default::default()),
        ],
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
        .expect("loopback runtime");
    runtime.resolve_loopback_inputs(&snapshot);
    runtime.fader_smoothing = false;
    runtime
}

fn render(runtime: &mut RuntimeProject, block: u64, input: f32) {
    let input = [input; FRAMES];
    let mut output = [0.0f32; FRAMES * 2];
    render_project_block_interleaved_with_live_input(
        runtime,
        block * FRAMES as u64,
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
}

fn peak(samples: &[f32]) -> f32 {
    samples.iter().fold(0.0f32, |peak, s| peak.max(s.abs()))
}

#[test]
fn a_return_is_heard_on_the_track_that_takes_it_as_input() {
    let mut runtime = runtime();
    assert!(matches!(
        runtime.tracks[DEST].input_source,
        RuntimeTrackInputSource::Loopback { source_index, .. } if source_index as usize == WDW
    ));
    assert!(runtime.tracks[WDW].loopback_publish);
    assert_eq!(runtime.tracks[SRC].output_track_index, Some(WDW));

    // Block 0: the return renders; the destination reads the block before.
    render(&mut runtime, 0, 0.5);
    assert!(peak(&runtime.tracks[WDW].loopback_out_l[..FRAMES]) > 0.1);
    assert_eq!(peak(&runtime.tracks[DEST].block_l[..FRAMES]), 0.0);

    // Block 1: the destination hears the return. A return renders in the
    // routing pass, and a tap only in the source pass never kept its block.
    render(&mut runtime, 1, 0.5);
    assert!(
        peak(&runtime.tracks[DEST].block_l[..FRAMES]) > 0.1,
        "the WDW return never reached the track that takes it as input"
    );
}

/// A source the graph stops rendering — here a muted return — must go
/// silent on its readers, not repeat the last block it rendered.
#[test]
fn a_source_that_stops_rendering_is_not_repeated() {
    let mut runtime = runtime();
    render(&mut runtime, 0, 0.5);
    render(&mut runtime, 1, 0.5);
    assert!(peak(&runtime.tracks[DEST].block_l[..FRAMES]) > 0.1);

    runtime.update_track_mute_at(WDW, true);
    render(&mut runtime, 2, 0.5);
    render(&mut runtime, 3, 0.5);
    assert_eq!(
        peak(&runtime.tracks[DEST].block_l[..FRAMES]),
        0.0,
        "a muted return kept feeding its last block"
    );
}

/// The take records the return's output of the block just rendered, with
/// no audio interface involved and no loopback delay, on the lane the
/// destination track was given.
#[test]
fn a_loopback_take_records_the_source_block_just_rendered() {
    use std::sync::atomic::{AtomicBool, AtomicU64};
    use std::sync::Arc;

    let (audio_tx, audio_rx) = crossbeam_channel::bounded(8);
    let (free_tx, free_rx) = crossbeam_channel::bounded(8);
    for _ in 0..8 {
        let _ = free_tx.try_send(Vec::with_capacity(FRAMES * 2));
    }
    let first_capture_sample = Arc::new(AtomicU64::new(u64::MAX));
    let sink = crate::capture_tap::RecordSink {
        audio_tx,
        free_rx,
        free_tx,
        samples_per_bin: 16,
        capture_on_transport: false,
        dropped_blocks: Arc::new(AtomicU64::new(0)),
        active: Arc::new(AtomicBool::new(true)),
        first_capture_sample: Arc::clone(&first_capture_sample),
        monitor: crate::capture_tap::PreviewAccum::new(0, 1),
        tracks: vec![crate::capture_tap::PreviewAccum::new(0, 1)],
        gap_frames: 0,
    };
    let recorder = crate::loopback::LoopbackRecorder::new(sink, 1);
    recorder.map_destinations(&[Some(DEST)]);
    let shared = crate::engine::SharedState::default();

    let mut runtime = runtime();
    render(&mut runtime, 5, 0.5);
    let expected = runtime.tracks[WDW].loopback_out_l[0];
    assert!(expected.abs() > 0.1);
    recorder.capture_block(&runtime.tracks, FRAMES, 5 * FRAMES as u64, &shared);

    let block: crate::recording::RecordBlock = audio_rx.try_recv().expect("a recorded block");
    assert_eq!(block.gap_frames, 0);
    assert_eq!(block.samples.len(), FRAMES * 2);
    let want = crate::recording::f32_to_s32(expected);
    assert_eq!(block.samples[0], want, "left: the return's own block");
    assert_eq!(block.samples[1], want, "right");
    assert_eq!(
        first_capture_sample.load(std::sync::atomic::Ordering::Relaxed),
        5 * FRAMES as u64,
        "the take starts on the block it recorded"
    );

    // A lane whose track is not in this graph records silence rather than
    // somebody else's audio.
    recorder.map_destinations(&[None]);
    recorder.capture_block(&runtime.tracks, FRAMES, 6 * FRAMES as u64, &shared);
    let block = audio_rx.try_recv().expect("a recorded block");
    assert!(block.samples.iter().all(|sample| *sample == 0));
}

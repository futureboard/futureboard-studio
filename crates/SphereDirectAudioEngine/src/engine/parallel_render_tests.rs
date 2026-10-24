//! Multi-core processing and the SIMD kernels must not change a sample.
//!
//! Drives the real graph — in-process inserts, sends, returns, a bus feeding a
//! group, a muted track and a master insert — once serially and once through
//! the worker pool, on every instruction set, and requires bit-identical
//! output.

use super::render_project_block_interleaved_with_live_input;
use crate::dsp::simd::{self, SimdLevel};
use crate::runtime::RuntimeProject;
use crate::types::{
    EngineInsertSnapshot, EngineProjectSnapshot, EngineRoutingSnapshot, EngineSendSnapshot,
    EngineTrackInputSourceSnapshot, EngineTrackSnapshot,
};
use std::collections::HashMap;

const FRAMES: usize = 128;
const CHANNELS: usize = 2;
const SAMPLE_RATE: u32 = 48_000;
const BLOCKS: usize = 48;

fn track(id: &str, track_type: &str) -> EngineTrackSnapshot {
    let is_audio = track_type == "audio";
    EngineTrackSnapshot {
        midi_programs: Vec::new(),
        id: id.to_string(),
        track_type: track_type.to_string(),
        volume: 1.0,
        pan: 0.0,
        muted: false,
        solo: false,
        armed: false,
        input_monitor: is_audio,
        input_source: if is_audio {
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

fn gain_insert(id: &str, gain_db: f32) -> EngineInsertSnapshot {
    let mut params = HashMap::new();
    params.insert("gainDb".to_string(), serde_json::json!(gain_db as f64));
    EngineInsertSnapshot {
        id: id.to_string(),
        kind: "gain".to_string(),
        enabled: true,
        params,
        state: None,
    }
}

fn send(target: &str, level: f32, pre_fader: bool) -> EngineSendSnapshot {
    EngineSendSnapshot {
        id: format!("send-{target}"),
        return_track_id: target.to_string(),
        level,
        enabled: true,
        pre_fader,
    }
}

/// Eight sources in varied states, two returns, a bus that sums into a group
/// that a return also feeds, and a master insert.
fn build_project() -> RuntimeProject {
    let mut tracks = Vec::new();
    for i in 0..8 {
        let mut source = track(&format!("audio-{i}"), "audio");
        source.volume = 0.4 + i as f32 * 0.07;
        source.pan = -0.8 + i as f32 * 0.23;
        source.inserts = vec![
            gain_insert(&format!("gain-{i}-a"), -6.0 + i as f32),
            gain_insert(&format!("gain-{i}-b"), 1.5),
        ];
        source.sends = vec![
            send("return-1", 0.3, false),
            send("return-2", 0.2, i % 2 == 0),
        ];
        if i % 3 == 0 {
            source.output_track_id = Some("bus-a".to_string());
        }
        source.muted = i == 5;
        tracks.push(source);
    }
    let mut return_1 = track("return-1", "return");
    return_1.inserts = vec![gain_insert("return-1-gain", -3.0)];
    let mut return_2 = track("return-2", "return");
    return_2.inserts = vec![gain_insert("return-2-gain", 2.0)];
    return_2.output_track_id = Some("group-1".to_string());
    let mut bus = track("bus-a", "bus");
    bus.inserts = vec![gain_insert("bus-gain", -1.0)];
    bus.output_track_id = Some("group-1".to_string());
    let mut group = track("group-1", "group");
    group.inserts = vec![gain_insert("group-gain", -2.0)];
    let mut master = track("master", "master");
    master.inserts = vec![gain_insert("master-gain", -0.5)];
    tracks.extend([return_1, return_2, bus, group, master]);

    let snapshot = EngineProjectSnapshot {
        spatial: Default::default(),
        project_id: "parallel-render".to_string(),
        project_root: None,
        preferred_input_device: None,
        bpm: 120.0,
        tempo_points: Vec::new(),
        time_signature: [4, 4],
        sample_rate: SAMPLE_RATE,
        tracks,
        clips: Vec::new(),
        midi_clips: Vec::new(),
        pdc_enabled: true,
        latency_graph_version: 1,
        routing: EngineRoutingSnapshot {
            master_output_device: None,
            sample_rate: SAMPLE_RATE,
            buffer_size: 256,
        },
    };
    RuntimeProject::build(&snapshot, SAMPLE_RATE, &mut HashMap::new(), None, true)
        .expect("parallel render runtime")
}

/// Render `BLOCKS` blocks of a changing input and return every output sample
/// together with the final track meters.
fn render(multicore: bool, level: SimdLevel) -> (Vec<f32>, Vec<(f32, f32)>) {
    crate::parallel::configure_multicore(multicore, 4);
    simd::set_simd_level(level);
    let mut runtime = build_project();
    let mut all = Vec::with_capacity(BLOCKS * FRAMES * CHANNELS);
    let mut input_l = vec![0.0f32; FRAMES];
    let mut input_r = vec![0.0f32; FRAMES];
    for block in 0..BLOCKS {
        for i in 0..FRAMES {
            let t = (block * FRAMES + i) as f32;
            input_l[i] = (t * 0.013).sin() * 0.6;
            input_r[i] = (t * 0.021).cos() * 0.5;
        }
        let mut output = vec![0.0f32; FRAMES * CHANNELS];
        render_project_block_interleaved_with_live_input(
            &mut runtime,
            (block * FRAMES) as u64,
            0.9,
            &mut output,
            CHANNELS,
            true,
            4,
            4,
            None,
            &input_l,
            &input_r,
        );
        all.extend_from_slice(&output);
    }
    let meters = runtime
        .tracks
        .iter()
        .map(|track| (track.meter_peak_l, track.meter_peak_r))
        .collect();
    (all, meters)
}

/// Peak of the device output when one audio track plays through `target` (a
/// routing track of `target_type`), or straight to master. The track plays a
/// clip when `from_clip`, its monitored input otherwise.
fn routed_output_peak(target: Option<(&str, &str)>, from_clip: bool) -> f32 {
    let mut source = track("audio-0", "audio");
    let mut cache: HashMap<String, std::sync::Arc<crate::audio_source::ClipAudioSource>> =
        HashMap::new();
    let mut clips = Vec::new();
    if from_clip {
        source.input_monitor = false;
        source.input_source = Default::default();
        let frames = SAMPLE_RATE as usize;
        cache.insert(
            "dc.wav".to_string(),
            std::sync::Arc::new(crate::audio_source::ClipAudioSource::InMemory(
                std::sync::Arc::new(crate::audio_file::AudioFileBuffer {
                    sample_rate: SAMPLE_RATE,
                    channels: 2,
                    frames,
                    samples: vec![0.5; frames * 2],
                }),
            )),
        );
        clips.push(crate::types::EngineClipSnapshot {
            id: "dc".to_string(),
            track_id: "audio-0".to_string(),
            asset_id: "dc".to_string(),
            media_path: Some("dc.wav".to_string()),
            start_beat: 0.0,
            duration_beats: 2.0,
            offset_seconds: 0.0,
            gain: 1.0,
            muted: false,
            ara_rendered: false,
            fades: None,
            stretch: Default::default(),
            audio_process: None,
        });
    }
    let mut tracks = Vec::new();
    if let Some((id, track_type)) = target {
        source.output_track_id = Some(id.to_string());
        tracks.push(source);
        tracks.push(track(id, track_type));
    } else {
        tracks.push(source);
    }
    tracks.push(track("master", "master"));
    let snapshot = EngineProjectSnapshot {
        spatial: Default::default(),
        project_id: "bus-routing".to_string(),
        project_root: None,
        preferred_input_device: None,
        bpm: 120.0,
        tempo_points: Vec::new(),
        time_signature: [4, 4],
        sample_rate: SAMPLE_RATE,
        tracks,
        clips,
        midi_clips: Vec::new(),
        pdc_enabled: true,
        latency_graph_version: 1,
        routing: EngineRoutingSnapshot {
            master_output_device: None,
            sample_rate: SAMPLE_RATE,
            buffer_size: 256,
        },
    };
    let mut runtime = RuntimeProject::build(&snapshot, SAMPLE_RATE, &mut cache, None, true)
        .expect("bus routing runtime");
    let input_l = vec![0.5f32; FRAMES];
    let input_r = vec![0.5f32; FRAMES];
    let mut peak = 0.0f32;
    for block in 0..8 {
        let mut output = vec![0.0f32; FRAMES * CHANNELS];
        render_project_block_interleaved_with_live_input(
            &mut runtime,
            (block * FRAMES) as u64,
            1.0,
            &mut output,
            CHANNELS,
            true,
            4,
            4,
            None,
            &input_l,
            &input_r,
        );
        peak = output
            .iter()
            .fold(peak, |peak, sample| peak.max(sample.abs()));
    }
    peak
}

#[test]
fn a_track_routed_to_a_bus_or_group_is_heard() {
    for from_clip in [false, true] {
        let direct = routed_output_peak(None, from_clip);
        assert!(direct > 0.1, "the source itself is silent: {direct}");
        for target in [
            ("bus-a", "bus"),
            ("group-1", "group"),
            ("return-1", "return"),
        ] {
            let routed = routed_output_peak(Some(target), from_clip);
            assert!(
                (routed - direct).abs() < 1e-3,
                "routing a {} through {target:?} gave {routed}, straight to master {direct}",
                if from_clip { "clip" } else { "monitored input" }
            );
        }
    }
}

#[test]
fn multicore_and_every_instruction_set_render_the_same_samples() {
    let (reference, reference_meters) = render(false, SimdLevel::Sse);
    assert!(
        reference.iter().any(|sample| sample.abs() > 0.01),
        "the project must actually produce audio"
    );
    for (multicore, level) in [
        (false, SimdLevel::Avx2),
        (true, SimdLevel::Sse),
        (true, SimdLevel::Avx2),
    ] {
        let dispatched = crate::parallel::DISPATCHED.load(std::sync::atomic::Ordering::Relaxed);
        let (output, meters) = render(multicore, level);
        if multicore {
            assert!(
                crate::parallel::DISPATCHED.load(std::sync::atomic::Ordering::Relaxed) > dispatched,
                "multi-core processing never handed a batch to the pool"
            );
        }
        assert_eq!(
            output, reference,
            "multicore={multicore} level={level:?} changed the rendered samples"
        );
        assert_eq!(
            meters, reference_meters,
            "multicore={multicore} level={level:?}"
        );
    }
    crate::parallel::configure_multicore(false, 0);
    simd::set_simd_level(SimdLevel::Avx2);
}

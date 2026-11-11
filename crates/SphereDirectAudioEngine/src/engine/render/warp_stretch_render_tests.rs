use super::render_project_block_interleaved;
use crate::audio_file::AudioFileBuffer;
use crate::audio_source::ClipAudioSource;
use crate::runtime::RuntimeProject;
use crate::types::{
    EngineClipSnapshot, EngineProjectSnapshot, EngineRoutingSnapshot, EngineTrackSnapshot,
};
use std::collections::HashMap;
use std::sync::Arc;

const SR: u32 = 48_000;

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
        input_monitor: false,
        input_source: Default::default(),
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

/// A Keep-Pitch Warp clip — 2 s of a 220 Hz tone over 4 beats at 120 BPM,
/// with one marker squeezing the first half-second of audio into the first
/// second — used to fall back to plain resampling, so its pitch moved with
/// every segment. It must now stream through the pitch-preserving
/// stretcher and still produce the whole clip.
#[test]
fn keep_pitch_warp_clip_renders_through_the_stretcher() {
    if !SphereAudioProcessor::signalsmith_stretch_available() {
        return; // C++ backend not built in this environment.
    }
    let frames = 96_000usize;
    let samples: Vec<f32> = (0..frames)
        .flat_map(|i| {
            let v = (i as f32 * 220.0 * std::f32::consts::TAU / SR as f32).sin() * 0.5;
            [v, v]
        })
        .collect();
    let mut cache: HashMap<String, Arc<ClipAudioSource>> = HashMap::new();
    cache.insert(
        "tone.wav".to_string(),
        Arc::new(ClipAudioSource::InMemory(Arc::new(AudioFileBuffer {
            sample_rate: SR,
            channels: 2,
            frames,
            samples,
        }))),
    );
    let audio_process = serde_json::from_value(serde_json::json!({
        "speedRatio": 1.0,
        "pitchSemitones": 0.0,
        "preservePitch": true,
        "mode": "warp",
        "quality": "Phase Vocoder",
        "sourceStartSamples": 0,
        "sourceEndSamples": frames,
        "warpMarkers": [
            { "id": 1, "sourceSample": 24_000, "timelineBeat": 2.0, "locked": false }
        ]
    }))
    .expect("audio process");
    let clip = EngineClipSnapshot {
        id: "warp".to_string(),
        track_id: "audio-1".to_string(),
        asset_id: "tone".to_string(),
        media_path: Some("tone.wav".to_string()),
        start_beat: 0.0,
        duration_beats: 4.0,
        offset_seconds: 0.0,
        gain: 1.0,
        muted: false,
        ara_rendered: false,
        fades: None,
        stretch: SphereAudioProcessor::StretchParams {
            mode: SphereAudioProcessor::StretchMode::Warp,
            algorithm: SphereAudioProcessor::StretchAlgorithm::PreservePitch,
            time_ratio: 1.0,
            preserve_pitch: true,
            target_bpm: Some(120.0),
            ..SphereAudioProcessor::StretchParams::default()
        },
        audio_process: Some(audio_process),
    };
    let snapshot = EngineProjectSnapshot {
        spatial: Default::default(),
        project_id: "warp-stretch".to_string(),
        project_root: None,
        preferred_input_device: None,
        bpm: 120.0,
        tempo_points: Vec::new(),
        time_signature: [4, 4],
        sample_rate: SR,
        tracks: vec![track("audio-1", "audio"), track("master", "master")],
        clips: vec![clip],
        midi_clips: Vec::new(),
        pdc_enabled: true,
        latency_graph_version: 1,
        routing: EngineRoutingSnapshot {
            master_output_device: None,
            sample_rate: SR,
            buffer_size: 512,
        },
    };
    let mut runtime =
        RuntimeProject::build(&snapshot, SR, &mut cache, None, true).expect("runtime");
    assert_eq!(runtime.clips.len(), 1);
    assert!(!runtime.clips[0].warp_segments.is_empty());
    assert!(runtime.clips[0].stretch_processor.is_some());

    let block = 512usize;
    let mut output = vec![0.0f32; block * 2];
    let mut energy_by_second = [0.0f64; 2];
    let mut left = Vec::with_capacity(frames);
    let mut sample = 0u64;
    while (sample as usize) < frames {
        output.iter_mut().for_each(|v| *v = 0.0);
        render_project_block_interleaved(
            &mut runtime,
            sample,
            1.0,
            &mut output,
            2,
            true,
            4,
            4,
            None,
        );
        assert!(output.iter().all(|v| v.is_finite()));
        // The stretcher path records where its stream continues; the
        // resample fallback never touches it.
        assert!(
            runtime.clips[0].stretch_next_project_sample.is_some(),
            "block at {sample} fell back to resampling"
        );
        let second = (sample as usize / SR as usize).min(1);
        energy_by_second[second] += output.iter().map(|v| (*v as f64).powi(2)).sum::<f64>();
        left.extend(output.iter().step_by(2).copied());
        sample += block as u64;
    }
    // The pitch holds in both segments: the first plays its audio at
    // half speed, the second at 1.5x — resampled, they would sound at
    // 110 Hz and 330 Hz.
    let pitch_hz = |from: f64, to: f64| {
        let span = &left[(from * SR as f64) as usize..(to * SR as f64) as usize];
        let crossings = span
            .windows(2)
            .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
            .count();
        crossings as f64 / 2.0 / (to - from)
    };
    for (from, to) in [(0.25, 0.75), (1.25, 1.75)] {
        let hz = pitch_hz(from, to);
        assert!(
            (hz - 220.0).abs() < 8.0,
            "{from}-{to} s plays at {hz:.1} Hz"
        );
    }
    // Both warp segments are heard — neither half was starved of input.
    assert!(
        energy_by_second[0] > 100.0,
        "first segment {energy_by_second:?}"
    );
    assert!(
        energy_by_second[1] > 100.0,
        "second segment {energy_by_second:?}"
    );
}

/// Where each 10 ms burst of a click track lands in the rendered output,
/// in seconds: the energy centroid of a window around where it belongs.
fn burst_centres(left: &[f32], expected: &[f64]) -> Vec<f64> {
    expected
        .iter()
        .map(|&at| {
            let from = ((at - 0.2) * SR as f64).max(0.0) as usize;
            let to = (((at + 0.2) * SR as f64) as usize).min(left.len());
            let (mut weighted, mut total) = (0.0f64, 0.0f64);
            for (i, v) in left[from..to].iter().enumerate() {
                let e = (*v as f64).powi(2);
                weighted += e * (from + i) as f64;
                total += e;
            }
            weighted / total.max(1e-12) / SR as f64
        })
        .collect()
}

/// Keep-Pitch Warp, with the markers in `audio_process`.
fn warp_stretch() -> SphereAudioProcessor::StretchParams {
    SphereAudioProcessor::StretchParams {
        mode: SphereAudioProcessor::StretchMode::Warp,
        algorithm: SphereAudioProcessor::StretchAlgorithm::PreservePitch,
        time_ratio: 1.0,
        preserve_pitch: true,
        target_bpm: Some(120.0),
        ..SphereAudioProcessor::StretchParams::default()
    }
}

/// Render a 4 s click track — a 10 ms, 1 kHz burst on every beat at
/// 120 BPM — as a `duration_beats` clip stretched by `stretch` (and
/// `audio_process`, for warp markers), from `start_sample`, and return the
/// left channel from there on.
fn render_click_track(
    stretch: SphereAudioProcessor::StretchParams,
    audio_process: Option<serde_json::Value>,
    duration_beats: f64,
    start_sample: u64,
) -> Vec<f32> {
    let frames = 4 * SR as usize;
    let beat = SR as usize / 2;
    let samples: Vec<f32> = (0..frames)
        .flat_map(|i| {
            let in_burst = i % beat < SR as usize / 100;
            let v = if in_burst {
                (i as f32 * 1_000.0 * std::f32::consts::TAU / SR as f32).sin() * 0.5
            } else {
                0.0
            };
            [v, v]
        })
        .collect();
    let mut cache: HashMap<String, Arc<ClipAudioSource>> = HashMap::new();
    cache.insert(
        "clicks.wav".to_string(),
        Arc::new(ClipAudioSource::InMemory(Arc::new(AudioFileBuffer {
            sample_rate: SR,
            channels: 2,
            frames,
            samples,
        }))),
    );
    let audio_process = audio_process.map(|v| serde_json::from_value(v).expect("audio process"));
    let clip = EngineClipSnapshot {
        id: "clicks".to_string(),
        track_id: "audio-1".to_string(),
        asset_id: "clicks".to_string(),
        media_path: Some("clicks.wav".to_string()),
        start_beat: 0.0,
        duration_beats,
        offset_seconds: 0.0,
        gain: 1.0,
        muted: false,
        ara_rendered: false,
        fades: None,
        stretch,
        audio_process,
    };
    let render_frames = (duration_beats / 2.0 * SR as f64) as usize;
    let snapshot = EngineProjectSnapshot {
        spatial: Default::default(),
        project_id: "click-stretch".to_string(),
        project_root: None,
        preferred_input_device: None,
        bpm: 120.0,
        tempo_points: Vec::new(),
        time_signature: [4, 4],
        sample_rate: SR,
        tracks: vec![track("audio-1", "audio"), track("master", "master")],
        clips: vec![clip],
        midi_clips: Vec::new(),
        pdc_enabled: true,
        latency_graph_version: 1,
        routing: EngineRoutingSnapshot {
            master_output_device: None,
            sample_rate: SR,
            buffer_size: 512,
        },
    };
    let mut runtime =
        RuntimeProject::build(&snapshot, SR, &mut cache, None, true).expect("runtime");
    assert!(runtime.clips[0].stretch_processor.is_some());

    let block = 512usize;
    let mut output = vec![0.0f32; block * 2];
    let mut left = Vec::with_capacity(render_frames);
    let mut sample = start_sample;
    while (sample as usize) + block <= render_frames {
        output.iter_mut().for_each(|v| *v = 0.0);
        render_project_block_interleaved(
            &mut runtime,
            sample,
            1.0,
            &mut output,
            2,
            true,
            4,
            4,
            None,
        );
        assert!(
            runtime.clips[0].stretch_next_project_sample.is_some(),
            "block at {sample} fell back to resampling"
        );
        left.extend(output.iter().step_by(2).copied());
        sample += block as u64;
    }
    left
}

/// A stretched clip plays on the grid, not a stretcher-latency late.
///
/// The stretcher needs input ahead of what it outputs. Priming it with the
/// audio *before* the play position and then feeding from the play
/// position put every beat of a Keep-Pitch clip ~100 ms behind the
/// metronome — from the start and after every locate.
#[test]
fn a_stretched_clip_plays_on_the_beat_from_any_start() {
    if !SphereAudioProcessor::signalsmith_stretch_available() {
        return; // C++ backend not built in this environment.
    }
    // A marker that maps beat 4 to 2 s: every beat stays where it was,
    // but the clip streams through the stretcher.
    let identity = serde_json::json!({
        "speedRatio": 1.0,
        "pitchSemitones": 0.0,
        "preservePitch": true,
        "mode": "warp",
        "quality": "Phase Vocoder",
        "sourceStartSamples": 0,
        "sourceEndSamples": 4 * SR,
        "warpMarkers": [
            { "id": 1, "sourceSample": 2 * SR, "timelineBeat": 4.0, "locked": false }
        ]
    });
    // From the top, and from a locate mid-clip (0.8 s, between beats).
    for start in [0u64, (SR as u64 * 4) / 5] {
        let left = render_click_track(warp_stretch(), Some(identity.clone()), 8.0, start);
        let start_s = start as f64 / SR as f64;
        let expected: Vec<f64> = (1..7)
            .map(|beat| beat as f64 * 0.5 + 0.005 - start_s)
            .filter(|&at| at > 0.3)
            .collect();
        for (want, got) in expected.iter().zip(burst_centres(&left, &expected)) {
            assert!(
                (got - want).abs() < 0.004,
                "from {start_s:.1} s: the beat due at {want:.3} s lands at {got:.3} s"
            );
        }
    }
}

/// Between markers the beats land where the warp map puts them: a clip
/// whose first two seconds of audio are squeezed into the first second
/// has its source beats on every quarter second there.
#[test]
fn a_warped_clip_puts_its_beats_where_the_markers_say() {
    if !SphereAudioProcessor::signalsmith_stretch_available() {
        return; // C++ backend not built in this environment.
    }
    let squeezed = serde_json::json!({
        "speedRatio": 1.0,
        "pitchSemitones": 0.0,
        "preservePitch": true,
        "mode": "warp",
        "quality": "Phase Vocoder",
        "sourceStartSamples": 0,
        "sourceEndSamples": 4 * SR,
        "warpMarkers": [
            { "id": 1, "sourceSample": 2 * SR, "timelineBeat": 2.0, "locked": false }
        ]
    });
    let left = render_click_track(warp_stretch(), Some(squeezed), 8.0, 0);
    // Source beats 1..3 (0.5, 1.0, 1.5 s) now sit at 0.25, 0.5, 0.75 s.
    let expected = [0.25 + 0.0025, 0.5 + 0.0025, 0.75 + 0.0025];
    for (want, got) in expected.iter().zip(burst_centres(&left, &expected)) {
        assert!(
            (got - want).abs() < 0.006,
            "the source beat due at {want:.3} s lands at {got:.3} s"
        );
    }
}

/// A clip stretched to twice its length — what Apply in the stretch
/// controls produces — has its beats every second, on the second, from
/// the top and after a locate.
#[test]
fn a_constant_stretch_keeps_its_beats_on_the_grid() {
    if !SphereAudioProcessor::signalsmith_stretch_available() {
        return; // C++ backend not built in this environment.
    }
    let doubled = SphereAudioProcessor::StretchParams {
        mode: SphereAudioProcessor::StretchMode::Manual,
        algorithm: SphereAudioProcessor::StretchAlgorithm::PreservePitch,
        time_ratio: 2.0,
        preserve_pitch: true,
        ..SphereAudioProcessor::StretchParams::default()
    };
    for start in [0u64, (SR as u64 * 5) / 2] {
        let left = render_click_track(doubled.clone(), None, 16.0, start);
        let start_s = start as f64 / SR as f64;
        // Each 10 ms burst is 20 ms long now.
        let expected: Vec<f64> = (1..7)
            .map(|beat| beat as f64 + 0.01 - start_s)
            .filter(|&at| at > 0.3)
            .collect();
        for (want, got) in expected.iter().zip(burst_centres(&left, &expected)) {
            assert!(
                (got - want).abs() < 0.006,
                "from {start_s:.1} s: the beat due at {want:.3} s lands at {got:.3} s"
            );
        }
    }
}

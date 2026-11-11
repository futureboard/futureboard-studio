//! Mute, solo, and mute automation used to freeze a track's compensation and
//! send delay lines. Un-muting then played the audio those rings held at the
//! moment of silence, for about the compensation length, and jumped to live.

use super::render_project_block_interleaved;
use crate::audio_file::AudioFileBuffer;
use crate::audio_source::ClipAudioSource;
use crate::runtime::{
    RuntimeAutomationCurve, RuntimeAutomationLane, RuntimeAutomationPoint, RuntimeAutomationTarget,
    RuntimeProject,
};
use crate::types::{
    EngineClipSnapshot, EngineProjectSnapshot, EngineRoutingSnapshot, EngineSendSnapshot,
    EngineTrackSnapshot,
};
use std::collections::HashMap;
use std::sync::Arc;

const SR: u32 = 48_000;
const BLOCK: usize = 64;
const DELAY: u32 = 256;
const BLOCKS: usize = 28;
/// Long enough that the delay ring is full of the live signal before silence.
const GAP_AT: usize = 12;

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

fn ramp_cache() -> HashMap<String, Arc<ClipAudioSource>> {
    let frames = SR as usize;
    let samples: Vec<f32> = (0..frames)
        .flat_map(|i| {
            let v = (i as f32 + 1.0) * 1.0e-5;
            [v, v]
        })
        .collect();
    let mut cache = HashMap::new();
    cache.insert(
        "ramp.wav".to_string(),
        Arc::new(ClipAudioSource::InMemory(Arc::new(AudioFileBuffer {
            sample_rate: SR,
            channels: 2,
            frames,
            samples,
        }))),
    );
    cache
}

fn clip_on(track_id: &str) -> EngineClipSnapshot {
    EngineClipSnapshot {
        id: format!("{track_id}-clip"),
        track_id: track_id.to_string(),
        asset_id: "ramp".to_string(),
        media_path: Some("ramp.wav".to_string()),
        start_beat: 0.0,
        duration_beats: 2.0,
        offset_seconds: 0.0,
        gain: 1.0,
        muted: false,
        ara_rendered: false,
        fades: None,
        stretch: Default::default(),
        audio_process: None,
    }
}

fn project(
    tracks: Vec<EngineTrackSnapshot>,
    clips: Vec<EngineClipSnapshot>,
) -> EngineProjectSnapshot {
    EngineProjectSnapshot {
        spatial: Default::default(),
        project_id: "pdc-silence".to_string(),
        project_root: None,
        preferred_input_device: None,
        bpm: 120.0,
        tempo_points: Vec::new(),
        time_signature: [4, 4],
        sample_rate: SR,
        tracks,
        clips,
        midi_clips: Vec::new(),
        pdc_enabled: true,
        latency_graph_version: 1,
        routing: EngineRoutingSnapshot {
            master_output_device: None,
            sample_rate: SR,
            buffer_size: 256,
        },
    }
}

fn build(
    tracks: Vec<EngineTrackSnapshot>,
    clips: Vec<EngineClipSnapshot>,
    cache: &mut HashMap<String, Arc<ClipAudioSource>>,
) -> RuntimeProject {
    let snapshot = project(tracks, clips);
    let mut runtime = RuntimeProject::build(&snapshot, SR, cache, None, true).expect("runtime");
    runtime.fader_smoothing = false;
    runtime
}

fn hold_track(runtime: &mut RuntimeProject, index: usize, delay: u32) {
    let cap = delay as usize + BLOCK;
    let track = &mut runtime.tracks[index];
    track.pdc_delay_l = vec![0.0; cap];
    track.pdc_delay_r = vec![0.0; cap];
    track.pdc_write_pos = 0;
    runtime.latency_graph.track_pdc_delay[index] = delay;
}

fn hold_send(runtime: &mut RuntimeProject, index: usize, slot: usize, delay: u32) {
    let cap = delay as usize + BLOCK;
    let line = &mut runtime.tracks[index].sends[slot].pdc;
    line.l = vec![0.0; cap];
    line.r = vec![0.0; cap];
    line.write_pos = 0;
    runtime.latency_graph.send_pdc_delay[index][slot] = delay;
}

fn render_left(
    runtime: &mut RuntimeProject,
    mut each: impl FnMut(usize, &mut RuntimeProject),
) -> Vec<f32> {
    let mut output = vec![0.0f32; BLOCK * 2];
    let mut left = Vec::with_capacity(BLOCKS * BLOCK);
    for block in 0..BLOCKS {
        each(block, runtime);
        render_project_block_interleaved(
            runtime,
            (block * BLOCK) as u64,
            1.0,
            &mut output,
            2,
            true,
            4,
            4,
            None,
        );
        left.extend(output.iter().step_by(2).copied());
    }
    left
}

/// The compensation ring is what this exercises. An undelayed mute/unmute
/// would resume in time too, and would not catch the stale-audio bug.
fn assert_output_is_delayed(rendered: &[f32]) {
    let delay = DELAY as usize;
    let lead = rendered[..delay]
        .iter()
        .fold(0.0f32, |peak, sample| peak.max(sample.abs()));
    assert!(
        lead < 1.0e-6,
        "the delay line should start empty, peak {lead}"
    );
    let at = delay + 800;
    let delayed = (at - delay + 1) as f32 * 1.0e-5;
    let undelayed = (at + 1) as f32 * 1.0e-5;
    assert!(
        (rendered[at] - delayed).abs() < 1.0e-5,
        "sample {at} is {} ; delayed {delayed} undelayed {undelayed}",
        rendered[at]
    );
    assert!(
        (rendered[at] - undelayed).abs() > 1.0e-3,
        "sample {at} was not held back by the compensation delay"
    );
}

fn assert_resumes_in_time(reference: &[f32], silenced: &[f32], gap_blocks: usize) {
    assert_output_is_delayed(reference);
    let gap_start = GAP_AT * BLOCK;
    let gap_end = gap_start + gap_blocks * BLOCK;
    let silent_peak = silenced[gap_start..gap_end]
        .iter()
        .fold(0.0f32, |peak, sample| peak.max(sample.abs()));
    let live_peak = reference[gap_start..gap_end]
        .iter()
        .fold(0.0f32, |peak, sample| peak.max(sample.abs()));
    assert!(
        silent_peak < 1.0e-5,
        "the silenced stretch must stay silent, peak {silent_peak}"
    );
    assert!(
        live_peak > 1.0e-3,
        "the reference must still be playing through the gap, peak {live_peak}"
    );
    let mut worst = 0.0f32;
    let mut at = gap_end;
    for (i, (got, expected)) in silenced[gap_end..]
        .iter()
        .zip(&reference[gap_end..])
        .enumerate()
    {
        let err = (got - expected).abs();
        if err > worst {
            worst = err;
            at = gap_end + i;
        }
    }
    assert!(
        worst < 1.0e-5,
        "audio after the gap diverged by {worst} at sample {at} (got {}, expected {})",
        silenced[at],
        reference[at]
    );
}

fn pair(
    make: impl Fn(&mut HashMap<String, Arc<ClipAudioSource>>) -> RuntimeProject,
) -> (RuntimeProject, RuntimeProject) {
    let mut cache = ramp_cache();
    let reference = make(&mut cache);
    let subject = make(&mut cache);
    (reference, subject)
}

fn compensated_source() -> (Vec<EngineTrackSnapshot>, Vec<EngineClipSnapshot>) {
    (
        vec![
            track("audio", "audio"),
            track("other", "audio"),
            track("master", "master"),
        ],
        vec![clip_on("audio")],
    )
}

#[test]
fn a_muted_compensated_track_resumes_in_time() {
    // Longer than the delay, so a ring that merely filled with silence
    // would still be empty when the mute lifts.
    let gap = 8;
    let (mut reference, mut subject) = pair(|cache| {
        let (tracks, clips) = compensated_source();
        let mut runtime = build(tracks, clips, cache);
        hold_track(&mut runtime, 0, DELAY);
        runtime
    });
    let expected = render_left(&mut reference, |_, _| {});
    let got = render_left(&mut subject, |block, runtime| {
        runtime.update_track_mute_at(0, (GAP_AT..GAP_AT + gap).contains(&block));
    });
    assert_resumes_in_time(&expected, &got, gap);
}

#[test]
fn a_short_mute_does_not_replay_the_delay_line() {
    // Shorter than the delay. Freezing the ring leaves pre-mute audio in
    // it; writing silence through the mute leaves a hole afterwards. The
    // live signal has to keep moving the whole time.
    let gap = 1;
    assert!(gap * BLOCK < DELAY as usize);
    let (mut reference, mut subject) = pair(|cache| {
        let (tracks, clips) = compensated_source();
        let mut runtime = build(tracks, clips, cache);
        hold_track(&mut runtime, 0, DELAY);
        runtime
    });
    let expected = render_left(&mut reference, |_, _| {});
    let got = render_left(&mut subject, |block, runtime| {
        runtime.update_track_mute_at(0, (GAP_AT..GAP_AT + gap).contains(&block));
    });
    assert_resumes_in_time(&expected, &got, gap);
}

#[test]
fn unsoloing_brings_compensated_tracks_back_in_time() {
    let gap = 6;
    let (mut reference, mut subject) = pair(|cache| {
        let (tracks, clips) = compensated_source();
        let mut runtime = build(tracks, clips, cache);
        hold_track(&mut runtime, 0, DELAY);
        runtime
    });
    let expected = render_left(&mut reference, |_, _| {});
    let got = render_left(&mut subject, |block, runtime| {
        runtime.update_track_solo_at(1, (GAP_AT..GAP_AT + gap).contains(&block));
    });
    assert_resumes_in_time(&expected, &got, gap);
}

#[test]
fn mute_automation_does_not_freeze_compensation() {
    let gap = 4;
    let (mut reference, mut subject) = pair(|cache| {
        let (tracks, clips) = compensated_source();
        let mut runtime = build(tracks, clips, cache);
        hold_track(&mut runtime, 0, DELAY);
        runtime.tracks[0]
            .automation_lanes
            .push(RuntimeAutomationLane {
                id: "mute".to_string(),
                name: "Mute".to_string(),
                target: RuntimeAutomationTarget::TrackMute,
                enabled: true,
                points: vec![RuntimeAutomationPoint {
                    beat: 0.0,
                    value: 0.0,
                    curve: RuntimeAutomationCurve::Hold,
                    tension: 0.0,
                }],
            });
        runtime
    });
    let expected = render_left(&mut reference, |_, _| {});
    let got = render_left(&mut subject, |block, runtime| {
        let muted = (GAP_AT..GAP_AT + gap).contains(&block);
        runtime.tracks[0].automation_lanes[0].points[0].value = if muted { 1.0 } else { 0.0 };
    });
    assert_resumes_in_time(&expected, &got, gap);
}

#[test]
fn a_muted_tracks_delayed_send_resumes_in_time() {
    let gap = 6;
    let (mut reference, mut subject) = pair(|cache| {
        let mut src = track("src", "audio");
        src.sends = vec![EngineSendSnapshot {
            id: "s".to_string(),
            return_track_id: "verb".to_string(),
            level: 1.0,
            enabled: true,
            pre_fader: true,
            wdw: false,
        }];
        let mut runtime = build(
            vec![src, track("verb", "return"), track("master", "master")],
            vec![clip_on("src")],
            cache,
        );
        // Dry path closed, so the master is only the delayed pre-fader send.
        runtime.tracks[0].volume = 0.0;
        hold_send(&mut runtime, 0, 0, DELAY);
        runtime
    });
    let expected = render_left(&mut reference, |_, _| {});
    let got = render_left(&mut subject, |block, runtime| {
        runtime.update_track_mute_at(0, (GAP_AT..GAP_AT + gap).contains(&block));
    });
    assert_resumes_in_time(&expected, &got, gap);
}

#[test]
fn a_muted_compensated_bus_resumes_in_time() {
    let gap = 6;
    let (mut reference, mut subject) = pair(|cache| {
        let mut src = track("src", "audio");
        src.output_track_id = Some("bus".to_string());
        let mut runtime = build(
            vec![src, track("bus", "bus"), track("master", "master")],
            vec![clip_on("src")],
            cache,
        );
        hold_track(&mut runtime, 1, DELAY);
        runtime
    });
    let expected = render_left(&mut reference, |_, _| {});
    let got = render_left(&mut subject, |block, runtime| {
        runtime.update_track_mute_at(1, (GAP_AT..GAP_AT + gap).contains(&block));
    });
    assert_resumes_in_time(&expected, &got, gap);
}

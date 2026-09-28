//! Acceptance tests for the continuous-pitch path: does a drawn pitch curve
//! actually change the frequency the Solfege instrument sounds?
//!
//! Everything here renders **real audio** through the ordinary project graph
//! and then *measures* the result, because the only failure that matters is the
//! silent one: an event list that looks correct, a snapshot that carries the
//! right numbers, and an output whose pitch never moved. A test that asserts on
//! the event list instead of on the waveform cannot see that.
//!
//! The instrument here is the model-free `BowedString` fallback that
//! `RuntimeSolfegeEngine` prepares when a track has no `.sfm`, so these tests
//! need no 146 MB voicebank on disk and still exercise the identical
//! snapshot → runtime → scheduler → engine → audio path a loaded model uses.

use std::collections::HashMap;

use super::render::{render_project_block_interleaved, schedule_midi_render_block};
use crate::runtime::RuntimeProject;
use crate::types::{
    EngineMidiClipSnapshot, EngineMidiNoteSnapshot, EnginePitchPoint, EngineProjectSnapshot,
    EngineRoutingSnapshot, EngineSolfegeSnapshot, EngineTrackInputSourceSnapshot,
    EngineTrackSnapshot,
};

const SR: u32 = 48_000;
const TRACK: &str = "solfege-1";

/// A4. The acceptance test in the brief is stated in these terms: 440 Hz must
/// become 466.16 Hz when the curve is drawn to +100 cents.
const A4_HZ: f32 = 440.0;
const A4_PLUS_100_CENTS_HZ: f32 = 466.163_76;

fn track(id: &str, track_type: &str, solfege: bool) -> EngineTrackSnapshot {
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
        input_source: EngineTrackInputSourceSnapshot {
            device_id: None,
            channels: Vec::new(),
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
        // `model_path: None` selects the built-in bowed-string physical
        // instrument, which honours `Event::Pitch` exactly as the voicebank
        // does. That is what keeps this test hermetic.
        solfege_engine: solfege.then(|| EngineSolfegeSnapshot {
            model_path: None,
            instrument: "Violin".to_string(),
            voice: "Solo Bowed String".to_string(),
            preset: "Test".to_string(),
            bow_pressure: 0.62,
            vibrato: 0.0,
            dynamics: 0.78,
            expression: 1.0,
        }),
    }
}

fn snapshot(notes: Vec<EngineMidiNoteSnapshot>) -> EngineProjectSnapshot {
    EngineProjectSnapshot {
        spatial: Default::default(),
        project_id: "solfege-pitch-test".to_string(),
        project_root: None,
        preferred_input_device: None,
        bpm: 120.0,
        tempo_points: Vec::new(),
        time_signature: [4, 4],
        sample_rate: SR,
        tracks: vec![
            track(TRACK, "instrument", true),
            track("master", "master", false),
        ],
        clips: Vec::new(),
        midi_clips: vec![EngineMidiClipSnapshot {
            id: "clip-1".to_string(),
            track_id: TRACK.to_string(),
            start_beat: 0.0,
            length_beats: 8.0,
            notes,
            controllers: Vec::new(),
            mpe: sphere_midi_service::mpe::MpeTrackConfiguration::default(),
        }],
        pdc_enabled: false,
        latency_graph_version: 1,
        routing: EngineRoutingSnapshot {
            master_output_device: None,
            sample_rate: SR,
            buffer_size: 256,
        },
    }
}

fn note(pitch: u8, start_beat: f64, length_beats: f64) -> EngineMidiNoteSnapshot {
    EngineMidiNoteSnapshot {
        id: 1,
        pitch,
        start_beat,
        length_beats,
        velocity: 100,
        channel: 0,
        expression: sphere_midi_service::NoteExpression::default(),
        articulation: None,
        pitch_points: Vec::new(),
    }
}

/// Render `seconds` of the project as mono, starting from the transport origin.
fn render_mono(snapshot: &EngineProjectSnapshot, seconds: f32, block: usize) -> Vec<f32> {
    let mut runtime = RuntimeProject::build(snapshot, SR, &mut HashMap::new(), None, true)
        .expect("runtime builds");
    let total = (seconds * SR as f32) as usize;
    let mut mono = Vec::with_capacity(total);
    let mut buffer = vec![0.0f32; block * 2];
    let mut base = 0u64;
    while mono.len() < total {
        let frames = block.min(total - mono.len());
        buffer[..frames * 2].fill(0.0);
        // Mirrors the audio callback: clear last block's events, schedule this
        // block's, then render. Doing it here rather than inside the render
        // function is what the real engine does, so the test exercises the same
        // ordering — including the point where a pitch event could be dropped.
        for track in &mut runtime.tracks {
            track.midi_block_events.clear();
            track.solfege_pitch_events.clear();
            track.solfege_articulation_events.clear();
        }
        schedule_midi_render_block(&mut runtime, base, frames as u64, None);
        render_project_block_interleaved(
            &mut runtime,
            base,
            1.0,
            &mut buffer[..frames * 2],
            2,
            true,
            4,
            4,
            None,
        );
        for frame in buffer[..frames * 2].chunks(2) {
            mono.push((frame[0] + frame[1]) * 0.5);
        }
        base += frames as u64;
    }
    mono
}

/// Fundamental of `samples` by normalised autocorrelation, with parabolic
/// interpolation on the peak.
///
/// Written out rather than imported so the acceptance criterion does not depend
/// on a analysis crate: the number this returns is the number the test asserts
/// on, and it must be readable. Returns `None` when the window carries no
/// confident periodicity, so silence cannot masquerade as a pitch.
fn estimate_hz(samples: &[f32], sample_rate: u32, fmin: f32, fmax: f32) -> Option<f32> {
    let min_lag = (sample_rate as f32 / fmax).floor().max(2.0) as usize;
    let max_lag = (sample_rate as f32 / fmin).ceil() as usize;
    if samples.len() < max_lag * 2 {
        return None;
    }
    let mean = samples.iter().sum::<f32>() / samples.len() as f32;
    let centred: Vec<f64> = samples.iter().map(|s| (s - mean) as f64).collect();
    let energy: f64 = centred.iter().map(|s| s * s).sum();
    if energy < 1e-12 {
        return None;
    }

    let window = centred.len() - max_lag;
    let mut best = (0usize, 0.0f64);
    let mut nsdf = vec![0.0f64; max_lag + 2];
    for lag in min_lag..=max_lag {
        let mut correlation = 0.0;
        let mut head = 0.0;
        let mut tail = 0.0;
        for i in 0..window {
            let a = centred[i];
            let b = centred[i + lag];
            correlation += a * b;
            head += a * a;
            tail += b * b;
        }
        let value = 2.0 * correlation / (head + tail).max(1e-20);
        nsdf[lag] = value;
        if value > best.1 {
            best = (lag, value);
        }
    }
    if best.1 < 0.3 {
        return None;
    }
    // Take the first peak reaching 80 % of the best, not the best itself: a
    // bowed string's second harmonic routinely beats its fundamental, and the
    // octave error that causes would make this test assert the wrong thing.
    let threshold = 0.8 * best.1;
    let mut lag = min_lag;
    while lag < max_lag && nsdf[lag] < threshold {
        lag += 1;
    }
    while lag + 1 <= max_lag && nsdf[lag + 1] > nsdf[lag] {
        lag += 1;
    }
    if lag <= min_lag || lag >= max_lag {
        return None;
    }
    let (y0, y1, y2) = (nsdf[lag - 1], nsdf[lag], nsdf[lag + 1]);
    let denom = y0 - 2.0 * y1 + y2;
    let shift = if denom.abs() > 1e-20 {
        0.5 * (y0 - y2) / denom
    } else {
        0.0
    };
    Some(sample_rate as f32 / (lag as f64 + shift) as f32)
}

fn cents(measured: f32, reference: f32) -> f32 {
    1200.0 * (measured / reference).log2()
}

/// Sample `samples` over `[from, to)` seconds.
fn window(samples: &[f32], from: f32, to: f32) -> &[f32] {
    let start = ((from * SR as f32) as usize).min(samples.len());
    let end = ((to * SR as f32) as usize).min(samples.len());
    &samples[start..end]
}

/// The headline acceptance test from the brief.
///
/// A4 with a curve drawn from 0 to +100 cents must *sound* a semitone higher by
/// the end of the note. Measured on the rendered waveform, not on the event
/// list.
#[test]
fn a_drawn_pitch_curve_changes_the_rendered_frequency() {
    let mut n = note(69, 0.0, 8.0);
    // Flat for two beats so the bow attack settles at the notated pitch, then
    // a ramp to +100 cents across beats 2..4, then hold. At 120 BPM that is
    // flat to 1.0 s, ramping to 2.0 s, held after.
    n.pitch_points = vec![
        EnginePitchPoint {
            beat: 0.0,
            hz: A4_HZ,
        },
        EnginePitchPoint {
            beat: 2.0,
            hz: A4_HZ,
        },
        EnginePitchPoint {
            beat: 4.0,
            hz: A4_PLUS_100_CENTS_HZ,
        },
        EnginePitchPoint {
            beat: 8.0,
            hz: A4_PLUS_100_CENTS_HZ,
        },
    ];
    let audio = render_mono(&snapshot(vec![n]), 3.0, 128);

    // Measured after the attack has settled (the first ~0.5 s is bow noise
    // with no stable period) and well after the ramp has arrived.
    let before = estimate_hz(window(&audio, 0.7, 1.0), SR, 200.0, 1200.0)
        .expect("the note sounds before the ramp");
    let after = estimate_hz(window(&audio, 2.4, 3.0), SR, 200.0, 1200.0)
        .expect("the note still sounds after the ramp");

    assert!(
        cents(before, A4_HZ).abs() < 15.0,
        "before the ramp the note must sound at its notated pitch: {before:.2} Hz \
         ({:+.1} cents from {A4_HZ})",
        cents(before, A4_HZ)
    );
    assert!(
        cents(after, A4_PLUS_100_CENTS_HZ).abs() < 15.0,
        "after the ramp the note must sound a semitone higher: {after:.2} Hz \
         ({:+.1} cents from {A4_PLUS_100_CENTS_HZ}); the drawn curve did not reach the engine",
        cents(after, A4_PLUS_100_CENTS_HZ)
    );
    assert!(
        cents(after, before) > 85.0,
        "the rendered pitch must actually move by ~100 cents, moved {:+.1}",
        cents(after, before)
    );
}

/// The negative control. Without this, the test above could pass on an engine
/// that always drifts upward.
#[test]
fn a_note_with_no_curve_holds_its_notated_pitch() {
    let audio = render_mono(&snapshot(vec![note(69, 0.0, 8.0)]), 3.0, 128);
    let early = estimate_hz(window(&audio, 0.7, 1.0), SR, 200.0, 1200.0).expect("sounds early");
    let late = estimate_hz(window(&audio, 2.4, 3.0), SR, 200.0, 1200.0).expect("sounds late");
    assert!(
        cents(early, A4_HZ).abs() < 15.0 && cents(late, A4_HZ).abs() < 15.0,
        "an untouched note must stay at 440 Hz: {early:.2} Hz then {late:.2} Hz"
    );
}

/// Block-size independence (brief item 53). The scheduler splits each block at
/// event offsets, so a pitch point landing mid-block must be applied at its own
/// offset rather than at the block edge. If it were not, the rendered pitch
/// would depend on the buffer size — audible as a different performance every
/// time the user changes their audio device.
#[test]
fn the_rendered_pitch_is_the_same_at_every_block_size() {
    let mut n = note(69, 0.0, 8.0);
    n.pitch_points = vec![
        EnginePitchPoint {
            beat: 0.0,
            hz: A4_HZ,
        },
        EnginePitchPoint {
            beat: 2.0,
            hz: A4_HZ,
        },
        EnginePitchPoint {
            beat: 4.0,
            hz: A4_PLUS_100_CENTS_HZ,
        },
    ];
    let snapshot = snapshot(vec![n]);
    let mut measured = Vec::new();
    for block in [32usize, 64, 128, 256] {
        let audio = render_mono(&snapshot, 3.0, block);
        let hz = estimate_hz(window(&audio, 2.4, 3.0), SR, 200.0, 1200.0)
            .unwrap_or_else(|| panic!("block {block} produced no measurable pitch"));
        measured.push((block, hz));
    }
    let reference = measured[0].1;
    for (block, hz) in &measured {
        assert!(
            cents(*hz, reference).abs() < 5.0,
            "block size {block} rendered {hz:.3} Hz vs {reference:.3} Hz at block 32 \
             ({:+.2} cents) — pitch events are being quantised to the block edge",
            cents(*hz, reference)
        );
    }
}

/// A pitch point may only address the note it belongs to. A curve that outlived
/// its note would retune whatever voice next reuses the same note id, which is
/// the classic way continuous pitch leaks between notes.
#[test]
fn a_curve_does_not_leak_into_the_following_note() {
    let mut first = note(69, 0.0, 1.0);
    first.pitch_points = vec![
        EnginePitchPoint {
            beat: 0.0,
            hz: A4_HZ,
        },
        // Deliberately extends far past the note's own end.
        EnginePitchPoint {
            beat: 6.0,
            hz: A4_HZ * 2.0,
        },
    ];
    let mut second = note(69, 2.0, 6.0);
    second.id = 2;
    let audio = render_mono(&snapshot(vec![first, second]), 4.0, 128);
    // Beat 2 = 1.0 s at 120 BPM; measure past the second note's own attack.
    let hz = estimate_hz(window(&audio, 2.0, 2.8), SR, 200.0, 1200.0).expect("second note sounds");
    assert!(
        cents(hz, A4_HZ).abs() < 25.0,
        "the second note must sound at its own notated pitch, got {hz:.2} Hz \
         ({:+.1} cents) — the previous note's curve leaked",
        cents(hz, A4_HZ)
    );
}

/// A long render must not accumulate state damage: no NaN, no runaway, no DC
/// crawl (brief item 54).
#[test]
fn a_long_continuous_render_stays_healthy() {
    let mut n = note(69, 0.0, 64.0);
    // A moving target for the whole render, so the smoothing and voice state
    // are continuously exercised rather than parked on one value.
    n.pitch_points = (0..=64)
        .map(|i| EnginePitchPoint {
            beat: i as f64 * 0.5,
            hz: A4_HZ * (1.0 + 0.03 * ((i as f32) * 0.7).sin()),
        })
        .collect();
    let audio = render_mono(&snapshot(vec![n]), 30.0, 128);

    assert!(
        audio.iter().all(|s| s.is_finite()),
        "long render produced NaN or Inf"
    );
    let peak = audio.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    assert!(peak < 4.0, "long render ran away to peak {peak}");

    // DC must not grow across the render: compare the first and last thirds.
    let third = audio.len() / 3;
    let dc = |slice: &[f32]| slice.iter().sum::<f32>() / slice.len().max(1) as f32;
    let (head, tail) = (dc(&audio[..third]), dc(&audio[audio.len() - third..]));
    assert!(
        (tail.abs() - head.abs()).abs() < 0.05,
        "DC offset drifted across a 30 s render: {head:.5} then {tail:.5}"
    );
}

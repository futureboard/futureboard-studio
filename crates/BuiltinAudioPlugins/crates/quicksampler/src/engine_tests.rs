use super::*;

const SR: u32 = 48_000;

/// A sine at `hz`, `seconds` long, at the output rate.
fn sine(hz: f32, seconds: f32) -> Arc<SampleData> {
    let frames = (SR as f32 * seconds) as usize;
    let samples: Vec<f32> = (0..frames)
        .map(|i| (2.0 * std::f32::consts::PI * hz * i as f32 / SR as f32).sin() * 0.5)
        .collect();
    Arc::new(SampleData::from_interleaved(&samples, 1, SR).unwrap())
}

/// A ramp from 0 to 1 over the sample, to tell direction and position apart.
fn ramp(frames: usize) -> Arc<SampleData> {
    let samples: Vec<f32> = (0..frames).map(|i| i as f32 / frames as f32).collect();
    Arc::new(SampleData::from_interleaved(&samples, 1, SR).unwrap())
}

fn params() -> QuickSamplerParams {
    QuickSamplerParams {
        volume: 1.0,
        release_ms: 0.0,
        ..QuickSamplerParams::default()
    }
}

fn render(sampler: &mut QuickSampler, frames: usize) -> (Vec<f32>, Vec<f32>) {
    let mut left = vec![0.0; frames];
    let mut right = vec![0.0; frames];
    sampler.render(&mut left, &mut right);
    (left, right)
}

/// Frequency from rising zero crossings.
fn frequency(signal: &[f32]) -> f32 {
    let crossings: Vec<usize> = (1..signal.len())
        .filter(|&i| signal[i - 1] < 0.0 && signal[i] >= 0.0)
        .collect();
    let span = (crossings[crossings.len() - 1] - crossings[0]) as f32;
    (crossings.len() - 1) as f32 * SR as f32 / span
}

#[test]
fn a_note_plays_the_sample_pitched_from_its_root() {
    let mut sampler = QuickSampler::new(SR, Some(sine(440.0, 1.0)), params());
    sampler.note_on(0, 72, 127); // an octave over the root
    let (left, _) = render(&mut sampler, 4_800);
    let hz = frequency(&left[200..]);
    assert!((hz - 880.0).abs() < 2.0, "{hz}");
}

#[test]
fn without_keytracking_every_key_plays_the_sample_as_recorded() {
    let p = QuickSamplerParams {
        keytrack: false,
        ..params()
    };
    let mut sampler = QuickSampler::new(SR, Some(sine(440.0, 1.0)), p);
    sampler.note_on(0, 30, 127);
    let (left, _) = render(&mut sampler, 4_800);
    assert!((frequency(&left[200..]) - 440.0).abs() < 2.0);
}

#[test]
fn transpose_fine_tune_and_pitch_bend_all_move_the_pitch() {
    let p = QuickSamplerParams {
        transpose: 12,
        fine_cents: 0.0,
        ..params()
    };
    let mut sampler = QuickSampler::new(SR, Some(sine(440.0, 1.0)), p);
    sampler.note_on(0, 60, 127);
    let (left, _) = render(&mut sampler, 4_800);
    assert!((frequency(&left[200..]) - 880.0).abs() < 2.0);

    // Full bend up at a 2-semitone range: 440 × 2^(2/12) ≈ 493.9 Hz.
    let mut sampler = QuickSampler::new(SR, Some(sine(440.0, 1.0)), params());
    sampler.controller(0, CONTROLLER_PITCH_BEND, 127);
    sampler.note_on(0, 60, 127);
    let (left, _) = render(&mut sampler, 4_800);
    let hz = frequency(&left[200..]);
    assert!(
        (hz - 440.0 * 2f32.powf(2.0 * 63.0 / 64.0 / 12.0)).abs() < 2.0,
        "{hz}"
    );
}

#[test]
fn a_one_shot_ends_with_its_sample_and_frees_the_voice() {
    let mut sampler = QuickSampler::new(SR, Some(sine(440.0, 0.01)), params());
    sampler.note_on(0, 60, 127);
    let (left, _) = render(&mut sampler, 1_000);
    assert!(left[100..400].iter().any(|v| v.abs() > 0.1));
    assert!(left[480..].iter().all(|v| *v == 0.0));
    assert_eq!(sampler.active_voice_count(), 0);
}

#[test]
fn a_loop_keeps_sounding_past_the_end_until_release() {
    let p = QuickSamplerParams {
        loop_mode: LoopMode::Forward,
        loop_start: 0.25,
        loop_end: 0.75,
        ..params()
    };
    let mut sampler = QuickSampler::new(SR, Some(sine(440.0, 0.01)), p);
    sampler.note_on(0, 60, 127);
    let (left, _) = render(&mut sampler, 4_800);
    assert!(left[4_000..].iter().any(|v| v.abs() > 0.1), "still looping");
    sampler.note_off(0, 60);
    let _ = render(&mut sampler, 480);
    assert_eq!(sampler.active_voice_count(), 0);
}

#[test]
fn ping_pong_turns_round_at_the_loop_end() {
    let p = QuickSamplerParams {
        loop_mode: LoopMode::PingPong,
        loop_start: 0.5,
        loop_end: 1.0,
        attack_ms: 0.0,
        ..params()
    };
    let mut sampler = QuickSampler::new(SR, Some(ramp(1_000)), p);
    sampler.note_on(0, 60, 127);
    let (left, _) = render(&mut sampler, 1_400);
    // Rising to the end, then falling back towards the loop start.
    assert!(left[990] > left[900]);
    assert!(left[1_300] < left[1_100]);
    assert!(left[1_300] > 0.45);
}

#[test]
fn reverse_plays_from_the_end_point_backwards() {
    let p = QuickSamplerParams {
        reverse: true,
        ..params()
    };
    let mut sampler = QuickSampler::new(SR, Some(ramp(1_000)), p);
    sampler.note_on(0, 60, 127);
    let (left, _) = render(&mut sampler, 900);
    assert!(left[50] > 0.9 && left[800] < left[100]);
}

#[test]
fn the_start_point_skips_into_the_sample() {
    let p = QuickSamplerParams {
        start: 0.5,
        ..params()
    };
    let mut sampler = QuickSampler::new(SR, Some(ramp(1_000)), p);
    sampler.note_on(0, 60, 127);
    let (left, _) = render(&mut sampler, 100);
    assert!((left[50] - 0.55).abs() < 0.01, "{}", left[50]);
}

#[test]
fn polyphony_one_hands_over_to_the_newest_note() {
    let p = QuickSamplerParams {
        polyphony: 1,
        ..params()
    };
    let mut sampler = QuickSampler::new(SR, Some(sine(440.0, 1.0)), p);
    sampler.note_on(0, 60, 127);
    let _ = render(&mut sampler, 64);
    sampler.note_on(0, 64, 127);
    let _ = render(&mut sampler, 480);
    assert_eq!(sampler.active_voice_count(), 1);
}

#[test]
fn the_sustain_pedal_holds_released_notes_until_it_lifts() {
    let mut sampler = QuickSampler::new(SR, Some(sine(440.0, 1.0)), params());
    sampler.controller(0, 64, 127);
    sampler.note_on(0, 60, 127);
    sampler.note_off(0, 60);
    let _ = render(&mut sampler, 480);
    assert_eq!(sampler.active_voice_count(), 1);
    sampler.controller(0, 64, 0);
    let _ = render(&mut sampler, 480);
    assert_eq!(sampler.active_voice_count(), 0);
}

#[test]
fn velocity_scales_the_level_by_its_amount() {
    let peak = |velocity: u8, amount: f32| {
        let p = QuickSamplerParams {
            velocity: amount,
            ..params()
        };
        let mut sampler = QuickSampler::new(SR, Some(sine(440.0, 1.0)), p);
        sampler.note_on(0, 60, velocity);
        let (left, _) = render(&mut sampler, 960);
        left.iter().fold(0.0_f32, |m, v| m.max(v.abs()))
    };
    assert!((peak(64, 1.0) / peak(127, 1.0) - 64.0 / 127.0).abs() < 0.02);
    assert!((peak(64, 0.0) - peak(127, 0.0)).abs() < 1.0e-4);
}

#[test]
fn a_low_pass_takes_the_top_off() {
    let energy = |filter: FilterMode| {
        let p = QuickSamplerParams {
            filter,
            cutoff_hz: 500.0,
            ..params()
        };
        let mut sampler = QuickSampler::new(SR, Some(sine(5_000.0, 1.0)), p);
        sampler.note_on(0, 60, 127);
        let (left, _) = render(&mut sampler, 4_800);
        left[480..].iter().map(|v| v * v).sum::<f32>()
    };
    assert!(energy(FilterMode::LowPass) < energy(FilterMode::Off) * 0.05);
}

#[test]
fn pan_and_volume_set_each_side() {
    let p = QuickSamplerParams {
        pan: -1.0,
        ..params()
    };
    let mut sampler = QuickSampler::new(SR, Some(sine(440.0, 1.0)), p);
    sampler.note_on(0, 60, 127);
    let (left, right) = render(&mut sampler, 480);
    assert!(left.iter().any(|v| v.abs() > 0.3));
    assert!(right.iter().all(|v| *v == 0.0));
}

#[test]
fn normalize_brings_a_quiet_sample_to_full_scale() {
    let p = QuickSamplerParams {
        normalize: true,
        ..params()
    };
    let mut sampler = QuickSampler::new(SR, Some(sine(440.0, 1.0)), p);
    sampler.note_on(0, 60, 127);
    let (left, _) = render(&mut sampler, 960);
    let peak = left.iter().fold(0.0_f32, |m, v| m.max(v.abs()));
    assert!((peak - 1.0).abs() < 0.02, "{peak}");
}

#[test]
fn without_a_sample_it_is_silent_and_holds_no_voices() {
    let mut sampler = QuickSampler::new(SR, None, params());
    sampler.note_on(0, 60, 127);
    let (left, right) = render(&mut sampler, 64);
    assert!(left.iter().chain(&right).all(|v| *v == 0.0));
    assert_eq!(sampler.active_voice_count(), 0);
}

#[test]
fn a_region_note_plays_only_its_slice_at_the_sample_pitch() {
    // Root C4 and key tracking on: a slice ignores both.
    let mut sampler = QuickSampler::new(SR, Some(ramp(48_000)), params());
    sampler.note_on_region(0, 90, 127, 0.5, 0.75);
    let (left, _) = render(&mut sampler, 24_000);
    assert!((left[100] - 0.5).abs() < 0.01, "{}", left[100]);
    assert!((left[11_000] - (0.5 + 11_000.0 / 48_000.0)).abs() < 0.01);
    // A quarter of the sample at its own speed, then silence.
    assert_eq!(left[12_100], 0.0);
    assert_eq!(sampler.active_voice_count(), 0);
}

#[test]
fn a_region_note_never_loops() {
    let p = QuickSamplerParams {
        loop_mode: LoopMode::Forward,
        loop_start: 0.0,
        loop_end: 1.0,
        ..params()
    };
    let mut sampler = QuickSampler::new(SR, Some(ramp(4_800)), p);
    sampler.note_on_region(0, 60, 127, 0.0, 0.5);
    render(&mut sampler, 4_800);
    assert_eq!(sampler.active_voice_count(), 0);
}

#[test]
fn a_choke_fades_every_voice_out_quickly() {
    let p = QuickSamplerParams {
        release_ms: 5_000.0,
        ..params()
    };
    let mut sampler = QuickSampler::new(SR, Some(sine(220.0, 2.0)), p);
    sampler.note_on(0, 60, 127);
    sampler.note_on(0, 64, 127);
    render(&mut sampler, 256);
    sampler.choke();
    let (left, _) = render(&mut sampler, 480); // 10 ms
    assert_eq!(sampler.active_voice_count(), 0);
    assert_eq!(left[479], 0.0);
}

#[test]
fn sounding_voices_are_listed_newest_first_with_their_positions() {
    let mut sampler = QuickSampler::new(SR, Some(ramp(48_000)), params());
    let mut out = [(0_u8, 0_u64, 0.0_f32); 2];
    assert_eq!(sampler.sounding(&mut out), 0);
    sampler.note_on_region(0, 50, 127, 0.0, 1.0);
    sampler.note_on_region(0, 51, 127, 0.5, 1.0);
    sampler.note_on_region(0, 52, 127, 0.25, 1.0);
    render(&mut sampler, 4_800);
    // Three sounding, room for two: the newest two.
    assert_eq!(sampler.sounding(&mut out), 2);
    assert_eq!((out[0].0, out[1].0), (52, 51));
    assert!(out[0].1 > out[1].1);
    assert!((out[0].2 - 0.35).abs() < 1.0e-3, "{}", out[0].2);
    assert!((out[1].2 - 0.6).abs() < 1.0e-3, "{}", out[1].2);
    // A choked voice is on its way out: not sounding.
    sampler.choke();
    assert_eq!(sampler.sounding(&mut out), 0);
}

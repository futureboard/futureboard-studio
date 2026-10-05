use super::*;

const SR: f32 = 48_000.0;

fn dsp_with(params: Params) -> Dsp {
    let mut dsp = Dsp::new(SR);
    dsp.set_params(params);
    dsp.reset();
    dsp
}

/// A voice-like tone: a fundamental at `hz(t)` and falling harmonics.
fn sing(dsp: &mut Dsp, seconds: f32, hz: impl Fn(f32) -> f32) -> Vec<f32> {
    let frames = (SR * seconds) as usize;
    let mut phase = 0.0f32;
    let mut out = Vec::with_capacity(frames);
    for n in 0..frames {
        let t = n as f32 / SR;
        phase = (phase + hz(t) / SR).fract();
        let x = (1..5)
            .map(|k| (std::f32::consts::TAU * phase * k as f32).sin() / k as f32)
            .sum::<f32>()
            * 0.25;
        let (l, r) = dsp.process_stereo(x, x);
        assert!(l.is_finite() && r.is_finite(), "non-finite at {n}");
        out.push(l);
    }
    out
}

/// The pitch of `signal` between `from` and `to` seconds, in MIDI notes,
/// measured with the crate's own detector run on the output.
fn pitch_of(signal: &[f32], from: f32, to: f32) -> f32 {
    let mut detector = detect::Detector::new(SR, 30.0);
    detector.set_range(50.0, 1_500.0);
    let mut periods = Vec::new();
    for (n, x) in signal.iter().enumerate() {
        if let Some(estimate) = detector.push(*x, 0.2) {
            let t = n as f32 / SR;
            if (from..to).contains(&t) {
                periods.extend(estimate.period);
            }
        }
    }
    assert!(!periods.is_empty(), "no pitch between {from} and {to} s");
    periods.sort_by(f32::total_cmp);
    midi_of(SR / periods[periods.len() / 2], 440.0)
}

fn hz_of(midi: f32) -> f32 {
    440.0 * 2f32.powf((midi - 69.0) / 12.0)
}

#[test]
fn descriptor_ids_are_unique_and_match_defaults() {
    let d = descriptor();
    assert_eq!(d.params.len(), ipc::PARAM_COUNT);
    let defaults = ipc::ui_values(&default_params());
    for (param, (id, value)) in d.params.iter().zip(&defaults) {
        assert_eq!(param.id, *id);
        assert!((param.default_value - value).abs() < 1.0e-6, "{id}");
        assert!(param.min <= *value && *value <= param.max, "{id}");
    }
}

#[test]
fn a_flat_note_is_pulled_onto_pitch() {
    for (sung, expected) in [(69.3f32, 69.0f32), (57.6, 58.0), (64.45, 64.0)] {
        let mut params = default_params();
        params.retune_ms = 0.0;
        let mut dsp = dsp_with(params);
        let out = sing(&mut dsp, 1.0, |_| hz_of(sung));
        let heard = pitch_of(&out, 0.5, 1.0);
        assert!(
            (heard - expected).abs() < 0.05,
            "sang {sung}, heard {heard}, wanted {expected}"
        );
    }
}

#[test]
fn the_scale_decides_the_target_and_removed_notes_are_skipped() {
    // C# + 30 c in C major goes to D.
    let mut params = default_params();
    params.retune_ms = 0.0;
    params.scale = Scale::Major;
    let mut dsp = dsp_with(params.clone());
    let out = sing(&mut dsp, 1.0, |_| hz_of(61.3));
    assert!((pitch_of(&out, 0.5, 1.0) - 62.0).abs() < 0.05);

    // With D removed, C is the nearest note left.
    params.remove_mask = 1 << 2;
    let mut dsp = dsp_with(params);
    let out = sing(&mut dsp, 1.0, |_| hz_of(61.3));
    assert!((pitch_of(&out, 0.5, 1.0) - 60.0).abs() < 0.05);
}

#[test]
fn a_bypassed_note_is_left_alone() {
    let mut params = default_params();
    params.retune_ms = 0.0;
    params.bypass_mask = 1 << 9; // A
    let mut dsp = dsp_with(params);
    let out = sing(&mut dsp, 1.0, |_| hz_of(69.3));
    assert!((pitch_of(&out, 0.5, 1.0) - 69.3).abs() < 0.05);
}

#[test]
fn retune_speed_sets_how_fast_the_pitch_arrives() {
    let mut params = default_params();
    params.retune_ms = 150.0;
    let mut dsp = dsp_with(params);
    // A fifth of a semitone sharp from the start; the delay is latency.
    let out = sing(&mut dsp, 1.6, |_| hz_of(69.2));
    let latency = dsp.latency_samples() as f32 / SR;
    let early = pitch_of(&out, latency + 0.03, latency + 0.08);
    let late = pitch_of(&out, latency + 1.0, latency + 1.4);
    assert!(early > 69.08, "already pulled in by {early}");
    assert!((late - 69.0).abs() < 0.04, "not arrived: {late}");
}

#[test]
fn flex_tune_leaves_a_far_off_note_alone() {
    let mut params = default_params();
    params.retune_ms = 0.0;
    params.flex_tune = 100.0;
    let mut dsp = dsp_with(params.clone());
    let out = sing(&mut dsp, 1.0, |_| hz_of(69.45));
    assert!((pitch_of(&out, 0.5, 1.0) - 69.45).abs() < 0.05);
    // ...but one close to its note is still corrected.
    let mut dsp = dsp_with(params);
    let out = sing(&mut dsp, 1.0, |_| hz_of(69.08));
    assert!((pitch_of(&out, 0.5, 1.0) - 69.0).abs() < 0.04);
}

#[test]
fn transpose_moves_the_output_by_semitones() {
    for transpose in [-12.0f32, -5.0, 7.0, 12.0] {
        let mut params = default_params();
        params.retune_ms = 0.0;
        params.transpose = transpose;
        let mut dsp = dsp_with(params);
        let out = sing(&mut dsp, 1.0, |_| hz_of(57.0));
        let heard = pitch_of(&out, 0.5, 1.0);
        assert!(
            (heard - (57.0 + transpose)).abs() < 0.06,
            "{transpose}: {heard}"
        );
    }
}

#[test]
fn natural_vibrato_scales_the_singers_vibrato() {
    let depth_of = |vibrato_db: f32| {
        let mut params = default_params();
        params.retune_ms = 400.0;
        params.vibrato_db = vibrato_db;
        let mut dsp = dsp_with(params);
        let vibrato = |t: f32| hz_of(60.0 + 0.3 * (std::f32::consts::TAU * 5.5 * t).sin());
        let out = sing(&mut dsp, 2.0, vibrato);
        let mut detector = detect::Detector::new(SR, 30.0);
        detector.set_range(50.0, 1_500.0);
        let mut pitches = Vec::new();
        for (n, x) in out.iter().enumerate() {
            if let Some(estimate) = detector.push(*x, 0.2) {
                if n as f32 / SR > 1.0 {
                    pitches.extend(estimate.period.map(|p| midi_of(SR / p, 440.0)));
                }
            }
        }
        let max = pitches.iter().copied().fold(f32::MIN, f32::max);
        let min = pitches.iter().copied().fold(f32::MAX, f32::min);
        max - min
    };
    let natural = depth_of(0.0);
    let wider = depth_of(6.0);
    let flatter = depth_of(-12.0);
    assert!(wider > natural * 1.5, "{wider} vs {natural}");
    assert!(flatter < natural * 0.6, "{flatter} vs {natural}");
}

#[test]
fn an_uncorrected_voice_comes_out_unchanged_and_on_time() {
    // Every note bypassed: the shifter runs, but at a ratio of exactly one.
    let mut params = default_params();
    params.bypass_mask = scale::ALL_NOTES;
    let mut dsp = dsp_with(params);
    let out = sing(&mut dsp, 1.0, |_| 440.0);
    let latency = dsp.latency_samples();
    // Recreate the input and compare against it, delayed by the latency.
    let mut phase = 0.0f32;
    let input: Vec<f32> = (0..out.len())
        .map(|_| {
            phase = (phase + 440.0 / SR).fract();
            (1..5)
                .map(|k| (std::f32::consts::TAU * phase * k as f32).sin() / k as f32)
                .sum::<f32>()
                * 0.25
        })
        .collect();
    let mut error = 0.0f32;
    let mut energy = 0.0f32;
    for n in 24_000..out.len() {
        error += (out[n] - input[n - latency]).powi(2);
        energy += input[n - latency].powi(2);
    }
    assert!(
        error / energy < 0.01,
        "residual {:.1} dB",
        10.0 * (error / energy).log10()
    );
}

#[test]
fn power_off_is_the_input_delayed_by_the_latency() {
    let mut params = default_params();
    params.power = false;
    let mut dsp = dsp_with(params);
    let latency = dsp.latency_samples();
    for n in 0..(latency + 200) {
        let x = (n as f32 * 0.07).sin();
        let (l, _) = dsp.process_stereo(x, -x);
        if n >= latency {
            let expected = ((n - latency) as f32 * 0.07).sin();
            assert!((l - expected).abs() < 1.0e-6);
        }
    }
}

#[test]
fn noise_passes_through_at_its_own_level() {
    let mut dsp = dsp_with(default_params());
    let mut seed = 7u32;
    let (mut input, mut output) = (0.0f32, 0.0f32);
    for n in 0..48_000 {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let x = ((seed >> 9) as f32 / (1u32 << 23) as f32 - 0.5) * 0.5;
        let (l, _) = dsp.process_stereo(x, x);
        assert!(l.is_finite());
        if n > 12_000 {
            input += x * x;
            output += l * l;
        }
    }
    let db = 10.0 * (output / input).log10();
    assert!(db.abs() < 1.0, "noise came out at {db:+.1} dB");
}

#[test]
fn the_input_type_sets_the_latency() {
    let mut dsp = Dsp::new(SR);
    let mut latencies = Vec::new();
    for input in InputType::ALL {
        assert!(dsp.apply_ui_param("inputType", input.to_wire()));
        latencies.push((input, dsp.latency_samples()));
    }
    let of = |kind| latencies.iter().find(|(k, _)| *k == kind).unwrap().1;
    assert!(of(InputType::Soprano) < of(InputType::AltoTenor));
    assert!(of(InputType::AltoTenor) < of(InputType::LowMale));
    assert!(of(InputType::LowMale) < of(InputType::BassInstrument));
    // Soprano answers in well under 40 ms.
    assert!((of(InputType::Soprano) as f32) < SR * 0.04);
}

#[test]
fn telemetry_shows_what_was_sung_and_what_came_out() {
    let mut params = default_params();
    params.retune_ms = 0.0;
    let mut dsp = dsp_with(params);
    let _ = sing(&mut dsp, 0.5, |_| hz_of(69.3));
    let (count, readings) = telemetry::decode(&dsp.telemetry());
    assert!(count > 50);
    let newest = readings[telemetry::POINTS - 1];
    assert!((newest.input.unwrap() - 69.3).abs() < 0.05);
    assert!((newest.output.unwrap() - 69.0).abs() < 0.02);
    assert_eq!(newest.target, Some(69));
}

#[test]
fn every_setting_stays_finite_at_every_rate() {
    for &sr in &[44_100.0f32, 96_000.0] {
        for input in InputType::ALL {
            let mut dsp = Dsp::new(sr);
            let mut params = default_params();
            params.input_type = input;
            params.retune_ms = 0.0;
            params.transpose = 24.0;
            params.formant = false;
            params.throat = 70.0;
            params.vibrato_db = 12.0;
            dsp.set_params(params);
            let mut peak = 0.0f32;
            for n in 0..(sr as usize / 2) {
                let t = n as f32 / sr;
                let x = (std::f32::consts::TAU * (110.0 + 300.0 * t) * t).sin() * 0.8;
                let (l, r) = dsp.process_stereo(x, x);
                assert!(l.is_finite() && r.is_finite());
                peak = peak.max(l.abs()).max(r.abs());
            }
            assert!(peak < 4.0, "{input:?} @ {sr}: peak {peak}");
        }
    }
}

#[test]
fn classic_glides_at_a_constant_rate() {
    let mut params = default_params();
    params.classic = true;
    params.retune_ms = 400.0;
    params.humanize = 100.0;
    params.flex_tune = 100.0;
    let mut dsp = dsp_with(params);
    // 40 cents sharp: a semitone per 400 ms is 160 ms to get there.
    let out = sing(&mut dsp, 1.2, |_| hz_of(69.4));
    let latency = dsp.latency_samples() as f32 / SR;
    // A straight line: a quarter of the way at 40 ms, half at 80 ms.
    let quarter = pitch_of(&out, latency + 0.03, latency + 0.05);
    let halfway = pitch_of(&out, latency + 0.07, latency + 0.09);
    let there = pitch_of(&out, latency + 0.5, latency + 1.0);
    assert!((quarter - 69.3).abs() < 0.06, "a quarter at {quarter}");
    assert!((halfway - 69.2).abs() < 0.06, "halfway at {halfway}");
    // Flex-Tune is set to leave a 40-cent note alone, but Classic ignores it.
    assert!((there - 69.0).abs() < 0.04, "arrived at {there}");
}

#[test]
fn created_vibrato_waits_for_its_delay_then_swings_at_its_rate() {
    let mut params = default_params();
    params.retune_ms = 0.0;
    params.vibrato_shape = VibratoShape::Sine;
    params.vibrato_rate_hz = 5.0;
    params.vibrato_delay_ms = 300.0;
    params.vibrato_onset_ms = 0.0;
    params.vibrato_pitch = 50.0;
    let mut dsp = dsp_with(params);
    let out = sing(&mut dsp, 2.0, |_| hz_of(64.0));
    let latency = dsp.latency_samples() as f32 / SR;
    // Before the delay: flat.
    let still = pitch_of(&out, latency + 0.05, latency + 0.25);
    assert!((still - 64.0).abs() < 0.04, "moved early: {still}");
    // After it: ±50 cents at 5 Hz.
    let mut detector = detect::Detector::new(SR, 30.0);
    detector.set_range(50.0, 1_500.0);
    let mut track = Vec::new();
    for (n, x) in out.iter().enumerate() {
        if let Some(estimate) = detector.push(*x, 0.2) {
            let t = n as f32 / SR;
            if t > latency + 0.8 {
                track.extend(estimate.period.map(|p| midi_of(SR / p, 440.0)));
            }
        }
    }
    let max = track.iter().copied().fold(f32::MIN, f32::max);
    let min = track.iter().copied().fold(f32::MAX, f32::min);
    assert!((max - min - 1.0).abs() < 0.2, "swing {}", max - min);
    let crossings = track
        .windows(2)
        .filter(|w| (w[0] - 64.0) <= 0.0 && (w[1] - 64.0) > 0.0)
        .count() as f32;
    let seconds = track.len() as f32 * detect::hop_seconds(SR);
    assert!(
        (crossings / seconds - 5.0).abs() < 0.8,
        "rate {}",
        crossings / seconds
    );
}

#[test]
fn created_vibrato_amplitude_moves_the_level() {
    let level_swing = |amp: f32| {
        let mut params = default_params();
        params.retune_ms = 0.0;
        params.vibrato_shape = VibratoShape::Sine;
        params.vibrato_rate_hz = 4.0;
        params.vibrato_delay_ms = 0.0;
        params.vibrato_onset_ms = 0.0;
        params.vibrato_pitch = 0.0;
        params.vibrato_amp = amp;
        let mut dsp = dsp_with(params);
        let out = sing(&mut dsp, 2.0, |_| hz_of(57.0));
        let window = (SR * 0.02) as usize;
        let levels: Vec<f32> = out[out.len() / 2..]
            .chunks(window)
            .map(|c| (c.iter().map(|x| x * x).sum::<f32>() / c.len() as f32).sqrt())
            .collect();
        let max = levels.iter().copied().fold(f32::MIN, f32::max);
        let min = levels.iter().copied().fold(f32::MAX, f32::min);
        20.0 * (max / min).log10()
    };
    assert!(level_swing(0.0) < 1.0, "steady swings {}", level_swing(0.0));
    assert!(level_swing(80.0) > 6.0, "deep swings {}", level_swing(80.0));
}

#[test]
fn created_vibrato_is_off_by_default() {
    assert_eq!(default_params().vibrato_shape, VibratoShape::None);
    assert!(!default_params().classic);
}

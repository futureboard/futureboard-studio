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

// ── Live latency ────────────────────────────────────────────────────────────

fn live(mut params: Params) -> Params {
    params.latency = LatencyMode::Live;
    params
}

/// A sine at `hz(t)` under a 5 Hz swell between 40 % and 100 % of `level`,
/// through `dsp`: the input and the output's left side.
fn swell(dsp: &mut Dsp, seconds: f32, level: f32, hz: impl Fn(f32) -> f32) -> (Vec<f32>, Vec<f32>) {
    let frames = (SR * seconds) as usize;
    let (mut input, mut output) = (Vec::with_capacity(frames), Vec::with_capacity(frames));
    let mut phase = 0.0f32;
    for n in 0..frames {
        let t = n as f32 / SR;
        phase = (phase + hz(t) / SR).fract();
        let envelope = 0.7 - 0.3 * (std::f32::consts::TAU * 5.0 * t).cos();
        let x = (std::f32::consts::TAU * phase).sin() * level * envelope;
        let (l, r) = dsp.process_stereo(x, x);
        assert!(l.is_finite() && r.is_finite(), "non-finite at {n}");
        input.push(x);
        output.push(l);
    }
    (input, output)
}

/// The frequency of `signal` between `from` and `to` seconds, from its
/// rising zero crossings, placed between samples.
fn crossing_hz(signal: &[f32], from: f32, to: f32) -> f32 {
    let (start, end) = ((from * SR) as usize, (to * SR) as usize);
    let mut crossings = Vec::new();
    for n in start.max(1)..end.min(signal.len()) {
        let (a, b) = (signal[n - 1], signal[n]);
        if a < 0.0 && b >= 0.0 {
            crossings.push(n as f32 - 1.0 + a / (a - b));
        }
    }
    assert!(crossings.len() > 10, "{} crossings", crossings.len());
    let span = crossings[crossings.len() - 1] - crossings[0];
    (crossings.len() - 1) as f32 * SR / span
}

/// How far `output`'s envelope lags `input`'s, in samples: the peak of the
/// cross-correlation of their 10 ms power envelopes, a lag at a time.
fn envelope_lag(input: &[f32], output: &[f32], from: f32, max_lag: usize) -> usize {
    let envelope = |s: &[f32]| -> Vec<f32> {
        let window = (SR * 0.01) as usize;
        let mut sum = 0.0f32;
        let mut out = Vec::with_capacity(s.len());
        for n in 0..s.len() {
            sum += s[n] * s[n];
            if n >= window {
                sum -= s[n - window] * s[n - window];
            }
            out.push(sum.max(0.0) / window as f32);
        }
        out
    };
    let (a, b) = (envelope(input), envelope(output));
    let start = (from * SR) as usize;
    let end = a.len() - max_lag;
    let mean = |s: &[f32]| s.iter().sum::<f32>() / s.len() as f32;
    let (ma, mb) = (mean(&a[start..end]), mean(&b[start..]));
    let mut best = (0, f32::MIN);
    for lag in 0..=max_lag {
        let score: f32 = (start..end).map(|n| (a[n] - ma) * (b[n + lag] - mb)).sum();
        if score > best.1 {
            best = (lag, score);
        }
    }
    best.0
}

/// The largest step between neighbouring samples from `from` seconds on.
fn largest_step(signal: &[f32], from: f32) -> f32 {
    signal[(from * SR) as usize..]
        .windows(2)
        .map(|w| (w[1] - w[0]).abs())
        .fold(0.0, f32::max)
}

#[test]
fn the_latency_table_matches_the_dsp_in_both_modes() {
    for &sr in &[44_100.0f32, 48_000.0, 96_000.0] {
        let mut dsp = Dsp::new(sr);
        for mode in LatencyMode::ALL {
            assert!(dsp.apply_ui_param("latency", mode.to_wire()));
            for input in InputType::ALL {
                assert!(dsp.apply_ui_param("inputType", input.to_wire()));
                assert_eq!(
                    dsp.latency_samples(),
                    latency_samples_for(sr, input, mode),
                    "{mode:?} {input:?} @ {sr}"
                );
                if mode == LatencyMode::Live {
                    assert_eq!(dsp.latency_samples(), splice::LATENCY);
                }
            }
        }
    }
    for input in InputType::ALL {
        let quality = latency_samples_for(SR, input, LatencyMode::Quality);
        let live = latency_samples_for(SR, input, LatencyMode::Live);
        eprintln!(
            "latency @48k {input:?}: quality {quality} smp ({:.2} ms), live {live} smp ({:.3} ms)",
            quality as f32 * 1_000.0 / SR,
            live as f32 * 1_000.0 / SR
        );
    }
}

#[test]
fn a_state_saved_before_the_latency_mode_loads_as_quality() {
    let json = ipc::WhiteSharpState::default().to_json().unwrap();
    let old = json.replace(",\"latency\":\"quality\"", "");
    assert_ne!(old, json, "the field is written as expected");
    let state = ipc::WhiteSharpState::from_json(&old).unwrap();
    assert_eq!(state.params.latency, LatencyMode::Quality);
    assert_eq!(default_params().latency, LatencyMode::Quality);
}

#[test]
fn live_mode_puts_a_sine_on_its_note_without_a_hidden_delay() {
    // 30 cents sharp of A3: pulled down onto 220 Hz.
    for (sung, target) in [(57.3f32, 57.0f32), (56.7, 57.0)] {
        let mut params = default_params();
        params.retune_ms = 0.0;
        let mut dsp = dsp_with(live(params.clone()));
        let (input, output) = swell(&mut dsp, 1.6, 0.5, |_| hz_of(sung));
        let heard = crossing_hz(&output, 0.6, 1.5);
        let cents = 1_200.0 * (heard / hz_of(target)).log2();
        assert!(cents.abs() < 3.0, "sang {sung}: {heard} Hz ({cents:+.1} c)");
        // 30 cents is a splice every 1/0.017 periods: about 6 here.
        let (_, splices) = dsp.live_head();
        assert!(splices >= 3, "only {splices} splices");

        // Aligned with the input to within a period of it: the envelope
        // comes out when it went in.
        let period = SR / hz_of(sung);
        let lag = envelope_lag(&input, &output, 0.4, 4_800);
        eprintln!(
            "live sang {sung}: {heard:.2} Hz ({cents:+.2} c), envelope lag {lag} smp, period {period:.0}, {splices} splices"
        );
        assert!(
            (lag as f32) <= period + SR * 0.0005,
            "sang {sung}: envelope {lag} samples late, period {period}"
        );

        // The measure itself: Quality comes out at its reported latency.
        let mut quality = dsp_with(params);
        let (input, output) = swell(&mut quality, 1.6, 0.5, |_| hz_of(sung));
        let lag = envelope_lag(&input, &output, 0.4, 4_800) as f32;
        let reported = quality.latency_samples() as f32;
        eprintln!("quality sang {sung}: envelope lag {lag} smp, reported {reported}");
        assert!(
            (lag - reported).abs() < SR * 0.002,
            "quality lag {lag} vs reported {reported}"
        );
    }
}

#[test]
fn live_mode_head_delay_per_input_type() {
    // A note in each type's range, a third of a semitone flat (raised) and
    // sharp (lowered); the head's delay over the held part.
    for (input, note) in [
        (InputType::Soprano, 69.0f32),
        (InputType::AltoTenor, 57.0),
        (InputType::LowMale, 45.0),
        (InputType::Instrument, 57.0),
        (InputType::BassInstrument, 33.0),
    ] {
        for offset in [-0.33f32, 0.33] {
            let mut params = default_params();
            params.retune_ms = 0.0;
            params.input_type = input;
            let mut dsp = dsp_with(live(params));
            let hz = hz_of(note + offset);
            let frames = (SR * 1.5) as usize;
            let (mut sum, mut worst, mut count) = (0.0f64, 0.0f64, 0usize);
            let mut phase = 0.0f32;
            let mut output = Vec::with_capacity(frames);
            for n in 0..frames {
                phase = (phase + hz / SR).fract();
                let x = (1..5)
                    .map(|k| (std::f32::consts::TAU * phase * k as f32).sin() / k as f32)
                    .sum::<f32>()
                    * 0.25;
                let (l, _) = dsp.process_stereo(x, x);
                output.push(l);
                if n as f32 > SR * 0.5 {
                    let (delay, _) = dsp.live_head();
                    sum += delay;
                    worst = worst.max(delay);
                    count += 1;
                }
            }
            let mean = sum / count as f64;
            let period = f64::from(SR / hz);
            let heard = pitch_of(&output, 0.5, 1.5);
            eprintln!(
                "live {input:?} {:.0} Hz {offset:+}: head mean {:.2} ms, max {:.2} ms (period {:.2} ms), heard {heard:.2}",
                hz,
                mean * 1_000.0 / f64::from(SR),
                worst * 1_000.0 / f64::from(SR),
                period * 1_000.0 / f64::from(SR),
            );
            assert!(
                (heard - note).abs() < 0.05,
                "{input:?} {offset}: heard {heard}"
            );
            assert!(
                worst <= 1.5 * period + 2.0 * splice::LATENCY as f64 + 64.0,
                "{input:?} {offset}: head {worst} for a period of {period}"
            );
        }
    }
}

#[test]
fn live_mode_snaps_to_the_scale_and_transposes() {
    let mut params = default_params();
    params.retune_ms = 0.0;
    params.scale = Scale::Major;
    // C# + 30 c in C major goes to D; with D removed, to C.
    let mut dsp = dsp_with(live(params.clone()));
    let out = sing(&mut dsp, 1.0, |_| hz_of(61.3));
    let heard = pitch_of(&out, 0.5, 1.0);
    assert!((heard - 62.0).abs() < 0.05, "heard {heard}");
    params.remove_mask = 1 << 2;
    let mut dsp = dsp_with(live(params));
    let out = sing(&mut dsp, 1.0, |_| hz_of(61.3));
    let heard = pitch_of(&out, 0.5, 1.0);
    assert!((heard - 60.0).abs() < 0.05, "heard {heard}");

    for transpose in [-12.0f32, -5.0, 7.0, 12.0] {
        let mut params = default_params();
        params.retune_ms = 0.0;
        params.transpose = transpose;
        let mut dsp = dsp_with(live(params));
        let out = sing(&mut dsp, 1.0, |_| hz_of(57.0));
        let heard = pitch_of(&out, 0.5, 1.0);
        assert!(
            (heard - (57.0 + transpose)).abs() < 0.06,
            "{transpose}: {heard}"
        );
    }
}

#[test]
fn live_splices_do_not_click() {
    // A pure sine: any discontinuity stands out against its smooth slope.
    for (sung, transpose) in [(57.4f32, 0.0f32), (56.6, 0.0), (57.0, 7.0), (57.0, -5.0)] {
        let mut params = default_params();
        params.retune_ms = 0.0;
        params.transpose = transpose;
        let mut dsp = dsp_with(live(params));
        let level = 0.5;
        let (_, output) = swell(&mut dsp, 1.5, level, |_| hz_of(sung));
        let (_, splices) = dsp.live_head();
        assert!(splices > 5, "{sung} {transpose}: {splices} splices");
        // The steepest a sine of this level gets, at the faster of the
        // pitch sung and the pitch played.
        let fastest = hz_of(sung).max(hz_of(sung.round() + transpose));
        let slope = std::f32::consts::TAU * fastest / SR * level;
        let step = largest_step(&output, 0.3);
        eprintln!(
            "live {sung} {transpose:+}: largest step {:.3} of the sine's slope, {splices} splices",
            step / slope
        );
        assert!(
            step <= 1.15 * slope,
            "{sung} {transpose:+}: step {step} against a slope of {slope}"
        );
    }
}

#[test]
fn live_mode_passes_an_uncorrected_voice_through_two_samples_late() {
    let mut params = live(default_params());
    params.bypass_mask = scale::ALL_NOTES;
    let mut dsp = dsp_with(params);
    assert_eq!(dsp.latency_samples(), 2);
    let (input, output) = swell(&mut dsp, 1.0, 0.5, |_| 440.0);
    let worst = (2..output.len())
        .map(|n| (output[n] - input[n - 2]).abs())
        .fold(0.0f32, f32::max);
    assert!(worst < 1.0e-5, "off by {worst}");

    let mut params = live(default_params());
    params.power = false;
    let mut dsp = dsp_with(params);
    for n in 0..2_000usize {
        let x = (n as f32 * 0.07).sin();
        let (l, _) = dsp.process_stereo(x, -x);
        if n >= 2 {
            assert!((l - ((n - 2) as f32 * 0.07).sin()).abs() < 1.0e-6);
        }
    }
}

#[test]
fn live_mode_passes_noise_at_its_own_level() {
    let mut dsp = dsp_with(live(default_params()));
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
fn switching_the_latency_mode_mid_note_crossfades() {
    let mut params = default_params();
    params.retune_ms = 0.0;
    let mut dsp = dsp_with(params);
    let level = 0.5;
    let hz = hz_of(57.3);
    let mut phase = 0.0f32;
    let mut output = Vec::new();
    let mut live = false;
    for n in 0..(SR as usize * 2) {
        if n > 0 && n % (SR as usize / 5) == 0 {
            live = !live;
            let mode = if live {
                LatencyMode::Live
            } else {
                LatencyMode::Quality
            };
            assert!(dsp.apply_ui_param("latency", mode.to_wire()));
            let expected = latency_samples_for(SR, InputType::AltoTenor, mode);
            assert_eq!(dsp.latency_samples(), expected);
        }
        phase = (phase + hz / SR).fract();
        let x = (std::f32::consts::TAU * phase).sin() * level;
        let (l, r) = dsp.process_stereo(x, x);
        assert!(l.is_finite() && r.is_finite());
        output.push(l);
    }
    // Two copies of the voice at different delays, faded by power: at
    // most √2 of the level, and of its slope.
    let slope = std::f32::consts::TAU * hz / SR * level;
    let step = largest_step(&output, 0.1);
    eprintln!(
        "mode switching: largest step {:.3} of the sine's slope",
        step / slope
    );
    assert!(
        step <= 1.5 * slope,
        "step {step} against a slope of {slope}"
    );
    let peak = output.iter().fold(0.0f32, |p, x| p.max(x.abs()));
    assert!(peak <= 1.45 * level, "peak {peak}");
}

#[test]
fn live_mode_stays_finite_at_every_setting_and_rate() {
    for &sr in &[44_100.0f32, 96_000.0] {
        for input in InputType::ALL {
            let mut dsp = Dsp::new(sr);
            let mut params = live(default_params());
            params.input_type = input;
            params.retune_ms = 0.0;
            params.transpose = 24.0;
            params.vibrato_db = 12.0;
            params.vibrato_shape = VibratoShape::Square;
            params.vibrato_delay_ms = 0.0;
            params.vibrato_amp = 100.0;
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

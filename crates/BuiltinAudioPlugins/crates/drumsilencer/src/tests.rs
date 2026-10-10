use super::*;

const SR: f32 = 48_000.0;

/// The untrained 16-unit network `training/export.py --random` wrote, with the
/// output `training/dsil.py` gives for its parity signal.
static PARITY_WEIGHTS: &[u8] = include_bytes!("testdata/random16.dsil");
static PARITY_VECTOR: &[u8] = include_bytes!("testdata/random16.dsil.parity");

fn parity_model() -> &'static Model {
    static MODEL: OnceLock<Model> = OnceLock::new();
    MODEL.get_or_init(|| Model::parse(PARITY_WEIGHTS).unwrap())
}

fn floats(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect()
}

fn noise(len: usize, seed: u32) -> Vec<(f32, f32)> {
    let mut s = seed.wrapping_mul(747_796_405).wrapping_add(1);
    let mut next = move || {
        s ^= s << 13;
        s ^= s >> 17;
        s ^= s << 5;
        (s as f32 / u32::MAX as f32) * 2.0 - 1.0
    };
    (0..len).map(|_| (next() * 0.5, next() * 0.3)).collect()
}

fn run(dsp: &mut Dsp, input: &[(f32, f32)]) -> Vec<(f32, f32)> {
    input.iter().map(|&(l, r)| dsp.process_stereo(l, r)).collect()
}

/// The largest difference between `out` and `input` delayed by `latency`.
fn delay_error(input: &[(f32, f32)], out: &[(f32, f32)], latency: usize) -> f32 {
    let mut worst = 0.0f32;
    for (i, o) in out.iter().enumerate() {
        let x = if i >= latency { input[i - latency] } else { (0.0, 0.0) };
        worst = worst.max((o.0 - x.0).abs()).max((o.1 - x.1).abs());
    }
    worst
}

#[test]
fn the_streaming_dsp_matches_the_training_code() {
    let header = &PARITY_VECTOR[..12];
    assert_eq!(&header[..4], b"DSPV");
    let rate = u32::from_le_bytes(header[4..8].try_into().unwrap()) as f32;
    let frames = u32::from_le_bytes(header[8..12].try_into().unwrap()) as usize;
    let data = floats(&PARITY_VECTOR[12..]);
    assert_eq!(data.len(), 4 * frames);
    let input: Vec<(f32, f32)> = data[..2 * frames].chunks(2).map(|c| (c[0], c[1])).collect();
    let expected = &data[2 * frames..];

    let mut dsp = Dsp::with_model(rate, parity_model());
    let out = run(&mut dsp, &input);
    let mut worst = 0.0f32;
    let mut peak = 0.0f32;
    for (i, o) in out.iter().enumerate() {
        worst = worst
            .max((o.0 - expected[2 * i]).abs())
            .max((o.1 - expected[2 * i + 1]).abs());
        peak = peak.max(expected[2 * i].abs());
    }
    assert!(peak > 0.1, "the parity output is not silent");
    assert!(worst < 2.0e-4, "worst sample error {worst}");
}

#[test]
fn bypassed_it_is_a_pure_delay_at_every_rate() {
    for rate in [44_100.0, 48_000.0, 88_200.0, 96_000.0, 192_000.0] {
        let mut dsp = Dsp::with_model(rate, parity_model());
        assert!(dsp.apply_ui_param("power", 0.0));
        let input = noise(rate as usize / 4, 3);
        let out = run(&mut dsp, &input);
        let latency = dsp.latency_samples();
        let scale = (rate / 48_000.0).round() as usize;
        assert_eq!(latency, 256 * scale - 1, "{rate}");
        let err = delay_error(&input, &out, latency);
        assert!(err < 1.0e-5, "{rate}: {err}");
    }
}

#[test]
fn no_amount_is_transparent_and_remove_plus_solo_is_the_input() {
    let input = noise(24_000, 5);
    let mut dsp = Dsp::with_model(SR, parity_model());
    assert!(dsp.apply_ui_param("amount", 0.0));
    let out = run(&mut dsp, &input);
    assert!(delay_error(&input, &out, dsp.latency_samples()) < 1.0e-5);

    let mut remove = Dsp::with_model(SR, parity_model());
    let mut solo = Dsp::with_model(SR, parity_model());
    assert!(solo.apply_ui_param("mode", 1.0));
    let a = run(&mut remove, &input);
    let b = run(&mut solo, &input);
    let sum: Vec<(f32, f32)> = a.iter().zip(&b).map(|(x, y)| (x.0 + y.0, x.1 + y.1)).collect();
    assert!(delay_error(&input, &sum, remove.latency_samples()) < 1.0e-5);
    // And they really split it: neither is the input on its own.
    assert!(delay_error(&input, &a, remove.latency_samples()) > 1.0e-3);
}

#[test]
fn the_range_fades_over_half_an_octave() {
    assert_eq!(range_weight(50.0, LOW_OFF_HZ, HIGH_OFF_HZ), 1.0);
    assert_eq!(range_weight(200.0, 200.0, HIGH_OFF_HZ), 1.0);
    assert_eq!(range_weight(100.0, 200.0, HIGH_OFF_HZ), 0.0);
    assert!((range_weight(200.0 / 2f32.sqrt().sqrt(), 200.0, HIGH_OFF_HZ) - 0.5).abs() < 1.0e-4);
    assert_eq!(range_weight(4_000.0, 20.0, 4_000.0), 1.0);
    assert_eq!(range_weight(8_000.0, 20.0, 4_000.0), 0.0);
    assert_eq!(range_weight(1_000.0, 2_000.0, 1_000.0), 0.0);
}

/// Outside the range the signal passes untouched; Solo silences it.
#[test]
fn outside_the_range_remove_passes_and_solo_mutes() {
    let tone = |i: usize| (std::f32::consts::TAU * 200.0 * i as f32 / SR).sin() * 0.5;
    let input: Vec<(f32, f32)> = (0..24_000).map(|i| (tone(i), tone(i))).collect();
    let mut remove = Dsp::with_model(SR, parity_model());
    assert!(remove.apply_ui_param("lowHz", 2_000.0));
    let out = run(&mut remove, &input);
    let lat = remove.latency_samples();
    // Not bit-exact: the short side of the analysis window leaks a little of
    // the 200 Hz tone into the bins above 1.4 kHz, which the mask does touch.
    // The leak is under −30 dB of the tone.
    let err = delay_error(&input[..20_000], &out[..20_000], lat);
    assert!(err < 0.5 * 0.0316, "{err}");

    let mut solo = Dsp::with_model(SR, parity_model());
    assert!(solo.apply_ui_param("mode", 1.0));
    assert!(solo.apply_ui_param("lowHz", 2_000.0));
    let out = run(&mut solo, &input);
    let peak = out[4_000..].iter().fold(0.0f32, |p, o| p.max(o.0.abs()));
    assert!(peak < 1.0e-3, "{peak}");
}

#[test]
fn the_meters_see_the_input_bands() {
    let mut dsp = Dsp::with_model(SR, parity_model());
    let tone = |i: usize| (std::f32::consts::TAU * 2_000.0 * i as f32 / SR).sin() * 0.5;
    for i in 0..24_000 {
        dsp.process_stereo(tone(i), tone(i));
    }
    let frame = dsp.meter_frame();
    // 2 kHz is band 3 (1–3 kHz); a 0.5 sine is 0.354 RMS.
    assert!((frame.input_bands[3] - 0.354).abs() < 0.03, "{:?}", frame.input_bands);
    for (b, level) in frame.input_bands.iter().enumerate() {
        if b != 3 {
            assert!(*level < 0.01, "band {b}: {level}");
        }
    }
    let (slot_in, slot_out) = frame.rack_slots::<6>();
    assert_eq!(slot_out, frame.input_bands);
    assert_eq!(slot_in, frame.drum_bands);
    assert!(frame.in_rms > 0.3 && frame.out_rms > 0.0);
}

#[test]
fn extreme_settings_stay_finite() {
    let mut dsp = Dsp::with_model(SR, parity_model());
    for (index, id) in UI_PARAM_IDS.iter().enumerate() {
        for value in [-1.0e9, 0.0, 1.0e9] {
            assert!(dsp.apply_wire_param(index as u32, value), "{id}");
            for (l, r) in noise(600, index as u32) {
                let (a, b) = dsp.process_stereo(l * 4.0, r);
                assert!(a.is_finite() && b.is_finite(), "{id}={value}");
            }
        }
    }
    dsp.set_sample_rate(22_050.0);
    assert_eq!(dsp.latency_samples(), 255);
    dsp.set_sample_rate(144_000.0);
    assert_eq!(dsp.latency_samples(), 511);
    dsp.reset();
    assert_eq!(dsp.process_stereo(0.0, 0.0), (0.0, 0.0));
}

#[test]
fn descriptor_and_presets_agree_with_the_wire() {
    let d = descriptor();
    assert_eq!(d.id, PLUGIN_ID);
    assert_eq!(d.params.len(), ipc::PARAM_COUNT);
    let defaults = ipc::ui_values(&default_params());
    for (param, (id, value)) in d.params.iter().zip(&defaults) {
        assert_eq!(param.id, *id);
        assert!((param.default_value - value).abs() < 1.0e-6, "{id}");
        let (min, max) = ipc::RANGES[ui_param_index(id).unwrap() as usize];
        if param.unit != "bool" && param.unit != "enum" {
            assert_eq!((param.min, param.max), (min, max), "{id}");
        }
    }
    let bank = factory_presets();
    assert_eq!(bank[0].name, "Default");
    for preset in &bank {
        let mut sanitized = preset.params.clone();
        ipc::sanitize_params(&mut sanitized);
        assert_eq!(ipc::ui_values(&sanitized), ipc::ui_values(&preset.params), "{}", preset.name);
    }
}

/// The shipped file is the trained network, not a stand-in: full size, and on
/// a drum-and-tone signal it takes the hits out and leaves the tone.
#[test]
fn the_shipped_weights_are_the_trained_network() {
    let model = embedded_model();
    assert_eq!(model.hidden(), 192, "model/drumsilencer.dsil is not the trained export");
}

/// How much faster than real time one instance runs, at 48 kHz stereo:
/// `cargo test --release -p drumsilencer realtime_cost -- --ignored --nocapture`.
/// `DRUMSIL_WEIGHTS=<file>` measures other weights than the shipped ones.
#[test]
#[ignore]
fn realtime_cost() {
    let model: &'static Model = match std::env::var("DRUMSIL_WEIGHTS") {
        Ok(path) => Box::leak(Box::new(Model::parse(&std::fs::read(path).unwrap()).unwrap())),
        Err(_) => embedded_model(),
    };
    let mut dsp = Dsp::with_model(SR, model);
    let input = noise(SR as usize * 10, 9);
    let started = std::time::Instant::now();
    let mut sink = 0.0f32;
    for &(l, r) in &input {
        sink += dsp.process_stereo(l, r).0;
    }
    let seconds = started.elapsed().as_secs_f64();
    println!(
        "hidden {}: 10 s of audio in {seconds:.3} s — {:.1}x real time, {:.1} % of a core ({sink})",
        model.hidden(),
        10.0 / seconds,
        seconds * 10.0
    );
}
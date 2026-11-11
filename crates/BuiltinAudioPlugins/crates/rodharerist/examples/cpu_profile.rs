//! Offline CPU / latency profile — NOT part of the shipping plugin.
//!
//! Renders a fixed amount of audio through each stage in isolation and prints
//! the cost as a percentage of one core's realtime budget, plus the latency
//! each engine reports to the host. Read it as: "at 48 kHz, one instance of
//! this stage eats N % of a core". A full chain over ~100 % cannot keep up and
//! is what a user hears as lag/xruns.
//!
//! ```text
//! cargo run -p rodharerist --example cpu_profile --release
//! ```

use std::time::Instant;

use builtin_dsp_core::StereoEffect;
use rodharerist::{
    CabModel, Dsp, PATH_SLOTS, Params, StageKind, ToneEngineKind, default_params,
    prepare_ir_runtime,
};

const SR: f32 = 48_000.0;
/// Two seconds of audio per measurement — long enough to average out cache
/// warm-up, short enough that the whole profile runs in a few seconds.
const N: usize = 96_000;

fn lcg(state: &mut u32) -> f32 {
    *state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
    (*state >> 8) as f32 / (1 << 24) as f32 * 2.0 - 1.0
}

fn only(stage: StageKind) -> Params {
    let mut p = default_params();
    p.stage_order = [None; PATH_SLOTS];
    p.stage_order[0] = Some(stage);
    p
}

/// Realtime factor in percent of one core: 100 % means the stage takes exactly
/// as long to compute as the audio it produces.
fn measure(label: &str, dsp: &mut Dsp) -> f64 {
    let mut rng = 0x1234_5678u32;
    let input: Vec<f32> = (0..N).map(|_| lcg(&mut rng) * 0.3).collect();

    // Warm the caches / settle the smoothers before timing.
    for &x in input.iter().take(4_096) {
        dsp.begin_block();
        let _ = dsp.process_stereo(x, x);
    }

    let started = Instant::now();
    let mut sink = 0.0f32;
    for (i, &x) in input.iter().enumerate() {
        if i % 128 == 0 {
            dsp.begin_block();
        }
        let (l, r) = dsp.process_stereo(x, x);
        sink += l + r;
    }
    let elapsed = started.elapsed().as_secs_f64();
    let audio_seconds = N as f64 / SR as f64;
    let percent = elapsed / audio_seconds * 100.0;
    std::hint::black_box(sink);
    println!(
        "{label:<34} {percent:>7.2} % of one core   ({:>5.0} ns/sample)",
        elapsed * 1.0e9 / N as f64
    );
    percent
}

/// Cost of running the chain on the silence *after* a loud passage, relative
/// to the cost of running it on the passage itself.
///
/// Every filter tail in the chain decays toward zero, and once its state falls
/// under the smallest normal float the CPU switches to denormal arithmetic —
/// on x86 that is tens to hundreds of cycles per operation. A ratio well over
/// 1.0 here is the classic "the plugin spikes a second after I stop playing"
/// fault, and it is invisible to any measurement that only feeds it signal.
fn measure_decay_ratio(label: &str, dsp: &mut Dsp) {
    let mut rng = 0x5eed_1234u32;
    let loud: Vec<f32> = (0..N / 2).map(|_| lcg(&mut rng) * 0.5).collect();

    let started = Instant::now();
    let mut sink = 0.0f32;
    for (i, &x) in loud.iter().enumerate() {
        if i % 128 == 0 {
            dsp.begin_block();
        }
        let (l, r) = dsp.process_stereo(x, x);
        sink += l + r;
    }
    let loud_ns = started.elapsed().as_secs_f64() * 1.0e9 / loud.len() as f64;

    // Now silence, for as long again: every tail decays into the denormal range.
    let started = Instant::now();
    for i in 0..loud.len() {
        if i % 128 == 0 {
            dsp.begin_block();
        }
        let (l, r) = dsp.process_stereo(0.0, 0.0);
        sink += l + r;
    }
    let quiet_ns = started.elapsed().as_secs_f64() * 1.0e9 / loud.len() as f64;
    std::hint::black_box(sink);
    println!(
        "{label:<34} {:>5.0} ns/sample on silence vs {loud_ns:>5.0} on signal  ({:.2}x)",
        quiet_ns,
        quiet_ns / loud_ns
    );
}

/// One WaveNet layer array: `(input_size, channels, head_size, dilations)`.
type ArraySpec = (usize, usize, usize, &'static [usize]);

/// The dilation ladder every stock NAM architecture uses.
const DILATIONS: &[usize] = &[1, 2, 4, 8, 16, 32, 64, 128, 256, 512];

/// Layer-array shapes matching the stock NAM trainer architectures, so the
/// measured cost is what a user's own `.nam` file actually costs.
const NAM_SHAPES: &[(&str, &[ArraySpec])] = &[
    ("feather", &[(1, 8, 4, DILATIONS), (8, 4, 1, DILATIONS)]),
    ("standard", &[(1, 16, 8, DILATIONS), (16, 8, 1, DILATIONS)]),
    ("heavy", &[(1, 20, 8, DILATIONS), (20, 8, 1, DILATIONS)]),
];

/// Build a WaveNet `.nam` from real NAM-shaped layer arrays. The weight
/// vector's exact length is whatever the config implies, so the first parse is
/// used to learn it from the mismatch error and the model is then rebuilt at
/// the right size.
fn nam_json(arrays: &[ArraySpec]) -> String {
    let layers: Vec<String> = arrays
        .iter()
        .enumerate()
        .map(|(i, (input_size, channels, head_size, dilations))| {
            format!(
                r#"{{"input_size": {input_size}, "condition_size": 1, "channels": {channels},
                 "head_size": {head_size}, "kernel_size": 3, "dilations": [{}],
                 "activation": "Tanh", "gated": false, "head_bias": {}}}"#,
                dilations
                    .iter()
                    .map(|d| d.to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
                i + 1 == arrays.len(),
            )
        })
        .collect();
    let build = |count: usize| {
        let mut rng = 0x9e37_79b9u32;
        let weights: Vec<String> = (0..count)
            .map(|_| format!("{:.6}", lcg(&mut rng) * 0.05))
            .collect();
        format!(
            r#"{{"version": "0.5.4", "architecture": "WaveNet",
                 "config": {{"layers": [{}], "head": null, "head_scale": 1.0}},
                 "weights": [{}], "sample_rate": 48000.0}}"#,
            layers.join(", "),
            weights.join(", ")
        )
    };
    // `NamModel::from_json_str` only parses the container; the weight count is
    // checked when the network is actually built, so probe through `from_nam`.
    let probe = build(1);
    let built = nam_rs::NamModel::from_json_str(&probe)
        .and_then(|model| nam_rs::Model::from_nam(&model).map(|_| ()));
    match built {
        Ok(()) => probe,
        Err(e) => {
            let text = e.to_string();
            let expected: usize = text
                .split("implies ")
                .nth(1)
                .and_then(|rest| rest.split(' ').next())
                .and_then(|n| n.parse().ok())
                .unwrap_or_else(|| panic!("cannot size weights from `{text}`"));
            build(expected)
        }
    }
}

fn ir_wav(seconds: f32, channels: u16) -> Vec<u8> {
    let frames = (SR * seconds) as usize;
    let mut rng = 0x0bad_f00du32;
    let mut samples = Vec::with_capacity(frames * channels as usize);
    for i in 0..frames {
        // A plausible cabinet shape: bright early reflections into a fast decay.
        let decay = (-(i as f32) / (SR * 0.02)).exp();
        for _ in 0..channels {
            samples.push(lcg(&mut rng) * decay * 0.5);
        }
    }
    let data: Vec<u8> = samples.iter().flat_map(|v| v.to_le_bytes()).collect();
    let block_align = channels * 4;
    let mut fmt = Vec::new();
    fmt.extend_from_slice(&3u16.to_le_bytes());
    fmt.extend_from_slice(&channels.to_le_bytes());
    fmt.extend_from_slice(&(SR as u32).to_le_bytes());
    fmt.extend_from_slice(&((SR as u32) * block_align as u32).to_le_bytes());
    fmt.extend_from_slice(&block_align.to_le_bytes());
    fmt.extend_from_slice(&32u16.to_le_bytes());

    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(4 + 8 + fmt.len() as u32 + 8 + data.len() as u32).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
    out.extend_from_slice(&fmt);
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&data);
    out
}

fn main() {
    println!("Rodhareist CPU profile — {SR} Hz, one stereo instance\n");

    println!("-- individual stages --");
    let stages = [
        (StageKind::Gate, "gate_on"),
        (StageKind::Comp, "comp_on"),
        (StageKind::Drive, "drive_on"),
        (StageKind::Eq, "eq_on"),
        (StageKind::Mod, "mod_on"),
        (StageKind::Wah, "wah_on"),
        (StageKind::Delay, "delay_on"),
        (StageKind::Reverb, "reverb_on"),
        (StageKind::Cab, "cab_on"),
        (StageKind::Amp, "amp_on"),
    ];
    for (stage, _) in stages {
        let mut dsp = Dsp::new(SR);
        dsp.set_params(only(stage));
        measure(&format!("{stage:?} (default model)"), &mut dsp);
    }

    println!("\n-- cabinet: modeled vs IR --");
    for seconds in [0.05f32, 0.2, 0.5] {
        let mut dsp = Dsp::new(SR);
        let mut p = only(StageKind::Cab);
        p.cab_model = CabModel::Ir;
        dsp.set_params(p);
        let bytes = ir_wav(seconds, 1);
        let info = dsp.load_ir_wav(&bytes, "profile").expect("IR loads");
        dsp.begin_block();
        measure(
            &format!("IR {seconds:>4.2} s ({} frames)", info.frames),
            &mut dsp,
        );
        println!(
            "{:<34} latency {} samples ({:.2} ms)",
            "  reported",
            info.latency_samples,
            info.latency_samples as f32 / SR * 1_000.0
        );
    }
    // Load cost, since it happens while the user waits.
    let bytes = ir_wav(0.5, 2);
    let started = Instant::now();
    let _ = prepare_ir_runtime(&bytes, "load".into(), SR as f64).expect("IR loads");
    println!(
        "{:<34} {:>7.1} ms to prepare (stereo)",
        "IR 0.50 s load",
        started.elapsed().as_secs_f64() * 1_000.0
    );

    println!("\n-- NAM capture --");
    for (label, arrays) in NAM_SHAPES {
        let json = nam_json(arrays);
        let mut dsp = Dsp::new(SR);
        let mut p = only(StageKind::Amp);
        p.tone_engine = ToneEngineKind::NamCapture;
        dsp.set_params(p);
        let started = Instant::now();
        let info = match dsp.load_nam_capture_json(&json, *label, false, true) {
            Ok(info) => info,
            Err(e) => {
                println!("{label:<34} skipped: {e}");
                continue;
            }
        };
        let load_ms = started.elapsed().as_secs_f64() * 1_000.0;
        dsp.begin_block();
        measure(&format!("NAM {label} (mono)"), &mut dsp);
        println!(
            "{:<34} latency {} samples ({:.1} ms), rf {}, {load_ms:.0} ms to load",
            "  reported",
            dsp.nam_latency_samples(),
            dsp.nam_latency_samples() as f32 / SR * 1_000.0,
            info.receptive_field,
        );
    }

    println!("\n-- denormal check: cost of the decay into silence --");
    for &stage in StageKind::ALL {
        let mut dsp = Dsp::new(SR);
        let mut p = only(stage);
        p.pitch_on = true;
        dsp.set_params(p);
        measure_decay_ratio(&format!("{stage:?}"), &mut dsp);
    }
    let mut dsp = Dsp::new(SR);
    dsp.set_params(default_params());
    measure_decay_ratio("full chain", &mut dsp);
    // What a stopped transport feeds a rig with the global transpose on, and
    // with tempo-synced repeats.
    let mut dsp = Dsp::new(SR);
    let mut p = default_params();
    p.transpose_semi = -2.0;
    dsp.set_params(p);
    measure_decay_ratio("full chain, transpose -2", &mut dsp);
    let mut dsp = Dsp::new(SR);
    let mut p = default_params();
    p.delay_sync = rodharerist::DelaySync::DottedEighth;
    dsp.set_params(p);
    dsp.set_tempo_bpm(97.0);
    measure_decay_ratio("full chain, synced delay", &mut dsp);

    // The bridge has one device period to answer a block; a single block over
    // it is a dropout however cheap the average is. Blocks exactly as the
    // plug-in host runs them, every factory preset, worst block reported.
    println!("\n-- worst block, host-style, 256 frames (5.33 ms period) --");
    const BLOCK: usize = 256;
    let mut worst_overall = (0.0f64, String::new());
    for preset in rodharerist::factory_presets() {
        let mut dsp = Dsp::new(SR);
        dsp.set_params(preset.params.clone());
        dsp.set_tempo_bpm(120.0);
        let mut rng = 0x0dd_ba11u32;
        let mut times = Vec::with_capacity(800);
        for block in 0..800 {
            // Half a second of playing, then silence: the paused case.
            let loud = block < 100;
            dsp.begin_block_with_frames(BLOCK);
            let started = Instant::now();
            let mut sink = 0.0f32;
            for _ in 0..BLOCK {
                let x = if loud { lcg(&mut rng) * 0.3 } else { 0.0 };
                let (l, r) = dsp.process_stereo(x, x);
                sink += l + r;
            }
            std::hint::black_box(sink);
            times.push(started.elapsed().as_secs_f64() * 1_000.0);
        }
        let mut sorted = times.clone();
        sorted.sort_by(f64::total_cmp);
        let p99 = sorted[sorted.len() * 99 / 100];
        let (worst_at, worst) = times
            .iter()
            .copied()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .unwrap_or_default();
        if worst > worst_overall.0 {
            worst_overall = (worst, preset.name.to_string());
        }
        println!(
            "{:<34} median {:>5.2} ms  p99 {:>5.2} ms  worst {:>6.2} ms (block {worst_at})",
            preset.name,
            sorted[sorted.len() / 2],
            p99,
            worst
        );
    }
    println!(
        "  worst block of any preset: {:.2} ms ({})",
        worst_overall.0, worst_overall.1
    );

    println!("\n-- nam-rs kernel: per-sample vs block --");
    for (label, arrays) in NAM_SHAPES {
        let json = nam_json(arrays);
        let Ok(model) = nam_rs::NamModel::from_json_str(&json) else {
            continue;
        };
        let mut rng = 0x2468_1357u32;
        let input: Vec<f32> = (0..N).map(|_| lcg(&mut rng) * 0.3).collect();

        let mut per_sample = nam_rs::Model::from_nam(&model).expect("builds");
        let started = Instant::now();
        let mut sink = 0.0f32;
        for &x in &input {
            sink += per_sample.process_sample(x);
        }
        let sample_ns = started.elapsed().as_secs_f64() * 1.0e9 / N as f64;
        std::hint::black_box(sink);

        for block in [32usize, 64, 128, 256] {
            let mut blocked = nam_rs::Model::from_nam(&model).expect("builds");
            let mut buffer = vec![0.0f32; block];
            let started = Instant::now();
            for chunk in input.chunks(block) {
                buffer[..chunk.len()].copy_from_slice(chunk);
                blocked.process_buffer(&mut buffer[..chunk.len()]);
            }
            let block_ns = started.elapsed().as_secs_f64() * 1.0e9 / N as f64;
            std::hint::black_box(buffer[0]);
            println!(
                "{:<34} {:>5.0} ns/sample vs {sample_ns:>5.0} per-sample  ({:.2}x)",
                format!("{label} @ block {block}"),
                block_ns,
                sample_ns / block_ns
            );
        }
    }

    println!("\n-- realistic full chains --");
    let mut dsp = Dsp::new(SR);
    dsp.set_params(default_params());
    let all = measure("everything on (modeled amp+cab)", &mut dsp);

    let mut dsp = Dsp::new(SR);
    let mut p = default_params();
    p.cab_on = false;
    dsp.set_params(p);
    measure("everything on, cabinet slot off", &mut dsp);

    let mut dsp = Dsp::new(SR);
    let mut p = default_params();
    p.amp_on = false;
    dsp.set_params(p);
    measure("everything on, amp slot off", &mut dsp);

    let mut dsp = Dsp::new(SR);
    let mut p = default_params();
    p.cab_model = CabModel::Ir;
    dsp.set_params(p);
    let bytes = ir_wav(0.2, 2);
    dsp.load_ir_wav(&bytes, "chain").expect("IR loads");
    dsp.begin_block();
    let with_ir = measure("everything on + stereo IR cab", &mut dsp);

    let json = nam_json(NAM_SHAPES[1].1);
    let mut dsp = Dsp::new(SR);
    let mut p = default_params();
    p.tone_engine = ToneEngineKind::NamCapture;
    p.cab_model = CabModel::Ir;
    dsp.set_params(p);
    dsp.load_nam_capture_json(&json, "chain", false, true)
        .expect("NAM loads");
    dsp.load_ir_wav(&bytes, "chain").expect("IR loads");
    dsp.begin_block();
    let nam_chain = measure("everything on + NAM + stereo IR", &mut dsp);
    println!(
        "\n  total reported latency of that chain: {} samples ({:.1} ms)",
        dsp.latency_samples(),
        dsp.latency_samples() as f32 / SR * 1_000.0
    );
    println!(
        "\n  modeled {all:.0} % / IR {with_ir:.0} % / NAM+IR {nam_chain:.0} % of one core per instance"
    );
}

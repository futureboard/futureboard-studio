//! Offline factory-preset audit — NOT part of the shipping plugin.
//!
//! Renders every factory preset through the real [`Dsp`] with a fixed
//! guitar-like DI signal and prints what each one actually does: output level,
//! peak headroom, how hard it is compressing, and its low/mid/high balance.
//!
//! The point is that preset values are *measured*, not guessed. The columns to
//! read first:
//!
//! * `rms` / `Δloud` — a factory bank whose presets differ by more than a few
//!   dB makes browsing a volume ride. `Δloud` is against the bank median.
//! * `peak` — anything at or above 0.0 dBFS clips the plugin output with a
//!   normal DI in front of it, and lights the editor's sticky clip indicator.
//! * `crest` — peak over RMS, against the DI's own crest printed in the footer.
//!   This is the column that says whether a preset matches its name: a "Clean"
//!   sitting 10 dB under the DI's crest is a crunch preset with a clean label.
//!   Pick Gain/Master from `--map`, which measures the same number per amp.
//!
//! `resid` (energy not explained by a pure rescale of the input) is included
//! for continuity with `drive_diagnostic`, but read it only for a single drive
//! stage: across a whole rig the cab and the time effects push it near 100 %
//! for everything, saturated or not, so `crest` is the honest character column.
//!
//! `trim` is what each preset's `outputTrim` would have to be to hit the bank
//! target without pushing its peaks past the ceiling — zero everywhere means
//! the committed values are already the measured ones.
//!
//! It renders the bank the editor's preset browser loads,
//! `rodharerist::factory_presets()` — there is no second copy to drift:
//!
//! ```text
//! cargo run -p rodharerist --example preset_audit --release
//! cargo run -p rodharerist --example preset_audit --release -- --map
//! ```

use std::f32::consts::TAU;

use builtin_dsp_core::StereoEffect;
use rodharerist::{AmpModel, CabModel, Dsp, MicModel, PATH_SLOTS, StageKind, default_params};
use rustfft::{FftPlanner, num_complex::Complex};

const SR: f32 = 48_000.0;
/// Long enough for a reverb tail and a few delay repeats to establish.
const SECONDS: f32 = 6.0;
const N: usize = (SR * SECONDS) as usize;
/// Skipped before measuring: stage smoothing, delay pre-fill, reverb build-up.
const SETTLE: usize = 24_000;

/// Loudness every factory preset is tuned to, with the DI below in front of it.
/// Presets are level-matched here rather than by retuning Gain/Master, so each
/// amp keeps the settings its tone actually calls for.
const TARGET_RMS_DB: f32 = -14.0;
/// Peak ceiling. Above this a preset lights the editor's sticky clip indicator
/// on a normal DI, so the trim is held down to respect it even if that leaves
/// the preset quieter than the target.
const PEAK_CEILING_DB: f32 = -1.5;

/// A deterministic stand-in for a DI'd guitar: an open-position chord struck
/// repeatedly, each string a decaying harmonic stack with a little pick noise.
/// Normalised to a -6 dBFS peak, which is where a hot humbucker DI actually
/// sits in a session.
fn test_signal() -> Vec<f32> {
    // E A D G B E, struck as an Em7-ish voicing.
    const STRINGS: [f32; 6] = [82.41, 123.47, 164.81, 196.00, 246.94, 329.63];
    const STRUM_SECONDS: f32 = 1.5;
    let mut buf = vec![0.0f32; N];
    let mut noise_state = 0x5EED_1234u32;
    for (n, sample) in buf.iter_mut().enumerate() {
        let t = n as f32 / SR;
        let in_strum = t % STRUM_SECONDS;
        let mut x = 0.0f32;
        for (s, &f0) in STRINGS.iter().enumerate() {
            // Strings are struck in sequence, ~12 ms apart, like a real strum.
            let onset = s as f32 * 0.012;
            if in_strum < onset {
                continue;
            }
            let age = in_strum - onset;
            // Higher partials decay faster — the usual plucked-string envelope.
            for h in 1..=8 {
                let f = f0 * h as f32;
                if f > 6_000.0 {
                    break;
                }
                let decay = (-age * (1.6 + 0.55 * h as f32)).exp();
                x += (t * f * TAU + s as f32).sin() * decay / h as f32;
            }
            // Pick attack: a short noise burst under the first few ms.
            if age < 0.004 {
                noise_state = noise_state
                    .wrapping_mul(1_664_525)
                    .wrapping_add(1_013_904_223);
                let noise = (noise_state >> 8) as f32 / (1 << 24) as f32 * 2.0 - 1.0;
                x += noise * 0.25 * (1.0 - age / 0.004);
            }
        }
        *sample = x;
    }
    let peak = buf.iter().fold(0.0f32, |m, &x| m.max(x.abs())).max(1.0e-9);
    let scale = 0.5 / peak; // -6 dBFS
    for sample in buf.iter_mut() {
        *sample *= scale;
    }
    buf
}

fn db(x: f32) -> f32 {
    20.0 * x.max(1.0e-9).log10()
}

fn rms(buf: &[f32]) -> f32 {
    (buf.iter().map(|x| x * x).sum::<f32>() / buf.len().max(1) as f32).sqrt()
}

/// Share of output energy not explained by a pure rescale of the input — a
/// cheap "how much waveshaping happened" number (same measure the drive
/// diagnostic uses).
fn residual(input: &[f32], output: &[f32]) -> f32 {
    let dot: f32 = input.iter().zip(output).map(|(a, b)| a * b).sum();
    let in_e: f32 = input.iter().map(|a| a * a).sum();
    let scale = dot / in_e.max(1.0e-9);
    let resid: f32 = input
        .iter()
        .zip(output)
        .map(|(a, b)| (b - a * scale).powi(2))
        .sum();
    let out_e: f32 = output.iter().map(|b| b * b).sum();
    resid / out_e.max(1.0e-9)
}

/// Average magnitude spectrum over the whole measured render (Welch: Hann
/// windows, 50 % overlap). Measuring one window at the end instead would read
/// a decayed strum and report no treble in anything.
fn spectrum(buf: &[f32]) -> Vec<f32> {
    const WINDOW: usize = 4_096;
    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(WINDOW);
    let hann: Vec<f32> = (0..WINDOW)
        .map(|n| 0.5 - 0.5 * (n as f32 * TAU / WINDOW as f32).cos())
        .collect();
    let mut acc = vec![0.0f32; WINDOW / 2];
    let mut frames = 0usize;
    let mut scratch = vec![Complex::new(0.0f32, 0.0); WINDOW];
    for start in (0..buf.len().saturating_sub(WINDOW)).step_by(WINDOW / 2) {
        for (i, slot) in scratch.iter_mut().enumerate() {
            *slot = Complex::new(buf[start + i] * hann[i], 0.0);
        }
        fft.process(&mut scratch);
        for (bin, slot) in acc.iter_mut().enumerate() {
            *slot += scratch[bin].norm_sqr();
        }
        frames += 1;
    }
    let scale = 1.0 / frames.max(1) as f32;
    for slot in acc.iter_mut() {
        *slot *= scale;
    }
    acc
}

/// Summed energy of `spectrum` between two frequencies.
fn band_energy(spectrum: &[f32], lo: f32, hi: f32) -> f32 {
    let bin_hz = SR / (spectrum.len() * 2) as f32;
    spectrum
        .iter()
        .enumerate()
        .filter(|(bin, _)| {
            let f = *bin as f32 * bin_hz;
            f >= lo && f < hi
        })
        .map(|(_, &e)| e)
        .sum()
}

/// Amp models in `AmpModel::ALL` order, with the cab each one is normally
/// paired with in the bank — the map has to include the cab, because a preset
/// is never auditioned without one.
const AMP_MAP: [(AmpModel, CabModel); 11] = [
    (AmpModel::Mandarin, CabModel::Vintage2x12),
    (AmpModel::Plexi, CabModel::Brit4x12),
    (AmpModel::Twin, CabModel::American2x12),
    (AmpModel::TopBoost, CabModel::OpenBack),
    (AmpModel::Recto, CabModel::Oversized4x12),
    (AmpModel::Jcm, CabModel::Brit4x12),
    (AmpModel::Slate, CabModel::Slo4x12),
    (AmpModel::Bassman, CabModel::Tweed1x12),
    (AmpModel::Boutique, CabModel::Vintage2x12),
    (AmpModel::Invader, CabModel::Uber4x12),
    (AmpModel::TweedCombo, CabModel::Tweed1x12),
];

/// Where each amp model actually sits on its Gain and Master travel, measured
/// through the DI and its cab. This is the table preset gain/master values are
/// picked from: `crest` says how hard the amp is compressing (the DI's own
/// crest is the clean reference), `rms` says what it costs in level.
fn print_amp_map(input: &[f32]) {
    let dry = &input[SETTLE..];
    let in_crest = db(input.iter().fold(0.0f32, |m, &x| m.max(x.abs())) / rms(dry).max(1.0e-9));
    println!("amp operating-region map — DI crest {in_crest:.1} dB (clean reference)");
    println!("each cell: rms dBFS / crest dB. Falling crest = the amp compressing.\n");

    let gains = [2.0f32, 4.0, 6.0, 8.0, 10.0];
    let masters = [4.0f32, 7.0, 10.0];
    print!("{:<10} {:<7}", "amp", "master");
    for gain in gains {
        print!("{:>16}", format!("gain {gain:.0}"));
    }
    println!();

    for (amp, cab) in AMP_MAP {
        for master in masters {
            print!("{:<10} {:<7.0}", format!("{amp:?}"), master);
            for gain in gains {
                let mut p = default_params();
                p.stage_order = [None; PATH_SLOTS];
                p.stage_order[0] = Some(StageKind::Amp);
                p.stage_order[1] = Some(StageKind::Cab);
                p.amp_on = true;
                p.cab_on = true;
                p.amp_model = amp;
                p.cab_model = cab;
                p.amp_gain = gain;
                p.amp_bass = 5.0;
                p.amp_middle = 5.0;
                p.amp_treble = 5.0;
                p.amp_presence = 5.0;
                p.amp_master = master;
                p.mic_model = MicModel::Dynamic;
                p.cab_mic = 40.0;
                p.cab_dist = 25.0;
                let mut dsp = Dsp::new(SR);
                dsp.set_params(p);
                let out: Vec<f32> = input
                    .iter()
                    .map(|&x| dsp.process_stereo(x, x).0)
                    .skip(SETTLE)
                    .collect();
                let r = rms(&out);
                let p = out.iter().fold(0.0f32, |m, &x| m.max(x.abs()));
                print!(
                    "{:>16}",
                    format!("{:.1}/{:.1}", db(r), db(p / r.max(1.0e-9)))
                );
            }
            println!();
        }
    }
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("--map") {
        print_amp_map(&test_signal());
        return;
    }
    let presets = rodharerist::factory_presets();

    let input = test_signal();
    let dry = &input[SETTLE..];
    let in_rms = db(rms(dry));
    let in_peak = db(input.iter().fold(0.0f32, |m, &x| m.max(x.abs())));
    println!(
        "input: {} presets, DI peak {in_peak:.1} dBFS, rms {in_rms:.1} dBFS, {SECONDS:.0} s @ {SR:.0} Hz\n",
        presets.len()
    );

    struct Row {
        id: String,
        name: String,
        rms_db: f32,
        peak_db: f32,
        crest_db: f32,
        resid: f32,
        low: f32,
        mid: f32,
        high: f32,
        path_len: usize,
    }

    let mut rows = Vec::new();
    for preset in presets {
        let mut dsp = Dsp::new(SR);
        dsp.set_params(preset.params.clone());
        let out: Vec<f32> = input
            .iter()
            .map(|&x| dsp.process_stereo(x, x).0)
            .skip(SETTLE)
            .collect();
        assert!(
            out.iter().all(|x| x.is_finite()),
            "`{}` produced a non-finite sample",
            preset.id
        );
        let spec = spectrum(&out);
        let low = band_energy(&spec, 80.0, 250.0);
        let mid = band_energy(&spec, 250.0, 2_000.0);
        let high = band_energy(&spec, 2_000.0, 6_000.0);
        let total = (low + mid + high).max(1.0e-30);
        let out_rms = rms(&out);
        let out_peak = out.iter().fold(0.0f32, |m, &x| m.max(x.abs()));
        rows.push(Row {
            id: preset.id.to_string(),
            name: preset.name.to_string(),
            rms_db: db(out_rms),
            peak_db: db(out_peak),
            crest_db: db(out_peak / out_rms.max(1.0e-9)),
            resid: residual(dry, &out) * 100.0,
            low: low / total * 100.0,
            mid: mid / total * 100.0,
            high: high / total * 100.0,
            path_len: preset.params.stage_order.iter().flatten().count(),
        });
    }

    // Median of the non-empty presets is the bank's loudness reference.
    let mut loud: Vec<f32> = rows
        .iter()
        .filter(|r| r.path_len > 0)
        .map(|r| r.rms_db)
        .collect();
    loud.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = loud.get(loud.len() / 2).copied().unwrap_or(0.0);

    let in_crest = db(input.iter().fold(0.0f32, |m, &x| m.max(x.abs())) / rms(dry).max(1.0e-9));
    println!(
        "{:<5} {:<22} {:>7} {:>7} {:>7} {:>7} {:>7}  {:>5} {:>5} {:>5} {:>7}",
        "id", "name", "rms", "Δloud", "peak", "crest", "resid", "low", "mid", "high", "trim"
    );
    for row in &rows {
        if row.path_len == 0 {
            println!("{:<5} {:<22} {:>7}", row.id, row.name, "(empty)");
            continue;
        }
        // A preset short of the target is only a fault if it had room to be
        // louder. The dynamic ones (a clean amp, a deep phaser) run out of peak
        // headroom first and correctly sit lower in RMS — flagging those would
        // just be asking to squash them.
        let peak_limited = row.peak_db > PEAK_CEILING_DB - 3.0;
        let flag = if row.peak_db >= 0.0 {
            " CLIP"
        } else if row.rms_db > TARGET_RMS_DB + 3.0
            || (row.rms_db < TARGET_RMS_DB - 3.0 && !peak_limited)
        {
            " LEVEL"
        } else {
            ""
        };
        // What `outputTrim` this preset would need to sit at the bank target
        // without letting its peaks past the ceiling. Rounded to 0.5 dB — the
        // preset table stores a number a human has to be able to read.
        let to_target = TARGET_RMS_DB - row.rms_db;
        let to_ceiling = PEAK_CEILING_DB - row.peak_db;
        let trim = (to_target.min(to_ceiling) * 2.0).round() / 2.0;
        println!(
            "{:<5} {:<22} {:>7.1} {:>+7.1} {:>7.1} {:>7.1} {:>6.1}%  {:>4.0}% {:>4.0}% {:>4.0}% {:>+7.1}{flag}",
            row.id,
            row.name,
            row.rms_db,
            row.rms_db - median,
            row.peak_db,
            row.crest_db,
            row.resid,
            row.low,
            row.mid,
            row.high,
            trim,
        );
    }
    println!(
        "\nbank median rms {median:.1} dBFS (Δloud is measured against it); \
         DI crest {in_crest:.1} dB — an output crest far below it is the preset compressing"
    );
}

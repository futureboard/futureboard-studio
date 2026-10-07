//! Test-only measurement harness: renders VerbSpace's wet impulse response and
//! a held sine at 48 kHz and reduces them to the numbers a listener hears —
//! per-band RT60 (Schroeder integration), first arrival, centre time, echo
//! density, level, spectral tilt, stereo correlation and modulation spread.
//!
//! Only the public `Dsp` API is used: params by id, exactly as an editor
//! sends them.

use builtin_dsp_core::StereoEffect;

use crate::{Dsp, Params};

pub const SR: f32 = 48_000.0;

/// What one render measures.
#[derive(Debug, Clone, Copy, Default)]
pub struct Metrics {
    /// First sample above −40 dB of the peak, in ms.
    pub first_ms: f32,
    /// Energy centre time (Ts), in ms.
    pub centre_ms: f32,
    /// Time after the first arrival at which the normalised echo density
    /// (Abel & Huang, 20 ms window) first reaches 0.9, in ms.
    pub mixing_ms: f32,
    /// Mean echo density 10–40 ms after the first arrival.
    pub early_density: f32,
    /// RT60 of the octave bands at 250 Hz, 1 kHz, 4 kHz and 8 kHz, in s.
    pub rt: [f32; 4],
    /// Broadband RT60 (T20 on the full-band EDC), in s.
    pub rt_broad: f32,
    /// Wet level for a steady white-noise input, in dB against the input.
    pub level_db: f32,
    /// Energy in the first 80 ms after the first arrival, in dB.
    pub early_db: f32,
    /// Energy density (per hertz) of the 4 kHz octave against the 500 Hz
    /// octave, in dB: 0 for a white tail, negative for a dark one.
    pub tilt_db: f32,
    /// Time from the first arrival to the loudest 10 ms of the response:
    /// how long the room takes to build.
    pub build_ms: f32,
    /// Correlation of left and right over the tail, for a mono input.
    pub correlation: f32,
}

pub const BAND_HZ: [f32; 4] = [250.0, 1_000.0, 4_000.0, 8_000.0];

/// A DSP at `base` with `edits` applied by id, reset to silence.
pub fn dsp_for(base: &Params, edits: &[(&str, f32)]) -> Dsp {
    let mut dsp = Dsp::new(SR);
    dsp.set_params(base.clone());
    for (id, value) in edits {
        assert!(dsp.apply_ui_param(id, *value), "unknown param {id}");
    }
    dsp.reset();
    dsp
}

/// The wet impulse response for a mono unit impulse.
pub fn impulse_response(dsp: &mut Dsp, frames: usize) -> (Vec<f32>, Vec<f32>) {
    let mut left = Vec::with_capacity(frames);
    let mut right = Vec::with_capacity(frames);
    for n in 0..frames {
        let x = if n == 0 { 1.0 } else { 0.0 };
        let (l, r) = dsp.process_stereo(x, x);
        assert!(l.is_finite() && r.is_finite(), "non-finite at {n}");
        left.push(l);
        right.push(r);
    }
    (left, right)
}

/// RBJ band-pass (0 dB peak), as `(b0, b1, b2, a1, a2)`.
fn bandpass(hz: f32, q: f32) -> [f64; 5] {
    let w = std::f64::consts::TAU * f64::from(hz) / f64::from(SR);
    let alpha = w.sin() / (2.0 * f64::from(q));
    let a0 = 1.0 + alpha;
    [
        alpha / a0,
        0.0,
        -alpha / a0,
        -2.0 * w.cos() / a0,
        (1.0 - alpha) / a0,
    ]
}

fn run_biquad(c: [f64; 5], x: &[f64]) -> Vec<f64> {
    let (mut x1, mut x2, mut y1, mut y2) = (0.0, 0.0, 0.0, 0.0);
    x.iter()
        .map(|&x0| {
            let y = c[0] * x0 + c[1] * x1 + c[2] * x2 - c[3] * y1 - c[4] * y2;
            x2 = x1;
            x1 = x0;
            y2 = y1;
            y1 = y;
            y
        })
        .collect()
}

/// An octave band of `x`: two cascaded band-passes.
pub fn octave(x: &[f64], hz: f32) -> Vec<f64> {
    let c = bandpass(hz, std::f32::consts::SQRT_2);
    run_biquad(c, &run_biquad(c, x))
}

/// RT60 from the Schroeder backward integral: a least-squares line through
/// the decay curve between −5 and −25 dB (T20), extrapolated to 60 dB.
pub fn rt60(signal: &[f64]) -> f32 {
    let mut edc: Vec<f64> = signal.iter().map(|x| x * x).collect();
    for i in (0..edc.len() - 1).rev() {
        edc[i] += edc[i + 1];
    }
    let total = edc[0].max(1.0e-300);
    let (mut n, mut sx, mut sy, mut sxx, mut sxy) = (0.0f64, 0.0, 0.0, 0.0, 0.0);
    for (i, e) in edc.iter().enumerate() {
        let db = 10.0 * (e / total).max(1.0e-300).log10();
        if db > -5.0 {
            continue;
        }
        if db < -25.0 {
            break;
        }
        let t = i as f64 / f64::from(SR);
        n += 1.0;
        sx += t;
        sy += db;
        sxx += t * t;
        sxy += t * db;
    }
    if n < 8.0 {
        return 0.0;
    }
    let slope = (n * sxy - sx * sy) / (n * sxx - sx * sx);
    if slope >= 0.0 {
        return f32::INFINITY;
    }
    (-60.0 / slope) as f32
}

/// Normalised echo density of `x` around each sample `at`, over a 20 ms
/// window: the share of samples beyond one standard deviation, over the
/// share a Gaussian puts there.
fn echo_density(x: &[f64], at: usize) -> f64 {
    let half = (0.010 * SR) as usize;
    let start = at.saturating_sub(half);
    let end = (at + half).min(x.len());
    let window = &x[start..end];
    let n = window.len() as f64;
    let sigma = (window.iter().map(|v| v * v).sum::<f64>() / n).sqrt();
    if sigma == 0.0 {
        return 0.0;
    }
    let beyond = window.iter().filter(|v| v.abs() > sigma).count() as f64;
    beyond / n / 0.317_310_5
}

pub fn measure(left: &[f32], right: &[f32]) -> Metrics {
    let l: Vec<f64> = left.iter().map(|v| f64::from(*v)).collect();
    let r: Vec<f64> = right.iter().map(|v| f64::from(*v)).collect();
    let sum: Vec<f64> = l.iter().zip(&r).map(|(a, b)| (a + b) * 0.5).collect();
    let ms = |i: usize| i as f32 / SR * 1_000.0;

    let energy: Vec<f64> = l.iter().zip(&r).map(|(a, b)| a * a + b * b).collect();
    let peak = energy.iter().copied().fold(0.0f64, f64::max);
    let first = energy.iter().position(|e| *e > peak * 1.0e-4).unwrap_or(0);
    let total: f64 = energy.iter().sum();
    let centre = energy
        .iter()
        .enumerate()
        .map(|(i, e)| i as f64 * e)
        .sum::<f64>()
        / total.max(1.0e-300);

    let step = (0.002 * SR) as usize;
    let mut mixing = f32::NAN;
    let mut at = first;
    while at < sum.len().min(first + SR as usize) {
        if echo_density(&sum, at) >= 0.9 {
            mixing = ms(at - first);
            break;
        }
        at += step;
    }
    let (from, to) = (first + (0.010 * SR) as usize, first + (0.040 * SR) as usize);
    let mut density = 0.0;
    let mut count = 0.0;
    let mut at = from;
    while at < to.min(sum.len()) {
        density += echo_density(&sum, at);
        count += 1.0;
        at += step;
    }

    let rt = BAND_HZ.map(|hz| rt60(&octave(&sum, hz)));
    let early_end = (first + (0.080 * SR) as usize).min(energy.len());
    let early: f64 = energy[first..early_end].iter().sum();
    let band_energy = |hz: f32| octave(&sum, hz).iter().map(|v| v * v).sum::<f64>();
    let tilt = 10.0 * (band_energy(4_000.0) / 8.0 / band_energy(500.0).max(1e-300)).log10();
    let window = (0.010 * SR) as usize;
    let mut loudest = (0.0f64, first);
    let mut at = first;
    while at + window < energy.len().min(first + SR as usize) {
        let e: f64 = energy[at..at + window].iter().sum();
        if e > loudest.0 {
            loudest = (e, at);
        }
        at += window / 2;
    }

    let tail = (first + (0.020 * SR) as usize).min(l.len());
    let (mut ll, mut rr, mut lr) = (0.0f64, 0.0f64, 0.0f64);
    for i in tail..l.len() {
        ll += l[i] * l[i];
        rr += r[i] * r[i];
        lr += l[i] * r[i];
    }

    Metrics {
        first_ms: ms(first),
        centre_ms: (centre / f64::from(SR) * 1_000.0) as f32,
        mixing_ms: mixing,
        early_density: (density / f64::max(count, 1.0)) as f32,
        rt,
        rt_broad: rt60(&sum),
        level_db: (10.0 * (total * 0.5).max(1e-300).log10()) as f32,
        early_db: (10.0 * (early * 0.5).max(1e-300).log10()) as f32,
        tilt_db: tilt as f32,
        build_ms: ms(loudest.1 - first),
        correlation: (lr / (ll * rr).sqrt().max(1e-300)) as f32,
    }
}

/// RMS frequency deviation of the wet output around a held 1 kHz sine, in
/// Hz: how far modulation smears a pure tone. A static tank leaves only the
/// analysis window's own spread (about 0.2 Hz).
pub fn modulation_spread_hz(dsp: &mut Dsp) -> f32 {
    let f0 = 1_000.0f64;
    let settle = (1.5 * SR) as usize;
    let len = (3.0 * SR) as usize;
    let mut out = Vec::with_capacity(len);
    for n in 0..settle + len {
        let x = (std::f64::consts::TAU * f0 * n as f64 / f64::from(SR)).sin() as f32 * 0.5;
        let (l, r) = dsp.process_stereo(x, x);
        if n >= settle {
            let hann = 0.5 - 0.5 * (std::f64::consts::TAU * (n - settle) as f64 / len as f64).cos();
            out.push(f64::from(l + r) * 0.5 * hann);
        }
    }
    let (mut weighted, mut power) = (0.0f64, 0.0f64);
    let mut offset = -40.0f64;
    while offset <= 40.0 {
        let w = std::f64::consts::TAU * (f0 + offset) / f64::from(SR);
        let coeff = 2.0 * w.cos();
        let (mut s1, mut s2) = (0.0f64, 0.0f64);
        for x in &out {
            let s0 = x + coeff * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        let p = s1 * s1 + s2 * s2 - coeff * s1 * s2;
        weighted += offset * offset * p;
        power += p;
        offset += 0.25;
    }
    (weighted / power.max(1e-300)).sqrt() as f32
}

/// Long enough to see `rt` seconds of decay down past −60 dB.
pub fn frames_for(rt: f32) -> usize {
    ((rt * 1.4 + 0.4).clamp(1.5, 32.0) * SR) as usize
}

/// One sweep row: `id` at `value` over `base`.
pub fn row(base: &Params, id: &str, value: f32, longest_rt: f32) -> (Metrics, f32) {
    let mut dsp = dsp_for(base, &[(id, value)]);
    let (l, r) = impulse_response(&mut dsp, frames_for(longest_rt));
    let metrics = measure(&l, &r);
    let mut dsp = dsp_for(base, &[(id, value)]);
    (metrics, modulation_spread_hz(&mut dsp))
}

pub fn header() -> String {
    format!(
        "{:<12} {:>8} | {:>7} {:>7} {:>7} {:>6} {:>5} | {:>6} {:>6} {:>6} {:>6} {:>6} | {:>6} {:>6} {:>6} {:>5} | {:>5}",
        "param",
        "value",
        "first",
        "centre",
        "mixing",
        "build",
        "dens",
        "rt250",
        "rt1k",
        "rt4k",
        "rt8k",
        "rtBB",
        "level",
        "early",
        "tilt",
        "corr",
        "modHz"
    )
}

/// The sweep table: every param from its minimum to its maximum over the
/// defaults at 100 % wet. `cargo test -p verbspace --release -- --ignored
/// --nocapture sweep_table`.
#[test]
#[ignore = "prints the measurement table; slow in debug"]
fn sweep_table() {
    let mut base = crate::default_params();
    base.mix = 100.0;
    let sweeps: &[(&str, &[f32])] = &[
        ("mode", &[0.0, 1.0, 2.0, 3.0, 4.0]),
        ("predelayMs", &[0.0, 20.0, 100.0, 250.0, 500.0]),
        ("size", &[0.0, 25.0, 50.0, 75.0, 100.0]),
        ("decaySec", &[0.1, 0.5, 2.4, 8.0, 20.0]),
        ("diffusion", &[0.0, 25.0, 50.0, 75.0, 100.0]),
        ("damping", &[0.0, 25.0, 50.0, 75.0, 100.0]),
        ("bassMult", &[0.2, 0.5, 1.0, 2.0, 3.0]),
        ("bassFreqHz", &[50.0, 120.0, 250.0, 500.0, 1_000.0]),
        (
            "dampFreqHz",
            &[1_000.0, 2_000.0, 4_000.0, 8_000.0, 16_000.0],
        ),
        ("earlyLate", &[0.0, 25.0, 50.0, 75.0, 100.0]),
        ("modDepth", &[0.0, 25.0, 50.0, 75.0, 100.0]),
        ("modRateHz", &[0.05, 0.3, 1.0, 2.5, 5.0]),
        ("lowCutHz", &[20.0, 90.0, 250.0, 500.0, 1_000.0]),
        (
            "highCutHz",
            &[1_000.0, 3_000.0, 6_000.0, 12_000.0, 20_000.0],
        ),
        ("width", &[0.0, 50.0, 100.0, 150.0, 200.0]),
        ("outputDb", &[-24.0, -12.0, 0.0, 6.0, 12.0]),
    ];
    println!("{}", header());
    for (id, values) in sweeps {
        for value in *values {
            let longest = if *id == "decaySec" { *value * 2.0 } else { 6.0 };
            let (m, spread) = row(&base, id, *value, longest);
            println!("{}", format_row(id, *value, &m, spread));
        }
        println!();
    }
}

pub fn format_row(id: &str, value: f32, m: &Metrics, spread: f32) -> String {
    format!(
        "{:<12} {:>8.2} | {:>7.1} {:>7.1} {:>7.1} {:>6.1} {:>5.2} | {:>6.2} {:>6.2} {:>6.2} {:>6.2} {:>6.2} | {:>6.1} {:>6.1} {:>6.1} {:>5.2} | {:>5.2}",
        id,
        value,
        m.first_ms,
        m.centre_ms,
        m.mixing_ms,
        m.build_ms,
        m.early_density,
        m.rt[0],
        m.rt[1],
        m.rt[2],
        m.rt[3],
        m.rt_broad,
        m.level_db,
        m.early_db,
        m.tilt_db,
        m.correlation,
        spread
    )
}

/// 16-bit stereo WAV at [`SR`].
fn write_wav(path: &std::path::Path, left: &[f32], right: &[f32]) -> std::io::Result<()> {
    let frames = left.len() as u32;
    let data_len = frames * 4;
    let mut bytes = Vec::with_capacity(44 + data_len as usize);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&(SR as u32).to_le_bytes());
    bytes.extend_from_slice(&(SR as u32 * 4).to_le_bytes());
    bytes.extend_from_slice(&4u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_len.to_le_bytes());
    for (l, r) in left.iter().zip(right) {
        for v in [l, r] {
            let s = (v.clamp(-1.0, 1.0) * 32_767.0).round() as i16;
            bytes.extend_from_slice(&s.to_le_bytes());
        }
    }
    std::fs::write(path, bytes)
}

/// A dry snare-ish hit: a pitched thump under a noise burst.
fn drum_hit(frames: usize) -> Vec<f32> {
    let mut seed = 0x2468_ace1u32;
    (0..frames)
        .map(|n| {
            let t = n as f32 / SR;
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = (seed >> 9) as f32 / (1u32 << 23) as f32 - 0.5;
            let thump = (std::f32::consts::TAU * 180.0 * t).sin() * (-t * 30.0).exp();
            0.5 * thump + 0.9 * noise * (-t * 45.0).exp()
        })
        .collect()
}

/// Renders every factory preset to `VERBSPACE_WAV_DIR`: the wet impulse
/// response and a drum hit at the preset's own mix, for listening.
/// `VERBSPACE_WAV_DIR=<dir> cargo test -p verbspace --release -- --ignored
/// render_preset_wavs`.
#[test]
#[ignore = "writes WAV files for listening"]
fn render_preset_wavs() {
    let Some(dir) = std::env::var_os("VERBSPACE_WAV_DIR") else {
        eprintln!("set VERBSPACE_WAV_DIR to render");
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();
    for preset in crate::factory_presets() {
        let slug: String = preset
            .name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect();
        let rt = preset.params.decay_sec * preset.params.bass_mult.max(1.0);
        let frames = ((rt * 1.2 + 0.6).min(20.0) * SR) as usize;

        let mut wet = preset.params.clone();
        wet.mix = 100.0;
        let mut dsp = dsp_for(&wet, &[]);
        let (l, r) = impulse_response(&mut dsp, frames);
        let peak = l
            .iter()
            .chain(&r)
            .fold(0.0f32, |m, v| m.max(v.abs()))
            .max(1.0e-6);
        let scale = 0.8 / peak;
        let (l, r): (Vec<f32>, Vec<f32>) = l
            .iter()
            .zip(&r)
            .map(|(a, b)| (a * scale, b * scale))
            .unzip();
        write_wav(&dir.join(format!("{slug}-ir.wav")), &l, &r).unwrap();

        let hit = drum_hit(frames);
        let mut dsp = dsp_for(&preset.params, &[]);
        let (l, r): (Vec<f32>, Vec<f32>) = hit.iter().map(|x| dsp.process_stereo(*x, *x)).unzip();
        write_wav(&dir.join(format!("{slug}-drum.wav")), &l, &r).unwrap();
    }
    // One hit through a sweep of Size, Decay and Damping, so each knob can
    // be heard on its own against the defaults.
    for (id, values) in [
        ("size", [0.0f32, 50.0, 100.0]),
        ("decaySec", [0.4, 2.4, 8.0]),
        ("damping", [0.0, 50.0, 100.0]),
        ("diffusion", [0.0, 50.0, 100.0]),
        ("earlyLate", [0.0, 50.0, 100.0]),
    ] {
        for value in values {
            let frames = (6.0 * SR) as usize;
            let hit = drum_hit(frames);
            let mut base = crate::default_params();
            base.mix = 40.0;
            let mut dsp = dsp_for(&base, &[(id, value)]);
            let (l, r): (Vec<f32>, Vec<f32>) =
                hit.iter().map(|x| dsp.process_stereo(*x, *x)).unzip();
            write_wav(&dir.join(format!("sweep-{id}-{value}.wav")), &l, &r).unwrap();
        }
    }
    eprintln!("wrote WAVs to {}", dir.display());
}

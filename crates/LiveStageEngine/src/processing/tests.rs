//! The strip processor's tests: responses, dynamics, delay, the mono path,
//! hostile settings, and the sweep test (a dragged control must not step at
//! block edges; a switch must not click).
//!
//! ```txt
//! cargo test -p livestage-engine --no-default-features --lib processing -- --nocapture
//! cargo test --release -p livestage-engine --no-default-features --lib processing::tests::cost -- --nocapture
//! ```

use std::time::Instant;

use super::*;

const SR: u32 = 48_000;
const BLOCK: usize = 128;

fn settled(p: &Processing, sr: u32) -> StripProcessor {
    let mut s = StripProcessor::new(sr);
    s.set(p);
    s.settle();
    s
}

fn all_off() -> Processing {
    let mut p = Processing::default();
    p.eq.on = false;
    p
}

/// Every section on, doing something.
fn busy() -> Processing {
    let mut p = Processing::default();
    p.hpf = Hpf {
        on: true,
        hz: 40.0,
        slope_db: 24,
    };
    p.gate = Gate {
        on: true,
        threshold_db: -70.0,
        range_db: -40.0,
        attack_ms: 0.5,
        hold_ms: 20.0,
        release_ms: 150.0,
    };
    p.eq.on = true;
    p.eq.bands = [
        EqBand {
            kind: EqKind::LowShelf,
            hz: 100.0,
            gain_db: 4.0,
            q: 0.7,
        },
        EqBand {
            kind: EqKind::Bell,
            hz: 400.0,
            gain_db: -3.0,
            q: 1.0,
        },
        EqBand {
            kind: EqKind::Bell,
            hz: 2_500.0,
            gain_db: 5.0,
            q: 1.5,
        },
        EqBand {
            kind: EqKind::HighShelf,
            hz: 8_000.0,
            gain_db: -2.0,
            q: 0.7,
        },
    ];
    p.comp = Comp {
        on: true,
        threshold_db: -24.0,
        ratio: 3.0,
        attack_ms: 10.0,
        release_ms: 150.0,
        knee_db: 6.0,
        makeup_db: 3.0,
    };
    p.delay = Delay { on: true, ms: 5.0 };
    p
}

/// [`busy`] with the gate shut (threshold above the signal), so its range
/// and on/off are heard.
fn busy_gate_shut() -> Processing {
    let mut p = busy();
    p.gate.threshold_db = 0.0;
    p
}

// --- Responses ---------------------------------------------------------------

/// Steady-state gain, dB, of a sine at `hz` through `p` (left of a stereo
/// pair), measured over exactly one second after half a second of settling.
fn sine_gain_db(p: &Processing, sr: u32, hz: f64, amplitude: f32) -> f32 {
    let mut s = settled(p, sr);
    let sr_us = sr as usize;
    let total = sr_us * 3 / 2;
    let mut l = vec![0.0f32; BLOCK];
    let mut r = vec![0.0f32; BLOCK];
    let (mut sum_in, mut sum_out) = (0.0f64, 0.0f64);
    let mut n = 0;
    while n < total {
        let len = BLOCK.min(total - n);
        for i in 0..len {
            let x =
                (std::f64::consts::TAU * hz * (n + i) as f64 / sr as f64).sin() as f32 * amplitude;
            l[i] = x;
            r[i] = x;
        }
        let input = l.clone();
        s.process(&mut l[..len], &mut r[..len], true);
        for i in 0..len {
            if n + i >= sr_us / 2 {
                sum_in += (input[i] as f64).powi(2);
                sum_out += (l[i] as f64).powi(2);
            }
        }
        n += len;
    }
    (10.0 * (sum_out / sum_in).log10()) as f32
}

fn assert_near(what: &str, got: f32, want: f32, tolerance: f32) {
    assert!(
        (got - want).abs() <= tolerance,
        "{what}: got {got:.3}, want {want:.3} ± {tolerance}"
    );
}

#[test]
fn hpf_corner_and_one_octave_below() {
    for (slope, octave_below) in [(12u8, -12.30f32), (18, -18.13), (24, -24.10)] {
        let mut p = all_off();
        p.hpf = Hpf {
            on: true,
            hz: 100.0,
            slope_db: slope,
        };
        assert_near(
            &format!("{slope} dB/oct at the corner"),
            sine_gain_db(&p, SR, 100.0, 0.25),
            -3.01,
            0.1,
        );
        assert_near(
            &format!("{slope} dB/oct one octave below"),
            sine_gain_db(&p, SR, 50.0, 0.25),
            octave_below,
            0.2,
        );
        assert_near(
            &format!("{slope} dB/oct in the pass band"),
            sine_gain_db(&p, SR, 1_000.0, 0.25),
            0.0,
            0.05,
        );
    }
    // The lowest corner at the highest rate: f64 coefficients hold it.
    let mut p = all_off();
    p.hpf = Hpf {
        on: true,
        hz: 20.0,
        slope_db: 24,
    };
    assert_near(
        "20 Hz at 192 kHz",
        sine_gain_db(&p, 192_000, 20.0, 0.25),
        -3.01,
        0.1,
    );
    assert_near(
        "20 Hz at 192 kHz, octave below",
        sine_gain_db(&p, 192_000, 10.0, 0.25),
        -24.10,
        0.2,
    );
}

fn one_band(kind: EqKind, hz: f32, gain_db: f32, q: f32) -> Processing {
    let mut p = all_off();
    p.eq.on = true;
    p.eq.bands[1] = EqBand {
        kind,
        hz,
        gain_db,
        q,
    };
    p
}

#[test]
fn eq_kinds_hit_their_gain() {
    let bell = one_band(EqKind::Bell, 1_000.0, 9.0, 1.0);
    assert_near(
        "bell +9 at 1 kHz",
        sine_gain_db(&bell, SR, 1_000.0, 0.1),
        9.0,
        0.05,
    );
    let notch = one_band(EqKind::Bell, 250.0, -12.0, 4.0);
    assert_near(
        "bell -12 at 250 Hz",
        sine_gain_db(&notch, SR, 250.0, 0.1),
        -12.0,
        0.05,
    );
    assert_near(
        "bell -12 far away",
        sine_gain_db(&notch, SR, 4_000.0, 0.1),
        0.0,
        0.1,
    );

    // RBJ shelves are half their gain at the set frequency.
    let low = one_band(EqKind::LowShelf, 200.0, 12.0, 0.707);
    assert_near(
        "low shelf at f0",
        sine_gain_db(&low, SR, 200.0, 0.1),
        6.0,
        0.05,
    );
    assert_near(
        "low shelf deep below",
        sine_gain_db(&low, SR, 20.0, 0.1),
        12.0,
        0.3,
    );
    assert_near(
        "low shelf far above",
        sine_gain_db(&low, SR, 5_000.0, 0.1),
        0.0,
        0.1,
    );
    let high = one_band(EqKind::HighShelf, 4_000.0, -10.0, 0.707);
    assert_near(
        "high shelf at f0",
        sine_gain_db(&high, SR, 4_000.0, 0.1),
        -5.0,
        0.05,
    );
    assert_near(
        "high shelf far above",
        sine_gain_db(&high, SR, 18_000.0, 0.1),
        -10.0,
        0.5,
    );
    assert_near(
        "high shelf far below",
        sine_gain_db(&high, SR, 200.0, 0.1),
        0.0,
        0.1,
    );
}

// --- Gate --------------------------------------------------------------------

/// Plays `ms` of a 1 kHz sine at `db` through a mono strip; the output/input
/// level (linear) over the last 10 ms, and the meters.
fn drive(s: &mut StripProcessor, frame: &mut usize, db: f32, ms: f32) -> (f32, ProcessingMeters) {
    let amplitude = 10f32.powf(db / 20.0);
    let total = (ms * 1.0e-3 * SR as f32) as usize;
    let tail = (0.01 * SR as f32) as usize;
    let mut buf = [0.0f32; BLOCK];
    let mut right = [7.0f32; BLOCK];
    let (mut sum_in, mut sum_out) = (0.0f64, 0.0f64);
    let mut done = 0;
    while done < total {
        let len = BLOCK.min(total - done);
        let mut input = [0.0f32; BLOCK];
        for i in 0..len {
            let t = (*frame + i) as f64 / SR as f64;
            input[i] = (std::f64::consts::TAU * 1_000.0 * t).sin() as f32 * amplitude;
        }
        buf[..len].copy_from_slice(&input[..len]);
        s.process(&mut buf[..len], &mut right[..len], false);
        for i in 0..len {
            if done + i >= total - tail {
                sum_in += (input[i] as f64).powi(2);
                sum_out += (buf[i] as f64).powi(2);
            }
        }
        *frame += len;
        done += len;
    }
    assert!(right.iter().all(|&x| x == 7.0), "mono must not touch right");
    (((sum_out / sum_in).sqrt()) as f32, s.meters())
}

#[test]
fn gate_opens_closes_with_hysteresis_and_reaches_its_range() {
    let mut p = all_off();
    p.gate = Gate {
        on: true,
        threshold_db: -30.0,
        range_db: -80.0,
        attack_ms: 0.5,
        hold_ms: 20.0,
        release_ms: 50.0,
    };
    let mut s = settled(&p, SR);
    let mut frame = 0;

    let (gain, m) = drive(&mut s, &mut frame, -27.0, 200.0);
    assert!(
        m.gate_open && m.gate_db == 0.0,
        "above threshold: open, {m:?}"
    );
    assert_near("open passes", gain, 1.0, 1.0e-4);

    // Inside the 4 dB hysteresis band (between −34 and −30): stays open.
    let (gain, m) = drive(&mut s, &mut frame, -32.0, 500.0);
    assert!(
        m.gate_open && m.gate_db == 0.0,
        "in the band: still open, {m:?}"
    );
    assert_near("still passes", gain, 1.0, 1.0e-4);

    // Under the band: closes, after hold and release, to a full mute.
    let (gain, m) = drive(&mut s, &mut frame, -37.0, 500.0);
    assert!(!m.gate_open, "under the band: closed, {m:?}");
    assert_near("full mute meter", m.gate_db, 80.0, 1.0e-3);
    assert_eq!(gain, 0.0, "a −80 dB range is a full mute");

    // Back into the band from below: stays shut (it must cross the threshold).
    let (gain, m) = drive(&mut s, &mut frame, -32.0, 500.0);
    assert!(
        !m.gate_open && gain == 0.0,
        "in the band from below: shut, {m:?}"
    );

    let (gain, m) = drive(&mut s, &mut frame, -27.0, 100.0);
    assert!(m.gate_open && m.gate_db == 0.0, "reopens, {m:?}");
    assert_near("reopened passes", gain, 1.0, 1.0e-4);

    // A partial range: attenuates by exactly that much once shut.
    p.gate.range_db = -20.0;
    s.set(&p);
    let (gain, m) = drive(&mut s, &mut frame, -40.0, 600.0);
    assert!(!m.gate_open);
    assert_near("range meter", m.gate_db, 20.0, 1.0e-3);
    assert_near("range gain", 20.0 * gain.log10(), -20.0, 0.01);
}

// --- Comp --------------------------------------------------------------------

#[test]
fn comp_gain_reduction_follows_the_static_curve() {
    // Threshold −20, ratio 4 (slope 0.75). Knee 6: from −23 to −17 the GR is
    // 0.75·(x − T + 3)² / 12.
    let cases: &[(f32, f32, f32, f32)] = &[
        // (knee, makeup, level, expected GR)
        (0.0, 0.0, -30.0, 0.0),
        (0.0, 0.0, -20.0, 0.0),
        (0.0, 0.0, -10.0, 7.5),
        (0.0, 6.0, -10.0, 7.5),
        (0.0, 0.0, -2.0, 13.5),
        (6.0, 0.0, -23.0, 0.0),
        (6.0, 0.0, -21.5, 0.140_625),
        (6.0, 0.0, -20.0, 0.562_5),
        (6.0, 0.0, -17.0, 2.25),
        (6.0, 0.0, -5.0, 11.25),
    ];
    for &(knee, makeup, level, expected) in cases {
        let mut p = all_off();
        p.comp = Comp {
            on: true,
            threshold_db: -20.0,
            ratio: 4.0,
            attack_ms: 1.0,
            release_ms: 50.0,
            knee_db: knee,
            makeup_db: makeup,
        };
        let mut s = settled(&p, SR);
        // DC: the peak detector sees a constant level.
        let dc = 10f32.powf(level / 20.0);
        let mut buf = [0.0f32; BLOCK];
        for _ in 0..(SR as usize / BLOCK) {
            buf.fill(dc);
            s.process(&mut buf, &mut [], false);
        }
        let m = s.meters();
        let what = format!("knee {knee}, level {level}");
        assert_near(&format!("{what}: GR meter"), m.comp_db, expected, 1.0e-3);
        let out_db = 20.0 * (buf[BLOCK - 1] / dc).log10();
        assert_near(
            &format!("{what}: output"),
            out_db,
            makeup - expected,
            1.0e-3,
        );
    }
}

// --- Delay -------------------------------------------------------------------

#[test]
fn delay_is_exact_in_samples() {
    for (sr, ms, block) in [
        (48_000u32, 10.0f32, 100usize),
        (44_100, 1_000.0, 1024),
        (96_000, 0.5, 7),
    ] {
        let mut p = all_off();
        p.delay = Delay { on: true, ms };
        let mut s = settled(&p, sr);
        let delay = (ms * 1.0e-3 * sr as f32).round() as usize;
        let total = delay + 3 * block;
        let mut out_l = Vec::with_capacity(total);
        let mut out_r = Vec::with_capacity(total);
        let mut n = 0;
        while n < total {
            let len = block.min(total - n);
            let mut l: Vec<f32> = (n..n + len)
                .map(|i| if i == 0 { 1.0 } else { 0.0 })
                .collect();
            let mut r: Vec<f32> = (n..n + len)
                .map(|i| if i == 3 { -0.5 } else { 0.0 })
                .collect();
            s.process(&mut l, &mut r, true);
            out_l.extend_from_slice(&l);
            out_r.extend_from_slice(&r);
            n += len;
        }
        for (i, (&a, &b)) in out_l.iter().zip(&out_r).enumerate() {
            let want_l = if i == delay { 1.0 } else { 0.0 };
            let want_r = if i == delay + 3 { -0.5 } else { 0.0 };
            assert_eq!((a, b), (want_l, want_r), "{sr} Hz, {ms} ms, frame {i}");
        }
    }
}

// --- Mono, pass-through, meters, settle ---------------------------------------

#[test]
fn mono_leaves_right_untouched() {
    let mut s = settled(&busy(), SR);
    let mut frame = 0;
    for _ in 0..200 {
        let mut l = [0.0f32; BLOCK];
        for (i, x) in l.iter_mut().enumerate() {
            *x = ((frame + i) as f32 * 0.05).sin() * 0.5;
        }
        let mut r: Vec<f32> = (0..BLOCK).map(|i| i as f32 * 0.25 - 3.0).collect();
        let before = r.clone();
        s.process(&mut l, &mut r, false);
        assert_eq!(r, before);
        // An empty right slice is fine for a mono strip.
        s.process(&mut l, &mut [], false);
        assert!(l.iter().all(|x| x.is_finite()));
        frame += BLOCK;
    }
}

#[test]
fn off_and_default_pass_bit_exact() {
    for p in [all_off(), Processing::default()] {
        let mut s = settled(&p, SR);
        let mut fresh = StripProcessor::new(SR);
        for block in 0..50 {
            let input: Vec<f32> = (0..BLOCK)
                .map(|i| ((block * BLOCK + i) as f32 * 0.013).sin() * 0.7)
                .collect();
            let (mut l, mut r) = (input.clone(), input.clone());
            s.process(&mut l, &mut r, true);
            assert_eq!(l, input);
            assert_eq!(r, input);
            let mut m = input.clone();
            fresh.process(&mut m, &mut [], false);
            assert_eq!(m, input);
        }
        let m = s.meters();
        assert!(m.gate_open && m.gate_db == 0.0 && m.comp_db == 0.0);
    }
}

#[test]
fn off_sections_report_open_and_zero() {
    let play = |s: &mut StripProcessor| {
        let mut l = [0.0f32; BLOCK];
        let mut r = [0.0f32; BLOCK];
        for n in 0..200 * BLOCK {
            let (a, b) = tone(n);
            l[n % BLOCK] = a;
            r[n % BLOCK] = b;
            if n % BLOCK == BLOCK - 1 {
                s.process(&mut l, &mut r, true);
            }
        }
        s.meters()
    };
    // Gate shut (comp on, but the gate starves its detector).
    let mut p = busy_gate_shut();
    let mut s = settled(&p, SR);
    let m = play(&mut s);
    assert!(!m.gate_open && (m.gate_db - 40.0).abs() < 1.0e-3, "{m:?}");
    p.gate.on = false;
    s.set(&p);
    let m = s.meters();
    assert!(m.gate_open && m.gate_db == 0.0, "{m:?}");
    // Comp working.
    let mut p = busy();
    let mut s = settled(&p, SR);
    let m = play(&mut s);
    assert!(m.gate_open && m.comp_db > 1.0, "{m:?}");
    p.comp.on = false;
    s.set(&p);
    assert_eq!(s.meters().comp_db, 0.0);
}

#[test]
fn settle_lands_at_once() {
    let target = busy();
    // From another setting, settled onto `target`, matches a processor that
    // started at `target`, sample for sample.
    let mut a = settled(&target, SR);
    let mut other = busy();
    other.order = ProcessingOrder::CompThenEq;
    other.hpf.slope_db = 12;
    other.hpf.hz = 300.0;
    other.eq.bands[0].kind = EqKind::Bell;
    other.eq.bands[2].gain_db = -12.0;
    other.delay.ms = 300.0;
    let mut b = settled(&other, SR);
    b.set(&target);
    b.settle();
    // And without the settle, it glides: different.
    let mut c = settled(&other, SR);
    c.set(&target);
    let mut differs = false;
    for block in 0..100 {
        let input: Vec<f32> = (0..BLOCK)
            .map(|i| ((block * BLOCK + i) as f32 * 0.021).sin() * 0.4)
            .collect();
        let (mut al, mut ar) = (input.clone(), input.clone());
        let (mut bl, mut br) = (input.clone(), input.clone());
        let (mut cl, mut cr) = (input.clone(), input.clone());
        a.process(&mut al, &mut ar, true);
        b.process(&mut bl, &mut br, true);
        c.process(&mut cl, &mut cr, true);
        assert_eq!((&al, &ar), (&bl, &br), "block {block}");
        differs |= al != cl;
    }
    assert!(differs, "set without settle glides");
}

// --- Hostile values ------------------------------------------------------------

#[test]
fn hostile_settings_are_clamped_and_stay_finite() {
    let nan = f32::NAN;
    let inf = f32::INFINITY;
    let mut wild = Vec::new();
    for v in [nan, inf, -inf, 1.0e30, -1.0e30, 0.0] {
        let mut p = busy();
        p.hpf.hz = v;
        p.hpf.slope_db = 255;
        p.gate.threshold_db = v;
        p.gate.range_db = -v;
        p.gate.attack_ms = v;
        p.gate.hold_ms = v;
        p.gate.release_ms = -v;
        for band in &mut p.eq.bands {
            band.hz = v;
            band.gain_db = v;
            band.q = -v;
        }
        p.comp.threshold_db = v;
        p.comp.ratio = -v;
        p.comp.attack_ms = v;
        p.comp.release_ms = v;
        p.comp.knee_db = v;
        p.comp.makeup_db = v;
        p.delay.ms = v;
        wild.push(p);
    }
    // Corners of the ranges.
    let mut corner = busy();
    for band in &mut corner.eq.bands {
        *band = EqBand {
            kind: EqKind::Bell,
            hz: 20_000.0,
            gain_db: 18.0,
            q: 0.1,
        };
    }
    corner.comp.makeup_db = 24.0;
    corner.comp.ratio = 20.0;
    corner.hpf.hz = 600.0;
    wild.push(corner);

    for sr in [22_050u32, 44_100, 96_000, 192_000] {
        let mut s = StripProcessor::new(sr);
        for (k, p) in wild.iter().enumerate() {
            s.set(p);
            let t = s.target;
            let ranges = [
                (t.hpf.hz, 20.0, 600.0),
                (t.gate.threshold_db, -80.0, 0.0),
                (t.gate.range_db, -80.0, 0.0),
                (t.gate.attack_ms, 0.05, 100.0),
                (t.gate.hold_ms, 0.0, 2_000.0),
                (t.gate.release_ms, 5.0, 4_000.0),
                (t.comp.threshold_db, -60.0, 0.0),
                (t.comp.ratio, 1.0, 20.0),
                (t.comp.attack_ms, 0.1, 200.0),
                (t.comp.release_ms, 10.0, 2_000.0),
                (t.comp.knee_db, 0.0, 24.0),
                (t.comp.makeup_db, 0.0, 24.0),
                (t.delay.ms, 0.0, MAX_DELAY_MS),
            ];
            for (v, lo, hi) in ranges {
                assert!(v >= lo && v <= hi, "{v} outside {lo}..{hi}");
            }
            for b in t.eq.bands {
                assert!((20.0..=20_000.0).contains(&b.hz));
                assert!((-18.0..=18.0).contains(&b.gain_db));
                assert!((0.1..=10.0).contains(&b.q));
            }
            assert!([12, 18, 24].contains(&t.hpf.slope_db));
            if k % 2 == 0 {
                s.settle();
            }
            let mut seed = 0x9e37_79b9u32;
            for _ in 0..40 {
                let mut l = [0.0f32; BLOCK];
                let mut r = [0.0f32; BLOCK];
                for (a, b) in l.iter_mut().zip(&mut r) {
                    seed ^= seed << 13;
                    seed ^= seed >> 17;
                    seed ^= seed << 5;
                    *a = (seed as f32 / u32::MAX as f32) * 2.0 - 1.0;
                    *b = -*a * 0.5;
                }
                s.process(&mut l, &mut r, true);
                assert!(
                    l.iter().chain(&r).all(|x| x.is_finite()),
                    "{sr} Hz, setting {k}"
                );
                let m = s.meters();
                assert!(
                    m.gate_db.is_finite()
                        && m.comp_db.is_finite()
                        && m.gate_db >= 0.0
                        && m.comp_db >= 0.0
                );
            }
        }
    }
}

#[test]
fn a_nan_input_does_not_poison_the_filters() {
    let mut p = busy();
    p.delay.on = false;
    let mut s = settled(&p, SR);
    let mut l = [0.1f32; BLOCK];
    let mut r = [0.1f32; BLOCK];
    l[5] = f32::NAN;
    r[9] = f32::INFINITY;
    s.process(&mut l, &mut r, true);
    for _ in 0..20 {
        l.fill(0.1);
        r.fill(0.1);
        s.process(&mut l, &mut r, true);
    }
    assert!(l.iter().chain(&r).all(|x| x.is_finite()));
    let m = s.meters();
    assert!(m.gate_db.is_finite() && m.comp_db.is_finite());
}

// --- The sweep test -------------------------------------------------------------

/// One drag across the range and back, in blocks: about a second at 48 kHz.
const DRAG_BLOCKS: usize = 375;
/// An edge kink this many times the biggest in-block one is a step.
const MAX_JUMP: f32 = 4.0;

/// Two low tones, no noise: smooth, so a step shows as a kink.
fn tone(n: usize) -> (f32, f32) {
    let t = n as f64 / SR as f64;
    let x = (std::f64::consts::TAU * 110.0 * t).sin() * 0.3
        + (std::f64::consts::TAU * 330.0 * t).sin() * 0.1;
    (x as f32, (x * 0.8) as f32)
}

/// Plays `blocks` blocks from `base`; `change` may edit the settings before
/// each one. The worst ratio of the block-edge kink (second difference) to
/// the biggest in-block one, and whether everything stayed finite.
fn kink_ratio(
    base: Processing,
    blocks: usize,
    mut change: impl FnMut(usize, &mut Processing) -> bool,
) -> (f32, bool) {
    let mut s = settled(&base, SR);
    let mut p = base;
    let mut l = [0.0f32; BLOCK];
    let mut r = [0.0f32; BLOCK];
    let mut n = 0;
    for _ in 0..200 {
        for i in 0..BLOCK {
            (l[i], r[i]) = tone(n);
            n += 1;
        }
        s.process(&mut l, &mut r, true);
    }
    let mut last = [(l[BLOCK - 2], r[BLOCK - 2]), (l[BLOCK - 1], r[BLOCK - 1])];
    let mut worst = 0.0f32;
    let mut finite = true;
    for block in 0..blocks {
        for i in 0..BLOCK {
            (l[i], r[i]) = tone(n);
            n += 1;
        }
        if change(block, &mut p) {
            s.set(&p);
        }
        s.process(&mut l, &mut r, true);
        let mut edge = 0.0f32;
        let mut inside = 1.0e-7f32;
        for i in 0..BLOCK {
            finite &= l[i].is_finite() && r[i].is_finite();
            let d2 = |a: f32, b: f32, c: f32| (c - 2.0 * b + a).abs();
            let kink = d2(last[0].0, last[1].0, l[i]).max(d2(last[0].1, last[1].1, r[i]));
            if i < 2 {
                edge = edge.max(kink);
            } else {
                inside = inside.max(kink);
            }
            last = [last[1], (l[i], r[i])];
        }
        worst = worst.max(edge / inside);
    }
    (worst, finite)
}

type Setter = fn(&mut Processing, f32);

fn continuous_params() -> Vec<(String, fn() -> Processing, Setter)> {
    let mut list: Vec<(String, fn() -> Processing, Setter)> = vec![
        ("hpf.hz".into(), busy, |p, x| p.hpf.hz = 20.0 + 580.0 * x),
        ("gate.threshold_db".into(), busy, |p, x| {
            p.gate.threshold_db = -80.0 + 80.0 * x
        }),
        ("gate.range_db (shut)".into(), busy_gate_shut, |p, x| {
            p.gate.range_db = -80.0 + 80.0 * x
        }),
        ("gate.attack_ms".into(), busy, |p, x| {
            p.gate.attack_ms = 0.05 + 99.95 * x
        }),
        ("gate.hold_ms".into(), busy, |p, x| {
            p.gate.hold_ms = 2_000.0 * x
        }),
        ("gate.release_ms".into(), busy, |p, x| {
            p.gate.release_ms = 5.0 + 3_995.0 * x
        }),
        ("comp.threshold_db".into(), busy, |p, x| {
            p.comp.threshold_db = -60.0 + 60.0 * x
        }),
        ("comp.ratio".into(), busy, |p, x| {
            p.comp.ratio = 1.0 + 19.0 * x
        }),
        ("comp.attack_ms".into(), busy, |p, x| {
            p.comp.attack_ms = 0.1 + 199.9 * x
        }),
        ("comp.release_ms".into(), busy, |p, x| {
            p.comp.release_ms = 10.0 + 1_990.0 * x
        }),
        ("comp.knee_db".into(), busy, |p, x| {
            p.comp.knee_db = 24.0 * x
        }),
        ("comp.makeup_db".into(), busy, |p, x| {
            p.comp.makeup_db = 24.0 * x
        }),
        ("delay.ms".into(), busy, |p, x| {
            p.delay.ms = MAX_DELAY_MS * x
        }),
    ];
    let bands: [[Setter; 3]; EQ_BANDS] = [
        [
            |p, x| p.eq.bands[0].hz = 20.0 + 19_980.0 * x,
            |p, x| p.eq.bands[0].gain_db = -18.0 + 36.0 * x,
            |p, x| p.eq.bands[0].q = 0.1 + 9.9 * x,
        ],
        [
            |p, x| p.eq.bands[1].hz = 20.0 + 19_980.0 * x,
            |p, x| p.eq.bands[1].gain_db = -18.0 + 36.0 * x,
            |p, x| p.eq.bands[1].q = 0.1 + 9.9 * x,
        ],
        [
            |p, x| p.eq.bands[2].hz = 20.0 + 19_980.0 * x,
            |p, x| p.eq.bands[2].gain_db = -18.0 + 36.0 * x,
            |p, x| p.eq.bands[2].q = 0.1 + 9.9 * x,
        ],
        [
            |p, x| p.eq.bands[3].hz = 20.0 + 19_980.0 * x,
            |p, x| p.eq.bands[3].gain_db = -18.0 + 36.0 * x,
            |p, x| p.eq.bands[3].q = 0.1 + 9.9 * x,
        ],
    ];
    for (b, setters) in bands.into_iter().enumerate() {
        for (name, setter) in ["hz", "gain_db", "q"].into_iter().zip(setters) {
            list.push((format!("eq.bands[{b}].{name}"), busy, setter));
        }
    }
    list
}

#[test]
fn dragging_any_control_does_not_step_at_block_edges() {
    let mut failures = Vec::new();
    let (still, _) = kink_ratio(busy(), DRAG_BLOCKS, |_, _| false);
    println!("{:<26} {:>7}", "(untouched)", format!("{still:.2}"));
    for (name, base, setter) in continuous_params() {
        let (ratio, finite) = kink_ratio(base(), DRAG_BLOCKS, |block, p| {
            let phase = block as f32 / DRAG_BLOCKS as f32 * 2.0;
            setter(p, if phase < 1.0 { phase } else { 2.0 - phase });
            true
        });
        println!(
            "{name:<26} {ratio:>7.2}{}",
            if finite { "" } else { " NaN" }
        );
        if !finite || ratio > MAX_JUMP {
            failures.push(format!("{name}: edge kink x{ratio:.2}, finite {finite}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

type Switch = fn(&mut Processing, usize);

#[test]
fn switches_do_not_click() {
    let mut switches: Vec<(String, fn() -> Processing, Switch)> = vec![
        ("hpf.on".into(), busy, |p, i| p.hpf.on = i % 2 == 1),
        ("hpf.slope_db".into(), busy, |p, i| {
            p.hpf.slope_db = [18, 24, 12][i % 3]
        }),
        ("gate.on (shut)".into(), busy_gate_shut, |p, i| {
            p.gate.on = i % 2 == 1
        }),
        ("eq.on".into(), busy, |p, i| p.eq.on = i % 2 == 1),
        ("comp.on".into(), busy, |p, i| p.comp.on = i % 2 == 1),
        ("delay.on".into(), busy, |p, i| p.delay.on = i % 2 == 1),
        ("order".into(), busy, |p, i| {
            p.order = if i % 2 == 0 {
                ProcessingOrder::CompThenEq
            } else {
                ProcessingOrder::EqThenComp
            }
        }),
    ];
    let band_switches: [Switch; EQ_BANDS] = [
        |p, i| p.eq.bands[0].kind = [EqKind::Bell, EqKind::HighShelf, EqKind::LowShelf][i % 3],
        |p, i| p.eq.bands[1].kind = [EqKind::HighShelf, EqKind::LowShelf, EqKind::Bell][i % 3],
        |p, i| p.eq.bands[2].kind = [EqKind::HighShelf, EqKind::LowShelf, EqKind::Bell][i % 3],
        |p, i| p.eq.bands[3].kind = [EqKind::LowShelf, EqKind::Bell, EqKind::HighShelf][i % 3],
    ];
    for (b, s) in band_switches.into_iter().enumerate() {
        switches.push((format!("eq.bands[{b}].kind"), busy, s));
    }
    let mut failures = Vec::new();
    for (name, base, switch) in switches {
        // A switch every quarter second (a few blocks apart, so fades overlap
        // nothing), through every value and back.
        let (ratio, finite) = kink_ratio(base(), 94 * 7, |block, p| {
            if block % 94 == 0 {
                switch(p, block / 94);
                true
            } else {
                false
            }
        });
        println!(
            "{name:<26} {ratio:>7.2}{}",
            if finite { "" } else { " NaN" }
        );
        if !finite || ratio > MAX_JUMP {
            failures.push(format!("{name}: edge kink x{ratio:.2}, finite {finite}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The output does not jump when a section switches: the largest
/// sample-to-sample step while switching stays within the larger of the two
/// settled states' own (the tones are smooth, a click is a jump far above
/// them).
#[test]
fn switches_have_no_step() {
    let cases: [(&str, fn() -> Processing, Switch); 6] = [
        ("hpf.on", busy, |p, i| p.hpf.on = i % 2 == 1),
        ("gate.on (shut)", busy_gate_shut, |p, i| {
            p.gate.on = i % 2 == 1
        }),
        ("eq.on", busy, |p, i| p.eq.on = i % 2 == 1),
        ("comp.on", busy, |p, i| p.comp.on = i % 2 == 1),
        ("delay.on", busy, |p, i| p.delay.on = i % 2 == 1),
        ("order", busy, |p, i| {
            p.order = if i % 2 == 1 {
                ProcessingOrder::CompThenEq
            } else {
                ProcessingOrder::EqThenComp
            }
        }),
    ];
    for (name, base, switch) in cases {
        // The compressor's own attack after a level change is an envelope,
        // not a click: keep it out except where it is the subject.
        let base = || {
            let mut p = base();
            p.comp.on = name == "comp.on" || name == "order";
            p
        };
        let still = |i: usize| {
            let mut p = base();
            switch(&mut p, i);
            steps(p, |_, _| false)
        };
        let limit = still(0).max(still(1)) * 1.25;
        let max_step = steps(base(), |block, p| {
            if block % 94 == 0 {
                switch(p, block / 94);
                true
            } else {
                false
            }
        });
        println!(
            "{name:<26} step {max_step:.5} (settled up to {:.5})",
            limit / 1.25
        );
        assert!(
            max_step <= limit,
            "{name}: step {max_step:.5} over {limit:.5}"
        );
    }
}

/// The largest first difference over a run.
fn steps(base: Processing, mut change: impl FnMut(usize, &mut Processing) -> bool) -> f32 {
    let mut s = settled(&base, SR);
    let mut p = base;
    let mut l = [0.0f32; BLOCK];
    let mut r = [0.0f32; BLOCK];
    let mut n = 0;
    let mut prev = (0.0f32, 0.0f32);
    let mut worst = 0.0f32;
    for block in 0..(200 + 94 * 7) {
        for i in 0..BLOCK {
            (l[i], r[i]) = tone(n);
            n += 1;
        }
        if block >= 200 && change(block - 200, &mut p) {
            s.set(&p);
        }
        s.process(&mut l, &mut r, true);
        for i in 0..BLOCK {
            if block >= 200 {
                worst = worst.max((l[i] - prev.0).abs()).max((r[i] - prev.1).abs());
            }
            prev = (l[i], r[i]);
        }
    }
    worst
}

// --- Cost ------------------------------------------------------------------------

/// Prints the per-block cost (48 kHz, 128 frames) for the cases that matter.
/// Timings depend on the machine and its load, so nothing is asserted; run
/// in release to read them.
#[test]
fn cost_per_block() {
    let cases: [(&str, Processing, bool, bool); 6] = [
        ("all on, stereo", busy(), true, false),
        ("all on, stereo, dragging", busy(), true, true),
        ("all on, mono", busy(), false, false),
        ("default (EQ on, flat)", Processing::default(), true, false),
        ("all off, stereo", all_off(), true, false),
        ("all off, mono", all_off(), false, false),
    ];
    let budget_us = BLOCK as f64 / SR as f64 * 1e6;
    println!("block budget {budget_us:.0} us (48 kHz, {BLOCK} frames)");
    for (name, p, stereo, drag) in cases {
        let mut s = settled(&p, SR);
        let mut l = [0.0f32; BLOCK];
        let mut r = [0.0f32; BLOCK];
        let mut n = 0;
        let blocks = 20_000;
        let mut total = 0.0f64;
        let mut worst = 0.0f64;
        let mut q = p;
        for block in 0..(blocks + 200) {
            for i in 0..BLOCK {
                (l[i], r[i]) = tone(n);
                n += 1;
            }
            let started = Instant::now();
            if drag {
                let x = (block % 375) as f32 / 375.0;
                q.eq.bands[1].gain_db = -12.0 + 24.0 * x;
                q.eq.bands[2].hz = 500.0 + 4_000.0 * x;
                q.hpf.hz = 20.0 + 200.0 * x;
                q.comp.threshold_db = -40.0 + 20.0 * x;
                s.set(&q);
            }
            s.process(&mut l, &mut r, stereo);
            let us = started.elapsed().as_secs_f64() * 1e6;
            if block >= 200 {
                total += us;
                worst = worst.max(us);
            }
        }
        std::hint::black_box((&l, &r));
        println!(
            "{name:<26} mean {:>6.2} us   max {:>6.1} us",
            total / blocks as f64,
            worst
        );
    }
}

use super::*;
use crate::measure::{
    self, Metrics, dsp_for, frames_for, impulse_response, measure, modulation_spread_hz,
};

const SR: f32 = measure::SR;

fn wet_only(mut params: Params) -> Params {
    params.mix = 100.0;
    params
}

fn dsp_with(params: Params) -> Dsp {
    dsp_for(&params, &[])
}

/// The wet impulse response's metrics for `params`.
fn metrics(params: &Params) -> Metrics {
    let rt = params.decay_sec * params.bass_mult.max(1.0);
    let mut dsp = dsp_with(wet_only(params.clone()));
    let (l, r) = impulse_response(&mut dsp, frames_for(rt));
    measure(&l, &r)
}

/// A quick room for the sweeps that are not about decay: short enough to
/// render fast, long enough for every metric.
fn quick() -> Params {
    Params {
        decay_sec: 1.2,
        mod_depth: 0.0,
        ..wet_only(default_params())
    }
}

/// Sweeps `id` over `values` on `base`, reads `metric`, and checks that it
/// moves monotonically (`rising` or falling) by at least `by` end to end.
fn assert_sweep(
    base: &Params,
    id: &str,
    values: &[f32],
    metric: impl Fn(&Params) -> f32,
    rising: bool,
    by: f32,
) -> Vec<f32> {
    let readings: Vec<f32> = values
        .iter()
        .map(|value| {
            let mut p = base.clone();
            assert!(ipc::apply_ui_param(&mut p, id, *value));
            metric(&p)
        })
        .collect();
    for pair in readings.windows(2) {
        let step = pair[1] - pair[0];
        assert!(
            if rising { step > 0.0 } else { step < 0.0 },
            "{id}: not monotonic over {values:?}: {readings:?}"
        );
    }
    let travel = (readings[readings.len() - 1] - readings[0]).abs();
    assert!(
        travel >= by,
        "{id}: moved only {travel} over {values:?} (want {by}): {readings:?}"
    );
    readings
}

// ── Contract ─────────────────────────────────────────────────────────────

#[test]
fn descriptor_ids_are_unique_and_match_defaults() {
    let d = descriptor();
    assert_eq!(d.id, PLUGIN_ID);
    assert_eq!(d.category, PluginCategory::Effect);
    assert_eq!(d.params.len(), ipc::PARAM_COUNT);

    let mut ids: Vec<_> = d.params.iter().map(|p| p.id).collect();
    let count = ids.len();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(count, ids.len(), "duplicate parameter id in descriptor");

    let defaults = ipc::ui_values(&default_params());
    for param in d.params {
        let (_, actual) = defaults
            .iter()
            .find(|(id, _)| *id == param.id)
            .copied()
            .unwrap_or_else(|| panic!("`{}` is missing from ui_values", param.id));
        assert!(
            (param.default_value - actual).abs() < 1.0e-6,
            "`{}`: descriptor says {}, default_params() says {actual}",
            param.id,
            param.default_value,
        );
        assert!(
            param.default_value >= param.min && param.default_value <= param.max,
            "`{}`: default {} is outside {}..{}",
            param.id,
            param.default_value,
            param.min,
            param.max,
        );
    }
}

#[test]
fn bypass_when_power_off() {
    let mut params = default_params();
    params.power = false;
    let mut dsp = dsp_with(params);
    assert_eq!(dsp.process_stereo(0.25, -0.25), (0.25, -0.25));
}

#[test]
fn mix_at_zero_is_the_dry_signal() {
    let mut params = default_params();
    params.mix = 0.0;
    let mut dsp = dsp_with(params);
    for n in 0..4_800 {
        let x = (n as f32 * 0.05).sin() * 0.5;
        let (l, r) = dsp.process_stereo(x, -x);
        assert!((l - x).abs() < 1.0e-6 && (r + x).abs() < 1.0e-6);
    }
}

/// Mix is equal power: the dry falls as the cosine, the wet rises as the
/// sine; Wet Only drops the dry and plays the wet at full whatever Mix says.
#[test]
fn mix_is_equal_power_and_wet_only_drops_the_dry() {
    let energy = |params: Params| {
        let mut dsp = dsp_with(params);
        let (mut dry, mut total) = (0.0f64, 0.0f64);
        // An impulse: sample 0 is the dry hit alone (the pre-delay holds
        // the wet back), the rest is the wet.
        for n in 0..48_000 {
            let x = if n == 0 { 1.0 } else { 0.0 };
            let (l, r) = dsp.process_stereo(x, x);
            let e = f64::from(l * l + r * r) * 0.5;
            if n == 0 {
                dry += e;
            } else {
                total += e;
            }
        }
        (dry, total)
    };
    let full = energy(wet_only(default_params())).1;
    let half = energy(Params {
        mix: 50.0,
        ..default_params()
    });
    assert!((half.0 - 0.5).abs() < 1.0e-3, "dry at 50 %: {}", half.0);
    assert!(
        (half.1 / full - 0.5).abs() < 0.02,
        "wet at 50 %: {}",
        half.1 / full
    );
    let send = energy(Params {
        mix: 10.0,
        wet_only: true,
        ..default_params()
    });
    assert_eq!(send.0, 0.0, "wet only still passes the dry");
    assert!(
        (send.1 / full - 1.0).abs() < 0.02,
        "wet only is not full wet"
    );
}

#[test]
fn the_feedback_matrix_preserves_energy() {
    let mut v: [f32; LINE_COUNT] = std::array::from_fn(|i| (i as f32 * 0.37).sin());
    let before: f32 = v.iter().map(|x| x * x).sum();
    hadamard(&mut v);
    let after: f32 = v.iter().map(|x| x * x).sum();
    assert!((before - after).abs() < 1.0e-5);
}

/// Two orthogonal output vectors are what decorrelate left and right;
/// injections that are not rows of the matrix are what spread at once.
#[test]
fn the_tank_vectors_are_orthogonal_and_spread() {
    let dot = |a: &[f32; LINE_COUNT], b: &[f32; LINE_COUNT]| -> f32 {
        a.iter().zip(b).map(|(x, y)| x * y).sum()
    };
    assert_eq!(dot(&INPUT_L, &INPUT_R), 0.0);
    assert_eq!(dot(&OUTPUT_L, &OUTPUT_R), 0.0);
    for input in [INPUT_L, INPUT_R] {
        let mut mixed = input;
        hadamard(&mut mixed);
        let loudest = mixed.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        assert!(loudest < 3.0, "an injection collapsed onto one line");
    }
}

/// The mode is a label: two DSPs that differ only in it play the same
/// samples.
#[test]
fn the_mode_never_changes_the_sound() {
    let mut a = dsp_with(wet_only(default_params()));
    let mut b = dsp_with(Params {
        mode: ReverbMode::Plate,
        ..wet_only(default_params())
    });
    assert!(a.apply_ui_param("mode", ReverbMode::Room.to_wire()));
    for n in 0..9_600 {
        let x = if n % 977 == 0 { 0.8 } else { 0.0 };
        assert_eq!(a.process_stereo(x, -x), b.process_stereo(x, -x));
    }
}

/// A space type is a starting point: it moves the space knobs to its values
/// and leaves the cuts, width, mix and output alone.
#[test]
fn a_starting_point_loads_the_space_and_keeps_the_rest() {
    let current = Params {
        low_cut_hz: 333.0,
        high_cut_hz: 5_555.0,
        width: 150.0,
        mix: 77.0,
        output_db: -4.0,
        wet_only: true,
        freeze: true,
        ..default_params()
    };
    for mode in ReverbMode::ALL {
        let next = mode.starting_point(&current);
        assert_eq!(next.mode, mode);
        assert_eq!(next.low_cut_hz, 333.0);
        assert_eq!(next.high_cut_hz, 5_555.0);
        assert_eq!(next.width, 150.0);
        assert_eq!(next.mix, 77.0);
        assert_eq!(next.output_db, -4.0);
        assert!(next.wet_only && next.freeze);
        let mut clamped = next.clone();
        ipc::sanitize_params(&mut clamped);
        assert_eq!(clamped, next, "{mode:?} starts out of range");
    }
    assert_eq!(
        ReverbMode::Hall.starting_point(&default_params()),
        default_params(),
        "the defaults are the Hall starting point"
    );
}

// ── Realtime safety ──────────────────────────────────────────────────────

#[test]
fn impulse_builds_a_tail_at_every_rate_and_type() {
    for &sr in &[44_100.0f32, 48_000.0, 96_000.0] {
        for mode in ReverbMode::ALL {
            let mut dsp = Dsp::new(sr);
            dsp.set_params(wet_only(mode.starting_point(&default_params())));
            dsp.reset();
            let _ = dsp.process_stereo(1.0, 1.0);
            let mut late = 0.0f32;
            for n in 0..(sr as usize / 2) {
                let (l, r) = dsp.process_stereo(0.0, 0.0);
                assert!(l.is_finite() && r.is_finite());
                if n > sr as usize / 5 {
                    late = late.max(l.abs()).max(r.abs());
                }
            }
            assert!(late > 1.0e-4, "{mode:?} @ {sr} produced no tail");
        }
    }
}

/// The tank is a feedback loop around a unit-gain mixing matrix; the
/// longest decay at either end of Size is where a coefficient slip shows up
/// as a slow build to infinity rather than as a tail.
#[test]
fn longest_decay_stays_bounded() {
    for size in [0.0, 100.0] {
        let params = Params {
            decay_sec: 20.0,
            size,
            damping: 0.0,
            damp_freq_hz: 16_000.0,
            bass_mult: 3.0,
            bass_freq_hz: 1_000.0,
            mod_depth: 100.0,
            mod_rate_hz: 5.0,
            diffusion: 100.0,
            high_cut_hz: 20_000.0,
            low_cut_hz: 20.0,
            ..wet_only(default_params())
        };
        let mut dsp = dsp_with(params);
        let mut peak = 0.0f32;
        for n in 0..(48_000 * 6) {
            let x = if n < 4_800 {
                (n as f32 * 0.01).sin() * 0.7
            } else {
                0.0
            };
            let (l, r) = dsp.process_stereo(x, x);
            assert!(l.is_finite() && r.is_finite(), "non-finite at sample {n}");
            peak = peak.max(l.abs()).max(r.abs());
        }
        assert!(peak < 4.0, "tank ran away at size {size}: peak {peak}");
    }
}

/// A short room left to ring out goes to exact silence: no subnormal ever
/// leaves the plug-in, and the tank holds none.
#[test]
fn a_finished_tail_flushes_to_silence_without_denormals() {
    let params = Params {
        decay_sec: 0.2,
        size: 0.0,
        ..wet_only(default_params())
    };
    let mut dsp = dsp_with(params);
    for n in 0..(48_000 * 6) {
        let x = if n < 480 { 0.5 } else { 0.0 };
        let (l, r) = dsp.process_stereo(x, x);
        for v in [l, r] {
            assert!(
                v == 0.0 || v.abs() >= f32::MIN_POSITIVE,
                "subnormal output {v:e} at {n}"
            );
        }
        if n == 48_000 * 6 - 1 {
            assert_eq!((l, r), (0.0, 0.0), "the tail never reached silence");
        }
    }
}

#[test]
fn freeze_holds_the_tail_after_the_input_stops() {
    let params = Params {
        decay_sec: 2.0,
        ..wet_only(default_params())
    };
    let mut dsp = dsp_with(params);
    for n in 0..24_000 {
        let x = (n as f32 * 0.03).sin() * 0.5;
        let _ = dsp.process_stereo(x, x);
    }
    assert!(dsp.apply_ui_param("freeze", 1.0));

    // Let the early pattern empty, then compare the held tail over time.
    let rms = |dsp: &mut Dsp, frames: usize| {
        let mut e = 0.0f64;
        for _ in 0..frames {
            let (l, r) = dsp.process_stereo(0.3, -0.3);
            e += f64::from(l * l + r * r);
        }
        (e / frames as f64).sqrt() as f32
    };
    let _ = rms(&mut dsp, 9_600);
    let early = rms(&mut dsp, 9_600);
    for _ in 0..(48_000 * 4) {
        let _ = dsp.process_stereo(0.3, -0.3);
    }
    let late = rms(&mut dsp, 9_600);
    assert!(early > 1.0e-3, "nothing in the tank to freeze");
    let drift_db = 20.0 * (late / early).log10();
    assert!(
        drift_db.abs() < 1.5,
        "freeze drifted {drift_db:.2} dB over 4 s (input kept out?)"
    );

    // And unfreezing lets it go.
    assert!(dsp.apply_ui_param("freeze", 0.0));
    for _ in 0..(48_000 * 6) {
        let _ = dsp.process_stereo(0.0, 0.0);
    }
    let (l, r) = dsp.process_stereo(0.0, 0.0);
    assert!(l.abs().max(r.abs()) < early * 0.01);
}

/// Dragging any space control on a running tail bends it; it never steps. A
/// step shows as a sample-to-sample jump out of proportion to the level
/// around it; a glide only bends the pitch for the moment it lasts.
#[test]
fn a_running_change_does_not_click() {
    for (id, value) in [
        ("size", 0.0f32),
        ("size", 100.0),
        ("decaySec", 0.3),
        ("freeze", 1.0),
        ("diffusion", 0.0),
        ("predelayMs", 300.0),
        ("bassFreqHz", 1_000.0),
        ("dampFreqHz", 1_000.0),
        ("damping", 100.0),
        ("earlyLate", 0.0),
        ("wetOnly", 1.0),
        ("width", 200.0),
    ] {
        let params = Params {
            low_cut_hz: 20.0,
            high_cut_hz: 20_000.0,
            ..wet_only(default_params())
        };
        let mut dsp = dsp_with(params);
        let mut previous = (0.0f32, 0.0f32);
        let mut window_step = 0.0f32;
        let mut window_peak = 0.0f32;
        let mut steady = 0.0f32;
        let mut worst = 0.0f32;
        for n in 0..(48_000 * 2) {
            let x = (n as f32 * TAU * 220.0 / SR).sin() * 0.3;
            if n == 48_000 {
                assert!(dsp.apply_ui_param(id, value));
            }
            let (l, r) = dsp.process_stereo(x, x);
            window_step = window_step.max((l - previous.0).abs().max((r - previous.1).abs()));
            window_peak = window_peak.max(l.abs()).max(r.abs());
            previous = (l, r);
            if n % 1_000 == 999 && n > 24_000 {
                let ratio = window_step / window_peak.max(1.0e-6);
                if n < 48_000 {
                    steady = steady.max(ratio);
                } else {
                    worst = worst.max(ratio);
                }
                window_step = 0.0;
                window_peak = 0.0;
            }
        }
        assert!(
            worst < steady * 2.5,
            "{id} → {value}: the change stepped ({worst} against {steady})"
        );
    }
}

#[test]
fn reset_clears_the_tail() {
    let mut dsp = dsp_with(wet_only(default_params()));
    for _ in 0..4_800 {
        let _ = dsp.process_stereo(0.5, -0.5);
    }
    dsp.reset();
    let (l, r) = dsp.process_stereo(0.0, 0.0);
    assert!(l.abs() < 1.0e-6 && r.abs() < 1.0e-6);
}

#[test]
fn sample_rate_change_keeps_delays_inside_the_new_rings() {
    let params = Params {
        size: 100.0,
        predelay_ms: MAX_PREDELAY_MS,
        mod_depth: 100.0,
        ..wet_only(default_params())
    };
    let mut dsp = dsp_with(params);
    dsp.set_sample_rate(44_100.0);
    assert_eq!(dsp.params().size, 100.0, "the params survive a rate change");
    for line in &dsp.lines {
        assert!(line.delay + MAX_MOD_MS * 0.001 * 44_100.0 * 2.5 < line.ring.max_cubic_delay());
    }
    assert!(dsp.tuning.predelay + 2.0 < dsp.predelay_l.len() as f32);
    assert!(dsp.tuning.early_span + 2.0 < dsp.early_l.len() as f32);
    for ap in dsp.diffusers_l.iter().chain(&dsp.diffusers_r) {
        assert!(ap.base * MAX_DIFFUSER_SCALE + 2.0 < ap.ring.len() as f32);
    }
    for _ in 0..44_100 {
        let (l, r) = dsp.process_stereo(0.3, -0.3);
        assert!(l.is_finite() && r.is_finite());
    }
}

#[test]
fn wire_update_changes_only_authoritative_params() {
    let mut dsp = Dsp::new(48_000.0);
    assert!(dsp.apply_wire_param(ipc::DECAY_INDEX, 6.0));
    assert_eq!(dsp.params().decay_sec, 6.0);
    assert!(dsp.apply_wire_param(ipc::EARLY_LATE_INDEX, 10.0));
    assert_eq!(dsp.params().early_late, 10.0);
    assert!(!dsp.apply_wire_param(u32::MAX, 0.0));
    assert!(!dsp.apply_wire_param(ipc::DECAY_INDEX, f32::NAN));
}

// ── The displays are true ────────────────────────────────────────────────

#[test]
fn the_wet_cuts_are_what_the_editor_draws() {
    let params = default_params();
    assert!(wet_filter_response_db(&params, 1_000.0, SR).abs() < 0.5);
    assert!(wet_filter_response_db(&params, 25.0, SR) < -12.0);
    assert!(wet_filter_response_db(&params, 19_000.0, SR) < -6.0);
}

/// The decay the editor draws is the decay that plays, band by band: the
/// Schroeder RT60 of each octave of the rendered impulse response against
/// `decay_profile` at the octave's centre.
#[test]
fn the_measured_decay_is_the_drawn_decay_in_every_band() {
    for (decay, damping, damp_freq, bass, bass_freq, size) in [
        (1.5f32, 0.0f32, 4_000.0f32, 1.0f32, 250.0f32, 60.0f32),
        (2.0, 60.0, 3_000.0, 2.0, 300.0, 40.0),
        (0.8, 85.0, 2_000.0, 0.5, 400.0, 20.0),
        (4.0, 40.0, 6_000.0, 1.5, 150.0, 100.0),
    ] {
        let params = Params {
            decay_sec: decay,
            damping,
            damp_freq_hz: damp_freq,
            bass_mult: bass,
            bass_freq_hz: bass_freq,
            size,
            low_cut_hz: 20.0,
            high_cut_hz: 20_000.0,
            mod_depth: 0.0,
            predelay_ms: 0.0,
            early_late: 100.0,
            ..default_params()
        };
        let profile = decay_profile(&params, SR);
        let m = metrics(&params);
        for (band, hz) in measure::BAND_HZ.iter().enumerate() {
            let drawn = profile.rt_at(*hz);
            let measured = m.rt[band];
            assert!(
                (measured / drawn - 1.0).abs() < 0.2,
                "decay {decay}, damping {damping}, bass {bass} @ {hz} Hz: drew {drawn:.2} s, \
                 measured {measured:.2} s"
            );
        }
        assert!((profile.rt_mid_sec - decay).abs() < 1.0e-6);
    }
}

#[test]
fn the_profile_draws_the_reflections_the_balance_lets_through() {
    let plate = decay_profile(&ReverbMode::Plate.starting_point(&default_params()), SR);
    assert!(plate.early.iter().all(|e| e.gain == 0.0));
    assert!((plate.late_db).abs() < 1.0e-3);
    let room = decay_profile(&ReverbMode::Room.starting_point(&default_params()), SR);
    assert!(room.early.iter().any(|e| e.gain.abs() > 0.05));
    assert!(room.early.iter().all(|e| e.at_ms >= room.predelay_ms));
    let small = decay_profile(
        &Params {
            size: 0.0,
            ..default_params()
        },
        SR,
    );
    let large = decay_profile(
        &Params {
            size: 100.0,
            ..default_params()
        },
        SR,
    );
    let last = |p: &DecayProfile| p.early.iter().map(|e| e.at_ms).fold(0.0, f32::max);
    assert!(last(&large) - large.predelay_ms > 10.0 * (last(&small) - small.predelay_ms));
    assert!(large.first_late_ms > 5.0 * small.first_late_ms);
}

// ── Every control is clearly audible, and moves one way ──────────────────

#[test]
fn predelay_moves_the_first_arrival() {
    let readings = assert_sweep(
        &quick(),
        "predelayMs",
        &[0.0, 100.0, 500.0],
        |p| metrics(p).first_ms,
        true,
        480.0,
    );
    assert!((readings[2] - readings[0] - 500.0).abs() < 2.0);
}

#[test]
fn size_moves_the_room_not_the_decay() {
    // The whole response gets later and slower to build...
    let rows: Vec<Metrics> = [0.0f32, 50.0, 100.0]
        .iter()
        .map(|size| {
            metrics(&Params {
                size: *size,
                ..quick()
            })
        })
        .collect();
    assert_sweep(
        &quick(),
        "size",
        &[0.0, 50.0, 100.0],
        |p| metrics(p).centre_ms,
        true,
        60.0,
    );
    // ...the late tail arrives later on its own...
    let late_only = Params {
        early_late: 100.0,
        ..quick()
    };
    assert_sweep(
        &late_only,
        "size",
        &[0.0, 50.0, 100.0],
        |p| metrics(p).first_ms,
        true,
        25.0,
    );
    // ...the echoes take longer to fuse...
    assert!(rows[2].mixing_ms > rows[0].mixing_ms + 30.0, "{rows:?}");
    // ...and none of it changes how long the room rings or how loud it is.
    for m in &rows {
        assert!((m.rt[1] / rows[1].rt[1] - 1.0).abs() < 0.12, "{rows:?}");
        assert!((m.level_db - rows[1].level_db).abs() < 1.5, "{rows:?}");
    }
}

#[test]
fn decay_sets_the_rt60() {
    let readings = assert_sweep(
        &Params {
            damping: 0.0,
            bass_mult: 1.0,
            ..quick()
        },
        "decaySec",
        &[0.3, 1.5, 6.0],
        |p| metrics(p).rt[1],
        true,
        5.0,
    );
    assert!(readings[2] / readings[0] > 12.0, "{readings:?}");
}

/// Stretching the decay changes how long the room rings, not how loud it is:
/// the wet level for a steady input stays within a few dB from a 0.2 s booth
/// to a 20 s cathedral, at any size.
#[test]
fn the_wet_level_holds_across_decay_and_size() {
    let mut levels = Vec::new();
    for size in [0.0f32, 50.0, 100.0] {
        for decay in [0.2f32, 1.0, 4.0, 20.0] {
            let p = Params {
                size,
                decay_sec: decay,
                ..wet_only(default_params())
            };
            levels.push((size, decay, metrics(&p).level_db));
        }
    }
    let loudest = levels.iter().map(|l| l.2).fold(f32::MIN, f32::max);
    let quietest = levels.iter().map(|l| l.2).fold(f32::MAX, f32::min);
    assert!(loudest - quietest < 3.0, "{levels:?}");
    // About 3–4 dB under the input at the default cuts.
    assert!((-6.0..=-1.0).contains(&loudest), "{levels:?}");
}

#[test]
fn diffusion_thickens_the_onset() {
    assert_sweep(
        &quick(),
        "diffusion",
        &[0.0, 50.0, 100.0],
        |p| metrics(p).early_density,
        true,
        0.35,
    );
}

#[test]
fn damping_shortens_the_top() {
    let base = Params {
        bass_mult: 1.0,
        ..quick()
    };
    let readings = assert_sweep(
        &base,
        "damping",
        &[0.0, 50.0, 100.0],
        |p| {
            let m = metrics(p);
            m.rt[3] / m.rt[1]
        },
        false,
        0.5,
    );
    assert!(readings[0] > 0.8 && readings[2] < 0.35, "{readings:?}");
}

#[test]
fn damp_freq_moves_where_the_top_starts_to_fall() {
    let base = Params {
        damping: 80.0,
        bass_mult: 1.0,
        ..quick()
    };
    let readings = assert_sweep(
        &base,
        "dampFreqHz",
        &[1_000.0, 4_000.0, 16_000.0],
        |p| metrics(p).rt[2],
        true,
        0.4,
    );
    assert!(readings[2] / readings[0] > 2.0, "{readings:?}");
}

#[test]
fn bass_scales_the_low_decay() {
    let readings = assert_sweep(
        &quick(),
        "bassMult",
        &[0.2, 1.0, 3.0],
        |p| metrics(p).rt[0],
        true,
        1.0,
    );
    assert!(readings[2] / readings[0] > 3.0, "{readings:?}");
}

#[test]
fn bass_freq_moves_where_the_bass_multiplier_reaches() {
    let base = Params {
        bass_mult: 2.5,
        ..quick()
    };
    let readings = assert_sweep(
        &base,
        "bassFreqHz",
        &[50.0, 250.0, 1_000.0],
        |p| metrics(p).rt[0],
        true,
        0.6,
    );
    assert!(readings[2] / readings[0] > 1.5, "{readings:?}");
}

#[test]
fn early_late_trades_reflections_for_tail() {
    let base = quick();
    // The share of the energy in the first 30 ms after the pre-delay falls...
    assert_sweep(
        &base,
        "earlyLate",
        &[0.0, 50.0, 100.0],
        |p| {
            let mut dsp = dsp_with(p.clone());
            let (l, r) = impulse_response(&mut dsp, frames_for(p.decay_sec));
            let edge = ((p.predelay_ms + 30.0) * 0.001 * SR) as usize;
            let (mut early, mut total) = (0.0f64, 0.0f64);
            for (n, (l, r)) in l.iter().zip(&r).enumerate() {
                let e = f64::from(l * l + r * r);
                total += e;
                if n < edge {
                    early += e;
                }
            }
            (10.0 * (early / total).log10()) as f32
        },
        false,
        10.0,
    );
    // ...the response's centre moves later...
    assert_sweep(
        &base,
        "earlyLate",
        &[0.0, 50.0, 100.0],
        |p| metrics(p).centre_ms,
        true,
        60.0,
    );
    // ...and the level stays.
    let level = |early_late: f32| {
        metrics(&Params {
            early_late,
            ..base.clone()
        })
        .level_db
    };
    assert!((level(0.0) - level(100.0)).abs() < 2.0);
}

#[test]
fn mod_depth_and_rate_chorus_the_tail() {
    let spread = |p: &Params| modulation_spread_hz(&mut dsp_with(p.clone()));
    let readings = assert_sweep(&quick(), "modDepth", &[0.0, 50.0, 100.0], spread, true, 3.0);
    assert!(
        readings[0] < 0.5,
        "a static tank smeared a tone: {readings:?}"
    );
    let base = Params {
        mod_depth: 50.0,
        ..quick()
    };
    assert_sweep(&base, "modRateHz", &[0.1, 1.0, 5.0], spread, true, 3.0);
}

#[test]
fn the_cuts_tilt_the_wet() {
    assert_sweep(
        &quick(),
        "lowCutHz",
        &[20.0, 200.0, 1_000.0],
        |p| metrics(p).tilt_db,
        true,
        6.0,
    );
    assert_sweep(
        &quick(),
        "highCutHz",
        &[1_000.0, 4_000.0, 20_000.0],
        |p| metrics(p).tilt_db,
        true,
        15.0,
    );
}

/// A mono source comes back as two different tails that still sum; Width
/// narrows them to mono or pushes them wider, and even at 200 % the
/// fold-down keeps most of the reverb.
#[test]
fn width_moves_the_correlation_and_folds_down() {
    let readings = assert_sweep(
        &quick(),
        "width",
        &[0.0, 100.0, 200.0],
        |p| metrics(p).correlation,
        false,
        1.4,
    );
    assert!(readings[0] > 0.99, "{readings:?}");
    assert!(readings[1].abs() < 0.2, "{readings:?}");
    assert!(readings[2] > -0.75, "200 % collapses in mono: {readings:?}");
}

#[test]
fn output_trims_the_wet_exactly() {
    let readings = assert_sweep(
        &quick(),
        "outputDb",
        &[-12.0, 0.0, 12.0],
        |p| metrics(p).level_db,
        true,
        23.5,
    );
    assert!(
        (readings[2] - readings[0] - 24.0).abs() < 0.3,
        "{readings:?}"
    );
}

// ── The factory bank sounds like its names ───────────────────────────────

#[test]
fn every_preset_sounds_like_its_name() {
    let bank = factory_presets();
    let find = |name: &str| {
        let preset = bank
            .iter()
            .find(|p| p.name == name)
            .unwrap_or_else(|| panic!("no preset {name}"));
        let m = metrics(&Params {
            // Measure the rooms themselves, not the cuts.
            low_cut_hz: 20.0,
            ..preset.params.clone()
        });
        (preset.params.clone(), m)
    };
    let (_, tight) = find("Tight Room");
    let (_, drum) = find("Drum Room");
    let (_, chamber) = find("Live Chamber");
    let (_, hall) = find("Concert Hall");
    let (_, cathedral) = find("Cathedral");
    let (_, pad) = find("Infinite Pad");
    let (_, dark) = find("Dark Hall");
    let (plate_params, plate) = find("Bright Plate");
    let (_, ambience) = find("Vocal Ambience");
    // Rooms ring shortest, then the chamber, the hall, the cathedral, the
    // pad.
    let rt = |m: &Metrics| m.rt[1];
    assert!(rt(&tight) < 0.6 && rt(&ambience) < 0.8 && rt(&drum) < 1.0);
    assert!(rt(&drum) < rt(&chamber) && rt(&chamber) < rt(&hall));
    assert!(rt(&hall) < rt(&cathedral) && rt(&cathedral) < rt(&pad));
    // Small rooms are dense at once; big ones take time to fuse.
    assert!(tight.mixing_ms < hall.mixing_ms && hall.mixing_ms <= cathedral.mixing_ms + 10.0);
    // The dark hall is darker than the concert hall; the plate's top rings
    // longest against its middle of all.
    assert!(dark.tilt_db < hall.tilt_db - 4.0, "{dark:?} vs {hall:?}");
    let sheen = |m: &Metrics| m.rt[3] / m.rt[1];
    for other in [&tight, &drum, &chamber, &hall, &dark, &cathedral, &ambience] {
        assert!(sheen(&plate) > sheen(other), "{plate:?} vs {other:?}");
    }
    // A plate has no discrete reflections.
    assert!(
        decay_profile(&plate_params, SR)
            .early
            .iter()
            .all(|e| e.gain == 0.0)
    );
}

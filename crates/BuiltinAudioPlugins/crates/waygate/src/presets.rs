//! WayGate's factory presets.
//!
//! Each is a whole `Params` with power on; an editor loads one by sending the
//! wire values that differ, so a preset saves and undoes like any other edit.
//! None uses lookahead: every preset is latency-free, so it is safe on a live
//! stage; lookahead is the user's call. There is no ducking preset: the key is
//! the insert's own signal (no external sidechain yet), so "duck under the
//! voice" is not something a preset could honestly promise.

use crate::{Params, default_params};

pub struct FactoryPreset {
    pub name: &'static str,
    pub params: Params,
}

/// The gate's dial-up: threshold, range, attack / hold / release, hysteresis
/// and the key filter.
#[allow(clippy::too_many_arguments)]
fn gate(
    name: &'static str,
    threshold_db: f32,
    range_db: f32,
    attack_ms: f32,
    hold_ms: f32,
    release_ms: f32,
    hysteresis_db: f32,
    key: (f32, f32),
) -> FactoryPreset {
    FactoryPreset {
        name,
        params: Params {
            threshold_db,
            range_db,
            attack_ms,
            hold_ms,
            release_ms,
            hysteresis_db,
            key_hpf_hz: key.0,
            key_lpf_hz: key.1,
            ..default_params()
        },
    }
}

/// The bank, `Default` first.
pub fn factory_presets() -> Vec<FactoryPreset> {
    vec![
        FactoryPreset {
            name: "Default",
            params: default_params(),
        },
        // Fast and fully closed; the key listens below 150 Hz so the snare
        // and hats in the kick mic cannot open it.
        gate("Kick", -30.0, -80.0, 0.05, 50.0, 120.0, 6.0, (20.0, 150.0)),
        // A partial close keeps some room; the key skips the kick's low end
        // and the hats' top.
        gate(
            "Snare",
            -28.0,
            -30.0,
            0.1,
            40.0,
            180.0,
            6.0,
            (120.0, 8_000.0),
        ),
        // Long hold and release for the tom's ring.
        gate("Tom", -32.0, -40.0, 0.2, 120.0, 400.0, 6.0, (60.0, 1_500.0)),
        // A gentle downward expander-like dip between phrases.
        gate(
            "Vocal",
            -50.0,
            -15.0,
            2.0,
            80.0,
            300.0,
            8.0,
            (100.0, 12_000.0),
        ),
        // Shuts the amp's hiss and hum between riffs.
        gate(
            "Guitar Amp",
            -45.0,
            -60.0,
            1.0,
            30.0,
            120.0,
            5.0,
            (80.0, 20_000.0),
        ),
        // A snare or tom mic: the key ignores the hat's top end, so only the
        // drum itself opens the gate.
        gate(
            "Hi-Hat Bleed",
            -26.0,
            -25.0,
            0.05,
            25.0,
            90.0,
            8.0,
            (150.0, 2_500.0),
        ),
    ]
}

//! MixStation's factory presets, ported from the retired React editor.
//!
//! Each is a whole `Params` with power on; an editor loads one by sending the
//! wire values that differ, so a preset saves and undoes like any other edit.
//! Every preset but the empty rack loads all six modules, in their natural
//! order, switched on.

use crate::{Params, default_params};

pub struct FactoryPreset {
    pub name: &'static str,
    pub params: Params,
}

/// Every module in the rack, Filters first, each switched on.
fn loaded_rack() -> Params {
    Params {
        filters_enabled: true,
        eq_enabled: true,
        comp_enabled: true,
        sat_enabled: true,
        width_enabled: true,
        limiter_enabled: true,
        slot1_module: 1,
        slot2_module: 2,
        slot3_module: 3,
        slot4_module: 4,
        slot5_module: 5,
        slot6_module: 6,
        ..default_params()
    }
}

/// The EQ: `(low, low-mid freq, low-mid gain, high-mid freq, high-mid gain,
/// high)`.
type Eq = (f32, f32, f32, f32, f32, f32);
/// The compressor: `(threshold, ratio, attack, release, makeup)`.
type Comp = (f32, f32, f32, f32, f32);

fn preset(
    name: &'static str,
    hpf_hz: f32,
    eq: Eq,
    comp: Comp,
    (sat_drive_pct, sat_character_pct, width_pct): (f32, f32, f32),
    edit: impl FnOnce(&mut Params),
) -> FactoryPreset {
    let mut params = Params {
        hpf_hz,
        low_gain_db: eq.0,
        low_mid_freq_hz: eq.1,
        low_mid_gain_db: eq.2,
        high_mid_freq_hz: eq.3,
        high_mid_gain_db: eq.4,
        high_gain_db: eq.5,
        comp_threshold_db: comp.0,
        comp_ratio: comp.1,
        comp_attack_ms: comp.2,
        comp_release_ms: comp.3,
        comp_makeup_db: comp.4,
        sat_drive_pct,
        sat_character_pct,
        width_pct,
        ..loaded_rack()
    };
    edit(&mut params);
    FactoryPreset { name, params }
}

/// The bank, the empty rack first.
pub fn factory_presets() -> Vec<FactoryPreset> {
    let defaults = default_params();
    vec![
        FactoryPreset {
            name: "Empty Rack",
            params: default_params(),
        },
        preset(
            "Mix Bus Polish",
            28.0,
            (0.8, 320.0, -0.7, 3_200.0, 0.5, 0.9),
            (-15.0, 2.0, 30.0, 250.0, 0.7),
            (12.0, 42.0, 106.0),
            |p| p.limiter_ceiling_db = -0.5,
        ),
        preset(
            "Drum Rack",
            35.0,
            (1.8, 520.0, -1.5, 4_200.0, 1.2, defaults.high_gain_db),
            (-21.0, 4.0, 18.0, 90.0, 1.5),
            (24.0, 68.0, 112.0),
            |_| {},
        ),
        preset(
            "Vocal Focus",
            85.0,
            (-0.8, 280.0, -1.7, 3_600.0, 1.8, 1.2),
            (-24.0, 3.0, 12.0, 160.0, 2.0),
            (8.0, 34.0, 100.0),
            |_| {},
        ),
        preset(
            "Low End Firm",
            24.0,
            (1.2, 180.0, -0.8, 2_400.0, 0.4, defaults.high_gain_db),
            (-20.0, 3.5, 35.0, 180.0, 1.0),
            (18.0, 56.0, 94.0),
            |_| {},
        ),
        preset(
            "Wide Master",
            defaults.hpf_hz,
            (
                0.4,
                defaults.low_mid_freq_hz,
                defaults.low_mid_gain_db,
                defaults.high_mid_freq_hz,
                defaults.high_mid_gain_db,
                0.8,
            ),
            (-12.0, 1.6, 50.0, 400.0, defaults.comp_makeup_db),
            (6.0, 30.0, 118.0),
            |p| {
                p.limiter_ceiling_db = -0.8;
                p.limiter_release_ms = 180.0;
            },
        ),
    ]
}

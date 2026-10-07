//! FA-76's factory presets, ported from the retired Svelte editor.
//!
//! Each is a whole `Params` with power on; an editor loads one by sending the
//! wire values that differ, so a preset saves and undoes like any other edit.

use crate::{Params, RatioButton, default_params};

pub struct FactoryPreset {
    pub name: &'static str,
    pub params: Params,
}

#[allow(clippy::too_many_arguments)]
fn preset(
    name: &'static str,
    ratio: RatioButton,
    input_db: f32,
    output_db: f32,
    attack_us: f32,
    release_ms: f32,
    mix: f32,
    sidechain_hpf_hz: f32,
) -> FactoryPreset {
    FactoryPreset {
        name,
        params: Params {
            ratio,
            input_db,
            output_db,
            attack_us,
            release_ms,
            mix,
            sidechain_hpf_hz,
            ..default_params()
        },
    }
}

/// The bank, `Default` first.
pub fn factory_presets() -> Vec<FactoryPreset> {
    use RatioButton::*;
    vec![
        FactoryPreset {
            name: "Default",
            params: default_params(),
        },
        preset("Vocal Catch", R4, 22.0, -14.0, 80.0, 220.0, 100.0, 120.0),
        preset("Drum Punch", R8, 24.0, -16.0, 20.0, 80.0, 100.0, 80.0),
        preset("Bass Glue", R4, 20.0, -12.0, 200.0, 350.0, 100.0, 40.0),
        preset("Bus Soft", R4, 14.0, -8.0, 400.0, 600.0, 100.0, 90.0),
        preset("Limiting", R20, 26.0, -18.0, 20.0, 120.0, 100.0, 60.0),
        preset("All Buttons", All, 28.0, -20.0, 20.0, 90.0, 100.0, 100.0),
        preset("Parallel Smash", All, 30.0, -18.0, 20.0, 70.0, 42.0, 150.0),
    ]
}

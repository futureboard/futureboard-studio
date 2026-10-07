//! Z-Comp's factory presets, ported from the retired React editor.
//!
//! Each is a whole `Params` with power on and SC Listen off; an editor loads
//! one by sending the wire values that differ, so a preset saves and undoes
//! like any other edit.

use crate::{CompModel, Params, default_params};

pub struct FactoryPreset {
    pub name: &'static str,
    pub params: Params,
}

/// `(threshold, ratio, attack, release, knee, makeup)`, then
/// `(mix, sidechain HPF, link, color, auto release)`.
fn preset(
    name: &'static str,
    model: CompModel,
    (threshold_db, ratio, attack_ms, release_ms, knee_db, makeup_db): (
        f32,
        f32,
        f32,
        f32,
        f32,
        f32,
    ),
    (mix, sidechain_hpf_hz, stereo_link, color, auto_release): (f32, f32, f32, f32, bool),
) -> FactoryPreset {
    FactoryPreset {
        name,
        params: Params {
            model,
            threshold_db,
            ratio,
            attack_ms,
            release_ms,
            knee_db,
            makeup_db,
            mix,
            sidechain_hpf_hz,
            stereo_link,
            color,
            auto_release,
            ..default_params()
        },
    }
}

/// The bank, `Default` first.
pub fn factory_presets() -> Vec<FactoryPreset> {
    use CompModel::*;
    vec![
        FactoryPreset {
            name: "Default",
            params: default_params(),
        },
        preset(
            "Vocal Catch",
            Avalon,
            (-22.0, 3.0, 12.0, 180.0, 8.0, 2.0),
            (100.0, 120.0, 100.0, 22.0, true),
        ),
        preset(
            "Drum Punch",
            Distressor,
            (-16.0, 6.0, 0.8, 80.0, 2.0, 3.0),
            (100.0, 80.0, 100.0, 45.0, false),
        ),
        preset(
            "Bass Glue",
            Comp2500,
            (-20.0, 4.0, 25.0, 280.0, 6.0, 1.5),
            (100.0, 40.0, 100.0, 28.0, true),
        ),
        preset(
            "Bus Soft",
            Ssl,
            (-14.0, 2.5, 18.0, 420.0, 10.0, 0.5),
            (100.0, 90.0, 100.0, 12.0, true),
        ),
        preset(
            "Mix Bus Glue",
            Ssl,
            (-12.0, 2.0, 30.0, 600.0, 12.0, 0.0),
            (100.0, 100.0, 100.0, 8.0, true),
        ),
        preset(
            "Parallel Smash",
            Distressor,
            (-28.0, 12.0, 0.2, 60.0, 0.0, 6.0),
            (38.0, 150.0, 100.0, 70.0, false),
        ),
        preset(
            "Optical Level",
            Avalon,
            (-24.0, 4.0, 40.0, 800.0, 14.0, 3.0),
            (100.0, 60.0, 100.0, 18.0, true),
        ),
    ]
}

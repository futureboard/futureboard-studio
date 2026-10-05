//! Transient's factory presets, ported from the retired React editor.
//!
//! Each is a whole `Params` with power on; an editor loads one by sending the
//! wire values that differ, so a preset saves and undoes like any other edit.

use crate::{Params, default_params};

pub struct FactoryPreset {
    pub name: &'static str,
    pub params: Params,
}

fn preset(name: &'static str, attack: f32, sustain: f32, speed: f32) -> FactoryPreset {
    FactoryPreset {
        name,
        params: Params {
            attack,
            sustain,
            speed,
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
        preset("Punch Up", 55.0, -15.0, 60.0),
        preset("Snap Cut", -45.0, 20.0, 70.0),
        preset("Body Boost", 10.0, 50.0, 40.0),
        preset("Drum Gate", 35.0, -70.0, 80.0),
    ]
}

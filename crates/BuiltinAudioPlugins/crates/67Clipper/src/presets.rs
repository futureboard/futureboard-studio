//! 67Clipper's factory presets, ported from the retired React editor.
//!
//! Each is a whole `Params` with power on; an editor loads one by sending the
//! wire values that differ, so a preset saves and undoes like any other edit.

use crate::{Mode, Params, default_params};

pub struct FactoryPreset {
    pub name: &'static str,
    pub params: Params,
}

fn preset(
    name: &'static str,
    mode: Mode,
    threshold_db: f32,
    shape: f32,
    ceiling_db: f32,
    dc_filter: bool,
) -> FactoryPreset {
    FactoryPreset {
        name,
        params: Params {
            mode,
            threshold_db,
            shape,
            ceiling_db,
            dc_filter,
            ..default_params()
        },
    }
}

/// The bank, `Default` first.
pub fn factory_presets() -> Vec<FactoryPreset> {
    use Mode::*;
    vec![
        FactoryPreset {
            name: "Default",
            params: default_params(),
        },
        preset("Soft Clip", Clip, -3.0, 80.0, -0.3, true),
        preset("Aggressive", Clip, -12.0, 12.0, -0.1, true),
        preset("Hybrid Glue", Hybrid, -8.0, 60.0, -0.3, true),
        preset("Brick Limit", Limit, -1.0, 0.0, -0.1, false),
    ]
}

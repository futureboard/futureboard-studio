//! 67Clipper's factory presets.
//!
//! Each is a whole `Params` with power on; an editor loads one by sending the
//! wire values that differ, so a preset saves and undoes like any other edit.
//! Oversampling and Delta are how you listen, not the sound: editors keep
//! them as they are when a preset loads.

use crate::{Mode, Params, default_params};

pub struct FactoryPreset {
    pub name: &'static str,
    pub params: Params,
}

fn preset(
    name: &'static str,
    mode: Mode,
    input_db: f32,
    threshold_db: f32,
    shape: f32,
    ceiling_db: f32,
    dc_filter: bool,
) -> FactoryPreset {
    FactoryPreset {
        name,
        params: Params {
            mode,
            input_db,
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
        // A gentle push into a wide knee at the ceiling.
        preset("Soft Clip", Clip, 3.0, 0.0, 80.0, -0.3, true),
        // Hard-cornered and driven: drums and loud masters.
        preset("Aggressive", Clip, 8.0, 0.0, 10.0, -0.1, true),
        // Peaks shaved 4 dB below full scale, no push: quieter, not louder.
        preset("Peak Shave", Clip, 0.0, -4.0, 40.0, -0.3, true),
        // The clipper rounds the first few dB, the limiter the rest.
        preset("Hybrid Glue", Hybrid, 4.0, 0.0, 60.0, -0.3, true),
        preset("Brick Limit", Limit, 1.0, 0.0, 0.0, -0.1, false),
    ]
}

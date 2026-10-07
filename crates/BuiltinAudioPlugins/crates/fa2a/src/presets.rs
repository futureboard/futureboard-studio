//! FA-2A's factory presets.
//!
//! Each is a whole `Params` with power on; an editor loads one by sending the
//! wire values that differ, so a preset saves and undoes like any other edit.

use crate::{Mode, Params, default_params};

pub struct FactoryPreset {
    pub name: &'static str,
    pub params: Params,
}

#[allow(clippy::too_many_arguments)]
fn preset(
    name: &'static str,
    mode: Mode,
    peak_reduction: f32,
    gain_db: f32,
    emphasis: f32,
    color: f32,
    sidechain_low_cut_hz: f32,
    mix: f32,
) -> FactoryPreset {
    FactoryPreset {
        name,
        params: Params {
            mode,
            peak_reduction,
            gain_db,
            emphasis,
            color,
            sidechain_low_cut_hz,
            mix,
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
        preset("Vocal Level", Compress, 48.0, 4.0, 55.0, 14.0, 110.0, 100.0),
        preset("Bass Smooth", Compress, 42.0, 3.0, 20.0, 18.0, 40.0, 100.0),
        preset("Gentle Bus", Compress, 25.0, 1.5, 35.0, 8.0, 90.0, 100.0),
        preset(
            "Acoustic Glue",
            Compress,
            36.0,
            2.5,
            60.0,
            10.0,
            140.0,
            100.0,
        ),
        preset("Peak Limit", Limit, 55.0, 3.0, 45.0, 12.0, 90.0, 100.0),
        preset("Parallel Squash", Limit, 80.0, 8.0, 40.0, 30.0, 120.0, 40.0),
    ]
}

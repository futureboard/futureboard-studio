//! Imager's factory presets: the retired React editor's, and three for
//! stereoize and Recover Sides.
//!
//! Each is a whole `Params` with power on and no band soloed; an editor loads
//! one by sending the wire values that differ, so a preset saves and undoes
//! like any other edit.

use crate::{
    BAND_COUNT, CROSSOVER_COUNT, DEFAULT_CROSSOVERS_HZ, Params, StereoizeMode, default_params,
};

pub struct FactoryPreset {
    pub name: &'static str,
    pub params: Params,
}

fn preset(
    name: &'static str,
    width: [f32; BAND_COUNT],
    crossover_hz: [f32; CROSSOVER_COUNT],
    output_db: f32,
) -> FactoryPreset {
    FactoryPreset {
        name,
        params: Params {
            width,
            crossover_hz,
            output_db,
            ..default_params()
        },
    }
}

/// Mono-ish material given width: stereoize in `mode`, by band.
fn stereoized(
    name: &'static str,
    stereoize_mode: StereoizeMode,
    stereoize: [f32; BAND_COUNT],
    width: [f32; BAND_COUNT],
) -> FactoryPreset {
    FactoryPreset {
        name,
        params: Params {
            stereoize,
            stereoize_mode,
            width,
            ..default_params()
        },
    }
}

/// The bank, `Default` first.
pub fn factory_presets() -> Vec<FactoryPreset> {
    let split = DEFAULT_CROSSOVERS_HZ;
    vec![
        FactoryPreset {
            name: "Default",
            params: default_params(),
        },
        preset(
            "Mono Bass",
            [0.0, 100.0, 100.0, 100.0],
            [150.0, 1_000.0, 6_000.0],
            0.0,
        ),
        preset(
            "Master Polish",
            [40.0, 100.0, 115.0, 130.0],
            [120.0, 800.0, 7_000.0],
            0.0,
        ),
        preset(
            "Wide Air",
            [100.0, 100.0, 120.0, 160.0],
            [120.0, 1_500.0, 9_000.0],
            0.0,
        ),
        preset(
            "Vocal Focus",
            [70.0, 60.0, 100.0, 120.0],
            [200.0, 1_200.0, 5_000.0],
            0.0,
        ),
        preset(
            "Drum Bus",
            [30.0, 90.0, 120.0, 135.0],
            [110.0, 700.0, 5_500.0],
            0.0,
        ),
        preset(
            "Super Wide",
            [60.0, 140.0, 170.0, 200.0],
            [150.0, 1_200.0, 6_000.0],
            -1.5,
        ),
        preset("Narrow", [50.0; BAND_COUNT], split, 0.0),
        preset("Mono Check", [0.0; BAND_COUNT], split, 0.0),
        stereoized(
            "Stereoize Synth",
            StereoizeMode::One,
            [0.0, 30.0, 45.0, 50.0],
            [100.0; BAND_COUNT],
        ),
        stereoized(
            "Natural Space",
            StereoizeMode::Two,
            [0.0, 20.0, 35.0, 40.0],
            [80.0, 100.0, 110.0, 120.0],
        ),
        FactoryPreset {
            name: "Tight Low End",
            // The narrowed lows stay in the mix, folded into the middle.
            params: Params {
                width: [0.0, 70.0, 100.0, 110.0],
                crossover_hz: [180.0, 1_000.0, 6_000.0],
                recover_sides: true,
                ..default_params()
            },
        },
    ]
}

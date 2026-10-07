//! The Compressor's factory presets.
//!
//! Each is a whole `Params` with power on and no band soloed; an editor loads
//! one by sending the wire values that differ, so a preset saves and undoes
//! like any other edit.

use crate::{Band, DEFAULT_BANDS, DEFAULT_CROSSOVERS_HZ, Mode, Params, default_params};

pub struct FactoryPreset {
    pub name: &'static str,
    pub params: Params,
}

/// A single-band setting: `(threshold, ratio, attack, release, makeup)`,
/// then the knee and the sidechain high-pass.
fn single(
    name: &'static str,
    (threshold_db, ratio, attack_ms, release_ms, makeup_db): (f32, f32, f32, f32, f32),
    knee_db: f32,
    sidechain_hpf_hz: f32,
) -> FactoryPreset {
    FactoryPreset {
        name,
        params: Params {
            threshold_db,
            ratio,
            attack_ms,
            release_ms,
            makeup_db,
            knee_db,
            sidechain_hpf_hz,
            ..default_params()
        },
    }
}

const fn band(
    threshold_db: f32,
    ratio: f32,
    attack_ms: f32,
    release_ms: f32,
    makeup_db: f32,
) -> Band {
    Band {
        threshold_db,
        ratio,
        attack_ms,
        release_ms,
        makeup_db,
        bypass: false,
    }
}

fn multi(
    name: &'static str,
    crossover_hz: [f32; 3],
    bands: [Band; 4],
    knee_db: f32,
) -> FactoryPreset {
    FactoryPreset {
        name,
        params: Params {
            mode: Mode::Multi,
            crossover_hz,
            bands,
            knee_db,
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
        single("Vocal Smooth", (-22.0, 3.0, 8.0, 160.0, 3.0), 8.0, 90.0),
        single("Drum Bus", (-16.0, 4.0, 25.0, 90.0, 2.5), 4.0, 60.0),
        single("Master Glue", (-12.0, 1.8, 30.0, 300.0, 1.0), 10.0, 40.0),
        single("Bass Control", (-20.0, 5.0, 15.0, 200.0, 3.0), 6.0, 0.0),
        multi(
            "Multiband Master",
            [120.0, 900.0, 6_000.0],
            [
                band(-18.0, 2.5, 30.0, 250.0, 1.0),
                band(-16.0, 2.0, 15.0, 180.0, 0.5),
                band(-18.0, 2.0, 8.0, 120.0, 0.5),
                band(-20.0, 2.5, 3.0, 90.0, 0.5),
            ],
            8.0,
        ),
        multi(
            "Tame Lows",
            DEFAULT_CROSSOVERS_HZ,
            [
                band(-24.0, 4.0, 20.0, 220.0, 2.0),
                DEFAULT_BANDS[1],
                DEFAULT_BANDS[2],
                DEFAULT_BANDS[3],
            ],
            6.0,
        ),
        multi(
            "De-Harsh",
            [150.0, 2_500.0, 7_000.0],
            [
                DEFAULT_BANDS[0],
                DEFAULT_BANDS[1],
                band(-26.0, 4.0, 2.0, 60.0, 0.0),
                band(-24.0, 3.0, 1.0, 50.0, 0.0),
            ],
            6.0,
        ),
    ]
}

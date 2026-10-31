//! The De-Esser's factory presets.
//!
//! Each is a whole `Params` with power on; an editor loads one by sending the
//! wire values that differ, so a preset saves and undoes like any other edit.

use crate::{Mode, Params, StereoMode, default_params};

pub struct FactoryPreset {
    pub name: &'static str,
    pub params: Params,
}

fn preset(name: &'static str, freq_hz: f32, edit: impl FnOnce(&mut Params)) -> FactoryPreset {
    let mut params = Params {
        freq_hz,
        ..default_params()
    };
    edit(&mut params);
    FactoryPreset { name, params }
}

/// The bank, `Default` first.
pub fn factory_presets() -> Vec<FactoryPreset> {
    vec![
        FactoryPreset {
            name: "Default",
            params: default_params(),
        },
        preset("Female Vocal", 7_500.0, |p| {
            p.width_oct = 1.0;
            p.threshold_db = -32.0;
        }),
        preset("Male Vocal", 5_500.0, |p| {
            p.width_oct = 1.2;
            p.threshold_db = -30.0;
        }),
        preset("Podcast", 6_000.0, |p| {
            p.width_oct = 1.6;
            p.threshold_db = -34.0;
            p.range_db = 8.0;
            p.release_ms = 90.0;
        }),
        preset("Gentle", 7_000.0, |p| {
            p.width_oct = 1.4;
            p.threshold_db = -26.0;
            p.range_db = 5.0;
            p.release_ms = 100.0;
        }),
        preset("Hard Sibilance", 7_200.0, |p| {
            p.width_oct = 0.8;
            p.threshold_db = -38.0;
            p.range_db = 18.0;
            p.release_ms = 40.0;
            p.lookahead_ms = 4.0;
        }),
        preset("Hi-Hat Tamer", 10_000.0, |p| {
            p.width_oct = 1.8;
            p.threshold_db = -30.0;
            p.range_db = 8.0;
            p.release_ms = 30.0;
        }),
        preset("Wideband Bright", 8_000.0, |p| {
            p.mode = Mode::Wide;
            p.width_oct = 1.6;
            p.threshold_db = -28.0;
            p.range_db = 6.0;
        }),
        preset("Side Air", 9_000.0, |p| {
            p.stereo_mode = StereoMode::Side;
            p.width_oct = 2.0;
            p.threshold_db = -36.0;
            p.range_db = 8.0;
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bank_opens_on_the_defaults_and_names_are_unique() {
        let bank = factory_presets();
        assert_eq!(bank[0].name, "Default");
        assert_eq!(bank[0].params, default_params());
        for (i, a) in bank.iter().enumerate() {
            assert!(a.params.power);
            assert!(!a.params.listen);
            let mut sanitized = a.params.clone();
            crate::ipc::sanitize_params(&mut sanitized);
            assert_eq!(sanitized, a.params, "{} is in range", a.name);
            for b in &bank[i + 1..] {
                assert_ne!(a.name, b.name);
            }
        }
    }
}

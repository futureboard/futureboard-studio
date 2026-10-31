//! AirBoost's factory presets.
//!
//! Each is a whole `Params` with power on; an editor loads one by sending the
//! wire values that differ, so a preset saves and undoes like any other edit.

use crate::{AirFreq, Params, Shape, StereoMode, default_params};

pub struct FactoryPreset {
    pub name: &'static str,
    pub params: Params,
}

fn preset(
    name: &'static str,
    air_freq: AirFreq,
    air_db: f32,
    edit: impl FnOnce(&mut Params),
) -> FactoryPreset {
    let mut params = Params {
        air_freq,
        air_db,
        ..default_params()
    };
    edit(&mut params);
    FactoryPreset { name, params }
}

/// The bank, `Default` first.
pub fn factory_presets() -> Vec<FactoryPreset> {
    use AirFreq::*;
    vec![
        FactoryPreset {
            name: "Default",
            params: default_params(),
        },
        preset("Vocal Air", K12, 5.0, |p| {
            p.presence_db = 1.5;
            p.excite = 20.0;
            p.deess = 55.0;
        }),
        preset("Silk Top", K20, 6.0, |p| {
            p.excite = 10.0;
            p.deess = 20.0;
        }),
        preset("Bright Guitar", K8, 4.0, |p| {
            p.shape = Shape::Bell;
            p.presence_db = 2.5;
            p.excite = 35.0;
            p.deess = 0.0;
        }),
        preset("Hi-Hat Sheen", K12, 3.0, |p| {
            p.excite = 45.0;
            p.deess = 40.0;
        }),
        preset("Master Sparkle", K40, 3.0, |p| {
            p.excite = 8.0;
            p.deess = 15.0;
            p.mix = 80.0;
        }),
        preset("Side Shine", K12, 6.0, |p| {
            p.stereo_mode = StereoMode::Side;
            p.excite = 30.0;
            p.deess = 10.0;
        }),
        preset("Dull Mix Lift", K5, 3.5, |p| {
            p.presence_db = 1.0;
            p.excite = 40.0;
            p.deess = 35.0;
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
        assert_eq!(
            crate::ipc::ui_values(&bank[0].params),
            crate::ipc::ui_values(&default_params())
        );
        for (i, a) in bank.iter().enumerate() {
            assert!(a.params.power);
            let mut copy = a.params.clone();
            crate::ipc::sanitize_params(&mut copy);
            assert_eq!(
                crate::ipc::ui_values(&copy),
                crate::ipc::ui_values(&a.params)
            );
            for b in &bank[i + 1..] {
                assert_ne!(a.name, b.name);
            }
        }
    }
}

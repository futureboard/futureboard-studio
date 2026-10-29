//! Zapp's factory presets.
//!
//! Each is a whole `Params` with power on; an editor loads one by sending the
//! wire values that differ, so a preset saves and undoes like any other edit.

use crate::{Character, Oversampling, Params, StereoMode, default_params};

pub struct FactoryPreset {
    pub name: &'static str,
    pub params: Params,
}

fn preset(
    name: &'static str,
    character: Character,
    drive_db: f32,
    edit: impl FnOnce(&mut Params),
) -> FactoryPreset {
    let mut params = Params {
        character,
        drive_db,
        ..default_params()
    };
    edit(&mut params);
    FactoryPreset { name, params }
}

/// The bank, `Default` first.
pub fn factory_presets() -> Vec<FactoryPreset> {
    use Character::*;
    vec![
        FactoryPreset {
            name: "Default",
            params: default_params(),
        },
        preset("Warm Tape", Tape, 12.0, |p| {
            p.tone_db = -2.0;
            p.high_cut_hz = 14_000.0;
        }),
        preset("Tube Glow", Tube, 10.0, |p| {
            p.bias = 0.25;
            p.mix = 70.0;
        }),
        preset("Bus Glue", Tape, 6.0, |p| {
            p.low_cut_hz = 30.0;
            p.mix = 50.0;
        }),
        preset("Transistor Bite", Transistor, 16.0, |p| {
            p.tone_db = 2.5;
            p.low_cut_hz = 80.0;
        }),
        preset("Diode Fuzz", Diode, 24.0, |p| {
            p.bias = 0.4;
            p.high_cut_hz = 8_000.0;
            p.oversampling = Oversampling::X8;
        }),
        preset("Wavefolder", Fold, 14.0, |p| {
            p.mix = 80.0;
            p.oversampling = Oversampling::X8;
        }),
        preset("Bitcrush Lo-Fi", Crush, 18.0, |p| {
            p.high_cut_hz = 6_000.0;
            p.mix = 65.0;
            p.oversampling = Oversampling::Off;
        }),
        preset("Wide Side Heat", Tape, 14.0, |p| {
            p.stereo_mode = StereoMode::Side;
            p.tone_db = 1.5;
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
            for b in &bank[i + 1..] {
                assert_ne!(a.name, b.name);
            }
        }
    }
}

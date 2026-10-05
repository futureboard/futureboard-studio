//! VerbSpace's factory presets, ported from the retired Svelte editor.
//!
//! Each is a whole `Params` (power on, freeze off); an editor loads one by
//! sending the wire values that differ, so a preset saves and undoes like any
//! other edit.

use crate::{Params, ReverbMode, default_params};

pub struct FactoryPreset {
    pub name: &'static str,
    pub params: Params,
}

#[allow(clippy::too_many_arguments)]
fn preset(
    name: &'static str,
    mode: ReverbMode,
    predelay_ms: f32,
    size: f32,
    decay_sec: f32,
    diffusion: f32,
    damping: f32,
    bass_mult: f32,
    modulation: (f32, f32),
    cuts: (f32, f32),
    width: f32,
    mix: f32,
) -> FactoryPreset {
    FactoryPreset {
        name,
        params: Params {
            mode,
            predelay_ms,
            size,
            decay_sec,
            diffusion,
            damping,
            bass_mult,
            mod_depth: modulation.0,
            mod_rate_hz: modulation.1,
            low_cut_hz: cuts.0,
            high_cut_hz: cuts.1,
            width,
            mix,
            ..default_params()
        },
    }
}

/// The bank, `Default` first.
pub fn factory_presets() -> Vec<FactoryPreset> {
    use ReverbMode::*;
    vec![
        FactoryPreset {
            name: "Default",
            params: default_params(),
        },
        preset(
            "Tight Room",
            Room,
            8.0,
            32.0,
            0.55,
            58.0,
            55.0,
            0.95,
            (12.0, 0.45),
            (120.0, 11_000.0),
            95.0,
            22.0,
        ),
        preset(
            "Live Chamber",
            Chamber,
            18.0,
            48.0,
            1.35,
            78.0,
            42.0,
            1.05,
            (18.0, 0.55),
            (95.0, 12_000.0),
            105.0,
            26.0,
        ),
        preset(
            "Concert Hall",
            Hall,
            28.0,
            82.0,
            3.8,
            80.0,
            38.0,
            1.25,
            (32.0, 0.42),
            (70.0, 9_000.0),
            125.0,
            30.0,
        ),
        preset(
            "Bright Plate",
            Plate,
            12.0,
            55.0,
            1.9,
            88.0,
            22.0,
            0.9,
            (14.0, 0.7),
            (140.0, 16_000.0),
            115.0,
            28.0,
        ),
        preset(
            "Dark Hall",
            Hall,
            35.0,
            74.0,
            4.6,
            70.0,
            68.0,
            1.45,
            (28.0, 0.35),
            (55.0, 6_200.0),
            118.0,
            32.0,
        ),
        preset(
            "Vocal Ambience",
            Ambience,
            14.0,
            38.0,
            0.95,
            65.0,
            40.0,
            0.85,
            (20.0, 0.8),
            (160.0, 14_000.0),
            100.0,
            18.0,
        ),
        preset(
            "Drum Room",
            Room,
            4.0,
            40.0,
            0.72,
            50.0,
            48.0,
            1.15,
            (8.0, 0.35),
            (80.0, 10_500.0),
            90.0,
            24.0,
        ),
        preset(
            "Infinite Pad",
            Hall,
            40.0,
            100.0,
            14.0,
            92.0,
            30.0,
            1.2,
            (55.0, 0.25),
            (120.0, 7_500.0),
            140.0,
            40.0,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc;

    #[test]
    fn every_preset_is_already_in_range_and_named_once() {
        let bank = factory_presets();
        assert_eq!(bank[0].name, "Default");
        let mut names: Vec<_> = bank.iter().map(|p| p.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), bank.len());
        for preset in &bank {
            let mut clamped = preset.params.clone();
            ipc::sanitize_params(&mut clamped);
            assert_eq!(clamped, preset.params, "{} is out of range", preset.name);
            assert!(preset.params.power && !preset.params.freeze);
        }
    }
}

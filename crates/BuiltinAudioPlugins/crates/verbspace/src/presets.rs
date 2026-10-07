//! VerbSpace's factory presets.
//!
//! Each is a whole `Params` (power on, freeze and wet-only off); an editor
//! loads one by sending the wire values that differ, so a preset saves and
//! undoes like any other edit. The mode is the space type each started
//! from, a label only.

use crate::{Params, ReverbMode, default_params};

pub struct FactoryPreset {
    pub name: &'static str,
    pub params: Params,
}

/// A preset's room: `(predelayMs, size, decaySec, diffusion, earlyLate)`.
type Room = (f32, f32, f32, f32, f32);
/// Its decay colour: `(damping, dampFreqHz, bassMult, bassFreqHz)`.
type Colour = (f32, f32, f32, f32);
/// Its motion and output: `(modDepth, modRateHz, lowCutHz, highCutHz,
/// width, mix)`.
type Finish = (f32, f32, f32, f32, f32, f32);

fn preset(
    name: &'static str,
    mode: ReverbMode,
    room: Room,
    colour: Colour,
    finish: Finish,
) -> FactoryPreset {
    let (predelay_ms, size, decay_sec, diffusion, early_late) = room;
    let (damping, damp_freq_hz, bass_mult, bass_freq_hz) = colour;
    let (mod_depth, mod_rate_hz, low_cut_hz, high_cut_hz, width, mix) = finish;
    FactoryPreset {
        name,
        params: Params {
            mode,
            predelay_ms,
            size,
            decay_sec,
            diffusion,
            early_late,
            damping,
            damp_freq_hz,
            bass_mult,
            bass_freq_hz,
            mod_depth,
            mod_rate_hz,
            low_cut_hz,
            high_cut_hz,
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
            (3.0, 18.0, 0.45, 70.0, 35.0),
            (50.0, 4_000.0, 1.0, 250.0),
            (10.0, 0.6, 120.0, 11_000.0, 90.0, 22.0),
        ),
        preset(
            "Drum Room",
            Room,
            (2.0, 28.0, 0.8, 60.0, 35.0),
            (50.0, 4_000.0, 1.2, 200.0),
            (6.0, 0.4, 80.0, 10_000.0, 100.0, 24.0),
        ),
        preset(
            "Vocal Ambience",
            Ambience,
            (12.0, 15.0, 0.6, 65.0, 30.0),
            (45.0, 5_000.0, 0.8, 300.0),
            (15.0, 0.8, 180.0, 14_000.0, 100.0, 18.0),
        ),
        preset(
            "Live Chamber",
            Chamber,
            (12.0, 42.0, 1.3, 85.0, 55.0),
            (35.0, 5_500.0, 1.1, 250.0),
            (18.0, 0.7, 100.0, 12_000.0, 105.0, 26.0),
        ),
        preset(
            "Bright Plate",
            Plate,
            (10.0, 35.0, 1.9, 95.0, 100.0),
            (10.0, 9_000.0, 0.8, 400.0),
            (30.0, 1.0, 150.0, 18_000.0, 115.0, 26.0),
        ),
        preset(
            "Concert Hall",
            Hall,
            (28.0, 78.0, 3.2, 85.0, 70.0),
            (40.0, 4_000.0, 1.35, 300.0),
            (28.0, 0.5, 60.0, 10_000.0, 125.0, 30.0),
        ),
        preset(
            "Dark Hall",
            Hall,
            (35.0, 80.0, 4.5, 80.0, 70.0),
            (85.0, 1_500.0, 1.6, 350.0),
            (25.0, 0.4, 50.0, 5_000.0, 118.0, 32.0),
        ),
        preset(
            "Cathedral",
            Hall,
            (45.0, 100.0, 7.5, 85.0, 80.0),
            (50.0, 3_000.0, 1.5, 300.0),
            (20.0, 0.3, 60.0, 9_000.0, 130.0, 30.0),
        ),
        preset(
            "Infinite Pad",
            Hall,
            (40.0, 100.0, 16.0, 95.0, 95.0),
            (45.0, 3_500.0, 1.3, 250.0),
            (45.0, 0.3, 120.0, 8_000.0, 140.0, 40.0),
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
            assert!(preset.params.power && !preset.params.freeze && !preset.params.wet_only);
        }
    }
}

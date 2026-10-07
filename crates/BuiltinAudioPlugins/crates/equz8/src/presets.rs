//! EQ-Z8's factory presets, moved here from the retired React editor so the
//! Rust crate stays the authority for every value the plug-in can hold.
//!
//! A preset switches the whole strip on with the numbers below laid over the
//! default band layout; "Default" is the neutral state with every band off.
//! Solo is an audition, never part of a preset.

use std::sync::OnceLock;

use crate::{BAND_COUNT, BandParams, Params, default_params, ipc};

/// One factory preset.
#[derive(Debug, Clone)]
pub struct FactoryPreset {
    pub name: &'static str,
    pub params: Params,
}

/// The overrides one band of a preset sets: frequency, gain and Q.
type BandLook = (f32, f32, f32);

fn preset(name: &'static str, bands: [BandLook; BAND_COUNT], output_db: f32) -> FactoryPreset {
    let mut params = default_params();
    params.output_db = output_db;
    params.solo_band = ipc::SOLO_NONE;
    for (band, (freq, gain_db, q)) in params.bands.iter_mut().zip(bands) {
        *band = BandParams {
            active: true,
            freq,
            gain_db,
            q,
            dynamic: false,
            range_db: 0.0,
            ..*band
        };
    }
    FactoryPreset { name, params }
}

/// Every factory preset, "Default" first.
pub fn factory_presets() -> &'static [FactoryPreset] {
    static PRESETS: OnceLock<Vec<FactoryPreset>> = OnceLock::new();
    PRESETS.get_or_init(|| {
        vec![
            FactoryPreset {
                name: "Default",
                params: default_params(),
            },
            preset(
                "Low-End Control",
                [
                    (32.0, 0.0, 0.72),
                    (95.0, -1.5, 0.8),
                    (220.0, -2.2, 1.15),
                    (620.0, 0.0, 1.2),
                    (1_600.0, 0.8, 1.0),
                    (3_800.0, 0.0, 1.1),
                    (9_500.0, 0.7, 0.75),
                    (19_000.0, 0.0, 0.7),
                ],
                0.0,
            ),
            preset(
                "Vocal Clarity",
                [
                    (70.0, 0.0, 0.75),
                    (140.0, -1.2, 0.8),
                    (320.0, -2.4, 1.3),
                    (900.0, -0.8, 1.5),
                    (2_600.0, 2.2, 1.1),
                    (4_800.0, -1.4, 2.0),
                    (11_000.0, 1.8, 0.72),
                    (19_500.0, 0.0, 0.7),
                ],
                0.0,
            ),
            preset(
                "Mix Bus Polish",
                [
                    (24.0, 0.0, 0.7),
                    (110.0, 0.7, 0.75),
                    (280.0, -0.8, 1.0),
                    (780.0, -0.4, 1.2),
                    (2_200.0, 0.5, 0.9),
                    (5_200.0, -0.5, 1.4),
                    (12_500.0, 1.1, 0.68),
                    (20_000.0, 0.0, 0.7),
                ],
                -0.3,
            ),
            preset(
                "Tame Harshness",
                [
                    (28.0, 0.0, 0.7),
                    (120.0, 0.0, 0.8),
                    (300.0, -0.7, 1.1),
                    (1_100.0, 0.4, 1.2),
                    (3_100.0, -1.1, 1.8),
                    (6_200.0, -2.0, 2.4),
                    (10_500.0, -1.0, 0.8),
                    (19_000.0, 0.0, 0.72),
                ],
                0.0,
            ),
            preset(
                "Air Lift",
                [
                    (26.0, 0.0, 0.7),
                    (130.0, -0.6, 0.8),
                    (260.0, 0.0, 1.2),
                    (700.0, -0.5, 1.3),
                    (1_800.0, 0.6, 1.0),
                    (4_200.0, 1.2, 1.2),
                    (13_500.0, 2.6, 0.66),
                    (20_000.0, 0.0, 0.7),
                ],
                0.0,
            ),
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_are_valid_and_leave_solo_off() {
        let presets = factory_presets();
        assert_eq!(presets[0].name, "Default");
        assert!(presets[0].params.bands.iter().all(|band| !band.active));
        for preset in &presets[1..] {
            let mut sanitized = preset.params.clone();
            ipc::sanitize_params(&mut sanitized);
            assert_eq!(
                ipc::ui_values(&sanitized),
                ipc::ui_values(&preset.params),
                "{} holds a value the DSP would pin",
                preset.name
            );
            assert_eq!(preset.params.solo_band, ipc::SOLO_NONE);
            assert!(preset.params.bands.iter().all(|band| band.active));
        }
        // The band layout is kept: the shapes are the default ones.
        assert_eq!(
            presets[2].params.bands[0].band_type,
            crate::BandType::HighPass
        );
        assert_eq!(presets[2].params.bands[4].gain_db, 2.2);
    }
}

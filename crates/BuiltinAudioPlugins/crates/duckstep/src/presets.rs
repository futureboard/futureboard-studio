//! Duckstep's presets.
//!
//! Two kinds. The factory bank is whole `Params` with power on, which the
//! editor's preset bar loads as ordinary wire edits. The shape presets and the
//! detector presets are the design's quick picks: an editor action writes one
//! into the points and curve, or into the detector and envelope settings, and
//! leaves everything else as it is.

use crate::{Mode, Params, ReleaseSync, default_params};

pub struct FactoryPreset {
    pub name: &'static str,
    pub params: Params,
}

/// A shaper curve: its points, x and y in 0…1 with the first at x 0 and the
/// last at x 1, and the segments' bow.
pub struct ShapePreset {
    pub name: &'static str,
    pub points: &'static [(f32, f32)],
    pub curve: f32,
}

/// The shaper's quick picks, in the design's order.
pub const SHAPE_PRESETS: [ShapePreset; 5] = [
    ShapePreset {
        name: "Pump",
        points: &[(0.0, 0.0), (0.04, 0.0), (0.5, 0.92), (1.0, 1.0)],
        curve: 45.0,
    },
    ShapePreset {
        name: "Smooth",
        points: &[(0.0, 0.25), (0.6, 0.9), (1.0, 1.0)],
        curve: 20.0,
    },
    ShapePreset {
        name: "Hard",
        points: &[(0.0, 0.0), (0.22, 0.0), (0.26, 1.0), (1.0, 1.0)],
        curve: 0.0,
    },
    ShapePreset {
        name: "Gate",
        points: &[(0.0, 1.0), (0.47, 1.0), (0.5, 0.0), (0.97, 0.0), (1.0, 1.0)],
        curve: 0.0,
    },
    ShapePreset {
        name: "Bounce",
        points: &[(0.0, 0.0), (0.2, 0.8), (0.3, 0.35), (0.55, 1.0), (1.0, 1.0)],
        curve: 30.0,
    },
];

/// A ducker's detector and envelope settings.
pub struct DetectorPreset {
    pub name: &'static str,
    pub threshold_db: f32,
    pub ratio: f32,
    pub knee_db: f32,
    pub attack_ms: f32,
    pub hold_ms: f32,
    pub release_ms: f32,
    pub lookahead_ms: f32,
    pub range_db: f32,
    pub release_sync: ReleaseSync,
}

/// The ducker's quick picks, in the design's order; the first is the
/// default.
pub const DETECTOR_PRESETS: [DetectorPreset; 4] = [
    DetectorPreset {
        name: "Kick Duck",
        threshold_db: -24.0,
        ratio: 8.0,
        knee_db: 6.0,
        attack_ms: 1.0,
        hold_ms: 20.0,
        release_ms: 180.0,
        lookahead_ms: 2.0,
        range_db: 24.0,
        release_sync: ReleaseSync::Off,
    },
    DetectorPreset {
        name: "Gentle Glue",
        threshold_db: -28.0,
        ratio: 3.0,
        knee_db: 12.0,
        attack_ms: 5.0,
        hold_ms: 0.0,
        release_ms: 320.0,
        lookahead_ms: 0.0,
        range_db: 9.0,
        release_sync: ReleaseSync::Off,
    },
    DetectorPreset {
        name: "Hard Pump",
        threshold_db: -20.0,
        ratio: 20.0,
        knee_db: 0.0,
        attack_ms: 0.3,
        hold_ms: 40.0,
        release_ms: 140.0,
        lookahead_ms: 4.0,
        range_db: 36.0,
        release_sync: ReleaseSync::Eighth,
    },
    DetectorPreset {
        name: "Vocal Duck",
        threshold_db: -32.0,
        ratio: 4.0,
        knee_db: 10.0,
        attack_ms: 10.0,
        hold_ms: 60.0,
        release_ms: 450.0,
        lookahead_ms: 5.0,
        range_db: 12.0,
        release_sync: ReleaseSync::Off,
    },
];

fn ducker(name: &'static str, detector: &DetectorPreset) -> FactoryPreset {
    let mut params = default_params();
    params.set_detector(detector);
    FactoryPreset { name, params }
}

fn shaper(
    name: &'static str,
    shape: &ShapePreset,
    edit: impl FnOnce(&mut Params),
) -> FactoryPreset {
    let mut params = Params {
        mode: Mode::Shaper,
        ..default_params()
    };
    params.set_shape(shape);
    edit(&mut params);
    FactoryPreset { name, params }
}

/// The bank, `Default` first.
pub fn factory_presets() -> Vec<FactoryPreset> {
    use crate::{Band, Feel, Rate};
    vec![
        FactoryPreset {
            name: "Default",
            params: default_params(),
        },
        ducker("Gentle Glue", &DETECTOR_PRESETS[1]),
        ducker("Hard Pump", &DETECTOR_PRESETS[2]),
        ducker("Vocal Duck", &DETECTOR_PRESETS[3]),
        {
            let mut preset = ducker("Bass Under Kick", &DETECTOR_PRESETS[0]);
            preset.params.band = Band::Low;
            preset.params.xover_hz = 180.0;
            preset
        },
        shaper("Quarter Pump", &SHAPE_PRESETS[0], |_| {}),
        shaper("Smooth Swell", &SHAPE_PRESETS[1], |p| p.rate = Rate::Half),
        shaper("Hard Chop", &SHAPE_PRESETS[2], |p| p.rate = Rate::Eighth),
        shaper("Half-Bar Gate", &SHAPE_PRESETS[3], |p| p.rate = Rate::Half),
        shaper("Dotted Bounce", &SHAPE_PRESETS[4], |p| {
            p.feel = Feel::Dotted
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
                crate::ipc::ui_values(&a.params),
                "{} survives a sanitize",
                a.name
            );
            for b in &bank[i + 1..] {
                assert_ne!(a.name, b.name);
            }
        }
    }

    #[test]
    fn the_defaults_are_kick_duck_and_pump() {
        let params = default_params();
        assert!(params.has_detector(&DETECTOR_PRESETS[0]));
        assert!(params.has_shape(&SHAPE_PRESETS[0]));
        for shape in &SHAPE_PRESETS {
            assert_eq!(shape.points.first().map(|p| p.0), Some(0.0));
            assert_eq!(shape.points.last().map(|p| p.0), Some(1.0));
            assert!(shape.points.windows(2).all(|w| w[0].0 <= w[1].0));
        }
    }
}

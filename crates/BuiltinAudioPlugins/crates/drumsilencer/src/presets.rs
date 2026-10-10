//! Drum Silencer's factory presets.
//!
//! Each is a whole `Params` with power on; an editor loads one by sending the
//! wire values that differ, so a preset saves and undoes like any other edit.
//! The range presets are honest about what they are: a frequency window on the
//! one drum estimate, not a per-drum model — "Keep Kick" leaves everything
//! under the range alone, bass included.

use crate::{Mode, Params, default_params};

pub struct FactoryPreset {
    pub name: &'static str,
    pub params: Params,
}

/// The bank, `Default` first.
pub fn factory_presets() -> Vec<FactoryPreset> {
    let preset = |name, params| FactoryPreset { name, params };
    vec![
        preset("Default", default_params()),
        preset(
            "Quieter Drums",
            Params {
                amount: 60.0,
                ..default_params()
            },
        ),
        preset(
            "Keep Kick",
            Params {
                low_hz: 150.0,
                ..default_params()
            },
        ),
        preset(
            "Cymbals Out",
            Params {
                low_hz: 2_000.0,
                ..default_params()
            },
        ),
        preset(
            "Drums Only",
            Params {
                mode: Mode::Solo,
                ..default_params()
            },
        ),
    ]
}

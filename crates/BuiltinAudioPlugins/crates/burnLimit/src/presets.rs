//! BurnLimit's factory presets, ported from the retired React editor.
//!
//! Each is a whole `Params` with power on; an editor loads one by sending the
//! wire values that differ, so a preset saves and undoes like any other edit.

use crate::{Params, Style, default_params};

pub struct FactoryPreset {
    pub name: &'static str,
    pub params: Params,
}

fn preset(
    name: &'static str,
    style: Style,
    gain_db: f32,
    ceiling_db: f32,
    release_ms: f32,
    lookahead_ms: f32,
    edit: impl FnOnce(&mut Params),
) -> FactoryPreset {
    let mut params = Params {
        style,
        gain_db,
        ceiling_db,
        release_ms,
        lookahead_ms,
        ..default_params()
    };
    edit(&mut params);
    FactoryPreset { name, params }
}

/// The bank, `Default` first.
pub fn factory_presets() -> Vec<FactoryPreset> {
    use Style::*;
    vec![
        FactoryPreset {
            name: "Default",
            params: default_params(),
        },
        preset("Master Soft", Clean, 3.0, -0.5, 350.0, 4.0, |_| {}),
        preset("Loud Punch", Punch, 8.0, -0.3, 80.0, 1.5, |_| {}),
        preset("Broadcast Safe", Modern, 4.0, -1.0, 180.0, 5.0, |_| {}),
        preset("Clip Heat", Clip, 12.0, -0.1, 40.0, 0.5, |p| {
            p.true_peak = false
        }),
        preset("Parallel Glue", Punch, 10.0, -0.5, 120.0, 2.0, |p| {
            p.mix = 45.0
        }),
    ]
}

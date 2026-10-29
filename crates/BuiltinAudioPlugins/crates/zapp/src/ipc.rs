//! Stable editor/DSP parameter wire contract for Zapp.
//!
//! String ids live at the UI/control boundary only. The native host resolves
//! them to compact indices before publishing edits to an audio-side bounded
//! queue; [`Dsp::apply_wire_param`](crate::Dsp::apply_wire_param) consumes the
//! numeric form without allocation, serialization, locking, or string lookup.

use serde::{Deserialize, Serialize};

use crate::{Character, Oversampling, Params, StereoMode, clamp, default_params};

pub const PROTOCOL_VERSION: u32 = 1;
pub const STATE_VERSION: u32 = 1;

pub const POWER_INDEX: u32 = 0;
pub const CHARACTER_INDEX: u32 = 1;
pub const DRIVE_INDEX: u32 = 2;
pub const BIAS_INDEX: u32 = 3;
pub const TONE_INDEX: u32 = 4;
pub const LOW_CUT_INDEX: u32 = 5;
pub const HIGH_CUT_INDEX: u32 = 6;
pub const MIX_INDEX: u32 = 7;
pub const OUTPUT_INDEX: u32 = 8;
pub const STEREO_MODE_INDEX: u32 = 9;
pub const OVERSAMPLING_INDEX: u32 = 10;
pub const AUTO_GAIN_INDEX: u32 = 11;

pub const PARAM_COUNT: usize = 12;

pub const UI_PARAM_IDS: [&str; PARAM_COUNT] = [
    "power",
    "character",
    "driveDb",
    "bias",
    "toneDb",
    "lowCutHz",
    "highCutHz",
    "mix",
    "outputDb",
    "stereoMode",
    "oversampling",
    "autoGain",
];

/// Continuous ranges, by wire index; `(0, 0)` for switches and choices.
pub const RANGES: [(f32, f32); PARAM_COUNT] = [
    (0.0, 0.0),          // power
    (0.0, 0.0),          // character
    (0.0, 30.0),         // driveDb
    (-1.0, 1.0),         // bias
    (-12.0, 12.0),       // toneDb
    (20.0, 1_000.0),     // lowCutHz
    (1_000.0, 20_000.0), // highCutHz
    (0.0, 100.0),        // mix
    (-18.0, 18.0),       // outputDb
    (0.0, 0.0),          // stereoMode
    (0.0, 0.0),          // oversampling
    (0.0, 0.0),          // autoGain
];

#[inline]
fn clamp_wire(index: u32, value: f32) -> f32 {
    let (min, max) = RANGES[index as usize];
    clamp(value, min, max)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ZappState {
    pub version: u32,
    pub params: Params,
}

impl ZappState {
    pub fn new(params: Params) -> Self {
        Self {
            version: STATE_VERSION,
            params,
        }
    }

    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        let mut state: Self = serde_json::from_str(json)?;
        sanitize_params(&mut state.params);
        Ok(state)
    }
}

impl Default for ZappState {
    fn default() -> Self {
        Self::new(default_params())
    }
}

pub fn ui_param_index(id: &str) -> Option<u32> {
    UI_PARAM_IDS
        .iter()
        .position(|candidate| *candidate == id)
        .map(|index| index as u32)
}

pub fn ui_param_id(index: u32) -> Option<&'static str> {
    UI_PARAM_IDS.get(index as usize).copied()
}

/// Clamps every continuous value into its range; a non-finite one returns to
/// its default.
pub fn sanitize_params(params: &mut Params) {
    let defaults = default_params();
    let fix = |index: u32, value: f32, default: f32| {
        if value.is_finite() {
            clamp_wire(index, value)
        } else {
            default
        }
    };
    params.drive_db = fix(DRIVE_INDEX, params.drive_db, defaults.drive_db);
    params.bias = fix(BIAS_INDEX, params.bias, defaults.bias);
    params.tone_db = fix(TONE_INDEX, params.tone_db, defaults.tone_db);
    params.low_cut_hz = fix(LOW_CUT_INDEX, params.low_cut_hz, defaults.low_cut_hz);
    params.high_cut_hz = fix(HIGH_CUT_INDEX, params.high_cut_hz, defaults.high_cut_hz);
    params.mix = fix(MIX_INDEX, params.mix, defaults.mix);
    params.output_db = fix(OUTPUT_INDEX, params.output_db, defaults.output_db);
}

pub fn apply_wire_param(params: &mut Params, index: u32, value: f32) -> bool {
    if !value.is_finite() || index as usize >= PARAM_COUNT {
        return false;
    }
    match index {
        POWER_INDEX => params.power = value >= 0.5,
        CHARACTER_INDEX => params.character = Character::from_wire(value),
        DRIVE_INDEX => params.drive_db = clamp_wire(index, value),
        BIAS_INDEX => params.bias = clamp_wire(index, value),
        TONE_INDEX => params.tone_db = clamp_wire(index, value),
        LOW_CUT_INDEX => params.low_cut_hz = clamp_wire(index, value),
        HIGH_CUT_INDEX => params.high_cut_hz = clamp_wire(index, value),
        MIX_INDEX => params.mix = clamp_wire(index, value),
        OUTPUT_INDEX => params.output_db = clamp_wire(index, value),
        STEREO_MODE_INDEX => params.stereo_mode = StereoMode::from_wire(value),
        OVERSAMPLING_INDEX => params.oversampling = Oversampling::from_wire(value),
        AUTO_GAIN_INDEX => params.auto_gain = value >= 0.5,
        _ => return false,
    }
    true
}

pub fn apply_ui_param(params: &mut Params, id: &str, value: f32) -> bool {
    let Some(index) = ui_param_index(id) else {
        return false;
    };
    apply_wire_param(params, index, value)
}

pub fn ui_values(params: &Params) -> Vec<(&'static str, f32)> {
    vec![
        ("power", f32::from(params.power)),
        ("character", params.character.to_wire()),
        ("driveDb", params.drive_db),
        ("bias", params.bias),
        ("toneDb", params.tone_db),
        ("lowCutHz", params.low_cut_hz),
        ("highCutHz", params.high_cut_hz),
        ("mix", params.mix),
        ("outputDb", params.output_db),
        ("stereoMode", params.stereo_mode.to_wire()),
        ("oversampling", params.oversampling.to_wire()),
        ("autoGain", f32::from(params.auto_gain)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip_to_wire_indices() {
        assert_eq!(UI_PARAM_IDS.len(), PARAM_COUNT);
        for (index, id) in UI_PARAM_IDS.iter().enumerate() {
            assert_eq!(ui_param_index(id), Some(index as u32));
            assert_eq!(ui_param_id(index as u32), Some(*id));
        }
        assert_eq!(ui_param_id(PARAM_COUNT as u32), None);
    }

    #[test]
    fn ui_values_covers_every_id_in_wire_order() {
        let values = ui_values(&default_params());
        assert_eq!(values.len(), PARAM_COUNT);
        for (index, (id, _)) in values.iter().enumerate() {
            assert_eq!(*id, UI_PARAM_IDS[index]);
        }
    }

    #[test]
    fn every_value_survives_the_wire() {
        let mut source = default_params();
        source.power = false;
        source.character = Character::Diode;
        source.drive_db = 21.5;
        source.bias = -0.4;
        source.tone_db = 3.0;
        source.low_cut_hz = 120.0;
        source.high_cut_hz = 9_000.0;
        source.mix = 60.0;
        source.output_db = -4.0;
        source.stereo_mode = StereoMode::Side;
        source.oversampling = Oversampling::X8;
        source.auto_gain = false;
        let mut copy = default_params();
        for (index, (_, value)) in ui_values(&source).into_iter().enumerate() {
            assert!(apply_wire_param(&mut copy, index as u32, value));
        }
        assert_eq!(ui_values(&copy), ui_values(&source));
    }

    #[test]
    fn state_round_trips() {
        let mut params = default_params();
        assert!(apply_ui_param(&mut params, "driveDb", 18.0));
        assert!(apply_ui_param(
            &mut params,
            "character",
            Character::Fold.to_wire()
        ));
        let json = ZappState::new(params).to_json().unwrap();
        let decoded = ZappState::from_json(&json).unwrap();
        assert_eq!(decoded.params.drive_db, 18.0);
        assert_eq!(decoded.params.character, Character::Fold);
        assert_eq!(decoded.version, STATE_VERSION);
    }

    #[test]
    fn state_missing_fields_loads_with_defaults() {
        let decoded =
            ZappState::from_json(r#"{"version":1,"params":{"driveDb":40,"character":"tube"}}"#)
                .unwrap();
        let defaults = default_params();
        assert_eq!(decoded.params.drive_db, 30.0, "clamped on load");
        assert_eq!(decoded.params.character, Character::Tube);
        assert_eq!(decoded.params.mix, defaults.mix);
        assert_eq!(decoded.params.oversampling, defaults.oversampling);
        assert_eq!(decoded.params.auto_gain, defaults.auto_gain);
        assert!(decoded.params.power);
    }

    #[test]
    fn invalid_and_non_finite_updates_are_rejected() {
        let mut params = default_params();
        assert!(!apply_wire_param(&mut params, u32::MAX, 1.0));
        assert!(!apply_wire_param(&mut params, PARAM_COUNT as u32, 1.0));
        assert!(!apply_wire_param(&mut params, DRIVE_INDEX, f32::NAN));
        assert!(!apply_ui_param(&mut params, "notAParam", 1.0));
        assert!(apply_ui_param(&mut params, "lowCutHz", 5.0));
        assert_eq!(params.low_cut_hz, 20.0);
    }
}

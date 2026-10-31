//! Stable editor/DSP parameter wire contract for AirBoost.
//!
//! String ids live at the UI/control boundary only. The native host resolves
//! them to compact indices before publishing edits to an audio-side bounded
//! queue; [`Dsp::apply_wire_param`](crate::Dsp::apply_wire_param) consumes the
//! numeric form without allocation, serialization, locking, or string lookup.

use serde::{Deserialize, Serialize};

use crate::{AirFreq, Params, Shape, StereoMode, default_params};
use builtin_dsp_core::clamp;

pub const PROTOCOL_VERSION: u32 = 1;
pub const STATE_VERSION: u32 = 1;

pub const POWER_INDEX: u32 = 0;
pub const AIR_DB_INDEX: u32 = 1;
pub const AIR_FREQ_INDEX: u32 = 2;
pub const SHAPE_INDEX: u32 = 3;
pub const PRESENCE_INDEX: u32 = 4;
pub const EXCITE_INDEX: u32 = 5;
pub const DEESS_INDEX: u32 = 6;
pub const STEREO_MODE_INDEX: u32 = 7;
pub const MIX_INDEX: u32 = 8;
pub const OUTPUT_INDEX: u32 = 9;
pub const AUTO_GAIN_INDEX: u32 = 10;

pub const PARAM_COUNT: usize = 11;

pub const UI_PARAM_IDS: [&str; PARAM_COUNT] = [
    "power",
    "airDb",
    "airFreq",
    "shape",
    "presenceDb",
    "excite",
    "deess",
    "stereoMode",
    "mix",
    "outputDb",
    "autoGain",
];

/// Continuous ranges, by wire index; `(0, 0)` for switches and choices.
pub const RANGES: [(f32, f32); PARAM_COUNT] = [
    (0.0, 0.0),    // power
    (0.0, 12.0),   // airDb
    (0.0, 0.0),    // airFreq
    (0.0, 0.0),    // shape
    (-6.0, 6.0),   // presenceDb
    (0.0, 100.0),  // excite
    (0.0, 100.0),  // deess
    (0.0, 0.0),    // stereoMode
    (0.0, 100.0),  // mix
    (-12.0, 12.0), // outputDb
    (0.0, 0.0),    // autoGain
];

#[inline]
fn clamp_wire(index: u32, value: f32) -> f32 {
    let (min, max) = RANGES[index as usize];
    clamp(value, min, max)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AirBoostState {
    pub version: u32,
    pub params: Params,
}

impl AirBoostState {
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

impl Default for AirBoostState {
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
    params.air_db = fix(AIR_DB_INDEX, params.air_db, defaults.air_db);
    params.presence_db = fix(PRESENCE_INDEX, params.presence_db, defaults.presence_db);
    params.excite = fix(EXCITE_INDEX, params.excite, defaults.excite);
    params.deess = fix(DEESS_INDEX, params.deess, defaults.deess);
    params.mix = fix(MIX_INDEX, params.mix, defaults.mix);
    params.output_db = fix(OUTPUT_INDEX, params.output_db, defaults.output_db);
}

pub fn apply_wire_param(params: &mut Params, index: u32, value: f32) -> bool {
    if !value.is_finite() || index as usize >= PARAM_COUNT {
        return false;
    }
    match index {
        POWER_INDEX => params.power = value >= 0.5,
        AIR_DB_INDEX => params.air_db = clamp_wire(index, value),
        AIR_FREQ_INDEX => params.air_freq = AirFreq::from_wire(value),
        SHAPE_INDEX => params.shape = Shape::from_wire(value),
        PRESENCE_INDEX => params.presence_db = clamp_wire(index, value),
        EXCITE_INDEX => params.excite = clamp_wire(index, value),
        DEESS_INDEX => params.deess = clamp_wire(index, value),
        STEREO_MODE_INDEX => params.stereo_mode = StereoMode::from_wire(value),
        MIX_INDEX => params.mix = clamp_wire(index, value),
        OUTPUT_INDEX => params.output_db = clamp_wire(index, value),
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
        ("airDb", params.air_db),
        ("airFreq", params.air_freq.to_wire()),
        ("shape", params.shape.to_wire()),
        ("presenceDb", params.presence_db),
        ("excite", params.excite),
        ("deess", params.deess),
        ("stereoMode", params.stereo_mode.to_wire()),
        ("mix", params.mix),
        ("outputDb", params.output_db),
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
        source.air_db = 7.5;
        source.air_freq = AirFreq::K40;
        source.shape = Shape::Bell;
        source.presence_db = -2.5;
        source.excite = 61.0;
        source.deess = 12.0;
        source.stereo_mode = StereoMode::Side;
        source.mix = 40.0;
        source.output_db = -3.0;
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
        assert!(apply_ui_param(&mut params, "airDb", 9.0));
        assert!(apply_ui_param(
            &mut params,
            "airFreq",
            AirFreq::K2_5.to_wire()
        ));
        let json = AirBoostState::new(params).to_json().unwrap();
        assert!(json.contains("\"airFreq\":\"2.5k\""), "{json}");
        let decoded = AirBoostState::from_json(&json).unwrap();
        assert_eq!(decoded.params.air_db, 9.0);
        assert_eq!(decoded.params.air_freq, AirFreq::K2_5);
        assert_eq!(decoded.version, STATE_VERSION);
    }

    #[test]
    fn state_missing_fields_loads_with_defaults() {
        let decoded = AirBoostState::from_json(
            r#"{"version":1,"params":{"airDb":40,"shape":"bell","airFreq":"5k"}}"#,
        )
        .unwrap();
        let defaults = default_params();
        assert_eq!(decoded.params.air_db, 12.0, "clamped on load");
        assert_eq!(decoded.params.shape, Shape::Bell);
        assert_eq!(decoded.params.air_freq, AirFreq::K5);
        assert_eq!(decoded.params.mix, defaults.mix);
        assert_eq!(decoded.params.excite, defaults.excite);
        assert_eq!(decoded.params.auto_gain, defaults.auto_gain);
        assert!(decoded.params.power);
    }

    #[test]
    fn invalid_and_non_finite_updates_are_rejected() {
        let mut params = default_params();
        assert!(!apply_wire_param(&mut params, u32::MAX, 1.0));
        assert!(!apply_wire_param(&mut params, PARAM_COUNT as u32, 1.0));
        assert!(!apply_wire_param(&mut params, AIR_DB_INDEX, f32::NAN));
        assert!(!apply_wire_param(&mut params, MIX_INDEX, f32::INFINITY));
        assert!(!apply_ui_param(&mut params, "notAParam", 1.0));
        assert!(apply_ui_param(&mut params, "presenceDb", -50.0));
        assert_eq!(params.presence_db, -6.0);
    }

    #[test]
    fn sanitize_puts_non_finite_values_back_to_their_defaults() {
        let mut params = default_params();
        params.excite = f32::NAN;
        params.output_db = 99.0;
        sanitize_params(&mut params);
        assert_eq!(params.excite, default_params().excite);
        assert_eq!(params.output_db, 12.0);
    }
}

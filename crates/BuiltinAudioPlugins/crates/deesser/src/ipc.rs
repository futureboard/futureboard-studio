//! Stable editor/DSP parameter wire contract for the De-Esser.
//!
//! String ids live at the UI/control boundary only. The native host resolves
//! them to compact indices before publishing edits to an audio-side bounded
//! queue; [`Dsp::apply_wire_param`](crate::Dsp::apply_wire_param) consumes the
//! numeric form without allocation, serialization, locking, or string lookup.

use serde::{Deserialize, Serialize};

use crate::{Mode, Params, StereoMode, default_params};

pub const PROTOCOL_VERSION: u32 = 1;
pub const STATE_VERSION: u32 = 1;

pub const POWER_INDEX: u32 = 0;
pub const MODE_INDEX: u32 = 1;
pub const STEREO_MODE_INDEX: u32 = 2;
pub const FREQ_INDEX: u32 = 3;
pub const WIDTH_INDEX: u32 = 4;
pub const THRESHOLD_INDEX: u32 = 5;
pub const RANGE_INDEX: u32 = 6;
pub const RELEASE_INDEX: u32 = 7;
pub const LOOKAHEAD_INDEX: u32 = 8;
pub const MIX_INDEX: u32 = 9;
pub const OUTPUT_INDEX: u32 = 10;
pub const LISTEN_INDEX: u32 = 11;

pub const PARAM_COUNT: usize = 12;

pub const UI_PARAM_IDS: [&str; PARAM_COUNT] = [
    "power",
    "mode",
    "stereoMode",
    "freqHz",
    "widthOct",
    "thresholdDb",
    "rangeDb",
    "releaseMs",
    "lookaheadMs",
    "mix",
    "outputDb",
    "listen",
];

/// Continuous ranges, by wire index; `(0, 0)` for switches and choices.
pub const RANGES: [(f32, f32); PARAM_COUNT] = [
    (0.0, 0.0),          // power
    (0.0, 0.0),          // mode
    (0.0, 0.0),          // stereoMode
    (2_000.0, 16_000.0), // freqHz
    (0.3, 3.0),          // widthOct
    (-60.0, 0.0),        // thresholdDb
    (0.0, 24.0),         // rangeDb
    (5.0, 300.0),        // releaseMs
    (0.0, 10.0),         // lookaheadMs
    (0.0, 100.0),        // mix
    (-12.0, 12.0),       // outputDb
    (0.0, 0.0),          // listen
];

#[inline]
fn clamp_wire(index: u32, value: f32) -> f32 {
    let (min, max) = RANGES[index as usize];
    value.max(min).min(max)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeEsserState {
    pub version: u32,
    pub params: Params,
}

impl DeEsserState {
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

impl Default for DeEsserState {
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
    params.freq_hz = fix(FREQ_INDEX, params.freq_hz, defaults.freq_hz);
    params.width_oct = fix(WIDTH_INDEX, params.width_oct, defaults.width_oct);
    params.threshold_db = fix(THRESHOLD_INDEX, params.threshold_db, defaults.threshold_db);
    params.range_db = fix(RANGE_INDEX, params.range_db, defaults.range_db);
    params.release_ms = fix(RELEASE_INDEX, params.release_ms, defaults.release_ms);
    params.lookahead_ms = fix(LOOKAHEAD_INDEX, params.lookahead_ms, defaults.lookahead_ms);
    params.mix = fix(MIX_INDEX, params.mix, defaults.mix);
    params.output_db = fix(OUTPUT_INDEX, params.output_db, defaults.output_db);
}

pub fn apply_wire_param(params: &mut Params, index: u32, value: f32) -> bool {
    if !value.is_finite() || index as usize >= PARAM_COUNT {
        return false;
    }
    match index {
        POWER_INDEX => params.power = value >= 0.5,
        MODE_INDEX => params.mode = Mode::from_wire(value),
        STEREO_MODE_INDEX => params.stereo_mode = StereoMode::from_wire(value),
        FREQ_INDEX => params.freq_hz = clamp_wire(index, value),
        WIDTH_INDEX => params.width_oct = clamp_wire(index, value),
        THRESHOLD_INDEX => params.threshold_db = clamp_wire(index, value),
        RANGE_INDEX => params.range_db = clamp_wire(index, value),
        RELEASE_INDEX => params.release_ms = clamp_wire(index, value),
        LOOKAHEAD_INDEX => params.lookahead_ms = clamp_wire(index, value),
        MIX_INDEX => params.mix = clamp_wire(index, value),
        OUTPUT_INDEX => params.output_db = clamp_wire(index, value),
        LISTEN_INDEX => params.listen = value >= 0.5,
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
        ("mode", params.mode.to_wire()),
        ("stereoMode", params.stereo_mode.to_wire()),
        ("freqHz", params.freq_hz),
        ("widthOct", params.width_oct),
        ("thresholdDb", params.threshold_db),
        ("rangeDb", params.range_db),
        ("releaseMs", params.release_ms),
        ("lookaheadMs", params.lookahead_ms),
        ("mix", params.mix),
        ("outputDb", params.output_db),
        ("listen", f32::from(params.listen)),
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
        let source = Params {
            power: false,
            mode: Mode::Wide,
            stereo_mode: StereoMode::Side,
            freq_hz: 5_500.0,
            width_oct: 2.0,
            threshold_db: -42.0,
            range_db: 18.0,
            release_ms: 120.0,
            lookahead_ms: 4.5,
            mix: 70.0,
            output_db: -3.0,
            listen: true,
        };
        let mut copy = default_params();
        for (index, (_, value)) in ui_values(&source).into_iter().enumerate() {
            assert!(apply_wire_param(&mut copy, index as u32, value));
        }
        assert_eq!(copy, source);
    }

    #[test]
    fn state_round_trips() {
        let mut params = default_params();
        assert!(apply_ui_param(&mut params, "freqHz", 5_500.0));
        assert!(apply_ui_param(&mut params, "mode", Mode::Wide.to_wire()));
        let json = DeEsserState::new(params.clone()).to_json().unwrap();
        let decoded = DeEsserState::from_json(&json).unwrap();
        assert_eq!(decoded.params, params);
        assert_eq!(decoded.version, STATE_VERSION);
    }

    #[test]
    fn state_missing_fields_loads_with_defaults() {
        let decoded =
            DeEsserState::from_json(r#"{"version":1,"params":{"freqHz":40000,"mode":"wide"}}"#)
                .unwrap();
        let defaults = default_params();
        assert_eq!(decoded.params.freq_hz, 16_000.0, "clamped on load");
        assert_eq!(decoded.params.mode, Mode::Wide);
        assert_eq!(decoded.params.range_db, defaults.range_db);
        assert_eq!(decoded.params.stereo_mode, defaults.stereo_mode);
        assert!(decoded.params.power);
        assert!(!decoded.params.listen);
    }

    #[test]
    fn invalid_and_non_finite_updates_are_rejected() {
        let mut params = default_params();
        assert!(!apply_wire_param(&mut params, u32::MAX, 1.0));
        assert!(!apply_wire_param(&mut params, PARAM_COUNT as u32, 1.0));
        assert!(!apply_wire_param(&mut params, FREQ_INDEX, f32::NAN));
        assert!(!apply_ui_param(&mut params, "notAParam", 1.0));
        assert!(apply_ui_param(&mut params, "rangeDb", 99.0));
        assert_eq!(params.range_db, 24.0);
        let mut broken = default_params();
        broken.threshold_db = f32::INFINITY;
        broken.width_oct = 0.0;
        sanitize_params(&mut broken);
        assert_eq!(broken.threshold_db, default_params().threshold_db);
        assert_eq!(broken.width_oct, 0.3);
    }
}

//! Stable editor/DSP parameter wire contract for Drum Silencer.
//!
//! String ids live at the UI/control boundary only. The native host resolves
//! them to compact indices before publishing edits to an audio-side bounded
//! queue; [`Dsp::apply_wire_param`](crate::Dsp::apply_wire_param) consumes the
//! numeric form without allocation, serialization, locking, or string lookup.

use serde::{Deserialize, Serialize};

use crate::{Mode, Params, clamp, default_params};

pub const PROTOCOL_VERSION: u32 = 1;
pub const STATE_VERSION: u32 = 1;

pub const POWER_INDEX: u32 = 0;
pub const MODE_INDEX: u32 = 1;
pub const AMOUNT_INDEX: u32 = 2;
pub const LOW_INDEX: u32 = 3;
pub const HIGH_INDEX: u32 = 4;
pub const OUTPUT_INDEX: u32 = 5;

pub const PARAM_COUNT: usize = 6;

/// Wire index *is* the position in this table; the editor and the host both
/// resolve through it, so the order is part of the persisted contract. Append
/// only.
pub const UI_PARAM_IDS: [&str; PARAM_COUNT] =
    ["power", "mode", "amount", "lowHz", "highHz", "outputDb"];

/// Inclusive `(min, max)` for every continuous parameter, indexed by wire
/// index. Booleans and the mode enum carry `(0, 0)`.
pub(crate) const RANGES: [(f32, f32); PARAM_COUNT] = [
    (0.0, 0.0),           // power
    (0.0, 0.0),           // mode
    (0.0, 100.0),         // amount, %
    (20.0, 2_000.0),      // lowHz (20 = off)
    (1_000.0, 20_000.0),  // highHz (20 k = off)
    (-24.0, 12.0),        // outputDb
];

#[inline]
fn clamp_wire(index: u32, value: f32) -> f32 {
    let (min, max) = RANGES[index as usize];
    clamp(value, min, max)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DrumSilencerState {
    pub version: u32,
    pub params: Params,
}

impl DrumSilencerState {
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
        serde_json::from_str(json)
    }
}

impl Default for DrumSilencerState {
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

/// Clamp a whole `Params` into range. Used after deserializing a project blob.
pub fn sanitize_params(params: &mut Params) {
    params.amount = clamp_wire(AMOUNT_INDEX, params.amount);
    params.low_hz = clamp_wire(LOW_INDEX, params.low_hz);
    params.high_hz = clamp_wire(HIGH_INDEX, params.high_hz);
    params.output_db = clamp_wire(OUTPUT_INDEX, params.output_db);
}

/// Apply one compact UI/control update. Allocation-free and total: invalid
/// indices and non-finite values are rejected, the rest clamped.
pub fn apply_wire_param(params: &mut Params, index: u32, value: f32) -> bool {
    if !value.is_finite() || index as usize >= PARAM_COUNT {
        return false;
    }
    match index {
        POWER_INDEX => params.power = value >= 0.5,
        MODE_INDEX => params.mode = Mode::from_wire(value),
        AMOUNT_INDEX => params.amount = clamp_wire(index, value),
        LOW_INDEX => params.low_hz = clamp_wire(index, value),
        HIGH_INDEX => params.high_hz = clamp_wire(index, value),
        OUTPUT_INDEX => params.output_db = clamp_wire(index, value),
        _ => return false,
    }
    true
}

/// Resolve a string id off the realtime path and apply it to a state mirror.
pub fn apply_ui_param(params: &mut Params, id: &str, value: f32) -> bool {
    let Some(index) = ui_param_index(id) else {
        return false;
    };
    apply_wire_param(params, index, value)
}

/// Every parameter as `(id, raw value)`, in wire order.
pub fn ui_values(params: &Params) -> Vec<(&'static str, f32)> {
    vec![
        ("power", f32::from(params.power)),
        ("mode", params.mode.to_wire()),
        ("amount", params.amount),
        ("lowHz", params.low_hz),
        ("highHz", params.high_hz),
        ("outputDb", params.output_db),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip_to_wire_indices() {
        for (index, id) in UI_PARAM_IDS.iter().enumerate() {
            assert_eq!(ui_param_index(id), Some(index as u32));
            assert_eq!(ui_param_id(index as u32), Some(*id));
        }
        let values = ui_values(&default_params());
        assert_eq!(values.len(), PARAM_COUNT);
        for (index, (id, _)) in values.iter().enumerate() {
            assert_eq!(*id, UI_PARAM_IDS[index]);
        }
    }

    #[test]
    fn state_round_trips() {
        let mut params = default_params();
        assert!(apply_ui_param(&mut params, "amount", 40.0));
        assert!(apply_ui_param(&mut params, "mode", 1.0));
        assert!(apply_ui_param(&mut params, "lowHz", 150.0));
        let json = DrumSilencerState::new(params).to_json().unwrap();
        let decoded = DrumSilencerState::from_json(&json).unwrap();
        assert_eq!(decoded.params.amount, 40.0);
        assert_eq!(decoded.params.mode, Mode::Solo);
        assert_eq!(decoded.params.low_hz, 150.0);
        assert_eq!(decoded.version, STATE_VERSION);
    }

    #[test]
    fn out_of_range_is_clamped_and_garbage_rejected() {
        let mut params = default_params();
        assert!(apply_ui_param(&mut params, "amount", 400.0));
        assert_eq!(params.amount, 100.0);
        assert!(apply_ui_param(&mut params, "highHz", 10.0));
        assert_eq!(params.high_hz, 1_000.0);
        assert!(!apply_wire_param(&mut params, PARAM_COUNT as u32, 1.0));
        assert!(!apply_wire_param(&mut params, AMOUNT_INDEX, f32::NAN));
        assert!(!apply_ui_param(&mut params, "threshold", 1.0));
        params.output_db = 99.0;
        params.low_hz = -3.0;
        sanitize_params(&mut params);
        assert_eq!((params.output_db, params.low_hz), (12.0, 20.0));
    }
}

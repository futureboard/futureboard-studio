//! Stable editor/DSP parameter wire contract for WayGate.
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
pub const THRESHOLD_INDEX: u32 = 2;
pub const RANGE_INDEX: u32 = 3;
pub const ATTACK_INDEX: u32 = 4;
pub const HOLD_INDEX: u32 = 5;
pub const RELEASE_INDEX: u32 = 6;
pub const HYSTERESIS_INDEX: u32 = 7;
pub const KEY_HPF_INDEX: u32 = 8;
pub const KEY_LPF_INDEX: u32 = 9;
pub const KEY_LISTEN_INDEX: u32 = 10;
pub const LOOKAHEAD_INDEX: u32 = 11;
pub const STEREO_LINK_INDEX: u32 = 12;

pub const PARAM_COUNT: usize = 13;

/// Wire index *is* the position in this table; the editor and the host both
/// resolve through it, so the order is part of the persisted contract. Append
/// only.
pub const UI_PARAM_IDS: [&str; PARAM_COUNT] = [
    "power",
    "mode",
    "thresholdDb",
    "rangeDb",
    "attackMs",
    "holdMs",
    "releaseMs",
    "hysteresisDb",
    "keyHpfHz",
    "keyLpfHz",
    "keyListen",
    "lookaheadMs",
    "stereoLink",
];

/// Inclusive `(min, max)` for every continuous parameter, indexed by wire
/// index. Booleans and the mode enum carry `(0, 0)` and are handled by their
/// own arms in [`apply_wire_param`]. Single source of truth for clamping, so
/// [`sanitize_params`] and the wire path cannot drift apart — and the
/// descriptor's ranges are checked against it in the tests.
pub(crate) const RANGES: [(f32, f32); PARAM_COUNT] = [
    (0.0, 0.0),        // power
    (0.0, 0.0),        // mode
    (-80.0, 0.0),      // thresholdDb
    (-80.0, 0.0),      // rangeDb (−80 = −∞, a full mute)
    (0.01, 100.0),     // attackMs
    (0.0, 2_000.0),    // holdMs
    (5.0, 4_000.0),    // releaseMs
    (0.0, 20.0),       // hysteresisDb
    (20.0, 4_000.0),   // keyHpfHz (20 = off)
    (100.0, 20_000.0), // keyLpfHz (20 k = off)
    (0.0, 0.0),        // keyListen
    (0.0, 10.0),       // lookaheadMs
    (0.0, 0.0),        // stereoLink
];

#[inline]
fn clamp_wire(index: u32, value: f32) -> f32 {
    let (min, max) = RANGES[index as usize];
    clamp(value, min, max)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WayGateState {
    pub version: u32,
    pub params: Params,
}

impl WayGateState {
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

impl Default for WayGateState {
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

/// Clamp a whole `Params` into range. Used after deserializing a project blob,
/// which may predate a range change or have been edited by hand.
pub fn sanitize_params(params: &mut Params) {
    params.threshold_db = clamp_wire(THRESHOLD_INDEX, params.threshold_db);
    params.range_db = clamp_wire(RANGE_INDEX, params.range_db);
    params.attack_ms = clamp_wire(ATTACK_INDEX, params.attack_ms);
    params.hold_ms = clamp_wire(HOLD_INDEX, params.hold_ms);
    params.release_ms = clamp_wire(RELEASE_INDEX, params.release_ms);
    params.hysteresis_db = clamp_wire(HYSTERESIS_INDEX, params.hysteresis_db);
    params.key_hpf_hz = clamp_wire(KEY_HPF_INDEX, params.key_hpf_hz);
    params.key_lpf_hz = clamp_wire(KEY_LPF_INDEX, params.key_lpf_hz);
    params.lookahead_ms = clamp_wire(LOOKAHEAD_INDEX, params.lookahead_ms);
}

/// Apply one compact UI/control update. Allocation-free and total: invalid
/// indices are rejected, non-finite values are rejected, and every continuous
/// value is clamped to its declared range.
pub fn apply_wire_param(params: &mut Params, index: u32, value: f32) -> bool {
    if !value.is_finite() || index as usize >= PARAM_COUNT {
        return false;
    }
    match index {
        POWER_INDEX => params.power = value >= 0.5,
        MODE_INDEX => params.mode = Mode::from_wire(value),
        THRESHOLD_INDEX => params.threshold_db = clamp_wire(index, value),
        RANGE_INDEX => params.range_db = clamp_wire(index, value),
        ATTACK_INDEX => params.attack_ms = clamp_wire(index, value),
        HOLD_INDEX => params.hold_ms = clamp_wire(index, value),
        RELEASE_INDEX => params.release_ms = clamp_wire(index, value),
        HYSTERESIS_INDEX => params.hysteresis_db = clamp_wire(index, value),
        KEY_HPF_INDEX => params.key_hpf_hz = clamp_wire(index, value),
        KEY_LPF_INDEX => params.key_lpf_hz = clamp_wire(index, value),
        KEY_LISTEN_INDEX => params.key_listen = value >= 0.5,
        LOOKAHEAD_INDEX => params.lookahead_ms = clamp_wire(index, value),
        STEREO_LINK_INDEX => params.stereo_link = value >= 0.5,
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

/// Every parameter as `(id, raw value)`, in wire order. Drives project replay
/// and the descriptor-vs-defaults check.
pub fn ui_values(params: &Params) -> Vec<(&'static str, f32)> {
    vec![
        ("power", f32::from(params.power)),
        ("mode", params.mode.to_wire()),
        ("thresholdDb", params.threshold_db),
        ("rangeDb", params.range_db),
        ("attackMs", params.attack_ms),
        ("holdMs", params.hold_ms),
        ("releaseMs", params.release_ms),
        ("hysteresisDb", params.hysteresis_db),
        ("keyHpfHz", params.key_hpf_hz),
        ("keyLpfHz", params.key_lpf_hz),
        ("keyListen", f32::from(params.key_listen)),
        ("lookaheadMs", params.lookahead_ms),
        ("stereoLink", f32::from(params.stereo_link)),
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
    fn state_round_trips() {
        let mut params = default_params();
        assert!(apply_ui_param(&mut params, "thresholdDb", -32.0));
        assert!(apply_ui_param(&mut params, "mode", 1.0));
        assert!(apply_ui_param(&mut params, "keyHpfHz", 120.0));
        let json = WayGateState::new(params).to_json().unwrap();
        let decoded = WayGateState::from_json(&json).unwrap();
        assert_eq!(decoded.params.threshold_db, -32.0);
        assert_eq!(decoded.params.mode, Mode::Duck);
        assert_eq!(decoded.params.key_hpf_hz, 120.0);
        assert_eq!(decoded.version, STATE_VERSION);
    }

    #[test]
    fn out_of_range_values_are_clamped_not_rejected() {
        let mut params = default_params();
        assert!(apply_ui_param(&mut params, "thresholdDb", 12.0));
        assert_eq!(params.threshold_db, 0.0);
        assert!(apply_ui_param(&mut params, "rangeDb", -200.0));
        assert_eq!(params.range_db, -80.0);
        assert!(apply_ui_param(&mut params, "attackMs", 0.0));
        assert_eq!(params.attack_ms, 0.01);
        assert!(apply_ui_param(&mut params, "lookaheadMs", 50.0));
        assert_eq!(params.lookahead_ms, 10.0);
        assert!(apply_ui_param(&mut params, "mode", 7.0));
        assert_eq!(params.mode, Mode::Duck);
    }

    #[test]
    fn invalid_and_non_finite_updates_are_rejected() {
        let mut params = default_params();
        assert!(!apply_wire_param(&mut params, u32::MAX, 1.0));
        assert!(!apply_wire_param(&mut params, PARAM_COUNT as u32, 1.0));
        assert!(!apply_wire_param(&mut params, THRESHOLD_INDEX, f32::NAN));
        assert!(!apply_wire_param(&mut params, RELEASE_INDEX, f32::INFINITY));
        assert!(!apply_ui_param(&mut params, "notAParam", 1.0));
    }

    #[test]
    fn sanitize_pulls_a_hand_edited_blob_back_into_range() {
        let mut params = default_params();
        params.threshold_db = 10.0;
        params.range_db = -120.0;
        params.attack_ms = 500.0;
        params.hold_ms = -5.0;
        params.release_ms = 1.0;
        params.hysteresis_db = 40.0;
        params.key_hpf_hz = 5.0;
        params.key_lpf_hz = 30_000.0;
        params.lookahead_ms = 20.0;
        sanitize_params(&mut params);
        assert_eq!(params.threshold_db, 0.0);
        assert_eq!(params.range_db, -80.0);
        assert_eq!(params.attack_ms, 100.0);
        assert_eq!(params.hold_ms, 0.0);
        assert_eq!(params.release_ms, 5.0);
        assert_eq!(params.hysteresis_db, 20.0);
        assert_eq!(params.key_hpf_hz, 20.0);
        assert_eq!(params.key_lpf_hz, 20_000.0);
        assert_eq!(params.lookahead_ms, 10.0);
    }
}

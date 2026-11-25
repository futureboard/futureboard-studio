//! Stable editor/DSP parameter wire contract for Trigger.
//!
//! String ids live at the UI/control boundary only. The native host resolves
//! them to compact indices before publishing edits to an audio-side bounded
//! queue; [`Dsp::apply_wire_param`](crate::Dsp::apply_wire_param) consumes the
//! numeric form without allocation, serialization, locking, or string lookup.
//!
//! The sample's file name is not a wire param (a name is not an `f32`): it is
//! state the project saves, and its audio reaches the DSP as a sample load.

use builtin_dsp_core::clamp;
use serde::{Deserialize, Serialize};

use crate::{
    LENGTH_FULL_MS, MAX_VOICES, Mode, OFFSET_MAX_MS, OFFSET_MIN_MS, Params, default_params,
};

pub const PROTOCOL_VERSION: u32 = 1;
pub const STATE_VERSION: u32 = 1;

pub const POWER_INDEX: u32 = 0;
pub const LISTEN_INDEX: u32 = 1;
pub const HPF_INDEX: u32 = 2;
pub const LPF_INDEX: u32 = 3;
pub const THRESHOLD_INDEX: u32 = 4;
pub const RETRIGGER_INDEX: u32 = 5;
pub const DYNAMICS_INDEX: u32 = 6;
pub const OFFSET_INDEX: u32 = 7;
pub const PITCH_INDEX: u32 = 8;
pub const LENGTH_INDEX: u32 = 9;
pub const FADE_INDEX: u32 = 10;
pub const VOICES_INDEX: u32 = 11;
pub const MODE_INDEX: u32 = 12;
pub const DRY_INDEX: u32 = 13;
pub const SAMPLE_INDEX: u32 = 14;

pub const PARAM_COUNT: usize = 15;

pub const UI_PARAM_IDS: [&str; PARAM_COUNT] = [
    "power",
    "listen",
    "hpfHz",
    "lpfHz",
    "thresholdDb",
    "retriggerMs",
    "dynamics",
    "offsetMs",
    "pitch",
    "lengthMs",
    "fadeMs",
    "voices",
    "mode",
    "dryDb",
    "sampleDb",
];

/// Continuous ranges by wire index; `(0, 0)` for switches and choices.
pub const RANGES: [(f32, f32); PARAM_COUNT] = [
    (0.0, 0.0),                     // power
    (0.0, 0.0),                     // listen
    (20.0, 2_000.0),                // hpfHz
    (100.0, 20_000.0),              // lpfHz
    (-60.0, 0.0),                   // thresholdDb
    (5.0, 500.0),                   // retriggerMs
    (0.0, 100.0),                   // dynamics
    (OFFSET_MIN_MS, OFFSET_MAX_MS), // offsetMs
    (-24.0, 24.0),                  // pitch
    (10.0, LENGTH_FULL_MS),         // lengthMs
    (1.0, 500.0),                   // fadeMs
    (1.0, MAX_VOICES as f32),       // voices
    (0.0, 0.0),                     // mode
    (-48.0, 6.0),                   // dryDb
    (-24.0, 12.0),                  // sampleDb
];

#[inline]
fn clamp_wire(index: u32, value: f32) -> f32 {
    let (min, max) = RANGES[index as usize];
    clamp(value, min, max)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TriggerState {
    pub version: u32,
    pub params: Params,
}

impl TriggerState {
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

impl Default for TriggerState {
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

fn voices(value: f32) -> u32 {
    (value.round() as i64).clamp(1, i64::from(MAX_VOICES)) as u32
}

/// Clamps every continuous value into its range — a non-finite one returns
/// to its default — and drops an empty sample name.
pub fn sanitize_params(params: &mut Params) {
    let defaults = default_params();
    let fix = |index: u32, value: f32, default: f32| {
        if value.is_finite() {
            clamp_wire(index, value)
        } else {
            default
        }
    };
    params.hpf_hz = fix(HPF_INDEX, params.hpf_hz, defaults.hpf_hz);
    params.lpf_hz = fix(LPF_INDEX, params.lpf_hz, defaults.lpf_hz);
    params.threshold_db = fix(THRESHOLD_INDEX, params.threshold_db, defaults.threshold_db);
    params.retrigger_ms = fix(RETRIGGER_INDEX, params.retrigger_ms, defaults.retrigger_ms);
    params.dynamics = fix(DYNAMICS_INDEX, params.dynamics, defaults.dynamics);
    params.offset_ms = fix(OFFSET_INDEX, params.offset_ms, defaults.offset_ms);
    params.pitch = fix(PITCH_INDEX, params.pitch, defaults.pitch);
    params.length_ms = fix(LENGTH_INDEX, params.length_ms, defaults.length_ms);
    params.fade_ms = fix(FADE_INDEX, params.fade_ms, defaults.fade_ms);
    params.voices = voices(params.voices as f32);
    params.dry_db = fix(DRY_INDEX, params.dry_db, defaults.dry_db);
    params.sample_db = fix(SAMPLE_INDEX, params.sample_db, defaults.sample_db);
    if params.sample_name.as_deref().is_some_and(str::is_empty) {
        params.sample_name = None;
    }
}

pub fn apply_wire_param(params: &mut Params, index: u32, value: f32) -> bool {
    if !value.is_finite() || index as usize >= PARAM_COUNT {
        return false;
    }
    match index {
        POWER_INDEX => params.power = value >= 0.5,
        LISTEN_INDEX => params.listen = value >= 0.5,
        HPF_INDEX => params.hpf_hz = clamp_wire(index, value),
        LPF_INDEX => params.lpf_hz = clamp_wire(index, value),
        THRESHOLD_INDEX => params.threshold_db = clamp_wire(index, value),
        RETRIGGER_INDEX => params.retrigger_ms = clamp_wire(index, value),
        DYNAMICS_INDEX => params.dynamics = clamp_wire(index, value),
        OFFSET_INDEX => params.offset_ms = clamp_wire(index, value),
        PITCH_INDEX => params.pitch = clamp_wire(index, value),
        LENGTH_INDEX => params.length_ms = clamp_wire(index, value),
        FADE_INDEX => params.fade_ms = clamp_wire(index, value),
        VOICES_INDEX => params.voices = voices(value),
        MODE_INDEX => params.mode = Mode::from_wire(value),
        DRY_INDEX => params.dry_db = clamp_wire(index, value),
        SAMPLE_INDEX => params.sample_db = clamp_wire(index, value),
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

/// Every wire param, in wire order.
pub fn ui_values(params: &Params) -> Vec<(&'static str, f32)> {
    vec![
        ("power", f32::from(u8::from(params.power))),
        ("listen", f32::from(u8::from(params.listen))),
        ("hpfHz", params.hpf_hz),
        ("lpfHz", params.lpf_hz),
        ("thresholdDb", params.threshold_db),
        ("retriggerMs", params.retrigger_ms),
        ("dynamics", params.dynamics),
        ("offsetMs", params.offset_ms),
        ("pitch", params.pitch),
        ("lengthMs", params.length_ms),
        ("fadeMs", params.fade_ms),
        ("voices", params.voices as f32),
        ("mode", params.mode.to_wire()),
        ("dryDb", params.dry_db),
        ("sampleDb", params.sample_db),
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
    fn the_descriptor_matches_the_wire_table() {
        let descriptor = crate::descriptor();
        assert_eq!(descriptor.params.len(), PARAM_COUNT);
        let defaults = ui_values(&default_params());
        for (index, param) in descriptor.params.iter().enumerate() {
            assert_eq!(param.id, UI_PARAM_IDS[index]);
            assert_eq!(param.default_value, defaults[index].1, "{}", param.id);
            let (min, max) = RANGES[index];
            if max > min {
                assert_eq!((param.min, param.max), (min, max), "{}", param.id);
            }
        }
    }

    #[test]
    fn every_value_survives_the_wire() {
        let source = Params {
            power: false,
            sample_name: None,
            listen: true,
            hpf_hz: 80.0,
            lpf_hz: 3_000.0,
            threshold_db: -31.0,
            retrigger_ms: 45.0,
            dynamics: 40.0,
            offset_ms: -1.5,
            pitch: -3.5,
            length_ms: 350.0,
            fade_ms: 12.0,
            voices: 2,
            mode: Mode::Blend,
            dry_db: -6.0,
            sample_db: 3.0,
        };
        let mut copy = default_params();
        for (index, (_, value)) in ui_values(&source).into_iter().enumerate() {
            assert!(apply_wire_param(&mut copy, index as u32, value), "{index}");
        }
        assert_eq!(copy, source);
    }

    #[test]
    fn state_round_trips_with_its_sample() {
        let params = Params {
            sample_name: Some("Kick 01.wav".into()),
            threshold_db: -12.0,
            mode: Mode::Blend,
            ..default_params()
        };
        let json = TriggerState::new(params.clone()).to_json().unwrap();
        let decoded = TriggerState::from_json(&json).unwrap();
        assert_eq!(decoded.params, params);
        assert_eq!(decoded.version, STATE_VERSION);
    }

    #[test]
    fn state_missing_fields_loads_with_defaults() {
        let decoded = TriggerState::from_json(
            r#"{"version":1,"params":{"thresholdDb":-80,"mode":"blend","sampleName":""}}"#,
        )
        .unwrap();
        let defaults = default_params();
        assert_eq!(decoded.params.threshold_db, -60.0, "clamped on load");
        assert_eq!(decoded.params.mode, Mode::Blend);
        assert_eq!(decoded.params.sample_name, None);
        assert_eq!(decoded.params.retrigger_ms, defaults.retrigger_ms);
        assert!(decoded.params.power);
    }

    #[test]
    fn invalid_and_non_finite_updates_are_rejected() {
        let mut params = default_params();
        assert!(!apply_wire_param(&mut params, u32::MAX, 1.0));
        assert!(!apply_wire_param(&mut params, PARAM_COUNT as u32, 1.0));
        assert!(!apply_wire_param(&mut params, THRESHOLD_INDEX, f32::NAN));
        assert!(!apply_ui_param(&mut params, "notAParam", 1.0));
        assert!(apply_ui_param(&mut params, "voices", 99.0));
        assert_eq!(params.voices, MAX_VOICES);
        assert!(apply_ui_param(&mut params, "offsetMs", -50.0));
        assert_eq!(params.offset_ms, OFFSET_MIN_MS);
    }
}

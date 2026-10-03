use serde::{Deserialize, Serialize};

use crate::{MAX_UNISON, Params, Waveform, clamp, default_params};

pub const PROTOCOL_VERSION: u32 = 1;
pub const STATE_VERSION: u32 = 1;

pub const UI_PARAM_IDS: [&str; 22] = [
    "power",
    "oscAWave",
    "oscAPosition",
    "oscALevel",
    "oscBWave",
    "oscBPosition",
    "oscBLevel",
    "oscBSemitones",
    "oscBDetuneCents",
    "unison",
    "unisonDetuneCents",
    "stereoWidth",
    "subLevel",
    "noiseLevel",
    "cutoffHz",
    "resonance",
    "filterDrive",
    "attackMs",
    "decayMs",
    "sustain",
    "releaseMs",
    "masterDb",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WrapSynthState {
    pub version: u32,
    pub params: Params,
}

impl WrapSynthState {
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

impl Default for WrapSynthState {
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

pub fn sanitize_params(params: &mut Params) {
    params.osc_a_position = clamp(params.osc_a_position, 0.0, 1.0);
    params.osc_a_level = clamp(params.osc_a_level, 0.0, 1.0);
    params.osc_b_position = clamp(params.osc_b_position, 0.0, 1.0);
    params.osc_b_level = clamp(params.osc_b_level, 0.0, 1.0);
    params.osc_b_semitones = clamp(params.osc_b_semitones, -24.0, 24.0);
    params.osc_b_detune_cents = clamp(params.osc_b_detune_cents, -50.0, 50.0);
    params.unison = params.unison.clamp(1, MAX_UNISON as u8);
    params.unison_detune_cents = clamp(params.unison_detune_cents, 0.0, 50.0);
    params.stereo_width = clamp(params.stereo_width, 0.0, 1.0);
    params.sub_level = clamp(params.sub_level, 0.0, 1.0);
    params.noise_level = clamp(params.noise_level, 0.0, 1.0);
    params.cutoff_hz = clamp(params.cutoff_hz, 40.0, 20_000.0);
    params.resonance = clamp(params.resonance, 0.0, 0.95);
    params.filter_drive = clamp(params.filter_drive, 0.0, 1.0);
    params.attack_ms = clamp(params.attack_ms, 0.5, 5_000.0);
    params.decay_ms = clamp(params.decay_ms, 1.0, 5_000.0);
    params.sustain = clamp(params.sustain, 0.0, 1.0);
    params.release_ms = clamp(params.release_ms, 5.0, 8_000.0);
    params.master_db = clamp(params.master_db, -24.0, 3.0);
}

pub fn apply_wire_param(params: &mut Params, index: u32, value: f32) -> bool {
    if !value.is_finite() {
        return false;
    }
    match ui_param_id(index) {
        Some("power") => params.power = value >= 0.5,
        Some("oscAWave") => params.osc_a_wave = Waveform::from_wire(value),
        Some("oscAPosition") => params.osc_a_position = clamp(value, 0.0, 1.0),
        Some("oscALevel") => params.osc_a_level = clamp(value, 0.0, 1.0),
        Some("oscBWave") => params.osc_b_wave = Waveform::from_wire(value),
        Some("oscBPosition") => params.osc_b_position = clamp(value, 0.0, 1.0),
        Some("oscBLevel") => params.osc_b_level = clamp(value, 0.0, 1.0),
        Some("oscBSemitones") => params.osc_b_semitones = clamp(value, -24.0, 24.0),
        Some("oscBDetuneCents") => params.osc_b_detune_cents = clamp(value, -50.0, 50.0),
        Some("unison") => params.unison = (value.round() as u8).clamp(1, MAX_UNISON as u8),
        Some("unisonDetuneCents") => params.unison_detune_cents = clamp(value, 0.0, 50.0),
        Some("stereoWidth") => params.stereo_width = clamp(value, 0.0, 1.0),
        Some("subLevel") => params.sub_level = clamp(value, 0.0, 1.0),
        Some("noiseLevel") => params.noise_level = clamp(value, 0.0, 1.0),
        Some("cutoffHz") => params.cutoff_hz = clamp(value, 40.0, 20_000.0),
        Some("resonance") => params.resonance = clamp(value, 0.0, 0.95),
        Some("filterDrive") => params.filter_drive = clamp(value, 0.0, 1.0),
        Some("attackMs") => params.attack_ms = clamp(value, 0.5, 5_000.0),
        Some("decayMs") => params.decay_ms = clamp(value, 1.0, 5_000.0),
        Some("sustain") => params.sustain = clamp(value, 0.0, 1.0),
        Some("releaseMs") => params.release_ms = clamp(value, 5.0, 8_000.0),
        Some("masterDb") => params.master_db = clamp(value, -24.0, 3.0),
        _ => return false,
    }
    true
}

pub fn ui_values(params: &Params) -> Vec<(&'static str, f32)> {
    vec![
        ("power", f32::from(params.power)),
        ("oscAWave", params.osc_a_wave.to_wire()),
        ("oscAPosition", params.osc_a_position),
        ("oscALevel", params.osc_a_level),
        ("oscBWave", params.osc_b_wave.to_wire()),
        ("oscBPosition", params.osc_b_position),
        ("oscBLevel", params.osc_b_level),
        ("oscBSemitones", params.osc_b_semitones),
        ("oscBDetuneCents", params.osc_b_detune_cents),
        ("unison", f32::from(params.unison)),
        ("unisonDetuneCents", params.unison_detune_cents),
        ("stereoWidth", params.stereo_width),
        ("subLevel", params.sub_level),
        ("noiseLevel", params.noise_level),
        ("cutoffHz", params.cutoff_hz),
        ("resonance", params.resonance),
        ("filterDrive", params.filter_drive),
        ("attackMs", params.attack_ms),
        ("decayMs", params.decay_ms),
        ("sustain", params.sustain),
        ("releaseMs", params.release_ms),
        ("masterDb", params.master_db),
    ]
}

/// The wire edits that turn `old` into `new`, as `(index, value)`.
pub fn wire_diff(old: &Params, new: &Params) -> Vec<(u32, f32)> {
    ui_values(old)
        .into_iter()
        .zip(ui_values(new))
        .filter(|((_, before), (_, after))| before != after)
        .filter_map(|(_, (id, value))| ui_param_index(id).map(|index| (index, value)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_diff_carries_only_what_changed() {
        let old = default_params();
        assert!(wire_diff(&old, &old).is_empty());
        let new = Params {
            cutoff_hz: 900.0,
            osc_b_wave: Waveform::Sine,
            ..old
        };
        let diff = wire_diff(&old, &new);
        assert_eq!(
            diff,
            vec![
                (ui_param_index("oscBWave").unwrap(), 3.0),
                (ui_param_index("cutoffHz").unwrap(), 900.0),
            ]
        );
        let mut applied = old;
        for (index, value) in diff {
            assert!(apply_wire_param(&mut applied, index, value));
        }
        assert_eq!(applied, new);
    }

    #[test]
    fn ids_round_trip_and_updates_clamp() {
        for (index, id) in UI_PARAM_IDS.iter().enumerate() {
            assert_eq!(ui_param_index(id), Some(index as u32));
            assert_eq!(ui_param_id(index as u32), Some(*id));
        }
        let mut params = default_params();
        assert!(apply_wire_param(
            &mut params,
            ui_param_index("unison").unwrap(),
            99.0
        ));
        assert_eq!(params.unison, MAX_UNISON as u8);
        assert!(!apply_wire_param(&mut params, u32::MAX, 1.0));
        assert!(!apply_wire_param(&mut params, 0, f32::NAN));
    }

    #[test]
    fn state_round_trips() {
        let mut params = default_params();
        params.cutoff_hz = 777.0;
        let json = WrapSynthState::new(params).to_json().unwrap();
        assert_eq!(
            WrapSynthState::from_json(&json).unwrap().params.cutoff_hz,
            777.0
        );
    }
}

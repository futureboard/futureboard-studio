use serde::{Deserialize, Serialize};

use crate::{PADS, Pad, Params, clamp, default_params};

pub const PROTOCOL_VERSION: u32 = 1;
pub const STATE_VERSION: u32 = 1;

/// Fields flattened per pad, in wire order. `apply_wire_param`/`ui_values`
/// index into a pad's fields by this order rather than by string match, since
/// the id space is `PADS * FIELDS_PER_PAD` (160) wide.
pub const FIELDS_PER_PAD: u32 = 10;

macro_rules! pad_ids {
    ($($n:literal),* $(,)?) => {
        [
            $(
                concat!("pad", $n, "Note"),
                concat!("pad", $n, "Tune"),
                concat!("pad", $n, "Gain"),
                concat!("pad", $n, "Pan"),
                concat!("pad", $n, "Choke"),
                concat!("pad", $n, "Attack"),
                concat!("pad", $n, "Release"),
                concat!("pad", $n, "Reverse"),
                concat!("pad", $n, "Mute"),
                concat!("pad", $n, "Solo"),
            )*
        ]
    };
}

pub const UI_PARAM_IDS: [&str; PADS * FIELDS_PER_PAD as usize] =
    pad_ids!(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DrumSamplerState {
    pub version: u32,
    pub params: Params,
}

impl DrumSamplerState {
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

impl Default for DrumSamplerState {
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

fn sanitize_pad(pad: &mut Pad) {
    pad.tune_semitones = clamp(pad.tune_semitones, -24.0, 24.0);
    pad.gain_db = clamp(pad.gain_db, -60.0, 12.0);
    pad.pan = clamp(pad.pan, -1.0, 1.0);
    pad.choke_group = pad.choke_group.min(8);
    pad.attack_ms = clamp(pad.attack_ms, 0.0, 250.0);
    pad.release_ms = clamp(pad.release_ms, 1.0, 2_000.0);
}

pub fn sanitize_params(params: &mut Params) {
    for pad in &mut params.pads {
        sanitize_pad(pad);
    }
}

/// Apply one wire edit. `index` is decomposed into a pad (`index /
/// FIELDS_PER_PAD`) and a field within that pad (`index % FIELDS_PER_PAD`) -
/// no string match on the realtime path.
pub fn apply_wire_param(params: &mut Params, index: u32, value: f32) -> bool {
    if !value.is_finite() {
        return false;
    }
    let pad_index = (index / FIELDS_PER_PAD) as usize;
    let field = index % FIELDS_PER_PAD;
    let Some(pad) = params.pads.get_mut(pad_index) else {
        return false;
    };
    match field {
        0 => pad.note = clamp(value, 0.0, 127.0).round() as u8,
        1 => pad.tune_semitones = clamp(value, -24.0, 24.0),
        2 => pad.gain_db = clamp(value, -60.0, 12.0),
        3 => pad.pan = clamp(value, -1.0, 1.0),
        4 => pad.choke_group = (clamp(value, 0.0, 8.0).round() as u8).min(8),
        5 => pad.attack_ms = clamp(value, 0.0, 250.0),
        6 => pad.release_ms = clamp(value, 1.0, 2_000.0),
        7 => pad.reverse = value >= 0.5,
        8 => pad.muted = value >= 0.5,
        9 => pad.solo = value >= 0.5,
        _ => return false,
    }
    true
}

pub fn ui_values(params: &Params) -> Vec<(&'static str, f32)> {
    let mut out = Vec::with_capacity(PADS * FIELDS_PER_PAD as usize);
    for (pad_index, pad) in params.pads.iter().enumerate() {
        let base = pad_index * FIELDS_PER_PAD as usize;
        out.push((UI_PARAM_IDS[base], f32::from(pad.note)));
        out.push((UI_PARAM_IDS[base + 1], pad.tune_semitones));
        out.push((UI_PARAM_IDS[base + 2], pad.gain_db));
        out.push((UI_PARAM_IDS[base + 3], pad.pan));
        out.push((UI_PARAM_IDS[base + 4], f32::from(pad.choke_group)));
        out.push((UI_PARAM_IDS[base + 5], pad.attack_ms));
        out.push((UI_PARAM_IDS[base + 6], pad.release_ms));
        out.push((UI_PARAM_IDS[base + 7], f32::from(pad.reverse)));
        out.push((UI_PARAM_IDS[base + 8], f32::from(pad.muted)));
        out.push((UI_PARAM_IDS[base + 9], f32::from(pad.solo)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_round_trip() {
        for (index, id) in UI_PARAM_IDS.iter().enumerate() {
            assert_eq!(ui_param_index(id), Some(index as u32));
            assert_eq!(ui_param_id(index as u32), Some(*id));
        }
        let mut seen = std::collections::HashSet::new();
        assert!(UI_PARAM_IDS.iter().all(|id| seen.insert(*id)));
    }

    #[test]
    fn apply_wire_param_clamps_and_rejects_out_of_range() {
        let mut params = default_params();
        assert!(apply_wire_param(
            &mut params,
            ui_param_index("pad2Gain").unwrap(),
            99.0
        ));
        assert_eq!(params.pads[2].gain_db, 12.0);
        assert!(!apply_wire_param(
            &mut params,
            (PADS * FIELDS_PER_PAD as usize) as u32,
            1.0
        ));
        assert!(!apply_wire_param(&mut params, 0, f32::NAN));
    }

    #[test]
    fn state_round_trips() {
        let mut params = default_params();
        params.pads[5].gain_db = -3.0;
        let json = DrumSamplerState::new(params).to_json().unwrap();
        assert_eq!(
            DrumSamplerState::from_json(&json).unwrap().params.pads[5].gain_db,
            -3.0
        );
    }
}

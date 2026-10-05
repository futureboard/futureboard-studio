//! Stable editor/DSP parameter wire contract for WhiteSharp.
//!
//! String ids live at the UI/control boundary only. The native host resolves
//! them to compact indices before publishing edits to an audio-side bounded
//! queue; [`Dsp::apply_wire_param`](crate::Dsp::apply_wire_param) consumes the
//! numeric form without allocation, serialization, locking, or string lookup.

use serde::{Deserialize, Serialize};

use crate::scale::ALL_NOTES;
use crate::{InputType, MAX_RETUNE_MS, Params, Scale, VibratoShape, clamp, default_params};

pub const PROTOCOL_VERSION: u32 = 1;
pub const STATE_VERSION: u32 = 1;

pub const POWER_INDEX: u32 = 0;
pub const KEY_INDEX: u32 = 1;
pub const SCALE_INDEX: u32 = 2;
pub const INPUT_TYPE_INDEX: u32 = 3;
pub const RETUNE_INDEX: u32 = 4;
pub const HUMANIZE_INDEX: u32 = 5;
pub const FLEX_TUNE_INDEX: u32 = 6;
pub const VIBRATO_INDEX: u32 = 7;
pub const FORMANT_INDEX: u32 = 8;
pub const THROAT_INDEX: u32 = 9;
pub const TRANSPOSE_INDEX: u32 = 10;
pub const DETUNE_INDEX: u32 = 11;
pub const TRACKING_INDEX: u32 = 12;
pub const REMOVE_MASK_INDEX: u32 = 13;
pub const BYPASS_MASK_INDEX: u32 = 14;
pub const MIX_INDEX: u32 = 15;
pub const OUTPUT_INDEX: u32 = 16;
pub const CLASSIC_INDEX: u32 = 17;
pub const VIBRATO_SHAPE_INDEX: u32 = 18;
pub const VIBRATO_RATE_INDEX: u32 = 19;
pub const VIBRATO_DELAY_INDEX: u32 = 20;
pub const VIBRATO_ONSET_INDEX: u32 = 21;
pub const VIBRATO_PITCH_INDEX: u32 = 22;
pub const VIBRATO_AMP_INDEX: u32 = 23;
pub const VIBRATO_VARIATION_INDEX: u32 = 24;

pub const PARAM_COUNT: usize = 25;

/// Wire index *is* the position in this table; the editor and the host both
/// resolve through it, so the order is part of the persisted contract.
/// Append only.
pub const UI_PARAM_IDS: [&str; PARAM_COUNT] = [
    "power",
    "key",
    "scale",
    "inputType",
    "retuneMs",
    "humanize",
    "flexTune",
    "vibratoDb",
    "formant",
    "throat",
    "transpose",
    "detune",
    "tracking",
    "removeMask",
    "bypassMask",
    "mix",
    "outputDb",
    "classic",
    "vibratoShape",
    "vibratoRateHz",
    "vibratoDelayMs",
    "vibratoOnsetMs",
    "vibratoPitch",
    "vibratoAmp",
    "vibratoVariation",
];

/// Inclusive `(min, max)` for every continuous parameter, indexed by wire
/// index; flags, enums and masks carry `(0, 0)` and have their own arms.
const RANGES: [(f32, f32); PARAM_COUNT] = [
    (0.0, 0.0),           // power
    (0.0, 0.0),           // key
    (0.0, 0.0),           // scale
    (0.0, 0.0),           // inputType
    (0.0, MAX_RETUNE_MS), // retuneMs
    (0.0, 100.0),         // humanize
    (0.0, 100.0),         // flexTune
    (-12.0, 12.0),        // vibratoDb
    (0.0, 0.0),           // formant
    (70.0, 140.0),        // throat
    (-24.0, 24.0),        // transpose
    (-100.0, 100.0),      // detune
    (0.0, 100.0),         // tracking
    (0.0, 0.0),           // removeMask
    (0.0, 0.0),           // bypassMask
    (0.0, 100.0),         // mix
    (-24.0, 12.0),        // outputDb
    (0.0, 0.0),           // classic
    (0.0, 0.0),           // vibratoShape
    (0.1, 10.0),          // vibratoRateHz
    (0.0, 2_000.0),       // vibratoDelayMs
    (0.0, 2_000.0),       // vibratoOnsetMs
    (0.0, 100.0),         // vibratoPitch
    (0.0, 100.0),         // vibratoAmp
    (0.0, 100.0),         // vibratoVariation
];

#[inline]
fn clamp_wire(index: u32, value: f32) -> f32 {
    let (min, max) = RANGES[index as usize];
    clamp(value, min, max)
}

#[inline]
fn wire_to_mask(value: f32) -> u16 {
    (clamp(value, 0.0, ALL_NOTES as f32).round() as u16) & ALL_NOTES
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WhiteSharpState {
    pub version: u32,
    pub params: Params,
}

impl WhiteSharpState {
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

impl Default for WhiteSharpState {
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

/// Clamp a whole `Params` into range — a project blob may predate a range
/// change or have been edited by hand.
pub fn sanitize_params(params: &mut Params) {
    params.key = params.key.min(11);
    params.retune_ms = clamp_wire(RETUNE_INDEX, params.retune_ms);
    params.humanize = clamp_wire(HUMANIZE_INDEX, params.humanize);
    params.flex_tune = clamp_wire(FLEX_TUNE_INDEX, params.flex_tune);
    params.vibrato_db = clamp_wire(VIBRATO_INDEX, params.vibrato_db);
    params.throat = clamp_wire(THROAT_INDEX, params.throat);
    params.transpose = clamp_wire(TRANSPOSE_INDEX, params.transpose).round();
    params.detune = clamp_wire(DETUNE_INDEX, params.detune);
    params.tracking = clamp_wire(TRACKING_INDEX, params.tracking);
    params.remove_mask &= ALL_NOTES;
    params.bypass_mask &= ALL_NOTES;
    params.mix = clamp_wire(MIX_INDEX, params.mix);
    params.output_db = clamp_wire(OUTPUT_INDEX, params.output_db);
    params.vibrato_rate_hz = clamp_wire(VIBRATO_RATE_INDEX, params.vibrato_rate_hz);
    params.vibrato_delay_ms = clamp_wire(VIBRATO_DELAY_INDEX, params.vibrato_delay_ms);
    params.vibrato_onset_ms = clamp_wire(VIBRATO_ONSET_INDEX, params.vibrato_onset_ms);
    params.vibrato_pitch = clamp_wire(VIBRATO_PITCH_INDEX, params.vibrato_pitch);
    params.vibrato_amp = clamp_wire(VIBRATO_AMP_INDEX, params.vibrato_amp);
    params.vibrato_variation = clamp_wire(VIBRATO_VARIATION_INDEX, params.vibrato_variation);
}

/// Apply one compact UI/control update. Allocation-free and total: invalid
/// indices and non-finite values are rejected; everything else is clamped.
pub fn apply_wire_param(params: &mut Params, index: u32, value: f32) -> bool {
    if !value.is_finite() || index as usize >= PARAM_COUNT {
        return false;
    }
    match index {
        POWER_INDEX => params.power = value >= 0.5,
        KEY_INDEX => params.key = clamp(value, 0.0, 11.0).round() as u8,
        SCALE_INDEX => params.scale = Scale::from_wire(value),
        INPUT_TYPE_INDEX => params.input_type = InputType::from_wire(value),
        FORMANT_INDEX => params.formant = value >= 0.5,
        TRANSPOSE_INDEX => params.transpose = clamp_wire(index, value).round(),
        REMOVE_MASK_INDEX => params.remove_mask = wire_to_mask(value),
        BYPASS_MASK_INDEX => params.bypass_mask = wire_to_mask(value),
        RETUNE_INDEX => params.retune_ms = clamp_wire(index, value),
        HUMANIZE_INDEX => params.humanize = clamp_wire(index, value),
        FLEX_TUNE_INDEX => params.flex_tune = clamp_wire(index, value),
        VIBRATO_INDEX => params.vibrato_db = clamp_wire(index, value),
        THROAT_INDEX => params.throat = clamp_wire(index, value),
        DETUNE_INDEX => params.detune = clamp_wire(index, value),
        TRACKING_INDEX => params.tracking = clamp_wire(index, value),
        MIX_INDEX => params.mix = clamp_wire(index, value),
        OUTPUT_INDEX => params.output_db = clamp_wire(index, value),
        CLASSIC_INDEX => params.classic = value >= 0.5,
        VIBRATO_SHAPE_INDEX => params.vibrato_shape = VibratoShape::from_wire(value),
        VIBRATO_RATE_INDEX => params.vibrato_rate_hz = clamp_wire(index, value),
        VIBRATO_DELAY_INDEX => params.vibrato_delay_ms = clamp_wire(index, value),
        VIBRATO_ONSET_INDEX => params.vibrato_onset_ms = clamp_wire(index, value),
        VIBRATO_PITCH_INDEX => params.vibrato_pitch = clamp_wire(index, value),
        VIBRATO_AMP_INDEX => params.vibrato_amp = clamp_wire(index, value),
        VIBRATO_VARIATION_INDEX => params.vibrato_variation = clamp_wire(index, value),
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

/// Every parameter as `(id, raw value)`, in wire order. Drives project replay,
/// the editor's edits and the descriptor-vs-defaults check.
pub fn ui_values(params: &Params) -> Vec<(&'static str, f32)> {
    vec![
        ("power", f32::from(params.power)),
        ("key", f32::from(params.key)),
        ("scale", params.scale.to_wire()),
        ("inputType", params.input_type.to_wire()),
        ("retuneMs", params.retune_ms),
        ("humanize", params.humanize),
        ("flexTune", params.flex_tune),
        ("vibratoDb", params.vibrato_db),
        ("formant", f32::from(params.formant)),
        ("throat", params.throat),
        ("transpose", params.transpose),
        ("detune", params.detune),
        ("tracking", params.tracking),
        ("removeMask", f32::from(params.remove_mask)),
        ("bypassMask", f32::from(params.bypass_mask)),
        ("mix", params.mix),
        ("outputDb", params.output_db),
        ("classic", f32::from(params.classic)),
        ("vibratoShape", params.vibrato_shape.to_wire()),
        ("vibratoRateHz", params.vibrato_rate_hz),
        ("vibratoDelayMs", params.vibrato_delay_ms),
        ("vibratoOnsetMs", params.vibrato_onset_ms),
        ("vibratoPitch", params.vibrato_pitch),
        ("vibratoAmp", params.vibrato_amp),
        ("vibratoVariation", params.vibrato_variation),
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
    fn a_replay_through_the_wire_rebuilds_the_params() {
        let mut saved = default_params();
        saved.key = 9;
        saved.scale = Scale::Minor;
        saved.input_type = InputType::Soprano;
        saved.retune_ms = 0.0;
        saved.remove_mask = 0b1000_0000_0001;
        saved.bypass_mask = 0b10;
        saved.transpose = -12.0;
        saved.formant = false;
        saved.classic = true;
        saved.vibrato_shape = VibratoShape::Square;
        saved.vibrato_rate_hz = 6.5;
        saved.vibrato_amp = 40.0;
        let mut rebuilt = default_params();
        for (id, value) in ui_values(&saved) {
            assert!(apply_ui_param(&mut rebuilt, id, value), "{id}");
        }
        assert_eq!(rebuilt, saved);
        let json = WhiteSharpState::new(saved.clone()).to_json().unwrap();
        assert_eq!(WhiteSharpState::from_json(&json).unwrap().params, saved);
    }

    #[test]
    fn edits_are_clamped_and_nonsense_rejected() {
        let mut params = default_params();
        assert!(apply_ui_param(&mut params, "key", 40.0));
        assert_eq!(params.key, 11);
        assert!(apply_ui_param(&mut params, "removeMask", 99_999.0));
        assert_eq!(params.remove_mask, ALL_NOTES);
        assert!(apply_ui_param(&mut params, "transpose", 3.4));
        assert_eq!(params.transpose, 3.0);
        assert!(!apply_ui_param(&mut params, "retuneMs", f32::NAN));
        assert!(!apply_wire_param(&mut params, PARAM_COUNT as u32, 1.0));
        assert!(!apply_ui_param(&mut params, "nope", 1.0));
    }
}

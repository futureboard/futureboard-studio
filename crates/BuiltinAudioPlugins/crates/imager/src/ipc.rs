//! Stable editor/DSP parameter wire contract for Imager.
//!
//! String ids live at the UI/control boundary only. The native host resolves
//! them to compact indices before publishing edits to an audio-side bounded
//! queue; [`Dsp::apply_wire_param`](crate::Dsp::apply_wire_param) consumes the
//! numeric form without allocation, serialization, locking, or string lookup.

use serde::{Deserialize, Serialize};

use crate::{
    BAND_COUNT, MAX_CROSSOVER_HZ, MAX_OUTPUT_DB, MAX_STEREOIZE, MAX_WIDTH, MIN_CROSSOVER_HZ,
    MIN_OUTPUT_DB, Params, SOLO_NONE, StereoizeMode, clamp, default_params,
};

pub const PROTOCOL_VERSION: u32 = 1;
pub const STATE_VERSION: u32 = 1;

pub const POWER_INDEX: u32 = 0;
pub const CROSSOVER_1_INDEX: u32 = 1;
pub const CROSSOVER_2_INDEX: u32 = 2;
pub const CROSSOVER_3_INDEX: u32 = 3;
pub const WIDTH_1_INDEX: u32 = 4;
pub const WIDTH_2_INDEX: u32 = 5;
pub const WIDTH_3_INDEX: u32 = 6;
pub const WIDTH_4_INDEX: u32 = 7;
pub const SOLO_INDEX: u32 = 8;
pub const OUTPUT_INDEX: u32 = 9;
pub const STEREOIZE_1_INDEX: u32 = 10;
pub const STEREOIZE_2_INDEX: u32 = 11;
pub const STEREOIZE_3_INDEX: u32 = 12;
pub const STEREOIZE_4_INDEX: u32 = 13;
pub const STEREOIZE_MODE_INDEX: u32 = 14;
pub const RECOVER_SIDES_INDEX: u32 = 15;
pub const MULTIBAND_INDEX: u32 = 16;

pub const PARAM_COUNT: usize = 17;

/// Wire index *is* the position in this table; the editor and the host both
/// resolve through it, so the order is part of the persisted contract. Append
/// only.
pub const UI_PARAM_IDS: [&str; PARAM_COUNT] = [
    "power",
    "crossover1Hz",
    "crossover2Hz",
    "crossover3Hz",
    "width1",
    "width2",
    "width3",
    "width4",
    "soloBand",
    "outputDb",
    "stereoize1",
    "stereoize2",
    "stereoize3",
    "stereoize4",
    "stereoizeMode",
    "recoverSides",
    "multiband",
];

/// Inclusive `(min, max)` for every continuous parameter, indexed by wire
/// index. The power flag carries `(0, 0)` and is handled by its own arm in
/// [`apply_wire_param`]. Single source of truth for clamping, so
/// [`sanitize_params`] and the wire path cannot drift apart.
const RANGES: [(f32, f32); PARAM_COUNT] = [
    (0.0, 0.0),                                  // power
    (MIN_CROSSOVER_HZ, MAX_CROSSOVER_HZ),        // crossover1Hz
    (MIN_CROSSOVER_HZ, MAX_CROSSOVER_HZ),        // crossover2Hz
    (MIN_CROSSOVER_HZ, MAX_CROSSOVER_HZ),        // crossover3Hz
    (0.0, MAX_WIDTH),                            // width1
    (0.0, MAX_WIDTH),                            // width2
    (0.0, MAX_WIDTH),                            // width3
    (0.0, MAX_WIDTH),                            // width4
    (SOLO_NONE as f32, (BAND_COUNT - 1) as f32), // soloBand
    (MIN_OUTPUT_DB, MAX_OUTPUT_DB),              // outputDb
    (0.0, MAX_STEREOIZE),                        // stereoize1
    (0.0, MAX_STEREOIZE),                        // stereoize2
    (0.0, MAX_STEREOIZE),                        // stereoize3
    (0.0, MAX_STEREOIZE),                        // stereoize4
    (0.0, 1.0),                                  // stereoizeMode
    (0.0, 0.0),                                  // recoverSides
    (0.0, 0.0),                                  // multiband
];

#[inline]
fn clamp_wire(index: u32, value: f32) -> f32 {
    let (min, max) = RANGES[index as usize];
    clamp(value, min, max)
}

/// A solo travels as a band index, or −1 for none. Rounded rather than
/// truncated so a host stepping it in normalised units cannot land one short.
#[inline]
fn wire_to_solo(value: f32) -> i32 {
    clamp_wire(SOLO_INDEX, value).round() as i32
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImagerState {
    pub version: u32,
    pub params: Params,
}

impl ImagerState {
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

impl Default for ImagerState {
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
/// which may predate a range change or have been edited by hand. Crossover
/// order is *not* forced here — the DSP sorts at use — so a blob round-trips
/// exactly as it was written.
pub fn sanitize_params(params: &mut Params) {
    for (index, hz) in params.crossover_hz.iter_mut().enumerate() {
        let value = if hz.is_finite() {
            *hz
        } else {
            crate::DEFAULT_CROSSOVERS_HZ[index]
        };
        *hz = clamp_wire(CROSSOVER_1_INDEX + index as u32, value);
    }
    for (index, width) in params.width.iter_mut().enumerate() {
        let value = if width.is_finite() {
            *width
        } else {
            crate::DEFAULT_WIDTH
        };
        *width = clamp_wire(WIDTH_1_INDEX + index as u32, value);
    }
    for (index, amount) in params.stereoize.iter_mut().enumerate() {
        let value = if amount.is_finite() { *amount } else { 0.0 };
        *amount = clamp_wire(STEREOIZE_1_INDEX + index as u32, value);
    }
    params.solo_band = params.solo_band.clamp(SOLO_NONE, BAND_COUNT as i32 - 1);
    params.output_db = if params.output_db.is_finite() {
        clamp_wire(OUTPUT_INDEX, params.output_db)
    } else {
        0.0
    };
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
        CROSSOVER_1_INDEX..=CROSSOVER_3_INDEX => {
            params.crossover_hz[(index - CROSSOVER_1_INDEX) as usize] = clamp_wire(index, value)
        }
        WIDTH_1_INDEX..=WIDTH_4_INDEX => {
            params.width[(index - WIDTH_1_INDEX) as usize] = clamp_wire(index, value)
        }
        SOLO_INDEX => params.solo_band = wire_to_solo(value),
        OUTPUT_INDEX => params.output_db = clamp_wire(index, value),
        STEREOIZE_1_INDEX..=STEREOIZE_4_INDEX => {
            params.stereoize[(index - STEREOIZE_1_INDEX) as usize] = clamp_wire(index, value)
        }
        STEREOIZE_MODE_INDEX => params.stereoize_mode = StereoizeMode::from_wire(value),
        RECOVER_SIDES_INDEX => params.recover_sides = value >= 0.5,
        MULTIBAND_INDEX => params.multiband = value >= 0.5,
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
        ("crossover1Hz", params.crossover_hz[0]),
        ("crossover2Hz", params.crossover_hz[1]),
        ("crossover3Hz", params.crossover_hz[2]),
        ("width1", params.width[0]),
        ("width2", params.width[1]),
        ("width3", params.width[2]),
        ("width4", params.width[3]),
        ("soloBand", params.solo_band as f32),
        ("outputDb", params.output_db),
        ("stereoize1", params.stereoize[0]),
        ("stereoize2", params.stereoize[1]),
        ("stereoize3", params.stereoize[2]),
        ("stereoize4", params.stereoize[3]),
        ("stereoizeMode", params.stereoize_mode.to_wire()),
        ("recoverSides", f32::from(params.recover_sides)),
        ("multiband", f32::from(params.multiband)),
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

    /// `ui_values` is what project replay pushes back through the wire, so a
    /// missing or misordered entry would silently drop a parameter on reload.
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
        assert!(apply_ui_param(&mut params, "width3", 160.0));
        assert!(apply_ui_param(&mut params, "crossover2Hz", 2_400.0));
        assert!(apply_ui_param(&mut params, "soloBand", 1.0));
        let json = ImagerState::new(params).to_json().unwrap();
        let decoded = ImagerState::from_json(&json).unwrap();
        assert_eq!(decoded.params.width[2], 160.0);
        assert_eq!(decoded.params.crossover_hz[1], 2_400.0);
        assert_eq!(decoded.params.solo_band, 1);
        assert_eq!(decoded.version, STATE_VERSION);
    }

    #[test]
    fn a_replay_through_the_wire_rebuilds_the_state() {
        let mut saved = default_params();
        saved.power = false;
        saved.crossover_hz = [300.0, 2_000.0, 9_000.0];
        saved.width = [0.0, 80.0, 140.0, 200.0];
        saved.solo_band = 3;
        saved.output_db = -3.5;
        saved.stereoize = [0.0, 25.0, 60.0, 100.0];
        saved.stereoize_mode = StereoizeMode::Two;
        saved.recover_sides = true;
        saved.multiband = false;

        let mut rebuilt = default_params();
        for (id, value) in ui_values(&saved) {
            assert!(
                apply_ui_param(&mut rebuilt, id, value),
                "`{id}` was rejected"
            );
        }
        assert!(!rebuilt.power);
        assert_eq!(rebuilt.crossover_hz, saved.crossover_hz);
        assert_eq!(rebuilt.width, saved.width);
        assert_eq!(rebuilt.solo_band, 3);
        assert_eq!(rebuilt.output_db, -3.5);
        assert_eq!(rebuilt.stereoize, saved.stereoize);
        assert_eq!(rebuilt.stereoize_mode, StereoizeMode::Two);
        assert!(rebuilt.recover_sides);
        assert!(!rebuilt.multiband);
    }

    #[test]
    fn out_of_range_values_are_clamped_not_rejected() {
        let mut params = default_params();
        assert!(apply_ui_param(&mut params, "width1", 900.0));
        assert_eq!(params.width[0], MAX_WIDTH);
        assert!(apply_ui_param(&mut params, "crossover3Hz", 96_000.0));
        assert_eq!(params.crossover_hz[2], MAX_CROSSOVER_HZ);
        assert!(apply_ui_param(&mut params, "soloBand", 7.0));
        assert_eq!(params.solo_band, BAND_COUNT as i32 - 1);
        assert!(apply_ui_param(&mut params, "soloBand", -5.0));
        assert_eq!(params.solo_band, SOLO_NONE);
        assert!(apply_ui_param(&mut params, "soloBand", 1.6));
        assert_eq!(params.solo_band, 2);
    }

    #[test]
    fn invalid_and_non_finite_updates_are_rejected() {
        let mut params = default_params();
        assert!(!apply_wire_param(&mut params, u32::MAX, 1.0));
        assert!(!apply_wire_param(&mut params, PARAM_COUNT as u32, 1.0));
        assert!(!apply_wire_param(&mut params, WIDTH_1_INDEX, f32::NAN));
        assert!(!apply_wire_param(&mut params, OUTPUT_INDEX, f32::INFINITY));
        assert!(!apply_ui_param(&mut params, "notAParam", 1.0));
    }

    /// A blob edited by hand must not reach the DSP with values the
    /// coefficient math cannot survive.
    #[test]
    fn sanitize_pulls_a_hand_edited_blob_back_into_range() {
        let mut params = default_params();
        params.crossover_hz = [-5.0, f32::NAN, 1.0e9];
        params.width = [-10.0, 500.0, f32::INFINITY, 100.0];
        params.solo_band = 42;
        params.output_db = 80.0;
        sanitize_params(&mut params);
        assert_eq!(params.crossover_hz[0], MIN_CROSSOVER_HZ);
        assert_eq!(params.crossover_hz[1], crate::DEFAULT_CROSSOVERS_HZ[1]);
        assert_eq!(params.crossover_hz[2], MAX_CROSSOVER_HZ);
        assert_eq!(params.width, [0.0, MAX_WIDTH, crate::DEFAULT_WIDTH, 100.0]);
        assert_eq!(params.solo_band, BAND_COUNT as i32 - 1);
        assert_eq!(params.output_db, MAX_OUTPUT_DB);
    }
}

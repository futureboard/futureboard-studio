//! Stable editor/DSP parameter wire contract for the Compressor.
//!
//! String ids live at the UI/control boundary only. The native host resolves
//! them to compact indices before publishing edits to an audio-side bounded
//! queue; [`Dsp::apply_wire_param`](crate::Dsp::apply_wire_param) consumes the
//! numeric form without allocation, serialization, locking, or string lookup.

use serde::{Deserialize, Serialize};

use crate::{
    BAND_COUNT, DEFAULT_BANDS, MAX_ATTACK_MS, MAX_CROSSOVER_HZ, MAX_KNEE_DB, MAX_MAKEUP_DB,
    MAX_OUTPUT_DB, MAX_RATIO, MAX_RELEASE_MS, MAX_SIDECHAIN_HZ, MAX_THRESHOLD_DB, MIN_ATTACK_MS,
    MIN_CROSSOVER_HZ, MIN_MAKEUP_DB, MIN_OUTPUT_DB, MIN_RATIO, MIN_RELEASE_MS, MIN_THRESHOLD_DB,
    Mode, Params, SOLO_NONE, default_params,
};
use builtin_dsp_core::clamp;

pub const PROTOCOL_VERSION: u32 = 1;
pub const STATE_VERSION: u32 = 1;

pub const POWER_INDEX: u32 = 0;
pub const MODE_INDEX: u32 = 1;
pub const THRESHOLD_INDEX: u32 = 2;
pub const RATIO_INDEX: u32 = 3;
pub const ATTACK_INDEX: u32 = 4;
pub const RELEASE_INDEX: u32 = 5;
pub const MAKEUP_INDEX: u32 = 6;
pub const SIDECHAIN_INDEX: u32 = 7;
pub const KNEE_INDEX: u32 = 8;
pub const MIX_INDEX: u32 = 9;
pub const OUTPUT_INDEX: u32 = 10;
pub const CROSSOVER_1_INDEX: u32 = 11;
pub const CROSSOVER_2_INDEX: u32 = 12;
pub const CROSSOVER_3_INDEX: u32 = 13;
/// First band parameter; each band owns [`BAND_STRIDE`] consecutive indices,
/// in the order of the `BAND_*` offsets below.
pub const BAND_BASE_INDEX: u32 = 14;
pub const BAND_STRIDE: u32 = 6;
pub const BAND_THRESHOLD: u32 = 0;
pub const BAND_RATIO: u32 = 1;
pub const BAND_ATTACK: u32 = 2;
pub const BAND_RELEASE: u32 = 3;
pub const BAND_MAKEUP: u32 = 4;
pub const BAND_BYPASS: u32 = 5;
pub const SOLO_INDEX: u32 = BAND_BASE_INDEX + BAND_STRIDE * BAND_COUNT as u32;

pub const PARAM_COUNT: usize = 39;
const _: () = assert!(SOLO_INDEX as usize + 1 == PARAM_COUNT);

/// Wire index *is* the position in this table; the editor and the host both
/// resolve through it, so the order is part of the persisted contract. Append
/// only.
pub const UI_PARAM_IDS: [&str; PARAM_COUNT] = [
    "power",
    "mode",
    "thresholdDb",
    "ratio",
    "attackMs",
    "releaseMs",
    "makeupDb",
    "sidechainHpfHz",
    "kneeDb",
    "mix",
    "outputDb",
    "crossover1Hz",
    "crossover2Hz",
    "crossover3Hz",
    "band1ThresholdDb",
    "band1Ratio",
    "band1AttackMs",
    "band1ReleaseMs",
    "band1MakeupDb",
    "band1Bypass",
    "band2ThresholdDb",
    "band2Ratio",
    "band2AttackMs",
    "band2ReleaseMs",
    "band2MakeupDb",
    "band2Bypass",
    "band3ThresholdDb",
    "band3Ratio",
    "band3AttackMs",
    "band3ReleaseMs",
    "band3MakeupDb",
    "band3Bypass",
    "band4ThresholdDb",
    "band4Ratio",
    "band4AttackMs",
    "band4ReleaseMs",
    "band4MakeupDb",
    "band4Bypass",
    "soloBand",
];

/// `(band, field offset)` for a band parameter's wire index.
pub fn band_param(index: u32) -> Option<(usize, u32)> {
    if !(BAND_BASE_INDEX..SOLO_INDEX).contains(&index) {
        return None;
    }
    let offset = index - BAND_BASE_INDEX;
    Some(((offset / BAND_STRIDE) as usize, offset % BAND_STRIDE))
}

/// Inclusive `(min, max)` of a continuous parameter. Switches carry `(0, 1)`.
/// Single source of truth for clamping, so [`sanitize_params`] and the wire
/// path cannot drift apart.
fn range(index: u32) -> (f32, f32) {
    match index {
        THRESHOLD_INDEX => (MIN_THRESHOLD_DB, MAX_THRESHOLD_DB),
        RATIO_INDEX => (MIN_RATIO, MAX_RATIO),
        ATTACK_INDEX => (MIN_ATTACK_MS, MAX_ATTACK_MS),
        RELEASE_INDEX => (MIN_RELEASE_MS, MAX_RELEASE_MS),
        MAKEUP_INDEX => (MIN_MAKEUP_DB, MAX_MAKEUP_DB),
        SIDECHAIN_INDEX => (0.0, MAX_SIDECHAIN_HZ),
        KNEE_INDEX => (0.0, MAX_KNEE_DB),
        MIX_INDEX => (0.0, 100.0),
        OUTPUT_INDEX => (MIN_OUTPUT_DB, MAX_OUTPUT_DB),
        CROSSOVER_1_INDEX..=CROSSOVER_3_INDEX => (MIN_CROSSOVER_HZ, MAX_CROSSOVER_HZ),
        SOLO_INDEX => (SOLO_NONE as f32, (BAND_COUNT - 1) as f32),
        _ => match band_param(index).map(|(_, field)| field) {
            Some(BAND_THRESHOLD) => (MIN_THRESHOLD_DB, MAX_THRESHOLD_DB),
            Some(BAND_RATIO) => (MIN_RATIO, MAX_RATIO),
            Some(BAND_ATTACK) => (MIN_ATTACK_MS, MAX_ATTACK_MS),
            Some(BAND_RELEASE) => (MIN_RELEASE_MS, MAX_RELEASE_MS),
            Some(BAND_MAKEUP) => (MIN_MAKEUP_DB, MAX_MAKEUP_DB),
            _ => (0.0, 1.0),
        },
    }
}

#[inline]
fn clamp_wire(index: u32, value: f32) -> f32 {
    let (min, max) = range(index);
    clamp(value, min, max)
}

/// A finite value clamped into `index`'s range, or `fallback` for a NaN or an
/// infinity a hand-edited blob might carry.
fn sane(index: u32, value: f32, fallback: f32) -> f32 {
    clamp_wire(index, if value.is_finite() { value } else { fallback })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompresserState {
    pub version: u32,
    pub params: Params,
}

impl CompresserState {
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

impl Default for CompresserState {
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
    let defaults = default_params();
    params.threshold_db = sane(THRESHOLD_INDEX, params.threshold_db, defaults.threshold_db);
    params.ratio = sane(RATIO_INDEX, params.ratio, defaults.ratio);
    params.attack_ms = sane(ATTACK_INDEX, params.attack_ms, defaults.attack_ms);
    params.release_ms = sane(RELEASE_INDEX, params.release_ms, defaults.release_ms);
    params.makeup_db = sane(MAKEUP_INDEX, params.makeup_db, defaults.makeup_db);
    params.sidechain_hpf_hz = sane(
        SIDECHAIN_INDEX,
        params.sidechain_hpf_hz,
        defaults.sidechain_hpf_hz,
    );
    params.knee_db = sane(KNEE_INDEX, params.knee_db, defaults.knee_db);
    params.mix = sane(MIX_INDEX, params.mix, defaults.mix);
    params.output_db = sane(OUTPUT_INDEX, params.output_db, defaults.output_db);
    for (index, hz) in params.crossover_hz.iter_mut().enumerate() {
        *hz = sane(
            CROSSOVER_1_INDEX + index as u32,
            *hz,
            defaults.crossover_hz[index],
        );
    }
    for (band, (values, fallback)) in params.bands.iter_mut().zip(DEFAULT_BANDS).enumerate() {
        let base = BAND_BASE_INDEX + BAND_STRIDE * band as u32;
        values.threshold_db = sane(
            base + BAND_THRESHOLD,
            values.threshold_db,
            fallback.threshold_db,
        );
        values.ratio = sane(base + BAND_RATIO, values.ratio, fallback.ratio);
        values.attack_ms = sane(base + BAND_ATTACK, values.attack_ms, fallback.attack_ms);
        values.release_ms = sane(base + BAND_RELEASE, values.release_ms, fallback.release_ms);
        values.makeup_db = sane(base + BAND_MAKEUP, values.makeup_db, fallback.makeup_db);
    }
    params.solo_band = params.solo_band.clamp(SOLO_NONE, BAND_COUNT as i32 - 1);
}

/// Apply one compact UI/control update. Allocation-free and total: invalid
/// indices are rejected, non-finite values are rejected, and every continuous
/// value is clamped to its declared range.
pub fn apply_wire_param(params: &mut Params, index: u32, value: f32) -> bool {
    if !value.is_finite() || index as usize >= PARAM_COUNT {
        return false;
    }
    let clamped = clamp_wire(index, value);
    match index {
        POWER_INDEX => params.power = value >= 0.5,
        MODE_INDEX => params.mode = Mode::from_wire(value),
        THRESHOLD_INDEX => params.threshold_db = clamped,
        RATIO_INDEX => params.ratio = clamped,
        ATTACK_INDEX => params.attack_ms = clamped,
        RELEASE_INDEX => params.release_ms = clamped,
        MAKEUP_INDEX => params.makeup_db = clamped,
        SIDECHAIN_INDEX => params.sidechain_hpf_hz = clamped,
        KNEE_INDEX => params.knee_db = clamped,
        MIX_INDEX => params.mix = clamped,
        OUTPUT_INDEX => params.output_db = clamped,
        CROSSOVER_1_INDEX..=CROSSOVER_3_INDEX => {
            params.crossover_hz[(index - CROSSOVER_1_INDEX) as usize] = clamped
        }
        // Rounded rather than truncated so a host stepping it in normalised
        // units cannot land one short.
        SOLO_INDEX => params.solo_band = clamped.round() as i32,
        _ => {
            let Some((band, field)) = band_param(index) else {
                return false;
            };
            let band = &mut params.bands[band];
            match field {
                BAND_THRESHOLD => band.threshold_db = clamped,
                BAND_RATIO => band.ratio = clamped,
                BAND_ATTACK => band.attack_ms = clamped,
                BAND_RELEASE => band.release_ms = clamped,
                BAND_MAKEUP => band.makeup_db = clamped,
                BAND_BYPASS => band.bypass = value >= 0.5,
                _ => return false,
            }
        }
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

/// One parameter's raw wire value, the inverse of [`apply_wire_param`].
pub fn wire_value(params: &Params, index: u32) -> Option<f32> {
    let value = match index {
        POWER_INDEX => f32::from(params.power),
        MODE_INDEX => params.mode.to_wire(),
        THRESHOLD_INDEX => params.threshold_db,
        RATIO_INDEX => params.ratio,
        ATTACK_INDEX => params.attack_ms,
        RELEASE_INDEX => params.release_ms,
        MAKEUP_INDEX => params.makeup_db,
        SIDECHAIN_INDEX => params.sidechain_hpf_hz,
        KNEE_INDEX => params.knee_db,
        MIX_INDEX => params.mix,
        OUTPUT_INDEX => params.output_db,
        CROSSOVER_1_INDEX..=CROSSOVER_3_INDEX => {
            params.crossover_hz[(index - CROSSOVER_1_INDEX) as usize]
        }
        SOLO_INDEX => params.solo_band as f32,
        _ => {
            let (band, field) = band_param(index)?;
            let band = &params.bands[band];
            match field {
                BAND_THRESHOLD => band.threshold_db,
                BAND_RATIO => band.ratio,
                BAND_ATTACK => band.attack_ms,
                BAND_RELEASE => band.release_ms,
                BAND_MAKEUP => band.makeup_db,
                BAND_BYPASS => f32::from(band.bypass),
                _ => return None,
            }
        }
    };
    Some(value)
}

/// Every parameter as `(id, raw value)`, in wire order. Drives project replay
/// and the descriptor-vs-defaults check. Mode leads the band values, so a
/// replay switches mode (which starts the stages from rest) before it sets
/// anything the new mode runs on.
pub fn ui_values(params: &Params) -> Vec<(&'static str, f32)> {
    UI_PARAM_IDS
        .iter()
        .enumerate()
        .filter_map(|(index, id)| wire_value(params, index as u32).map(|value| (*id, value)))
        .collect()
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

    /// The band block is laid out by arithmetic, so pin it against the ids.
    #[test]
    fn band_indices_match_their_ids() {
        for band in 0..BAND_COUNT {
            let n = band + 1;
            let base = BAND_BASE_INDEX + BAND_STRIDE * band as u32;
            for (field, suffix) in [
                (BAND_THRESHOLD, "ThresholdDb"),
                (BAND_RATIO, "Ratio"),
                (BAND_ATTACK, "AttackMs"),
                (BAND_RELEASE, "ReleaseMs"),
                (BAND_MAKEUP, "MakeupDb"),
                (BAND_BYPASS, "Bypass"),
            ] {
                let id = format!("band{n}{suffix}");
                assert_eq!(ui_param_index(&id), Some(base + field), "{id}");
                assert_eq!(band_param(base + field), Some((band, field)));
            }
        }
        assert_eq!(band_param(SOLO_INDEX), None);
        assert_eq!(band_param(CROSSOVER_3_INDEX), None);
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
        assert!(apply_ui_param(&mut params, "mode", 1.0));
        assert!(apply_ui_param(&mut params, "band3Ratio", 6.5));
        assert!(apply_ui_param(&mut params, "band4Bypass", 1.0));
        assert!(apply_ui_param(&mut params, "crossover2Hz", 2_400.0));
        assert!(apply_ui_param(&mut params, "soloBand", 1.0));
        let json = CompresserState::new(params.clone()).to_json().unwrap();
        assert!(json.contains("\"mode\":\"multi\""), "{json}");
        let decoded = CompresserState::from_json(&json).unwrap();
        assert_eq!(decoded.params, params);
        assert_eq!(decoded.version, STATE_VERSION);
    }

    /// A blob written before a field existed loads with that field's default.
    #[test]
    fn a_partial_blob_fills_in_defaults() {
        let decoded =
            CompresserState::from_json(r#"{"version":1,"params":{"ratio":8.0}}"#).unwrap();
        let mut expected = default_params();
        expected.ratio = 8.0;
        assert_eq!(decoded.params, expected);
    }

    #[test]
    fn a_replay_through_the_wire_rebuilds_the_state() {
        let mut saved = default_params();
        saved.power = false;
        saved.mode = Mode::Multi;
        saved.threshold_db = -33.0;
        saved.sidechain_hpf_hz = 150.0;
        saved.knee_db = 12.0;
        saved.mix = 60.0;
        saved.crossover_hz = [300.0, 2_000.0, 9_000.0];
        saved.bands[0].ratio = 2.0;
        saved.bands[1].attack_ms = 3.0;
        saved.bands[2].release_ms = 400.0;
        saved.bands[3].makeup_db = 4.5;
        saved.bands[3].bypass = true;
        saved.solo_band = 2;

        let mut rebuilt = default_params();
        for (id, value) in ui_values(&saved) {
            assert!(
                apply_ui_param(&mut rebuilt, id, value),
                "`{id}` was rejected"
            );
        }
        assert_eq!(rebuilt, saved);
    }

    #[test]
    fn out_of_range_values_are_clamped_not_rejected() {
        let mut params = default_params();
        assert!(apply_ui_param(&mut params, "ratio", 900.0));
        assert_eq!(params.ratio, MAX_RATIO);
        assert!(apply_ui_param(&mut params, "band2ThresholdDb", -500.0));
        assert_eq!(params.bands[1].threshold_db, MIN_THRESHOLD_DB);
        assert!(apply_ui_param(&mut params, "crossover3Hz", 96_000.0));
        assert_eq!(params.crossover_hz[2], MAX_CROSSOVER_HZ);
        assert!(apply_ui_param(&mut params, "soloBand", 7.0));
        assert_eq!(params.solo_band, BAND_COUNT as i32 - 1);
        assert!(apply_ui_param(&mut params, "soloBand", -5.0));
        assert_eq!(params.solo_band, SOLO_NONE);
        assert!(apply_ui_param(&mut params, "soloBand", 1.6));
        assert_eq!(params.solo_band, 2);
        assert!(apply_ui_param(&mut params, "mode", 0.7));
        assert_eq!(params.mode, Mode::Multi);
    }

    #[test]
    fn invalid_and_non_finite_updates_are_rejected() {
        let mut params = default_params();
        assert!(!apply_wire_param(&mut params, u32::MAX, 1.0));
        assert!(!apply_wire_param(&mut params, PARAM_COUNT as u32, 1.0));
        assert!(!apply_wire_param(&mut params, RATIO_INDEX, f32::NAN));
        assert!(!apply_wire_param(&mut params, OUTPUT_INDEX, f32::INFINITY));
        assert!(!apply_ui_param(&mut params, "notAParam", 1.0));
        assert_eq!(params, default_params());
    }

    /// A blob edited by hand must not reach the DSP with values the gain
    /// computer or the coefficient math cannot survive.
    #[test]
    fn sanitize_pulls_a_hand_edited_blob_back_into_range() {
        let mut params = default_params();
        params.ratio = 0.0;
        params.knee_db = f32::NAN;
        params.crossover_hz = [-5.0, f32::NAN, 1.0e9];
        params.bands[0].attack_ms = 0.0;
        params.bands[1].release_ms = f32::INFINITY;
        params.bands[2].makeup_db = 80.0;
        params.solo_band = 42;
        sanitize_params(&mut params);
        assert_eq!(params.ratio, MIN_RATIO);
        assert_eq!(params.knee_db, default_params().knee_db);
        assert_eq!(params.crossover_hz[0], MIN_CROSSOVER_HZ);
        assert_eq!(params.crossover_hz[1], crate::DEFAULT_CROSSOVERS_HZ[1]);
        assert_eq!(params.crossover_hz[2], MAX_CROSSOVER_HZ);
        assert_eq!(params.bands[0].attack_ms, MIN_ATTACK_MS);
        assert_eq!(params.bands[1].release_ms, DEFAULT_BANDS[1].release_ms);
        assert_eq!(params.bands[2].makeup_db, MAX_MAKEUP_DB);
        assert_eq!(params.solo_band, BAND_COUNT as i32 - 1);
    }
}

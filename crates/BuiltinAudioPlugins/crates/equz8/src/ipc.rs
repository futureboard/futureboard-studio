//! Stable editor/DSP parameter wire contract for EQ-Z8.
//!
//! String ids live at the UI/control boundary only. The native host resolves
//! them to compact indices before publishing edits to an audio-side bounded
//! queue; [`Dsp::apply_wire_param`](crate::Dsp::apply_wire_param) consumes the
//! numeric form without allocation, serialization, locking, or string lookup.

use serde::{Deserialize, Serialize};

use crate::{BAND_COUNT, BandType, DynMode, Params, RATIO_LEGACY, clamp, default_params};

pub const PROTOCOL_VERSION: u32 = 1;
pub const STATE_VERSION: u32 = 1;

pub const POWER_INDEX: u32 = 0;
pub const MIX_INDEX: u32 = 1;
pub const OUTPUT_INDEX: u32 = 2;
pub const BAND_BASE_INDEX: u32 = 3;
pub const BAND_STRIDE: u32 = 5;

pub const BAND_ENABLED: u32 = 0;
pub const BAND_TYPE: u32 = 1;
pub const BAND_FREQ: u32 = 2;
pub const BAND_GAIN: u32 = 3;
pub const BAND_Q: u32 = 4;

/// Which band is auditioned in isolation, or [`SOLO_NONE`].
///
/// Deliberately appended *after* the per-band block rather than inserted next
/// to the other globals: band wire indices are derived from
/// [`BAND_BASE_INDEX`], so inserting ahead of them would renumber every band
/// and silently repoint in-flight edits.
pub const SOLO_INDEX: u32 = BAND_BASE_INDEX + BAND_COUNT as u32 * BAND_STRIDE;

/// `solo_band` value meaning "no band is soloed".
pub const SOLO_NONE: i32 = -1;

/// Per-band dynamic EQ wires, appended after solo so the static band block and
/// solo index stay stable for hosts that already resolve those string ids.
pub const DYN_BASE_INDEX: u32 = SOLO_INDEX + 1;
pub const DYN_STRIDE: u32 = 5;

pub const BAND_DYN_ENABLED: u32 = 0;
pub const BAND_DYN_THRESHOLD: u32 = 1;
pub const BAND_DYN_RANGE: u32 = 2;
pub const BAND_DYN_ATTACK: u32 = 3;
pub const BAND_DYN_RELEASE: u32 = 4;

/// Per-band slope, dynamic mode and ratio, appended after the dynamics block
/// for the same reason the dynamics block follows solo: every index before
/// it keeps its meaning. `EXT_BASE_INDEX + band * EXT_STRIDE + field`.
pub const EXT_BASE_INDEX: u32 = DYN_BASE_INDEX + BAND_COUNT as u32 * DYN_STRIDE;
pub const EXT_STRIDE: u32 = 3;

pub const BAND_SLOPE: u32 = 0;
pub const BAND_DYN_MODE: u32 = 1;
pub const BAND_RATIO: u32 = 2;

pub const PARAM_COUNT: usize = EXT_BASE_INDEX as usize + BAND_COUNT * EXT_STRIDE as usize;

// --- value ranges, shared by the wire and restored state --------------------

pub const FREQ_MIN: f32 = 20.0;
pub const FREQ_MAX: f32 = 20_000.0;
pub const GAIN_MIN_DB: f32 = -18.0;
pub const GAIN_MAX_DB: f32 = 18.0;
pub const Q_MIN: f32 = 0.1;
/// Above the shared coefficient builder's 12: the crate designs those
/// filters itself (`design`). ±18 dB at Q 18 still keeps the poles inside
/// the unit circle with margin at every centre the band can reach.
pub const Q_MAX: f32 = 18.0;
pub const THRESHOLD_MIN_DB: f32 = -60.0;
pub const THRESHOLD_MAX_DB: f32 = 0.0;
pub const RANGE_MIN_DB: f32 = -24.0;
pub const RANGE_MAX_DB: f32 = 24.0;
pub const ATTACK_MIN_MS: f32 = 0.1;
pub const ATTACK_MAX_MS: f32 = 500.0;
pub const RELEASE_MIN_MS: f32 = 1.0;
pub const RELEASE_MAX_MS: f32 = 5_000.0;
pub const OUTPUT_MIN_DB: f32 = -24.0;
pub const OUTPUT_MAX_DB: f32 = 18.0;
pub const RATIO_MIN: f32 = 1.0;
pub const RATIO_MAX: f32 = 10.0;

/// The cut slopes, in dB/oct.
pub const SLOPES: [f32; 3] = [12.0, 24.0, 48.0];

/// The supported slope nearest `value`.
pub fn snap_slope(value: f32) -> f32 {
    let mut best = SLOPES[0];
    for slope in SLOPES {
        if (slope - value).abs() < (best - value).abs() {
            best = slope;
        }
    }
    best
}

/// A wire or stored ratio: anything under 0.5 is [`RATIO_LEGACY`] — the
/// value a project saved before the ratio control replays with, which must
/// keep its law rather than be pinned to 1:1 — otherwise 1..10.
pub fn decode_ratio(value: f32) -> f32 {
    if value < 0.5 {
        RATIO_LEGACY
    } else {
        clamp(value, RATIO_MIN, RATIO_MAX)
    }
}

pub const UI_PARAM_IDS: [&str; PARAM_COUNT] = [
    "power",
    "mix",
    "outputDb",
    "band1_enabled",
    "band1_type",
    "band1_freq",
    "band1_gainDb",
    "band1_q",
    "band2_enabled",
    "band2_type",
    "band2_freq",
    "band2_gainDb",
    "band2_q",
    "band3_enabled",
    "band3_type",
    "band3_freq",
    "band3_gainDb",
    "band3_q",
    "band4_enabled",
    "band4_type",
    "band4_freq",
    "band4_gainDb",
    "band4_q",
    "band5_enabled",
    "band5_type",
    "band5_freq",
    "band5_gainDb",
    "band5_q",
    "band6_enabled",
    "band6_type",
    "band6_freq",
    "band6_gainDb",
    "band6_q",
    "band7_enabled",
    "band7_type",
    "band7_freq",
    "band7_gainDb",
    "band7_q",
    "band8_enabled",
    "band8_type",
    "band8_freq",
    "band8_gainDb",
    "band8_q",
    "soloBand",
    "band1_dynEnabled",
    "band1_thresholdDb",
    "band1_rangeDb",
    "band1_attackMs",
    "band1_releaseMs",
    "band2_dynEnabled",
    "band2_thresholdDb",
    "band2_rangeDb",
    "band2_attackMs",
    "band2_releaseMs",
    "band3_dynEnabled",
    "band3_thresholdDb",
    "band3_rangeDb",
    "band3_attackMs",
    "band3_releaseMs",
    "band4_dynEnabled",
    "band4_thresholdDb",
    "band4_rangeDb",
    "band4_attackMs",
    "band4_releaseMs",
    "band5_dynEnabled",
    "band5_thresholdDb",
    "band5_rangeDb",
    "band5_attackMs",
    "band5_releaseMs",
    "band6_dynEnabled",
    "band6_thresholdDb",
    "band6_rangeDb",
    "band6_attackMs",
    "band6_releaseMs",
    "band7_dynEnabled",
    "band7_thresholdDb",
    "band7_rangeDb",
    "band7_attackMs",
    "band7_releaseMs",
    "band8_dynEnabled",
    "band8_thresholdDb",
    "band8_rangeDb",
    "band8_attackMs",
    "band8_releaseMs",
    "band1_slope",
    "band1_dynMode",
    "band1_ratio",
    "band2_slope",
    "band2_dynMode",
    "band2_ratio",
    "band3_slope",
    "band3_dynMode",
    "band3_ratio",
    "band4_slope",
    "band4_dynMode",
    "band4_ratio",
    "band5_slope",
    "band5_dynMode",
    "band5_ratio",
    "band6_slope",
    "band6_dynMode",
    "band6_ratio",
    "band7_slope",
    "band7_dynMode",
    "band7_ratio",
    "band8_slope",
    "band8_dynMode",
    "band8_ratio",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Equz8State {
    pub version: u32,
    pub params: Params,
}

impl Equz8State {
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

impl Default for Equz8State {
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

pub const fn band_wire_index(band: usize, field: u32) -> u32 {
    BAND_BASE_INDEX + band as u32 * BAND_STRIDE + field
}

pub const fn dyn_wire_index(band: usize, field: u32) -> u32 {
    DYN_BASE_INDEX + band as u32 * DYN_STRIDE + field
}

pub fn decode_band_wire(index: u32) -> Option<(usize, u32)> {
    let offset = index.checked_sub(BAND_BASE_INDEX)?;
    if index >= SOLO_INDEX {
        return None;
    }
    let band = (offset / BAND_STRIDE) as usize;
    let field = offset % BAND_STRIDE;
    (band < BAND_COUNT).then_some((band, field))
}

pub fn decode_dyn_wire(index: u32) -> Option<(usize, u32)> {
    let offset = index.checked_sub(DYN_BASE_INDEX)?;
    if index >= EXT_BASE_INDEX {
        return None;
    }
    let band = (offset / DYN_STRIDE) as usize;
    let field = offset % DYN_STRIDE;
    (band < BAND_COUNT).then_some((band, field))
}

pub const fn ext_wire_index(band: usize, field: u32) -> u32 {
    EXT_BASE_INDEX + band as u32 * EXT_STRIDE + field
}

pub fn decode_ext_wire(index: u32) -> Option<(usize, u32)> {
    let offset = index.checked_sub(EXT_BASE_INDEX)?;
    let band = (offset / EXT_STRIDE) as usize;
    let field = offset % EXT_STRIDE;
    (band < BAND_COUNT).then_some((band, field))
}

pub fn sanitize_params(params: &mut Params) {
    params.output_db = clamp(params.output_db, OUTPUT_MIN_DB, OUTPUT_MAX_DB);
    params.mix = clamp(params.mix, 0.0, 100.0);
    if params.solo_band < 0 || params.solo_band >= BAND_COUNT as i32 {
        params.solo_band = SOLO_NONE;
    }
    for band in &mut params.bands {
        band.freq = clamp(band.freq, FREQ_MIN, FREQ_MAX);
        band.gain_db = clamp(band.gain_db, GAIN_MIN_DB, GAIN_MAX_DB);
        band.q = clamp(band.q, Q_MIN, Q_MAX);
        band.threshold_db = clamp(band.threshold_db, THRESHOLD_MIN_DB, THRESHOLD_MAX_DB);
        band.range_db = clamp(band.range_db, RANGE_MIN_DB, RANGE_MAX_DB);
        band.attack_ms = clamp(band.attack_ms, ATTACK_MIN_MS, ATTACK_MAX_MS);
        band.release_ms = clamp(band.release_ms, RELEASE_MIN_MS, RELEASE_MAX_MS);
        band.slope = snap_slope(band.slope);
        band.ratio = decode_ratio(band.ratio);
        if !band.band_type.has_gain() {
            band.dynamic = false;
        }
    }
}

/// Decode a wire `soloBand` value into a validated band index.
///
/// Anything outside `0..BAND_COUNT` clears solo rather than being rejected, so
/// a UI can turn solo off by sending `-1` and can never wedge the DSP into
/// auditioning a band that does not exist.
pub fn decode_solo_band(value: f32) -> i32 {
    let index = value.round() as i32;
    if index < 0 || index >= BAND_COUNT as i32 {
        SOLO_NONE
    } else {
        index
    }
}

/// Apply one compact UI/control update. This is allocation-free and total:
/// invalid indices are rejected and all continuous values are clamped.
pub fn apply_wire_param(params: &mut Params, index: u32, value: f32) -> bool {
    if !value.is_finite() {
        return false;
    }
    match index {
        POWER_INDEX => params.power = value >= 0.5,
        MIX_INDEX => params.mix = clamp(value, 0.0, 100.0),
        OUTPUT_INDEX => params.output_db = clamp(value, OUTPUT_MIN_DB, OUTPUT_MAX_DB),
        SOLO_INDEX => params.solo_band = decode_solo_band(value),
        _ => {
            if let Some((band_index, field)) = decode_band_wire(index) {
                let band = &mut params.bands[band_index];
                match field {
                    BAND_ENABLED => band.active = value >= 0.5,
                    BAND_TYPE => {
                        band.band_type = BandType::from_wire(value);
                        if !band.band_type.has_gain() {
                            band.dynamic = false;
                        }
                    }
                    BAND_FREQ => band.freq = clamp(value, FREQ_MIN, FREQ_MAX),
                    BAND_GAIN => band.gain_db = clamp(value, GAIN_MIN_DB, GAIN_MAX_DB),
                    BAND_Q => band.q = clamp(value, Q_MIN, Q_MAX),
                    _ => return false,
                }
            } else if let Some((band_index, field)) = decode_dyn_wire(index) {
                let band = &mut params.bands[band_index];
                match field {
                    BAND_DYN_ENABLED => {
                        band.dynamic = value >= 0.5 && band.band_type.has_gain();
                    }
                    BAND_DYN_THRESHOLD => {
                        band.threshold_db = clamp(value, THRESHOLD_MIN_DB, THRESHOLD_MAX_DB);
                    }
                    BAND_DYN_RANGE => band.range_db = clamp(value, RANGE_MIN_DB, RANGE_MAX_DB),
                    BAND_DYN_ATTACK => band.attack_ms = clamp(value, ATTACK_MIN_MS, ATTACK_MAX_MS),
                    BAND_DYN_RELEASE => {
                        band.release_ms = clamp(value, RELEASE_MIN_MS, RELEASE_MAX_MS);
                    }
                    _ => return false,
                }
            } else if let Some((band_index, field)) = decode_ext_wire(index) {
                let band = &mut params.bands[band_index];
                match field {
                    BAND_SLOPE => band.slope = snap_slope(value),
                    BAND_DYN_MODE => band.dyn_mode = DynMode::from_wire(value),
                    BAND_RATIO => band.ratio = decode_ratio(value),
                    _ => return false,
                }
            } else {
                return false;
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

pub fn ui_values(params: &Params) -> Vec<(&'static str, f32)> {
    let mut values = Vec::with_capacity(PARAM_COUNT);
    values.push(("power", f32::from(params.power)));
    values.push(("mix", params.mix));
    values.push(("outputDb", params.output_db));
    for (index, band) in params.bands.iter().enumerate() {
        let base = BAND_BASE_INDEX as usize + index * BAND_STRIDE as usize;
        values.push((UI_PARAM_IDS[base], f32::from(band.active)));
        values.push((UI_PARAM_IDS[base + 1], band.band_type.to_wire()));
        values.push((UI_PARAM_IDS[base + 2], band.freq));
        values.push((UI_PARAM_IDS[base + 3], band.gain_db));
        values.push((UI_PARAM_IDS[base + 4], band.q));
    }
    values.push(("soloBand", params.solo_band as f32));
    for (index, band) in params.bands.iter().enumerate() {
        let base = DYN_BASE_INDEX as usize + index * DYN_STRIDE as usize;
        values.push((UI_PARAM_IDS[base], f32::from(band.dynamic)));
        values.push((UI_PARAM_IDS[base + 1], band.threshold_db));
        values.push((UI_PARAM_IDS[base + 2], band.range_db));
        values.push((UI_PARAM_IDS[base + 3], band.attack_ms));
        values.push((UI_PARAM_IDS[base + 4], band.release_ms));
    }
    for (index, band) in params.bands.iter().enumerate() {
        let base = EXT_BASE_INDEX as usize + index * EXT_STRIDE as usize;
        values.push((UI_PARAM_IDS[base], band.slope));
        values.push((UI_PARAM_IDS[base + 1], band.dyn_mode.to_wire()));
        values.push((UI_PARAM_IDS[base + 2], band.ratio));
    }
    values
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
    fn state_round_trips() {
        let mut params = default_params();
        assert!(apply_ui_param(&mut params, "band4_gainDb", -4.5));
        assert!(apply_ui_param(&mut params, "band4_dynEnabled", 1.0));
        assert!(apply_ui_param(&mut params, "band4_rangeDb", -6.0));
        let json = Equz8State::new(params).to_json().unwrap();
        let decoded = Equz8State::from_json(&json).unwrap();
        assert_eq!(decoded.params.bands[3].gain_db, -4.5);
        assert!(decoded.params.bands[3].dynamic);
        assert_eq!(decoded.params.bands[3].range_db, -6.0);
    }

    /// Projects saved before band solo existed have no `soloBand` field. They
    /// must still load, with solo off rather than failing to deserialize.
    #[test]
    fn state_without_solo_band_still_loads() {
        let json = Equz8State::new(default_params()).to_json().unwrap();
        let stripped = json
            .replace(",\"soloBand\":-1", "")
            .replace("\"soloBand\":-1,", "");
        assert!(
            !stripped.contains("soloBand"),
            "the field should be gone for this test to mean anything"
        );
        let decoded = Equz8State::from_json(&stripped).expect("legacy state must load");
        assert_eq!(decoded.params.solo_band, SOLO_NONE);
    }

    /// Adding solo must not have renumbered the band block.
    #[test]
    fn solo_is_appended_after_the_band_block() {
        assert_eq!(BAND_BASE_INDEX, 3, "globals still occupy 0..3");
        assert_eq!(band_wire_index(0, BAND_ENABLED), 3);
        assert_eq!(band_wire_index(BAND_COUNT - 1, BAND_Q), SOLO_INDEX - 1);
        assert_eq!(ui_param_index("soloBand"), Some(SOLO_INDEX));
    }

    #[test]
    fn dynamic_wires_follow_solo() {
        assert_eq!(DYN_BASE_INDEX, SOLO_INDEX + 1);
        assert_eq!(dyn_wire_index(0, BAND_DYN_ENABLED), DYN_BASE_INDEX);
        assert_eq!(ui_param_index("band1_dynEnabled"), Some(DYN_BASE_INDEX));
        assert_eq!(
            ui_param_index("band8_releaseMs"),
            Some(DYN_BASE_INDEX + 8 * DYN_STRIDE - 1)
        );
    }

    #[test]
    fn solo_index_is_validated_not_trusted() {
        let mut params = default_params();
        assert!(apply_wire_param(&mut params, SOLO_INDEX, 3.0));
        assert_eq!(params.solo_band, 3);
        for bogus in [-5.0, BAND_COUNT as f32, 999.0] {
            assert!(apply_wire_param(&mut params, SOLO_INDEX, bogus));
            assert_eq!(params.solo_band, SOLO_NONE, "{bogus} should clear solo");
        }
    }

    /// The indices a host already resolves keep their meaning: the new
    /// per-band fields only extend the table.
    #[test]
    fn existing_wire_indices_are_unchanged() {
        for (index, id) in [
            (0, "power"),
            (1, "mix"),
            (2, "outputDb"),
            (3, "band1_enabled"),
            (6, "band1_gainDb"),
            (42, "band8_q"),
            (43, "soloBand"),
            (44, "band1_dynEnabled"),
            (63, "band4_releaseMs"),
            (83, "band8_releaseMs"),
        ] {
            assert_eq!(ui_param_id(index), Some(id), "{index}");
        }
        assert_eq!(EXT_BASE_INDEX, 84);
        assert_eq!(ui_param_index("band1_slope"), Some(84));
        assert_eq!(
            ui_param_index("band8_ratio"),
            Some(ext_wire_index(BAND_COUNT - 1, BAND_RATIO))
        );
        assert_eq!(PARAM_COUNT, 108);
        for index in 0..PARAM_COUNT as u32 {
            let decoded = [
                decode_band_wire(index).is_some(),
                decode_dyn_wire(index).is_some(),
                decode_ext_wire(index).is_some(),
            ];
            assert!(decoded.iter().filter(|hit| **hit).count() <= 1, "{index}");
        }
    }

    /// A ratio of 0 is the legacy law and must survive a replay over the wire.
    #[test]
    fn the_legacy_ratio_survives_the_wire() {
        let mut params = default_params();
        assert!(apply_wire_param(
            &mut params,
            ext_wire_index(2, BAND_RATIO),
            0.0
        ));
        assert_eq!(params.bands[2].ratio, RATIO_LEGACY);
        assert!(apply_wire_param(
            &mut params,
            ext_wire_index(2, BAND_RATIO),
            40.0
        ));
        assert_eq!(params.bands[2].ratio, RATIO_MAX);
        assert!(apply_wire_param(
            &mut params,
            ext_wire_index(2, BAND_SLOPE),
            30.0
        ));
        assert_eq!(params.bands[2].slope, 24.0);
        assert!(apply_wire_param(
            &mut params,
            ext_wire_index(2, BAND_DYN_MODE),
            1.0
        ));
        assert_eq!(params.bands[2].dyn_mode, DynMode::Below);
    }

    #[test]
    fn invalid_and_non_finite_updates_are_rejected() {
        let mut params = default_params();
        assert!(!apply_wire_param(&mut params, u32::MAX, 1.0));
        assert!(!apply_wire_param(&mut params, BAND_BASE_INDEX, f32::NAN));
    }
}

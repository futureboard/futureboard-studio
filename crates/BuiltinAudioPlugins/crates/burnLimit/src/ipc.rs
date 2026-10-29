//! Stable editor/DSP parameter wire contract for BurnLimit.
//!
//! String ids live at the UI/control boundary only. The native host resolves
//! them to compact indices before publishing edits to an audio-side bounded
//! queue; [`Dsp::apply_wire_param`](crate::Dsp::apply_wire_param) consumes the
//! numeric form without allocation, serialization, locking, or string lookup.

use serde::{Deserialize, Serialize};

use crate::{Dither, Oversampling, Params, Style, clamp, default_params};

pub const PROTOCOL_VERSION: u32 = 1;
pub const STATE_VERSION: u32 = 1;

pub const POWER_INDEX: u32 = 0;
pub const STYLE_INDEX: u32 = 1;
pub const GAIN_INDEX: u32 = 2;
pub const CEILING_INDEX: u32 = 3;
pub const RELEASE_INDEX: u32 = 4;
pub const LOOKAHEAD_INDEX: u32 = 5;
pub const TRUE_PEAK_INDEX: u32 = 6;
pub const MIX_INDEX: u32 = 7;
pub const STEREO_LINK_INDEX: u32 = 8;
// Appended after the original nine; never reorder.
pub const KNEE_INDEX: u32 = 9;
pub const LINK_PCT_INDEX: u32 = 10;
pub const UNITY_INDEX: u32 = 11;
pub const DITHER_INDEX: u32 = 12;
pub const OVERSAMPLING_INDEX: u32 = 13;
pub const TARGET_LUFS_INDEX: u32 = 14;
/// A trigger: any new value resets the program loudness readings.
pub const METER_RESET_INDEX: u32 = 15;

pub const PARAM_COUNT: usize = 16;

pub const UI_PARAM_IDS: [&str; PARAM_COUNT] = [
    "power",
    "style",
    "gainDb",
    "ceilingDb",
    "releaseMs",
    "lookaheadMs",
    "truePeak",
    "mix",
    "stereoLink",
    "kneeDb",
    "linkPct",
    "unity",
    "dither",
    "oversampling",
    "targetLufs",
    "meterReset",
];

const RANGES: [(f32, f32); PARAM_COUNT] = [
    (0.0, 0.0),     // power
    (0.0, 0.0),     // style
    (-12.0, 24.0),  // gainDb
    (-12.0, 0.0),   // ceilingDb
    (1.0, 2_000.0), // releaseMs
    (0.0, 10.0),    // lookaheadMs
    (0.0, 0.0),     // truePeak
    (0.0, 100.0),   // mix
    (0.0, 0.0),     // stereoLink
    (0.0, 12.0),    // kneeDb
    (0.0, 100.0),   // linkPct
    (0.0, 0.0),     // unity
    (0.0, 0.0),     // dither
    (0.0, 0.0),     // oversampling
    (-36.0, 0.0),   // targetLufs
    (0.0, 0.0),     // meterReset
];

#[inline]
fn clamp_wire(index: u32, value: f32) -> f32 {
    let (min, max) = RANGES[index as usize];
    clamp(value, min, max)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BurnLimitState {
    pub version: u32,
    pub params: Params,
}

impl BurnLimitState {
    pub fn new(params: Params) -> Self {
        Self {
            version: STATE_VERSION,
            params,
        }
    }

    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// Parses a saved state. State written before the link amount existed
    /// gets it from its on/off link here, so a decoded `Params` always
    /// carries a real `link_pct`.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        let mut state: Self = serde_json::from_str(json)?;
        resolve_link(&mut state.params);
        Ok(state)
    }
}

impl Default for BurnLimitState {
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

/// A `link_pct` the state did not carry ([`crate::LINK_UNSET`], or anything
/// unusable) comes from the on/off link: on is 100 %, off 0 %.
pub fn resolve_link(params: &mut Params) {
    if !params.link_pct.is_finite() || params.link_pct < 0.0 {
        params.link_pct = if params.stereo_link { 100.0 } else { 0.0 };
    }
}

pub fn sanitize_params(params: &mut Params) {
    resolve_link(params);
    params.gain_db = clamp_wire(GAIN_INDEX, params.gain_db);
    params.ceiling_db = clamp_wire(CEILING_INDEX, params.ceiling_db);
    params.release_ms = clamp_wire(RELEASE_INDEX, params.release_ms);
    params.lookahead_ms = clamp_wire(LOOKAHEAD_INDEX, params.lookahead_ms);
    params.mix = clamp_wire(MIX_INDEX, params.mix);
    params.knee_db = clamp_wire(KNEE_INDEX, params.knee_db);
    params.link_pct = clamp_wire(LINK_PCT_INDEX, params.link_pct);
    params.stereo_link = params.link_pct > 0.0;
    params.target_lufs = if params.target_lufs.is_finite() {
        clamp_wire(TARGET_LUFS_INDEX, params.target_lufs)
    } else {
        -14.0
    };
    if !params.meter_reset.is_finite() {
        params.meter_reset = 0.0;
    }
}

pub fn apply_wire_param(params: &mut Params, index: u32, value: f32) -> bool {
    if !value.is_finite() || index as usize >= PARAM_COUNT {
        return false;
    }
    match index {
        POWER_INDEX => params.power = value >= 0.5,
        STYLE_INDEX => params.style = Style::from_wire(value),
        GAIN_INDEX => params.gain_db = clamp_wire(index, value),
        CEILING_INDEX => params.ceiling_db = clamp_wire(index, value),
        RELEASE_INDEX => params.release_ms = clamp_wire(index, value),
        LOOKAHEAD_INDEX => params.lookahead_ms = clamp_wire(index, value),
        TRUE_PEAK_INDEX => params.true_peak = value >= 0.5,
        MIX_INDEX => params.mix = clamp_wire(index, value),
        STEREO_LINK_INDEX => {
            params.stereo_link = value >= 0.5;
            params.link_pct = if params.stereo_link { 100.0 } else { 0.0 };
        }
        KNEE_INDEX => params.knee_db = clamp_wire(index, value),
        LINK_PCT_INDEX => {
            params.link_pct = clamp_wire(index, value);
            params.stereo_link = params.link_pct > 0.0;
        }
        UNITY_INDEX => params.unity = value >= 0.5,
        DITHER_INDEX => params.dither = Dither::from_wire(value),
        OVERSAMPLING_INDEX => params.oversampling = Oversampling::from_wire(value),
        TARGET_LUFS_INDEX => params.target_lufs = clamp_wire(index, value),
        METER_RESET_INDEX => params.meter_reset = value,
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
        ("style", params.style.to_wire()),
        ("gainDb", params.gain_db),
        ("ceilingDb", params.ceiling_db),
        ("releaseMs", params.release_ms),
        ("lookaheadMs", params.lookahead_ms),
        ("truePeak", f32::from(params.true_peak)),
        ("mix", params.mix),
        ("stereoLink", f32::from(params.stereo_link)),
        ("kneeDb", params.knee_db),
        ("linkPct", params.link_pct),
        ("unity", f32::from(params.unity)),
        ("dither", params.dither.to_wire()),
        ("oversampling", params.oversampling.to_wire()),
        ("targetLufs", params.target_lufs),
        ("meterReset", params.meter_reset),
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
        assert!(apply_ui_param(&mut params, "gainDb", 6.0));
        assert!(apply_ui_param(&mut params, "style", Style::Clip.to_wire()));
        let json = BurnLimitState::new(params).to_json().unwrap();
        let decoded = BurnLimitState::from_json(&json).unwrap();
        assert_eq!(decoded.params.gain_db, 6.0);
        assert_eq!(decoded.params.style, Style::Clip);
        assert_eq!(decoded.version, STATE_VERSION);
    }

    #[test]
    fn out_of_range_values_are_clamped_not_rejected() {
        let mut params = default_params();
        assert!(apply_ui_param(&mut params, "gainDb", 900.0));
        assert_eq!(params.gain_db, 24.0);
        assert!(apply_ui_param(&mut params, "ceilingDb", 5.0));
        assert_eq!(params.ceiling_db, 0.0);
    }

    #[test]
    fn invalid_and_non_finite_updates_are_rejected() {
        let mut params = default_params();
        assert!(!apply_wire_param(&mut params, u32::MAX, 1.0));
        assert!(!apply_wire_param(&mut params, PARAM_COUNT as u32, 1.0));
        assert!(!apply_wire_param(&mut params, GAIN_INDEX, f32::NAN));
        assert!(!apply_ui_param(&mut params, "notAParam", 1.0));
    }

    #[test]
    fn style_wire_values_round_trip() {
        for style in Style::UI_ORDER {
            assert_eq!(Style::from_wire(style.to_wire()), style);
            assert_eq!(Style::parse(style.as_str()), Some(style));
        }
        // The original four keep their wire values.
        assert_eq!(Style::Clean.to_wire(), 0.0);
        assert_eq!(Style::Punch.to_wire(), 1.0);
        assert_eq!(Style::Modern.to_wire(), 2.0);
        assert_eq!(Style::Clip.to_wire(), 3.0);
        assert_eq!(Style::Clip.label(), "Burn");
        for value in Dither::ALL {
            assert_eq!(Dither::from_wire(value.to_wire()), value);
        }
        for value in Oversampling::ALL {
            assert_eq!(Oversampling::from_wire(value.to_wire()), value);
        }
    }

    /// The wire table only ever grows at the end: an old project's
    /// automation and the host's replay still mean what they meant.
    #[test]
    fn the_wire_table_is_append_only() {
        let original = [
            "power",
            "style",
            "gainDb",
            "ceilingDb",
            "releaseMs",
            "lookaheadMs",
            "truePeak",
            "mix",
            "stereoLink",
        ];
        assert_eq!(&UI_PARAM_IDS[..original.len()], &original);
        assert_eq!(STEREO_LINK_INDEX, 8);
        assert_eq!(METER_RESET_INDEX as usize, PARAM_COUNT - 1);
    }

    /// A state saved before the new controls existed loads as it played.
    #[test]
    fn an_old_state_loads_with_its_link_and_neutral_new_controls() {
        for (link, pct) in [(true, 100.0), (false, 0.0)] {
            let json = format!(
                r#"{{"version":1,"params":{{"power":true,"style":"punch","gainDb":6.0,"ceilingDb":-1.0,"releaseMs":150.0,"lookaheadMs":3.0,"truePeak":true,"mix":100.0,"stereoLink":{link}}}}}"#
            );
            let params = BurnLimitState::from_json(&json).unwrap().params;
            assert_eq!(params.link_pct, pct);
            assert_eq!(params.stereo_link, link);
            assert_eq!(params.knee_db, 0.0);
            assert!(!params.unity);
            assert_eq!(params.dither, Dither::Off);
            assert_eq!(params.oversampling, Oversampling::Off);
            assert_eq!(params.target_lufs, -14.0);
        }
        // Parsed without the fix-up, the amount is the unset sentinel.
        let raw: Params = serde_json::from_str(
            r#"{"power":true,"style":"modern","gainDb":0.0,"ceilingDb":-0.3,"releaseMs":200.0,"lookaheadMs":2.0,"truePeak":true,"mix":100.0,"stereoLink":true}"#,
        )
        .unwrap();
        assert_eq!(raw.link_pct, crate::LINK_UNSET);
        // The new fields round-trip; the reset trigger is never saved.
        let mut params = default_params();
        params.link_pct = 40.0;
        params.oversampling = Oversampling::X4;
        params.dither = Dither::Bits16;
        params.meter_reset = 3.0;
        let json = BurnLimitState::new(params).to_json().unwrap();
        assert!(!json.contains("meterReset"));
        let decoded = BurnLimitState::from_json(&json).unwrap().params;
        assert_eq!(decoded.link_pct, 40.0);
        assert_eq!(decoded.oversampling, Oversampling::X4);
        assert_eq!(decoded.dither, Dither::Bits16);
        assert_eq!(decoded.meter_reset, 0.0);
    }

    #[test]
    fn the_two_link_controls_stay_in_step() {
        let mut params = default_params();
        assert!(apply_ui_param(&mut params, "linkPct", 30.0));
        assert!(params.stereo_link);
        assert!(apply_ui_param(&mut params, "linkPct", 0.0));
        assert!(!params.stereo_link);
        assert!(apply_ui_param(&mut params, "stereoLink", 1.0));
        assert_eq!(params.link_pct, 100.0);
        assert!(apply_ui_param(&mut params, "stereoLink", 0.0));
        assert_eq!(params.link_pct, 0.0);
    }
}

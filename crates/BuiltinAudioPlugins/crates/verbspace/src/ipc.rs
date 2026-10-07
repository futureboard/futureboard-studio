//! Stable editor/DSP parameter wire contract for VerbSpace.
//!
//! String ids live at the UI/control boundary only. The native host resolves
//! them to compact indices before publishing edits to an audio-side bounded
//! queue; [`Dsp::apply_wire_param`](crate::Dsp::apply_wire_param) consumes the
//! numeric form without allocation, serialization, locking, or string lookup.
//!
//! Version 2 (the rebuilt DSP) appended `bassFreqHz`, `dampFreqHz`,
//! `earlyLate` and `wetOnly`, widened `size` to 0–100 and `bassMult` to
//! 0.2–3, and turned `mode` into a label the DSP no longer reads. A version-1
//! blob still loads: the appended fields take their defaults.

use serde::{Deserialize, Serialize};

use crate::{MAX_PREDELAY_MS, Params, ReverbMode, clamp, default_params};

pub const PROTOCOL_VERSION: u32 = 2;
pub const STATE_VERSION: u32 = 2;

pub const POWER_INDEX: u32 = 0;
pub const MODE_INDEX: u32 = 1;
pub const PREDELAY_INDEX: u32 = 2;
pub const SIZE_INDEX: u32 = 3;
pub const DECAY_INDEX: u32 = 4;
pub const DIFFUSION_INDEX: u32 = 5;
pub const DAMPING_INDEX: u32 = 6;
pub const BASS_INDEX: u32 = 7;
pub const MOD_DEPTH_INDEX: u32 = 8;
pub const MOD_RATE_INDEX: u32 = 9;
pub const LOW_CUT_INDEX: u32 = 10;
pub const HIGH_CUT_INDEX: u32 = 11;
pub const WIDTH_INDEX: u32 = 12;
pub const MIX_INDEX: u32 = 13;
pub const OUTPUT_INDEX: u32 = 14;
pub const FREEZE_INDEX: u32 = 15;
pub const BASS_FREQ_INDEX: u32 = 16;
pub const DAMP_FREQ_INDEX: u32 = 17;
pub const EARLY_LATE_INDEX: u32 = 18;
pub const WET_ONLY_INDEX: u32 = 19;

pub const PARAM_COUNT: usize = 20;

/// Wire index *is* the position in this table; the editor and the host both
/// resolve through it, so the order is part of the persisted contract. Append
/// only.
pub const UI_PARAM_IDS: [&str; PARAM_COUNT] = [
    "power",
    "mode",
    "predelayMs",
    "size",
    "decaySec",
    "diffusion",
    "damping",
    "bassMult",
    "modDepth",
    "modRateHz",
    "lowCutHz",
    "highCutHz",
    "width",
    "mix",
    "outputDb",
    "freeze",
    "bassFreqHz",
    "dampFreqHz",
    "earlyLate",
    "wetOnly",
];

/// Inclusive `(min, max)` for every continuous parameter, indexed by wire
/// index. Booleans and the mode enum carry `(0, 0)` and are handled by their
/// own arms in [`apply_wire_param`]. Single source of truth for clamping, so
/// [`sanitize_params`] and the wire path cannot drift apart.
const RANGES: [(f32, f32); PARAM_COUNT] = [
    (0.0, 0.0),             // power
    (0.0, 0.0),             // mode
    (0.0, MAX_PREDELAY_MS), // predelayMs
    (0.0, 100.0),           // size
    (0.1, 20.0),            // decaySec
    (0.0, 100.0),           // diffusion
    (0.0, 100.0),           // damping
    (0.2, 3.0),             // bassMult
    (0.0, 100.0),           // modDepth
    (0.05, 5.0),            // modRateHz
    (20.0, 1_000.0),        // lowCutHz
    (1_000.0, 20_000.0),    // highCutHz
    (0.0, 200.0),           // width
    (0.0, 100.0),           // mix
    (-24.0, 12.0),          // outputDb
    (0.0, 0.0),             // freeze
    (50.0, 1_000.0),        // bassFreqHz
    (1_000.0, 16_000.0),    // dampFreqHz
    (0.0, 100.0),           // earlyLate
    (0.0, 0.0),             // wetOnly
];

#[inline]
fn clamp_wire(index: u32, value: f32) -> f32 {
    let (min, max) = RANGES[index as usize];
    clamp(value, min, max)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerbspaceState {
    pub version: u32,
    pub params: Params,
}

impl VerbspaceState {
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

impl Default for VerbspaceState {
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
    params.predelay_ms = clamp_wire(PREDELAY_INDEX, params.predelay_ms);
    params.size = clamp_wire(SIZE_INDEX, params.size);
    params.decay_sec = clamp_wire(DECAY_INDEX, params.decay_sec);
    params.diffusion = clamp_wire(DIFFUSION_INDEX, params.diffusion);
    params.damping = clamp_wire(DAMPING_INDEX, params.damping);
    params.bass_mult = clamp_wire(BASS_INDEX, params.bass_mult);
    params.mod_depth = clamp_wire(MOD_DEPTH_INDEX, params.mod_depth);
    params.mod_rate_hz = clamp_wire(MOD_RATE_INDEX, params.mod_rate_hz);
    params.low_cut_hz = clamp_wire(LOW_CUT_INDEX, params.low_cut_hz);
    params.high_cut_hz = clamp_wire(HIGH_CUT_INDEX, params.high_cut_hz);
    params.width = clamp_wire(WIDTH_INDEX, params.width);
    params.mix = clamp_wire(MIX_INDEX, params.mix);
    params.output_db = clamp_wire(OUTPUT_INDEX, params.output_db);
    params.bass_freq_hz = clamp_wire(BASS_FREQ_INDEX, params.bass_freq_hz);
    params.damp_freq_hz = clamp_wire(DAMP_FREQ_INDEX, params.damp_freq_hz);
    params.early_late = clamp_wire(EARLY_LATE_INDEX, params.early_late);
}

/// Apply one compact UI/control update. Allocation-free and total: invalid
/// indices are rejected, non-finite values are rejected, and every continuous
/// value is clamped to its declared range.
///
/// `mode` only stores the label: loading a type's space is the editor's
/// explicit act ([`ReverbMode::starting_point`]), sent as ordinary edits, so
/// replaying a project or a preset diff never has a hidden side effect.
pub fn apply_wire_param(params: &mut Params, index: u32, value: f32) -> bool {
    if !value.is_finite() || index as usize >= PARAM_COUNT {
        return false;
    }
    match index {
        POWER_INDEX => params.power = value >= 0.5,
        MODE_INDEX => params.mode = ReverbMode::from_wire(value),
        FREEZE_INDEX => params.freeze = value >= 0.5,
        WET_ONLY_INDEX => params.wet_only = value >= 0.5,
        PREDELAY_INDEX => params.predelay_ms = clamp_wire(index, value),
        SIZE_INDEX => params.size = clamp_wire(index, value),
        DECAY_INDEX => params.decay_sec = clamp_wire(index, value),
        DIFFUSION_INDEX => params.diffusion = clamp_wire(index, value),
        DAMPING_INDEX => params.damping = clamp_wire(index, value),
        BASS_INDEX => params.bass_mult = clamp_wire(index, value),
        MOD_DEPTH_INDEX => params.mod_depth = clamp_wire(index, value),
        MOD_RATE_INDEX => params.mod_rate_hz = clamp_wire(index, value),
        LOW_CUT_INDEX => params.low_cut_hz = clamp_wire(index, value),
        HIGH_CUT_INDEX => params.high_cut_hz = clamp_wire(index, value),
        WIDTH_INDEX => params.width = clamp_wire(index, value),
        MIX_INDEX => params.mix = clamp_wire(index, value),
        OUTPUT_INDEX => params.output_db = clamp_wire(index, value),
        BASS_FREQ_INDEX => params.bass_freq_hz = clamp_wire(index, value),
        DAMP_FREQ_INDEX => params.damp_freq_hz = clamp_wire(index, value),
        EARLY_LATE_INDEX => params.early_late = clamp_wire(index, value),
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
        ("predelayMs", params.predelay_ms),
        ("size", params.size),
        ("decaySec", params.decay_sec),
        ("diffusion", params.diffusion),
        ("damping", params.damping),
        ("bassMult", params.bass_mult),
        ("modDepth", params.mod_depth),
        ("modRateHz", params.mod_rate_hz),
        ("lowCutHz", params.low_cut_hz),
        ("highCutHz", params.high_cut_hz),
        ("width", params.width),
        ("mix", params.mix),
        ("outputDb", params.output_db),
        ("freeze", f32::from(params.freeze)),
        ("bassFreqHz", params.bass_freq_hz),
        ("dampFreqHz", params.damp_freq_hz),
        ("earlyLate", params.early_late),
        ("wetOnly", f32::from(params.wet_only)),
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

    /// The first sixteen ids are persisted in version-1 projects and
    /// automation: they must never move.
    #[test]
    fn version_one_wire_indices_are_unchanged() {
        let v1 = [
            "power",
            "mode",
            "predelayMs",
            "size",
            "decaySec",
            "diffusion",
            "damping",
            "bassMult",
            "modDepth",
            "modRateHz",
            "lowCutHz",
            "highCutHz",
            "width",
            "mix",
            "outputDb",
            "freeze",
        ];
        for (index, id) in v1.iter().enumerate() {
            assert_eq!(ui_param_index(id), Some(index as u32), "{id}");
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
        assert!(apply_ui_param(&mut params, "decaySec", 7.5));
        assert!(apply_ui_param(&mut params, "earlyLate", 12.0));
        assert!(apply_ui_param(
            &mut params,
            "mode",
            ReverbMode::Plate.to_wire()
        ));
        let json = VerbspaceState::new(params).to_json().unwrap();
        let decoded = VerbspaceState::from_json(&json).unwrap();
        assert_eq!(decoded.params.decay_sec, 7.5);
        assert_eq!(decoded.params.early_late, 12.0);
        assert_eq!(decoded.params.mode, ReverbMode::Plate);
        assert_eq!(decoded.version, STATE_VERSION);
    }

    /// A project saved before the rebuild has none of the appended fields:
    /// it loads, keeps every value it had, and the new ones take defaults.
    #[test]
    fn a_version_one_blob_still_loads() {
        let json = r#"{"version":1,"params":{"power":true,"mode":"room","predelayMs":8.0,
            "size":32.0,"decaySec":0.55,"diffusion":58.0,"damping":55.0,"bassMult":0.95,
            "modDepth":12.0,"modRateHz":0.45,"lowCutHz":120.0,"highCutHz":11000.0,
            "width":95.0,"mix":22.0,"outputDb":-1.5,"freeze":false}}"#;
        let state = VerbspaceState::from_json(json).unwrap();
        assert_eq!(state.version, 1);
        assert_eq!(state.params.mode, ReverbMode::Room);
        assert_eq!(state.params.size, 32.0);
        assert_eq!(state.params.decay_sec, 0.55);
        assert_eq!(state.params.output_db, -1.5);
        let defaults = default_params();
        assert_eq!(state.params.bass_freq_hz, defaults.bass_freq_hz);
        assert_eq!(state.params.damp_freq_hz, defaults.damp_freq_hz);
        assert_eq!(state.params.early_late, defaults.early_late);
        assert!(!state.params.wet_only);
    }

    #[test]
    fn mode_is_a_label_on_the_wire() {
        let mut params = default_params();
        let before = params.clone();
        assert!(apply_ui_param(
            &mut params,
            "mode",
            ReverbMode::Room.to_wire()
        ));
        assert_eq!(params.mode, ReverbMode::Room);
        assert_eq!(
            Params {
                mode: before.mode,
                ..params
            },
            before,
            "a mode edit moved another param"
        );
    }

    #[test]
    fn out_of_range_values_are_clamped_not_rejected() {
        let mut params = default_params();
        assert!(apply_ui_param(&mut params, "decaySec", 1_000.0));
        assert_eq!(params.decay_sec, 20.0);
        assert!(apply_ui_param(&mut params, "mix", -5.0));
        assert_eq!(params.mix, 0.0);
        assert!(apply_ui_param(&mut params, "dampFreqHz", 40_000.0));
        assert_eq!(params.damp_freq_hz, 16_000.0);
    }

    #[test]
    fn invalid_and_non_finite_updates_are_rejected() {
        let mut params = default_params();
        assert!(!apply_wire_param(&mut params, u32::MAX, 1.0));
        assert!(!apply_wire_param(&mut params, PARAM_COUNT as u32, 1.0));
        assert!(!apply_wire_param(&mut params, DECAY_INDEX, f32::NAN));
        assert!(!apply_wire_param(&mut params, DECAY_INDEX, f32::INFINITY));
        assert!(!apply_ui_param(&mut params, "notAParam", 1.0));
    }

    /// A blob written by an older build (or edited by hand) must not reach the
    /// DSP with values the coefficient math cannot survive.
    #[test]
    fn sanitize_pulls_a_hand_edited_blob_back_into_range() {
        let mut params = default_params();
        params.decay_sec = -3.0;
        params.size = 900.0;
        params.high_cut_hz = 96_000.0;
        params.bass_mult = 40.0;
        params.bass_freq_hz = 0.0;
        sanitize_params(&mut params);
        assert_eq!(params.decay_sec, 0.1);
        assert_eq!(params.size, 100.0);
        assert_eq!(params.high_cut_hz, 20_000.0);
        assert_eq!(params.bass_mult, 3.0);
        assert_eq!(params.bass_freq_hz, 50.0);
    }

    #[test]
    fn mode_wire_values_round_trip() {
        for mode in ReverbMode::ALL {
            assert_eq!(ReverbMode::from_wire(mode.to_wire()), mode);
            assert_eq!(ReverbMode::parse(mode.as_str()), Some(mode));
        }
    }
}

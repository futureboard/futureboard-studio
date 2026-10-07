//! Stable editor/DSP parameter wire contract for EchoSpace.
//!
//! String ids live at the UI/control boundary only. The native host resolves
//! them to compact indices before publishing edits to an audio-side bounded
//! queue; [`Dsp::apply_wire_param`](crate::Dsp::apply_wire_param) consumes the
//! numeric form without allocation, serialization, locking, or string lookup.

use serde::{Deserialize, Serialize};

use crate::{
    DelayMode, MAX_DELAY_MS, MAX_DIVISION_WIRE, MAX_MOD_RATE_HZ, Params, clamp, default_params,
};

pub const PROTOCOL_VERSION: u32 = 1;
pub const STATE_VERSION: u32 = 1;

pub const POWER_INDEX: u32 = 0;
pub const MODE_INDEX: u32 = 1;
pub const TIME_L_INDEX: u32 = 2;
pub const TIME_R_INDEX: u32 = 3;
pub const FEEDBACK_INDEX: u32 = 4;
pub const CROSS_INDEX: u32 = 5;
pub const LOW_CUT_INDEX: u32 = 6;
pub const HIGH_CUT_INDEX: u32 = 7;
pub const SATURATION_INDEX: u32 = 8;
pub const MIX_INDEX: u32 = 9;
pub const OUTPUT_INDEX: u32 = 10;
pub const FREEZE_INDEX: u32 = 11;
pub const SYNC_INDEX: u32 = 12;
pub const DIVISION_L_INDEX: u32 = 13;
pub const DIVISION_R_INDEX: u32 = 14;
pub const LINK_INDEX: u32 = 15;
pub const MOD_DEPTH_INDEX: u32 = 16;
pub const MOD_RATE_INDEX: u32 = 17;
pub const DUCK_INDEX: u32 = 18;
pub const DIFFUSION_INDEX: u32 = 19;
pub const WIDTH_INDEX: u32 = 20;

pub const PARAM_COUNT: usize = 21;

/// Wire index *is* the position in this table; the editor and the host both
/// resolve through it, so the order is part of the persisted contract. Append
/// only.
pub const UI_PARAM_IDS: [&str; PARAM_COUNT] = [
    "power",
    "mode",
    "timeMsL",
    "timeMsR",
    "feedback",
    "crossFeedback",
    "lowCutHz",
    "highCutHz",
    "saturation",
    "mix",
    "outputDb",
    "freeze",
    "sync",
    "divisionL",
    "divisionR",
    "link",
    "modDepth",
    "modRateHz",
    "duck",
    "diffusion",
    "width",
];

/// Inclusive `(min, max)` for every continuous parameter, indexed by wire
/// index. Booleans and the mode enum carry `(0, 0)` and are handled by their
/// own arms in [`apply_wire_param`]. Single source of truth for clamping, so
/// [`sanitize_params`] and the wire path cannot drift apart.
const RANGES: [(f32, f32); PARAM_COUNT] = [
    (0.0, 0.0),               // power
    (0.0, 0.0),               // mode
    (1.0, MAX_DELAY_MS),      // timeMsL
    (1.0, MAX_DELAY_MS),      // timeMsR
    (0.0, 98.0),              // feedback
    (0.0, 100.0),             // crossFeedback
    (20.0, 2_000.0),          // lowCutHz
    (1_000.0, 20_000.0),      // highCutHz
    (0.0, 100.0),             // saturation
    (0.0, 100.0),             // mix
    (-24.0, 12.0),            // outputDb
    (0.0, 0.0),               // freeze
    (0.0, 0.0),               // sync
    (0.0, MAX_DIVISION_WIRE), // divisionL
    (0.0, MAX_DIVISION_WIRE), // divisionR
    (0.0, 0.0),               // link
    (0.0, 100.0),             // modDepth
    (0.05, MAX_MOD_RATE_HZ),  // modRateHz
    (0.0, 100.0),             // duck
    (0.0, 100.0),             // diffusion
    (0.0, 200.0),             // width
];

#[inline]
fn clamp_wire(index: u32, value: f32) -> f32 {
    let (min, max) = RANGES[index as usize];
    clamp(value, min, max)
}

/// A division travels as its table index. Rounded rather than truncated so a
/// host that steps the control in normalised units cannot land one entry short.
#[inline]
fn wire_to_division(value: f32) -> u8 {
    clamp(value, 0.0, MAX_DIVISION_WIRE).round() as u8
}

/// `link` is a promise that the two sides agree, so enabling it — or restoring
/// it from a blob — pulls the right side onto the left. Every path that can set
/// the flag goes through this, so a lit Link can never sit over two different
/// times.
#[inline]
fn apply_link(params: &mut Params) {
    if params.link {
        params.time_ms_r = params.time_ms_l;
        params.division_r = params.division_l;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EchospaceState {
    pub version: u32,
    pub params: Params,
}

impl EchospaceState {
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

impl Default for EchospaceState {
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
    params.time_ms_l = clamp_wire(TIME_L_INDEX, params.time_ms_l);
    params.time_ms_r = clamp_wire(TIME_R_INDEX, params.time_ms_r);
    params.feedback = clamp_wire(FEEDBACK_INDEX, params.feedback);
    params.cross_feedback = clamp_wire(CROSS_INDEX, params.cross_feedback);
    params.low_cut_hz = clamp_wire(LOW_CUT_INDEX, params.low_cut_hz);
    params.high_cut_hz = clamp_wire(HIGH_CUT_INDEX, params.high_cut_hz);
    params.saturation = clamp_wire(SATURATION_INDEX, params.saturation);
    params.mix = clamp_wire(MIX_INDEX, params.mix);
    params.output_db = clamp_wire(OUTPUT_INDEX, params.output_db);
    params.division_l = params.division_l.min(MAX_DIVISION_WIRE as u8);
    params.division_r = params.division_r.min(MAX_DIVISION_WIRE as u8);
    params.mod_depth = clamp_wire(MOD_DEPTH_INDEX, params.mod_depth);
    params.mod_rate_hz = clamp_wire(MOD_RATE_INDEX, params.mod_rate_hz);
    params.duck = clamp_wire(DUCK_INDEX, params.duck);
    params.diffusion = clamp_wire(DIFFUSION_INDEX, params.diffusion);
    params.width = clamp_wire(WIDTH_INDEX, params.width);
    apply_link(params);
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
        MODE_INDEX => params.mode = DelayMode::from_wire(value),
        FREEZE_INDEX => params.freeze = value >= 0.5,
        SYNC_INDEX => params.sync = value >= 0.5,
        LINK_INDEX => {
            params.link = value >= 0.5;
            apply_link(params);
        }
        // While linked, either side's control drives both — the edit is made on
        // the side the user touched and mirrored onto the other, rather than the
        // right side being a read-only shadow of the left.
        TIME_L_INDEX => {
            params.time_ms_l = clamp_wire(index, value);
            if params.link {
                params.time_ms_r = params.time_ms_l;
            }
        }
        TIME_R_INDEX => {
            params.time_ms_r = clamp_wire(index, value);
            if params.link {
                params.time_ms_l = params.time_ms_r;
            }
        }
        DIVISION_L_INDEX => {
            params.division_l = wire_to_division(value);
            if params.link {
                params.division_r = params.division_l;
            }
        }
        DIVISION_R_INDEX => {
            params.division_r = wire_to_division(value);
            if params.link {
                params.division_l = params.division_r;
            }
        }
        FEEDBACK_INDEX => params.feedback = clamp_wire(index, value),
        CROSS_INDEX => params.cross_feedback = clamp_wire(index, value),
        LOW_CUT_INDEX => params.low_cut_hz = clamp_wire(index, value),
        HIGH_CUT_INDEX => params.high_cut_hz = clamp_wire(index, value),
        SATURATION_INDEX => params.saturation = clamp_wire(index, value),
        MIX_INDEX => params.mix = clamp_wire(index, value),
        OUTPUT_INDEX => params.output_db = clamp_wire(index, value),
        MOD_DEPTH_INDEX => params.mod_depth = clamp_wire(index, value),
        MOD_RATE_INDEX => params.mod_rate_hz = clamp_wire(index, value),
        DUCK_INDEX => params.duck = clamp_wire(index, value),
        DIFFUSION_INDEX => params.diffusion = clamp_wire(index, value),
        WIDTH_INDEX => params.width = clamp_wire(index, value),
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

/// Every parameter as `(id, raw value)`, in wire order but for `link`, which
/// goes last. Drives project replay, the editor's edits and the
/// descriptor-vs-defaults check.
pub fn ui_values(params: &Params) -> Vec<(&'static str, f32)> {
    vec![
        ("power", f32::from(params.power)),
        ("mode", params.mode.to_wire()),
        ("timeMsL", params.time_ms_l),
        ("timeMsR", params.time_ms_r),
        ("feedback", params.feedback),
        ("crossFeedback", params.cross_feedback),
        ("lowCutHz", params.low_cut_hz),
        ("highCutHz", params.high_cut_hz),
        ("saturation", params.saturation),
        ("mix", params.mix),
        ("outputDb", params.output_db),
        ("freeze", f32::from(params.freeze)),
        ("sync", f32::from(params.sync)),
        ("divisionL", f32::from(params.division_l)),
        ("divisionR", f32::from(params.division_r)),
        ("modDepth", params.mod_depth),
        ("modRateHz", params.mod_rate_hz),
        ("duck", params.duck),
        ("diffusion", params.diffusion),
        ("width", params.width),
        // Replayed last, after both sides carry their own restored values.
        // `link` only ever pulls right onto left, and a blob that had it lit was
        // sanitized with the two sides already equal, so this is a no-op on a
        // consistent project and a repair on a hand-edited one.
        ("link", f32::from(params.link)),
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
    /// missing entry would silently drop a parameter on reload. `link` is the
    /// one exception to wire order: it must land after both sides.
    #[test]
    fn ui_values_covers_every_id_with_link_last() {
        let values = ui_values(&default_params());
        assert_eq!(values.len(), PARAM_COUNT);
        assert_eq!(values.last().map(|(id, _)| *id), Some("link"));
        let mut ids: Vec<_> = values.iter().map(|(id, _)| *id).collect();
        ids.sort_unstable();
        let mut expected = UI_PARAM_IDS.to_vec();
        expected.sort_unstable();
        assert_eq!(ids, expected);
    }

    #[test]
    fn state_round_trips() {
        let mut params = default_params();
        assert!(apply_ui_param(&mut params, "timeMsL", 250.0));
        assert!(apply_ui_param(
            &mut params,
            "mode",
            DelayMode::Mono.to_wire()
        ));
        let json = EchospaceState::new(params).to_json().unwrap();
        let decoded = EchospaceState::from_json(&json).unwrap();
        assert_eq!(decoded.params.time_ms_l, 250.0);
        assert_eq!(decoded.params.mode, DelayMode::Mono);
        assert_eq!(decoded.version, STATE_VERSION);
    }

    #[test]
    fn out_of_range_values_are_clamped_not_rejected() {
        let mut params = default_params();
        assert!(apply_ui_param(&mut params, "timeMsL", 99_000.0));
        assert_eq!(params.time_ms_l, MAX_DELAY_MS);
        // The feedback ceiling is below 100 % on purpose — the delay line is a
        // feedback loop and unity would never decay.
        assert!(apply_ui_param(&mut params, "feedback", 100.0));
        assert_eq!(params.feedback, 98.0);
    }

    #[test]
    fn invalid_and_non_finite_updates_are_rejected() {
        let mut params = default_params();
        assert!(!apply_wire_param(&mut params, u32::MAX, 1.0));
        assert!(!apply_wire_param(&mut params, PARAM_COUNT as u32, 1.0));
        assert!(!apply_wire_param(&mut params, TIME_L_INDEX, f32::NAN));
        assert!(!apply_wire_param(&mut params, TIME_L_INDEX, f32::INFINITY));
        assert!(!apply_ui_param(&mut params, "notAParam", 1.0));
    }

    /// A blob written by an older build (or edited by hand) must not reach the
    /// DSP with values the coefficient math cannot survive.
    #[test]
    fn sanitize_pulls_a_hand_edited_blob_back_into_range() {
        let mut params = default_params();
        params.time_ms_l = -20.0;
        params.feedback = 400.0;
        params.high_cut_hz = 96_000.0;
        params.output_db = 60.0;
        sanitize_params(&mut params);
        assert_eq!(params.time_ms_l, 1.0);
        assert_eq!(params.feedback, 98.0);
        assert_eq!(params.high_cut_hz, 20_000.0);
        assert_eq!(params.output_db, 12.0);
    }

    /// Link is the "both sides at once" promise: whichever control the user
    /// touched drives both, and the other is never left behind.
    #[test]
    fn link_carries_an_edit_from_either_side_to_the_other() {
        let mut params = default_params();
        assert!(apply_ui_param(&mut params, "link", 1.0));

        assert!(apply_ui_param(&mut params, "timeMsL", 250.0));
        assert_eq!(params.time_ms_r, 250.0);
        assert!(apply_ui_param(&mut params, "timeMsR", 800.0));
        assert_eq!(params.time_ms_l, 800.0);

        assert!(apply_ui_param(&mut params, "divisionL", 7.0));
        assert_eq!(params.division_r, 7);
        assert!(apply_ui_param(&mut params, "divisionR", 12.0));
        assert_eq!(params.division_l, 12);

        // Unlinked, the two sides go back to moving on their own.
        assert!(apply_ui_param(&mut params, "link", 0.0));
        assert!(apply_ui_param(&mut params, "timeMsL", 100.0));
        assert_eq!(params.time_ms_r, 800.0);
        assert!(apply_ui_param(&mut params, "divisionL", 3.0));
        assert_eq!(params.division_r, 12);
    }

    /// Turning Link on has to reconcile the two sides immediately — a lit
    /// toggle over two different times would be a lie until the next edit.
    #[test]
    fn enabling_link_pulls_the_right_side_onto_the_left() {
        let mut params = default_params();
        params.time_ms_l = 375.0;
        params.time_ms_r = 900.0;
        params.division_l = 4;
        params.division_r = 15;
        assert!(apply_ui_param(&mut params, "link", 1.0));
        assert_eq!(params.time_ms_r, 375.0);
        assert_eq!(params.division_r, 4);
    }

    #[test]
    fn division_wire_values_are_rounded_and_clamped() {
        let mut params = default_params();
        assert!(apply_ui_param(&mut params, "divisionL", 6.6));
        assert_eq!(params.division_l, 7);
        assert!(apply_ui_param(&mut params, "divisionL", -12.0));
        assert_eq!(params.division_l, 0);
        assert!(apply_ui_param(&mut params, "divisionL", 9_000.0));
        assert_eq!(params.division_l, MAX_DIVISION_WIRE as u8);
        assert!(!apply_ui_param(&mut params, "divisionL", f32::NAN));
    }

    #[test]
    fn sync_and_link_are_flags_like_power_and_freeze() {
        let mut params = default_params();
        assert!(!params.sync);
        assert!(apply_ui_param(&mut params, "sync", 1.0));
        assert!(params.sync);
        assert!(apply_ui_param(&mut params, "sync", 0.0));
        assert!(!params.sync);
    }

    /// A project written before EchoSpace had tempo sync carries none of the
    /// new fields. It must open at the documented defaults rather than being
    /// rejected whole, which would silently reset the whole insert.
    #[test]
    fn a_blob_written_before_tempo_sync_still_loads() {
        let legacy = r#"{
            "version": 1,
            "params": {
                "power": true,
                "mode": "stereo",
                "timeMsL": 320.0,
                "timeMsR": 340.0,
                "feedback": 52.0,
                "crossFeedback": 25.0,
                "lowCutHz": 260.0,
                "highCutHz": 4200.0,
                "saturation": 62.0,
                "mix": 30.0,
                "outputDb": -1.5,
                "freeze": false
            }
        }"#;
        let decoded = EchospaceState::from_json(legacy).expect("legacy blob must still decode");
        assert_eq!(decoded.params.time_ms_l, 320.0);
        assert!(!decoded.params.sync);
        assert!(!decoded.params.link);
        assert_eq!(decoded.params.division_l, crate::DEFAULT_DIVISION_L);
        assert_eq!(decoded.params.division_r, crate::DEFAULT_DIVISION_R);
        // ...and so do the controls added with the rebuilt engine.
        assert_eq!(decoded.params.mod_depth, 0.0);
        assert_eq!(decoded.params.duck, 0.0);
        assert_eq!(decoded.params.diffusion, 0.0);
        assert_eq!(decoded.params.width, 100.0);
        assert_eq!(decoded.params.mod_rate_hz, crate::DEFAULT_MOD_RATE_HZ);
    }

    /// `link` is an invariant, not just a flag, so a hand-edited blob that
    /// claims it while carrying two different sides is repaired on the way in.
    #[test]
    fn sanitize_reconciles_a_linked_blob_whose_sides_disagree() {
        let mut params = default_params();
        params.link = true;
        params.time_ms_l = 250.0;
        params.time_ms_r = 900.0;
        params.division_l = 5;
        params.division_r = 200;
        sanitize_params(&mut params);
        assert_eq!(params.time_ms_r, 250.0);
        assert_eq!(params.division_r, 5);
    }

    #[test]
    fn sanitize_clamps_an_out_of_table_division() {
        let mut params = default_params();
        params.division_l = 250;
        params.division_r = 99;
        sanitize_params(&mut params);
        assert_eq!(params.division_l, MAX_DIVISION_WIRE as u8);
        assert_eq!(params.division_r, MAX_DIVISION_WIRE as u8);
    }

    /// Replay walks `ui_values` in wire order, and `link` only ever pulls right
    /// onto left — so it has to land after both sides have been restored.
    #[test]
    fn a_linked_state_survives_a_replay_through_the_wire() {
        let mut saved = default_params();
        saved.link = true;
        saved.sync = true;
        saved.time_ms_l = 250.0;
        saved.division_l = 7;
        sanitize_params(&mut saved);

        let mut rebuilt = default_params();
        for (id, value) in ui_values(&saved) {
            assert!(
                apply_ui_param(&mut rebuilt, id, value),
                "`{id}` was rejected"
            );
        }
        assert_eq!(rebuilt.time_ms_l, saved.time_ms_l);
        assert_eq!(rebuilt.time_ms_r, saved.time_ms_r);
        assert_eq!(rebuilt.division_l, saved.division_l);
        assert_eq!(rebuilt.division_r, saved.division_r);
        assert!(rebuilt.link && rebuilt.sync);
    }

    /// The same replay for an *unlinked* project: the two sides are different
    /// on purpose, and nothing on the way in may reconcile them.
    #[test]
    fn an_unlinked_replay_keeps_the_two_sides_apart() {
        let mut saved = default_params();
        saved.time_ms_l = 250.0;
        saved.time_ms_r = 900.0;
        saved.division_l = 7;
        saved.division_r = 12;

        let mut rebuilt = default_params();
        for (id, value) in ui_values(&saved) {
            assert!(apply_ui_param(&mut rebuilt, id, value));
        }
        assert_eq!(rebuilt.time_ms_r, 900.0);
        assert_eq!(rebuilt.division_r, 12);
    }

    #[test]
    fn mode_wire_values_round_trip() {
        for mode in [DelayMode::Stereo, DelayMode::PingPong, DelayMode::Mono] {
            assert_eq!(DelayMode::from_wire(mode.to_wire()), mode);
            assert_eq!(DelayMode::parse(mode.as_str()), Some(mode));
        }
    }
}

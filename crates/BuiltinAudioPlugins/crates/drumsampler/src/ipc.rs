use serde::{Deserialize, Serialize};

use crate::{
    FilterMode, MAX_CUTOFF_HZ, MAX_DECAY_MS, MAX_HOLD_MS, MIN_CUTOFF_HZ, PADS, Pad, Params, clamp,
    default_params,
};

pub const PROTOCOL_VERSION: u32 = 1;
pub const STATE_VERSION: u32 = 1;

/// Fields of the first release, flattened per pad, in wire order:
/// note, tune, gain, pan, choke, attack, release, reverse, mute, solo.
/// `apply_wire_param`/`ui_values` index into a pad's fields by this order
/// rather than by string match.
pub const FIELDS_PER_PAD: u32 = 10;

/// Fields added with the region, filter, velocity and AHD envelope, flattened
/// per pad after the whole first block: start, end, filter mode, cutoff,
/// resonance, velocity sensitivity, hold, decay. Appended rather than
/// interleaved so every first-release id keeps its wire index.
pub const EXTRA_FIELDS_PER_PAD: u32 = 8;

/// First wire index of the second per-pad block.
pub const EXTRA_BASE: u32 = PADS as u32 * FIELDS_PER_PAD;
/// Whole-kit controls, after both per-pad blocks.
pub const MASTER_GAIN_INDEX: u32 = EXTRA_BASE + PADS as u32 * EXTRA_FIELDS_PER_PAD;
pub const MASTER_TUNE_INDEX: u32 = MASTER_GAIN_INDEX + 1;

pub const PARAM_COUNT: usize = MASTER_TUNE_INDEX as usize + 1;

macro_rules! wire_ids {
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
            $(
                concat!("pad", $n, "Start"),
                concat!("pad", $n, "End"),
                concat!("pad", $n, "FilterMode"),
                concat!("pad", $n, "Cutoff"),
                concat!("pad", $n, "Resonance"),
                concat!("pad", $n, "Velocity"),
                concat!("pad", $n, "Hold"),
                concat!("pad", $n, "Decay"),
            )*
            "masterGain",
            "masterTune",
        ]
    };
}

/// Wire index *is* the position in this table. Append only: the first
/// release's 160 ids keep the indices they shipped with.
pub const UI_PARAM_IDS: [&str; PARAM_COUNT] =
    wire_ids!(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15);

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

/// Finite `value`, or `fallback`.
fn finite_or(value: f32, fallback: f32) -> f32 {
    if value.is_finite() { value } else { fallback }
}

fn sanitize_pad(pad: &mut Pad) {
    pad.tune_semitones = clamp(finite_or(pad.tune_semitones, 0.0), -24.0, 24.0);
    pad.gain_db = clamp(finite_or(pad.gain_db, 0.0), -60.0, 12.0);
    pad.pan = clamp(finite_or(pad.pan, 0.0), -1.0, 1.0);
    pad.choke_group = pad.choke_group.min(8);
    pad.attack_ms = clamp(finite_or(pad.attack_ms, 1.0), 0.0, 250.0);
    pad.release_ms = clamp(finite_or(pad.release_ms, 60.0), 1.0, 2_000.0);
    pad.start = clamp(finite_or(pad.start, 0.0), 0.0, 1.0);
    pad.end = clamp(finite_or(pad.end, 1.0), 0.0, 1.0);
    pad.cutoff_hz = clamp(
        finite_or(pad.cutoff_hz, MAX_CUTOFF_HZ),
        MIN_CUTOFF_HZ,
        MAX_CUTOFF_HZ,
    );
    pad.resonance = clamp(finite_or(pad.resonance, 0.0), 0.0, 100.0);
    pad.velocity_sensitivity = clamp(finite_or(pad.velocity_sensitivity, 100.0), 0.0, 100.0);
    pad.hold_ms = clamp(finite_or(pad.hold_ms, 0.0), 0.0, MAX_HOLD_MS);
    pad.decay_ms = clamp(finite_or(pad.decay_ms, 0.0), 0.0, MAX_DECAY_MS);
}

pub fn sanitize_params(params: &mut Params) {
    for pad in &mut params.pads {
        sanitize_pad(pad);
    }
    params.master_gain_db = clamp(finite_or(params.master_gain_db, 0.0), -60.0, 12.0);
    params.master_tune = clamp(finite_or(params.master_tune, 0.0), -24.0, 24.0);
}

/// What an applied wire edit needs the DSP to recompute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Affects {
    Nothing,
    /// The filter coefficients of this pad.
    Filter(usize),
    MasterGain,
}

/// Classify a wire index for [`crate::Dsp::apply_wire_param`] — arithmetic
/// only, no lookup.
pub fn affected(index: u32) -> Affects {
    if index == MASTER_GAIN_INDEX {
        return Affects::MasterGain;
    }
    if (EXTRA_BASE..MASTER_GAIN_INDEX).contains(&index) {
        let offset = index - EXTRA_BASE;
        // Filter mode, cutoff and resonance.
        if (2..=4).contains(&(offset % EXTRA_FIELDS_PER_PAD)) {
            return Affects::Filter((offset / EXTRA_FIELDS_PER_PAD) as usize);
        }
    }
    Affects::Nothing
}

/// Apply one wire edit. `index` is decomposed into a block, a pad and a field
/// within that pad by arithmetic - no string match on the realtime path.
pub fn apply_wire_param(params: &mut Params, index: u32, value: f32) -> bool {
    if !value.is_finite() {
        return false;
    }
    if index == MASTER_GAIN_INDEX {
        params.master_gain_db = clamp(value, -60.0, 12.0);
        return true;
    }
    if index == MASTER_TUNE_INDEX {
        params.master_tune = clamp(value, -24.0, 24.0);
        return true;
    }
    if index >= EXTRA_BASE {
        let offset = index - EXTRA_BASE;
        let Some(pad) = params
            .pads
            .get_mut((offset / EXTRA_FIELDS_PER_PAD) as usize)
        else {
            return false;
        };
        match offset % EXTRA_FIELDS_PER_PAD {
            0 => pad.start = clamp(value, 0.0, 1.0),
            1 => pad.end = clamp(value, 0.0, 1.0),
            2 => pad.filter_mode = FilterMode::from_wire(value),
            3 => pad.cutoff_hz = clamp(value, MIN_CUTOFF_HZ, MAX_CUTOFF_HZ),
            4 => pad.resonance = clamp(value, 0.0, 100.0),
            5 => pad.velocity_sensitivity = clamp(value, 0.0, 100.0),
            6 => pad.hold_ms = clamp(value, 0.0, MAX_HOLD_MS),
            7 => pad.decay_ms = clamp(value, 0.0, MAX_DECAY_MS),
            _ => return false,
        }
        return true;
    }
    let Some(pad) = params.pads.get_mut((index / FIELDS_PER_PAD) as usize) else {
        return false;
    };
    match index % FIELDS_PER_PAD {
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
    let mut out = Vec::with_capacity(PARAM_COUNT);
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
    for (pad_index, pad) in params.pads.iter().enumerate() {
        let base = EXTRA_BASE as usize + pad_index * EXTRA_FIELDS_PER_PAD as usize;
        out.push((UI_PARAM_IDS[base], pad.start));
        out.push((UI_PARAM_IDS[base + 1], pad.end));
        out.push((UI_PARAM_IDS[base + 2], pad.filter_mode.to_wire()));
        out.push((UI_PARAM_IDS[base + 3], pad.cutoff_hz));
        out.push((UI_PARAM_IDS[base + 4], pad.resonance));
        out.push((UI_PARAM_IDS[base + 5], pad.velocity_sensitivity));
        out.push((UI_PARAM_IDS[base + 6], pad.hold_ms));
        out.push((UI_PARAM_IDS[base + 7], pad.decay_ms));
    }
    out.push(("masterGain", params.master_gain_db));
    out.push(("masterTune", params.master_tune));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_round_trip() {
        assert_eq!(UI_PARAM_IDS.len(), PARAM_COUNT);
        for (index, id) in UI_PARAM_IDS.iter().enumerate() {
            assert_eq!(ui_param_index(id), Some(index as u32));
            assert_eq!(ui_param_id(index as u32), Some(*id));
        }
        let mut seen = std::collections::HashSet::new();
        assert!(UI_PARAM_IDS.iter().all(|id| seen.insert(*id)));
    }

    /// The first release's ids are a persisted contract: each must keep the
    /// index it shipped with.
    #[test]
    fn first_release_ids_keep_their_wire_indices() {
        assert_eq!(ui_param_index("pad0Note"), Some(0));
        assert_eq!(ui_param_index("pad0Solo"), Some(9));
        assert_eq!(ui_param_index("pad2Gain"), Some(22));
        assert_eq!(ui_param_index("pad15Solo"), Some(159));
        assert_eq!(ui_param_index("pad0Start"), Some(160));
        assert_eq!(ui_param_index("pad15Decay"), Some(160 + 16 * 8 - 1));
        assert_eq!(ui_param_index("masterGain"), Some(MASTER_GAIN_INDEX));
        assert_eq!(ui_param_index("masterTune"), Some(MASTER_TUNE_INDEX));
    }

    #[test]
    fn apply_wire_param_clamps_and_rejects_out_of_range() {
        let mut params = default_params();
        assert!(apply_ui_param(&mut params, "pad2Gain", 99.0));
        assert_eq!(params.pads[2].gain_db, 12.0);
        assert!(apply_ui_param(&mut params, "pad4Cutoff", 1.0e9));
        assert_eq!(params.pads[4].cutoff_hz, MAX_CUTOFF_HZ);
        assert!(apply_ui_param(&mut params, "pad4FilterMode", 9.0));
        assert_eq!(params.pads[4].filter_mode, FilterMode::BandPass);
        assert!(apply_ui_param(&mut params, "pad7Decay", -4.0));
        assert_eq!(params.pads[7].decay_ms, 0.0);
        assert!(apply_ui_param(&mut params, "masterTune", 99.0));
        assert_eq!(params.master_tune, 24.0);
        assert!(!apply_wire_param(&mut params, PARAM_COUNT as u32, 1.0));
        assert!(!apply_wire_param(&mut params, u32::MAX, 1.0));
        assert!(!apply_wire_param(&mut params, 0, f32::NAN));
    }

    #[test]
    fn filter_edits_and_only_filter_edits_ask_for_new_coefficients() {
        for (id, expected) in [
            ("pad3FilterMode", Affects::Filter(3)),
            ("pad3Cutoff", Affects::Filter(3)),
            ("pad15Resonance", Affects::Filter(15)),
            ("pad3Start", Affects::Nothing),
            ("pad3Decay", Affects::Nothing),
            ("pad3Gain", Affects::Nothing),
            ("masterGain", Affects::MasterGain),
            ("masterTune", Affects::Nothing),
        ] {
            assert_eq!(affected(ui_param_index(id).unwrap()), expected, "{id}");
        }
    }

    #[test]
    fn a_replay_through_the_wire_rebuilds_the_state() {
        let mut saved = default_params();
        saved.pads[5].gain_db = -3.0;
        saved.pads[5].start = 0.25;
        saved.pads[5].end = 0.75;
        saved.pads[5].filter_mode = FilterMode::HighPass;
        saved.pads[5].cutoff_hz = 800.0;
        saved.pads[5].resonance = 40.0;
        saved.pads[5].velocity_sensitivity = 30.0;
        saved.pads[5].hold_ms = 20.0;
        saved.pads[5].decay_ms = 350.0;
        saved.pads[9].reverse = true;
        saved.master_gain_db = -6.0;
        saved.master_tune = 2.0;

        let mut rebuilt = default_params();
        for (id, value) in ui_values(&saved) {
            assert!(
                apply_ui_param(&mut rebuilt, id, value),
                "`{id}` was rejected"
            );
        }
        let (a, b) = (&rebuilt.pads[5], &saved.pads[5]);
        assert_eq!(a.gain_db, b.gain_db);
        assert_eq!((a.start, a.end), (b.start, b.end));
        assert_eq!(a.filter_mode, b.filter_mode);
        assert_eq!((a.cutoff_hz, a.resonance), (b.cutoff_hz, b.resonance));
        assert_eq!(a.velocity_sensitivity, b.velocity_sensitivity);
        assert_eq!((a.hold_ms, a.decay_ms), (b.hold_ms, b.decay_ms));
        assert!(rebuilt.pads[9].reverse);
        assert_eq!((rebuilt.master_gain_db, rebuilt.master_tune), (-6.0, 2.0));
        assert_eq!(ui_values(&saved).len(), PARAM_COUNT);
    }

    #[test]
    fn state_round_trips() {
        let mut params = default_params();
        params.pads[5].gain_db = -3.0;
        params.pads[5].filter_mode = FilterMode::LowPass;
        params.master_gain_db = -1.5;
        let json = DrumSamplerState::new(params).to_json().unwrap();
        let decoded = DrumSamplerState::from_json(&json).unwrap().params;
        assert_eq!(decoded.pads[5].gain_db, -3.0);
        assert_eq!(decoded.pads[5].filter_mode, FilterMode::LowPass);
        assert_eq!(decoded.master_gain_db, -1.5);
    }

    #[test]
    fn sanitize_pulls_a_hand_edited_blob_back_into_range() {
        let mut params = default_params();
        params.pads[0].start = -1.0;
        params.pads[0].end = 4.0;
        params.pads[0].cutoff_hz = f32::NAN;
        params.pads[0].decay_ms = 1.0e9;
        params.master_gain_db = f32::INFINITY;
        sanitize_params(&mut params);
        assert_eq!((params.pads[0].start, params.pads[0].end), (0.0, 1.0));
        assert_eq!(params.pads[0].cutoff_hz, MAX_CUTOFF_HZ);
        assert_eq!(params.pads[0].decay_ms, MAX_DECAY_MS);
        assert_eq!(params.master_gain_db, 0.0);
    }
}

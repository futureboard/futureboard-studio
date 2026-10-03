//! Wire contract between Studio and the plug-in host: parameter ids, their
//! wire indices, and the persisted state blob.

use serde::{Deserialize, Serialize};

use crate::{FilterMode, LoopMode, Params, QuickSamplerParams, default_params};

pub const STATE_VERSION: u32 = 1;

/// Wire index *is* the position in this table. Append only: a shipped id
/// keeps its index forever, because projects and the param ring carry indices.
pub const UI_PARAM_IDS: [&str; 23] = [
    "rootNote",
    "transpose",
    "fineCents",
    "keytrack",
    "start",
    "end",
    "reverse",
    "loopMode",
    "loopStart",
    "loopEnd",
    "normalize",
    "attack",
    "decay",
    "sustain",
    "release",
    "filterMode",
    "cutoff",
    "resonance",
    "volume",
    "pan",
    "velocity",
    "polyphony",
    "bendRange",
];

pub const PARAM_COUNT: usize = UI_PARAM_IDS.len();

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuickSamplerState {
    pub version: u32,
    pub params: Params,
}

impl QuickSamplerState {
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
        let mut state: Self = serde_json::from_str(json)?;
        state.params.sampler = state.params.sampler.sanitized();
        Ok(state)
    }
}

impl Default for QuickSamplerState {
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

fn loop_mode_from_wire(value: f32) -> LoopMode {
    match value.round() as i32 {
        1 => LoopMode::Forward,
        2 => LoopMode::PingPong,
        _ => LoopMode::Off,
    }
}

pub fn loop_mode_to_wire(mode: LoopMode) -> f32 {
    match mode {
        LoopMode::Off => 0.0,
        LoopMode::Forward => 1.0,
        LoopMode::PingPong => 2.0,
    }
}

fn filter_from_wire(value: f32) -> FilterMode {
    match value.round() as i32 {
        1 => FilterMode::LowPass,
        2 => FilterMode::HighPass,
        3 => FilterMode::BandPass,
        _ => FilterMode::Off,
    }
}

pub fn filter_to_wire(mode: FilterMode) -> f32 {
    match mode {
        FilterMode::Off => 0.0,
        FilterMode::LowPass => 1.0,
        FilterMode::HighPass => 2.0,
        FilterMode::BandPass => 3.0,
    }
}

/// Applies one wire edit by index — arithmetic only, no string match on the
/// realtime path. The result is sanitized, so a stray value cannot leave the
/// sampler out of range. `false` for an unknown index or a non-finite value.
pub fn apply_wire_param(p: &mut QuickSamplerParams, index: u32, value: f32) -> bool {
    if !value.is_finite() {
        return false;
    }
    let on = value >= 0.5;
    match index {
        0 => p.root_note = value.round().clamp(0.0, 127.0) as u8,
        1 => p.transpose = value.round().clamp(-128.0, 127.0) as i8,
        2 => p.fine_cents = value,
        3 => p.keytrack = on,
        4 => p.start = value,
        5 => p.end = value,
        6 => p.reverse = on,
        7 => p.loop_mode = loop_mode_from_wire(value),
        8 => p.loop_start = value,
        9 => p.loop_end = value,
        10 => p.normalize = on,
        11 => p.attack_ms = value,
        12 => p.decay_ms = value,
        13 => p.sustain = value,
        14 => p.release_ms = value,
        15 => p.filter = filter_from_wire(value),
        16 => p.cutoff_hz = value,
        17 => p.resonance = value,
        18 => p.volume = value,
        19 => p.pan = value,
        20 => p.velocity = value,
        21 => p.polyphony = value.round().clamp(0.0, 255.0) as u8,
        22 => p.pitch_bend_range = value.round().clamp(0.0, 255.0) as u8,
        _ => return false,
    }
    *p = p.sanitized();
    true
}

/// Every parameter as `(id, raw value)`, in wire order. Drives project replay
/// and lets an editor send a whole new params value as edits.
pub fn ui_values(p: &QuickSamplerParams) -> Vec<(&'static str, f32)> {
    let flag = |on: bool| if on { 1.0 } else { 0.0 };
    let values = [
        p.root_note as f32,
        p.transpose as f32,
        p.fine_cents,
        flag(p.keytrack),
        p.start,
        p.end,
        flag(p.reverse),
        loop_mode_to_wire(p.loop_mode),
        p.loop_start,
        p.loop_end,
        flag(p.normalize),
        p.attack_ms,
        p.decay_ms,
        p.sustain,
        p.release_ms,
        filter_to_wire(p.filter),
        p.cutoff_hz,
        p.resonance,
        p.volume,
        p.pan,
        p.velocity,
        p.polyphony as f32,
        p.pitch_bend_range as f32,
    ];
    UI_PARAM_IDS.iter().copied().zip(values).collect()
}

/// The wire edits that turn `from` into `to`: only the fields that differ.
/// An editor that builds a whole new params value sends just these.
pub fn wire_diff(from: &QuickSamplerParams, to: &QuickSamplerParams) -> Vec<(u32, f32)> {
    ui_values(from)
        .into_iter()
        .zip(ui_values(to))
        .enumerate()
        .filter(|(_, ((_, a), (_, b)))| a != b)
        .map(|(index, (_, (_, value)))| (index as u32, value))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_round_trip() {
        let mut seen = std::collections::HashSet::new();
        for (index, id) in UI_PARAM_IDS.iter().enumerate() {
            assert!(seen.insert(*id), "{id} twice");
            assert_eq!(ui_param_index(id), Some(index as u32));
            assert_eq!(ui_param_id(index as u32), Some(*id));
        }
    }

    #[test]
    fn a_replay_through_the_wire_rebuilds_the_params() {
        let saved = QuickSamplerParams {
            root_note: 48,
            transpose: -5,
            fine_cents: 12.0,
            keytrack: false,
            start: 0.1,
            end: 0.7,
            reverse: true,
            loop_mode: LoopMode::PingPong,
            loop_start: 0.3,
            loop_end: 0.6,
            normalize: true,
            attack_ms: 5.0,
            decay_ms: 120.0,
            sustain: 0.4,
            release_ms: 800.0,
            filter: FilterMode::HighPass,
            cutoff_hz: 900.0,
            resonance: 0.3,
            volume: 0.5,
            pan: -0.4,
            velocity: 0.25,
            polyphony: 1,
            pitch_bend_range: 7,
        };
        let mut rebuilt = QuickSamplerParams::default();
        for (id, value) in ui_values(&saved) {
            assert!(apply_wire_param(
                &mut rebuilt,
                ui_param_index(id).unwrap(),
                value
            ));
        }
        assert_eq!(rebuilt, saved);
        assert_eq!(wire_diff(&saved, &rebuilt), Vec::new());
    }

    #[test]
    fn a_diff_carries_only_what_changed() {
        let from = QuickSamplerParams::default();
        let to = QuickSamplerParams {
            cutoff_hz: 500.0,
            filter: FilterMode::LowPass,
            ..from
        };
        assert_eq!(
            wire_diff(&from, &to),
            vec![(15, filter_to_wire(FilterMode::LowPass)), (16, 500.0)]
        );
    }

    #[test]
    fn wire_edits_are_clamped_and_bad_ones_rejected() {
        let mut p = QuickSamplerParams::default();
        assert!(apply_wire_param(&mut p, 16, 1.0e9));
        assert_eq!(p.cutoff_hz, crate::CUTOFF_MAX_HZ);
        assert!(!apply_wire_param(&mut p, 99, 1.0));
        assert!(!apply_wire_param(&mut p, 0, f32::NAN));
    }

    #[test]
    fn state_round_trips_with_the_sample_name() {
        let mut params = default_params();
        params.sample_name = Some("808 kick.wav".to_string());
        params.sampler.root_note = 36;
        let json = QuickSamplerState::new(params.clone()).to_json().unwrap();
        assert_eq!(QuickSamplerState::from_json(&json).unwrap().params, params);
        // An older or partial blob fills in defaults.
        let partial = QuickSamplerState::from_json(r#"{"version":1,"params":{}}"#).unwrap();
        assert_eq!(partial.params, default_params());
    }
}

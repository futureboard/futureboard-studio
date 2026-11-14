//! Wire contract between Studio and the plug-in host: parameter ids, their
//! wire indices, and the persisted state blob.

use serde::{Deserialize, Serialize};

use crate::{
    BeatDivision, MAX_SLICES, MIN_EQUAL_SLICES, Params, SliceMode, SlicerParams, default_params,
    sanitize_bpm,
};

pub const STATE_VERSION: u32 = 1;

/// The voice settings a slice plays with, as their Quick Sampler wire
/// indices — so the two plug-ins share one meaning per id.
const VOICE_WIRE: [u32; 16] = [1, 2, 6, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22];
const VOICE_COUNT: usize = VOICE_WIRE.len();
/// Wire index of `sliceCount`; the slice points follow it.
pub const SLICE_COUNT_WIRE: u32 = VOICE_COUNT as u32 + 8;
pub const FIRST_SLICE_WIRE: u32 = SLICE_COUNT_WIRE + 1;
/// Wire index of slice 0's own gain, pitch and direction; slice `n` is `n`
/// after each.
pub const FIRST_SLICE_GAIN_WIRE: u32 = FIRST_SLICE_WIRE + MAX_SLICES as u32;
pub const FIRST_SLICE_PITCH_WIRE: u32 = FIRST_SLICE_GAIN_WIRE + MAX_SLICES as u32;
pub const FIRST_SLICE_REVERSE_WIRE: u32 = FIRST_SLICE_PITCH_WIRE + MAX_SLICES as u32;

/// Wire index *is* the position in this table. Append only: a shipped id
/// keeps its index forever, because projects and the param ring carry indices.
#[rustfmt::skip]
pub const UI_PARAM_IDS: [&str; VOICE_COUNT + 9 + MAX_SLICES * 4] = [
    // How a slice plays (the Quick Sampler ids).
    "transpose", "fineCents", "reverse", "normalize", "attack", "decay", "sustain", "release",
    "filterMode", "cutoff", "resonance", "volume", "pan", "velocity", "polyphony", "bendRange",
    // The keys, and how slices meet.
    "firstKey", "choke", "oneShot",
    // How the editor slices.
    "sliceMode", "sensitivity", "equalSlices", "beatDivision", "sampleBpm",
    // The slices.
    "sliceCount",
    "slice0", "slice1", "slice2", "slice3", "slice4", "slice5", "slice6", "slice7",
    "slice8", "slice9", "slice10", "slice11", "slice12", "slice13", "slice14", "slice15",
    "slice16", "slice17", "slice18", "slice19", "slice20", "slice21", "slice22", "slice23",
    "slice24", "slice25", "slice26", "slice27", "slice28", "slice29", "slice30", "slice31",
    "slice32", "slice33", "slice34", "slice35", "slice36", "slice37", "slice38", "slice39",
    "slice40", "slice41", "slice42", "slice43", "slice44", "slice45", "slice46", "slice47",
    "slice48", "slice49", "slice50", "slice51", "slice52", "slice53", "slice54", "slice55",
    "slice56", "slice57", "slice58", "slice59", "slice60", "slice61", "slice62", "slice63",
    // Each slice's own gain (dB), pitch (semitones) and direction.
    "sliceGain0", "sliceGain1", "sliceGain2", "sliceGain3", "sliceGain4", "sliceGain5", "sliceGain6", "sliceGain7",
    "sliceGain8", "sliceGain9", "sliceGain10", "sliceGain11", "sliceGain12", "sliceGain13", "sliceGain14", "sliceGain15",
    "sliceGain16", "sliceGain17", "sliceGain18", "sliceGain19", "sliceGain20", "sliceGain21", "sliceGain22", "sliceGain23",
    "sliceGain24", "sliceGain25", "sliceGain26", "sliceGain27", "sliceGain28", "sliceGain29", "sliceGain30", "sliceGain31",
    "sliceGain32", "sliceGain33", "sliceGain34", "sliceGain35", "sliceGain36", "sliceGain37", "sliceGain38", "sliceGain39",
    "sliceGain40", "sliceGain41", "sliceGain42", "sliceGain43", "sliceGain44", "sliceGain45", "sliceGain46", "sliceGain47",
    "sliceGain48", "sliceGain49", "sliceGain50", "sliceGain51", "sliceGain52", "sliceGain53", "sliceGain54", "sliceGain55",
    "sliceGain56", "sliceGain57", "sliceGain58", "sliceGain59", "sliceGain60", "sliceGain61", "sliceGain62", "sliceGain63",
    "slicePitch0", "slicePitch1", "slicePitch2", "slicePitch3", "slicePitch4", "slicePitch5", "slicePitch6", "slicePitch7",
    "slicePitch8", "slicePitch9", "slicePitch10", "slicePitch11", "slicePitch12", "slicePitch13", "slicePitch14", "slicePitch15",
    "slicePitch16", "slicePitch17", "slicePitch18", "slicePitch19", "slicePitch20", "slicePitch21", "slicePitch22", "slicePitch23",
    "slicePitch24", "slicePitch25", "slicePitch26", "slicePitch27", "slicePitch28", "slicePitch29", "slicePitch30", "slicePitch31",
    "slicePitch32", "slicePitch33", "slicePitch34", "slicePitch35", "slicePitch36", "slicePitch37", "slicePitch38", "slicePitch39",
    "slicePitch40", "slicePitch41", "slicePitch42", "slicePitch43", "slicePitch44", "slicePitch45", "slicePitch46", "slicePitch47",
    "slicePitch48", "slicePitch49", "slicePitch50", "slicePitch51", "slicePitch52", "slicePitch53", "slicePitch54", "slicePitch55",
    "slicePitch56", "slicePitch57", "slicePitch58", "slicePitch59", "slicePitch60", "slicePitch61", "slicePitch62", "slicePitch63",
    "sliceReverse0", "sliceReverse1", "sliceReverse2", "sliceReverse3", "sliceReverse4", "sliceReverse5", "sliceReverse6", "sliceReverse7",
    "sliceReverse8", "sliceReverse9", "sliceReverse10", "sliceReverse11", "sliceReverse12", "sliceReverse13", "sliceReverse14", "sliceReverse15",
    "sliceReverse16", "sliceReverse17", "sliceReverse18", "sliceReverse19", "sliceReverse20", "sliceReverse21", "sliceReverse22", "sliceReverse23",
    "sliceReverse24", "sliceReverse25", "sliceReverse26", "sliceReverse27", "sliceReverse28", "sliceReverse29", "sliceReverse30", "sliceReverse31",
    "sliceReverse32", "sliceReverse33", "sliceReverse34", "sliceReverse35", "sliceReverse36", "sliceReverse37", "sliceReverse38", "sliceReverse39",
    "sliceReverse40", "sliceReverse41", "sliceReverse42", "sliceReverse43", "sliceReverse44", "sliceReverse45", "sliceReverse46", "sliceReverse47",
    "sliceReverse48", "sliceReverse49", "sliceReverse50", "sliceReverse51", "sliceReverse52", "sliceReverse53", "sliceReverse54", "sliceReverse55",
    "sliceReverse56", "sliceReverse57", "sliceReverse58", "sliceReverse59", "sliceReverse60", "sliceReverse61", "sliceReverse62", "sliceReverse63",
];

pub const PARAM_COUNT: usize = UI_PARAM_IDS.len();

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlicerState {
    pub version: u32,
    pub params: Params,
}

impl SlicerState {
    pub fn new(params: Params) -> Self {
        Self {
            version: STATE_VERSION,
            params,
        }
    }

    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// Reads a saved blob. Slices a hand edit left out of order are put back
    /// in order here, on the control side.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        let mut state: Self = serde_json::from_str(json)?;
        let saved = state.params.slicer.sanitized();
        let count = saved.slice_count as usize;
        // Put the slices in order, each taking its own settings along.
        let mut order: Vec<usize> = (0..count).collect();
        order.sort_by(|a, b| saved.slices[*a].total_cmp(&saved.slices[*b]));
        let mut slicer = saved;
        for (index, &old) in order.iter().enumerate() {
            slicer.slices[index] = saved.slices[old];
        }
        slicer.carry_slice_settings(&saved, |index| {
            Some(order.get(index).copied().unwrap_or(index))
        });
        state.params.slicer = slicer;
        Ok(state)
    }
}

impl Default for SlicerState {
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

fn slice_mode_from_wire(value: f32) -> SliceMode {
    match value.round() as i32 {
        1 => SliceMode::Beat,
        2 => SliceMode::Equal,
        3 => SliceMode::Manual,
        _ => SliceMode::Transient,
    }
}

pub fn slice_mode_to_wire(mode: SliceMode) -> f32 {
    match mode {
        SliceMode::Transient => 0.0,
        SliceMode::Beat => 1.0,
        SliceMode::Equal => 2.0,
        SliceMode::Manual => 3.0,
    }
}

fn beat_division_from_wire(value: f32) -> BeatDivision {
    match value.round() as i32 {
        0 => BeatDivision::Quarter,
        2 => BeatDivision::Sixteenth,
        _ => BeatDivision::Eighth,
    }
}

pub fn beat_division_to_wire(division: BeatDivision) -> f32 {
    match division {
        BeatDivision::Quarter => 0.0,
        BeatDivision::Eighth => 1.0,
        BeatDivision::Sixteenth => 2.0,
    }
}

/// Applies one wire edit by index — arithmetic only, no string match and no
/// sort, so it is safe on the realtime path. `false` for an unknown index or
/// a non-finite value.
pub fn apply_wire_param(p: &mut SlicerParams, index: u32, value: f32) -> bool {
    if !value.is_finite() {
        return false;
    }
    let index = index as usize;
    if let Some(&voice_index) = VOICE_WIRE.get(index) {
        return quicksampler::ipc::apply_wire_param(&mut p.voice, voice_index, value);
    }
    let on = value >= 0.5;
    match index - VOICE_COUNT {
        0 => p.first_key = value.round().clamp(0.0, 127.0) as u8,
        1 => p.choke = on,
        2 => p.one_shot = on,
        3 => p.slice_mode = slice_mode_from_wire(value),
        4 => p.sensitivity = value.clamp(0.0, 1.0),
        5 => {
            p.equal_slices = value
                .round()
                .clamp(MIN_EQUAL_SLICES as f32, MAX_SLICES as f32)
                as u8
        }
        6 => p.beat_division = beat_division_from_wire(value),
        7 => p.sample_bpm = sanitize_bpm(value),
        8 => p.slice_count = value.round().clamp(0.0, MAX_SLICES as f32) as u8,
        n if n >= 9 && n - 9 < MAX_SLICES => p.slices[n - 9] = value.clamp(0.0, 1.0),
        n if n >= 9 + MAX_SLICES && n - 9 - MAX_SLICES < MAX_SLICES => {
            p.slice_gain_db[n - 9 - MAX_SLICES] =
                value.clamp(crate::SLICE_GAIN_MIN_DB, crate::SLICE_GAIN_MAX_DB)
        }
        n if n >= 9 + MAX_SLICES * 2 && n - 9 - MAX_SLICES * 2 < MAX_SLICES => {
            p.slice_pitch[n - 9 - MAX_SLICES * 2] = value
                .round()
                .clamp(-crate::SLICE_PITCH_RANGE, crate::SLICE_PITCH_RANGE)
        }
        n if n >= 9 + MAX_SLICES * 3 && n - 9 - MAX_SLICES * 3 < MAX_SLICES => {
            p.set_slice_reversed(n - 9 - MAX_SLICES * 3, on)
        }
        _ => return false,
    }
    true
}

/// Every parameter as `(id, raw value)`, in wire order. Drives project replay
/// and lets an editor send a whole new params value as edits.
pub fn ui_values(p: &SlicerParams) -> Vec<(&'static str, f32)> {
    let flag = |on: bool| if on { 1.0 } else { 0.0 };
    let voice = quicksampler::ipc::ui_values(&p.voice);
    let mut values: Vec<f32> = VOICE_WIRE
        .iter()
        .map(|&index| voice[index as usize].1)
        .collect();
    values.extend([
        p.first_key as f32,
        flag(p.choke),
        flag(p.one_shot),
        slice_mode_to_wire(p.slice_mode),
        p.sensitivity,
        p.equal_slices as f32,
        beat_division_to_wire(p.beat_division),
        p.sample_bpm,
        p.slice_count as f32,
    ]);
    values.extend(p.slices);
    values.extend(p.slice_gain_db);
    values.extend(p.slice_pitch);
    values.extend((0..MAX_SLICES).map(|index| flag(p.slice_reversed(index))));
    UI_PARAM_IDS.iter().copied().zip(values).collect()
}

/// The wire edits that turn `from` into `to`: only the fields that differ.
pub fn wire_diff(from: &SlicerParams, to: &SlicerParams) -> Vec<(u32, f32)> {
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
    fn ids_are_unique_round_trip_and_share_the_quick_sampler_meaning() {
        let mut seen = std::collections::HashSet::new();
        for (index, id) in UI_PARAM_IDS.iter().enumerate() {
            assert!(seen.insert(*id), "{id} twice");
            assert_eq!(ui_param_index(id), Some(index as u32));
            assert_eq!(ui_param_id(index as u32), Some(*id));
        }
        for (index, &voice_index) in VOICE_WIRE.iter().enumerate() {
            assert_eq!(
                quicksampler::ui_param_id(voice_index),
                Some(UI_PARAM_IDS[index])
            );
        }
        assert_eq!(UI_PARAM_IDS[SLICE_COUNT_WIRE as usize], "sliceCount");
        assert_eq!(UI_PARAM_IDS[FIRST_SLICE_WIRE as usize], "slice0");
        assert_eq!(UI_PARAM_IDS[FIRST_SLICE_GAIN_WIRE as usize - 1], "slice63");
        assert_eq!(UI_PARAM_IDS[FIRST_SLICE_GAIN_WIRE as usize], "sliceGain0");
        assert_eq!(UI_PARAM_IDS[FIRST_SLICE_PITCH_WIRE as usize], "slicePitch0");
        assert_eq!(
            UI_PARAM_IDS[FIRST_SLICE_REVERSE_WIRE as usize],
            "sliceReverse0"
        );
        assert_eq!(UI_PARAM_IDS[PARAM_COUNT - 1], "sliceReverse63");
    }

    #[test]
    fn a_replay_through_the_wire_rebuilds_the_params() {
        let mut saved = SlicerParams {
            first_key: 36,
            choke: false,
            one_shot: false,
            slice_mode: SliceMode::Beat,
            sensitivity: 0.8,
            equal_slices: 16,
            beat_division: BeatDivision::Sixteenth,
            sample_bpm: 92.5,
            slice_count: 4,
            ..SlicerParams::default()
        };
        saved.voice.transpose = -3;
        saved.voice.release_ms = 400.0;
        saved.voice.reverse = true;
        saved.slices[..4].copy_from_slice(&[0.0, 0.1, 0.4, 0.75]);
        saved.slice_gain_db[1] = -6.0;
        saved.slice_pitch[2] = 7.0;
        saved.set_slice_reversed(3, true);
        let mut rebuilt = SlicerParams::default();
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
        let from = SlicerParams::default();
        let mut to = from;
        to.slice_count = 2;
        to.slices[1] = 0.5;
        assert_eq!(
            wire_diff(&from, &to),
            vec![(SLICE_COUNT_WIRE, 2.0), (FIRST_SLICE_WIRE + 1, 0.5)]
        );
    }

    #[test]
    fn bad_wire_edits_are_rejected() {
        let mut p = SlicerParams::default();
        assert!(!apply_wire_param(&mut p, PARAM_COUNT as u32, 0.5));
        assert!(!apply_wire_param(&mut p, 0, f32::NAN));
        assert!(apply_wire_param(&mut p, FIRST_SLICE_WIRE, 7.0));
        assert_eq!(p.slices[0], 1.0);
    }

    #[test]
    fn state_round_trips_and_reorders_a_hand_edit() {
        let mut params = default_params();
        params.sample_name = Some("break.wav".to_string());
        params.slicer.slice_count = 3;
        params.slicer.slices[..3].copy_from_slice(&[0.0, 0.5, 0.25]);
        params.slicer.slice_pitch[1] = 5.0; // the slice at 0.5
        let json = SlicerState::new(params.clone()).to_json().unwrap();
        let restored = SlicerState::from_json(&json).unwrap().params;
        assert_eq!(restored.sample_name, params.sample_name);
        assert_eq!(restored.slicer.points(), &[0.0, 0.25, 0.5]);
        assert_eq!(&restored.slicer.slice_pitch[..3], &[0.0, 0.0, 5.0]);
        let partial = SlicerState::from_json(r#"{"version":1,"params":{}}"#).unwrap();
        assert_eq!(partial.params, default_params());
    }
}

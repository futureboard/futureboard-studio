//! WrapSynth's wire format: the parameter table edits and automation travel
//! by, and the saved state.
//!
//! [`WIRE`] is the single authority. A parameter's index is its position in
//! the table — that index is what the param ring carries and what a project
//! replays — so entries are only ever **appended**: the first 22 are the
//! original WrapSynth's and keep their meaning. Every setter clamps, so a
//! value from the wire, the editor or a saved project lands in range the
//! same way, and [`sanitize_params`] is a get → set round trip.
//!
//! [`apply_wire_param`] is an index into the table — no string lookup — so
//! the host can apply automation on its block path.

use serde::{Deserialize, Serialize};

use crate::lfo::{LfoShape, MAX_RATE_HZ, MIN_RATE_HZ};
use crate::modulation::{MOD_SLOTS, ModDest, ModSource};
use crate::{
    CUTOFF_MAX_HZ, CUTOFF_MIN_HZ, FilterType, MAX_UNISON, Params, VoiceMode, Wavetable,
    default_params, legacy_params,
};

pub const PROTOCOL_VERSION: u32 = 2;
/// 2: the full synth (filter types, envelopes, LFOs, matrix, voicing,
/// effects). A version-1 state is the 22 original controls and reads with
/// the rest at [`legacy_params`].
pub const STATE_VERSION: u32 = 2;

/// One parameter on the wire.
pub struct WireParam {
    pub id: &'static str,
    get: fn(&Params) -> f32,
    set: fn(&mut Params, f32),
}

#[inline]
fn unit(value: f32) -> f32 {
    value.clamp(0.0, 1.0)
}

#[inline]
fn bipolar(value: f32) -> f32 {
    value.clamp(-1.0, 1.0)
}

#[inline]
fn on(value: f32) -> bool {
    value >= 0.5
}

#[inline]
fn flag(value: bool) -> f32 {
    f32::from(value)
}

#[inline]
fn division(value: f32) -> u8 {
    value.round().clamp(0.0, echospace::MAX_DIVISION_WIRE) as u8
}

fn attack(value: f32) -> f32 {
    value.clamp(0.5, 5_000.0)
}

fn decay(value: f32) -> f32 {
    value.clamp(1.0, 10_000.0)
}

fn release(value: f32) -> f32 {
    value.clamp(5.0, 10_000.0)
}

fn lfo_rate(value: f32) -> f32 {
    value.clamp(MIN_RATE_HZ, MAX_RATE_HZ)
}

macro_rules! slot {
    ($n:literal, $source:literal, $dest:literal, $amount:literal) => {
        [
            WireParam {
                id: $source,
                get: |p| p.mod_slots[$n].source.to_wire(),
                set: |p, v| p.mod_slots[$n].source = ModSource::from_wire(v),
            },
            WireParam {
                id: $dest,
                get: |p| p.mod_slots[$n].dest.to_wire(),
                set: |p, v| p.mod_slots[$n].dest = ModDest::from_wire(v),
            },
            WireParam {
                id: $amount,
                get: |p| p.mod_slots[$n].amount,
                set: |p, v| p.mod_slots[$n].amount = bipolar(v),
            },
        ]
    };
}

const BASE: [WireParam; 43] = [
    // ── The original 22, in their original order ──
    WireParam {
        id: "power",
        get: |p| flag(p.power),
        set: |p, v| p.power = on(v),
    },
    WireParam {
        id: "oscAWave",
        get: |p| p.osc_a_wave.to_wire(),
        set: |p, v| p.osc_a_wave = Wavetable::from_wire(v),
    },
    WireParam {
        id: "oscAPosition",
        get: |p| p.osc_a_position,
        set: |p, v| p.osc_a_position = unit(v),
    },
    WireParam {
        id: "oscALevel",
        get: |p| p.osc_a_level,
        set: |p, v| p.osc_a_level = unit(v),
    },
    WireParam {
        id: "oscBWave",
        get: |p| p.osc_b_wave.to_wire(),
        set: |p, v| p.osc_b_wave = Wavetable::from_wire(v),
    },
    WireParam {
        id: "oscBPosition",
        get: |p| p.osc_b_position,
        set: |p, v| p.osc_b_position = unit(v),
    },
    WireParam {
        id: "oscBLevel",
        get: |p| p.osc_b_level,
        set: |p, v| p.osc_b_level = unit(v),
    },
    WireParam {
        id: "oscBSemitones",
        get: |p| p.osc_b_semitones,
        set: |p, v| p.osc_b_semitones = v.clamp(-24.0, 24.0),
    },
    WireParam {
        id: "oscBDetuneCents",
        get: |p| p.osc_b_detune_cents,
        set: |p, v| p.osc_b_detune_cents = v.clamp(-50.0, 50.0),
    },
    WireParam {
        id: "unison",
        get: |p| f32::from(p.unison),
        set: |p, v| p.unison = v.round().clamp(1.0, MAX_UNISON as f32) as u8,
    },
    WireParam {
        id: "unisonDetuneCents",
        get: |p| p.unison_detune_cents,
        set: |p, v| p.unison_detune_cents = v.clamp(0.0, 50.0),
    },
    WireParam {
        id: "stereoWidth",
        get: |p| p.stereo_width,
        set: |p, v| p.stereo_width = unit(v),
    },
    WireParam {
        id: "subLevel",
        get: |p| p.sub_level,
        set: |p, v| p.sub_level = unit(v),
    },
    WireParam {
        id: "noiseLevel",
        get: |p| p.noise_level,
        set: |p, v| p.noise_level = unit(v),
    },
    WireParam {
        id: "cutoffHz",
        get: |p| p.cutoff_hz,
        set: |p, v| p.cutoff_hz = v.clamp(CUTOFF_MIN_HZ, CUTOFF_MAX_HZ),
    },
    WireParam {
        id: "resonance",
        get: |p| p.resonance,
        set: |p, v| p.resonance = v.clamp(0.0, 0.95),
    },
    WireParam {
        id: "filterDrive",
        get: |p| p.filter_drive,
        set: |p, v| p.filter_drive = unit(v),
    },
    WireParam {
        id: "attackMs",
        get: |p| p.attack_ms,
        set: |p, v| p.attack_ms = attack(v),
    },
    WireParam {
        id: "decayMs",
        get: |p| p.decay_ms,
        set: |p, v| p.decay_ms = v.clamp(1.0, 5_000.0),
    },
    WireParam {
        id: "sustain",
        get: |p| p.sustain,
        set: |p, v| p.sustain = unit(v),
    },
    WireParam {
        id: "releaseMs",
        get: |p| p.release_ms,
        set: |p, v| p.release_ms = v.clamp(5.0, 8_000.0),
    },
    WireParam {
        id: "masterDb",
        get: |p| p.master_db,
        set: |p, v| p.master_db = v.clamp(-24.0, 3.0),
    },
    // ── Filter (22..) ──
    WireParam {
        id: "filterType",
        get: |p| p.filter_type.to_wire(),
        set: |p, v| p.filter_type = FilterType::from_wire(v),
    },
    WireParam {
        id: "filterKeyTrack",
        get: |p| p.filter_key_track,
        set: |p, v| p.filter_key_track = unit(v),
    },
    WireParam {
        id: "filterEnvAmount",
        get: |p| p.filter_env_amount,
        set: |p, v| p.filter_env_amount = bipolar(v),
    },
    WireParam {
        id: "filterAttackMs",
        get: |p| p.filter_attack_ms,
        set: |p, v| p.filter_attack_ms = attack(v),
    },
    WireParam {
        id: "filterDecayMs",
        get: |p| p.filter_decay_ms,
        set: |p, v| p.filter_decay_ms = decay(v),
    },
    WireParam {
        id: "filterSustain",
        get: |p| p.filter_sustain,
        set: |p, v| p.filter_sustain = unit(v),
    },
    WireParam {
        id: "filterReleaseMs",
        get: |p| p.filter_release_ms,
        set: |p, v| p.filter_release_ms = release(v),
    },
    // ── Mod envelope (29..) ──
    WireParam {
        id: "modAttackMs",
        get: |p| p.mod_attack_ms,
        set: |p, v| p.mod_attack_ms = attack(v),
    },
    WireParam {
        id: "modDecayMs",
        get: |p| p.mod_decay_ms,
        set: |p, v| p.mod_decay_ms = decay(v),
    },
    WireParam {
        id: "modSustain",
        get: |p| p.mod_sustain,
        set: |p, v| p.mod_sustain = unit(v),
    },
    WireParam {
        id: "modReleaseMs",
        get: |p| p.mod_release_ms,
        set: |p, v| p.mod_release_ms = release(v),
    },
    // ── LFOs (33..) ──
    WireParam {
        id: "lfo1Shape",
        get: |p| p.lfo1_shape.to_wire(),
        set: |p, v| p.lfo1_shape = LfoShape::from_wire(v),
    },
    WireParam {
        id: "lfo1RateHz",
        get: |p| p.lfo1_rate_hz,
        set: |p, v| p.lfo1_rate_hz = lfo_rate(v),
    },
    WireParam {
        id: "lfo1Sync",
        get: |p| flag(p.lfo1_sync),
        set: |p, v| p.lfo1_sync = on(v),
    },
    WireParam {
        id: "lfo1Division",
        get: |p| f32::from(p.lfo1_division),
        set: |p, v| p.lfo1_division = division(v),
    },
    WireParam {
        id: "lfo1Retrigger",
        get: |p| flag(p.lfo1_retrigger),
        set: |p, v| p.lfo1_retrigger = on(v),
    },
    WireParam {
        id: "lfo2Shape",
        get: |p| p.lfo2_shape.to_wire(),
        set: |p, v| p.lfo2_shape = LfoShape::from_wire(v),
    },
    WireParam {
        id: "lfo2RateHz",
        get: |p| p.lfo2_rate_hz,
        set: |p, v| p.lfo2_rate_hz = lfo_rate(v),
    },
    WireParam {
        id: "lfo2Sync",
        get: |p| flag(p.lfo2_sync),
        set: |p, v| p.lfo2_sync = on(v),
    },
    WireParam {
        id: "lfo2Division",
        get: |p| f32::from(p.lfo2_division),
        set: |p, v| p.lfo2_division = division(v),
    },
    WireParam {
        id: "lfo2Retrigger",
        get: |p| flag(p.lfo2_retrigger),
        set: |p, v| p.lfo2_retrigger = on(v),
    },
];

// ── Modulation matrix (43..67): source, destination, amount per slot ──
const SLOTS: [[WireParam; 3]; MOD_SLOTS] = [
    slot!(0, "mod1Source", "mod1Dest", "mod1Amount"),
    slot!(1, "mod2Source", "mod2Dest", "mod2Amount"),
    slot!(2, "mod3Source", "mod3Dest", "mod3Amount"),
    slot!(3, "mod4Source", "mod4Dest", "mod4Amount"),
    slot!(4, "mod5Source", "mod5Dest", "mod5Amount"),
    slot!(5, "mod6Source", "mod6Dest", "mod6Amount"),
    slot!(6, "mod7Source", "mod7Dest", "mod7Amount"),
    slot!(7, "mod8Source", "mod8Dest", "mod8Amount"),
];

const TAIL: [WireParam; 26] = [
    // ── Voicing (67..) ──
    WireParam {
        id: "voiceMode",
        get: |p| p.voice_mode.to_wire(),
        set: |p, v| p.voice_mode = VoiceMode::from_wire(v),
    },
    WireParam {
        id: "glideMs",
        get: |p| p.glide_ms,
        set: |p, v| p.glide_ms = v.clamp(0.0, 2_000.0),
    },
    WireParam {
        id: "pitchBendRange",
        get: |p| p.pitch_bend_range,
        set: |p, v| p.pitch_bend_range = v.round().clamp(0.0, 24.0),
    },
    WireParam {
        id: "velocityAmount",
        get: |p| p.velocity_amount,
        set: |p, v| p.velocity_amount = unit(v),
    },
    WireParam {
        id: "unisonBlend",
        get: |p| p.unison_blend,
        set: |p, v| p.unison_blend = unit(v),
    },
    WireParam {
        id: "analog",
        get: |p| p.analog,
        set: |p, v| p.analog = unit(v),
    },
    WireParam {
        id: "phaseRandom",
        get: |p| p.phase_random,
        set: |p, v| p.phase_random = unit(v),
    },
    // ── Effects (74..) ──
    WireParam {
        id: "distOn",
        get: |p| flag(p.dist_on),
        set: |p, v| p.dist_on = on(v),
    },
    WireParam {
        id: "distDrive",
        get: |p| p.dist_drive,
        set: |p, v| p.dist_drive = unit(v),
    },
    WireParam {
        id: "distMix",
        get: |p| p.dist_mix,
        set: |p, v| p.dist_mix = unit(v),
    },
    WireParam {
        id: "chorusOn",
        get: |p| flag(p.chorus_on),
        set: |p, v| p.chorus_on = on(v),
    },
    WireParam {
        id: "chorusRateHz",
        get: |p| p.chorus_rate_hz,
        set: |p, v| p.chorus_rate_hz = v.clamp(0.05, 5.0),
    },
    WireParam {
        id: "chorusDepth",
        get: |p| p.chorus_depth,
        set: |p, v| p.chorus_depth = unit(v),
    },
    WireParam {
        id: "chorusMix",
        get: |p| p.chorus_mix,
        set: |p, v| p.chorus_mix = unit(v),
    },
    WireParam {
        id: "delayOn",
        get: |p| flag(p.delay_on),
        set: |p, v| p.delay_on = on(v),
    },
    WireParam {
        id: "delaySync",
        get: |p| flag(p.delay_sync),
        set: |p, v| p.delay_sync = on(v),
    },
    WireParam {
        id: "delayDivision",
        get: |p| f32::from(p.delay_division),
        set: |p, v| p.delay_division = division(v),
    },
    WireParam {
        id: "delayTimeMs",
        get: |p| p.delay_time_ms,
        set: |p, v| p.delay_time_ms = v.clamp(1.0, echospace::MAX_DELAY_MS),
    },
    WireParam {
        id: "delayFeedback",
        get: |p| p.delay_feedback,
        set: |p, v| p.delay_feedback = v.clamp(0.0, 0.95),
    },
    WireParam {
        id: "delayMix",
        get: |p| p.delay_mix,
        set: |p, v| p.delay_mix = unit(v),
    },
    WireParam {
        id: "delayPingPong",
        get: |p| flag(p.delay_ping_pong),
        set: |p, v| p.delay_ping_pong = on(v),
    },
    WireParam {
        id: "reverbOn",
        get: |p| flag(p.reverb_on),
        set: |p, v| p.reverb_on = on(v),
    },
    WireParam {
        id: "reverbSize",
        get: |p| p.reverb_size,
        set: |p, v| p.reverb_size = unit(v),
    },
    WireParam {
        id: "reverbDecaySec",
        get: |p| p.reverb_decay_sec,
        set: |p, v| p.reverb_decay_sec = v.clamp(0.1, 20.0),
    },
    WireParam {
        id: "reverbDamping",
        get: |p| p.reverb_damping,
        set: |p, v| p.reverb_damping = unit(v),
    },
    WireParam {
        id: "reverbMix",
        get: |p| p.reverb_mix,
        set: |p, v| p.reverb_mix = unit(v),
    },
];

pub const WIRE_COUNT: usize = BASE.len() + MOD_SLOTS * 3 + TAIL.len();

/// The whole table, in wire order.
pub fn wire_param(index: u32) -> Option<&'static WireParam> {
    let index = index as usize;
    if index < BASE.len() {
        return Some(&BASE[index]);
    }
    let index = index - BASE.len();
    if index < MOD_SLOTS * 3 {
        return Some(&SLOTS[index / 3][index % 3]);
    }
    TAIL.get(index - MOD_SLOTS * 3)
}

/// Every id in wire order.
pub const UI_PARAM_IDS: [&str; WIRE_COUNT] = {
    let mut ids = [""; WIRE_COUNT];
    let mut i = 0;
    while i < BASE.len() {
        ids[i] = BASE[i].id;
        i += 1;
    }
    let mut slot = 0;
    while slot < MOD_SLOTS {
        let mut part = 0;
        while part < 3 {
            ids[BASE.len() + slot * 3 + part] = SLOTS[slot][part].id;
            part += 1;
        }
        slot += 1;
    }
    let mut j = 0;
    while j < TAIL.len() {
        ids[BASE.len() + MOD_SLOTS * 3 + j] = TAIL[j].id;
        j += 1;
    }
    ids
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WrapSynthState {
    pub version: u32,
    pub params: Params,
}

impl WrapSynthState {
    pub fn new(params: Params) -> Self {
        Self {
            version: STATE_VERSION,
            params,
        }
    }

    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// Reads a state of any version: missing fields take [`legacy_params`],
    /// and every value is brought into range.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        let mut state: Self = serde_json::from_str(json)?;
        sanitize_params(&mut state.params);
        Ok(state)
    }
}

impl Default for WrapSynthState {
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

/// Bring every field into range; a non-finite value takes the legacy
/// default of its parameter.
pub fn sanitize_params(params: &mut Params) {
    let fallback = legacy_params();
    for index in 0..WIRE_COUNT as u32 {
        let Some(wire) = wire_param(index) else {
            continue;
        };
        let mut value = (wire.get)(params);
        if !value.is_finite() {
            value = (wire.get)(&fallback);
        }
        (wire.set)(params, value);
    }
}

/// Apply one wire edit. Allocation- and lookup-free: an index into the
/// table. `false` for an unknown index or a non-finite value.
#[inline]
pub fn apply_wire_param(params: &mut Params, index: u32, value: f32) -> bool {
    if !value.is_finite() {
        return false;
    }
    match wire_param(index) {
        Some(wire) => {
            (wire.set)(params, value);
            true
        }
        None => false,
    }
}

/// The wire value of parameter `index`.
pub fn wire_value(params: &Params, index: u32) -> Option<f32> {
    wire_param(index).map(|wire| (wire.get)(params))
}

/// Every parameter's id and value, in wire order: what a restarted host is
/// replayed.
pub fn ui_values(params: &Params) -> Vec<(&'static str, f32)> {
    (0..WIRE_COUNT as u32)
        .filter_map(|index| wire_param(index).map(|wire| (wire.id, (wire.get)(params))))
        .collect()
}

/// The wire edits that turn `old` into `new`, as `(index, value)`.
pub fn wire_diff(old: &Params, new: &Params) -> Vec<(u32, f32)> {
    (0..WIRE_COUNT as u32)
        .filter_map(|index| {
            let wire = wire_param(index)?;
            let after = (wire.get)(new);
            ((wire.get)(old) != after).then_some((index, after))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_original_indices_keep_their_meaning() {
        let original = [
            "power",
            "oscAWave",
            "oscAPosition",
            "oscALevel",
            "oscBWave",
            "oscBPosition",
            "oscBLevel",
            "oscBSemitones",
            "oscBDetuneCents",
            "unison",
            "unisonDetuneCents",
            "stereoWidth",
            "subLevel",
            "noiseLevel",
            "cutoffHz",
            "resonance",
            "filterDrive",
            "attackMs",
            "decayMs",
            "sustain",
            "releaseMs",
            "masterDb",
        ];
        for (index, id) in original.iter().enumerate() {
            assert_eq!(UI_PARAM_IDS[index], *id);
        }
        // The old wave wire values are the legacy tables.
        assert_eq!(Wavetable::from_wire(0.0), Wavetable::Saw);
        assert_eq!(Wavetable::from_wire(3.0), Wavetable::Sine);
    }

    #[test]
    fn ids_are_unique_and_round_trip() {
        for (index, id) in UI_PARAM_IDS.iter().enumerate() {
            assert!(!id.is_empty(), "{index}");
            assert_eq!(ui_param_index(id), Some(index as u32), "{id}");
            assert_eq!(ui_param_id(index as u32), Some(*id));
        }
        assert_eq!(ui_param_id(WIRE_COUNT as u32), None);
    }

    #[test]
    fn a_diff_carries_only_what_changed_and_replays_exactly() {
        let old = default_params();
        assert!(wire_diff(&old, &old).is_empty());
        let mut new = old;
        new.cutoff_hz = 900.0;
        new.osc_b_wave = Wavetable::Formant;
        new.mod_slots[2] = crate::ModSlot {
            source: ModSource::Lfo1,
            dest: ModDest::FilterCutoff,
            amount: -0.4,
        };
        new.reverb_on = true;
        let diff = wire_diff(&old, &new);
        assert_eq!(diff.len(), 6, "{diff:?}");
        let mut applied = old;
        for (index, value) in diff {
            assert!(apply_wire_param(&mut applied, index, value));
        }
        assert_eq!(applied, new);
    }

    #[test]
    fn edits_clamp_and_bad_values_are_refused() {
        let mut params = default_params();
        assert!(apply_wire_param(
            &mut params,
            ui_param_index("unison").unwrap(),
            99.0
        ));
        assert_eq!(params.unison, MAX_UNISON as u8);
        assert!(apply_wire_param(
            &mut params,
            ui_param_index("delayFeedback").unwrap(),
            5.0
        ));
        assert_eq!(params.delay_feedback, 0.95);
        assert!(!apply_wire_param(&mut params, u32::MAX, 1.0));
        assert!(!apply_wire_param(&mut params, 0, f32::NAN));
    }

    #[test]
    fn a_first_version_state_loads_as_the_patch_it_was() {
        let json = r#"{"version":1,"params":{"power":true,"oscAWave":"square","oscAPosition":0.3,
            "oscALevel":0.5,"oscBWave":"sine","oscBPosition":0.1,"oscBLevel":0.2,"oscBSemitones":7.0,
            "oscBDetuneCents":-3.0,"unison":5,"unisonDetuneCents":20.0,"stereoWidth":0.4,
            "subLevel":0.1,"noiseLevel":0.0,"cutoffHz":1200.0,"resonance":0.5,"filterDrive":0.0,
            "attackMs":12.0,"decayMs":300.0,"sustain":0.6,"releaseMs":800.0,"masterDb":-10.0}}"#;
        let state = WrapSynthState::from_json(json).expect("old state reads");
        let p = state.params;
        assert_eq!(p.osc_a_wave, Wavetable::Square);
        assert_eq!(p.osc_b_wave, Wavetable::Sine);
        assert_eq!((p.unison, p.cutoff_hz, p.master_db), (5, 1_200.0, -10.0));
        // Everything new sits where it changes nothing.
        let legacy = legacy_params();
        assert_eq!(p.filter_type, FilterType::LowPass12);
        assert_eq!(p.filter_env_amount, 0.0);
        assert_eq!(p.mod_slots, legacy.mod_slots);
        assert!(!p.reverb_on && !p.delay_on && !p.chorus_on && !p.dist_on);
    }

    #[test]
    fn state_round_trips_and_sanitizes() {
        let mut params = default_params();
        params.cutoff_hz = 777.0;
        params.mod_slots[0].amount = 0.25;
        let json = WrapSynthState::new(params).to_json().unwrap();
        let back = WrapSynthState::from_json(&json).unwrap();
        assert_eq!(back.version, STATE_VERSION);
        assert_eq!(back.params, params);

        let mut broken = default_params();
        broken.resonance = f32::NAN;
        broken.glide_ms = 1.0e9;
        sanitize_params(&mut broken);
        assert_eq!(broken.resonance, legacy_params().resonance);
        assert_eq!(broken.glide_ms, 2_000.0);
    }
}

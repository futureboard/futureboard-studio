//! Stable editor/DSP wire contract. Indices and IDs are append-only.

use builtin_dsp_core::clamp;
use serde::de::Error as _;
use serde::{Deserialize, Serialize};

use crate::{Params, default_params};

pub const PROTOCOL_VERSION: u32 = 1;
pub const STATE_VERSION: u32 = 1;

pub const POWER_INDEX: u32 = 0;
pub const INPUT_TRIM_INDEX: u32 = 1;
pub const FILTERS_ENABLED_INDEX: u32 = 2;
pub const HPF_INDEX: u32 = 3;
pub const LPF_INDEX: u32 = 4;
pub const EQ_ENABLED_INDEX: u32 = 5;
pub const LOW_GAIN_INDEX: u32 = 6;
pub const LOW_MID_FREQ_INDEX: u32 = 7;
pub const LOW_MID_GAIN_INDEX: u32 = 8;
pub const HIGH_MID_FREQ_INDEX: u32 = 9;
pub const HIGH_MID_GAIN_INDEX: u32 = 10;
pub const HIGH_GAIN_INDEX: u32 = 11;
pub const COMP_ENABLED_INDEX: u32 = 12;
pub const COMP_THRESHOLD_INDEX: u32 = 13;
pub const COMP_RATIO_INDEX: u32 = 14;
pub const COMP_ATTACK_INDEX: u32 = 15;
pub const COMP_RELEASE_INDEX: u32 = 16;
pub const COMP_MAKEUP_INDEX: u32 = 17;
pub const SAT_ENABLED_INDEX: u32 = 18;
pub const SAT_DRIVE_INDEX: u32 = 19;
pub const SAT_CHARACTER_INDEX: u32 = 20;
pub const WIDTH_ENABLED_INDEX: u32 = 21;
pub const WIDTH_INDEX: u32 = 22;
pub const OUTPUT_TRIM_INDEX: u32 = 23;
pub const LIMITER_ENABLED_INDEX: u32 = 24;
pub const LIMITER_CEILING_INDEX: u32 = 25;
pub const LIMITER_RELEASE_INDEX: u32 = 26;
pub const SLOT_1_INDEX: u32 = 27;
pub const SLOT_2_INDEX: u32 = 28;
pub const SLOT_3_INDEX: u32 = 29;
pub const SLOT_4_INDEX: u32 = 30;
pub const SLOT_5_INDEX: u32 = 31;
pub const SLOT_6_INDEX: u32 = 32;
// Per-module output trim. Keyed by module, not by rack position, so dragging a
// stage up or down the chain carries its trim with it.
pub const FILTERS_TRIM_INDEX: u32 = 33;
pub const EQ_TRIM_INDEX: u32 = 34;
pub const COMP_TRIM_INDEX: u32 = 35;
pub const SAT_TRIM_INDEX: u32 = 36;
pub const WIDTH_TRIM_INDEX: u32 = 37;
pub const LIMITER_TRIM_INDEX: u32 = 38;

pub const PARAM_COUNT: usize = 39;

/// Wire-only, not a parameter: the whole rack assignment in one edit, the
/// six slot codes packed as base-7 digits, slot 1 lowest. A reorder sent as
/// one wire edit per slot could reach the DSP half-applied — a block between
/// the edits faded to a rack missing a module, then faded again and restarted
/// that module from rest (#354). This lands the whole order at once.
///
/// Kept clear of the append-only parameter indices. Never listed, automated
/// or stored: the slot params remain the state.
pub const RACK_ORDER_INDEX: u32 = 1024;
const RACK_CODES: u32 = 7;
const RACK_ORDER_MAX: u32 = 117_648; // 7^6 - 1

/// Whether `index` is something the wire carries: a parameter or the rack
/// order.
pub fn is_wire_index(index: u32) -> bool {
    (index as usize) < PARAM_COUNT || index == RACK_ORDER_INDEX
}

/// The rack assignment as a [`RACK_ORDER_INDEX`] value. Exact in `f32`.
pub fn rack_order_value(params: &Params) -> f32 {
    [
        params.slot1_module,
        params.slot2_module,
        params.slot3_module,
        params.slot4_module,
        params.slot5_module,
        params.slot6_module,
    ]
    .iter()
    .rev()
    .fold(0u32, |packed, &code| {
        packed * RACK_CODES + u32::from(code).min(RACK_CODES - 1)
    }) as f32
}

/// Set all six slots from a [`RACK_ORDER_INDEX`] value. A module listed twice
/// keeps its first slot, as [`sanitize_params`] does.
fn apply_rack_order(params: &mut Params, input: f32) -> bool {
    if !(0.0..=RACK_ORDER_MAX as f32).contains(&input) || input.fract() != 0.0 {
        return false;
    }
    let mut packed = input as u32;
    let mut seen = [false; RACK_CODES as usize];
    for slot in [
        &mut params.slot1_module,
        &mut params.slot2_module,
        &mut params.slot3_module,
        &mut params.slot4_module,
        &mut params.slot5_module,
        &mut params.slot6_module,
    ] {
        let code = (packed % RACK_CODES) as usize;
        packed /= RACK_CODES;
        *slot = if code != 0 && !seen[code] {
            seen[code] = true;
            code as u8
        } else {
            0
        };
    }
    true
}

pub const UI_PARAM_IDS: [&str; PARAM_COUNT] = [
    "power",
    "inputTrimDb",
    "filtersEnabled",
    "hpfHz",
    "lpfHz",
    "eqEnabled",
    "lowGainDb",
    "lowMidFreqHz",
    "lowMidGainDb",
    "highMidFreqHz",
    "highMidGainDb",
    "highGainDb",
    "compEnabled",
    "compThresholdDb",
    "compRatio",
    "compAttackMs",
    "compReleaseMs",
    "compMakeupDb",
    "satEnabled",
    "satDrivePct",
    "satCharacterPct",
    "widthEnabled",
    "widthPct",
    "outputTrimDb",
    "limiterEnabled",
    "limiterCeilingDb",
    "limiterReleaseMs",
    "slot1Module",
    "slot2Module",
    "slot3Module",
    "slot4Module",
    "slot5Module",
    "slot6Module",
    "filtersTrimDb",
    "eqTrimDb",
    "compTrimDb",
    "satTrimDb",
    "widthTrimDb",
    "limiterTrimDb",
];

const RANGES: [(f32, f32); PARAM_COUNT] = [
    (0.0, 1.0),
    (-24.0, 24.0),
    (0.0, 1.0),
    (20.0, 500.0),
    (1_000.0, 20_000.0),
    (0.0, 1.0),
    (-18.0, 18.0),
    (80.0, 2_000.0),
    (-18.0, 18.0),
    (500.0, 12_000.0),
    (-18.0, 18.0),
    (-18.0, 18.0),
    (0.0, 1.0),
    (-60.0, 0.0),
    (1.0, 20.0),
    (0.1, 100.0),
    (10.0, 1_000.0),
    (-12.0, 24.0),
    (0.0, 1.0),
    (0.0, 100.0),
    (0.0, 100.0),
    (0.0, 1.0),
    (0.0, 200.0),
    (-24.0, 24.0),
    (0.0, 1.0),
    (-12.0, 0.0),
    (10.0, 1_000.0),
    (0.0, 6.0),
    (0.0, 6.0),
    (0.0, 6.0),
    (0.0, 6.0),
    (0.0, 6.0),
    (0.0, 6.0),
    (-24.0, 24.0),
    (-24.0, 24.0),
    (-24.0, 24.0),
    (-24.0, 24.0),
    (-24.0, 24.0),
    (-24.0, 24.0),
];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MixStationState {
    pub version: u32,
    pub params: Params,
}

impl MixStationState {
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
        let state: Self = serde_json::from_str(json)?;
        if state.version != STATE_VERSION {
            return Err(serde_json::Error::custom(format!(
                "unsupported MixStation state version {}",
                state.version
            )));
        }
        Ok(state)
    }
}

impl Default for MixStationState {
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

#[inline]
fn bounded(index: u32, input: f32) -> f32 {
    let (min, max) = RANGES[index as usize];
    clamp(input, min, max)
}

fn assign_slot(params: &mut Params, index: u32, module: u8) {
    if module != 0 {
        if index != SLOT_1_INDEX && params.slot1_module == module {
            params.slot1_module = 0;
        }
        if index != SLOT_2_INDEX && params.slot2_module == module {
            params.slot2_module = 0;
        }
        if index != SLOT_3_INDEX && params.slot3_module == module {
            params.slot3_module = 0;
        }
        if index != SLOT_4_INDEX && params.slot4_module == module {
            params.slot4_module = 0;
        }
        if index != SLOT_5_INDEX && params.slot5_module == module {
            params.slot5_module = 0;
        }
        if index != SLOT_6_INDEX && params.slot6_module == module {
            params.slot6_module = 0;
        }
    }
    match index {
        SLOT_1_INDEX => params.slot1_module = module,
        SLOT_2_INDEX => params.slot2_module = module,
        SLOT_3_INDEX => params.slot3_module = module,
        SLOT_4_INDEX => params.slot4_module = module,
        SLOT_5_INDEX => params.slot5_module = module,
        SLOT_6_INDEX => params.slot6_module = module,
        _ => {}
    }
}

pub fn sanitize_params(params: &mut Params) {
    let values = ui_values(params);
    let defaults = ui_values(&default_params());
    for (index, input) in values.into_iter().enumerate() {
        let safe = if input.is_finite() {
            input
        } else {
            defaults[index]
        };
        let _ = apply_wire_param(params, index as u32, safe);
    }
    let mut seen = [false; 7];
    for slot in [
        &mut params.slot1_module,
        &mut params.slot2_module,
        &mut params.slot3_module,
        &mut params.slot4_module,
        &mut params.slot5_module,
        &mut params.slot6_module,
    ] {
        let module = *slot as usize;
        if module != 0 {
            if seen[module] {
                *slot = 0;
            } else {
                seen[module] = true;
            }
        }
    }
}

pub fn apply_wire_param(params: &mut Params, index: u32, input: f32) -> bool {
    if index == RACK_ORDER_INDEX {
        return apply_rack_order(params, input);
    }
    if !input.is_finite() || index as usize >= PARAM_COUNT {
        return false;
    }
    let v = bounded(index, input);
    match index {
        POWER_INDEX => params.power = v >= 0.5,
        INPUT_TRIM_INDEX => params.input_trim_db = v,
        FILTERS_ENABLED_INDEX => params.filters_enabled = v >= 0.5,
        HPF_INDEX => params.hpf_hz = v,
        LPF_INDEX => params.lpf_hz = v,
        EQ_ENABLED_INDEX => params.eq_enabled = v >= 0.5,
        LOW_GAIN_INDEX => params.low_gain_db = v,
        LOW_MID_FREQ_INDEX => params.low_mid_freq_hz = v,
        LOW_MID_GAIN_INDEX => params.low_mid_gain_db = v,
        HIGH_MID_FREQ_INDEX => params.high_mid_freq_hz = v,
        HIGH_MID_GAIN_INDEX => params.high_mid_gain_db = v,
        HIGH_GAIN_INDEX => params.high_gain_db = v,
        COMP_ENABLED_INDEX => params.comp_enabled = v >= 0.5,
        COMP_THRESHOLD_INDEX => params.comp_threshold_db = v,
        COMP_RATIO_INDEX => params.comp_ratio = v,
        COMP_ATTACK_INDEX => params.comp_attack_ms = v,
        COMP_RELEASE_INDEX => params.comp_release_ms = v,
        COMP_MAKEUP_INDEX => params.comp_makeup_db = v,
        SAT_ENABLED_INDEX => params.sat_enabled = v >= 0.5,
        SAT_DRIVE_INDEX => params.sat_drive_pct = v,
        SAT_CHARACTER_INDEX => params.sat_character_pct = v,
        WIDTH_ENABLED_INDEX => params.width_enabled = v >= 0.5,
        WIDTH_INDEX => params.width_pct = v,
        OUTPUT_TRIM_INDEX => params.output_trim_db = v,
        LIMITER_ENABLED_INDEX => params.limiter_enabled = v >= 0.5,
        LIMITER_CEILING_INDEX => params.limiter_ceiling_db = v,
        LIMITER_RELEASE_INDEX => params.limiter_release_ms = v,
        SLOT_1_INDEX | SLOT_2_INDEX | SLOT_3_INDEX | SLOT_4_INDEX | SLOT_5_INDEX | SLOT_6_INDEX => {
            assign_slot(params, index, v.round() as u8)
        }
        FILTERS_TRIM_INDEX => params.filters_trim_db = v,
        EQ_TRIM_INDEX => params.eq_trim_db = v,
        COMP_TRIM_INDEX => params.comp_trim_db = v,
        SAT_TRIM_INDEX => params.sat_trim_db = v,
        WIDTH_TRIM_INDEX => params.width_trim_db = v,
        LIMITER_TRIM_INDEX => params.limiter_trim_db = v,
        _ => return false,
    }
    true
}

pub fn apply_ui_param(params: &mut Params, id: &str, input: f32) -> bool {
    ui_param_index(id).is_some_and(|index| apply_wire_param(params, index, input))
}

pub fn ui_values(params: &Params) -> [f32; PARAM_COUNT] {
    [
        f32::from(params.power),
        params.input_trim_db,
        f32::from(params.filters_enabled),
        params.hpf_hz,
        params.lpf_hz,
        f32::from(params.eq_enabled),
        params.low_gain_db,
        params.low_mid_freq_hz,
        params.low_mid_gain_db,
        params.high_mid_freq_hz,
        params.high_mid_gain_db,
        params.high_gain_db,
        f32::from(params.comp_enabled),
        params.comp_threshold_db,
        params.comp_ratio,
        params.comp_attack_ms,
        params.comp_release_ms,
        params.comp_makeup_db,
        f32::from(params.sat_enabled),
        params.sat_drive_pct,
        params.sat_character_pct,
        f32::from(params.width_enabled),
        params.width_pct,
        params.output_trim_db,
        f32::from(params.limiter_enabled),
        params.limiter_ceiling_db,
        params.limiter_release_ms,
        params.slot1_module as f32,
        params.slot2_module as f32,
        params.slot3_module as f32,
        params.slot4_module as f32,
        params.slot5_module as f32,
        params.slot6_module as f32,
        params.filters_trim_db,
        params.eq_trim_db,
        params.comp_trim_db,
        params.sat_trim_db,
        params.width_trim_db,
        params.limiter_trim_db,
    ]
}

/// Per-module trim wire index for a rack module code (`1..=6`).
pub const fn module_trim_index(module: u8) -> Option<u32> {
    match module {
        1 => Some(FILTERS_TRIM_INDEX),
        2 => Some(EQ_TRIM_INDEX),
        3 => Some(COMP_TRIM_INDEX),
        4 => Some(SAT_TRIM_INDEX),
        5 => Some(WIDTH_TRIM_INDEX),
        6 => Some(LIMITER_TRIM_INDEX),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip_in_wire_order() {
        for (index, id) in UI_PARAM_IDS.iter().enumerate() {
            assert_eq!(ui_param_index(id), Some(index as u32));
            assert_eq!(ui_param_id(index as u32), Some(*id));
        }
    }

    #[test]
    fn state_round_trips_as_json() {
        let mut params = default_params();
        params.low_mid_gain_db = 5.5;
        params.sat_enabled = false;
        let json = MixStationState::new(params).to_json().unwrap();
        let decoded = MixStationState::from_json(&json).unwrap();
        assert_eq!(decoded.version, STATE_VERSION);
        assert_eq!(decoded.params.low_mid_gain_db, 5.5);
        assert!(!decoded.params.sat_enabled);
    }

    #[test]
    fn invalid_updates_are_rejected_and_ranges_are_clamped() {
        let mut params = default_params();
        assert!(!apply_wire_param(&mut params, u32::MAX, 0.0));
        assert!(!apply_wire_param(&mut params, WIDTH_INDEX, f32::NAN));
        assert!(apply_wire_param(&mut params, WIDTH_INDEX, 999.0));
        assert_eq!(params.width_pct, 200.0);
    }

    #[test]
    fn whole_state_sanitization_replaces_non_finite_values() {
        let mut params = default_params();
        params.hpf_hz = f32::NAN;
        params.comp_ratio = f32::INFINITY;
        sanitize_params(&mut params);
        assert_eq!(params.hpf_hz, default_params().hpf_hz);
        assert_eq!(params.comp_ratio, default_params().comp_ratio);
    }

    #[test]
    fn unsupported_state_versions_are_rejected() {
        let json = MixStationState {
            version: STATE_VERSION + 1,
            params: default_params(),
        }
        .to_json()
        .unwrap();
        assert!(MixStationState::from_json(&json).is_err());
    }

    #[test]
    fn moving_a_module_to_any_slot_keeps_it_unique() {
        let mut params = default_params();
        assert!(apply_wire_param(&mut params, SLOT_1_INDEX, 3.0));
        assert!(apply_wire_param(&mut params, SLOT_4_INDEX, 3.0));
        assert_eq!(params.slot1_module, 0);
        assert_eq!(params.slot4_module, 3);
    }

    /// #354: the rack order carries all six slots in one exact value, keeps
    /// a module unique, and rejects anything that is not an order.
    #[test]
    fn the_rack_order_sets_every_slot_at_once() {
        let mut target = default_params();
        [
            target.slot1_module,
            target.slot2_module,
            target.slot3_module,
            target.slot4_module,
            target.slot5_module,
            target.slot6_module,
        ] = [6, 5, 4, 3, 2, 1];
        let value = rack_order_value(&target);
        let mut params = default_params();
        params.slot1_module = 1;
        assert!(apply_wire_param(&mut params, RACK_ORDER_INDEX, value));
        assert_eq!(
            ui_values(&params)[SLOT_1_INDEX as usize..=SLOT_6_INDEX as usize],
            [6.0, 5.0, 4.0, 3.0, 2.0, 1.0]
        );
        assert_eq!(rack_order_value(&params), value);

        // [3, 3, 0, ...]: the second 3 is dropped.
        assert!(apply_wire_param(
            &mut params,
            RACK_ORDER_INDEX,
            3.0 + 3.0 * 7.0
        ));
        assert_eq!((params.slot1_module, params.slot2_module), (3, 0));
        assert_eq!(params.slot6_module, 0);

        let before = params.clone();
        for bad in [-1.0, 0.5, 117_649.0, f32::NAN, f32::INFINITY] {
            assert!(
                !apply_wire_param(&mut params, RACK_ORDER_INDEX, bad),
                "{bad}"
            );
        }
        assert_eq!(rack_order_value(&params), rack_order_value(&before));
        assert!(is_wire_index(RACK_ORDER_INDEX));
        assert!(!is_wire_index(PARAM_COUNT as u32));
        assert_eq!(ui_param_id(RACK_ORDER_INDEX), None);
    }
}

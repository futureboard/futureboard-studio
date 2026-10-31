//! Stable editor/DSP parameter wire contract for Duckstep.
//!
//! String ids live at the UI/control boundary only. The native host resolves
//! them to compact indices before publishing edits to an audio-side bounded
//! queue; [`Dsp::apply_wire_param`](crate::Dsp::apply_wire_param) consumes the
//! numeric form without allocation, serialization, locking, or string lookup.
//!
//! The shaper's points are fixed slots — a count, then every x, then every y
//! — so a drag, an added point or a loaded shape travels as ordinary edits
//! and automation, saves, and undoes like any knob.

use builtin_dsp_core::clamp;
use serde::{Deserialize, Serialize};

use crate::{
    Band, Feel, MAX_POINTS, Mode, Monitor, Params, Rate, ReleaseSync, Source, Trigger,
    default_params,
};

pub const PROTOCOL_VERSION: u32 = 1;
pub const STATE_VERSION: u32 = 1;

pub const POWER_INDEX: u32 = 0;
pub const MODE_INDEX: u32 = 1;
pub const LISTEN_INDEX: u32 = 2;
pub const SOURCE_INDEX: u32 = 3;
pub const SC_HPF_INDEX: u32 = 4;
pub const SC_LPF_INDEX: u32 = 5;
pub const THRESHOLD_INDEX: u32 = 6;
pub const RATIO_INDEX: u32 = 7;
pub const KNEE_INDEX: u32 = 8;
pub const ATTACK_INDEX: u32 = 9;
pub const HOLD_INDEX: u32 = 10;
pub const RELEASE_INDEX: u32 = 11;
pub const LOOKAHEAD_INDEX: u32 = 12;
pub const RANGE_INDEX: u32 = 13;
pub const RELEASE_SYNC_INDEX: u32 = 14;
pub const TRIGGER_INDEX: u32 = 15;
pub const RATE_INDEX: u32 = 16;
pub const FEEL_INDEX: u32 = 17;
pub const DEPTH_INDEX: u32 = 18;
pub const CURVE_INDEX: u32 = 19;
pub const SMOOTH_INDEX: u32 = 20;
pub const OFFSET_INDEX: u32 = 21;
pub const SHAPER_THRESHOLD_INDEX: u32 = 22;
pub const BAND_INDEX: u32 = 23;
pub const XOVER_INDEX: u32 = 24;
pub const MIX_INDEX: u32 = 25;
pub const OUTPUT_INDEX: u32 = 26;
pub const MONITOR_INDEX: u32 = 27;
pub const POINT_COUNT_INDEX: u32 = 28;
/// `pointX0`; point `i`'s x is at `POINT_X_INDEX + i`.
pub const POINT_X_INDEX: u32 = 29;
/// `pointY0`; point `i`'s y is at `POINT_Y_INDEX + i`.
pub const POINT_Y_INDEX: u32 = POINT_X_INDEX + MAX_POINTS as u32;

pub const PARAM_COUNT: usize = POINT_Y_INDEX as usize + MAX_POINTS;

pub const UI_PARAM_IDS: [&str; PARAM_COUNT] = [
    "power",
    "mode",
    "listen",
    "source",
    "scHpfHz",
    "scLpfHz",
    "thresholdDb",
    "ratio",
    "kneeDb",
    "attackMs",
    "holdMs",
    "releaseMs",
    "lookaheadMs",
    "rangeDb",
    "releaseSync",
    "trigger",
    "rate",
    "feel",
    "depth",
    "curve",
    "smoothMs",
    "offset",
    "shaperThresholdDb",
    "band",
    "xoverHz",
    "mix",
    "outputDb",
    "monitor",
    "pointCount",
    "pointX0",
    "pointX1",
    "pointX2",
    "pointX3",
    "pointX4",
    "pointX5",
    "pointX6",
    "pointX7",
    "pointX8",
    "pointX9",
    "pointX10",
    "pointX11",
    "pointX12",
    "pointX13",
    "pointX14",
    "pointX15",
    "pointY0",
    "pointY1",
    "pointY2",
    "pointY3",
    "pointY4",
    "pointY5",
    "pointY6",
    "pointY7",
    "pointY8",
    "pointY9",
    "pointY10",
    "pointY11",
    "pointY12",
    "pointY13",
    "pointY14",
    "pointY15",
];

/// Continuous ranges for the knob params, by wire index below
/// [`POINT_COUNT_INDEX`]; `(0, 0)` for switches and choices.
pub const RANGES: [(f32, f32); POINT_COUNT_INDEX as usize] = [
    (0.0, 0.0),        // power
    (0.0, 0.0),        // mode
    (0.0, 0.0),        // listen
    (0.0, 0.0),        // source
    (20.0, 500.0),     // scHpfHz
    (100.0, 20_000.0), // scLpfHz
    (-60.0, 0.0),      // thresholdDb
    (1.0, 20.0),       // ratio
    (0.0, 24.0),       // kneeDb
    (0.1, 50.0),       // attackMs
    (0.0, 200.0),      // holdMs
    (10.0, 1_000.0),   // releaseMs
    (0.0, 10.0),       // lookaheadMs
    (0.0, 48.0),       // rangeDb
    (0.0, 0.0),        // releaseSync
    (0.0, 0.0),        // trigger
    (0.0, 0.0),        // rate
    (0.0, 0.0),        // feel
    (0.0, 100.0),      // depth
    (-100.0, 100.0),   // curve
    (0.0, 50.0),       // smoothMs
    (-50.0, 50.0),     // offset
    (-60.0, 0.0),      // shaperThresholdDb
    (0.0, 0.0),        // band
    (40.0, 2_000.0),   // xoverHz
    (0.0, 100.0),      // mix
    (-12.0, 12.0),     // outputDb
    (0.0, 0.0),        // monitor
];

#[inline]
fn clamp_wire(index: u32, value: f32) -> f32 {
    let (min, max) = RANGES[index as usize];
    clamp(value, min, max)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DuckstepState {
    pub version: u32,
    pub params: Params,
}

impl DuckstepState {
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
        sanitize_params(&mut state.params);
        Ok(state)
    }
}

impl Default for DuckstepState {
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

fn point_count(value: f32) -> u32 {
    (value.round() as i64).clamp(2, MAX_POINTS as i64) as u32
}

/// Clamps every continuous value into its range — a non-finite one returns
/// to its default — and puts the shape in order: 2 to 16 points, x rising
/// from 0 at the first to 1 at the last, every coordinate in 0…1.
pub fn sanitize_params(params: &mut Params) {
    let defaults = default_params();
    let fix = |index: u32, value: f32, default: f32| {
        if value.is_finite() {
            clamp_wire(index, value)
        } else {
            default
        }
    };
    params.sc_hpf_hz = fix(SC_HPF_INDEX, params.sc_hpf_hz, defaults.sc_hpf_hz);
    params.sc_lpf_hz = fix(SC_LPF_INDEX, params.sc_lpf_hz, defaults.sc_lpf_hz);
    params.threshold_db = fix(THRESHOLD_INDEX, params.threshold_db, defaults.threshold_db);
    params.ratio = fix(RATIO_INDEX, params.ratio, defaults.ratio);
    params.knee_db = fix(KNEE_INDEX, params.knee_db, defaults.knee_db);
    params.attack_ms = fix(ATTACK_INDEX, params.attack_ms, defaults.attack_ms);
    params.hold_ms = fix(HOLD_INDEX, params.hold_ms, defaults.hold_ms);
    params.release_ms = fix(RELEASE_INDEX, params.release_ms, defaults.release_ms);
    params.lookahead_ms = fix(LOOKAHEAD_INDEX, params.lookahead_ms, defaults.lookahead_ms);
    params.range_db = fix(RANGE_INDEX, params.range_db, defaults.range_db);
    params.depth = fix(DEPTH_INDEX, params.depth, defaults.depth);
    params.curve = fix(CURVE_INDEX, params.curve, defaults.curve);
    params.smooth_ms = fix(SMOOTH_INDEX, params.smooth_ms, defaults.smooth_ms);
    params.offset = fix(OFFSET_INDEX, params.offset, defaults.offset);
    params.shaper_threshold_db = fix(
        SHAPER_THRESHOLD_INDEX,
        params.shaper_threshold_db,
        defaults.shaper_threshold_db,
    );
    params.xover_hz = fix(XOVER_INDEX, params.xover_hz, defaults.xover_hz);
    params.mix = fix(MIX_INDEX, params.mix, defaults.mix);
    params.output_db = fix(OUTPUT_INDEX, params.output_db, defaults.output_db);

    let count = point_count(params.point_count as f32) as usize;
    params.point_count = count as u32;
    for i in 0..MAX_POINTS {
        let x = params.point_x[i];
        let y = params.point_y[i];
        params.point_x[i] = if x.is_finite() {
            clamp(x, 0.0, 1.0)
        } else {
            0.0
        };
        params.point_y[i] = if y.is_finite() {
            clamp(y, 0.0, 1.0)
        } else {
            1.0
        };
    }
    params.point_x[0] = 0.0;
    params.point_x[count - 1] = 1.0;
    for i in 1..count {
        params.point_x[i] = params.point_x[i].max(params.point_x[i - 1]);
    }
}

pub fn apply_wire_param(params: &mut Params, index: u32, value: f32) -> bool {
    if !value.is_finite() || index as usize >= PARAM_COUNT {
        return false;
    }
    if index >= POINT_Y_INDEX {
        params.point_y[(index - POINT_Y_INDEX) as usize] = clamp(value, 0.0, 1.0);
        return true;
    }
    if index >= POINT_X_INDEX {
        params.point_x[(index - POINT_X_INDEX) as usize] = clamp(value, 0.0, 1.0);
        return true;
    }
    match index {
        POWER_INDEX => params.power = value >= 0.5,
        MODE_INDEX => params.mode = Mode::from_wire(value),
        LISTEN_INDEX => params.listen = value >= 0.5,
        SOURCE_INDEX => params.source = Source::from_wire(value),
        SC_HPF_INDEX => params.sc_hpf_hz = clamp_wire(index, value),
        SC_LPF_INDEX => params.sc_lpf_hz = clamp_wire(index, value),
        THRESHOLD_INDEX => params.threshold_db = clamp_wire(index, value),
        RATIO_INDEX => params.ratio = clamp_wire(index, value),
        KNEE_INDEX => params.knee_db = clamp_wire(index, value),
        ATTACK_INDEX => params.attack_ms = clamp_wire(index, value),
        HOLD_INDEX => params.hold_ms = clamp_wire(index, value),
        RELEASE_INDEX => params.release_ms = clamp_wire(index, value),
        LOOKAHEAD_INDEX => params.lookahead_ms = clamp_wire(index, value),
        RANGE_INDEX => params.range_db = clamp_wire(index, value),
        RELEASE_SYNC_INDEX => params.release_sync = ReleaseSync::from_wire(value),
        TRIGGER_INDEX => params.trigger = Trigger::from_wire(value),
        RATE_INDEX => params.rate = Rate::from_wire(value),
        FEEL_INDEX => params.feel = Feel::from_wire(value),
        DEPTH_INDEX => params.depth = clamp_wire(index, value),
        CURVE_INDEX => params.curve = clamp_wire(index, value),
        SMOOTH_INDEX => params.smooth_ms = clamp_wire(index, value),
        OFFSET_INDEX => params.offset = clamp_wire(index, value),
        SHAPER_THRESHOLD_INDEX => params.shaper_threshold_db = clamp_wire(index, value),
        BAND_INDEX => params.band = Band::from_wire(value),
        XOVER_INDEX => params.xover_hz = clamp_wire(index, value),
        MIX_INDEX => params.mix = clamp_wire(index, value),
        OUTPUT_INDEX => params.output_db = clamp_wire(index, value),
        MONITOR_INDEX => params.monitor = Monitor::from_wire(value),
        POINT_COUNT_INDEX => params.point_count = point_count(value),
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
    let mut values = vec![
        ("power", f32::from(params.power)),
        ("mode", params.mode.to_wire()),
        ("listen", f32::from(params.listen)),
        ("source", params.source.to_wire()),
        ("scHpfHz", params.sc_hpf_hz),
        ("scLpfHz", params.sc_lpf_hz),
        ("thresholdDb", params.threshold_db),
        ("ratio", params.ratio),
        ("kneeDb", params.knee_db),
        ("attackMs", params.attack_ms),
        ("holdMs", params.hold_ms),
        ("releaseMs", params.release_ms),
        ("lookaheadMs", params.lookahead_ms),
        ("rangeDb", params.range_db),
        ("releaseSync", params.release_sync.to_wire()),
        ("trigger", params.trigger.to_wire()),
        ("rate", params.rate.to_wire()),
        ("feel", params.feel.to_wire()),
        ("depth", params.depth),
        ("curve", params.curve),
        ("smoothMs", params.smooth_ms),
        ("offset", params.offset),
        ("shaperThresholdDb", params.shaper_threshold_db),
        ("band", params.band.to_wire()),
        ("xoverHz", params.xover_hz),
        ("mix", params.mix),
        ("outputDb", params.output_db),
        ("monitor", params.monitor.to_wire()),
        ("pointCount", params.point_count as f32),
    ];
    let first_x = POINT_X_INDEX as usize;
    let first_y = POINT_Y_INDEX as usize;
    for i in 0..MAX_POINTS {
        values.push((UI_PARAM_IDS[first_x + i], params.point_x[i]));
    }
    for i in 0..MAX_POINTS {
        values.push((UI_PARAM_IDS[first_y + i], params.point_y[i]));
    }
    values
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip_to_wire_indices() {
        assert_eq!(UI_PARAM_IDS.len(), PARAM_COUNT);
        assert_eq!(PARAM_COUNT, 61);
        for (index, id) in UI_PARAM_IDS.iter().enumerate() {
            assert_eq!(ui_param_index(id), Some(index as u32));
            assert_eq!(ui_param_id(index as u32), Some(*id));
        }
        assert_eq!(ui_param_id(PARAM_COUNT as u32), None);
        assert_eq!(ui_param_index("pointX3"), Some(POINT_X_INDEX + 3));
        assert_eq!(ui_param_index("pointY15"), Some(POINT_Y_INDEX + 15));
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
    fn every_value_survives_the_wire() {
        let mut source = default_params();
        source.power = false;
        source.mode = Mode::Shaper;
        source.listen = true;
        source.source = Source::External;
        source.sc_hpf_hz = 120.0;
        source.sc_lpf_hz = 4_000.0;
        source.threshold_db = -31.0;
        source.ratio = 3.5;
        source.knee_db = 2.0;
        source.attack_ms = 0.4;
        source.hold_ms = 75.0;
        source.release_ms = 600.0;
        source.lookahead_ms = 6.0;
        source.range_db = 30.0;
        source.release_sync = ReleaseSync::Quarter;
        source.trigger = Trigger::Audio;
        source.rate = Rate::Sixteenth;
        source.feel = Feel::Triplet;
        source.depth = 70.0;
        source.curve = -40.0;
        source.smooth_ms = 12.0;
        source.offset = -20.0;
        source.shaper_threshold_db = -33.0;
        source.band = Band::High;
        source.xover_hz = 800.0;
        source.mix = 55.0;
        source.output_db = 3.0;
        source.monitor = Monitor::Delta;
        source.set_shape(&crate::SHAPE_PRESETS[4]);
        let mut copy = default_params();
        for (index, (_, value)) in ui_values(&source).into_iter().enumerate() {
            assert!(apply_wire_param(&mut copy, index as u32, value), "{index}");
        }
        assert_eq!(ui_values(&copy), ui_values(&source));
    }

    #[test]
    fn state_round_trips_with_its_points() {
        let mut params = default_params();
        params.set_shape(&crate::SHAPE_PRESETS[3]);
        assert!(apply_ui_param(&mut params, "mode", Mode::Shaper.to_wire()));
        assert!(apply_ui_param(&mut params, "pointY1", 0.4));
        let json = DuckstepState::new(params.clone()).to_json().unwrap();
        let decoded = DuckstepState::from_json(&json).unwrap();
        assert_eq!(ui_values(&decoded.params), ui_values(&params));
        assert_eq!(decoded.params.point_count, 5);
        assert_eq!(decoded.params.point_y[1], 0.4);
        assert_eq!(decoded.version, STATE_VERSION);
    }

    #[test]
    fn state_missing_fields_loads_with_defaults() {
        let decoded = DuckstepState::from_json(
            r#"{"version":1,"params":{"thresholdDb":-80,"mode":"shaper","band":"low"}}"#,
        )
        .unwrap();
        let defaults = default_params();
        assert_eq!(decoded.params.threshold_db, -60.0, "clamped on load");
        assert_eq!(decoded.params.mode, Mode::Shaper);
        assert_eq!(decoded.params.band, Band::Low);
        assert_eq!(decoded.params.source, Source::Internal);
        assert_eq!(decoded.params.point_count, defaults.point_count);
        assert_eq!(decoded.params.point_y, defaults.point_y);
        assert_eq!(decoded.params.ratio, defaults.ratio);
        assert!(decoded.params.power);
    }

    #[test]
    fn sanitize_orders_the_shape() {
        let mut params = default_params();
        params.point_count = 40;
        params.point_x = [0.5; MAX_POINTS];
        params.point_x[3] = 0.2;
        params.point_y[2] = 7.0;
        params.point_y[4] = f32::NAN;
        sanitize_params(&mut params);
        assert_eq!(params.point_count as usize, MAX_POINTS);
        assert_eq!(params.point_x[0], 0.0);
        assert_eq!(params.point_x[MAX_POINTS - 1], 1.0);
        assert!(params.point_x.windows(2).all(|w| w[0] <= w[1]));
        assert_eq!(params.point_y[2], 1.0);
        assert_eq!(params.point_y[4], 1.0);
        params.point_count = 0;
        sanitize_params(&mut params);
        assert_eq!(params.point_count, 2);
        assert_eq!((params.point_x[0], params.point_x[1]), (0.0, 1.0));
    }

    #[test]
    fn invalid_and_non_finite_updates_are_rejected() {
        let mut params = default_params();
        assert!(!apply_wire_param(&mut params, u32::MAX, 1.0));
        assert!(!apply_wire_param(&mut params, PARAM_COUNT as u32, 1.0));
        assert!(!apply_wire_param(&mut params, THRESHOLD_INDEX, f32::NAN));
        assert!(!apply_wire_param(&mut params, POINT_X_INDEX, f32::INFINITY));
        assert!(!apply_ui_param(&mut params, "notAParam", 1.0));
        assert!(apply_ui_param(&mut params, "ratio", 0.2));
        assert_eq!(params.ratio, 1.0);
        assert!(apply_ui_param(&mut params, "pointCount", 99.0));
        assert_eq!(params.point_count as usize, MAX_POINTS);
        assert!(apply_ui_param(&mut params, "pointY2", -3.0));
        assert_eq!(params.point_y[2], 0.0);
    }
}

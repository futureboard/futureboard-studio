//! What WayGate's native editor edits: its params, their wire table, presets,
//! knobs, and the static response the editor draws.
//!
//! No GPUI here. Every value goes back to the plug-in as wire edits the DSP
//! crate's own `ipc` table computes and clamps, so an edit lands in the
//! editor's copy exactly as it lands in Studio's state mirror and in the host.

use std::sync::{Arc, OnceLock};

use crate::components::fx_model::{format_value, spec, KnobSpec, Taper, Unit};
use crate::components::plugin_kit::{bank, KitPreset};

pub use waygate::Params as GateParams;

/// Every param as `(wire index, value)`, in wire order.
pub fn wire_values(params: &GateParams) -> Vec<(u32, f32)> {
    waygate::ipc::ui_values(params)
        .into_iter()
        .filter_map(|(id, value)| waygate::ui_param_index(id).map(|index| (index, value)))
        .collect()
}

/// Applies one edit by id the way the DSP will.
pub fn with(params: &GateParams, id: &str, value: f32) -> GateParams {
    let mut next = params.clone();
    waygate::ipc::apply_ui_param(&mut next, id, value);
    next
}

pub fn value(params: &GateParams, id: &str) -> f32 {
    waygate::ipc::ui_values(params)
        .into_iter()
        .find(|(candidate, _)| *candidate == id)
        .map_or(0.0, |(_, value)| value)
}

/// The factory bank, `Default` first.
pub fn presets() -> Arc<Vec<KitPreset<GateParams>>> {
    static BANK: OnceLock<Arc<Vec<KitPreset<GateParams>>>> = OnceLock::new();
    BANK.get_or_init(|| bank(waygate::factory_presets(), |p| (p.name, p.params)))
        .clone()
}

/// `preset` as it would play here: power stays as it is, and Key Listen —
/// a listening aid, not a sound — stays as the user left it.
pub fn preset_applied(current: &GateParams, preset: &GateParams) -> GateParams {
    GateParams {
        power: current.power,
        key_listen: current.key_listen,
        ..preset.clone()
    }
}

/// The ids the knobs cover, in the order the editor lays them out.
pub const KNOB_IDS: [&str; 8] = [
    "thresholdDb",
    "rangeDb",
    "hysteresisDb",
    "attackMs",
    "holdMs",
    "releaseMs",
    "keyHpfHz",
    "keyLpfHz",
];

/// The lookahead knob's id: kept apart because it costs latency.
pub const LOOKAHEAD_ID: &str = "lookaheadMs";

/// The knob table. Ranges come from the DSP crate's descriptor, so a range
/// change there reaches the editor without a second edit here.
pub fn knob(id: &str) -> Option<KnobSpec> {
    let descriptor = waygate::descriptor();
    let range = descriptor.params.iter().find(|p| p.id == id)?;
    let (label, taper, unit) = match id {
        "thresholdDb" => ("Threshold", Taper::Linear, Unit::Db),
        "rangeDb" => ("Range", Taper::Linear, Unit::Db),
        "hysteresisDb" => ("Hysteresis", Taper::Linear, Unit::Db),
        "attackMs" => ("Attack", Taper::Log, Unit::Ms),
        "holdMs" => ("Hold", Taper::Square, Unit::Ms),
        "releaseMs" => ("Release", Taper::Log, Unit::Ms),
        "keyHpfHz" => ("Key HPF", Taper::Log, Unit::CutHz(waygate::KEY_HPF_OFF_HZ)),
        "keyLpfHz" => ("Key LPF", Taper::Log, Unit::Hz),
        "lookaheadMs" => ("Lookahead", Taper::Linear, Unit::Ms),
        _ => return None,
    };
    Some(spec(range.id, label, range.min, range.max, taper, unit))
}

/// A knob's readout, in the gate's own words where the generic one would
/// mislead: a mute is −∞, not −80 dB, a filter at its end stop is off, and
/// hysteresis is a distance, not a gain.
pub fn readout(id: &str, value: f32) -> String {
    match id {
        "rangeDb" if waygate::is_full_mute(value) => "−∞ dB".to_string(),
        "keyLpfHz" if value >= waygate::KEY_LPF_OFF_HZ - 0.5 => "Off".to_string(),
        "lookaheadMs" if value <= 0.0 => "Off".to_string(),
        "holdMs" if value <= 0.0 => "0 ms".to_string(),
        "hysteresisDb" => format!("{value:.1} dB"),
        "thresholdDb" if value.abs() < 0.05 => "0.0 dB".to_string(),
        "thresholdDb" => format!("{value:.1} dB"),
        "rangeDb" if value.abs() < 0.05 => "0.0 dB".to_string(),
        "rangeDb" => format!("{value:.1} dB"),
        _ => knob(id).map_or_else(String::new, |k| format_value(k.unit, value)),
    }
}

pub fn mode_labels() -> &'static [&'static str] {
    &["Gate", "Duck"]
}

pub fn mode_hint(params: &GateParams) -> &'static str {
    match params.mode {
        waygate::Mode::Gate => "Opens above the threshold, takes the range off below",
        waygate::Mode::Duck => "Takes the range off while the key is above the threshold",
    }
}

/// The steady-state output, in dB, of a key held at `key_db` with the
/// detector open (`true`) or shut — the two branches of the hysteresis
/// loop. The signal is taken as the key itself.
pub fn response_db(params: &GateParams, key_db: f32, open: bool) -> f32 {
    if !params.power {
        return key_db;
    }
    let closed = match params.mode {
        waygate::Mode::Gate => !open,
        waygate::Mode::Duck => open,
    };
    let gain = waygate::gain_db_at(params.range_db, if closed { 1.0 } else { 0.0 });
    key_db + gain
}

/// Whether the detector is open for a key held at `key_db`, coming from
/// below (`rising`) or from above.
pub fn opens_at(params: &GateParams, key_db: f32, rising: bool) -> bool {
    if rising {
        key_db >= params.threshold_db
    } else {
        key_db >= params.threshold_db - params.hysteresis_db
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn same(a: &GateParams, b: &GateParams) -> bool {
        let theirs = wire_values(b);
        wire_values(a).iter().all(|(index, value)| {
            theirs
                .iter()
                .find(|(i, _)| i == index)
                .is_some_and(|(_, other)| (value - other).abs() <= 0.002 * value.abs().max(1.0))
        })
    }

    /// Replaying a diff through the DSP's own wire table lands on the target —
    /// the same path Studio's mirror and the host take.
    #[test]
    fn every_preset_lands_exactly_from_every_other() {
        let bank = presets();
        assert_eq!(bank[0].name, "Default");
        assert!(bank.len() >= 8);
        for from in bank.iter() {
            for to in bank.iter() {
                let before = wire_values(&from.params);
                let mut landed = from.params.clone();
                for (index, value) in wire_values(&to.params) {
                    if before.iter().any(|(i, v)| *i == index && *v == value) {
                        continue;
                    }
                    landed = with(&landed, waygate::ui_param_id(index).unwrap(), value);
                }
                assert!(same(&landed, &to.params), "{} → {}", from.name, to.name);
            }
        }
    }

    #[test]
    fn a_preset_keeps_power_and_key_listen() {
        let current = with(
            &with(&waygate::default_params(), "power", 0.0),
            "keyListen",
            1.0,
        );
        for preset in presets().iter() {
            let applied = preset_applied(&current, &preset.params);
            assert!(!applied.power);
            assert!(applied.key_listen);
        }
    }

    #[test]
    fn every_knob_round_trips_through_its_taper() {
        let defaults = waygate::default_params();
        for id in KNOB_IDS.iter().chain([&LOOKAHEAD_ID]) {
            let knob = knob(id).unwrap_or_else(|| panic!("no knob for {id}"));
            let default = value(&defaults, id);
            let back = knob.from_knob(knob.to_knob(default));
            assert!(
                (back - default).abs() <= 0.011 * default.abs().max(1.0),
                "{id}: {default} came back {back}"
            );
            // The end stops are reachable.
            assert_eq!(knob.from_knob(0.0), knob.min, "{id}");
            assert_eq!(knob.from_knob(1.0), knob.max, "{id}");
        }
    }

    #[test]
    fn readouts_say_what_the_gate_does() {
        assert_eq!(readout("rangeDb", -80.0), "−∞ dB");
        assert_eq!(readout("rangeDb", -24.0), "-24.0 dB");
        assert_eq!(readout("keyLpfHz", 20_000.0), "Off");
        assert_eq!(readout("keyLpfHz", 150.0), "150 Hz");
        assert_eq!(readout("keyHpfHz", 20.0), "Off");
        assert_eq!(readout("lookaheadMs", 0.0), "Off");
        assert_eq!(readout("hysteresisDb", 4.0), "4.0 dB");
        assert_eq!(readout("attackMs", 0.05), "0.05 ms");
    }

    #[test]
    fn the_response_is_a_hysteresis_loop() {
        let params = waygate::default_params();
        // Inside the band, the branch decides.
        let inside = params.threshold_db - 2.0;
        assert!(!opens_at(&params, inside, true));
        assert!(opens_at(&params, inside, false));
        assert_eq!(response_db(&params, -20.0, true), -20.0);
        assert_eq!(
            response_db(&params, -50.0, false),
            -50.0 + waygate::RANGE_FLOOR_DB
        );
        let duck = with(&params, "mode", 1.0);
        assert_eq!(response_db(&duck, -50.0, false), -50.0);
        let off = with(&params, "power", 0.0);
        assert_eq!(response_db(&off, -50.0, false), -50.0);
    }
}

//! What the native MixStation editor edits: the channel strip's params, its
//! user-ordered rack of six modules, presets and knobs.
//!
//! No GPUI here. The rack is plain wire params — `slot1Module` to
//! `slot6Module`, each holding a module code — and the DSP's own wire table
//! keeps a module in one slot at most: writing the slots in order, first to
//! last, always lands on the order written, in the editor, the state mirror
//! and the host alike.

use std::sync::{Arc, OnceLock};

use mixstation::Params;

use crate::components::fx_model::{spec, KnobSpec, Taper, Unit};
use crate::components::plugin_kit::{bank, KitPreset};

pub const SLOTS: usize = mixstation::RACK_SLOTS;
pub const SLOT_IDS: [&str; SLOTS] = [
    "slot1Module",
    "slot2Module",
    "slot3Module",
    "slot4Module",
    "slot5Module",
    "slot6Module",
];

/// One kind of module the rack holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Module {
    /// Its code on the wire, 1 to 6.
    pub code: u8,
    pub name: &'static str,
    pub hint: &'static str,
    /// The param switching it in and out of the path.
    pub enabled: &'static str,
    pub knobs: &'static [&'static str],
    /// Its own output trim, which travels with it through the rack.
    pub trim: &'static str,
}

pub const MODULES: [Module; SLOTS] = [
    Module {
        code: 1,
        name: "Filters",
        hint: "24 dB/oct high and low cut",
        enabled: "filtersEnabled",
        knobs: &["hpfHz", "lpfHz"],
        trim: "filtersTrimDb",
    },
    Module {
        code: 2,
        name: "EQ",
        hint: "Four-band with proportional-Q mids",
        enabled: "eqEnabled",
        knobs: &[
            "lowGainDb",
            "lowMidFreqHz",
            "lowMidGainDb",
            "highMidFreqHz",
            "highMidGainDb",
            "highGainDb",
        ],
        trim: "eqTrimDb",
    },
    Module {
        code: 3,
        name: "Compressor",
        hint: "Stereo-linked, program-dependent release",
        enabled: "compEnabled",
        knobs: &[
            "compThresholdDb",
            "compRatio",
            "compAttackMs",
            "compReleaseMs",
            "compMakeupDb",
        ],
        trim: "compTrimDb",
    },
    Module {
        code: 4,
        name: "Drive",
        hint: "Anti-aliased asymmetric saturation",
        enabled: "satEnabled",
        knobs: &["satDrivePct", "satCharacterPct"],
        trim: "satTrimDb",
    },
    Module {
        code: 5,
        name: "Width",
        hint: "Mid/side stereo image",
        enabled: "widthEnabled",
        knobs: &["widthPct"],
        trim: "widthTrimDb",
    },
    Module {
        code: 6,
        name: "Limiter",
        hint: "Zero-latency brickwall ceiling",
        enabled: "limiterEnabled",
        knobs: &["limiterCeilingDb", "limiterReleaseMs"],
        trim: "limiterTrimDb",
    },
];

pub fn module(code: u8) -> Option<&'static Module> {
    MODULES.iter().find(|m| m.code == code)
}

/// Every param as `(wire index, value)`, in wire order.
pub fn wire_values(params: &Params) -> Vec<(u32, f32)> {
    mixstation::ipc::ui_values(params)
        .into_iter()
        .enumerate()
        .map(|(index, value)| (index as u32, value))
        .collect()
}

/// Applies one edit by id the way the DSP will, a slot's de-duplication
/// included.
pub fn with(params: &Params, id: &str, value: f32) -> Params {
    let mut next = params.clone();
    mixstation::ipc::apply_ui_param(&mut next, id, value);
    next
}

pub fn value(params: &Params, id: &str) -> f32 {
    mixstation::ui_param_index(id)
        .and_then(|index| {
            mixstation::ipc::ui_values(params)
                .get(index as usize)
                .copied()
        })
        .unwrap_or(0.0)
}

pub fn flag(params: &Params, id: &str) -> bool {
    value(params, id) >= 0.5
}

/// The modules in the rack, first processed first.
pub fn order(params: &Params) -> Vec<u8> {
    SLOT_IDS
        .iter()
        .map(|id| value(params, id).round() as u8)
        .filter(|code| module(*code).is_some())
        .collect()
}

/// The params with the rack holding `order`, the rest of the slots empty.
pub fn with_order(params: &Params, order: &[u8]) -> Params {
    SLOT_IDS
        .iter()
        .enumerate()
        .fold(params.clone(), |next, (slot, id)| {
            with(&next, id, order.get(slot).copied().unwrap_or(0) as f32)
        })
}

/// Whether a module processes: in the rack and switched on.
pub fn active(params: &Params, code: u8) -> bool {
    order(params).contains(&code) && module(code).is_some_and(|m| flag(params, m.enabled))
}

// ── Presets ─────────────────────────────────────────────────────────────────

pub fn presets() -> Arc<Vec<KitPreset<Params>>> {
    static BANK: OnceLock<Arc<Vec<KitPreset<Params>>>> = OnceLock::new();
    BANK.get_or_init(|| bank(mixstation::factory_presets(), |p| (p.name, p.params)))
        .clone()
}

/// `preset` as it would play here: power stays as it is.
pub fn preset_applied(current: &Params, preset: &Params) -> Params {
    let mut next = preset.clone();
    next.power = current.power;
    next
}

// ── Knobs ───────────────────────────────────────────────────────────────────

pub fn knob(id: &str) -> Option<KnobSpec> {
    let descriptor = mixstation::descriptor();
    let range = descriptor.params.iter().find(|p| p.id == id)?;
    let (label, taper, unit) = match id {
        "inputTrimDb" => ("Input", Taper::Linear, Unit::Db),
        "outputTrimDb" => ("Output", Taper::Linear, Unit::Db),
        "hpfHz" => ("Low Cut", Taper::Log, Unit::Hz),
        "lpfHz" => ("High Cut", Taper::Log, Unit::Hz),
        "lowGainDb" => ("Low", Taper::Linear, Unit::Db),
        "lowMidFreqHz" => ("LM Freq", Taper::Log, Unit::Hz),
        "lowMidGainDb" => ("LM Gain", Taper::Linear, Unit::Db),
        "highMidFreqHz" => ("HM Freq", Taper::Log, Unit::Hz),
        "highMidGainDb" => ("HM Gain", Taper::Linear, Unit::Db),
        "highGainDb" => ("High", Taper::Linear, Unit::Db),
        "compThresholdDb" => ("Threshold", Taper::Linear, Unit::Db),
        "compRatio" => ("Ratio", Taper::Log, Unit::Ratio),
        "compAttackMs" => ("Attack", Taper::Log, Unit::Ms),
        "compReleaseMs" => ("Release", Taper::Log, Unit::Ms),
        "compMakeupDb" => ("Makeup", Taper::Linear, Unit::Db),
        "satDrivePct" => ("Drive", Taper::Linear, Unit::Percent),
        "satCharacterPct" => ("Character", Taper::Linear, Unit::Percent),
        "widthPct" => ("Width", Taper::Linear, Unit::Percent),
        "limiterCeilingDb" => ("Ceiling", Taper::Linear, Unit::Db),
        "limiterReleaseMs" => ("Release", Taper::Log, Unit::Ms),
        _ if id.ends_with("TrimDb") => ("Trim", Taper::Linear, Unit::Db),
        _ => return None,
    };
    let mut knob = spec(range.id, label, range.min, range.max, taper, unit);
    match id {
        "lowGainDb" | "lowMidGainDb" | "highMidGainDb" | "highGainDb" | "compMakeupDb" => {
            knob.bipolar = true
        }
        "widthPct" => {
            knob.bipolar = true;
            knob.centre = 100.0;
        }
        _ if id.ends_with("TrimDb") => knob.bipolar = true,
        _ => {}
    }
    Some(knob)
}

pub fn default_value(id: &str) -> f32 {
    value(&mixstation::default_params(), id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn same(a: &Params, b: &Params) -> bool {
        wire_values(a)
            .iter()
            .zip(wire_values(b))
            .all(|((_, x), (_, y))| (x - y).abs() <= 0.002 * x.abs().max(1.0))
    }

    /// The editor sends what changed in wire order; replaying that through
    /// the DSP's wire table lands on the target, rack and all.
    #[test]
    fn every_reorder_lands_exactly() {
        let base = presets()[1].params.clone();
        let orders: [&[u8]; 5] = [&[6, 5, 4, 3, 2, 1], &[2, 1], &[3], &[], &[4, 1, 6, 2, 5, 3]];
        for from in orders {
            for to in orders {
                let start = with_order(&base, from);
                assert_eq!(order(&start), from);
                let target = with_order(&start, to);
                assert_eq!(order(&target), to, "{from:?} → {to:?}");
                let before = wire_values(&start);
                let mut landed = start.clone();
                for (index, value) in wire_values(&target) {
                    if before[index as usize].1 != value {
                        landed = with(&landed, mixstation::ui_param_id(index).unwrap(), value);
                    }
                }
                assert!(same(&landed, &target), "{from:?} → {to:?}");
            }
        }
    }

    #[test]
    fn every_preset_lands_exactly_from_every_other() {
        let bank = presets();
        for from in bank.iter() {
            for to in bank.iter() {
                let before = wire_values(&from.params);
                let mut landed = from.params.clone();
                for (index, value) in wire_values(&to.params) {
                    if before[index as usize].1 != value {
                        landed = with(&landed, mixstation::ui_param_id(index).unwrap(), value);
                    }
                }
                assert!(same(&landed, &to.params), "{} → {}", from.name, to.name);
            }
        }
    }

    #[test]
    fn every_knob_round_trips_through_its_taper() {
        let ids = MODULES
            .iter()
            .flat_map(|m| m.knobs.iter().copied().chain([m.trim]))
            .chain(["inputTrimDb", "outputTrimDb"]);
        for id in ids {
            let knob = knob(id).unwrap_or_else(|| panic!("no knob for {id}"));
            let default = default_value(id);
            let back = knob.from_knob(knob.to_knob(default));
            assert!(
                (back - default).abs() <= 0.011 * default.abs().max(1.0),
                "{id}: {default} came back {back}"
            );
        }
    }

    #[test]
    fn a_module_only_runs_in_the_rack_and_switched_on() {
        let params = with(&mixstation::default_params(), "eqEnabled", 1.0);
        assert!(!active(&params, 2));
        let params = with_order(&params, &[2]);
        assert!(active(&params, 2));
        assert!(!active(&with(&params, "eqEnabled", 0.0), 2));
    }
}

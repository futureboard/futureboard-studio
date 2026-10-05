//! What the native dynamics editors edit: FA-2A, FA-76, Z-Comp, BurnLimit,
//! 67Clipper and Transient — their params behind one type, their wire table,
//! presets and knobs.
//!
//! No GPUI here. Every value goes back to the plug-in as wire edits the DSP
//! crates' own `ipc` tables compute and clamp, so an edit lands in the
//! editor's copy exactly as it lands in Studio's state mirror and in the host.

use std::sync::{Arc, OnceLock};

use crate::components::fx_model::{spec, KnobSpec, Taper, Unit};
use crate::components::plugin_kit::{bank, KitPreset};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DynKind {
    Fa2a,
    Fa76,
    Zcomp,
    BurnLimit,
    Clipper,
    Transient,
}

impl DynKind {
    pub const ALL: [DynKind; 6] = [
        DynKind::Fa2a,
        DynKind::Fa76,
        DynKind::Zcomp,
        DynKind::BurnLimit,
        DynKind::Clipper,
        DynKind::Transient,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Self::Fa2a => "FA-2A",
            Self::Fa76 => "FA-76",
            Self::Zcomp => "Z-Comp",
            Self::BurnLimit => "BurnLimit",
            Self::Clipper => "67Clipper",
            Self::Transient => "Transient",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Self::Fa2a => "fa2a",
            Self::Fa76 => "fa76",
            Self::Zcomp => "zcomp",
            Self::BurnLimit => "burnlimit",
            Self::Clipper => "clipper67",
            Self::Transient => "transient",
        }
    }

    /// The meter a reduction column reads, and the sign its readout takes.
    pub fn reduction_label(self) -> (&'static str, &'static str) {
        match self {
            Self::Clipper => ("Clip", "−"),
            Self::Transient => ("Shape", ""),
            _ => ("GR", "−"),
        }
    }

    /// Whether the editor has a VU meter.
    pub fn has_vu(self) -> bool {
        matches!(self, Self::Fa2a | Self::Fa76 | Self::Zcomp)
    }
}

#[derive(Clone, Debug)]
pub enum DynParams {
    Fa2a(fa2a::Params),
    Fa76(fa76::Params),
    Zcomp(zcomp::Params),
    BurnLimit(burnlimit::Params),
    Clipper(clipper67::Params),
    Transient(transient::Params),
}

/// `ui_values` mapped onto wire indices.
fn indexed(
    values: Vec<(&'static str, f32)>,
    index: impl Fn(&str) -> Option<u32>,
) -> Vec<(u32, f32)> {
    values
        .into_iter()
        .filter_map(|(id, value)| index(id).map(|i| (i, value)))
        .collect()
}

impl DynParams {
    pub fn defaults(kind: DynKind) -> Self {
        match kind {
            DynKind::Fa2a => Self::Fa2a(fa2a::default_params()),
            DynKind::Fa76 => Self::Fa76(fa76::default_params()),
            DynKind::Zcomp => Self::Zcomp(zcomp::default_params()),
            DynKind::BurnLimit => Self::BurnLimit(burnlimit::default_params()),
            DynKind::Clipper => Self::Clipper(clipper67::default_params()),
            DynKind::Transient => Self::Transient(transient::default_params()),
        }
    }

    pub fn kind(&self) -> DynKind {
        match self {
            Self::Fa2a(_) => DynKind::Fa2a,
            Self::Fa76(_) => DynKind::Fa76,
            Self::Zcomp(_) => DynKind::Zcomp,
            Self::BurnLimit(_) => DynKind::BurnLimit,
            Self::Clipper(_) => DynKind::Clipper,
            Self::Transient(_) => DynKind::Transient,
        }
    }

    fn ui_values(&self) -> Vec<(&'static str, f32)> {
        match self {
            Self::Fa2a(p) => fa2a::ipc::ui_values(p),
            Self::Fa76(p) => fa76::ipc::ui_values(p),
            Self::Zcomp(p) => zcomp::ipc::ui_values(p),
            Self::BurnLimit(p) => burnlimit::ipc::ui_values(p),
            Self::Clipper(p) => clipper67::ipc::ui_values(p),
            Self::Transient(p) => transient::ipc::ui_values(p),
        }
    }

    /// Every param as `(wire index, value)`, in wire order.
    pub fn wire_values(&self) -> Vec<(u32, f32)> {
        let values = self.ui_values();
        match self {
            Self::Fa2a(_) => indexed(values, fa2a::ui_param_index),
            Self::Fa76(_) => indexed(values, fa76::ui_param_index),
            Self::Zcomp(_) => indexed(values, zcomp::ui_param_index),
            Self::BurnLimit(_) => indexed(values, burnlimit::ui_param_index),
            Self::Clipper(_) => indexed(values, clipper67::ui_param_index),
            Self::Transient(_) => indexed(values, transient::ui_param_index),
        }
    }

    /// Applies one edit by id the way the DSP will.
    pub fn with(&self, id: &str, value: f32) -> Self {
        let mut next = self.clone();
        match &mut next {
            Self::Fa2a(p) => {
                fa2a::ipc::apply_ui_param(p, id, value);
            }
            Self::Fa76(p) => {
                fa76::ipc::apply_ui_param(p, id, value);
            }
            Self::Zcomp(p) => {
                zcomp::ipc::apply_ui_param(p, id, value);
            }
            Self::BurnLimit(p) => {
                burnlimit::ipc::apply_ui_param(p, id, value);
            }
            Self::Clipper(p) => {
                clipper67::ipc::apply_ui_param(p, id, value);
            }
            Self::Transient(p) => {
                transient::ipc::apply_ui_param(p, id, value);
            }
        }
        next
    }

    pub fn value(&self, id: &str) -> f32 {
        self.ui_values()
            .into_iter()
            .find(|(candidate, _)| *candidate == id)
            .map_or(0.0, |(_, value)| value)
    }

    pub fn power(&self) -> bool {
        self.value("power") >= 0.5
    }

    /// The steady-state output, in dBFS, of a level held at `input_db` —
    /// the DSP crates' own static models. `None` for a plug-in without one
    /// drawn.
    pub fn transfer_db(&self, input_db: f32) -> Option<f32> {
        match self {
            Self::Zcomp(p) => Some(zcomp::transfer_db(p, input_db)),
            Self::BurnLimit(p) => Some(burnlimit::transfer_db(p, input_db)),
            Self::Clipper(p) => Some(clipper67::transfer_db(p, input_db)),
            _ => None,
        }
    }

    /// The input the meters' "In" reads, relative to what enters the
    /// plug-in: BurnLimit meters its input after the drive.
    pub fn metered_input_offset_db(&self) -> f32 {
        match self {
            Self::BurnLimit(p) => p.gain_db,
            _ => 0.0,
        }
    }
}

// ── Presets ─────────────────────────────────────────────────────────────────

/// The kind's factory bank, `Default` first.
pub fn presets(kind: DynKind) -> Arc<Vec<KitPreset<DynParams>>> {
    static BANKS: OnceLock<Vec<Arc<Vec<KitPreset<DynParams>>>>> = OnceLock::new();
    let banks = BANKS.get_or_init(|| {
        vec![
            bank(fa2a::factory_presets(), |p| {
                (p.name, DynParams::Fa2a(p.params))
            }),
            bank(fa76::factory_presets(), |p| {
                (p.name, DynParams::Fa76(p.params))
            }),
            bank(zcomp::factory_presets(), |p| {
                (p.name, DynParams::Zcomp(p.params))
            }),
            bank(burnlimit::factory_presets(), |p| {
                (p.name, DynParams::BurnLimit(p.params))
            }),
            bank(clipper67::factory_presets(), |p| {
                (p.name, DynParams::Clipper(p.params))
            }),
            bank(transient::factory_presets(), |p| {
                (p.name, DynParams::Transient(p.params))
            }),
        ]
    });
    let index = DynKind::ALL.iter().position(|k| *k == kind).unwrap_or(0);
    banks[index].clone()
}

/// `preset` as it would play here: power stays as it is, and Z-Comp's
/// SC Listen — a listening aid, not a sound — stays off.
pub fn preset_applied(current: &DynParams, preset: &DynParams) -> DynParams {
    if current.kind() != preset.kind() {
        return current.clone();
    }
    let next = preset.with("power", if current.power() { 1.0 } else { 0.0 });
    match &next {
        DynParams::Zcomp(_) => next.with("scListen", current.value("scListen")),
        _ => next,
    }
}

// ── Knobs ───────────────────────────────────────────────────────────────────

/// The knob table. Ranges come from the DSP crates' descriptors, so a range
/// change there reaches the editor without a second edit here.
pub fn knob(kind: DynKind, id: &str) -> Option<KnobSpec> {
    let descriptor = match kind {
        DynKind::Fa2a => fa2a::descriptor(),
        DynKind::Fa76 => fa76::descriptor(),
        DynKind::Zcomp => zcomp::descriptor(),
        DynKind::BurnLimit => burnlimit::descriptor(),
        DynKind::Clipper => clipper67::descriptor(),
        DynKind::Transient => transient::descriptor(),
    };
    let range = descriptor.params.iter().find(|p| p.id == id)?;
    use DynKind::*;
    let (label, taper, unit) = match (kind, id) {
        (Fa2a, "peakReduction") => ("Peak Reduction", Taper::Linear, Unit::Plain),
        (Fa2a, "gainDb") => ("Gain", Taper::Linear, Unit::Db),
        (Fa2a, "emphasis") => ("Emphasis", Taper::Linear, Unit::Percent),
        (Fa2a, "color") => ("Color", Taper::Linear, Unit::Percent),
        (Fa2a, "sidechainLowCutHz") => ("Sidechain", Taper::Log, Unit::Hz),
        (Fa2a, "outputTrimDb") => ("Trim", Taper::Linear, Unit::Db),
        (Fa76, "inputDb") => ("Input", Taper::Linear, Unit::Db),
        (Fa76, "outputDb") => ("Output", Taper::Linear, Unit::Db),
        (Fa76, "attackUs") => ("Attack", Taper::Log, Unit::Us),
        (Fa76, "releaseMs") => ("Release", Taper::Log, Unit::Ms),
        (Fa76, "sidechainHpfHz") => ("SC HPF", Taper::Square, Unit::CutHz(9.5)),
        (Zcomp, "thresholdDb") => ("Thresh", Taper::Linear, Unit::Db),
        (Zcomp, "ratio") => ("Ratio", Taper::Log, Unit::Ratio),
        (Zcomp, "attackMs") => ("Attack", Taper::Log, Unit::Ms),
        (Zcomp, "releaseMs") => ("Release", Taper::Log, Unit::Ms),
        (Zcomp, "kneeDb") => ("Knee", Taper::Linear, Unit::Db),
        (Zcomp, "makeupDb") => ("Makeup", Taper::Linear, Unit::Db),
        (Zcomp, "sidechainHpfHz") => ("SC HPF", Taper::Log, Unit::Hz),
        (Zcomp, "stereoLink") => ("Link", Taper::Linear, Unit::Percent),
        (Zcomp, "color") => ("Color", Taper::Linear, Unit::Percent),
        (BurnLimit, "gainDb") => ("Gain", Taper::Linear, Unit::Db),
        (BurnLimit, "ceilingDb") => ("Ceiling", Taper::Linear, Unit::Db),
        (BurnLimit, "releaseMs") => ("Release", Taper::Log, Unit::Ms),
        (BurnLimit, "lookaheadMs") => ("Lookahead", Taper::Linear, Unit::Ms),
        (Clipper, "thresholdDb") => ("Threshold", Taper::Linear, Unit::Db),
        (Clipper, "shape") => ("Shape", Taper::Linear, Unit::Percent),
        (Clipper, "ceilingDb") => ("Ceiling", Taper::Linear, Unit::Db),
        (Transient, "attack") => ("Attack", Taper::Linear, Unit::Percent),
        (Transient, "sustain") => ("Sustain", Taper::Linear, Unit::Percent),
        (Transient, "speed") => ("Speed", Taper::Linear, Unit::Percent),
        (_, "mix") => ("Mix", Taper::Linear, Unit::Percent),
        _ => return None,
    };
    let mut knob = spec(range.id, label, range.min, range.max, taper, unit);
    // Drawn from where they rest: a trim at unity, a shaper at no change.
    knob.bipolar = matches!(
        (kind, id),
        (Fa2a, "gainDb" | "outputTrimDb")
            | (Zcomp, "makeupDb")
            | (BurnLimit, "gainDb")
            | (Transient, "attack" | "sustain")
    );
    Some(knob)
}

pub fn default_value(kind: DynKind, id: &str) -> f32 {
    DynParams::defaults(kind).value(id)
}

/// The ids every kind's knobs cover, for the tests and the preview.
pub fn knob_ids(kind: DynKind) -> &'static [&'static str] {
    match kind {
        DynKind::Fa2a => &[
            "peakReduction",
            "gainDb",
            "emphasis",
            "color",
            "sidechainLowCutHz",
            "outputTrimDb",
            "mix",
        ],
        DynKind::Fa76 => &[
            "inputDb",
            "outputDb",
            "attackUs",
            "releaseMs",
            "sidechainHpfHz",
            "mix",
        ],
        DynKind::Zcomp => &[
            "thresholdDb",
            "ratio",
            "attackMs",
            "releaseMs",
            "kneeDb",
            "makeupDb",
            "sidechainHpfHz",
            "stereoLink",
            "color",
            "mix",
        ],
        DynKind::BurnLimit => &["gainDb", "ceilingDb", "releaseMs", "lookaheadMs", "mix"],
        DynKind::Clipper => &["thresholdDb", "shape", "ceilingDb", "mix"],
        DynKind::Transient => &["attack", "sustain", "speed", "mix"],
    }
}

/// A choice param's labels, in wire order.
pub fn choice_labels(kind: DynKind) -> Option<(&'static str, &'static [&'static str])> {
    match kind {
        DynKind::Fa2a => Some(("mode", &["Compress", "Limit"])),
        DynKind::Fa76 => Some(("ratio", &["4", "8", "12", "20", "All"])),
        DynKind::Zcomp => Some(("model", &["2500", "Distress", "Avalon", "SSL"])),
        DynKind::BurnLimit => Some(("style", &["Clean", "Punch", "Modern", "Clip"])),
        DynKind::Clipper => Some(("mode", &["Clip", "Hybrid", "Limit"])),
        DynKind::Transient => None,
    }
}

/// What the selected choice does, in a line.
pub fn choice_hint(params: &DynParams) -> &'static str {
    match params {
        DynParams::Fa2a(p) => match p.mode {
            fa2a::Mode::Compress => "Gentle ratio, slow optical release",
            fa2a::Mode::Limit => "Higher ratio for peak control",
        },
        DynParams::Fa76(p) => match p.ratio {
            fa76::RatioButton::All => "All buttons in: hard-knee, aggressive, faster",
            _ => "Fixed −24 dB threshold: drive the input into it",
        },
        DynParams::Zcomp(p) => match p.model {
            zcomp::CompModel::Comp2500 => "VCA · feed-forward · thrust sidechain",
            zcomp::CompModel::Distressor => "FET · feedback loop · British grit",
            zcomp::CompModel::Avalon => "Opto · Class-A · over-easy ratio",
            zcomp::CompModel::Ssl => "Bus VCA · dual time-constant release",
        },
        DynParams::BurnLimit(p) => match p.style {
            burnlimit::Style::Clean => "4 dB knee, 2 ms attack",
            burnlimit::Style::Punch => "2 dB knee, 0.8 ms attack",
            burnlimit::Style::Modern => "1.5 dB knee, 0.4 ms attack",
            burnlimit::Style::Clip => "Near-hard 0.2 dB knee, 0.05 ms attack",
        },
        DynParams::Clipper(p) => match p.mode {
            clipper67::Mode::Clip => "Soft clipper only",
            clipper67::Mode::Hybrid => "Clipper plus peak gain reduction",
            clipper67::Mode::Limit => "Clipper plus the strongest peak gain reduction",
        },
        DynParams::Transient(_) => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index_of(kind: DynKind, index: u32) -> &'static str {
        match kind {
            DynKind::Fa2a => fa2a::ui_param_id(index),
            DynKind::Fa76 => fa76::ui_param_id(index),
            DynKind::Zcomp => zcomp::ui_param_id(index),
            DynKind::BurnLimit => burnlimit::ui_param_id(index),
            DynKind::Clipper => clipper67::ui_param_id(index),
            DynKind::Transient => transient::ui_param_id(index),
        }
        .unwrap()
    }

    fn same(a: &DynParams, b: &DynParams) -> bool {
        let theirs = b.wire_values();
        a.wire_values().iter().all(|(index, value)| {
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
        for kind in DynKind::ALL {
            let bank = presets(kind);
            assert_eq!(bank[0].name, "Default", "{kind:?}");
            for from in bank.iter() {
                for to in bank.iter() {
                    let before = from.params.wire_values();
                    let mut landed = from.params.clone();
                    for (index, value) in to.params.wire_values() {
                        if before.iter().any(|(i, v)| *i == index && *v == value) {
                            continue;
                        }
                        landed = landed.with(index_of(kind, index), value);
                    }
                    assert!(
                        same(&landed, &to.params),
                        "{kind:?}: {} → {}",
                        from.name,
                        to.name
                    );
                }
            }
        }
    }

    #[test]
    fn a_preset_keeps_power_and_never_turns_on_sc_listen() {
        let off = DynParams::defaults(DynKind::Zcomp)
            .with("power", 0.0)
            .with("scListen", 0.0);
        for preset in presets(DynKind::Zcomp).iter() {
            let applied = preset_applied(&off, &preset.params);
            assert!(!applied.power());
            assert_eq!(applied.value("scListen"), 0.0);
        }
    }

    #[test]
    fn every_knob_round_trips_through_its_taper() {
        for kind in DynKind::ALL {
            for id in knob_ids(kind) {
                let knob = knob(kind, id).unwrap_or_else(|| panic!("{kind:?}: no knob for {id}"));
                let default = default_value(kind, id);
                let back = knob.from_knob(knob.to_knob(default));
                assert!(
                    (back - default).abs() <= 0.011 * default.abs().max(1.0),
                    "{kind:?} {id}: {default} came back {back}"
                );
            }
        }
    }

    #[test]
    fn every_choice_label_names_a_wire_value() {
        for kind in DynKind::ALL {
            let Some((id, labels)) = choice_labels(kind) else {
                continue;
            };
            let params = DynParams::defaults(kind);
            for (index, _) in labels.iter().enumerate() {
                assert_eq!(
                    params.with(id, index as f32).value(id),
                    index as f32,
                    "{kind:?}"
                );
            }
        }
    }

    #[test]
    fn transfer_curves_rest_on_unity_below_the_action() {
        // Far under every threshold, the static curves pass the level as is.
        let quiet = -70.0;
        for kind in [DynKind::Zcomp, DynKind::BurnLimit] {
            let params = DynParams::defaults(kind);
            let out = params.transfer_db(quiet).unwrap();
            assert!((out - quiet).abs() < 0.2, "{kind:?}: {out}");
        }
        // A limiter never passes its ceiling.
        let limiter = DynParams::defaults(DynKind::BurnLimit).with("gainDb", 12.0);
        assert!(limiter.transfer_db(0.0).unwrap() <= -0.3 + 1.0e-3);
    }
}

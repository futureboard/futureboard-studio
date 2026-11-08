//! What the native dynamics editors edit: Z-Comp, 67Clipper and
//! Transient — their params behind one type, their wire table, presets and
//! choices.
//!
//! No GPUI here. Every value goes back to the plug-in as wire edits the DSP
//! crates' own `ipc` tables compute and clamp, so an edit lands in the
//! editor's copy exactly as it lands in Studio's state mirror and in the host.

use std::sync::{Arc, OnceLock};

use crate::components::plugin_kit::{bank, KitPreset};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DynKind {
    Zcomp,
    Clipper,
    Transient,
}

impl DynKind {
    pub const ALL: [DynKind; 3] = [DynKind::Zcomp, DynKind::Clipper, DynKind::Transient];

    pub fn title(self) -> &'static str {
        match self {
            Self::Zcomp => "Z-Comp",
            Self::Clipper => "67Clipper",
            Self::Transient => "Transient",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Self::Zcomp => "zcomp",
            Self::Clipper => "clipper67",
            Self::Transient => "transient",
        }
    }

    /// The DSP crate's descriptor row `id`: `(id, min, max, default)`.
    pub fn descriptor_row(self, id: &str) -> Option<(&'static str, f32, f32, f32)> {
        let descriptor = match self {
            Self::Zcomp => zcomp::descriptor(),
            Self::Clipper => clipper67::descriptor(),
            Self::Transient => transient::descriptor(),
        };
        descriptor
            .params
            .iter()
            .find(|row| row.id == id)
            .map(|row| (row.id, row.min, row.max, row.default_value))
    }
}

#[derive(Clone, Debug)]
pub enum DynParams {
    Zcomp(zcomp::Params),
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
            DynKind::Zcomp => Self::Zcomp(zcomp::default_params()),
            DynKind::Clipper => Self::Clipper(clipper67::default_params()),
            DynKind::Transient => Self::Transient(transient::default_params()),
        }
    }

    pub fn kind(&self) -> DynKind {
        match self {
            Self::Zcomp(_) => DynKind::Zcomp,
            Self::Clipper(_) => DynKind::Clipper,
            Self::Transient(_) => DynKind::Transient,
        }
    }

    fn ui_values(&self) -> Vec<(&'static str, f32)> {
        match self {
            Self::Zcomp(p) => zcomp::ipc::ui_values(p),
            Self::Clipper(p) => clipper67::ipc::ui_values(p),
            Self::Transient(p) => transient::ipc::ui_values(p),
        }
    }

    /// Every param as `(wire index, value)`, in wire order.
    pub fn wire_values(&self) -> Vec<(u32, f32)> {
        let values = self.ui_values();
        match self {
            Self::Zcomp(_) => indexed(values, zcomp::ui_param_index),
            Self::Clipper(_) => indexed(values, clipper67::ui_param_index),
            Self::Transient(_) => indexed(values, transient::ui_param_index),
        }
    }

    /// Applies one edit by id the way the DSP will.
    pub fn with(&self, id: &str, value: f32) -> Self {
        let mut next = self.clone();
        match &mut next {
            Self::Zcomp(p) => {
                zcomp::ipc::apply_ui_param(p, id, value);
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
    /// the DSP crates' own static models. `None` for a plug-in without one.
    pub fn transfer_db(&self, input_db: f32) -> Option<f32> {
        match self {
            Self::Zcomp(p) => Some(zcomp::transfer_db(p, input_db)),
            Self::Clipper(p) => Some(clipper67::transfer_db(p, input_db)),
            Self::Transient(_) => None,
        }
    }
}

// ── Presets ─────────────────────────────────────────────────────────────────

/// The kind's factory bank, `Default` first.
pub fn presets(kind: DynKind) -> Arc<Vec<KitPreset<DynParams>>> {
    static BANKS: OnceLock<Vec<Arc<Vec<KitPreset<DynParams>>>>> = OnceLock::new();
    let banks = BANKS.get_or_init(|| {
        vec![
            bank(zcomp::factory_presets(), |p| {
                (p.name, DynParams::Zcomp(p.params))
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
/// SC Listen — a listening aid, not a sound — stays off. 67Clipper keeps its
/// oversampling (a latency choice) and Delta (a listening aid).
pub fn preset_applied(current: &DynParams, preset: &DynParams) -> DynParams {
    if current.kind() != preset.kind() {
        return current.clone();
    }
    let next = preset.with("power", if current.power() { 1.0 } else { 0.0 });
    match &next {
        DynParams::Zcomp(_) => next.with("scListen", current.value("scListen")),
        DynParams::Clipper(_) => next
            .with("oversampling", current.value("oversampling"))
            .with("delta", current.value("delta")),
        _ => next,
    }
}

// ── Choices ─────────────────────────────────────────────────────────────────

/// A choice param's labels, in wire order.
pub fn choice_labels(kind: DynKind) -> Option<(&'static str, &'static [&'static str])> {
    match kind {
        DynKind::Zcomp => Some(("model", &["2500", "Distress", "Avalon", "SSL"])),
        DynKind::Clipper => Some(("mode", &["Clip", "Hybrid", "Limit"])),
        DynKind::Transient => None,
    }
}

/// What the selected choice does, in a line.
pub fn choice_hint(params: &DynParams) -> &'static str {
    match params {
        DynParams::Zcomp(p) => match p.model {
            zcomp::CompModel::Comp2500 => "VCA · feed-forward · thrust sidechain",
            zcomp::CompModel::Distressor => "FET · feedback loop · British grit",
            zcomp::CompModel::Avalon => "Opto · Class-A · over-easy ratio",
            zcomp::CompModel::Ssl => "Bus VCA · dual time-constant release",
        },
        DynParams::Clipper(p) => match p.mode {
            clipper67::Mode::Clip => "Clips at the threshold, untouched below the knee",
            clipper67::Mode::Hybrid => "Clips 3 dB past the knee, 1 ms limiter beyond",
            clipper67::Mode::Limit => "1 ms lookahead peak limiter at the threshold",
        },
        DynParams::Transient(_) => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index_of(kind: DynKind, index: u32) -> &'static str {
        match kind {
            DynKind::Zcomp => zcomp::ui_param_id(index),
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
    fn a_clipper_preset_keeps_oversampling_and_delta() {
        let live = DynParams::defaults(DynKind::Clipper)
            .with("oversampling", clipper67::Oversampling::X1.to_wire())
            .with("delta", 1.0);
        for preset in presets(DynKind::Clipper).iter() {
            let applied = preset_applied(&live, &preset.params);
            assert_eq!(applied.value("oversampling"), 0.0, "{}", preset.name);
            assert_eq!(applied.value("delta"), 1.0, "{}", preset.name);
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
        for kind in [DynKind::Zcomp, DynKind::Clipper] {
            let params = DynParams::defaults(kind);
            let out = params.transfer_db(quiet).unwrap();
            assert!((out - quiet).abs() < 0.2, "{kind:?}: {out}");
        }
    }
}

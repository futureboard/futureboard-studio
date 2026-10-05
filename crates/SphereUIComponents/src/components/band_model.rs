//! What the native band editors edit: the Compressor and Imager — their
//! params behind one type, wire table, presets and knobs.
//!
//! No GPUI here. Every value goes back to the plug-in as wire edits the DSP
//! crates' own `ipc` tables compute and clamp.

use std::sync::{Arc, OnceLock};

use crate::components::fx_model::{spec, KnobSpec, Taper, Unit};
use crate::components::plugin_kit::{bank, KitPreset};

/// Both plug-ins split the spectrum the same way.
pub const BANDS: usize = 4;
pub const CROSSOVERS: usize = BANDS - 1;
pub const BAND_NAMES: [&str; BANDS] = ["Low", "Low Mid", "High Mid", "High"];
/// The closest two crossovers may sit, as a frequency ratio.
pub const MIN_CROSSOVER_RATIO: f32 = 1.25;
pub const CROSSOVER_IDS: [&str; CROSSOVERS] = ["crossover1Hz", "crossover2Hz", "crossover3Hz"];

const _: () = assert!(compresser::BAND_COUNT == BANDS && imager::BAND_COUNT == BANDS);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BandKind {
    Comp,
    Imager,
}

#[derive(Clone, Debug)]
pub enum BandParams {
    Comp(compresser::Params),
    Imager(imager::Params),
}

fn indexed(
    values: Vec<(&'static str, f32)>,
    index: impl Fn(&str) -> Option<u32>,
) -> Vec<(u32, f32)> {
    values
        .into_iter()
        .filter_map(|(id, value)| index(id).map(|i| (i, value)))
        .collect()
}

impl BandParams {
    pub fn defaults(kind: BandKind) -> Self {
        match kind {
            BandKind::Comp => Self::Comp(compresser::default_params()),
            BandKind::Imager => Self::Imager(imager::default_params()),
        }
    }

    pub fn kind(&self) -> BandKind {
        match self {
            Self::Comp(_) => BandKind::Comp,
            Self::Imager(_) => BandKind::Imager,
        }
    }

    fn ui_values(&self) -> Vec<(&'static str, f32)> {
        match self {
            Self::Comp(p) => compresser::ipc::ui_values(p),
            Self::Imager(p) => imager::ipc::ui_values(p),
        }
    }

    pub fn wire_values(&self) -> Vec<(u32, f32)> {
        let values = self.ui_values();
        match self {
            Self::Comp(_) => indexed(values, compresser::ui_param_index),
            Self::Imager(_) => indexed(values, imager::ui_param_index),
        }
    }

    pub fn with(&self, id: &str, value: f32) -> Self {
        let mut next = self.clone();
        match &mut next {
            Self::Comp(p) => {
                compresser::ipc::apply_ui_param(p, id, value);
            }
            Self::Imager(p) => {
                imager::ipc::apply_ui_param(p, id, value);
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

    /// The soloed band, if any.
    pub fn solo(&self) -> Option<usize> {
        let solo = self.value("soloBand").round();
        (solo >= 0.0).then_some(solo as usize)
    }

    /// The crossovers as the plug-in stores them.
    pub fn crossovers(&self) -> [f32; CROSSOVERS] {
        match self {
            Self::Comp(p) => p.crossover_hz,
            Self::Imager(p) => p.crossover_hz,
        }
    }

    /// The band edges, low to high, from 20 Hz to 20 kHz: the crossovers
    /// sorted, as the DSP sorts them.
    pub fn edges(&self) -> [f32; BANDS + 1] {
        let mut sorted = self.crossovers();
        sorted.sort_by(f32::total_cmp);
        [20.0, sorted[0], sorted[1], sorted[2], 20_000.0]
    }

    /// Whether the band split is in play: in Multi mode for the Compressor,
    /// with Multiband on for the Imager.
    pub fn multiband(&self) -> bool {
        match self {
            Self::Comp(p) => p.mode == compresser::Mode::Multi,
            Self::Imager(p) => p.multiband,
        }
    }
}

/// How far crossover `index` may move: past neither neighbour, kept
/// [`MIN_CROSSOVER_RATIO`] clear of each.
pub fn crossover_bounds(crossovers: &[f32; CROSSOVERS], index: usize) -> (f32, f32) {
    let low = if index == 0 {
        20.0
    } else {
        crossovers[index - 1] * MIN_CROSSOVER_RATIO
    };
    let high = if index + 1 == CROSSOVERS {
        20_000.0
    } else {
        crossovers[index + 1] / MIN_CROSSOVER_RATIO
    };
    (low, high.max(low))
}

/// A band param's id, e.g. `band2Ratio` for `(1, "Ratio")`.
pub fn band_id(band: usize, field: &str) -> &'static str {
    let ids: &[&'static str] = &compresser::UI_PARAM_IDS;
    let wanted = format!("band{}{field}", band + 1);
    ids.iter().copied().find(|id| *id == wanted).unwrap_or("")
}

pub fn width_id(band: usize) -> &'static str {
    ["width1", "width2", "width3", "width4"][band.min(BANDS - 1)]
}

pub fn stereoize_id(band: usize) -> &'static str {
    ["stereoize1", "stereoize2", "stereoize3", "stereoize4"][band.min(BANDS - 1)]
}

/// A width as Ozone-style imagers read it: −100 folds to mono, 0 leaves the
/// band as it came, +100 doubles its side.
pub fn width_readout(width: f32) -> String {
    let relative = (width - imager::DEFAULT_WIDTH).round();
    if relative.abs() < 0.5 {
        "0".to_string()
    } else {
        format!("{relative:+.0}")
    }
}

// ── Presets ─────────────────────────────────────────────────────────────────

pub fn presets(kind: BandKind) -> Arc<Vec<KitPreset<BandParams>>> {
    static COMP: OnceLock<Arc<Vec<KitPreset<BandParams>>>> = OnceLock::new();
    static IMAGER: OnceLock<Arc<Vec<KitPreset<BandParams>>>> = OnceLock::new();
    match kind {
        BandKind::Comp => COMP
            .get_or_init(|| {
                bank(compresser::factory_presets(), |p| {
                    (p.name, BandParams::Comp(p.params))
                })
            })
            .clone(),
        BandKind::Imager => IMAGER
            .get_or_init(|| {
                bank(imager::factory_presets(), |p| {
                    (p.name, BandParams::Imager(p.params))
                })
            })
            .clone(),
    }
}

/// `preset` as it would play here: power and the soloed band stay as they
/// are — a solo is listening, not sound.
pub fn preset_applied(current: &BandParams, preset: &BandParams) -> BandParams {
    if current.kind() != preset.kind() {
        return current.clone();
    }
    preset
        .with("power", current.value("power"))
        .with("soloBand", current.value("soloBand"))
}

// ── Knobs ───────────────────────────────────────────────────────────────────

pub fn knob(kind: BandKind, id: &str) -> Option<KnobSpec> {
    let descriptor = match kind {
        BandKind::Comp => compresser::descriptor(),
        BandKind::Imager => imager::descriptor(),
    };
    let range = descriptor.params.iter().find(|p| p.id == id)?;
    // A band's knobs read like the single band's.
    let field = id
        .strip_prefix("band")
        .and_then(|rest| rest.get(1..))
        .unwrap_or(id);
    let (label, taper, unit) = match (kind, field) {
        (BandKind::Comp, "thresholdDb" | "ThresholdDb") => ("Thresh", Taper::Linear, Unit::Db),
        (BandKind::Comp, "ratio" | "Ratio") => ("Ratio", Taper::Log, Unit::Ratio),
        (BandKind::Comp, "attackMs" | "AttackMs") => ("Attack", Taper::Log, Unit::Ms),
        (BandKind::Comp, "releaseMs" | "ReleaseMs") => ("Release", Taper::Log, Unit::Ms),
        (BandKind::Comp, "makeupDb" | "MakeupDb") => ("Makeup", Taper::Linear, Unit::Db),
        (BandKind::Comp, "sidechainHpfHz") => (
            "SC HPF",
            Taper::Square,
            Unit::CutHz(compresser::SIDECHAIN_OFF_HZ),
        ),
        (BandKind::Comp, "kneeDb") => ("Knee", Taper::Linear, Unit::Db),
        (BandKind::Comp, "mix") => ("Mix", Taper::Linear, Unit::Percent),
        (BandKind::Imager, _) if id.starts_with("width") => ("Width", Taper::Linear, Unit::Percent),
        (BandKind::Imager, _) if id.starts_with("stereoize") && id != "stereoizeMode" => {
            ("Stereoize", Taper::Linear, Unit::Percent)
        }
        (_, "outputDb") => ("Output", Taper::Linear, Unit::Db),
        _ => return None,
    };
    let mut knob = spec(range.id, label, range.min, range.max, taper, unit);
    match field {
        "makeupDb" | "MakeupDb" | "outputDb" => knob.bipolar = true,
        _ if id.starts_with("width") => {
            knob.bipolar = true;
            knob.centre = imager::DEFAULT_WIDTH;
        }
        _ => {}
    }
    Some(knob)
}

pub fn default_value(kind: BandKind, id: &str) -> f32 {
    BandParams::defaults(kind).value(id)
}

/// A width in words.
pub fn describe_width(width: f32) -> &'static str {
    if width < 0.5 {
        "Mono"
    } else if width < 95.0 {
        "Narrower"
    } else if width <= 105.0 {
        "Unchanged"
    } else {
        "Wider"
    }
}

/// A frequency for a label: `120`, `1.2k`, `12k`.
pub fn short_hz(hz: f32) -> String {
    if hz >= 10_000.0 {
        format!("{:.1}k", hz / 1_000.0)
    } else if hz >= 1_000.0 {
        format!("{:.2}k", hz / 1_000.0)
    } else {
        format!("{hz:.0}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id_at(kind: BandKind, index: u32) -> &'static str {
        match kind {
            BandKind::Comp => compresser::ui_param_id(index),
            BandKind::Imager => imager::ui_param_id(index),
        }
        .unwrap()
    }

    fn same(a: &BandParams, b: &BandParams) -> bool {
        let theirs = b.wire_values();
        a.wire_values().iter().all(|(index, value)| {
            theirs
                .iter()
                .find(|(i, _)| i == index)
                .is_some_and(|(_, other)| (value - other).abs() <= 0.002 * value.abs().max(1.0))
        })
    }

    #[test]
    fn every_preset_lands_exactly_from_every_other() {
        for kind in [BandKind::Comp, BandKind::Imager] {
            let bank = presets(kind);
            for from in bank.iter() {
                for to in bank.iter() {
                    let before = from.params.wire_values();
                    let mut landed = from.params.clone();
                    for (index, value) in to.params.wire_values() {
                        if before.iter().any(|(i, v)| *i == index && *v == value) {
                            continue;
                        }
                        landed = landed.with(id_at(kind, index), value);
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
    fn a_preset_keeps_power_and_the_solo() {
        let current = BandParams::defaults(BandKind::Imager)
            .with("power", 0.0)
            .with("soloBand", 2.0);
        for preset in presets(BandKind::Imager).iter() {
            let applied = preset_applied(&current, &preset.params);
            assert!(!applied.power());
            assert_eq!(applied.solo(), Some(2));
        }
    }

    #[test]
    fn every_knob_round_trips_through_its_taper() {
        let comp: Vec<&str> = [
            "thresholdDb",
            "ratio",
            "attackMs",
            "releaseMs",
            "makeupDb",
            "sidechainHpfHz",
            "kneeDb",
            "mix",
            "outputDb",
        ]
        .into_iter()
        .chain((0..BANDS).flat_map(|b| {
            ["ThresholdDb", "Ratio", "AttackMs", "ReleaseMs", "MakeupDb"].map(|f| band_id(b, f))
        }))
        .collect();
        let imager: Vec<&str> = (0..BANDS)
            .map(width_id)
            .chain((0..BANDS).map(stereoize_id))
            .chain(["outputDb"])
            .collect();
        for (kind, ids) in [(BandKind::Comp, comp), (BandKind::Imager, imager)] {
            for id in ids {
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
    fn widths_read_from_mono_to_double() {
        assert_eq!(width_readout(0.0), "-100");
        assert_eq!(width_readout(100.0), "0");
        assert_eq!(width_readout(130.0), "+30");
        assert_eq!(width_readout(200.0), "+100");
    }

    #[test]
    fn crossovers_keep_their_distance() {
        let crossovers = [120.0, 1_000.0, 6_000.0];
        assert_eq!(crossover_bounds(&crossovers, 0), (20.0, 800.0));
        assert_eq!(crossover_bounds(&crossovers, 1), (150.0, 4_800.0));
        assert_eq!(crossover_bounds(&crossovers, 2), (1_250.0, 20_000.0));
        let edges = BandParams::defaults(BandKind::Comp)
            .with("crossover1Hz", 3_000.0)
            .edges();
        assert!(edges.windows(2).all(|pair| pair[0] <= pair[1]));
    }
}

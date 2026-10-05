//! What the native VerbSpace and EchoSpace editors edit: the two plug-ins'
//! params behind one type, their wire edits, presets, and the knob table the
//! panels are drawn from.
//!
//! No GPUI here. Every value goes back to the plug-in as the wire edits
//! [`FxParams::wire_diff`] computes — the DSP crates' own `ipc` tables do the
//! clamping and the Link bookkeeping, so an edit lands in the editor's copy
//! exactly as it lands in Studio's state mirror and in the host.

use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FxKind {
    Verb,
    Echo,
}

impl FxKind {
    pub fn title(self) -> &'static str {
        match self {
            Self::Verb => "VerbSpace",
            Self::Echo => "EchoSpace",
        }
    }

    pub fn subtitle(self) -> &'static str {
        match self {
            Self::Verb => "Algorithmic reverb",
            Self::Echo => "Stereo delay",
        }
    }

    /// Element-id prefix, so the two editors' controls never share ids.
    pub fn key(self) -> &'static str {
        match self {
            Self::Verb => "verb",
            Self::Echo => "echo",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum FxParams {
    Verb(verbspace::Params),
    Echo(echospace::Params),
}

impl FxParams {
    pub fn defaults(kind: FxKind) -> Self {
        match kind {
            FxKind::Verb => Self::Verb(verbspace::default_params()),
            FxKind::Echo => Self::Echo(echospace::default_params()),
        }
    }

    pub fn kind(&self) -> FxKind {
        match self {
            Self::Verb(_) => FxKind::Verb,
            Self::Echo(_) => FxKind::Echo,
        }
    }

    /// Every param as `(wire index, value)`, in the order an editor sends
    /// them. EchoSpace's `link` leads: while it is on the DSP mirrors one
    /// side's edit onto the other, so turning it off has to arrive before
    /// two sides that differ.
    pub fn wire_values(&self) -> Vec<(u32, f32)> {
        match self {
            Self::Verb(p) => verbspace::ipc::ui_values(p)
                .into_iter()
                .filter_map(|(id, value)| verbspace::ui_param_index(id).map(|i| (i, value)))
                .collect(),
            Self::Echo(p) => {
                let mut values: Vec<_> = echospace::ipc::ui_values(p)
                    .into_iter()
                    .filter_map(|(id, value)| echospace::ui_param_index(id).map(|i| (i, value)))
                    .collect();
                values.sort_by_key(|(index, _)| *index != echospace::ipc::LINK_INDEX);
                values
            }
        }
    }

    /// The wire edits that turn `self` into `next`.
    pub fn wire_diff(&self, next: &Self) -> Vec<(u32, f32)> {
        if self.kind() != next.kind() {
            return Vec::new();
        }
        let before = self.wire_values();
        next.wire_values()
            .into_iter()
            .filter(|(index, value)| {
                before
                    .iter()
                    .find(|(i, _)| i == index)
                    .is_none_or(|(_, old)| old != value)
            })
            .collect()
    }

    /// Applies one edit by id the way the DSP will, Link included.
    pub fn with(&self, id: &str, value: f32) -> Self {
        let mut next = self.clone();
        match &mut next {
            Self::Verb(p) => {
                verbspace::ipc::apply_ui_param(p, id, value);
            }
            Self::Echo(p) => {
                echospace::ipc::apply_ui_param(p, id, value);
            }
        }
        next
    }

    /// A param's value by id.
    pub fn value(&self, id: &str) -> f32 {
        let values = match self {
            Self::Verb(p) => verbspace::ipc::ui_values(p),
            Self::Echo(p) => echospace::ipc::ui_values(p),
        };
        values
            .into_iter()
            .find(|(candidate, _)| *candidate == id)
            .map_or(0.0, |(_, value)| value)
    }

    pub fn flag(&self, id: &str) -> bool {
        self.value(id) >= 0.5
    }

    pub fn power(&self) -> bool {
        self.flag("power")
    }

    /// Whether two param sets sound the same: equal within the rounding a
    /// knob and the wire leave behind.
    pub fn same_sound(&self, other: &Self) -> bool {
        if self.kind() != other.kind() {
            return false;
        }
        let ours = self.wire_values();
        let theirs = other.wire_values();
        ours.iter().all(|(index, value)| {
            theirs
                .iter()
                .find(|(i, _)| i == index)
                .is_some_and(|(_, other)| (value - other).abs() <= 0.002 * value.abs().max(1.0))
        })
    }
}

// ── Presets ─────────────────────────────────────────────────────────────────

pub struct Preset {
    pub name: &'static str,
    pub params: FxParams,
}

/// The kind's factory bank, `Default` first.
pub fn presets(kind: FxKind) -> Arc<Vec<Preset>> {
    use std::sync::OnceLock;
    static VERB: OnceLock<Arc<Vec<Preset>>> = OnceLock::new();
    static ECHO: OnceLock<Arc<Vec<Preset>>> = OnceLock::new();
    match kind {
        FxKind::Verb => VERB
            .get_or_init(|| {
                Arc::new(
                    verbspace::factory_presets()
                        .into_iter()
                        .map(|p| Preset {
                            name: p.name,
                            params: FxParams::Verb(p.params),
                        })
                        .collect(),
                )
            })
            .clone(),
        FxKind::Echo => ECHO
            .get_or_init(|| {
                Arc::new(
                    echospace::factory_presets()
                        .into_iter()
                        .map(|p| Preset {
                            name: p.name,
                            params: FxParams::Echo(p.params),
                        })
                        .collect(),
                )
            })
            .clone(),
    }
}

/// `preset` as it would play here: a preset carries no listening state, so
/// power and freeze stay as they are.
pub fn preset_applied(current: &FxParams, preset: &FxParams) -> FxParams {
    let mut next = preset.clone();
    match (&mut next, current) {
        (FxParams::Verb(next), FxParams::Verb(current)) => {
            next.power = current.power;
            next.freeze = current.freeze;
        }
        (FxParams::Echo(next), FxParams::Echo(current)) => {
            next.power = current.power;
            next.freeze = current.freeze;
        }
        _ => {}
    }
    next
}

/// The preset the params are, if any, ignoring power and freeze.
pub fn matching_preset(params: &FxParams) -> Option<usize> {
    presets(params.kind())
        .iter()
        .position(|preset| preset_applied(params, &preset.params).same_sound(params))
}

// ── Knobs ───────────────────────────────────────────────────────────────────

/// How a knob's travel maps onto its range.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Taper {
    Linear,
    /// Equal ratios per equal turn: frequencies, times, decays.
    Log,
    /// Fine at the bottom: pre-delay, where the first milliseconds matter
    /// most.
    Square,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Unit {
    Ms,
    Sec,
    Hz,
    Percent,
    Db,
    Times,
    /// A note division, by its index into `echospace::DIVISION_LABELS`.
    Division,
    /// Whole semitones.
    Semitones,
    Cents,
    /// A depth either side, in cents.
    CentsDepth,
    /// Microseconds.
    Us,
    /// A compression ratio, `n:1`.
    Ratio,
    /// A bare number, no unit.
    Plain,
    /// A sidechain filter in Hz, off at or below the threshold given.
    CutHz(f32),
}

/// One knob: which param, its label, range, taper and readout.
#[derive(Clone, Copy, Debug)]
pub struct KnobSpec {
    pub id: &'static str,
    pub label: &'static str,
    pub min: f32,
    pub max: f32,
    pub taper: Taper,
    pub unit: Unit,
    /// Drawn from `centre` outward rather than from the bottom.
    pub bipolar: bool,
    /// The value a bipolar knob's arc starts from.
    pub centre: f32,
}

pub(crate) const fn spec(
    id: &'static str,
    label: &'static str,
    min: f32,
    max: f32,
    taper: Taper,
    unit: Unit,
) -> KnobSpec {
    KnobSpec {
        id,
        label,
        min,
        max,
        taper,
        unit,
        bipolar: false,
        centre: 0.0,
    }
}

impl KnobSpec {
    /// The knob's position, 0–1, for `value`.
    pub fn fraction(&self, value: f32) -> f32 {
        let v = value.clamp(self.min, self.max);
        match self.taper {
            Taper::Linear => (v - self.min) / (self.max - self.min),
            Taper::Log => (v / self.min).ln() / (self.max / self.min).ln(),
            Taper::Square => ((v - self.min) / (self.max - self.min)).sqrt(),
        }
    }

    /// The value at knob position `fraction`, rounded to what the readout
    /// shows.
    pub fn value_at(&self, fraction: f32) -> f32 {
        let f = fraction.clamp(0.0, 1.0);
        let raw = match self.taper {
            Taper::Linear => self.min + f * (self.max - self.min),
            Taper::Log => self.min * (self.max / self.min).powf(f),
            Taper::Square => self.min + f * f * (self.max - self.min),
        };
        round_for(self.unit, raw)
    }

    /// The span the knob widget runs over: 0–1 through the taper for a
    /// unipolar knob; the raw range, shifted so `centre` is zero, for a
    /// bipolar one (its arc starts at zero).
    pub fn knob_range(&self) -> (f32, f32) {
        if self.bipolar {
            (self.min - self.centre, self.max - self.centre)
        } else {
            (0.0, 1.0)
        }
    }

    /// `value` in the knob widget's units.
    pub fn to_knob(&self, value: f32) -> f32 {
        if self.bipolar {
            value.clamp(self.min, self.max) - self.centre
        } else {
            self.fraction(value)
        }
    }

    /// The param value at the knob widget's `units`.
    pub fn from_knob(&self, units: f32) -> f32 {
        if self.bipolar {
            round_for(self.unit, (units + self.centre).clamp(self.min, self.max))
        } else {
            self.value_at(units)
        }
    }

    pub fn readout(&self, value: f32) -> String {
        format_value(self.unit, value)
    }
}

/// `value` rounded to its readout's resolution, so a knob never leaves a
/// value behind that the readout cannot show.
pub fn round_for(unit: Unit, value: f32) -> f32 {
    let step = match unit {
        Unit::Ms if value < 1.0 => 0.01,
        Unit::Ms if value < 10.0 => 0.1,
        Unit::Ms => 1.0,
        Unit::Sec if value < 10.0 => 0.01,
        Unit::Sec => 0.1,
        Unit::Hz if value < 1.0 => 0.01,
        // The readout shows tenths below 10 Hz; a whole-hertz step would
        // snap a 5.5 Hz rate to 6.
        Unit::Hz if value < 10.0 => 0.1,
        Unit::Hz if value < 1_000.0 => 1.0,
        Unit::Hz => 10.0,
        Unit::Percent => 1.0,
        Unit::Db => 0.1,
        Unit::Times => 0.01,
        Unit::Division | Unit::Semitones | Unit::Cents | Unit::CentsDepth => 1.0,
        Unit::Us | Unit::Plain | Unit::CutHz(_) => 1.0,
        Unit::Ratio if value < 10.0 => 0.1,
        Unit::Ratio => 0.5,
    };
    (value / step).round() * step
}

pub fn format_value(unit: Unit, value: f32) -> String {
    match unit {
        Unit::Ms if value >= 1_000.0 => format!("{:.2} s", value / 1_000.0),
        Unit::Ms if value < 1.0 => format!("{value:.2} ms"),
        Unit::Ms if value < 10.0 => format!("{value:.1} ms"),
        Unit::Ms => format!("{value:.0} ms"),
        Unit::Sec if value.is_infinite() => "∞".to_string(),
        Unit::Sec if value < 10.0 => format!("{value:.2} s"),
        Unit::Sec => format!("{value:.1} s"),
        Unit::Hz if value < 1.0 => format!("{value:.2} Hz"),
        Unit::Hz if value < 10.0 => format!("{value:.1} Hz"),
        Unit::Hz if value < 1_000.0 => format!("{value:.0} Hz"),
        Unit::Hz => format!("{:.1} kHz", value / 1_000.0),
        Unit::Percent => format!("{value:.0} %"),
        Unit::Db if value.abs() < 0.05 => "0.0 dB".to_string(),
        Unit::Db => format!("{value:+.1} dB"),
        Unit::Times => format!("{value:.2}×"),
        Unit::Semitones if value.abs() < 0.5 => "0 st".to_string(),
        Unit::Semitones => format!("{value:+.0} st"),
        Unit::Cents if value.abs() < 0.5 => "0 ¢".to_string(),
        Unit::Cents => format!("{value:+.0} ¢"),
        Unit::CentsDepth => format!("±{value:.0} ¢"),
        Unit::Us => format!("{value:.0} µs"),
        Unit::Ratio if value < 10.0 => format!("{value:.1}:1"),
        Unit::Ratio => format!("{value:.0}:1"),
        Unit::Plain => format!("{value:.0}"),
        Unit::CutHz(off) if value <= off => "Off".to_string(),
        Unit::CutHz(_) => format_value(Unit::Hz, value),
        Unit::Division => echospace::DIVISION_LABELS
            .get(value.round().clamp(0.0, echospace::MAX_DIVISION_WIRE) as usize)
            .copied()
            .unwrap_or("—")
            .to_string(),
    }
}

/// The knob table. Ranges come from the DSP crates' descriptors, so a range
/// change there reaches the editor without a second edit here.
pub fn knob(kind: FxKind, id: &str) -> Option<KnobSpec> {
    let descriptor = match kind {
        FxKind::Verb => verbspace::descriptor(),
        FxKind::Echo => echospace::descriptor(),
    };
    let range = descriptor.params.iter().find(|p| p.id == id)?;
    let (label, taper, unit) = match (kind, id) {
        (FxKind::Verb, "predelayMs") => ("Pre-Delay", Taper::Square, Unit::Ms),
        (FxKind::Verb, "size") => ("Size", Taper::Linear, Unit::Percent),
        (FxKind::Verb, "decaySec") => ("Decay", Taper::Log, Unit::Sec),
        (FxKind::Verb, "diffusion") => ("Diffusion", Taper::Linear, Unit::Percent),
        (FxKind::Verb, "damping") => ("Damping", Taper::Linear, Unit::Percent),
        (FxKind::Verb, "bassMult") => ("Bass", Taper::Log, Unit::Times),
        (FxKind::Verb, "modDepth") => ("Depth", Taper::Linear, Unit::Percent),
        (FxKind::Verb, "modRateHz") => ("Rate", Taper::Log, Unit::Hz),
        (FxKind::Echo, "timeMsL") => ("Time L", Taper::Log, Unit::Ms),
        (FxKind::Echo, "timeMsR") => ("Time R", Taper::Log, Unit::Ms),
        (FxKind::Echo, "divisionL") => ("Note L", Taper::Linear, Unit::Division),
        (FxKind::Echo, "divisionR") => ("Note R", Taper::Linear, Unit::Division),
        (FxKind::Echo, "feedback") => ("Feedback", Taper::Linear, Unit::Percent),
        (FxKind::Echo, "crossFeedback") => ("Cross", Taper::Linear, Unit::Percent),
        (FxKind::Echo, "saturation") => ("Drive", Taper::Linear, Unit::Percent),
        (FxKind::Echo, "modDepth") => ("Wow", Taper::Linear, Unit::Percent),
        (FxKind::Echo, "modRateHz") => ("Rate", Taper::Log, Unit::Hz),
        (FxKind::Echo, "duck") => ("Duck", Taper::Linear, Unit::Percent),
        (FxKind::Echo, "diffusion") => ("Diffusion", Taper::Linear, Unit::Percent),
        (_, "lowCutHz") => ("Low Cut", Taper::Log, Unit::Hz),
        (_, "highCutHz") => ("High Cut", Taper::Log, Unit::Hz),
        (_, "width") => ("Width", Taper::Linear, Unit::Percent),
        (_, "mix") => ("Mix", Taper::Linear, Unit::Percent),
        (_, "outputDb") => ("Output", Taper::Linear, Unit::Db),
        _ => return None,
    };
    let mut knob = spec(range.id, label, range.min, range.max, taper, unit);
    // Output trims around unity, width around an untouched image.
    match id {
        "outputDb" => knob.bipolar = true,
        "width" => {
            knob.bipolar = true;
            knob.centre = 100.0;
        }
        _ => {}
    }
    Some(knob)
}

/// A param's factory default, by id.
pub fn default_value(kind: FxKind, id: &str) -> f32 {
    FxParams::defaults(kind).value(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_diff_carries_exactly_what_changed() {
        let before = FxParams::defaults(FxKind::Verb);
        let after = before.with("decaySec", 6.0).with("mode", 3.0);
        let diff = before.wire_diff(&after);
        assert_eq!(
            diff,
            vec![
                (verbspace::ipc::MODE_INDEX, 3.0),
                (verbspace::ipc::DECAY_INDEX, 6.0)
            ]
        );
        assert!(after.wire_diff(&after).is_empty());
    }

    /// Replaying a diff through the DSP's own wire table lands on the target,
    /// Link and all — the same path Studio's mirror and the host take.
    #[test]
    fn every_preset_lands_exactly_from_every_other() {
        for kind in [FxKind::Verb, FxKind::Echo] {
            let bank = presets(kind);
            for from in bank.iter() {
                for to in bank.iter() {
                    let mut landed = from.params.clone();
                    for (index, value) in from.params.wire_diff(&to.params) {
                        landed = match &landed {
                            FxParams::Verb(_) => {
                                landed.with(verbspace::ui_param_id(index).unwrap(), value)
                            }
                            FxParams::Echo(_) => {
                                landed.with(echospace::ui_param_id(index).unwrap(), value)
                            }
                        };
                    }
                    assert!(
                        landed.same_sound(&to.params),
                        "{} → {} did not land",
                        from.name,
                        to.name
                    );
                }
            }
        }
    }

    #[test]
    fn presets_are_recognised_whatever_the_power() {
        for kind in [FxKind::Verb, FxKind::Echo] {
            for (index, preset) in presets(kind).iter().enumerate() {
                assert_eq!(
                    matching_preset(&preset.params),
                    Some(index),
                    "{}",
                    preset.name
                );
                let off = preset.params.with("power", 0.0);
                assert_eq!(matching_preset(&off), Some(index));
            }
            let edited = FxParams::defaults(kind).with("mix", 77.0);
            assert_eq!(matching_preset(&edited), None);
        }
    }

    #[test]
    fn every_knob_round_trips_through_its_taper() {
        for (kind, ids) in [
            (
                FxKind::Verb,
                &[
                    "predelayMs",
                    "size",
                    "decaySec",
                    "diffusion",
                    "damping",
                    "bassMult",
                    "modDepth",
                    "modRateHz",
                    "lowCutHz",
                    "highCutHz",
                    "width",
                    "mix",
                    "outputDb",
                ][..],
            ),
            (
                FxKind::Echo,
                &[
                    "timeMsL",
                    "timeMsR",
                    "divisionL",
                    "divisionR",
                    "feedback",
                    "crossFeedback",
                    "saturation",
                    "modDepth",
                    "modRateHz",
                    "duck",
                    "diffusion",
                    "lowCutHz",
                    "highCutHz",
                    "width",
                    "mix",
                    "outputDb",
                ][..],
            ),
        ] {
            for id in ids {
                let knob = knob(kind, id).unwrap_or_else(|| panic!("no knob for {id}"));
                assert_eq!(knob.value_at(0.0), round_for(knob.unit, knob.min), "{id}");
                assert_eq!(knob.value_at(1.0), round_for(knob.unit, knob.max), "{id}");
                let default = default_value(kind, id);
                let back = knob.from_knob(knob.to_knob(default));
                assert!(
                    (back - default).abs() <= 0.011 * default.abs().max(1.0),
                    "{id}: {default} came back {back}"
                );
            }
        }
    }

    #[test]
    fn readouts_read_like_a_studio() {
        assert_eq!(format_value(Unit::Ms, 375.0), "375 ms");
        assert_eq!(format_value(Unit::Ms, 1_250.0), "1.25 s");
        assert_eq!(format_value(Unit::Hz, 9_500.0), "9.5 kHz");
        assert_eq!(format_value(Unit::Db, -1.5), "-1.5 dB");
        assert_eq!(format_value(Unit::Db, 0.01), "0.0 dB");
        assert_eq!(format_value(Unit::Division, 9.0), "1/8.");
        assert_eq!(format_value(Unit::Sec, f32::INFINITY), "∞");
        assert_eq!(format_value(Unit::Ms, 0.25), "0.25 ms");
        assert_eq!(round_for(Unit::Ms, 0.013), 0.01);
        assert_eq!(format_value(Unit::Ratio, 4.0), "4.0:1");
        assert_eq!(format_value(Unit::Ratio, 20.0), "20:1");
        assert_eq!(format_value(Unit::CutHz(20.0), 0.0), "Off");
        assert_eq!(format_value(Unit::CutHz(20.0), 90.0), "90 Hz");
        assert_eq!(format_value(Unit::Us, 400.0), "400 µs");
    }
}

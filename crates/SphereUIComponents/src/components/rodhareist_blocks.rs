//! Rodhareist's block model, as the native editor presents it.
//!
//! Pure data over `rodharerist`: which category each [`StageKind`] belongs
//! to, the models a block offers and which is selected, the slider rows a
//! block shows, and how a value reads. Ranges and units come from the
//! plug-in's own `descriptor()` (NAM rows, which it does not describe, from
//! the DSP's documented ranges); every edit is a wire id applied with
//! `rodharerist::apply_to_params`, so this module never touches a field the
//! DSP does not route.

use rodharerist::{
    AmpModel, CabModel, DelayModel, DriveModel, EqModel, MicModel, ModModel, Params, ReverbModel,
    StageKind, ToneEngineKind, WahModel,
};

/// The block categories the editor lists, in signal-chain order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Category {
    Gate,
    Comp,
    Wah,
    Drive,
    Amp,
    Cab,
    Eq,
    Mod,
    Delay,
    Reverb,
}

/// Category colour family: amber for the dirt and dynamics before the amp,
/// teal for time and modulation, neutral for tone shaping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tint {
    Amber,
    Teal,
    Neutral,
}

impl Category {
    pub const ALL: [Category; 10] = [
        Category::Gate,
        Category::Comp,
        Category::Wah,
        Category::Drive,
        Category::Amp,
        Category::Cab,
        Category::Eq,
        Category::Mod,
        Category::Delay,
        Category::Reverb,
    ];

    pub fn of(kind: StageKind) -> Category {
        match kind {
            StageKind::Gate => Category::Gate,
            StageKind::Comp | StageKind::Comp2 => Category::Comp,
            StageKind::Wah => Category::Wah,
            StageKind::Drive | StageKind::Drive2 => Category::Drive,
            StageKind::Amp => Category::Amp,
            StageKind::Cab => Category::Cab,
            StageKind::Eq | StageKind::Eq2 => Category::Eq,
            StageKind::Mod | StageKind::Mod2 => Category::Mod,
            StageKind::Delay | StageKind::Delay2 => Category::Delay,
            StageKind::Reverb => Category::Reverb,
        }
    }

    /// The stages of this category, first instance first.
    pub fn kinds(self) -> &'static [StageKind] {
        match self {
            Category::Gate => &[StageKind::Gate],
            Category::Comp => &[StageKind::Comp, StageKind::Comp2],
            Category::Wah => &[StageKind::Wah],
            Category::Drive => &[StageKind::Drive, StageKind::Drive2],
            Category::Amp => &[StageKind::Amp],
            Category::Cab => &[StageKind::Cab],
            Category::Eq => &[StageKind::Eq, StageKind::Eq2],
            Category::Mod => &[StageKind::Mod, StageKind::Mod2],
            Category::Delay => &[StageKind::Delay, StageKind::Delay2],
            Category::Reverb => &[StageKind::Reverb],
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Category::Gate => "Gate",
            Category::Comp => "Compressor",
            Category::Wah => "Wah",
            Category::Drive => "Drive",
            Category::Amp => "Amp",
            Category::Cab => "Cab",
            Category::Eq => "EQ",
            Category::Mod => "Modulation",
            Category::Delay => "Delay",
            Category::Reverb => "Reverb",
        }
    }

    /// The three-letter mark on a signal-path node.
    pub fn glyph(self) -> &'static str {
        match self {
            Category::Gate => "GTE",
            Category::Comp => "CMP",
            Category::Wah => "WAH",
            Category::Drive => "DRV",
            Category::Amp => "AMP",
            Category::Cab => "CAB",
            Category::Eq => "EQ",
            Category::Mod => "MOD",
            Category::Delay => "DLY",
            Category::Reverb => "REV",
        }
    }

    pub fn tint(self) -> Tint {
        match self {
            Category::Gate | Category::Comp | Category::Wah | Category::Drive | Category::Amp => {
                Tint::Amber
            }
            Category::Cab | Category::Eq => Tint::Neutral,
            Category::Mod | Category::Delay | Category::Reverb => Tint::Teal,
        }
    }
}

/// The name a placed block goes by: its category, plus A/B where the
/// category has two instances.
pub(crate) fn block_name(kind: StageKind) -> &'static str {
    match kind {
        StageKind::Gate => "Gate",
        StageKind::Comp => "Comp A",
        StageKind::Comp2 => "Comp B",
        StageKind::Wah => "Wah",
        StageKind::Drive => "Drive A",
        StageKind::Drive2 => "Drive B",
        StageKind::Amp => "Amp",
        StageKind::Cab => "Cab",
        StageKind::Eq => "EQ A",
        StageKind::Eq2 => "EQ B",
        StageKind::Mod => "Mod A",
        StageKind::Mod2 => "Mod B",
        StageKind::Delay => "Delay A",
        StageKind::Delay2 => "Delay B",
        StageKind::Reverb => "Reverb",
    }
}

/// The wire id of a block's own on/off flag.
pub(crate) fn enable_id(kind: StageKind) -> &'static str {
    match kind {
        StageKind::Gate => "gate_on",
        StageKind::Comp => "comp_on",
        StageKind::Comp2 => "comp2_on",
        StageKind::Wah => "wah_on",
        StageKind::Drive => "drive_on",
        StageKind::Drive2 => "drive2_on",
        StageKind::Amp => "amp_on",
        StageKind::Cab => "cab_on",
        StageKind::Eq => "eq_on",
        StageKind::Eq2 => "eq2_on",
        StageKind::Mod => "mod_on",
        StageKind::Mod2 => "mod2_on",
        StageKind::Delay => "delay_on",
        StageKind::Delay2 => "delay2_on",
        StageKind::Reverb => "reverb_on",
    }
}

pub(crate) fn is_on(p: &Params, kind: StageKind) -> bool {
    let b = &p.stage_b;
    match kind {
        StageKind::Gate => p.gate_on,
        StageKind::Comp => p.comp_on,
        StageKind::Comp2 => b.comp_on,
        StageKind::Wah => p.wah_on,
        StageKind::Drive => p.drive_on,
        StageKind::Drive2 => b.drive_on,
        StageKind::Amp => p.amp_on,
        StageKind::Cab => p.cab_on,
        StageKind::Eq => p.eq_on,
        StageKind::Eq2 => b.eq_on,
        StageKind::Mod => p.mod_on,
        StageKind::Mod2 => b.mod_on,
        StageKind::Delay => p.delay_on,
        StageKind::Delay2 => b.delay_on,
        StageKind::Reverb => p.reverb_on,
    }
}

/// One entry in a block's model list: what it is called, and the wire edit
/// that selects it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ModelChoice {
    pub label: &'static str,
    pub wire_id: &'static str,
    pub value: f32,
}

fn choices<T: Copy>(
    all: &[T],
    wire_id: &'static str,
    label: fn(T) -> &'static str,
) -> Vec<ModelChoice> {
    all.iter()
        .enumerate()
        .map(|(index, model)| ModelChoice {
            label: label(*model),
            wire_id,
            value: index as f32,
        })
        .collect()
}

fn position<T: PartialEq>(all: &[T], current: &T) -> Option<usize> {
    all.iter().position(|model| model == current)
}

/// A block's model list and the index of the selected one. Gate and
/// compressor have a single algorithm, so a single fixed entry.
pub(crate) fn models(p: &Params, kind: StageKind) -> (Vec<ModelChoice>, Option<usize>) {
    let b = &p.stage_b;
    let fixed = |label| {
        (
            vec![ModelChoice {
                label,
                wire_id: "",
                value: 0.0,
            }],
            Some(0),
        )
    };
    match kind {
        StageKind::Gate => fixed("Noise Gate"),
        StageKind::Comp | StageKind::Comp2 => fixed("Studio Compressor"),
        StageKind::Wah => (
            choices(WahModel::ALL, "wah_model", wah_model_label),
            position(WahModel::ALL, &p.wah_model),
        ),
        StageKind::Drive => (
            choices(DriveModel::ALL, "drive_model", drive_model_label),
            position(DriveModel::ALL, &p.drive_model),
        ),
        StageKind::Drive2 => (
            choices(DriveModel::ALL, "drive2_model", drive_model_label),
            position(DriveModel::ALL, &b.drive_model),
        ),
        StageKind::Amp => {
            // Classic amp voicings, then the NAM capture engine: selecting
            // an amp model puts the engine back on Classic (the DSP does
            // that for `amp_model`), the last entry switches to NAM.
            let mut list = choices(AmpModel::ALL, "amp_model", amp_model_label);
            list.push(ModelChoice {
                label: "NAM Capture",
                wire_id: "tone_engine",
                value: ToneEngineKind::NamCapture.index() as f32,
            });
            let selected = match p.tone_engine {
                ToneEngineKind::Classic => position(AmpModel::ALL, &p.amp_model),
                ToneEngineKind::NamCapture => Some(list.len() - 1),
                ToneEngineKind::Bypass => None,
            };
            (list, selected)
        }
        StageKind::Cab => (
            choices(CabModel::ALL, "cab_model", cab_model_label),
            position(CabModel::ALL, &p.cab_model),
        ),
        StageKind::Eq => (
            choices(EqModel::ALL, "eq_model", eq_model_label),
            position(EqModel::ALL, &p.eq_model),
        ),
        StageKind::Eq2 => (
            choices(EqModel::ALL, "eq2_model", eq_model_label),
            position(EqModel::ALL, &b.eq_model),
        ),
        StageKind::Mod => (
            choices(ModModel::ALL, "mod_model", mod_model_label),
            position(ModModel::ALL, &p.mod_model),
        ),
        StageKind::Mod2 => (
            choices(ModModel::ALL, "mod2_model", mod_model_label),
            position(ModModel::ALL, &b.mod_model),
        ),
        StageKind::Delay => (
            choices(DelayModel::ALL, "delay_model", delay_model_label),
            position(DelayModel::ALL, &p.delay_model),
        ),
        StageKind::Delay2 => (
            choices(DelayModel::ALL, "delay2_model", delay_model_label),
            position(DelayModel::ALL, &b.delay_model),
        ),
        StageKind::Reverb => (
            choices(ReverbModel::ALL, "reverb_model", reverb_model_label),
            position(ReverbModel::ALL, &p.reverb_model),
        ),
    }
}

/// The selected model's name, or `None` (amp with its engine bypassed).
pub(crate) fn model_name(p: &Params, kind: StageKind) -> Option<&'static str> {
    let (list, selected) = models(p, kind);
    selected.and_then(|index| list.get(index)).map(|m| m.label)
}

pub(crate) fn mic_choices() -> Vec<ModelChoice> {
    choices(MicModel::ALL, "cab_mic_type", mic_model_label)
}

pub(crate) fn mic_index(p: &Params) -> Option<usize> {
    position(MicModel::ALL, &p.mic_model)
}

/// One slider row.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ParamSpec {
    /// The wire id the row reads and writes.
    pub id: &'static str,
    pub label: &'static str,
    pub min: f32,
    pub max: f32,
    pub unit: &'static str,
    /// Shown but inert for the selected model (e.g. Sensitivity on a Cry
    /// Wah) — dimmed, never hidden, so the layout does not jump.
    pub inactive: bool,
}

/// `(wire id, descriptor id, label)` — the descriptor id is the first
/// instance's, which a second instance shares its range with.
type Row = (&'static str, &'static str, &'static str);

fn rows(kind: StageKind) -> &'static [Row] {
    match kind {
        StageKind::Gate => &[("gate_thresh", "gate_thresh", "Threshold")],
        StageKind::Comp => &[
            ("comp_thresh", "comp_thresh", "Threshold"),
            ("comp_ratio", "comp_ratio", "Ratio"),
            ("comp_attack", "comp_attack", "Attack"),
            ("comp_release", "comp_release", "Release"),
            ("comp_makeup", "comp_makeup", "Makeup"),
        ],
        StageKind::Comp2 => &[
            ("comp2_thresh", "comp_thresh", "Threshold"),
            ("comp2_ratio", "comp_ratio", "Ratio"),
            ("comp2_attack", "comp_attack", "Attack"),
            ("comp2_release", "comp_release", "Release"),
            ("comp2_makeup", "comp_makeup", "Makeup"),
        ],
        StageKind::Wah => &[
            ("wah_pos", "wah_pos", "Position"),
            ("wah_res", "wah_res", "Resonance"),
            ("wah_sens", "wah_sens", "Sensitivity"),
        ],
        StageKind::Drive => &[
            ("drive_gain", "drive_gain", "Drive"),
            ("drive_tone", "drive_tone", "Tone"),
            ("drive_level", "drive_level", "Level"),
        ],
        StageKind::Drive2 => &[
            ("drive2_gain", "drive_gain", "Drive"),
            ("drive2_tone", "drive_tone", "Tone"),
            ("drive2_level", "drive_level", "Level"),
        ],
        StageKind::Amp => &[
            ("amp_gain", "amp_gain", "Drive"),
            ("amp_bass", "amp_bass", "Bass"),
            ("amp_middle", "amp_middle", "Mid"),
            ("amp_treble", "amp_treble", "Treble"),
            ("amp_presence", "amp_presence", "Presence"),
            ("amp_master", "amp_master", "Master"),
        ],
        StageKind::Cab => &[
            ("cab_mic", "cab_mic", "Mic Position"),
            ("cab_dist", "cab_dist", "Distance"),
        ],
        StageKind::Eq => &[
            ("eq_low_gain", "eq_low_gain", "Low"),
            ("eq_mid1_freq", "eq_mid1_freq", "Low Mid Freq"),
            ("eq_mid1_gain", "eq_mid1_gain", "Low Mid"),
            ("eq_mid2_freq", "eq_mid2_freq", "High Mid Freq"),
            ("eq_mid2_gain", "eq_mid2_gain", "High Mid"),
            ("eq_high_gain", "eq_high_gain", "High"),
        ],
        StageKind::Eq2 => &[
            ("eq2_low_gain", "eq_low_gain", "Low"),
            ("eq2_mid1_freq", "eq_mid1_freq", "Low Mid Freq"),
            ("eq2_mid1_gain", "eq_mid1_gain", "Low Mid"),
            ("eq2_mid2_freq", "eq_mid2_freq", "High Mid Freq"),
            ("eq2_mid2_gain", "eq_mid2_gain", "High Mid"),
            ("eq2_high_gain", "eq_high_gain", "High"),
        ],
        StageKind::Mod => &[
            ("chorus_rate", "chorus_rate", "Rate"),
            ("chorus_depth", "chorus_depth", "Depth"),
            ("chorus_mix", "chorus_mix", "Mix"),
        ],
        StageKind::Mod2 => &[
            ("chorus2_rate", "chorus_rate", "Rate"),
            ("chorus2_depth", "chorus_depth", "Depth"),
            ("chorus2_mix", "chorus_mix", "Mix"),
        ],
        StageKind::Delay => &[
            ("delay_time", "delay_time", "Time"),
            ("delay_fb", "delay_fb", "Feedback"),
            ("delay_tone", "delay_tone", "Tone"),
            ("delay_mix", "delay_mix", "Mix"),
        ],
        StageKind::Delay2 => &[
            ("delay2_time", "delay_time", "Time"),
            ("delay2_fb", "delay_fb", "Feedback"),
            ("delay2_tone", "delay_tone", "Tone"),
            ("delay2_mix", "delay_mix", "Mix"),
        ],
        StageKind::Reverb => &[
            ("reverb_decay", "reverb_decay", "Decay"),
            ("reverb_mix", "reverb_mix", "Mix"),
            ("reverb_shimmer", "reverb_shimmer", "Shimmer"),
        ],
    }
}

/// The NAM capture rows: the plug-in's descriptor does not list them, so
/// their ranges are the DSP's (trim ±24 dB, mix and slim size in percent).
const NAM_ROWS: [(&str, &str, f32, f32, &str); 4] = [
    ("nam_input_trim", "Capture In", -24.0, 24.0, "dB"),
    ("nam_output_trim", "Capture Out", -24.0, 24.0, "dB"),
    ("nam_mix", "Capture Mix", 0.0, 100.0, "%"),
    ("nam_slim_size", "Slim Size", 0.0, 100.0, "%"),
];

/// Global trims, shown in the preset bar.
pub(crate) fn trim_spec(id: &'static str, label: &'static str) -> Option<ParamSpec> {
    describe(id, id, label, false)
}

fn describe(
    id: &'static str,
    descriptor_id: &str,
    label: &'static str,
    inactive: bool,
) -> Option<ParamSpec> {
    let d = rodharerist::descriptor()
        .params
        .iter()
        .find(|d| d.id == descriptor_id)?;
    Some(ParamSpec {
        id,
        label,
        min: d.min,
        max: d.max,
        unit: d.unit,
        inactive,
    })
}

/// The slider rows for a block, in order.
pub(crate) fn params(p: &Params, kind: StageKind) -> Vec<ParamSpec> {
    let inactive = |id: &str| match id {
        "wah_sens" => p.wah_model != WahModel::TouchWah,
        "reverb_shimmer" => p.reverb_model != ReverbModel::Shimmer,
        "cab_mic" | "cab_dist" => p.cab_model == CabModel::Ir,
        _ => false,
    };
    let mut out: Vec<ParamSpec> = rows(kind)
        .iter()
        .filter_map(|(id, descriptor_id, label)| describe(id, descriptor_id, label, inactive(id)))
        .collect();
    if kind == StageKind::Amp && p.tone_engine == ToneEngineKind::NamCapture {
        out.extend(
            NAM_ROWS
                .iter()
                .map(|(id, label, min, max, unit)| ParamSpec {
                    id,
                    label,
                    min: *min,
                    max: *max,
                    unit,
                    inactive: false,
                }),
        );
    }
    out
}

/// A value as the editor prints it, with its unit.
pub(crate) fn format_value(value: f32, spec: &ParamSpec) -> String {
    match spec.unit {
        "dB" if spec.min < 0.0 && spec.max > 0.0 => format!("{value:+.1} dB"),
        "dB" => format!("{value:.1} dB"),
        "%" => format!("{value:.0} %"),
        "ms" if value < 10.0 => format!("{value:.1} ms"),
        "ms" => format!("{value:.0} ms"),
        "s" => format!("{value:.1} s"),
        "Hz" if value >= 1000.0 => format!("{:.2} kHz", value / 1000.0),
        "Hz" => format!("{value:.0} Hz"),
        ":1" => format!("{value:.1}:1"),
        _ => format!("{value:.1}"),
    }
}

pub(crate) fn drive_model_label(m: DriveModel) -> &'static str {
    use DriveModel::*;
    match m {
        Screamer => "Green Screamer",
        Minotaur => "Minotaur Boost",
        Rat => "Rats Nest",
        Breaker => "Breaker Blues",
        Fuzz => "Face Fuzz",
        Centurion => "Centurion",
        DsOne => "DS Classic",
        SuperDrive => "Super Drive",
        MetalCore => "Metal Core",
        TightRift => "Tight Rift",
        AmberCrunch => "Amber Crunch",
        CopperFuzz => "Copper Fuzz",
    }
}

pub(crate) fn amp_model_label(m: AmpModel) -> &'static str {
    use AmpModel::*;
    match m {
        Mandarin => "Mandarin 80",
        Plexi => "Brit Plexi 100",
        Twin => "Twin Clean",
        TopBoost => "Top Boost",
        Recto => "Recto Modern",
        Jcm => "JCM Crunch",
        Slate => "Lead Slate",
        Bassman => "Bassman",
        Boutique => "Overdrive Special",
        Invader => "Invader 5150",
        TweedCombo => "Tweed Deluxe",
    }
}

pub(crate) fn cab_model_label(model: CabModel) -> &'static str {
    match model {
        CabModel::Vintage4x12 => "1960v Vintage 4x12",
        CabModel::American2x12 => "American 2x12",
        CabModel::Tweed1x12 => "Tweed 1x12",
        CabModel::Modern4x12 => "Modern 4x12",
        CabModel::OpenBack => "Open Back",
        CabModel::Vintage2x12 => "Vintage 2x12",
        CabModel::Oversized4x12 => "Oversized 4x12",
        CabModel::BassCabinet => "Bass Cabinet",
        CabModel::Brit4x12 => "British Stack 4x12",
        CabModel::Uber4x12 => "Uberkab 4x12",
        CabModel::Slo4x12 => "SLO Custom 4x12",
        CabModel::Ir => "Impulse Response",
        CabModel::Modern2x12 => "Modern 2x12",
        CabModel::American1x12 => "American 1x12 Combo",
    }
}

pub(crate) fn mic_model_label(model: MicModel) -> &'static str {
    match model {
        MicModel::Dynamic => "Dynamic",
        MicModel::Ribbon => "Ribbon",
        MicModel::Condenser => "Condenser",
    }
}

pub(crate) fn mod_model_label(model: ModModel) -> &'static str {
    match model {
        ModModel::Chorus => "70s Analog Chorus",
        ModModel::Phaser => "Vibe Phase 90",
        ModModel::Flanger => "Jet Flanger",
        ModModel::Tremolo => "Opto Tremolo",
        ModModel::MolamSwirl => "Molam Swirl",
        ModModel::PhinVibe => "Phin Vibe",
        ModModel::KhaenSwirl => "Khaen Swirl",
        ModModel::BiLam => "Bi-Lam",
        ModModel::IsanJet => "Isan Jet",
        ModModel::SoftPhase => "Soft Phase",
        ModModel::WideVibe => "Wide Vibe",
    }
}

pub(crate) fn wah_model_label(model: WahModel) -> &'static str {
    match model {
        WahModel::CryWah => "Cry Wah",
        WahModel::TouchWah => "Touch Wah",
    }
}

pub(crate) fn delay_model_label(model: DelayModel) -> &'static str {
    match model {
        DelayModel::Tape => "Tape Echo",
        DelayModel::Digital => "Digital Delay",
        DelayModel::Analog => "Analog Delay",
        DelayModel::PingPong => "Ping-Pong",
        DelayModel::Dual => "Dual Delay",
    }
}

pub(crate) fn reverb_model_label(model: ReverbModel) -> &'static str {
    match model {
        ReverbModel::Plate => "Studio Plate",
        ReverbModel::Room => "Tracking Room",
        ReverbModel::Hall => "Concert Hall",
        ReverbModel::Shimmer => "Shimmer",
    }
}

pub(crate) fn eq_model_label(model: EqModel) -> &'static str {
    match model {
        EqModel::Studio => "Studio EQ",
        EqModel::Vintage => "Vintage EQ",
        EqModel::Modern => "Modern EQ",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_stage_has_a_category_that_lists_it() {
        for kind in StageKind::ALL {
            assert!(Category::of(*kind).kinds().contains(kind), "{kind:?}");
        }
        let listed: usize = Category::ALL.iter().map(|c| c.kinds().len()).sum();
        assert_eq!(listed, StageKind::COUNT);
    }

    #[test]
    fn every_row_and_switch_is_a_routed_wire_id() {
        let mut p = rodharerist::default_params();
        p.tone_engine = ToneEngineKind::NamCapture;
        for kind in StageKind::ALL {
            let specs = params(&p, *kind);
            assert_eq!(
                specs.len(),
                rows(*kind).len() + if *kind == StageKind::Amp { 4 } else { 0 }
            );
            for spec in specs {
                assert!(spec.min < spec.max, "{}", spec.id);
                assert!(
                    rodharerist::apply_to_params(&mut p.clone(), spec.id, spec.min),
                    "{}",
                    spec.id
                );
            }
            assert!(rodharerist::apply_to_params(
                &mut p.clone(),
                enable_id(*kind),
                1.0
            ));
            for choice in models(&p, *kind).0 {
                if !choice.wire_id.is_empty() {
                    assert!(rodharerist::apply_to_params(
                        &mut p.clone(),
                        choice.wire_id,
                        choice.value
                    ));
                }
            }
        }
    }

    #[test]
    fn a_model_choice_selects_that_model() {
        let mut p = rodharerist::default_params();
        let (list, _) = models(&p, StageKind::Drive2);
        let target = list[3];
        rodharerist::apply_to_params(&mut p, target.wire_id, target.value);
        assert_eq!(models(&p, StageKind::Drive2).1, Some(3));

        let (list, _) = models(&p, StageKind::Amp);
        let nam = *list.last().unwrap();
        rodharerist::apply_to_params(&mut p, nam.wire_id, nam.value);
        assert_eq!(models(&p, StageKind::Amp).1, Some(list.len() - 1));
        rodharerist::apply_to_params(&mut p, list[1].wire_id, list[1].value);
        assert_eq!(p.tone_engine, ToneEngineKind::Classic);
        assert_eq!(models(&p, StageKind::Amp).1, Some(1));
    }

    #[test]
    fn values_print_with_their_units() {
        let spec = |unit, min, max| ParamSpec {
            id: "x",
            label: "x",
            min,
            max,
            unit,
            inactive: false,
        };
        assert_eq!(format_value(-3.0, &spec("dB", -15.0, 15.0)), "-3.0 dB");
        assert_eq!(format_value(2500.0, &spec("Hz", 600.0, 6000.0)), "2.50 kHz");
        assert_eq!(format_value(4.0, &spec(":1", 1.0, 20.0)), "4.0:1");
    }
}

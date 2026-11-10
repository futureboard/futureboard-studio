//! Host parameters: one per Rodhareist wire id.
//!
//! Generated from the core's own tables rather than written out field by
//! field: `rodharerist::UI_PARAM_IDS` names every parameter (and the order the
//! DSP needs them replayed in), `descriptor()` and the editor's block model
//! give ranges, units and labels, and `ui_values(&default_params())` gives
//! defaults. The host-facing id of each parameter is its wire id, so
//! automation and saved sessions use the names Studio's projects use.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use nih_plug::params::persist::PersistentField;
use nih_plug::prelude::*;
use nih_plug_gpui::EditorContext;
// Not `rodharerist::Params`: an explicit import of it would shadow the
// prelude's `Params` trait that `RodhareistParams` derives and `WireParams`
// implements. The core struct is spelled `CoreParams` here.
use rodharerist::{
    AmpModel, CabModel, CompModel, DelayModel, DelaySync, DriveModel, EqModel, MicModel, ModModel,
    Params as CoreParams, PitchModel, ReverbModel, StageChannel, StageKind, ToneEngineKind,
    WahModel, PATH_SLOTS, UI_PARAM_IDS,
};

use crate::components::rodhareist_blocks::{self as blocks, ParamSpec};

/// The wire id that is an action rather than state; never a host parameter.
pub const CLEAR_CLIP: &str = "clear_clip";

#[derive(Params)]
pub struct RodhareistParams {
    /// The `.nam` capture the amp's NAM engine runs: a file name in
    /// `Documents/Futureboard Studio/Rodhareist/NAMs`, as Studio saves it.
    #[persist = "nam-file"]
    pub nam_file: FileName,

    /// The `.wav` impulse response the cabinet convolves with: a file name in
    /// `Documents/Futureboard Studio/Rodhareist/IRs`.
    #[persist = "ir-file"]
    pub ir_file: FileName,

    #[nested]
    pub wire: WireParams,
}

/// A saved file name, and how many times a state load has written it.
///
/// nih-plug runs `initialize` both for a state load and for a plain host
/// re-activation. Only a state load goes through [`PersistentField::set`],
/// so the revision tells the two apart. The plug-in's own writes (a load
/// that succeeded) leave it alone.
#[derive(Default)]
pub struct FileName {
    name: RwLock<Option<String>>,
    revision: AtomicU64,
}

impl FileName {
    pub fn get(&self) -> Option<String> {
        self.name.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn put(&self, name: Option<String>) {
        *self.name.write().unwrap_or_else(|e| e.into_inner()) = name;
    }

    /// Bumped by every state load.
    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }
}

impl<'a> PersistentField<'a, Option<String>> for FileName {
    fn set(&self, new_value: Option<String>) {
        self.put(new_value);
        self.revision.fetch_add(1, Ordering::AcqRel);
    }

    fn map<F, R>(&self, f: F) -> R
    where
        F: Fn(&Option<String>) -> R,
    {
        f(&self.name.read().unwrap_or_else(|e| e.into_inner()))
    }
}

impl Default for RodhareistParams {
    fn default() -> Self {
        Self {
            nam_file: FileName::default(),
            ir_file: FileName::default(),
            wire: WireParams::new(),
        }
    }
}

impl RodhareistParams {
    pub fn nam_file(&self) -> Option<String> {
        self.nam_file.get()
    }

    pub fn ir_file(&self) -> Option<String> {
        self.ir_file.get()
    }

    pub fn set_nam_file(&self, name: Option<String>) {
        self.nam_file.put(name);
    }

    pub fn set_ir_file(&self, name: Option<String>) {
        self.ir_file.put(name);
    }

    /// The core params the host values describe, with the files they name:
    /// what the editor shows. Not for the audio thread (it clones strings).
    pub fn to_core_params_with_files(&self) -> rodharerist::Params {
        let mut params = self.wire.to_core_params();
        params.nam_file = self.nam_file();
        params.ir_file = self.ir_file();
        params
    }

    /// An editor edit by wire index: one complete gesture, so the host
    /// records it. `clear_clip` is an action and has no parameter; the caller
    /// handles it.
    pub fn set_from_editor(&self, context: &EditorContext, index: u32, value: f32) {
        let Some(id) = rodharerist::ui_param_id(index) else {
            return;
        };
        let Some(entry) = self.wire.get(id) else {
            return;
        };
        let setter = context.setter();
        entry.param.begin(&setter);
        entry.param.set_plain(&setter, value);
        entry.param.end(&setter);
        // The title a host shows for MIDI learn follows the selected stomp.
        // This runs on the editor thread; the audio thread must not rename.
        if self.wire.refresh_host_names() {
            context.gui_context().param_titles_changed();
        }
    }
}

/// What a wire parameter is, as far as the processor cares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    /// Routed to the DSP with `Dsp::apply_ui_param`.
    Value,
    /// One slot of the signal path; applied as a whole order with
    /// `Dsp::set_path_order`.
    PathSlot(usize),
}

pub enum WireParam {
    Float(FloatParam),
    /// An integer and the highest value the core takes from it: a model
    /// catalogue keeps a fixed host range ([`MODEL_SLOTS`]) wider than its
    /// list, and its unused top steps select the last model.
    Int(IntParam, i32),
    Bool(BoolParam),
}

pub struct WireEntry {
    /// Wire id, shared with the DSP and Studio's project format.
    pub id: &'static str,
    pub kind: EntryKind,
    pub param: WireParam,
    /// How the host title is made, worked out once.
    title: Title,
}

/// The shape of a host title.
enum Title {
    /// Never changes (global trims, lanes, the signal path).
    Fixed,
    /// `"{stomp} {label}"`: follows the model selected in `kind`'s block.
    Stomp { kind: StageKind, label: String },
}

/// Every wire parameter, in `UI_PARAM_IDS` order (minus `clear_clip`).
///
/// Built once and never resized: the host holds raw pointers into it (see
/// [`Params::param_map`]), so the entries must not move.
pub struct WireParams {
    entries: Box<[WireEntry]>,
    /// Entries that select a model: the only values a title depends on.
    title_sources: Box<[usize]>,
    /// `title_sources`' values the current titles were made from.
    titled_from: Mutex<Vec<f32>>,
}

impl WireParams {
    fn new() -> Self {
        let defaults = rodharerist::ui_values(&rodharerist::default_params());
        let specs = block_specs();
        let entries: Box<[WireEntry]> = UI_PARAM_IDS
            .iter()
            .filter(|id| **id != CLEAR_CLIP)
            .map(|&id| {
                let default = defaults
                    .iter()
                    .find(|(d, _)| *d == id)
                    .map(|(_, v)| *v)
                    .unwrap_or_else(|| panic!("no default for wire id `{id}`"));
                build_entry(id, default, &specs)
            })
            .collect();
        let selectors = model_selectors();
        let title_sources: Box<[usize]> = entries
            .iter()
            .enumerate()
            .filter(|(_, e)| selectors.contains(&e.id))
            .map(|(i, _)| i)
            .collect();
        // The entries were named from these (default) values.
        let titled_from = title_sources
            .iter()
            .map(|&i| entries[i].param.plain())
            .collect();
        Self {
            entries,
            title_sources,
            titled_from: Mutex::new(titled_from),
        }
    }

    pub fn entries(&self) -> &[WireEntry] {
        &self.entries
    }

    /// Position of `id` in [`Self::entries`]. Linear scan: setup and GUI only.
    pub fn index_of(&self, id: &str) -> Option<usize> {
        self.entries.iter().position(|e| e.id == id)
    }

    pub fn get(&self, id: &str) -> Option<&WireEntry> {
        self.entries.iter().find(|e| e.id == id)
    }

    /// Every value in entry order, for the editor to see what moved.
    pub fn values(&self) -> Vec<f32> {
        self.entries.iter().map(|e| e.param.plain()).collect()
    }

    /// The signal path the `path_slot_*` values describe. Not for the audio
    /// thread (it scans every entry).
    pub fn path_order(&self) -> [Option<StageKind>; PATH_SLOTS] {
        let mut order = [None; PATH_SLOTS];
        for entry in self.entries.iter() {
            if let EntryKind::PathSlot(slot) = entry.kind {
                order[slot] = StageKind::from_index(entry.param.plain().round() as i32);
            }
        }
        order
    }

    /// The core parameter set these host values describe: the defaults with
    /// every wire value replayed over them in table order, the property
    /// `rodharerist`'s own state restore relies on.
    pub fn to_core_params(&self) -> rodharerist::Params {
        let mut p = rodharerist::default_params();
        for e in self.entries.iter() {
            rodharerist::apply_to_params(&mut p, e.id, e.param.plain());
        }
        p
    }

    /// Republish each parameter title from the live stomp names. Returns
    /// whether any title changed; the caller then tells the host
    /// (`param_titles_changed`), which reads them again.
    ///
    /// Only a model selector changes a title, so this is a comparison of a
    /// few values unless one of them moved. The VST3 unit id is assigned once,
    /// when the parameter map is built, and stays the block category. Call
    /// this from the editor or from `initialize`, never from the audio
    /// thread: it locks, and a rename allocates.
    pub fn refresh_host_names(&self) -> bool {
        let mut titled_from = self.titled_from.lock().unwrap_or_else(|e| e.into_inner());
        let unchanged = self
            .title_sources
            .iter()
            .zip(titled_from.iter())
            .all(|(&i, &value)| self.entries[i].param.plain() == value);
        if unchanged {
            return false;
        }
        for (slot, &i) in titled_from.iter_mut().zip(self.title_sources.iter()) {
            *slot = self.entries[i].param.plain();
        }

        let params = self.to_core_params();
        let stomps: Vec<String> = StageKind::ALL
            .iter()
            .map(|&kind| stomp_title(&params, kind))
            .collect();
        let mut changed = false;
        for entry in self.entries.iter() {
            if let Title::Stomp { kind, label } = &entry.title {
                let name = format!("{} {label}", stomps[kind.index() as usize]);
                changed |= entry.param.set_display_name(&name);
            }
        }
        changed
    }
}

/// Wire ids that select a block's model (and the amp's engine): what
/// [`stomp_title`] reads.
fn model_selectors() -> Vec<&'static str> {
    let defaults = rodharerist::default_params();
    let mut ids: Vec<&'static str> = Vec::new();
    for &kind in StageKind::ALL {
        for choice in blocks::models(&defaults, kind).0 {
            if !choice.wire_id.is_empty() && !ids.contains(&choice.wire_id) {
                ids.push(choice.wire_id);
            }
        }
    }
    ids
}

unsafe impl Params for WireParams {
    fn param_map(&self) -> Vec<(String, ParamPtr, String)> {
        self.entries
            .iter()
            .map(|e| (e.id.to_string(), e.param.ptr(), describe(e.id).0))
            .collect()
    }
}

impl WireParam {
    /// The current plain value in the core's units (bools as 0/1, choices as
    /// their index). Lock- and allocation-free.
    #[inline]
    pub fn plain(&self) -> f32 {
        match self {
            WireParam::Float(p) => p.value(),
            WireParam::Int(p, last) => p.value().min(*last) as f32,
            WireParam::Bool(p) => f32::from(u8::from(p.value())),
        }
    }

    pub fn ptr(&self) -> ParamPtr {
        match self {
            WireParam::Float(p) => p.as_ptr(),
            WireParam::Int(p, _) => p.as_ptr(),
            WireParam::Bool(p) => p.as_ptr(),
        }
    }

    fn begin(&self, setter: &ParamSetter) {
        match self {
            WireParam::Float(p) => setter.begin_set_parameter(p),
            WireParam::Int(p, _) => setter.begin_set_parameter(p),
            WireParam::Bool(p) => setter.begin_set_parameter(p),
        }
    }

    fn set_plain(&self, setter: &ParamSetter, value: f32) {
        match self {
            WireParam::Float(p) => setter.set_parameter(p, value),
            WireParam::Int(p, _) => setter.set_parameter(p, value.round() as i32),
            WireParam::Bool(p) => setter.set_parameter(p, value >= 0.5),
        }
    }

    fn end(&self, setter: &ParamSetter) {
        match self {
            WireParam::Float(p) => setter.end_set_parameter(p),
            WireParam::Int(p, _) => setter.end_set_parameter(p),
            WireParam::Bool(p) => setter.end_set_parameter(p),
        }
    }

    fn display_name(&self) -> &str {
        match self {
            WireParam::Float(p) => p.name(),
            WireParam::Int(p, _) => p.name(),
            WireParam::Bool(p) => p.name(),
        }
    }

    /// Replace the host-visible title. Returns whether it changed.
    fn set_display_name(&self, name: &str) -> bool {
        if self.display_name() == name {
            return false;
        }
        match self {
            WireParam::Float(p) => p.set_name(name),
            WireParam::Int(p, _) => p.set_name(name),
            WireParam::Bool(p) => p.set_name(name),
        }
        true
    }
}

fn is_toggle(id: &str) -> bool {
    id == "power" || id == "nam_loudness_norm" || id.ends_with("_on")
}

fn path_slot(id: &str) -> Option<usize> {
    id.strip_prefix("path_slot_")?
        .parse()
        .ok()
        .filter(|slot| *slot < PATH_SLOTS)
}

fn labels<T: Copy>(all: &[T], label: fn(T) -> &'static str) -> Vec<&'static str> {
    all.iter().map(|m| label(*m)).collect()
}

/// `ToneEngineKind` in wire order (the core has no `ALL` for it).
const TONE_ENGINES: &[ToneEngineKind] = &[
    ToneEngineKind::Classic,
    ToneEngineKind::NamCapture,
    ToneEngineKind::Bypass,
];

fn sync_label(sync: DelaySync) -> &'static str {
    match sync {
        DelaySync::Free => "Free",
        DelaySync::Quarter => "1/4",
        DelaySync::DottedEighth => "1/8.",
        DelaySync::Eighth => "1/8",
        DelaySync::Half => "1/2",
        DelaySync::DottedQuarter => "1/4.",
        DelaySync::QuarterTriplet => "1/4t",
        DelaySync::EighthTriplet => "1/8t",
        DelaySync::Sixteenth => "1/16",
        DelaySync::DottedSixteenth => "1/16.",
    }
}

fn tone_engine_label(engine: ToneEngineKind) -> &'static str {
    match engine {
        ToneEngineKind::Classic => "Amp model",
        ToneEngineKind::NamCapture => "NAM capture",
        ToneEngineKind::Bypass => "Off",
    }
}

/// Host steps every model catalogue is exposed with, whatever its length
/// today. Hosts store automation and MIDI learn as normalized values, so a
/// range that grew with each new model moved what those values select
/// (#342). New models take the next free step; the unused ones above the
/// last model select it. Never shrink this: it would shift every stored value.
pub const MODEL_SLOTS: i32 = 64;

/// Whether `id` selects from a model catalogue (one that gains entries
/// between releases), as opposed to a fixed list of modes.
fn is_model_catalogue(id: &str) -> bool {
    id.ends_with("_model") || id == "cab_mic_type"
}

/// The display names of a select's indices, if `id` is one.
pub fn choice_labels(id: &str) -> Option<Vec<&'static str>> {
    if id.ends_with("_row") {
        return Some(vec!["Lane 1", "Lane 2", "Lane 3", "Lane 4"]);
    }
    if id.ends_with("_channel") {
        return Some(labels(StageChannel::ALL, blocks::channel_label));
    }
    if id.starts_with("row_send_") {
        return Some(vec!["Off", "Serial", "Parallel", "Wet/Dry/Wet"]);
    }
    Some(match id {
        "drive_model" | "drive2_model" => labels(DriveModel::ALL, blocks::drive_model_label),
        "comp_model" | "comp2_model" => labels(CompModel::ALL, blocks::comp_model_label),
        "amp_model" => labels(AmpModel::ALL, blocks::amp_model_label),
        "cab_model" => labels(CabModel::ALL, blocks::cab_model_label),
        "cab_mic_type" => labels(MicModel::ALL, blocks::mic_model_label),
        "mod_model" | "mod2_model" => labels(ModModel::ALL, blocks::mod_model_label),
        "wah_model" => labels(WahModel::ALL, blocks::wah_model_label),
        "eq_model" | "eq2_model" => labels(EqModel::ALL, blocks::eq_model_label),
        "reverb_model" => labels(ReverbModel::ALL, blocks::reverb_model_label),
        "delay_model" | "delay2_model" => labels(DelayModel::ALL, blocks::delay_model_label),
        "pitch_model" => labels(PitchModel::ALL, blocks::pitch_model_label),
        "delay_sync" | "delay_sync_r" | "delay2_sync" | "delay2_sync_r" => {
            labels(DelaySync::ALL, sync_label)
        }
        "tone_engine" => labels(TONE_ENGINES, tone_engine_label),
        "line2_mode" => vec!["Wet", "Dry", "Wet/Dry/Wet"],
        _ => return None,
    })
}

/// Every block knob the editor can show, from the block model over the
/// variants that add rows (the NAM capture's trims, the Stereo delays'
/// right side), keyed by `(block, spec)`.
fn block_specs() -> Vec<(StageKind, ParamSpec)> {
    let base = rodharerist::default_params();
    let mut nam = base.clone();
    nam.tone_engine = ToneEngineKind::NamCapture;
    let mut stereo = base.clone();
    stereo.delay_model = DelayModel::Stereo;
    stereo.stage_b.delay_model = DelayModel::Stereo;
    let mut specs: Vec<(StageKind, ParamSpec)> = Vec::new();
    for p in [&base, &nam, &stereo] {
        for &kind in StageKind::ALL {
            for spec in blocks::params(p, kind) {
                if !specs.iter().any(|(_, known)| known.id == spec.id) {
                    specs.push((kind, spec));
                }
            }
        }
    }
    specs
}

/// The spec of a continuous parameter: the block model's, else a global trim,
/// else the descriptor's (a second instance falls back to its first's).
fn spec_of(id: &'static str, specs: &[(StageKind, ParamSpec)]) -> Option<ParamSpec> {
    if let Some((_, spec)) = specs.iter().find(|(_, spec)| spec.id == id) {
        return Some(*spec);
    }
    if let Some(spec) = blocks::trim_spec(id, "") {
        return Some(spec);
    }
    let descriptor = rodharerist::descriptor();
    let find = |key: &str| {
        descriptor
            .params
            .iter()
            .find(|d| d.id == key)
            .map(|d| (d.min, d.max, d.unit))
    };
    let (min, max, unit) = find(id).or_else(|| find(&id.replacen("2_", "_", 1)))?;
    Some(ParamSpec {
        id,
        label: "",
        min,
        max,
        unit,
        inactive: false,
    })
}

/// `(group, label)` a host shows for a wire id. The group is the block name,
/// so a host's parameter list reads like the editor.
pub fn describe(id: &str) -> (String, String) {
    let global = |label: &str| ("Global".to_string(), label.to_string());
    match id {
        "power" => return global("Power"),
        "input_trim" => return global("Input"),
        "output_trim" => return global("Output"),
        "tuner_on" => return global("Tuner"),
        "tone_engine" => return ("Amp".into(), "Engine".into()),
        "cab_mic_type" => return ("Cab".into(), "Microphone".into()),
        "nam_loudness_norm" => return ("Amp".into(), "Capture Normalized".into()),
        "line2_mode" => return ("Lanes".into(), "Lane 1 Mode".into()),
        "line2_mix" => return ("Lanes".into(), "Lane 1 Blend".into()),
        "transpose_semi" => return global("Transpose"),
        "transpose_cents" => return global("Transpose Fine"),
        "delay_sync" => return ("Delay A".into(), "Time Sync".into()),
        "delay_sync_r" => return ("Delay A".into(), "Time R Sync".into()),
        "delay2_sync" => return ("Delay B".into(), "Time Sync".into()),
        "delay2_sync_r" => return ("Delay B".into(), "Time R Sync".into()),
        _ => {}
    }
    if let Some(n) = id.strip_prefix("row_send_") {
        let lane = n.parse::<usize>().map_or(0, |n| n + 1);
        return ("Lanes".into(), format!("Lane {lane} Mode"));
    }
    if let Some(n) = id.strip_prefix("row_level_") {
        let lane = n.parse::<usize>().map_or(0, |n| n + 1);
        return ("Lanes".into(), format!("Lane {lane} Send"));
    }
    if let Some(slot) = path_slot(id) {
        return ("Signal Path".into(), format!("Slot {}", slot + 1));
    }
    let defaults = rodharerist::default_params();
    let specs = block_specs();
    for &kind in StageKind::ALL {
        let block = blocks::block_name(kind).to_string();
        if blocks::enable_id(kind) == id {
            return (block, "On".into());
        }
        if kind.row_id() == id {
            return (block, "Lane".into());
        }
        if kind.channel_id() == Some(id) {
            return (block, "Channel".into());
        }
        if let Some((_, spec)) = specs.iter().find(|(k, spec)| *k == kind && spec.id == id) {
            return (block, spec.label.into());
        }
        if blocks::models(&defaults, kind)
            .0
            .iter()
            .any(|m| m.wire_id == id)
        {
            return (block, "Model".into());
        }
    }
    ("Other".into(), id.into())
}

/// The title a host lists for MIDI learn and automation.
///
/// The prefix is the selected stomp ("Green Screamer", "Twin Clean"), not
/// the block category ("Drive A", "Amp"). A second instance of the same
/// stomp gets " 2" so the two stay distinct. Lanes, the signal path, and
/// the global trims keep the group `describe` already uses.
pub fn host_name_for(params: &CoreParams, id: &str) -> String {
    display_name(params, id, &block_specs())
}

fn display_name(params: &CoreParams, id: &str, specs: &[(StageKind, ParamSpec)]) -> String {
    let (group, label) = describe(id);
    if group == "Global" {
        return label;
    }
    if matches!(group.as_str(), "Lanes" | "Signal Path") {
        return format!("{group} {label}");
    }
    match kind_owning(id, specs) {
        Some(kind) => format!("{} {label}", stomp_title(params, kind)),
        None => format!("{group} {label}"),
    }
}

fn kind_owning(id: &str, specs: &[(StageKind, ParamSpec)]) -> Option<StageKind> {
    // The microphone select is described on its own, ahead of the block loop.
    if id == "cab_mic_type" {
        return Some(StageKind::Cab);
    }
    let defaults = rodharerist::default_params();
    for &kind in StageKind::ALL {
        if blocks::enable_id(kind) == id || kind.row_id() == id || kind.channel_id() == Some(id) {
            return Some(kind);
        }
        if specs.iter().any(|(k, spec)| *k == kind && spec.id == id) {
            return Some(kind);
        }
        if blocks::models(&defaults, kind)
            .0
            .iter()
            .any(|m| m.wire_id == id)
        {
            return Some(kind);
        }
    }
    None
}

/// The stomp a host should show. The later instance of a repeated name is
/// "Green Screamer 2"; a different model needs no suffix.
fn stomp_title(params: &CoreParams, kind: StageKind) -> String {
    let name = blocks::model_name(params, kind).unwrap_or_else(|| blocks::block_name(kind));
    let repeated = StageKind::ALL.iter().any(|other| {
        other.index() < kind.index() && blocks::model_name(params, *other) == Some(name)
    });
    if repeated {
        format!("{name} 2")
    } else {
        name.to_string()
    }
}

/// Leading number of a typed value, honouring a `k` multiplier ("2.4 kHz").
fn parse_number(text: &str) -> Option<f32> {
    let text = text.trim();
    let end = text
        .char_indices()
        .find(|(i, c)| !(c.is_ascii_digit() || *c == '.' || (*i == 0 && (*c == '-' || *c == '+'))))
        .map(|(i, _)| i)
        .unwrap_or(text.len());
    let value: f32 = text[..end].parse().ok()?;
    let rest = text[end..].trim_start();
    Some(if rest.starts_with('k') || rest.starts_with('K') {
        value * 1000.0
    } else {
        value
    })
}

fn build_entry(id: &'static str, default: f32, specs: &[(StageKind, ParamSpec)]) -> WireEntry {
    let name = host_name_for(&rodharerist::default_params(), id);

    if let Some(slot) = path_slot(id) {
        let param = IntParam::new(
            name,
            default.round() as i32,
            IntRange::Linear {
                min: -1,
                max: StageKind::COUNT as i32 - 1,
            },
        )
        .with_value_to_string(Arc::new(|v| match StageKind::from_index(v) {
            Some(kind) => blocks::block_name(kind).to_string(),
            None => "Empty".to_string(),
        }))
        .with_string_to_value(Arc::new(|s| {
            let s = s.trim();
            if s.eq_ignore_ascii_case("empty") {
                return Some(-1);
            }
            StageKind::ALL
                .iter()
                .find(|k| blocks::block_name(**k).eq_ignore_ascii_case(s))
                .map(|k| k.index() as i32)
        }))
        // The rack order is edited in the editor, not automated.
        .non_automatable()
        .hide_in_generic_ui();
        return WireEntry {
            id,
            kind: EntryKind::PathSlot(slot),
            param: WireParam::Int(param, StageKind::COUNT as i32 - 1),
            title: Title::Fixed,
        };
    }

    let param = if is_toggle(id) {
        let param = BoolParam::new(name, default >= 0.5);
        // The tuner is something the player looks at, not part of the sound.
        WireParam::Bool(if id == "tuner_on" {
            param.non_automatable()
        } else {
            param
        })
    } else if let Some(names) = choice_labels(id) {
        let parse_names = names.clone();
        let last = names.len() as i32 - 1;
        // A model catalogue grows between releases; a fixed range keeps the
        // normalized values hosts stored for automation and MIDI learn on the
        // same models (#342).
        let max = if is_model_catalogue(id) {
            MODEL_SLOTS - 1
        } else {
            last
        };
        let param = IntParam::new(
            name,
            default.round() as i32,
            IntRange::Linear { min: 0, max },
        )
        .with_value_to_string(Arc::new(move |v| {
            names
                .get(v.clamp(0, last) as usize)
                .copied()
                .unwrap_or("?")
                .to_string()
        }))
        .with_string_to_value(Arc::new(move |s| {
            let s = s.trim();
            parse_names
                .iter()
                .position(|n| n.eq_ignore_ascii_case(s))
                .map(|i| i as i32)
                .or_else(|| s.parse().ok())
        }));
        // Moving a block between lanes rebuilds the routing: edited in the
        // editor, not automated, like the path order.
        WireParam::Int(
            if id.ends_with("_row") {
                param.non_automatable()
            } else {
                param
            },
            last,
        )
    } else {
        let spec = spec_of(id, specs).unwrap_or_else(|| panic!("no range for wire id `{id}`"));
        let range = match spec.unit {
            // Frequency, time and ratio read better skewed toward their low end.
            "Hz" | "ms" | "s" | ":1" if spec.min > 0.0 => FloatRange::Skewed {
                min: spec.min,
                max: spec.max,
                factor: FloatRange::skew_factor(-1.0),
            },
            _ => FloatRange::Linear {
                min: spec.min,
                max: spec.max,
            },
        };
        WireParam::Float(
            FloatParam::new(name, default, range)
                .with_value_to_string(Arc::new(move |v| blocks::format_value(v, &spec)))
                .with_string_to_value(Arc::new(parse_number)),
        )
    };
    WireEntry {
        id,
        kind: EntryKind::Value,
        param,
        title: title_of(id, specs),
    }
}

/// How [`display_name`] builds `id`'s title, without the model it names.
fn title_of(id: &str, specs: &[(StageKind, ParamSpec)]) -> Title {
    let (group, label) = describe(id);
    if matches!(group.as_str(), "Global" | "Lanes" | "Signal Path") {
        return Title::Fixed;
    }
    match kind_owning(id, specs) {
        Some(kind) => Title::Stomp { kind, label },
        None => Title::Fixed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_wire_id_but_clear_clip_is_a_host_parameter_in_order() {
        let wire = WireParams::new();
        let ids: Vec<&str> = wire.entries().iter().map(|e| e.id).collect();
        let expected: Vec<&str> = UI_PARAM_IDS
            .iter()
            .copied()
            .filter(|id| *id != CLEAR_CLIP)
            .collect();
        assert_eq!(ids, expected);
    }

    #[test]
    fn host_defaults_reproduce_the_core_defaults() {
        assert_eq!(
            WireParams::new().to_core_params(),
            rodharerist::default_params()
        );
    }

    #[test]
    fn every_continuous_parameter_has_a_range_containing_its_default() {
        let specs = block_specs();
        for (id, value) in rodharerist::ui_values(&rodharerist::default_params()) {
            if is_toggle(id) || path_slot(id).is_some() || choice_labels(id).is_some() {
                continue;
            }
            let spec = spec_of(id, &specs).unwrap_or_else(|| panic!("`{id}` has no range"));
            assert!(
                spec.min < spec.max && (spec.min..=spec.max).contains(&value),
                "`{id}`"
            );
        }
    }

    /// #342: a model catalogue's host range does not follow the length of its
    /// list, so a stored normalized value keeps selecting the same model when
    /// models are added; steps above the last model select it.
    #[test]
    fn model_catalogues_keep_a_fixed_host_range() {
        let wire = WireParams::new();
        let mut catalogues = 0;
        for entry in wire.entries() {
            let Some(names) = choice_labels(entry.id) else {
                continue;
            };
            let WireParam::Int(param, last) = &entry.param else {
                panic!("`{}` is a choice but not an IntParam", entry.id);
            };
            assert_eq!(*last, names.len() as i32 - 1, "`{}`", entry.id);
            if !is_model_catalogue(entry.id) {
                assert_eq!(param.range().unnormalize(1.0), *last, "`{}`", entry.id);
                continue;
            }
            catalogues += 1;
            assert!(
                names.len() as i32 <= MODEL_SLOTS,
                "`{}` has outgrown MODEL_SLOTS",
                entry.id
            );
            assert_eq!(param.range().unnormalize(0.0), 0, "`{}`", entry.id);
            assert_eq!(
                param.range().unnormalize(1.0),
                MODEL_SLOTS - 1,
                "`{}`",
                entry.id
            );
            // Model i sits at i / (MODEL_SLOTS - 1) whatever the list length.
            for i in 0..names.len() as i32 {
                let normalized = param.range().normalize(i);
                assert_eq!(
                    normalized,
                    i as f32 / (MODEL_SLOTS - 1) as f32,
                    "`{}` {i}",
                    entry.id
                );
                assert_eq!(param.range().unnormalize(normalized), i);
            }
            // An unused top step reads, and applies, as the last model.
            let top = MODEL_SLOTS - 1;
            if top > *last {
                assert_eq!(
                    param.normalized_value_to_string(param.range().normalize(top), false),
                    names[*last as usize],
                    "`{}`",
                    entry.id
                );
            }
        }
        assert!(catalogues >= 10, "only {catalogues} model catalogues found");
    }

    #[test]
    fn choices_cover_every_index_the_core_takes() {
        assert_eq!(
            choice_labels("drive_model").unwrap().len(),
            DriveModel::ALL.len()
        );
        assert_eq!(
            choice_labels("cab_model").unwrap().len(),
            CabModel::ALL.len()
        );
        assert_eq!(
            choice_labels("gate_channel").unwrap().len(),
            StageChannel::ALL.len()
        );
        for (i, engine) in TONE_ENGINES.iter().enumerate() {
            assert_eq!(engine.index() as usize, i);
        }
        for (i, label) in choice_labels("row_send_1").unwrap().iter().enumerate() {
            assert_eq!(
                rodharerist::RowSend::from_index(i as u32).index(),
                i as u32,
                "{label}"
            );
        }
    }

    #[test]
    fn every_parameter_has_a_block_group() {
        for id in UI_PARAM_IDS.iter().filter(|id| **id != CLEAR_CLIP) {
            assert_ne!(describe(id).0, "Other", "`{id}` has no group");
        }
    }

    #[test]
    fn host_names_follow_the_selected_stomp() {
        let defaults = rodharerist::default_params();
        assert_eq!(
            host_name_for(&defaults, "drive_gain"),
            "Green Screamer Drive"
        );
        let mut rat = defaults.clone();
        rat.drive_model = DriveModel::Rat;
        assert_eq!(host_name_for(&rat, "drive_gain"), "Rats Nest Drive");
        let mut both = rat.clone();
        both.stage_b.drive_model = DriveModel::Rat;
        assert_eq!(host_name_for(&both, "drive2_gain"), "Rats Nest 2 Drive");
        both.stage_b.drive_model = DriveModel::Fuzz;
        assert_eq!(host_name_for(&both, "drive_gain"), "Rats Nest Drive");
        assert_eq!(host_name_for(&both, "drive2_gain"), "Face Fuzz Drive");
        assert_eq!(
            host_name_for(&defaults, "comp_thresh"),
            "Studio Compressor Threshold"
        );
        let wire = WireParams::new();
        assert_eq!(
            wire.get("drive_gain").unwrap().param.display_name(),
            "Green Screamer Drive"
        );
        // Nothing selects another stomp: no title changes, no host notice.
        assert!(!wire.refresh_host_names());
        assert_eq!(
            wire.get("drive_gain").unwrap().param.display_name(),
            "Green Screamer Drive"
        );
    }

    #[test]
    fn typed_values_parse() {
        assert_eq!(parse_number("2.40 kHz"), Some(2400.0));
        assert_eq!(parse_number("-12.5 dB"), Some(-12.5));
        assert_eq!(parse_number("+3"), Some(3.0));
        assert_eq!(parse_number("4.0:1"), Some(4.0));
    }
}

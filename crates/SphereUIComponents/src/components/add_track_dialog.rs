use std::{ops::Range, sync::Arc};

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, size, svg, App, AppContext, Bounds, Context, DragMoveEvent, Empty, Entity,
    EntityInputHandler, FocusHandle, InteractiveElement, IntoElement, KeyDownEvent, ParentElement,
    Pixels, Point, Render, StatefulInteractiveElement, Styled, UTF16Selection, Window,
    WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind,
};

use crate::assets;
use crate::components::color_picker::{
    color_picker_field, ColorPickerCallbacks, ColorPickerPlacement, ColorPickerState,
    ColorPickerValue,
};
use crate::components::controls::{
    fb_button, fb_checkbox, fb_form_row, fb_stepper_button, FbButtonKind,
};
use crate::components::form::{
    select_dismiss_backdrop, select_with_placement, select_with_placement_and_header,
    SelectMenuPlacement, SelectOption,
};
use crate::components::text_input::{
    bind_mouse_selection, text_field_with_callbacks, text_field_with_callbacks_and_ime,
    text_input_context_entries, TextInputAction, TextInputCallbacks, TextInputMouseEvent,
    TextInputMousePhase, TextInputState,
};
use crate::components::timeline::timeline_state::TrackType;
use crate::components::title_bar::external_window_titlebar_with_icon;
use crate::i18n::I18n;
use crate::theme::{self, radius, size, space, typography, Colors};
use crate::window_position::{apply_owner_display, centered_window_bounds};
use SpherePluginHost::{PluginFormat, RegistryPlugin};

const MAX_TRACK_COUNT: u32 = 128;
/// Measured width of the form's label column — a layout constant, not spacing.
/// Every row's control starts on this line so the dialog reads as one form
/// rather than a stack of independent fields.
const FORM_LABEL_WIDTH: f32 = 86.0;
/// Vertical rhythm between form rows and between the panels that hold them.
const FORM_GAP: f32 = space::BASE;
/// Horizontal inset shared by the header, the tab strip, the scrolling body and
/// the footer, so every band in the dialog starts on the same line.
const BODY_PAD_X: f32 = space::LOOSE;
/// Footer band: one primary button plus its breathing room, top and bottom.
const FOOTER_HEIGHT: f32 = size::PROMINENT + 2.0 * space::BASE;
/// Numeric field wide enough for three digits plus the drag affordance.
const COUNT_FIELD_WIDTH: f32 = 54.0;
/// Reserved width for the count field's unit label, so the row does not reflow
/// as it flips between "track" and "tracks".
const COUNT_UNIT_WIDTH: f32 = 44.0;

/// The two glyph sizes this dialog uses. A third would be the point at which a
/// compact form starts looking assembled from spare parts.
const ICON_SM: f32 = 10.0;
const ICON_MD: f32 = 12.0;

/// Colour swatch inside the Auto chip. Under `radius::MIN_SIDE`, so it takes
/// `radius::MICRO` rather than a control radius.
const SWATCH_SM: f32 = 11.0;

/// Vertical drag sensitivity for the Count field: one track per this many
/// pixels dragged. Dragging up increases, down decreases (DAW convention),
/// mirroring the transport BPM scrubber.
const COUNT_DRAG_PX_PER_STEP: f32 = 7.0;

static COUNT_DRAG_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_count_drag_id() -> u64 {
    COUNT_DRAG_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Marker payload for a Count scrub drag. Carries a unique `drag_id` so the
/// receiver can tell a fresh drag from a continuation, plus the count captured
/// when the drag began. Mirrors the transport `BpmDrag` pattern.
#[derive(Clone, Debug)]
pub struct CountDrag {
    pub drag_id: u64,
    pub start_count: u32,
}

impl Render for CountDrag {
    fn render(&mut self, _w: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

/// One drag-move sample delivered to the host from the Count scrubber.
#[derive(Clone, Copy, Debug)]
pub struct CountDragSample {
    pub drag_id: u64,
    pub start_count: u32,
    pub cur_y: f32,
}

/// Host-side anchor for an in-flight Count drag (first sample seeds `start_y`).
#[derive(Clone, Copy, Debug)]
struct CountDragState {
    drag_id: u64,
    start_count: u32,
    start_y: f32,
}

type VoidCb = Arc<dyn Fn(&(), &mut Window, &mut App) + 'static>;
type KindCb = Arc<dyn Fn(&AddTrackKind, &mut Window, &mut App) + 'static>;
type InstrumentModeCb = Arc<dyn Fn(&InstrumentMode, &mut Window, &mut App) + 'static>;
type U32Cb = Arc<dyn Fn(&u32, &mut Window, &mut App) + 'static>;
type BoolCb = Arc<dyn Fn(&bool, &mut Window, &mut App) + 'static>;
type StringCb = Arc<dyn Fn(&String, &mut Window, &mut App) + 'static>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddTrackKind {
    Audio,
    Instrument,
    Midi,
    Automation,
    Folder,
    /// Legacy / menu-only kinds (not shown in the primary tab row).
    Plugin,
    Bus,
    Return,
    Group,
    Master,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioFormat {
    Mono,
    Stereo,
}

/// Label for an Audio track with no input connection. Captures nothing — it is
/// never a system default input.
pub(crate) const NO_AUDIO_INPUT_LABEL: &str = crate::input_routing::NO_INPUT_LABEL;

/// One logical Input Audio Connection the dialog can offer.
///
/// Carries the bus's channel count so switching Mono/Stereo re-filters the list
/// without asking the layout for a fresh one — a mono track must never be able
/// to select a stereo bus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddTrackInputChoice {
    pub label: String,
    pub connection_id: crate::audio_connections::AudioConnectionId,
    pub channels: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstrumentMode {
    Vsti,
    SoundfontPlayer,
    SolfegeEngine,
}

impl InstrumentMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Vsti => "VSTi",
            Self::SoundfontPlayer => "Soundfont Player",
            Self::SolfegeEngine => "Solfege Engine",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddTrackSelectId {
    AudioFormat,
    InstrumentPlugin,
    SolfegeModel,
    Input,
    Output,
    MidiChannel,
}

impl AddTrackKind {
    pub fn label(self) -> &'static str {
        self.default_name_stem()
    }

    pub fn label_key(self) -> &'static str {
        match self {
            Self::Audio => "add-track.kind.audio",
            Self::Instrument => "add-track.kind.instrument",
            Self::Midi => "add-track.kind.midi",
            Self::Plugin => "add-track.kind.plugin",
            Self::Bus => "add-track.kind.bus",
            Self::Return => "add-track.kind.return",
            Self::Group => "add-track.kind.group",
            Self::Master => "add-track.kind.master",
            Self::Automation => "add-track.kind.automation",
            Self::Folder => "add-track.kind.folder",
        }
    }

    pub fn description_key(self) -> &'static str {
        match self {
            Self::Audio => "add-track.description.audio",
            Self::Instrument => "add-track.description.instrument",
            Self::Midi => "add-track.description.midi",
            Self::Plugin => "add-track.description.plugin",
            Self::Bus => "add-track.description.bus",
            Self::Return => "add-track.description.return",
            Self::Group => "add-track.description.group",
            Self::Master => "add-track.description.master",
            Self::Automation => "add-track.description.automation",
            Self::Folder => "add-track.description.folder",
        }
    }

    pub fn tab_label(self) -> &'static str {
        match self {
            Self::Audio => "Audio",
            Self::Instrument => "Instrument",
            Self::Midi => "MIDI",
            Self::Automation => "Automation",
            Self::Folder => "Folder",
            Self::Plugin => "Plugin",
            Self::Bus => "Bus",
            Self::Return => "Return",
            Self::Group => "Group",
            Self::Master => "Master",
        }
    }

    /// Compact tab chrome labels (no "Track" suffix) so the type row fits.
    pub fn tab_label_key(self) -> &'static str {
        match self {
            Self::Audio => "add-track.tab.audio",
            Self::Instrument => "add-track.tab.instrument",
            Self::Midi => "add-track.tab.midi",
            Self::Plugin => "add-track.tab.plugin",
            Self::Bus => "add-track.tab.bus",
            Self::Return => "add-track.tab.return",
            Self::Group => "add-track.tab.group",
            Self::Master => "add-track.tab.master",
            Self::Automation => "add-track.tab.automation",
            Self::Folder => "add-track.tab.folder",
        }
    }

    pub fn default_name_stem(self) -> &'static str {
        match self {
            Self::Audio => "Audio Track",
            Self::Instrument => "Instrument Track",
            Self::Midi => "MIDI Track",
            Self::Automation => "Automation Track",
            Self::Folder => "Folder Track",
            Self::Plugin => "Plugin Track",
            Self::Bus => "Bus Track",
            Self::Return => "Return Track",
            Self::Group => "Group Track",
            Self::Master => "Master Track",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Self::Audio => assets::ICON_MIC_PATH,
            Self::Instrument => assets::ICON_CPU_PATH,
            Self::Midi => assets::ICON_MUSIC_PATH,
            Self::Automation => assets::ICON_AUTOMATION_PATH,
            Self::Folder => assets::ICON_FOLDER_PATH,
            Self::Plugin => assets::ICON_PLUG_PATH,
            Self::Bus => assets::ICON_GIT_MERGE_PATH,
            Self::Return => assets::ICON_CORNER_DOWN_LEFT_PATH,
            Self::Group => assets::ICON_GIT_FORK_PATH,
            Self::Master => assets::ICON_VOLUME_2_PATH,
        }
    }

    pub fn native_track_type(self) -> Option<TrackType> {
        match self {
            Self::Audio => Some(TrackType::Audio),
            Self::Instrument => Some(TrackType::Instrument),
            Self::Midi => Some(TrackType::Midi),
            Self::Bus => Some(TrackType::Bus),
            Self::Return => Some(TrackType::Return),
            Self::Folder | Self::Group => Some(TrackType::Group),
            Self::Automation | Self::Plugin | Self::Master => None,
        }
    }

    pub fn default_input(self) -> &'static str {
        match self {
            Self::Midi | Self::Instrument => "All MIDI Inputs",
            // An Audio track starts with no input: creating one must never
            // open a capture device the user did not choose.
            Self::Audio => NO_AUDIO_INPUT_LABEL,
            _ => "None",
        }
    }

    /// Primary DAW-style tabs shown at the top of the dialog.
    pub fn primary_tabs() -> &'static [Self] {
        &[
            Self::Audio,
            Self::Instrument,
            Self::Midi,
            Self::Bus,
            Self::Automation,
            Self::Folder,
        ]
    }

    /// Tabs visible for the current selection (primary + menu-only kinds).
    pub fn visible_tabs(selected: Self) -> Vec<Self> {
        let mut tabs = Self::primary_tabs().to_vec();
        for extra in [
            Self::Bus,
            Self::Return,
            Self::Plugin,
            Self::Group,
            Self::Master,
        ] {
            if selected == extra && !tabs.contains(&extra) {
                tabs.push(extra);
            }
        }
        tabs
    }
}

#[derive(Debug, Clone)]
pub struct AddTrackDialogState {
    pub is_open: bool,
    pub selected_kind: AddTrackKind,
    pub track_name: String,
    pub count: u32,
    pub auto_color: bool,
    pub color_index: usize,
    /// Chosen custom color when `auto_color` is false. `None` falls back to the
    /// palette color at `color_index`. Persisted indirectly via track creation.
    pub custom_color: Option<gpui::Rgba>,
    pub audio_format: AudioFormat,
    pub instrument_mode: InstrumentMode,
    pub instrument_plugin_id: Option<String>,
    pub instrument_plugin_name: Option<String>,
    pub solfege_model_path: Option<String>,
    pub fx_chain: Option<String>,
    pub input_label: String,
    pub output_label: String,
    /// Live Bus/Return destinations for the Output select: `(track_id, display_name)`.
    pub audio_output_targets: Vec<(String, String)>,
    /// Logical Input Audio Connections offered by the Input select, as
    /// `(menu label, connection id)`. Built from the project registry — the
    /// dialog never enumerates hardware and never creates a bus of its own.
    pub audio_input_choices: Vec<AddTrackInputChoice>,
    pub ascending_input: bool,
    pub ascending_output: bool,
    pub midi_channel_label: String,
    pub pack_folder: bool,
    pub channel_count: u32,
    pub arm_track: bool,
    pub monitor_mode: &'static str,
    pub next_number: usize,
    pub has_master_track: bool,
    pub base_track_count: usize,
}

pub(crate) fn add_track_debug_enabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| {
        std::env::var("FUTUREBOARD_ADD_TRACK_DEBUG")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    })
}

pub(crate) fn add_track_debug(message: &str) {
    if add_track_debug_enabled() {
        eprintln!("[Add Track] {message}");
    }
}

impl AddTrackDialogState {
    pub fn closed() -> Self {
        Self::open_for(0, false)
    }

    pub fn open_for(track_count: usize, has_master_track: bool) -> Self {
        Self::open_for_with_monitor(track_count, has_master_track, "off")
    }

    pub fn open_for_with_monitor(
        track_count: usize,
        has_master_track: bool,
        default_monitor_mode: &'static str,
    ) -> Self {
        let next_number = track_count.saturating_add(1);
        let kind = AddTrackKind::Audio;
        Self {
            is_open: true,
            selected_kind: kind,
            track_name: format!("{} {}", kind.default_name_stem(), next_number),
            count: 1,
            auto_color: true,
            color_index: track_count % Colors::TRACK_COLORS.len(),
            custom_color: None,
            audio_format: AudioFormat::Stereo,
            instrument_mode: InstrumentMode::Vsti,
            instrument_plugin_id: None,
            instrument_plugin_name: None,
            solfege_model_path: crate::solfege::default_model_path()
                .map(|path| path.to_string_lossy().into_owned()),
            fx_chain: None,
            input_label: kind.default_input().to_string(),
            output_label: "Main".to_string(),
            audio_output_targets: Vec::new(),
            audio_input_choices: Vec::new(),
            ascending_input: false,
            ascending_output: false,
            midi_channel_label: "All Channels".to_string(),
            pack_folder: false,
            channel_count: 2,
            arm_track: false,
            monitor_mode: valid_monitor_mode(default_monitor_mode),
            next_number,
            has_master_track,
            base_track_count: track_count,
        }
    }

    pub fn selected_color(&self) -> gpui::Rgba {
        if self.auto_color {
            track_color(self.color_index)
        } else {
            self.custom_color
                .unwrap_or_else(|| track_color(self.color_index))
        }
    }

    pub fn is_valid(&self) -> bool {
        let name_ok = !self.track_name.trim().is_empty();
        let count_ok = self.count >= 1 && self.count <= MAX_TRACK_COUNT;
        name_ok
            && count_ok
            && self.selected_kind.native_track_type().is_some()
            && kind_singleton_available(self.selected_kind, self)
    }

    /// Menu labels for the Audio track Input select.
    ///
    /// No Input first, then the project's Input Audio Connections that fit this
    /// track's channel count. A physical device never appears: mapping hardware
    /// is Audio Connections' job, and this dialog creates no bus of its own.
    pub fn audio_input_option_labels(&self) -> Vec<SelectOption> {
        let track_channels = match self.audio_format {
            AudioFormat::Mono => 1,
            AudioFormat::Stereo => 2,
        };
        let mut options = vec![SelectOption::new(
            NO_AUDIO_INPUT_LABEL,
            NO_AUDIO_INPUT_LABEL,
        )];
        options.extend(
            self.audio_input_choices
                .iter()
                // A mono bus up-mixed into a stereo track is well defined; a
                // stereo bus feeding a mono track is not, so it is not offered.
                .filter(|choice| choice.channels <= track_channels)
                .map(|choice| SelectOption::new(choice.label.clone(), choice.label.clone())),
        );
        options
    }

    /// The connection a label selects. `None` for No Input and for the
    /// Audio Connections escape hatch, which is not a routing value.
    pub fn audio_input_connection_for_label(
        &self,
        label: &str,
    ) -> Option<crate::audio_connections::AudioConnectionId> {
        self.audio_input_option_labels()
            .iter()
            .any(|option| option.id == label)
            .then(|| {
                self.audio_input_choices
                    .iter()
                    .find(|choice| choice.label == label)
                    .map(|choice| choice.connection_id.clone())
            })
            .flatten()
    }

    pub fn sync_channel_count_from_format(&mut self) {
        self.channel_count = match self.audio_format {
            AudioFormat::Mono => 1,
            AudioFormat::Stereo => 2,
        };
    }

    pub fn set_audio_format(&mut self, audio_format: AudioFormat) {
        self.audio_format = audio_format;
        self.sync_channel_count_from_format();
        // Mono/Stereo changes which connections *fit*, so a selection that no
        // longer appears falls back to No Input rather than to another bus.
        if !self
            .audio_input_option_labels()
            .iter()
            .any(|option| option.id == self.input_label)
        {
            self.input_label = NO_AUDIO_INPUT_LABEL.to_string();
        }
    }

    pub fn set_kind(&mut self, kind: AddTrackKind) {
        self.selected_kind = kind;
        self.input_label = kind.default_input().to_string();
    }
}

#[derive(Clone)]
pub struct AddTrackDialogCallbacks {
    pub on_close: VoidCb,
    pub on_confirm: VoidCb,
    pub on_select_kind: KindCb,
    pub on_count_delta: Arc<dyn Fn(&i32, &mut Window, &mut App) + 'static>,
    pub on_count_drag: Arc<dyn Fn(&CountDragSample, &mut Window, &mut App) + 'static>,
    pub on_count_begin_edit: VoidCb,
    pub on_audio_format: Arc<dyn Fn(&AudioFormat, &mut Window, &mut App) + 'static>,
    pub on_color_index: U32Cb,
    pub on_auto_color: BoolCb,
    pub on_ascending_input: BoolCb,
    pub on_ascending_output: BoolCb,
    pub on_pack_folder: BoolCb,
    pub on_arm: BoolCb,
    pub on_monitor: Arc<dyn Fn(&String, &mut Window, &mut App) + 'static>,
    pub on_toggle_select: Arc<dyn Fn(&AddTrackSelectId, &mut Window, &mut App) + 'static>,
    pub on_instrument_mode: InstrumentModeCb,
    pub on_instrument_plugin: StringCb,
    pub on_solfege_model: StringCb,
    pub on_input_label: StringCb,
    pub on_output_label: StringCb,
    pub on_midi_channel_label: StringCb,
}

pub fn track_color(index: usize) -> gpui::Rgba {
    Colors::track_color_for_index(index)
}

/// Map the dialog's auto/custom color state to a [`ColorPickerValue`].
fn color_picker_value_for(state: &AddTrackDialogState) -> ColorPickerValue {
    if state.auto_color {
        ColorPickerValue::auto()
    } else {
        ColorPickerValue::custom(state.selected_color())
    }
}

fn kind_supported(kind: AddTrackKind, state: &AddTrackDialogState) -> bool {
    kind.native_track_type().is_some() && kind_singleton_available(kind, state)
}

/// `false` when `kind` is a one-per-project track type the project already has.
fn kind_singleton_available(kind: AddTrackKind, state: &AddTrackDialogState) -> bool {
    match kind {
        AddTrackKind::Master => !state.has_master_track,
        _ => true,
    }
}

fn icon(path: &'static str, size: f32, color: gpui::Rgba) -> impl IntoElement {
    svg().path(path).w(px(size)).h(px(size)).text_color(color)
}

fn select_box(text: impl Into<String>) -> impl IntoElement {
    let text = text.into();
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .w_full()
        .h(px(size::COMFORTABLE))
        .rounded(px(radius::CONTROL))
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_input())
        .px(px(space::BASE))
        .text_size(px(typography::UI_XS))
        .text_color(Colors::text_secondary())
        .child(text)
        .child(icon(
            assets::ICON_CHEVRON_DOWN_PATH,
            ICON_SM,
            Colors::text_faint(),
        ))
}

/// A field whose value is fixed for this track kind — an FX chain slot with no
/// presets yet, the Soundfont Player's built-in instrument, a MIDI output that
/// is always None.
///
/// It used to be `select_box` verbatim, chevron and all: a control that looks
/// exactly like the openable selects beside it and opens nothing. Disabled has
/// a defined look in this system — muted content, no chevron to promise a menu,
/// no hover — so the field can still show its value without claiming to be a
/// control.
fn locked_select_box(text: impl Into<String>) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(size::COMFORTABLE))
        .rounded(px(radius::CONTROL))
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_input())
        .px(px(space::BASE))
        .text_size(px(typography::UI_XS))
        // `text.disabled` is the token that already encodes this state; adding
        // an opacity on top would mute it twice and drop it under the contrast
        // floor.
        .text_color(Colors::text_disabled())
        .child(text.into())
}

fn select_options(values: &[&'static str]) -> Vec<SelectOption> {
    values
        .iter()
        .map(|value| SelectOption::new(*value, *value))
        .collect()
}

/// Output destinations for Audio/Instrument/MIDI tracks: Main, each live
/// project Bus/Return as `Bus - {name}`, then None. Replaces the old hardcoded
/// "Bus A" stub that never mapped to a real track id.
fn audio_output_select_options(bus_targets: &[(String, String)]) -> Vec<SelectOption> {
    let mut options = vec![SelectOption::new("Main", "Main")];
    for (_, name) in bus_targets {
        let label = format!("Bus - {name}");
        options.push(SelectOption::new(label.clone(), label));
    }
    options.push(SelectOption::new("None", "None"));
    options
}

/// MIDI input options for the Instrument/MIDI routing selects: real detected
/// input devices (from `device_registry::cached_midi_devices`, resolved
/// against Preferences โ’ MIDI enable state) sandwiched between the two
/// synthetic entries every track kind supports. Empty `devices` (no hardware,
/// or everything disconnected/disabled) still yields a usable All/None list.
fn midi_input_select_options(devices: &[String]) -> Vec<SelectOption> {
    let mut options = vec![SelectOption::new("All MIDI Inputs", "All MIDI Inputs")];
    options.extend(
        devices
            .iter()
            .map(|name| SelectOption::new(name.clone(), name.clone())),
    );
    options.push(SelectOption::new("None", "None"));
    options
}

fn solfege_model_options() -> Vec<SelectOption> {
    let paths = crate::solfege::discover_model_paths();
    if paths.is_empty() {
        return vec![SelectOption::new("", "No Solfege models found")
            .description(crate::solfege::model_directory().display().to_string())];
    }
    paths
        .into_iter()
        .map(|path| {
            let id = path.to_string_lossy().into_owned();
            let label = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("Solfege model")
                .to_string();
            SelectOption::new(id, label).description(path.display().to_string())
        })
        .collect()
}

fn plugin_format_label(format: PluginFormat) -> &'static str {
    match format {
        PluginFormat::Vst3 => "VST3",
        PluginFormat::Vst2 => "VST2",
        PluginFormat::Clap => "CLAP",
        PluginFormat::Au => "AU",
        PluginFormat::Lv2 => "LV2",
        PluginFormat::Unknown => "Unknown",
    }
}

fn instrument_plugin_options(plugins: &[RegistryPlugin]) -> Vec<SelectOption> {
    if plugins.is_empty() {
        return vec![SelectOption::new("", "No Instrument")
            .description("Open Plugin Manager to scan instruments")];
    }
    // Prefer VST3 when the same instrument ships in several formats: identical
    // display names made the dropdown pick CLAP first during Surge XT testing,
    // and VST3 is the most exercised of the hosted bridges.
    let mut sorted: Vec<&RegistryPlugin> = plugins.iter().collect();
    sorted.sort_by(|a, b| {
        let fmt_rank = |f: PluginFormat| match f {
            PluginFormat::Vst3 => 0u8,
            PluginFormat::Clap => 1,
            PluginFormat::Au => 2,
            // Below the rest: when the same instrument ships in several
            // formats, VST2 is the legacy one and should only win if nothing
            // else exists.
            PluginFormat::Vst2 => 3,
            PluginFormat::Lv2 => 4,
            PluginFormat::Unknown => 5,
        };
        a.name
            .to_ascii_lowercase()
            .cmp(&b.name.to_ascii_lowercase())
            .then_with(|| fmt_rank(a.format).cmp(&fmt_rank(b.format)))
            .then_with(|| a.id.cmp(&b.id))
    });
    let mut name_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for plugin in &sorted {
        *name_counts
            .entry(plugin.name.to_ascii_lowercase())
            .or_default() += 1;
    }
    let mut options = vec![SelectOption::new("", "No Instrument")];
    options.extend(sorted.into_iter().map(|plugin| {
        let vendor = plugin.vendor.trim();
        let fmt = plugin_format_label(plugin.format);
        let description = if vendor.is_empty() {
            fmt.to_string()
        } else {
            format!("{vendor} / {fmt}")
        };
        let label = if name_counts
            .get(&plugin.name.to_ascii_lowercase())
            .copied()
            .unwrap_or(0)
            > 1
        {
            format!("{} ({fmt})", plugin.name)
        } else {
            plugin.name.clone()
        };
        SelectOption::new(plugin.id.clone(), label).description(description)
    }));
    options
}

fn filtered_instrument_plugin_options(
    plugins: &[RegistryPlugin],
    query: &str,
) -> Vec<SelectOption> {
    let query = query.trim().to_lowercase();
    let options = instrument_plugin_options(plugins);
    if query.is_empty() {
        return options;
    }
    let filtered: Vec<_> = options
        .into_iter()
        .filter(|option| {
            option.label.to_lowercase().contains(&query)
                || option
                    .description
                    .as_deref()
                    .is_some_and(|description| description.to_lowercase().contains(&query))
        })
        .collect();
    if filtered.is_empty() {
        vec![SelectOption::new("__no_instrument_results", "No matching instruments").disabled(true)]
    } else {
        filtered
    }
}

fn plugin_search_header(
    search_input: &TextInputState,
    search_focused: bool,
    search_callbacks: TextInputCallbacks,
    ime_target: Entity<AddTrackWindow>,
) -> gpui::AnyElement {
    div()
        .mb(px(space::TIGHT))
        .child(text_field_with_callbacks_and_ime(
            search_input,
            search_focused,
            search_callbacks,
            ime_target,
        ))
        .into_any_element()
}

fn find_instrument_plugin<'a>(
    plugins: &'a [RegistryPlugin],
    value: &str,
) -> Option<&'a RegistryPlugin> {
    plugins.iter().find(|plugin| {
        plugin.id == value
            || plugin.class_id.as_deref() == Some(value)
            || plugin.name.eq_ignore_ascii_case(value)
    })
}

fn add_track_select_open(open_select: Option<AddTrackSelectId>, id: AddTrackSelectId) -> bool {
    open_select == Some(id)
}

fn add_track_select(
    id: &'static str,
    selected_id: Option<&str>,
    placeholder: impl Into<String>,
    options: Vec<SelectOption>,
    open: bool,
    placement: SelectMenuPlacement,
    on_toggle: Arc<dyn Fn(&(), &mut Window, &mut App) + 'static>,
    on_change: Arc<dyn Fn(&String, &mut Window, &mut App) + 'static>,
) -> impl IntoElement {
    select_with_placement(
        id,
        selected_id,
        placeholder,
        options,
        open,
        false,
        placement,
        on_toggle,
        on_change,
    )
}

/// The number portion of the Count control. While editing it is a text field
/// (click-to-type); otherwise it is a draggable scrubber โ€” drag up/down to
/// change the value, click to edit โ€” reusing the transport BPM drag pattern.
fn count_field(
    state: &AddTrackDialogState,
    count_input: &TextInputState,
    count_focused: bool,
    count_callbacks: TextInputCallbacks,
    callbacks: &AddTrackDialogCallbacks,
) -> gpui::AnyElement {
    if count_focused {
        return div()
            .w(px(COUNT_FIELD_WIDTH))
            .child(text_field_with_callbacks(
                count_input,
                count_focused,
                count_callbacks,
            ))
            .into_any_element();
    }

    let count = state.count;
    let on_begin_edit = callbacks.on_count_begin_edit.clone();
    let on_drag_move = callbacks.on_count_drag.clone();
    div()
        .id("add-track-count-field")
        .w(px(COUNT_FIELD_WIDTH))
        .h(px(size::COMFORTABLE))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(radius::CONTROL))
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_input())
        .text_size(px(typography::UI_SM))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(Colors::text_primary())
        .cursor(gpui::CursorStyle::ResizeUpDown)
        .hover(|s| s.bg(Colors::surface_control_hover()))
        .occlude()
        .child(count.to_string())
        // Click (no drag) enters inline edit; a drag scrubs the value instead.
        .on_click(move |_event, w, cx| on_begin_edit(&(), w, cx))
        .on_drag(
            CountDrag {
                drag_id: 0,
                start_count: count,
            },
            move |drag, _offset, _window, cx| {
                let id = next_count_drag_id();
                let start_count = drag.start_count;
                cx.new(|_| CountDrag {
                    drag_id: id,
                    start_count,
                })
            },
        )
        .on_drag_move::<CountDrag>(move |event: &DragMoveEvent<CountDrag>, w, cx| {
            let drag = event.drag(cx);
            let sample = CountDragSample {
                drag_id: drag.drag_id,
                start_count: drag.start_count,
                cur_y: event.event.position.y.into(),
            };
            on_drag_move(&sample, w, cx);
        })
        .into_any_element()
}

fn count_stepper(
    state: &AddTrackDialogState,
    count_input: &TextInputState,
    count_focused: bool,
    count_callbacks: TextInputCallbacks,
    callbacks: &AddTrackDialogCallbacks,
) -> impl IntoElement {
    let down = callbacks.on_count_delta.clone();
    let up = callbacks.on_count_delta.clone();
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::SNUG))
        .child(fb_stepper_button(
            "add-track-count-minus",
            "-",
            move |_, w, cx| down(&-1, w, cx),
        ))
        .child(count_field(
            state,
            count_input,
            count_focused,
            count_callbacks,
            callbacks,
        ))
        .child(fb_stepper_button(
            "add-track-count-plus",
            "+",
            move |_, w, cx| up(&1, w, cx),
        ))
        .child(
            div()
                .min_w(px(COUNT_UNIT_WIDTH))
                .text_size(px(typography::DENSE_LABEL))
                .text_color(Colors::text_faint())
                .child(if state.count == 1 { "track" } else { "tracks" }),
        )
}

fn instrument_mode_selector(
    selected: InstrumentMode,
    callbacks: &AddTrackDialogCallbacks,
) -> impl IntoElement {
    let mut row = div()
        .flex()
        .flex_row()
        .gap(px(space::TIGHT))
        .w_full()
        .h(px(size::COMFORTABLE));

    for (index, mode) in [
        InstrumentMode::Vsti,
        InstrumentMode::SoundfontPlayer,
        InstrumentMode::SolfegeEngine,
    ]
    .into_iter()
    .enumerate()
    {
        let active = selected == mode;
        let cb = callbacks.on_instrument_mode.clone();
        row = row.child(
            div()
                .id(("add-track-instrument-mode", index))
                .flex()
                .items_center()
                .justify_center()
                .h_full()
                .flex_1()
                .px(px(space::BASE))
                .rounded(px(radius::CONTROL))
                .border(px(1.0))
                .border_color(if active {
                    Colors::border_accent()
                } else {
                    Colors::border_subtle()
                })
                .bg(if active {
                    Colors::accent_muted()
                } else {
                    Colors::surface_input()
                })
                .text_size(px(typography::UI_XS))
                .font_weight(if active {
                    gpui::FontWeight::SEMIBOLD
                } else {
                    gpui::FontWeight::NORMAL
                })
                .text_color(if active {
                    Colors::text_primary()
                } else {
                    Colors::text_muted()
                })
                .cursor(gpui::CursorStyle::PointingHand)
                .hover(|s| s.bg(Colors::surface_hover()))
                .on_click(move |_, w, cx| cb(&mode, w, cx))
                .child(mode.label()),
        );
    }

    row
}

fn type_tabs(
    state: &AddTrackDialogState,
    callbacks: &AddTrackDialogCallbacks,
    i18n: I18n,
) -> impl IntoElement {
    let tabs = AddTrackKind::visible_tabs(state.selected_kind);
    let mut row = div()
        .id("add-track-type-tabs")
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .gap(px(space::HAIR))
        .px(px(BODY_PAD_X))
        .py(px(space::SNUG));
    for (i, kind) in tabs.iter().enumerate() {
        let active = state.selected_kind == *kind;
        let supported = kind_supported(*kind, state);
        let cb = callbacks.on_select_kind.clone();
        let k = *kind;
        let mut tab = div()
            .id(("add-track-tab", i))
            .flex()
            .flex_1()
            .flex_row()
            .items_center()
            .justify_center()
            .gap(px(space::TIGHT))
            .h(px(size::COMFORTABLE))
            .min_w(px(0.0))
            .px(px(space::SNUG))
            .overflow_hidden()
            .rounded(px(radius::CONTROL))
            .border(px(1.0))
            .border_color(if active {
                Colors::border_accent()
            } else {
                Colors::border_subtle()
            })
            .bg(if active {
                Colors::accent_muted()
            } else {
                Colors::surface_input()
            })
            .opacity(if supported { 1.0 } else { 0.42 })
            .child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .w(px(ICON_MD))
                    .h(px(ICON_MD))
                    .child(icon(
                        kind.icon(),
                        ICON_MD,
                        if active {
                            Colors::accent_primary()
                        } else {
                            Colors::text_muted()
                        },
                    )),
            )
            .child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .truncate()
                    .text_size(px(typography::DENSE_LABEL))
                    .font_weight(if active {
                        gpui::FontWeight::SEMIBOLD
                    } else {
                        gpui::FontWeight::NORMAL
                    })
                    .text_color(if active {
                        Colors::text_primary()
                    } else {
                        Colors::text_muted()
                    })
                    .child(i18n.tr(kind.tab_label_key())),
            );
        if supported {
            tab = tab
                .cursor(gpui::CursorStyle::PointingHand)
                .hover(|s| s.bg(Colors::surface_hover()))
                .on_click(move |_, w, cx| cb(&k, w, cx));
        }
        row = row.child(tab);
    }
    row
}

/// Everything needed to render the color picker inside the Add Track color row.
/// Bundled so the (already large) body signature stays readable.
pub struct AddTrackColorUi<'a> {
    pub picker: &'a ColorPickerState,
    pub presets: Vec<gpui::Rgba>,
    pub hex_focused: bool,
    pub hex_callbacks: TextInputCallbacks,
    pub callbacks: ColorPickerCallbacks,
}

/// A single bordered color box: an integrated Auto option, the full DAW palette
/// as a swatch grid, and a "Customโ€ฆ" trigger for arbitrary hex/RGB colors.
/// Selecting a manual swatch turns Auto off (via `on_pick`); selecting Auto
/// re-enables automatic color assignment and previews the computed color.
fn color_row(
    state: &AddTrackDialogState,
    callbacks: &AddTrackDialogCallbacks,
    color_ui: AddTrackColorUi,
    i18n: I18n,
) -> impl IntoElement {
    let auto_cb = callbacks.on_auto_color.clone();
    let auto_on = state.auto_color;
    let selected_hex = crate::color::rgba_to_hex(state.selected_color());
    // Color previewed while Auto is on (index-derived, matches track creation).
    let computed_auto = track_color(state.color_index);

    // Auto option โ€” integrated into the box, left of the palette. Clicking it
    // enables automatic color assignment; the chip previews the computed color.
    let auto_chip = div()
        .id("add-track-auto-color")
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::SNUG))
        .flex_shrink_0()
        .h(px(size::ROW_DENSE))
        .px(px(space::SNUG))
        // A 22 px chip is in the small tier, like every other sub-24 control in
        // this dialog.
        .rounded(px(radius::CONTROL_SM))
        .border(px(1.0))
        .border_color(if auto_on {
            Colors::border_accent()
        } else {
            Colors::border_subtle()
        })
        .bg(if auto_on {
            Colors::accent_muted()
        } else {
            Colors::surface_card()
        })
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(|s| s.bg(Colors::surface_control_hover()))
        .on_click(move |_, w, cx| auto_cb(&true, w, cx))
        .child(
            div()
                .w(px(SWATCH_SM))
                .h(px(SWATCH_SM))
                // Under `radius::MIN_SIDE` a 6 px corner merges into a lozenge;
                // an 11 px swatch takes the micro radius the contract reserves
                // for exactly this.
                .rounded(px(radius::MICRO))
                .border(px(1.0))
                .border_color(Colors::with_alpha(Colors::text_primary(), 0.22))
                .bg(computed_auto),
        )
        .child(
            div()
                .text_size(px(typography::DENSE_LABEL))
                .font_weight(if auto_on {
                    gpui::FontWeight::SEMIBOLD
                } else {
                    gpui::FontWeight::MEDIUM
                })
                .text_color(if auto_on {
                    Colors::text_primary()
                } else {
                    Colors::text_secondary()
                })
                .child(i18n.tr("add-track.option.auto-color")),
        );

    // Palette grid โ€” square swatches inside the box. Selecting one turns Auto
    // off (shared `on_pick` keeps the custom-picker preview in sync).
    let mut grid = div()
        .flex()
        .flex_row()
        .flex_wrap()
        .items_center()
        .gap(px(space::SNUG));
    for (i, preset) in color_ui.presets.iter().enumerate() {
        let preset = *preset;
        let on_pick = color_ui.callbacks.on_pick.clone();
        let active = !auto_on && crate::color::rgba_to_hex(preset) == selected_hex;
        grid = grid.child(
            div()
                .id(("add-track-color", i))
                .w(px(size::MICRO))
                .h(px(size::MICRO))
                // Same reason as the Auto chip's swatch: below `MIN_SIDE`.
                .rounded(px(radius::MICRO))
                .border(px(if active { 2.0 } else { 1.0 }))
                .border_color(if active {
                    Colors::text_primary()
                } else {
                    Colors::with_alpha(Colors::text_primary(), 0.22)
                })
                .bg(preset)
                .cursor(gpui::CursorStyle::PointingHand)
                .hover(|s| s.border_color(Colors::border_strong()))
                .on_click(move |_, w, cx| on_pick(preset, w, cx)),
        );
    }

    // Custom color popover (hex + RGB). Auto lives in the chip above, so the
    // popover does not render its own Auto toggle (avoids a duplicate control).
    let picker = color_picker_field(
        "add-track-color-picker",
        color_ui.picker,
        &color_ui.presets,
        false,
        ColorPickerPlacement::Below,
        color_ui.hex_focused,
        color_ui.hex_callbacks,
        color_ui.callbacks,
    );

    let color_box = div()
        .flex()
        .flex_col()
        .gap(px(space::BASE))
        .rounded(px(radius::CONTROL))
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_input())
        .p(px(space::BASE))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::BASE))
                .child(auto_chip)
                .child(
                    div()
                        .w(px(1.0))
                        .h(px(size::MICRO))
                        .flex_shrink_0()
                        .bg(Colors::divider()),
                )
                .child(div().flex_1().min_w_0().child(grid)),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(px(space::BASE))
                .pt(px(space::SNUG))
                .border_t(px(1.0))
                .border_color(Colors::border_subtle())
                .child(
                    div()
                        .text_size(px(typography::DENSE_CAPTION))
                        .font_weight(gpui::FontWeight::BOLD)
                        .text_color(Colors::text_faint())
                        .child("CUSTOM"),
                )
                .child(picker),
        );

    div().child(fb_form_row(i18n.tr("add-track.field.color"), color_box))
}

fn dialog_intro(state: &AddTrackDialogState, i18n: I18n) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(space::BASE))
        .px(px(BODY_PAD_X))
        .py(px(space::SNUG))
        .border_b(px(1.0))
        .border_color(Colors::divider())
        .bg(Colors::surface_panel_alt())
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::BASE))
                .min_w_0()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(1.0))
                        .min_w_0()
                        .child(
                            div()
                                .truncate()
                                .text_size(px(typography::UI_SM))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(Colors::text_primary())
                                .child(i18n.tr(state.selected_kind.label_key())),
                        )
                        .child(
                            div()
                                .truncate()
                                .text_size(px(typography::DENSE_LABEL))
                                .text_color(Colors::text_faint())
                                .child(i18n.tr(state.selected_kind.description_key())),
                        ),
                ),
        )
        .child(
            div()
                .flex_shrink_0()
                .px(px(space::BASE))
                .h(px(size::DENSE))
                .flex()
                .items_center()
                // A 20 px chip takes the small tier — at `CONTROL` it reads as a
                // lozenge next to the 28 px controls below it.
                .rounded(px(radius::CONTROL_SM))
                .border(px(1.0))
                .border_color(Colors::border_subtle())
                .bg(Colors::surface_input())
                .text_size(px(typography::DENSE_LABEL))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(Colors::text_secondary())
                .child(if state.count == 1 {
                    "1 track".to_string()
                } else {
                    format!("{} tracks", state.count)
                }),
        )
}

fn form_panel(child: impl IntoElement) -> impl IntoElement {
    div()
        .rounded(px(radius::SURFACE))
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_panel_alt())
        .p(px(space::BASE))
        .child(child)
}

fn disabled_hint(text: &'static str) -> impl IntoElement {
    div()
        .rounded(px(radius::CONTROL))
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_input())
        .px(px(space::BASE))
        .py(px(space::SNUG))
        .text_size(px(typography::DENSE_LABEL))
        .text_color(Colors::text_faint())
        .child(text)
}

fn type_fields(
    state: &AddTrackDialogState,
    callbacks: &AddTrackDialogCallbacks,
    open_select: Option<AddTrackSelectId>,
    instrument_plugins: &[RegistryPlugin],
    instrument_plugin_query: &str,
    instrument_search_input: Option<&TextInputState>,
    instrument_search_focused: bool,
    instrument_search_callbacks: Option<TextInputCallbacks>,
    instrument_search_ime: Option<Entity<AddTrackWindow>>,
    midi_input_devices: &[String],
    i18n: I18n,
) -> gpui::AnyElement {
    let show_asc = state.count > 1;
    match state.selected_kind {
        AddTrackKind::Audio => {
            let fmt_cb = callbacks.on_audio_format.clone();
            let toggle_format = callbacks.on_toggle_select.clone();
            let input_cb = callbacks.on_input_label.clone();
            let output_cb = callbacks.on_output_label.clone();
            let toggle_input = callbacks.on_toggle_select.clone();
            let toggle_output = callbacks.on_toggle_select.clone();
            let asc_in = callbacks.on_ascending_input.clone();
            let asc_out = callbacks.on_ascending_output.clone();
            let arm_cb = callbacks.on_arm.clone();
            let asc_in_on = state.ascending_input;
            let asc_out_on = state.ascending_output;
            let arm_on = state.arm_track;
            div()
                .flex()
                .flex_col()
                .gap(px(space::SNUG))
                .child(fb_form_row(
                    "Format",
                    add_track_select(
                        "add-track-format-select",
                        Some(match state.audio_format {
                            AudioFormat::Mono => "mono",
                            AudioFormat::Stereo => "stereo",
                        }),
                        "Select format...",
                        vec![
                            SelectOption::new("mono", i18n.tr("add-track.channel.mono")),
                            SelectOption::new("stereo", i18n.tr("add-track.channel.stereo")),
                        ],
                        add_track_select_open(open_select, AddTrackSelectId::AudioFormat),
                        SelectMenuPlacement::Below,
                        Arc::new(move |_, w, cx| {
                            toggle_format(&AddTrackSelectId::AudioFormat, w, cx)
                        }),
                        Arc::new(move |value, w, cx| {
                            let format = if value == "mono" {
                                AudioFormat::Mono
                            } else {
                                AudioFormat::Stereo
                            };
                            fmt_cb(&format, w, cx);
                        }),
                    ),
                ))
                .child(fb_form_row(
                    "FX Chain",
                    locked_select_box(
                        state
                            .fx_chain
                            .clone()
                            .unwrap_or_else(|| "No Preset".to_string()),
                    ),
                ))
                .child(fb_form_row(
                    i18n.tr("add-track.routing.input"),
                    add_track_select(
                        "add-track-input-select",
                        Some(state.input_label.as_str()),
                        "Select input...",
                        state.audio_input_option_labels(),
                        add_track_select_open(open_select, AddTrackSelectId::Input),
                        SelectMenuPlacement::Below,
                        Arc::new(move |_, w, cx| toggle_input(&AddTrackSelectId::Input, w, cx)),
                        Arc::new(move |value, w, cx| input_cb(value, w, cx)),
                    ),
                ))
                .child(fb_form_row(
                    i18n.tr("add-track.routing.output"),
                    add_track_select(
                        "add-track-output-select",
                        Some(state.output_label.as_str()),
                        "Select output...",
                        audio_output_select_options(&state.audio_output_targets),
                        add_track_select_open(open_select, AddTrackSelectId::Output),
                        SelectMenuPlacement::Above,
                        Arc::new(move |_, w, cx| toggle_output(&AddTrackSelectId::Output, w, cx)),
                        Arc::new(move |value, w, cx| output_cb(value, w, cx)),
                    ),
                ))
                .when(show_asc, |col| {
                    col.child(fb_checkbox(
                        "add-track-asc-in",
                        "Ascending Input",
                        asc_in_on,
                        true,
                        move |_, w, cx| asc_in(&!asc_in_on, w, cx),
                    ))
                    .child(fb_checkbox(
                        "add-track-asc-out",
                        "Ascending Output",
                        asc_out_on,
                        true,
                        move |_, w, cx| asc_out(&!asc_out_on, w, cx),
                    ))
                })
                .child(fb_checkbox(
                    "add-track-arm",
                    i18n.tr("add-track.arm"),
                    arm_on,
                    true,
                    move |_, w, cx| arm_cb(&!arm_on, w, cx),
                ))
                .into_any_element()
        }
        AddTrackKind::Instrument => {
            let asc_in = callbacks.on_ascending_input.clone();
            let instrument_cb = callbacks.on_instrument_plugin.clone();
            let solfege_cb = callbacks.on_solfege_model.clone();
            let toggle_instrument = callbacks.on_toggle_select.clone();
            let toggle_solfege = callbacks.on_toggle_select.clone();
            let input_cb = callbacks.on_input_label.clone();
            let output_cb = callbacks.on_output_label.clone();
            let toggle_input = callbacks.on_toggle_select.clone();
            let toggle_output = callbacks.on_toggle_select.clone();
            let asc_in_on = state.ascending_input;
            div()
                .flex()
                .flex_col()
                .gap(px(space::SNUG))
                .child(fb_form_row(
                    "Mode",
                    instrument_mode_selector(state.instrument_mode, callbacks),
                ))
                .child(fb_form_row(
                    "Instrument",
                    if state.instrument_mode == InstrumentMode::Vsti {
                        let search_header = match (
                            instrument_search_input,
                            instrument_search_callbacks,
                            instrument_search_ime,
                        ) {
                            (Some(input), Some(cbs), Some(ime)) => Some(plugin_search_header(
                                input,
                                instrument_search_focused,
                                cbs,
                                ime,
                            )),
                            _ => None,
                        };
                        select_with_placement_and_header(
                            "add-track-instrument-plugin-select",
                            state.instrument_plugin_id.as_deref().or(Some("")),
                            "Select instrument...",
                            filtered_instrument_plugin_options(
                                instrument_plugins,
                                instrument_plugin_query,
                            ),
                            add_track_select_open(open_select, AddTrackSelectId::InstrumentPlugin),
                            false,
                            SelectMenuPlacement::Below,
                            search_header,
                            Arc::new(move |_, w, cx| {
                                toggle_instrument(&AddTrackSelectId::InstrumentPlugin, w, cx)
                            }),
                            Arc::new(move |value, w, cx| instrument_cb(value, w, cx)),
                        )
                        .into_any_element()
                    } else if state.instrument_mode == InstrumentMode::SoundfontPlayer {
                        locked_select_box("Built-in Soundfont Player".to_string())
                            .into_any_element()
                    } else {
                        add_track_select(
                            "add-track-solfege-model-select",
                            Some(state.solfege_model_path.as_deref().unwrap_or("")),
                            "Select Solfege model...",
                            solfege_model_options(),
                            add_track_select_open(open_select, AddTrackSelectId::SolfegeModel),
                            SelectMenuPlacement::Below,
                            Arc::new(move |_, w, cx| {
                                toggle_solfege(&AddTrackSelectId::SolfegeModel, w, cx)
                            }),
                            Arc::new(move |value, w, cx| solfege_cb(value, w, cx)),
                        )
                        .into_any_element()
                    },
                ))
                .when(state.instrument_mode == InstrumentMode::SolfegeEngine, |col| {
                    col.child(disabled_hint(
                        "SFM and FBMX models are loaded from Documents/Futureboard Studio/Utilities/Model/Solfage.",
                    ))
                })
                .child(fb_form_row(
                    i18n.tr("add-track.routing.midi-in"),
                    add_track_select(
                        "add-track-instrument-input-select",
                        Some(state.input_label.as_str()),
                        "Select MIDI input...",
                        midi_input_select_options(midi_input_devices),
                        add_track_select_open(open_select, AddTrackSelectId::Input),
                        SelectMenuPlacement::Below,
                        Arc::new(move |_, w, cx| toggle_input(&AddTrackSelectId::Input, w, cx)),
                        Arc::new(move |value, w, cx| input_cb(value, w, cx)),
                    ),
                ))
                .child(fb_form_row(
                    i18n.tr("add-track.routing.output"),
                    add_track_select(
                        "add-track-instrument-output-select",
                        Some(state.output_label.as_str()),
                        "Select output...",
                        audio_output_select_options(&state.audio_output_targets),
                        add_track_select_open(open_select, AddTrackSelectId::Output),
                        SelectMenuPlacement::Above,
                        Arc::new(move |_, w, cx| toggle_output(&AddTrackSelectId::Output, w, cx)),
                        Arc::new(move |value, w, cx| output_cb(value, w, cx)),
                    ),
                ))
                .child(fb_form_row("FX Chain", select_box("No Preset".to_string())))
                .when(show_asc, |col| {
                    col.child(fb_checkbox(
                        "add-track-asc-in",
                        "Ascending MIDI Input",
                        asc_in_on,
                        true,
                        move |_, w, cx| asc_in(&!asc_in_on, w, cx),
                    ))
                })
                .into_any_element()
        }
        AddTrackKind::Midi => {
            let asc_in = callbacks.on_ascending_input.clone();
            let input_cb = callbacks.on_input_label.clone();
            let output_cb = callbacks.on_output_label.clone();
            let midi_channel_cb = callbacks.on_midi_channel_label.clone();
            let toggle_input = callbacks.on_toggle_select.clone();
            let toggle_output = callbacks.on_toggle_select.clone();
            let toggle_channel = callbacks.on_toggle_select.clone();
            let asc_in_on = state.ascending_input;
            div()
                .flex()
                .flex_col()
                .gap(px(space::SNUG))
                .child(fb_form_row(
                    i18n.tr("add-track.routing.midi-in"),
                    add_track_select(
                        "add-track-midi-input-select",
                        Some(state.input_label.as_str()),
                        "Select MIDI input...",
                        midi_input_select_options(midi_input_devices),
                        add_track_select_open(open_select, AddTrackSelectId::Input),
                        SelectMenuPlacement::Below,
                        Arc::new(move |_, w, cx| toggle_input(&AddTrackSelectId::Input, w, cx)),
                        Arc::new(move |value, w, cx| input_cb(value, w, cx)),
                    ),
                ))
                .child(fb_form_row(
                    "MIDI Output",
                    locked_select_box("None".to_string()),
                ))
                .child(fb_form_row(
                    i18n.tr("add-track.routing.channel"),
                    add_track_select(
                        "add-track-midi-channel-select",
                        Some(state.midi_channel_label.as_str()),
                        "Select channel...",
                        select_options(&[
                            "All Channels",
                            "Channel 1",
                            "Channel 2",
                            "Channel 3",
                            "Channel 4",
                            "Channel 5",
                            "Channel 6",
                            "Channel 7",
                            "Channel 8",
                            "Channel 9",
                            "Channel 10",
                            "Channel 11",
                            "Channel 12",
                            "Channel 13",
                            "Channel 14",
                            "Channel 15",
                            "Channel 16",
                        ]),
                        add_track_select_open(open_select, AddTrackSelectId::MidiChannel),
                        SelectMenuPlacement::Above,
                        Arc::new(move |_, w, cx| {
                            toggle_channel(&AddTrackSelectId::MidiChannel, w, cx)
                        }),
                        Arc::new(move |value, w, cx| midi_channel_cb(value, w, cx)),
                    ),
                ))
                .child(fb_form_row(
                    i18n.tr("add-track.routing.output"),
                    add_track_select(
                        "add-track-midi-output-select",
                        Some(state.output_label.as_str()),
                        "Select output...",
                        audio_output_select_options(&state.audio_output_targets),
                        add_track_select_open(open_select, AddTrackSelectId::Output),
                        SelectMenuPlacement::Above,
                        Arc::new(move |_, w, cx| toggle_output(&AddTrackSelectId::Output, w, cx)),
                        Arc::new(move |value, w, cx| output_cb(value, w, cx)),
                    ),
                ))
                .when(show_asc, |col| {
                    col.child(fb_checkbox(
                        "add-track-asc-in",
                        "Ascending Input",
                        asc_in_on,
                        true,
                        move |_, w, cx| asc_in(&!asc_in_on, w, cx),
                    ))
                })
                .into_any_element()
        }
        AddTrackKind::Automation => div()
            .flex()
            .flex_col()
            .gap(px(space::SNUG))
            .child(fb_form_row("Target", locked_select_box("None".to_string())))
            .child(disabled_hint(
                "Automation lanes on existing tracks. Dedicated automation tracks are coming soon.",
            ))
            .into_any_element(),
        AddTrackKind::Folder => div()
            .flex()
            .flex_col()
            .gap(px(space::SNUG))
            .child(
                div()
                    .text_size(px(typography::DENSE_LABEL))
                    .text_color(Colors::text_secondary())
                    .child(
                        "Creates an arrangement folder. Drag track headers onto it to group them.",
                    ),
            )
            .into_any_element(),
        AddTrackKind::Bus | AddTrackKind::Return => div()
            .flex()
            .flex_col()
            .gap(px(space::SNUG))
            .child(fb_form_row(
                i18n.tr("add-track.routing.output"),
                select_box("Main".to_string()),
            ))
            .child(
                div()
                    .text_size(px(typography::DENSE_LABEL))
                    .text_color(Colors::text_faint())
                    .child(if state.selected_kind == AddTrackKind::Bus {
                        i18n.tr("add-track.hint.bus-route-selected")
                    } else {
                        i18n.tr("add-track.hint.return-output")
                    }),
            )
            .into_any_element(),
        _ => div()
            .text_size(px(typography::DENSE_LABEL))
            .text_color(Colors::text_faint())
            .child("This track type is not available in Native yet.")
            .into_any_element(),
    }
}

/// Compact DAW-style Add Tracks form (external window body).
#[allow(clippy::too_many_arguments)]
pub fn add_track_dialog_body(
    state: &AddTrackDialogState,
    track_name_input: &TextInputState,
    track_name_focused: bool,
    track_name_callbacks: TextInputCallbacks,
    track_name_ime_target: Entity<AddTrackWindow>,
    count_input: &TextInputState,
    count_focused: bool,
    count_callbacks: TextInputCallbacks,
    open_select: Option<AddTrackSelectId>,
    instrument_plugins: &[RegistryPlugin],
    instrument_plugin_query: &str,
    instrument_search_input: Option<&TextInputState>,
    instrument_search_focused: bool,
    instrument_search_callbacks: Option<TextInputCallbacks>,
    instrument_search_ime: Option<Entity<AddTrackWindow>>,
    midi_input_devices: &[String],
    color_ui: AddTrackColorUi,
    callbacks: AddTrackDialogCallbacks,
    i18n: I18n,
) -> gpui::Div {
    let confirm = callbacks.on_confirm.clone();
    let cancel = callbacks.on_close.clone();
    let valid = state.is_valid();
    let ok_label = if state.count == 1 {
        i18n.tr("add-track.button.add-one")
    } else {
        i18n.tr_vars(
            "add-track.button.add-many",
            &[("count", state.count.to_string())],
        )
    };

    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .bg(Colors::surface_base())
        .child(div().flex_shrink_0().child(dialog_intro(state, i18n)))
        .child(
            div()
                .flex_shrink_0()
                .child(type_tabs(state, &callbacks, i18n)),
        )
        .child(
            div()
                .id("add-track-body-scroll")
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .px(px(BODY_PAD_X))
                .pb(px(space::LOOSE))
                .gap(px(FORM_GAP))
                .child(form_panel(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(FORM_GAP))
                        .child(fb_form_row(
                            i18n.tr("add-track.field.name"),
                            text_field_with_callbacks_and_ime(
                                track_name_input,
                                track_name_focused,
                                track_name_callbacks,
                                track_name_ime_target,
                            ),
                        ))
                        .child(fb_form_row(
                            i18n.tr("add-track.field.count"),
                            count_stepper(
                                state,
                                count_input,
                                count_focused,
                                count_callbacks,
                                &callbacks,
                            ),
                        ))
                        .child(color_row(state, &callbacks, color_ui, i18n)),
                ))
                .child(form_panel(type_fields(
                    state,
                    &callbacks,
                    open_select,
                    instrument_plugins,
                    instrument_plugin_query,
                    instrument_search_input,
                    instrument_search_focused,
                    instrument_search_callbacks,
                    instrument_search_ime,
                    midi_input_devices,
                    i18n,
                ))),
        )
        .child(
            div()
                .flex_shrink_0()
                .flex()
                .flex_row()
                .items_center()
                // The footer is the action band, so its actions sit at the end
                // of it. It used to also carry a half-opacity "Load Track
                // Preset..." label on the left: not a button, not disabled, not
                // wired to anything — a promise the dialog could not keep. It
                // comes back when there is a preset store to open.
                .justify_end()
                .gap(px(space::BASE))
                .h(px(FOOTER_HEIGHT))
                .px(px(BODY_PAD_X))
                .border_t(px(1.0))
                .border_color(Colors::border_subtle())
                .bg(Colors::surface_titlebar())
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(space::BASE))
                        .child(fb_button(
                            "add-track-cancel",
                            i18n.tr("add-track.button.cancel"),
                            FbButtonKind::Default,
                            true,
                            move |_, w, cx| cancel(&(), w, cx),
                        ))
                        .child(fb_button(
                            "add-track-ok",
                            ok_label,
                            FbButtonKind::Primary,
                            valid,
                            move |_, w, cx| {
                                if valid {
                                    confirm(&(), w, cx);
                                }
                            },
                        )),
                ),
        )
}
pub const ADD_TRACK_WINDOW_WIDTH: f32 = 560.0;
pub const ADD_TRACK_WINDOW_HEIGHT: f32 = 520.0;
pub const ADD_TRACK_WINDOW_MIN_WIDTH: f32 = 480.0;
pub const ADD_TRACK_WINDOW_MIN_HEIGHT: f32 = 500.0;

/// Text field inside the dialog that an open context menu belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AddTrackTextField {
    Name,
    Count,
    InstrumentSearch,
}

/// Open text-input context menu: which field, and where it was summoned.
#[derive(Debug, Clone, Copy)]
struct AddTrackTextMenu {
    field: AddTrackTextField,
    x: f32,
    y: f32,
}

pub struct AddTrackWindow {
    pub state: AddTrackDialogState,
    language: String,
    track_name_input: TextInputState,
    count_input: TextInputState,
    count_editing: bool,
    /// Anchor for an in-flight Count scrub drag (`None` when not dragging).
    count_drag: Option<CountDragState>,
    color_picker: ColorPickerState,
    open_select: Option<AddTrackSelectId>,
    /// Cut/Copy/Paste menu for this window's text fields.
    ///
    /// The studio shell renders one of these from its own overlay state, but
    /// this dialog is a separate window, so it has to own and draw its own —
    /// which is why every field here shipped with `on_context_menu: None` and
    /// no right-click menu at all.
    text_context_menu: Option<AddTrackTextMenu>,
    instrument_plugin_query: String,
    instrument_search_input: TextInputState,
    instrument_plugins: Vec<RegistryPlugin>,
    /// Real detected MIDI input device names (Preferences โ’ MIDI enabled
    /// inputs), refreshed whenever the dialog opens or devices change.
    midi_input_devices: Vec<String>,
    focus_handle: FocusHandle,
    /// Called when the user confirms (creates tracks).
    on_confirm_request: Arc<dyn Fn(AddTrackDialogState, String, &mut App) + 'static>,
}

impl AddTrackWindow {
    fn text_input_for(&self, field: AddTrackTextField) -> &TextInputState {
        match field {
            AddTrackTextField::Name => &self.track_name_input,
            AddTrackTextField::Count => &self.count_input,
            AddTrackTextField::InstrumentSearch => &self.instrument_search_input,
        }
    }

    fn text_input_for_mut(&mut self, field: AddTrackTextField) -> &mut TextInputState {
        match field {
            AddTrackTextField::Name => &mut self.track_name_input,
            AddTrackTextField::Count => &mut self.count_input,
            AddTrackTextField::InstrumentSearch => &mut self.instrument_search_input,
        }
    }

    /// Push an edited field's text back into the state the dialog acts on.
    /// Cut/Paste change the buffer without going through the key handler, so
    /// without this the menu would edit the visible text and nothing else.
    fn sync_text_field(&mut self, field: AddTrackTextField) {
        match field {
            AddTrackTextField::Name => {
                self.state.track_name = self.track_name_input.value.clone();
            }
            AddTrackTextField::InstrumentSearch => {
                self.instrument_plugin_query = self.instrument_search_input.value.clone();
            }
            // Count deliberately does not commit here. Typing into it does not
            // commit either — the value lands on Enter or on blur — so
            // committing on paste would make the menu behave unlike the
            // keyboard and could close the editor mid-edit.
            AddTrackTextField::Count => {}
        }
    }

    pub fn new(
        initial_state: AddTrackDialogState,
        language: impl Into<String>,
        instrument_plugins: Vec<RegistryPlugin>,
        midi_input_devices: Vec<String>,
        on_confirm_request: Arc<dyn Fn(AddTrackDialogState, String, &mut App) + 'static>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut track_name_input = TextInputState::new("add-track-window-name", cx.focus_handle())
            .with_accessible_label("Track name");
        track_name_input.set_value(initial_state.track_name.clone());
        track_name_input.select_all();
        let mut count_input = TextInputState::new("add-track-window-count", cx.focus_handle())
            .with_accessible_label("Number of tracks")
            .with_ascii_charset("0123456789");
        count_input.set_value(initial_state.count.to_string());
        let color_picker = ColorPickerState::new(
            "add-track-hex",
            cx.focus_handle(),
            color_picker_value_for(&initial_state),
            initial_state.selected_color(),
            crate::color::load_recent_colors(),
        );
        let mut instrument_search_input =
            TextInputState::new("add-track-instrument-search", cx.focus_handle())
                .with_placeholder("Search instruments...")
                .blur_on_outside_click(true);
        instrument_search_input.set_value("");
        Self {
            state: initial_state,
            language: language.into(),
            track_name_input,
            count_input,
            count_editing: false,
            count_drag: None,
            color_picker,
            open_select: None,
            text_context_menu: None,
            instrument_plugin_query: String::new(),
            instrument_search_input,
            instrument_plugins,
            midi_input_devices,
            focus_handle: cx.focus_handle(),
            on_confirm_request,
        }
    }

    pub fn set_context(
        &mut self,
        kind: AddTrackKind,
        track_count: usize,
        has_master: bool,
        default_monitor_mode: &'static str,
    ) {
        let mut dialog = AddTrackDialogState::open_for_with_monitor(
            track_count,
            has_master,
            default_monitor_mode,
        );
        dialog.set_kind(kind);
        let i18n = I18n::new(&self.language);
        dialog.track_name = format!("{} {}", i18n.tr(kind.label_key()), dialog.next_number);
        dialog.instrument_mode = InstrumentMode::Vsti;
        dialog.instrument_plugin_id = None;
        dialog.instrument_plugin_name = None;
        dialog.solfege_model_path =
            crate::solfege::default_model_path().map(|path| path.to_string_lossy().into_owned());
        add_track_debug(&format!(
            "dialog open kind={} count={}",
            kind.tab_label(),
            dialog.count
        ));
        self.track_name_input.set_value(dialog.track_name.clone());
        self.track_name_input.select_all();
        self.count_input.set_value(dialog.count.to_string());
        self.count_editing = false;
        self.open_select = None;
        self.instrument_plugin_query.clear();
        self.instrument_search_input.set_value("");
        self.color_picker
            .reset(color_picker_value_for(&dialog), dialog.selected_color());
        self.state = dialog;
    }

    pub fn set_instrument_plugins(&mut self, instrument_plugins: Vec<RegistryPlugin>) {
        self.instrument_plugins = instrument_plugins;
    }

    /// Refresh the real MIDI input device list rendered in the Instrument/MIDI
    /// routing selects. Called whenever the dialog opens/reactivates and on
    /// Preferences MIDI device changes, mirroring `set_instrument_plugins`.
    pub fn set_midi_input_devices(&mut self, midi_input_devices: Vec<String>) {
        self.midi_input_devices = midi_input_devices;
    }

    /// Refresh Bus/Return destinations for the Output select. Called whenever
    /// the dialog opens/reactivates so newly created buses appear immediately.
    pub fn set_audio_output_targets(&mut self, audio_output_targets: Vec<(String, String)>) {
        self.state.audio_output_targets = audio_output_targets;
        if self.state.output_label != "Main"
            && self.state.output_label != "None"
            && !self
                .state
                .audio_output_targets
                .iter()
                .any(|(_, name)| self.state.output_label == format!("Bus - {name}"))
        {
            self.state.output_label = "Main".to_string();
        }
    }

    /// Refresh the logical Input Audio Connections offered by the Input select.
    /// Called whenever the dialog opens or reactivates, so buses created in the
    /// Audio Connections window appear without reopening the dialog.
    ///
    /// A selection whose bus disappeared falls back to No Input — never to
    /// another connection.
    pub fn set_audio_input_choices(&mut self, audio_input_choices: Vec<AddTrackInputChoice>) {
        self.state.audio_input_choices = audio_input_choices;
        if self.state.selected_kind == AddTrackKind::Audio
            && self.state.input_label != NO_AUDIO_INPUT_LABEL
            && self
                .state
                .audio_input_connection_for_label(&self.state.input_label.clone())
                .is_none()
        {
            self.state.input_label = NO_AUDIO_INPUT_LABEL.to_string();
        }
    }

    /// Push the picker's current selection back into the dialog state so the
    /// confirm path (and quick-swatch highlight) see the chosen color.
    fn sync_color_from_picker(&mut self) {
        self.state.auto_color = self.color_picker.auto;
        self.state.custom_color = if self.color_picker.auto {
            None
        } else {
            Some(self.color_picker.draft)
        };
    }

    /// Remember the chosen color, sync it into the dialog, and close the popover.
    fn close_color_picker(&mut self) {
        if self.color_picker.open {
            self.color_picker.remember_current();
            self.sync_color_from_picker();
            self.color_picker.close();
        }
    }

    fn set_count(&mut self, count: u32) {
        self.state.count = count.clamp(1, MAX_TRACK_COUNT);
        self.count_input.set_value(self.state.count.to_string());
    }

    fn begin_count_edit(&mut self) {
        self.count_input.set_value(self.state.count.to_string());
        self.count_input.select_all();
        self.count_editing = true;
    }

    fn commit_count_edit(&mut self) {
        let parsed = self.count_input.value.trim().parse::<u32>().ok();
        if let Some(count) = parsed {
            self.set_count(count);
        } else {
            self.count_input.set_value(self.state.count.to_string());
        }
        self.count_editing = false;
    }

    fn cancel_count_edit(&mut self) {
        self.count_input.set_value(self.state.count.to_string());
        self.count_editing = false;
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.count_editing {
            self.commit_count_edit();
        }
        if !self.state.is_valid() {
            return;
        }
        // Capture the picker's final color and remember it for next time.
        self.sync_color_from_picker();
        if !self.color_picker.auto {
            self.color_picker.remember_current();
        }
        self.state.track_name = self.track_name_input.value.clone();
        add_track_debug(&format!(
            "confirm kind={} count={}",
            self.state.selected_kind.tab_label(),
            self.state.count
        ));
        let req = self.state.clone();
        let name = self.track_name_input.value.clone();
        let cb = self.on_confirm_request.clone();
        cb(req, name, cx);
        window.remove_window();
    }

    fn clear_instrument_search(&mut self) {
        self.instrument_plugin_query.clear();
        self.instrument_search_input.set_value("");
    }

    fn sync_instrument_search_query(&mut self) {
        self.instrument_plugin_query = self.instrument_search_input.value.clone();
    }

    fn handle_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.open_select == Some(AddTrackSelectId::InstrumentPlugin) {
            if event.keystroke.key.as_str() == "escape" {
                self.open_select = None;
                self.clear_instrument_search();
                cx.notify();
                cx.stop_propagation();
                return;
            }
            if self.instrument_search_input.is_focused(window)
                || !self.track_name_input.is_focused(window)
            {
                let action = self
                    .instrument_search_input
                    .handle_key_with_clipboard(event, Some(cx));
                match action {
                    TextInputAction::Cancel => {
                        self.open_select = None;
                        self.clear_instrument_search();
                        cx.notify();
                    }
                    TextInputAction::Submit => {}
                    TextInputAction::Consumed | TextInputAction::Pass => {
                        self.sync_instrument_search_query();
                        cx.notify();
                    }
                }
                cx.stop_propagation();
                return;
            }
        }

        // Route keys to the color-picker hex field when it owns focus. Enter
        // commits the hex color, Escape closes the popover; everything else
        // edits the field with a live preview.
        if self.color_picker.open && self.color_picker.hex_input.is_focused(window) {
            let action = self
                .color_picker
                .hex_input
                .handle_key_with_clipboard(event, Some(cx));
            match action {
                TextInputAction::Submit => {
                    if self.color_picker.commit_hex().is_some() {
                        self.color_picker.remember_current();
                        self.sync_color_from_picker();
                    }
                    cx.notify();
                }
                TextInputAction::Cancel => {
                    self.close_color_picker();
                    cx.notify();
                }
                TextInputAction::Consumed | TextInputAction::Pass => {
                    self.color_picker.on_hex_changed();
                    self.sync_color_from_picker();
                    cx.notify();
                }
            }
            return;
        }

        if self.count_editing {
            let action = self.count_input.handle_key_with_clipboard(event, Some(cx));
            match action {
                TextInputAction::Submit => self.commit_count_edit(),
                TextInputAction::Cancel => self.cancel_count_edit(),
                TextInputAction::Consumed | TextInputAction::Pass => {}
            }
            cx.notify();
            return;
        }

        if self.track_name_input.is_focused(window) {
            let action = self.track_name_input.handle_key_ime(event, Some(cx));
            self.state.track_name = self.track_name_input.value.clone();
            match action {
                crate::components::text_input::TextInputAction::Submit => {
                    self.confirm(window, cx);
                }
                crate::components::text_input::TextInputAction::Cancel => window.remove_window(),
                crate::components::text_input::TextInputAction::Consumed
                | crate::components::text_input::TextInputAction::Pass => cx.notify(),
            }
            return;
        }

        match event.keystroke.key.as_str() {
            "escape" => {
                if self.color_picker.open {
                    self.close_color_picker();
                    cx.notify();
                } else if self.open_select.take().is_some() {
                    cx.notify();
                } else {
                    window.remove_window();
                }
            }
            "enter" | "numpad_enter" => self.confirm(window, cx),
            _ => {}
        }
    }
}

impl EntityInputHandler for AddTrackWindow {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        if self.instrument_search_input.is_focused(window) {
            self.instrument_search_input
                .text_for_utf16_range(range, actual_range)
        } else {
            self.track_name_input
                .text_for_utf16_range(range, actual_range)
        }
    }

    fn selected_text_range(
        &mut self,
        ignore_disabled_input: bool,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        if self.instrument_search_input.is_focused(window) {
            self.instrument_search_input
                .selected_text_range_utf16(ignore_disabled_input)
        } else {
            self.track_name_input
                .selected_text_range_utf16(ignore_disabled_input)
        }
    }

    fn marked_text_range(
        &self,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        if self.instrument_search_input.is_focused(window) {
            self.instrument_search_input.marked_text_range_utf16()
        } else {
            self.track_name_input.marked_text_range_utf16()
        }
    }

    fn unmark_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.instrument_search_input.is_focused(window) {
            self.instrument_search_input.unmark_text();
            self.sync_instrument_search_query();
        } else {
            self.track_name_input.unmark_text();
        }
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.instrument_search_input.is_focused(window)
            || self.open_select == Some(AddTrackSelectId::InstrumentPlugin)
                && !self.track_name_input.is_focused(window)
        {
            self.instrument_search_input
                .replace_text_in_utf16_range(range, text);
            self.sync_instrument_search_query();
        } else {
            self.track_name_input
                .replace_text_in_utf16_range(range, text);
            self.state.track_name = self.track_name_input.value.clone();
        }
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.instrument_search_input.is_focused(window)
            || self.open_select == Some(AddTrackSelectId::InstrumentPlugin)
                && !self.track_name_input.is_focused(window)
        {
            self.instrument_search_input
                .replace_and_mark_text_in_utf16_range(range, new_text, new_selected_range);
            self.sync_instrument_search_query();
        } else {
            self.track_name_input.replace_and_mark_text_in_utf16_range(
                range,
                new_text,
                new_selected_range,
            );
            self.state.track_name = self.track_name_input.value.clone();
        }
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        if self.instrument_search_input.is_focused(window) {
            self.instrument_search_input
                .bounds_for_utf16_range(range_utf16, bounds)
        } else {
            self.track_name_input
                .bounds_for_utf16_range(range_utf16, bounds)
        }
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

impl Render for AddTrackWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let i18n = I18n::new(&self.language);
        let target = cx.entity().clone();
        let search_focused = self.track_name_input.is_focused(window) && !self.count_editing;
        let instrument_search_focused = self.instrument_search_input.is_focused(window);
        let count_focused = self.count_editing;
        // Snapshot the open select for this frame. The dismiss backdrop (below)
        // sits above the form and resets `open_select` first on a click, so the
        // trigger must decide open-vs-close against the frame it was drawn for โ€”
        // otherwise re-clicking the active trigger would reopen it.
        let open_select_at_render = self.open_select;

        let callbacks = AddTrackDialogCallbacks {
            on_close: Arc::new(|_: &(), window: &mut Window, _cx: &mut App| window.remove_window()),
            on_confirm: Arc::new({
                let target = target.clone();
                move |_: &(), window, cx| {
                    let _ = target.update(cx, |this, cx| this.confirm(window, cx));
                }
            }),
            on_select_kind: Arc::new({
                let target = target.clone();
                move |kind: &AddTrackKind, _w, cx| {
                    let kind = *kind;
                    let _ = target.update(cx, |this, cx| {
                        this.state.set_kind(kind);
                        let i18n = I18n::new(&this.language);
                        this.state.track_name =
                            format!("{} {}", i18n.tr(kind.label_key()), this.state.next_number);
                        this.open_select = None;
                        this.track_name_input
                            .set_value(this.state.track_name.clone());
                        this.track_name_input.select_all();
                        add_track_debug(&format!("selected kind={}", kind.tab_label()));
                        cx.notify();
                    });
                }
            }),
            on_count_delta: Arc::new({
                let target = target.clone();
                move |delta: &i32, _w, cx| {
                    let delta = *delta;
                    let _ = target.update(cx, |this, cx| {
                        let current = this.state.count as i32;
                        let next = (current + delta).clamp(1, MAX_TRACK_COUNT as i32) as u32;
                        this.set_count(next);
                        this.count_editing = false;
                        this.count_drag = None;
                        cx.notify();
                    });
                }
            }),
            on_count_drag: Arc::new({
                let target = target.clone();
                move |sample: &CountDragSample, _w, cx| {
                    let sample = *sample;
                    let _ = target.update(cx, |this, cx| {
                        // Seed the anchor on the first sample of a new drag so the
                        // value is computed absolutely from the drag origin (no
                        // accumulation drift); later samples reuse it.
                        let anchor = match this.count_drag {
                            Some(a) if a.drag_id == sample.drag_id => a,
                            _ => {
                                let a = CountDragState {
                                    drag_id: sample.drag_id,
                                    start_count: sample.start_count,
                                    start_y: sample.cur_y,
                                };
                                this.count_drag = Some(a);
                                a
                            }
                        };
                        // Drag up increases, down decreases.
                        let steps = ((anchor.start_y - sample.cur_y) / COUNT_DRAG_PX_PER_STEP)
                            .round() as i32;
                        let next = (anchor.start_count as i32 + steps)
                            .clamp(1, MAX_TRACK_COUNT as i32)
                            as u32;
                        this.count_editing = false;
                        this.set_count(next);
                        cx.notify();
                    });
                }
            }),
            on_count_begin_edit: Arc::new({
                let target = target.clone();
                move |_: &(), _w, cx| {
                    let _ = target.update(cx, |this, cx| {
                        this.count_drag = None;
                        this.begin_count_edit();
                        cx.notify();
                    });
                }
            }),
            on_audio_format: Arc::new({
                let target = target.clone();
                move |format: &AudioFormat, _w, cx| {
                    let format = *format;
                    let _ = target.update(cx, |this, cx| {
                        this.state.set_audio_format(format);
                        this.open_select = None;
                        cx.notify();
                    });
                }
            }),
            on_color_index: Arc::new({
                let target = target.clone();
                move |index: &u32, _w, cx| {
                    let index = *index as usize;
                    let _ = target.update(cx, |this, cx| {
                        this.state.color_index = index;
                        this.state.auto_color = false;
                        cx.notify();
                    });
                }
            }),
            on_auto_color: Arc::new({
                let target = target.clone();
                move |on: &bool, _w, cx| {
                    let on = *on;
                    let _ = target.update(cx, |this, cx| {
                        this.color_picker.set_auto(on);
                        this.sync_color_from_picker();
                        cx.notify();
                    });
                }
            }),
            on_ascending_input: Arc::new({
                let target = target.clone();
                move |on: &bool, _w, cx| {
                    let on = *on;
                    let _ = target.update(cx, |this, cx| {
                        this.state.ascending_input = on;
                        cx.notify();
                    });
                }
            }),
            on_ascending_output: Arc::new({
                let target = target.clone();
                move |on: &bool, _w, cx| {
                    let on = *on;
                    let _ = target.update(cx, |this, cx| {
                        this.state.ascending_output = on;
                        cx.notify();
                    });
                }
            }),
            on_pack_folder: Arc::new({
                let target = target.clone();
                move |on: &bool, _w, cx| {
                    let on = *on;
                    let _ = target.update(cx, |this, cx| {
                        this.state.pack_folder = on;
                        cx.notify();
                    });
                }
            }),
            on_arm: Arc::new({
                let target = target.clone();
                move |armed: &bool, _w, cx| {
                    let armed = *armed;
                    let _ = target.update(cx, |this, cx| {
                        this.state.arm_track = armed;
                        cx.notify();
                    });
                }
            }),
            on_monitor: Arc::new({
                let target = target.clone();
                move |mode: &String, _w, cx| {
                    let mode = match mode.as_str() {
                        "auto" => "auto",
                        "in" => "in",
                        _ => "off",
                    };
                    let _ = target.update(cx, |this, cx| {
                        this.state.monitor_mode = mode;
                        cx.notify();
                    });
                }
            }),
            on_toggle_select: Arc::new({
                let target = target.clone();
                move |select_id: &AddTrackSelectId, window, cx| {
                    let select_id = *select_id;
                    let _ = target.update(cx, |this, cx| {
                        let opening = open_select_at_render != Some(select_id);
                        this.open_select = if opening { Some(select_id) } else { None };
                        if select_id == AddTrackSelectId::InstrumentPlugin {
                            this.clear_instrument_search();
                            if opening {
                                this.instrument_search_input.focus_handle.focus(window, cx);
                            }
                        }
                        if crate::ui_debug_enabled() {
                            match this.open_select {
                                Some(open) => {
                                    eprintln!("[ui-select] open id={open:?}")
                                }
                                None => {
                                    eprintln!("[ui-select] close id={select_id:?} reason=toggle")
                                }
                            }
                        }
                        cx.notify();
                    });
                }
            }),
            on_instrument_mode: Arc::new({
                let target = target.clone();
                move |mode: &InstrumentMode, _w, cx| {
                    let mode = *mode;
                    let _ = target.update(cx, |this, cx| {
                        this.state.instrument_mode = mode;
                        match mode {
                            InstrumentMode::Vsti => {
                                this.state.solfege_model_path = None;
                            }
                            InstrumentMode::SoundfontPlayer => {
                                this.state.instrument_plugin_id = None;
                                this.state.instrument_plugin_name = None;
                                this.state.solfege_model_path = None;
                            }
                            InstrumentMode::SolfegeEngine => {
                                this.state.instrument_plugin_id = None;
                                this.state.instrument_plugin_name = None;
                                if this.state.solfege_model_path.is_none() {
                                    this.state.solfege_model_path =
                                        crate::solfege::default_model_path()
                                            .map(|path| path.to_string_lossy().into_owned());
                                }
                            }
                        }
                        this.open_select = None;
                        add_track_debug(&format!("instrument mode={}", mode.label()));
                        cx.notify();
                    });
                }
            }),
            on_instrument_plugin: Arc::new({
                let target = target.clone();
                move |value: &String, _w, cx| {
                    let value = value.clone();
                    let _ = target.update(cx, |this, cx| {
                        if value.is_empty() {
                            this.state.instrument_plugin_id = None;
                            this.state.instrument_plugin_name = None;
                        } else {
                            let selected = find_instrument_plugin(&this.instrument_plugins, &value);
                            let plugin_id = selected
                                .map(|plugin| plugin.id.clone())
                                .unwrap_or_else(|| value.clone());
                            let plugin_name = selected
                                .map(|plugin| plugin.name.clone())
                                .unwrap_or_else(|| value.clone());
                            this.state.instrument_plugin_id = Some(plugin_id);
                            this.state.instrument_plugin_name = Some(plugin_name.clone());
                            this.state.track_name = plugin_name;
                            this.track_name_input
                                .set_value(this.state.track_name.clone());
                            this.track_name_input.select_all();
                        }
                        this.open_select = None;
                        this.clear_instrument_search();
                        cx.notify();
                    });
                }
            }),
            on_solfege_model: Arc::new({
                let target = target.clone();
                move |value: &String, _w, cx| {
                    let value = value.clone();
                    let _ = target.update(cx, |this, cx| {
                        this.state.solfege_model_path = (!value.is_empty()).then_some(value);
                        this.open_select = None;
                        cx.notify();
                    });
                }
            }),
            on_input_label: Arc::new({
                let target = target.clone();
                move |value: &String, _w, cx| {
                    let value = value.clone();
                    let _ = target.update(cx, |this, cx| {
                        this.state.input_label = value;
                        this.open_select = None;
                        cx.notify();
                    });
                }
            }),
            on_output_label: Arc::new({
                let target = target.clone();
                move |value: &String, _w, cx| {
                    let value = value.clone();
                    let _ = target.update(cx, |this, cx| {
                        this.state.output_label = value;
                        this.open_select = None;
                        cx.notify();
                    });
                }
            }),
            on_midi_channel_label: Arc::new({
                let target = target.clone();
                move |value: &String, _w, cx| {
                    let value = value.clone();
                    let _ = target.update(cx, |this, cx| {
                        this.state.midi_channel_label = value;
                        this.open_select = None;
                        cx.notify();
                    });
                }
            }),
        };

        // Click-catcher that dismisses the open Select when the user clicks
        // anywhere outside its (deferred) menu. Rendered at the dialog root so
        // it spans the whole window; the menu paints above it and occludes its
        // own clicks. See `select_dismiss_backdrop`.
        let dismiss_backdrop = self.open_select.is_some().then(|| {
            let target = target.clone();
            let on_dismiss: VoidCb = Arc::new(move |_: &(), _w: &mut Window, cx: &mut App| {
                let _ = target.update(cx, |this, cx| {
                    if this.open_select.take().is_some() {
                        this.clear_instrument_search();
                        if crate::ui_debug_enabled() {
                            eprintln!("[ui-select] close reason=click_outside");
                        }
                        cx.notify();
                    }
                });
            });
            select_dismiss_backdrop(on_dismiss)
        });

        // Color picker callbacks โ€” mutate the host-owned `ColorPickerState` and
        // mirror the result into the dialog state so the confirm path sees it.
        let picker_callbacks = ColorPickerCallbacks {
            on_toggle: Arc::new({
                let target = target.clone();
                move |_w: &mut Window, cx: &mut App| {
                    let _ = target.update(cx, |this, cx| {
                        if this.color_picker.open {
                            this.close_color_picker();
                        } else {
                            this.open_select = None;
                            this.color_picker.open();
                        }
                        cx.notify();
                    });
                }
            }),
            on_close: Arc::new({
                let target = target.clone();
                move |_w: &mut Window, cx: &mut App| {
                    let _ = target.update(cx, |this, cx| {
                        this.close_color_picker();
                        cx.notify();
                    });
                }
            }),
            on_pick: Arc::new({
                let target = target.clone();
                move |color: gpui::Rgba, _w: &mut Window, cx: &mut App| {
                    let _ = target.update(cx, |this, cx| {
                        this.color_picker.set_color(color);
                        this.sync_color_from_picker();
                        cx.notify();
                    });
                }
            }),
            on_hue: Arc::new({
                let target = target.clone();
                move |hue: f32, _w: &mut Window, cx: &mut App| {
                    let _ = target.update(cx, |this, cx| {
                        this.color_picker.set_hue(hue);
                        this.sync_color_from_picker();
                        cx.notify();
                    });
                }
            }),
            on_sv: Arc::new({
                let target = target.clone();
                move |saturation: f32, value: f32, _w: &mut Window, cx: &mut App| {
                    let _ = target.update(cx, |this, cx| {
                        this.color_picker.set_saturation_value(saturation, value);
                        this.sync_color_from_picker();
                        cx.notify();
                    });
                }
            }),
            on_auto: Arc::new({
                let target = target.clone();
                move |on: bool, _w: &mut Window, cx: &mut App| {
                    let _ = target.update(cx, |this, cx| {
                        this.color_picker.set_auto(on);
                        this.sync_color_from_picker();
                        cx.notify();
                    });
                }
            }),
        };

        // Click-outside dismissal for the color popover. Stops propagation so a
        // click on the (occluded) trigger does not immediately reopen it.
        let color_backdrop = self.color_picker.open.then(|| {
            let target = target.clone();
            div()
                .absolute()
                .inset_0()
                .id("color-picker-dismiss")
                .on_mouse_down(gpui::MouseButton::Left, move |_, _w, cx| {
                    cx.stop_propagation();
                    let _ = target.update(cx, |this, cx| {
                        this.close_color_picker();
                        cx.notify();
                    });
                })
        });

        let color_ui = AddTrackColorUi {
            picker: &self.color_picker,
            presets: (0..Colors::TRACK_COLORS.len()).map(track_color).collect(),
            hex_focused: self.color_picker.hex_input.is_focused(window),
            hex_callbacks: bind_mouse_selection(cx.entity().clone(), |this| {
                &mut this.color_picker.hex_input
            }),
            callbacks: picker_callbacks,
        };
        let name_callbacks = TextInputCallbacks {
            on_mouse_down_out: None,
            on_context_command: None,
            on_context_menu: Some(Arc::new({
                let target = target.clone();
                move |pos: &(f32, f32), _w, cx| {
                    let (x, y) = *pos;
                    let _ = target.update(cx, |this, cx| {
                        this.text_context_menu = Some(AddTrackTextMenu {
                            field: AddTrackTextField::Name,
                            x,
                            y,
                        });
                        cx.notify();
                    });
                }
            })),
            on_mouse: Some(Arc::new({
                let target = target.clone();
                move |event: &TextInputMouseEvent, _w, cx| {
                    let phase = event.phase;
                    let index = event.index;
                    let extend = event.extend;
                    let _ = target.update(cx, |this, cx| {
                        this.count_editing = false;
                        match phase {
                            TextInputMousePhase::Down => {
                                this.track_name_input.handle_mouse_down(index, extend)
                            }
                            TextInputMousePhase::Drag => {
                                this.track_name_input.handle_mouse_drag(index)
                            }
                            TextInputMousePhase::Up => this.track_name_input.handle_mouse_up(),
                        }
                        cx.notify();
                    });
                }
            })),
        };
        let count_callbacks = TextInputCallbacks {
            on_mouse_down_out: None,
            on_context_command: None,
            on_context_menu: Some(Arc::new({
                let target = target.clone();
                move |pos: &(f32, f32), _w, cx| {
                    let (x, y) = *pos;
                    let _ = target.update(cx, |this, cx| {
                        this.text_context_menu = Some(AddTrackTextMenu {
                            field: AddTrackTextField::Count,
                            x,
                            y,
                        });
                        cx.notify();
                    });
                }
            })),
            on_mouse: Some(Arc::new({
                let target = target.clone();
                move |event: &TextInputMouseEvent, _w, cx| {
                    let phase = event.phase;
                    let index = event.index;
                    let extend = event.extend;
                    let _ = target.update(cx, |this, cx| {
                        if !this.count_editing {
                            this.begin_count_edit();
                        }
                        match phase {
                            TextInputMousePhase::Down => {
                                this.count_input.handle_mouse_down(index, extend)
                            }
                            TextInputMousePhase::Drag => this.count_input.handle_mouse_drag(index),
                            TextInputMousePhase::Up => this.count_input.handle_mouse_up(),
                        }
                        cx.notify();
                    });
                }
            })),
        };

        let instrument_search_callbacks = TextInputCallbacks {
            on_mouse_down_out: None,
            on_context_command: None,
            on_context_menu: Some(Arc::new({
                let target = target.clone();
                move |pos: &(f32, f32), _w, cx| {
                    let (x, y) = *pos;
                    let _ = target.update(cx, |this, cx| {
                        this.text_context_menu = Some(AddTrackTextMenu {
                            field: AddTrackTextField::InstrumentSearch,
                            x,
                            y,
                        });
                        cx.notify();
                    });
                }
            })),
            on_mouse: Some(Arc::new({
                let target = target.clone();
                move |event: &TextInputMouseEvent, _w, cx| {
                    let phase = event.phase;
                    let index = event.index;
                    let extend = event.extend;
                    let _ = target.update(cx, |this, cx| {
                        match phase {
                            TextInputMousePhase::Down => this
                                .instrument_search_input
                                .handle_mouse_down(index, extend),
                            TextInputMousePhase::Drag => {
                                this.instrument_search_input.handle_mouse_drag(index)
                            }
                            TextInputMousePhase::Up => {
                                this.instrument_search_input.handle_mouse_up()
                            }
                        }
                        this.sync_instrument_search_query();
                        cx.notify();
                    });
                }
            })),
        };

        // Cut/Copy/Paste for this window's text fields. Entries and enablement
        // come from the same `text_input_context_entries` the studio shell uses,
        // so the two menus cannot drift apart.
        let text_context_overlay = self.text_context_menu.map(|menu| {
            let clipboard_has_text = cx
                .read_from_clipboard()
                .and_then(|item| item.text())
                .is_some_and(|text| !text.is_empty());
            let entries =
                text_input_context_entries(self.text_input_for(menu.field), clipboard_has_text);
            let command_target = cx.entity().clone();
            let close_target = cx.entity().clone();
            crate::components::context_menu::context_menu_overlay(
                entries,
                menu.x,
                menu.y,
                ADD_TRACK_WINDOW_WIDTH,
                ADD_TRACK_WINDOW_HEIGHT,
                Arc::new(move |command: &String, _window, cx| {
                    let command = command.clone();
                    let _ = command_target.update(cx, |this, cx| {
                        if let Some(menu) = this.text_context_menu {
                            let applied = this
                                .text_input_for_mut(menu.field)
                                .apply_context_command(&command, cx);
                            if applied {
                                this.sync_text_field(menu.field);
                            }
                        }
                        this.text_context_menu = None;
                        cx.notify();
                    });
                }),
                Arc::new(move |_: &(), _window, cx| {
                    let _ = close_target.update(cx, |this, cx| {
                        this.text_context_menu = None;
                        cx.notify();
                    });
                }),
            )
        });

        div()
            .flex()
            .flex_col()
            .size_full()
            .font(theme::ui_font())
            .bg(Colors::surface_base())
            .overflow_hidden()
            .capture_key_down({
                let target = target.clone();
                move |event, window, cx| {
                    let _ = target.update(cx, |this, cx| this.handle_key(event, window, cx));
                }
            })
            .child(div().w(px(0.0)).h(px(0.0)).track_focus(&self.focus_handle))
            .child(external_window_titlebar_with_icon(
                Some(assets::ICON_PLUS_PATH),
                i18n.tr("add-track.title"),
                "add-track-window-close",
                move |window, _cx| window.remove_window(),
            ))
            .child(add_track_dialog_body(
                &self.state,
                &self.track_name_input,
                search_focused,
                name_callbacks,
                cx.entity().clone(),
                &self.count_input,
                count_focused,
                count_callbacks,
                self.open_select,
                &self.instrument_plugins,
                &self.instrument_plugin_query,
                Some(&self.instrument_search_input),
                instrument_search_focused,
                Some(instrument_search_callbacks),
                Some(cx.entity().clone()),
                &self.midi_input_devices,
                color_ui,
                callbacks,
                i18n,
            ))
            .children(dismiss_backdrop)
            .children(color_backdrop)
            .children(text_context_overlay)
    }
}

pub fn open_add_track_window(
    owner_bounds: Option<Bounds<gpui::Pixels>>,
    kind: AddTrackKind,
    track_count: usize,
    has_master_track: bool,
    default_monitor_mode: &'static str,
    language: impl Into<String>,
    instrument_plugins: Vec<RegistryPlugin>,
    midi_input_devices: Vec<String>,
    audio_output_targets: Vec<(String, String)>,
    audio_input_choices: Vec<AddTrackInputChoice>,
    on_confirm_request: Arc<dyn Fn(AddTrackDialogState, String, &mut App) + 'static>,
    cx: &mut App,
) -> Result<WindowHandle<AddTrackWindow>, String> {
    let window_bounds = centered_window_bounds(
        owner_bounds,
        size(px(ADD_TRACK_WINDOW_WIDTH), px(ADD_TRACK_WINDOW_HEIGHT)),
        cx,
    );

    let language = language.into();
    let i18n = I18n::new(&language);
    let mut state = AddTrackDialogState::open_for_with_monitor(
        track_count,
        has_master_track,
        default_monitor_mode,
    );
    state.selected_kind = kind;
    state.input_label = kind.default_input().to_string();
    state.audio_output_targets = audio_output_targets;
    state.audio_input_choices = audio_input_choices;
    state.track_name = format!("{} {}", i18n.tr(kind.label_key()), state.next_number);
    add_track_debug(&format!(
        "open window kind={} track_count={}",
        kind.tab_label(),
        track_count
    ));

    let mut options = crate::platform_chrome::external_dialog_window_options_partial();
    options.window_bounds = Some(WindowBounds::Windowed(window_bounds));
    options.kind = WindowKind::Dialog;
    options.is_resizable = true;
    options.is_minimizable = false;
    options.window_background = WindowBackgroundAppearance::Transparent;
    options.window_min_size = Some(size(
        px(ADD_TRACK_WINDOW_MIN_WIDTH),
        px(ADD_TRACK_WINDOW_MIN_HEIGHT),
    ));
    apply_owner_display(&mut options, owner_bounds, cx);

    cx.open_window(options, |_window, cx| {
        cx.new(|cx| {
            AddTrackWindow::new(
                state,
                language,
                instrument_plugins,
                midi_input_devices,
                on_confirm_request,
                cx,
            )
        })
    })
    .map_err(|e| e.to_string())
}

fn valid_monitor_mode(mode: &'static str) -> &'static str {
    match mode {
        "auto" | "input" => mode,
        _ => "off",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mic_choice() -> AddTrackInputChoice {
        AddTrackInputChoice {
            label: "Microphone".to_string(),
            connection_id: crate::audio_connections::AudioConnectionId::from_stored("ac-mic"),
            channels: 1,
        }
    }

    fn stereo_choice() -> AddTrackInputChoice {
        AddTrackInputChoice {
            label: "Stereo Input".to_string(),
            connection_id: crate::audio_connections::AudioConnectionId::from_stored("ac-stereo"),
            channels: 2,
        }
    }

    /// An Audio track starts with No Input. Creating one must never open a
    /// capture device the user did not choose.
    #[test]
    fn a_new_audio_track_starts_with_no_input() {
        let state = AddTrackDialogState::open_for(0, false);
        assert_eq!(state.selected_kind, AddTrackKind::Audio);
        assert_eq!(state.input_label, NO_AUDIO_INPUT_LABEL);
    }

    #[test]
    fn only_connections_that_fit_the_format_are_offered() {
        let mut state = AddTrackDialogState::open_for(0, false);
        state.audio_input_choices = vec![mic_choice(), stereo_choice()];

        state.set_audio_format(AudioFormat::Stereo);
        let stereo: Vec<String> = state
            .audio_input_option_labels()
            .into_iter()
            .map(|option| option.id)
            .collect();
        assert_eq!(
            stereo,
            vec![
                NO_AUDIO_INPUT_LABEL.to_string(),
                "Microphone".to_string(),
                "Stereo Input".to_string(),
            ]
        );

        state.set_audio_format(AudioFormat::Mono);
        let mono: Vec<String> = state
            .audio_input_option_labels()
            .into_iter()
            .map(|option| option.id)
            .collect();
        assert_eq!(
            mono,
            vec![NO_AUDIO_INPUT_LABEL.to_string(), "Microphone".to_string()]
        );
    }

    /// Switching to Mono drops a stereo selection to No Input rather than
    /// silently substituting another bus.
    #[test]
    fn a_format_change_that_invalidates_the_selection_falls_back_to_no_input() {
        let mut state = AddTrackDialogState::open_for(0, false);
        state.audio_input_choices = vec![mic_choice(), stereo_choice()];

        state.set_audio_format(AudioFormat::Stereo);
        state.input_label = "Stereo Input".to_string();
        state.set_audio_format(AudioFormat::Mono);
        assert_eq!(state.input_label, NO_AUDIO_INPUT_LABEL);

        // A mono selection survives the same change.
        state.input_label = "Microphone".to_string();
        state.set_audio_format(AudioFormat::Stereo);
        assert_eq!(state.input_label, "Microphone");
    }

    #[test]
    fn kind_round_trip_restores_the_audio_default() {
        let mut state = AddTrackDialogState::open_for(0, false);
        state.audio_input_choices = vec![mic_choice()];
        state.set_audio_format(AudioFormat::Mono);
        state.input_label = "Microphone".to_string();

        state.set_kind(AddTrackKind::Midi);
        assert_eq!(state.input_label, "All MIDI Inputs");
        state.set_kind(AddTrackKind::Audio);

        assert_eq!(state.audio_format, AudioFormat::Mono);
        assert_eq!(state.input_label, NO_AUDIO_INPUT_LABEL);
    }

    /// The Video track is created by dropping a video onto the arrangement, not
    /// from this dialog. No tab may offer it, or a project could end up with an
    /// empty Video lane the drop path would then have to reuse or duplicate.
    #[test]
    fn no_tab_creates_a_video_track() {
        for kind in AddTrackKind::visible_tabs(AddTrackKind::Audio) {
            assert_ne!(kind.native_track_type(), Some(TrackType::Video));
        }
    }

    #[test]
    fn folder_tab_creates_a_native_group_container() {
        let mut state = AddTrackDialogState::open_for(0, false);
        state.set_kind(AddTrackKind::Folder);

        assert_eq!(
            AddTrackKind::Folder.native_track_type(),
            Some(TrackType::Group)
        );
        assert!(state.is_valid());
    }
}

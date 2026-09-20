//! Inspector panel — the main detail/edit surface for the current selection.
//!
//! The Inspector renders one of several "targets" derived fresh from the live
//! [`TimelineState`] each frame (see [`InspectorTarget`]). It never stores a
//! duplicate of track/clip state: the panel reads the same `TrackState` the
//! TrackHeader and Mixer read, so any edit made here is reflected everywhere.
//!
//! Phase A establishes the redesigned shell (section-based layout, richer
//! detail, project-summary empty state) and the target model. Editing controls
//! and their callbacks are layered on in later phases without changing this
//! file's read-render structure.

use std::sync::Arc;

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, svg, App, AppContext, InteractiveElement, IntoElement, MouseButton, ParentElement,
    StatefulInteractiveElement, Styled, Window,
};

use crate::assets;
use crate::audio_connections::AudioConnectionRegistry;
use crate::components::color_picker::{
    color_picker_field, default_presets, ColorPickerCallbacks, ColorPickerPlacement,
    ColorPickerState,
};
use crate::components::combo_box::{combo_box_string_menu, combo_box_trigger};
use crate::components::controls::{
    fb_button, fb_checkbox, fb_form_row, fb_shortcut_hint, FbButtonKind,
};
use crate::components::inspector::{
    inspector_checkbox as shared_inspector_checkbox, inspector_hint_text, inspector_mini_button,
    inspector_numeric_stepper, inspector_numeric_stepper_with_drag_callbacks,
    inspector_row as shared_inspector_row, inspector_section as shared_inspector_section,
    inspector_select, inspector_value, InspectorSelectOption,
};
use crate::components::inspector_kit;
use crate::components::reorder::drop_over_highlight;
use crate::components::slider::{bipolar_slider_with_drag_callbacks, slider_with_drag_callbacks};
use crate::components::solfege_editor::SolfegePitchSummary;
use crate::components::text_input::{
    text_field_with_callbacks, TextInputCallbacks, TextInputState,
};
use crate::components::timeline::timeline_state::{
    volume, vsti_output_bus_strip_indices, vsti_output_child_channels_for_bus_layout,
    AudioClipStretchState, ClipType, InsertLoadStatus, InsertSlotState, StretchAlgorithm,
    StretchMode, TrackAudioFormat, TrackMidiInputRouting, TrackOutputRouting, TrackState,
    TrackType,
};
use crate::i18n::I18n;
use crate::overlay::{inspector_combo_menu_position, OverlayAnchor};
use crate::solfege::{ModelLoadState, SolfegeModelInfo};
use crate::theme::{space, typography, Colors};
use sphere_midi_service::mpe::{MpeOutputMode, MpeTrackConfiguration};

type RoutingComboToggleCb =
    Arc<dyn Fn(InspectorRoutingCombo, Option<OverlayAnchor>, &mut Window, &mut App) + 'static>;

type StrCb = Arc<dyn Fn(&String, &mut Window, &mut App) + 'static>;
type StrF32Cb = Arc<dyn Fn(&(String, f32), &mut Window, &mut App) + 'static>;
type InputRoutingCb = Arc<
    dyn Fn(&(String, crate::input_routing::TrackInputSelection), &mut Window, &mut App) + 'static,
>;
type OutputRoutingCb = Arc<dyn Fn(&(String, TrackOutputRouting), &mut Window, &mut App) + 'static>;
type AudioFormatCb = Arc<dyn Fn(&(String, TrackAudioFormat), &mut Window, &mut App) + 'static>;
type MidiInputCb = Arc<dyn Fn(&(String, TrackMidiInputRouting), &mut Window, &mut App) + 'static>;
type MidiChannelCb = Arc<dyn Fn(&(String, Option<u8>), &mut Window, &mut App) + 'static>;
type MpeConfigurationCb =
    Arc<dyn Fn(&(String, MpeTrackConfiguration), &mut Window, &mut App) + 'static>;
type InsertPairCb = Arc<dyn Fn(&(String, String), &mut Window, &mut App) + 'static>;
type InsertOpenCb = Arc<dyn Fn(&(String, usize, String), &mut Window, &mut App) + 'static>;
type InsertMoveCb = Arc<dyn Fn(&(String, String, bool), &mut Window, &mut App) + 'static>;
type InsertPickerCb = Arc<dyn Fn(&(String, usize, bool), &mut Window, &mut App) + 'static>;
type InsertOutputChannelCb =
    Arc<dyn Fn(&(String, String, u8, bool), &mut Window, &mut App) + 'static>;
type ClipF32Cb = Arc<dyn Fn(&(String, f32), &mut Window, &mut App) + 'static>;
type ClipBoolCb = Arc<dyn Fn(&(String, bool), &mut Window, &mut App) + 'static>;
/// Apply a full replacement of a clip's stretch/pitch state. One callback drives
/// every stretch control; the inspector builds the mutated state and the layout
/// records it as a single undo entry (see `set_clip_stretch_cb`).
type ClipStretchCb = Arc<dyn Fn(&(String, AudioClipStretchState), &mut Window, &mut App) + 'static>;

/// Transient UI state for async clip tempo analysis (not persisted).
#[derive(Debug, Clone, Default)]
pub struct StretchTempoUiSnapshot {
    pub finding: bool,
    pub error: Option<String>,
    pub alternatives: Vec<f32>,
    pub confidence: Option<f32>,
    pub low_confidence: bool,
    pub suggested_bpm: Option<f32>,
}
/// Reorder an FX/insert slot via drag. `(track_id, dragged_insert_id,
/// insertion_index)` where `insertion_index` is the gap (0..=len) the dragged
/// slot should move into. Identity is the stable `plugin_instance_id`, never
/// the visual index. One completed drag = one undo entry (see
/// `reorder_insert_cb`).
type InsertReorderCb = Arc<dyn Fn(&(String, String, usize), &mut Window, &mut App) + 'static>;

/// Drag payload for FX/insert reorder. Carries the stable instance identity
/// plus a label rendered in the drag preview. Cloned into the GPUI drag view.
#[derive(Clone)]
pub struct FxSlotDrag {
    pub track_id: String,
    pub insert_id: String,
    pub display_name: String,
}

impl gpui::Render for FxSlotDrag {
    fn render(&mut self, _window: &mut Window, _cx: &mut gpui::Context<Self>) -> impl IntoElement {
        div()
            .px(px(8.0))
            .py(px(3.0))
            .rounded(px(crate::theme::radius::CONTROL))
            .bg(Colors::surface_raised())
            .border(px(1.0))
            .border_color(Colors::accent_primary())
            .text_size(px(11.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(Colors::text_primary())
            .child(self.display_name.clone())
    }
}

/// Open routing ComboBox in the Inspector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InspectorRoutingCombo {
    AudioFormat,
    AudioInput,
    AudioOutput,
    VstiOutputs,
    MidiInput,
    MidiChannel,
    MidiOut,
}

const ROUTING_COMBO_MENU_HEIGHT: f32 = 220.0;

/// Edit callbacks handed to the Inspector. Built by the layout
/// (`build_inspector_callbacks`) and dispatched to the shared `TimelineState`.
#[derive(Clone)]
pub struct InspectorCallbacks {
    pub on_volume: StrF32Cb,
    pub on_volume_drag_start: StrF32Cb,
    pub on_volume_drag_preview: StrF32Cb,
    pub on_volume_drag_commit: StrCb,
    /// Toggle whether Track Volume automation drives this track's effective
    /// volume (the `[A]` button beside the volume readout).
    pub on_toggle_volume_automation_read: StrCb,
    pub on_pan: StrF32Cb,
    pub on_pan_drag_start: StrCb,
    pub on_pan_drag_preview: StrF32Cb,
    pub on_pan_drag_commit: StrCb,
    pub on_toggle_mute: StrCb,
    pub on_toggle_solo: StrCb,
    pub on_toggle_arm: StrCb,
    pub on_toggle_input: StrCb,
    pub on_set_input_routing: InputRoutingCb,
    pub on_set_output_routing: OutputRoutingCb,
    pub on_set_audio_format: AudioFormatCb,
    pub on_set_midi_input: MidiInputCb,
    pub on_set_midi_channel: MidiChannelCb,
    pub on_set_mpe_configuration: MpeConfigurationCb,
    pub on_open_insert_picker: InsertPickerCb,
    pub on_remove_insert: InsertPairCb,
    pub on_toggle_insert_bypass: InsertPairCb,
    pub on_toggle_insert_enabled: InsertPairCb,
    pub on_toggle_insert_output_channel: InsertOutputChannelCb,
    pub on_move_insert: InsertMoveCb,
    /// Drag-reorder commit for an FX/insert slot (one undo entry per drag).
    pub on_reorder_insert: InsertReorderCb,
    pub on_open_insert_editor: InsertOpenCb,
    pub on_set_clip_start: ClipF32Cb,
    pub on_set_clip_length: ClipF32Cb,
    pub on_set_clip_gain: ClipF32Cb,
    pub on_set_clip_muted: ClipBoolCb,
    /// Apply a new stretch/pitch state to an audio clip (one undo entry).
    pub on_set_clip_stretch: ClipStretchCb,
    pub on_preview_clip_stretch: ClipStretchCb,
    pub on_begin_clip_property_drag: StrCb,
    pub on_commit_clip_property_drag: StrCb,
    pub on_preview_clip_start: ClipF32Cb,
    pub on_preview_clip_length: ClipF32Cb,
    pub on_preview_clip_gain: ClipF32Cb,
    /// Analyze source audio and set `bpm_source` asynchronously.
    pub on_clip_stretch_auto_find_bpm: StrCb,
    /// Fit clip tempo to project BPM (auto-finds source BPM first if needed).
    pub on_clip_stretch_fit_project: StrCb,
    /// Append a warp marker at the current playhead on the given clip.
    pub on_clip_warp_add_at_playhead: StrCb,
    /// Remove all warp markers from the given clip.
    pub on_clip_warp_clear: StrCb,
    pub on_open_clip_bottom_editor: StrCb,
    pub on_open_clip_external_midi_editor: StrCb,
    /// Opens (or focuses) the built-in Soundfont Player MDI window. Only
    /// used by [`instrument_section`] when `track.builtin_soundfont_player`
    /// is set — the built-in player has no plugin insert to route through
    /// `on_open_insert_editor`.
    pub on_open_soundfont_player: StrCb,
    pub open_routing_combo: Option<InspectorRoutingCombo>,
    pub on_toggle_routing_combo: RoutingComboToggleCb,
}

/// Width of the docked Inspector column. Mirrors the constant in
/// `studio_render.rs` (`INSPECTOR_WIDTH`).
pub const INSPECTOR_WIDTH: f32 = 292.0;

/// `FUTUREBOARD_INSPECTOR_DEBUG=1` gates verbose Inspector edit logging.
pub fn inspector_debug_enabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| {
        std::env::var("FUTUREBOARD_INSPECTOR_DEBUG")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    })
}

/// Emit an Inspector debug line when `FUTUREBOARD_INSPECTOR_DEBUG=1`.
pub fn inspector_debug(message: &str) {
    if inspector_debug_enabled() {
        eprintln!("[inspector] {message}");
    }
}

/// Lightweight projection of the currently selected clip, built by the layout
/// from `TimelineState`. The inspector only needs a read-only summary.
pub struct SelectedClipSummary<'a> {
    pub clip_id: &'a str,
    pub track_id: &'a str,
    pub name: &'a str,
    pub start_beat: f32,
    pub duration_beats: f32,
    pub muted: bool,
    pub gain: f32,
    pub source_duration_seconds: Option<f64>,
    pub source_path: Option<&'a str>,
    pub note_count: Option<usize>,
    pub kind: &'static str,
    pub track_name: &'a str,
    /// Live stretch/pitch state of the selected clip (for the audio Inspector).
    pub stretch: &'a AudioClipStretchState,
    /// Current project tempo, shown as the read-only Tempo-Sync target.
    pub project_bpm: f64,
    /// Active arrangement time-selection duration in beats, when available.
    pub selection_duration_beats: Option<f32>,
}

/// What the Inspector is currently editing. Resolved fresh from the live
/// selection every render — the Inspector must never guess from stale state,
/// and if the referenced object has been deleted the resolver falls back to a
/// lower-priority target (ultimately [`InspectorTarget::None`]).
///
/// Selection priority (highest first):
/// 1. `MidiNotes` — resolved inside the Piano Roll window, not here.
/// 2. `AutomationPoints` — when points are selected on the focused lane.
/// 3. `Clip` — when a clip is selected.
/// 4. `PluginInsert` — when an insert slot is selected.
/// 5. `Track` — when only a track is selected.
/// 6. `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InspectorTarget {
    None,
    Track {
        track_id: String,
    },
    Clip {
        track_id: String,
        clip_id: String,
    },
    MidiNotes {
        track_id: String,
        clip_id: String,
        note_ids: Vec<u64>,
    },
    AutomationPoints {
        track_id: String,
        lane_id: String,
        point_ids: Vec<u64>,
    },
    PluginInsert {
        track_id: String,
        insert_id: String,
    },
}

/// Stable badge label for a resolved target, used in the Inspector header.
pub fn target_badge(target: &InspectorTarget, tracks: &[TrackState]) -> &'static str {
    match target {
        InspectorTarget::None => "",
        InspectorTarget::Track { track_id } => tracks
            .iter()
            .find(|t| &t.id == track_id)
            .map(|t| track_type_badge(t.track_type))
            .unwrap_or(""),
        InspectorTarget::Clip { .. } | InspectorTarget::MidiNotes { .. } => "Clip",
        InspectorTarget::AutomationPoints { .. } => "Automation",
        InspectorTarget::PluginInsert { .. } => "Plugin",
    }
}

fn track_type_badge(t: TrackType) -> &'static str {
    match t {
        TrackType::Audio => "Audio Track",
        TrackType::Midi => "MIDI Track",
        TrackType::Instrument => "Instrument Track",
        TrackType::Bus => "Bus",
        TrackType::Return => "Return",
        TrackType::Group => "Group",
        TrackType::Master => "Master",
        TrackType::Video => "Video Track",
    }
}

fn track_type_label(i18n: I18n, t: TrackType) -> String {
    match t {
        TrackType::Audio => i18n.tr("inspector.track-type.audio"),
        TrackType::Midi => i18n.tr("inspector.track-type.midi"),
        TrackType::Instrument => i18n.tr("inspector.track-type.instrument"),
        TrackType::Bus => i18n.tr("inspector.track-type.bus"),
        TrackType::Return => i18n.tr("inspector.track-type.return"),
        TrackType::Group => "Group".to_string(),
        TrackType::Master => i18n.tr("inspector.track-type.master"),
        TrackType::Video => i18n.tr("inspector.track-type.video"),
    }
}

/// Semantic hue per track type — drives the inspector title rail/badge so the
/// header reads by type at a glance, independent of the per-track identity color
/// (which the user still edits via the Color row).
fn track_type_color(t: TrackType) -> gpui::Rgba {
    match t {
        TrackType::Audio => Colors::accent_cyan(),
        TrackType::Instrument => Colors::accent_green(),
        TrackType::Midi => Colors::track_midi(),
        TrackType::Bus => Colors::track_bus(),
        TrackType::Return => Colors::track_return(),
        TrackType::Group => Colors::accent_primary(),
        TrackType::Master => Colors::track_master(),
        TrackType::Video => Colors::accent_purple(),
    }
}

/// Legacy entry point — kept so any existing call sites still compile. Returns
/// an empty placeholder identical to the pre-state version.
pub fn right_panel() -> impl IntoElement {
    let i18n = I18n::new("en");
    inspector_shell(false, i18n).child(no_selection(0, i18n))
}

/// Inspector driven by the live selection. Renders one of:
/// 1. Clip details when a clip is selected.
/// 2. Track details when only a track is selected.
/// 3. "No Selection" placeholder (with a small project summary) otherwise.
pub fn inspector_panel<'a>(
    tracks: &'a [TrackState],
    connections: &'a AudioConnectionRegistry,
    selected_track_id: Option<&str>,
    selected_clip_id: Option<&str>,
    clip_summary: Option<SelectedClipSummary<'a>>,
    stretch_tempo: Option<StretchTempoUiSnapshot>,
    name_input: &TextInputState,
    name_focused: bool,
    name_callbacks: TextInputCallbacks,
    clip_name_input: &TextInputState,
    clip_name_focused: bool,
    clip_name_callbacks: TextInputCallbacks,
    color_picker: InspectorColorPicker<'a>,
    active: bool,
    callbacks: &InspectorCallbacks,
    i18n: I18n,
) -> impl IntoElement {
    let body: gpui::AnyElement = if let Some(clip) = clip_summary {
        let tempo = stretch_tempo.unwrap_or_default();
        clip_inspector(
            clip,
            clip_name_input,
            clip_name_focused,
            clip_name_callbacks,
            tempo,
            callbacks,
            i18n,
        )
        .into_any_element()
    } else if let Some(tid) = selected_track_id {
        match tracks.iter().find(|t| t.id == tid) {
            Some(t) => {
                // MIDI Out needs the live Instrument-track roster (id, name)
                // to offer/label real VSTi routing targets instead of the
                // audio-only `Main` bus. Cheap: a handful of tracks at most.
                let instrument_targets: Vec<(String, String)> = tracks
                    .iter()
                    .filter(|track| track.track_type == TrackType::Instrument)
                    .map(|track| (track.id.clone(), track.name.clone()))
                    .collect();
                track_inspector(
                    t,
                    connections,
                    name_input,
                    name_focused,
                    name_callbacks,
                    &instrument_targets,
                    &color_picker,
                    callbacks,
                    i18n,
                )
                .into_any_element()
            }
            None => no_selection(tracks.len(), i18n).into_any_element(),
        }
    } else {
        let _ = selected_clip_id; // currently only used via clip_summary
        no_selection(tracks.len(), i18n).into_any_element()
    };

    inspector_shell(active, i18n).child(body)
}

fn inspector_shell(active: bool, i18n: I18n) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .w(px(INSPECTOR_WIDTH))
        .h_full()
        .bg(Colors::surface_panel())
        .border_l(px(1.0))
        .border_color(if active {
            Colors::panel_border_focused()
        } else {
            Colors::border_subtle()
        })
        .child(
            div()
                .flex_shrink_0()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(7.0))
                .h(px(32.0))
                .px(px(10.0))
                .border_b(px(1.0))
                .border_color(if active {
                    Colors::panel_border_focused()
                } else {
                    Colors::border_subtle()
                })
                .child(
                    svg()
                        .path(assets::ICON_SLIDERS_HORIZONTAL_PATH)
                        .w(px(13.0))
                        .h(px(13.0))
                        .text_color(if active {
                            Colors::panel_header_active()
                        } else {
                            Colors::text_muted()
                        }),
                )
                .child(
                    div()
                        .text_color(if active {
                            Colors::panel_header_active()
                        } else {
                            Colors::tab_text()
                        })
                        .text_size(px(typography::DENSE_LABEL))
                        .font_weight(gpui::FontWeight::BOLD)
                        .child(i18n.tr("panel.inspector")),
                )
                .child(fb_shortcut_hint(crate::keymap::accel_display("Ctrl+2"))),
        )
}

/// Scrollable body wrapper shared by every populated inspector view.
fn scroll_body() -> gpui::Stateful<gpui::Div> {
    div()
        .id("inspector-scroll")
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .px(px(10.0))
        .py(px(10.0))
        .gap(px(12.0))
}

fn no_selection(track_count: usize, i18n: I18n) -> impl IntoElement {
    div()
        .flex_1()
        .flex()
        .flex_col()
        .child(
            div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(7.0))
                .px(px(16.0))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .w(px(30.0))
                        .h(px(30.0))
                        .rounded(px(crate::theme::radius::CONTROL))
                        .bg(Colors::surface_input())
                        .border(px(1.0))
                        .border_color(Colors::border_subtle())
                        .child(
                            svg()
                                .path(assets::ICON_SLIDERS_HORIZONTAL_PATH)
                                .w(px(15.0))
                                .h(px(15.0))
                                .text_color(Colors::text_faint()),
                        ),
                )
                .child(
                    div()
                        .text_color(Colors::text_secondary())
                        .text_size(px(11.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child(i18n.tr("inspector.no-selection.title")),
                )
                .child(
                    div()
                        .text_color(Colors::text_faint())
                        .text_size(px(10.5))
                        .text_center()
                        .child(i18n.tr("inspector.no-selection.hint")),
                ),
        )
        .child(
            div()
                .flex_shrink_0()
                .px(px(10.0))
                .py(px(10.0))
                .border_t(px(1.0))
                .border_color(Colors::border_subtle())
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(inspector_kit::ins_section_header(
                    assets::ICON_FILE_PATH,
                    "PROJECT",
                ))
                .child(kv_row(
                    i18n.tr("wizard.summary.tracks"),
                    track_count.to_string(),
                )),
        )
}

fn kv_row(key: impl Into<String>, value: impl Into<String>) -> impl IntoElement {
    inspector_kit::ins_kv_row(key, value)
}

/// Header strip shown at the top of every populated inspector: color chip,
/// title, and a type badge.
fn inspector_header_icon(
    icon: &'static str,
    accent: gpui::Rgba,
    title: impl Into<String>,
    badge: impl Into<String>,
) -> impl IntoElement {
    inspector_kit::ins_header(icon, accent, title, badge)
}

#[allow(dead_code)]
fn inspector_header(
    accent: gpui::Rgba,
    title: impl Into<String>,
    badge: impl Into<String>,
) -> impl IntoElement {
    let badge = badge.into();
    let mut row = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        // Left accent rail carries the type/identity hue.
        .child(
            div()
                .w(px(3.0))
                .h(px(22.0))
                .rounded(px(crate::theme::radius::PILL))
                .bg(accent),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .truncate()
                .text_size(px(13.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(Colors::text_primary())
                .child(title.into()),
        );
    if !badge.is_empty() {
        row = row.child(
            div()
                .flex_shrink_0()
                .px(px(7.0))
                .py(px(2.0))
                .rounded(px(crate::theme::radius::CONTROL))
                .bg(Colors::with_alpha(accent, 0.14))
                .text_size(px(typography::DENSE_CAPTION))
                .font_weight(gpui::FontWeight::BOLD)
                .text_color(Colors::with_alpha(accent, 0.92))
                .child(badge),
        );
    }
    row
}

fn format_pan(i18n: I18n, pan: f32) -> String {
    if pan.abs() < 0.01 {
        i18n.tr("inspector.pan.center")
    } else if pan < 0.0 {
        let percent = (pan * -100.0).round().clamp(1.0, 100.0) as i32;
        i18n.tr_vars("inspector.pan.left", &[("percent", percent.to_string())])
    } else {
        let percent = (pan * 100.0).round().clamp(1.0, 100.0) as i32;
        i18n.tr_vars("inspector.pan.right", &[("percent", percent.to_string())])
    }
}

/// Clickable M/S/R/I-style state badge.
/// An inspector card.
///
/// `id` is the section's stable name. It carries no control of its own: the
/// header is a title and a rule, and the rule is what ties a long stack of
/// cards together for the eye. A switch there had nothing to switch — the
/// sections are short and all of them want to be visible at once — so it was a
/// control whose only effect was to hide something the user had just gone
/// looking for.
fn section_card(
    id: &'static str,
    title: impl Into<String>,
    _callbacks: &InspectorCallbacks,
    body: impl IntoElement,
) -> impl IntoElement {
    let _ = id;
    inspector_kit::ins_card(title, body)
}

/// A stack of rows, for a card body built up one child at a time.
fn section_rows() -> gpui::Div {
    div().flex().flex_col().gap(px(inspector_kit::CARD_ROW_GAP))
}

/// The Inspector's track-colour control: the shared color picker, hosted by
/// `StudioLayout`.
///
/// The field is a borrowed view over the host's [`ColorPickerState`] — the
/// Inspector renders it, the host owns it — so the preset palette, arbitrary
/// hex/HSV colours, and recent colours all come from one implementation shared
/// with Add Track instead of a second, palette-only picker here.
pub struct InspectorColorPicker<'a> {
    pub state: &'a ColorPickerState,
    /// Whether the picker's hex field currently owns the keyboard.
    pub hex_focused: bool,
    pub hex_callbacks: TextInputCallbacks,
    pub callbacks: ColorPickerCallbacks,
}

fn color_field(picker: &InspectorColorPicker<'_>) -> impl IntoElement {
    color_picker_field(
        "inspector-color-picker",
        picker.state,
        &default_presets(),
        // Auto Colour is an Add Track concern: an existing track always has a
        // concrete stored colour, and offering "Auto" here would have to mean
        // "recolour by index", which `set_track_color` does not do.
        false,
        ColorPickerPlacement::Below,
        picker.hex_focused,
        picker.hex_callbacks.clone(),
        picker.callbacks.clone(),
    )
}

fn format_selector(track: &TrackState, callbacks: &InspectorCallbacks) -> impl IntoElement {
    routing_combo_trigger(
        "inspector-format-combo",
        track.routing.audio_format.label().to_string(),
        InspectorRoutingCombo::AudioFormat,
        callbacks.open_routing_combo,
        callbacks.on_toggle_routing_combo.clone(),
    )
}

fn audio_input_selector(
    track: &TrackState,
    connections: &AudioConnectionRegistry,
    callbacks: &InspectorCallbacks,
) -> impl IntoElement {
    routing_combo_trigger(
        "inspector-input-combo",
        audio_input_combo_label(track, connections),
        InspectorRoutingCombo::AudioInput,
        callbacks.open_routing_combo,
        callbacks.on_toggle_routing_combo.clone(),
    )
}

fn output_selector(track: &TrackState, callbacks: &InspectorCallbacks) -> impl IntoElement {
    routing_combo_trigger(
        "inspector-output-combo",
        audio_output_combo_label(&track.routing.output, track.routing.audio_format),
        InspectorRoutingCombo::AudioOutput,
        callbacks.open_routing_combo,
        callbacks.on_toggle_routing_combo.clone(),
    )
}

fn normalized_vsti_output_channels(slot: &InsertSlotState) -> Vec<u8> {
    let mut channels = if slot.enabled_audio_output_channels.is_empty() {
        vec![1, 2]
    } else {
        slot.enabled_audio_output_channels.clone()
    };
    if !channels.contains(&1) {
        channels.push(1);
    }
    if !channels.contains(&2) {
        channels.push(2);
    }
    channels.retain(|channel| (1..=32).contains(channel));
    channels.sort_unstable();
    channels.dedup();
    channels
}

fn vsti_output_label(slot: &InsertSlotState) -> String {
    let channels = normalized_vsti_output_channels(slot);
    let extras: Vec<String> = channels
        .iter()
        .copied()
        .filter(|channel| *channel > 2)
        .map(|channel| format!("Ch {channel}"))
        .collect();
    if extras.is_empty() {
        "Main 1/2".to_string()
    } else {
        format!("Main 1/2 + {}", extras.join(", "))
    }
}

fn vsti_output_selector(
    slot: &InsertSlotState,
    callbacks: &InspectorCallbacks,
) -> impl IntoElement {
    routing_combo_trigger(
        "inspector-vsti-output-combo",
        vsti_output_label(slot),
        InspectorRoutingCombo::VstiOutputs,
        callbacks.open_routing_combo,
        callbacks.on_toggle_routing_combo.clone(),
    )
}

fn vsti_output_dropdown(
    track: &TrackState,
    slot: &InsertSlotState,
    callbacks: &InspectorCallbacks,
    position: crate::overlay::OverlayPosition,
) -> impl IntoElement {
    let selected = normalized_vsti_output_channels(slot);
    let left: f32 = position.x.into();
    let top: f32 = position.y.into();
    let width: f32 = position.width.map(|w| w.into()).unwrap_or(176.0);
    let max_h: f32 = position.max_height.map(|h| h.into()).unwrap_or(260.0);
    let mut menu = div()
        .id("inspector-vsti-output-dropdown")
        .absolute()
        .left(px(left))
        .top(px(top))
        .w(px(width))
        .max_h(px(max_h))
        .flex()
        .flex_col()
        .gap(px(2.0))
        .p(px(6.0))
        .rounded(px(crate::theme::radius::CONTROL))
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_card())
        .shadow(vec![gpui::BoxShadow {
            color: Colors::surface_overlay().into(),
            offset: gpui::point(px(0.0), px(10.0)),
            blur_radius: px(28.0),
            spread_radius: px(0.0),
            inset: false,
        }])
        .overflow_y_scroll()
        .occlude()
        .on_mouse_down(MouseButton::Left, |_, _window, cx| cx.stop_propagation());

    menu = menu.child(fb_checkbox(
        "vsti-output-main",
        "Main 1/2",
        true,
        false,
        |_, _, _| {},
    ));

    // One row per OUTPUT BUS (stereo pair) beyond Main: ticking it routes the
    // whole pair, so a single tick gives full left+right sound (a mono bus
    // reports a duplicated pair). Bus 0 is Main 1/2, already shown above.
    let bus_counts = &slot.output_bus_channel_counts;
    for bus_index in vsti_output_bus_strip_indices(bus_counts) {
        if bus_index == 0 {
            continue;
        }
        let Some((l, r)) = vsti_output_child_channels_for_bus_layout(bus_counts, bus_index) else {
            continue;
        };
        let track_id = track.id.clone();
        let insert_id = slot.id.clone();
        let checked = selected.contains(&l) && selected.contains(&r);
        let label = if l == r {
            format!("Output {} (Ch {l})", bus_index + 1)
        } else {
            format!("Output {} (Ch {l}/{r})", bus_index + 1)
        };
        let toggle = callbacks.on_toggle_insert_output_channel.clone();
        menu = menu.child(fb_checkbox(
            ("vsti-output-bus", bus_index as u32),
            label,
            checked,
            true,
            move |_, window, cx| {
                toggle(
                    &(track_id.clone(), insert_id.clone(), l, !checked),
                    window,
                    cx,
                )
            },
        ));
    }

    menu
}

fn audio_format_options() -> Vec<String> {
    vec![
        TrackAudioFormat::Mono.label().to_string(),
        TrackAudioFormat::Stereo.label().to_string(),
    ]
}

/// Display label for a track's audio input.
///
/// Resolved from the Audio Connections registry, so renaming a connection
/// updates the Inspector with no change to the track. An unavailable
/// connection is labelled as such without clearing the assignment; a reference
/// that no longer resolves is named as missing rather than shown as No Input.
fn audio_input_combo_label(track: &TrackState, connections: &AudioConnectionRegistry) -> String {
    let Some(id) = track.routing.audio_input_connection_id.as_ref() else {
        return "No Input".to_string();
    };
    match connections.get(id) {
        Some(connection) if connection.status.is_usable() => connection.name.clone(),
        Some(connection) => format!("{} — Unavailable", connection.name),
        None => "Missing connection".to_string(),
    }
}

fn build_audio_output_options(
    track: &TrackState,
    bus_targets: &[(String, String)],
    _output_device: Option<&(String, u32)>,
) -> Vec<(String, TrackOutputRouting)> {
    let mut out = vec![
        (
            master_output_label(track.routing.audio_format),
            TrackOutputRouting::Main,
        ),
        ("None".to_string(), TrackOutputRouting::None),
    ];
    for (bus_id, name) in bus_targets {
        if *bus_id == track.id {
            continue;
        }
        out.push((
            format!("Bus - {name}"),
            TrackOutputRouting::Bus {
                bus_id: bus_id.clone(),
            },
        ));
    }
    if !out.iter().any(|(_, r)| *r == track.routing.output) {
        out.push((
            format!(
                "Missing - {}",
                audio_output_combo_label(&track.routing.output, track.routing.audio_format)
            ),
            track.routing.output.clone(),
        ));
    }
    out
}

fn audio_output_combo_label(
    routing: &TrackOutputRouting,
    audio_format: TrackAudioFormat,
) -> String {
    match routing {
        TrackOutputRouting::Main => master_output_label(audio_format),
        TrackOutputRouting::Bus { bus_id } => format!("Bus - {bus_id}"),
        TrackOutputRouting::HardwareOutput { channel, .. } => {
            format!("Channel {}", channel + 1)
        }
        // Audio/Instrument tracks never carry `Instrument` routing (it only
        // applies to MIDI tracks); kept for exhaustiveness.
        TrackOutputRouting::Instrument { track_id } => format!("Instrument - {track_id}"),
        TrackOutputRouting::None => "None".to_string(),
    }
}

fn master_output_label(audio_format: TrackAudioFormat) -> String {
    match audio_format {
        TrackAudioFormat::Mono => "Mono Master".to_string(),
        TrackAudioFormat::Stereo => "Stereo Master".to_string(),
    }
}

fn parse_audio_format_option(label: &str) -> TrackAudioFormat {
    match label {
        "Mono" => TrackAudioFormat::Mono,
        _ => TrackAudioFormat::Stereo,
    }
}

fn midi_input_combo_label(routing: &TrackMidiInputRouting) -> String {
    match routing {
        TrackMidiInputRouting::AllInputs => "All".to_string(),
        TrackMidiInputRouting::None => "None".to_string(),
        TrackMidiInputRouting::MidiDevice { device_id } => device_id.clone(),
    }
}

fn midi_channel_combo_label(channel: Option<u8>) -> String {
    channel
        .map(|ch| ch.to_string())
        .unwrap_or_else(|| "All".to_string())
}

fn midi_input_options(detected: &[String]) -> Vec<String> {
    let mut options = vec!["All".to_string(), "None".to_string()];
    options.extend(detected.iter().cloned());
    options
}

fn midi_channel_options() -> Vec<String> {
    std::iter::once("All".to_string())
        .chain((1..=16).map(|ch| ch.to_string()))
        .collect()
}

/// MIDI Out destinations for a `TrackType::Midi` track: real Instrument
/// (VSTi) tracks in the project to actually make sound, plus any detected
/// external MIDI hardware/virtual ports. Deliberately excludes `Main` — a
/// MIDI track has no audio to send to the Master bus, so offering it there
/// just looked like a mislabeled audio-output field.
fn build_midi_output_options(
    track: &TrackState,
    instrument_targets: &[(String, String)],
    detected_midi_outputs: &[String],
) -> Vec<(String, TrackOutputRouting)> {
    let mut out = vec![("None".to_string(), TrackOutputRouting::None)];
    for (track_id, name) in instrument_targets {
        out.push((
            format!("Instrument - {name}"),
            TrackOutputRouting::Instrument {
                track_id: track_id.clone(),
            },
        ));
    }
    for device in detected_midi_outputs {
        out.push((
            format!("MIDI Device - {device}"),
            TrackOutputRouting::HardwareOutput {
                device_id: device.clone(),
                channel: 0,
            },
        ));
    }
    if !out.iter().any(|(_, r)| *r == track.routing.output) {
        out.push((
            format!(
                "Missing - {}",
                midi_output_combo_label(&track.routing.output, instrument_targets)
            ),
            track.routing.output.clone(),
        ));
    }
    out
}

/// Display label for the MIDI Out trigger/selected value, resolving an
/// `Instrument` target's live track name when it's still known.
fn midi_output_combo_label(
    routing: &TrackOutputRouting,
    instrument_targets: &[(String, String)],
) -> String {
    match routing {
        TrackOutputRouting::Instrument { track_id } => instrument_targets
            .iter()
            .find(|(id, _)| id == track_id)
            .map(|(_, name)| format!("Instrument - {name}"))
            .unwrap_or_else(|| routing.label()),
        TrackOutputRouting::HardwareOutput { device_id, .. } => {
            format!("MIDI Device - {device_id}")
        }
        _ => routing.label(),
    }
}

fn parse_midi_input_option(label: &str) -> TrackMidiInputRouting {
    match label {
        "All" => TrackMidiInputRouting::AllInputs,
        "None" => TrackMidiInputRouting::None,
        device => TrackMidiInputRouting::MidiDevice {
            device_id: device.to_string(),
        },
    }
}

fn parse_midi_channel_option(label: &str) -> Option<u8> {
    if label == "All" {
        None
    } else {
        label.parse::<u8>().ok().map(|ch| ch.clamp(1, 16))
    }
}

fn routing_combo_trigger(
    trigger_id: &'static str,
    label: String,
    combo: InspectorRoutingCombo,
    open_combo: Option<InspectorRoutingCombo>,
    on_toggle: RoutingComboToggleCb,
) -> impl IntoElement {
    let open = open_combo == Some(combo);
    let toggle = on_toggle.clone();
    div().w_full().child(combo_box_trigger(
        trigger_id,
        label,
        open,
        move |event, window, cx| {
            let anchor = if open {
                None
            } else {
                Some(OverlayAnchor {
                    bounds: crate::overlay::inspector_combo_trigger_bounds(
                        window,
                        INSPECTOR_WIDTH,
                        event,
                    ),
                })
            };
            toggle(combo, anchor, window, cx);
        },
    ))
}

fn midi_input_selector(track: &TrackState, callbacks: &InspectorCallbacks) -> impl IntoElement {
    routing_combo_trigger(
        "inspector-midi-input-combo",
        midi_input_combo_label(&track.routing.midi_input),
        InspectorRoutingCombo::MidiInput,
        callbacks.open_routing_combo,
        callbacks.on_toggle_routing_combo.clone(),
    )
}

fn midi_channel_selector(track: &TrackState, callbacks: &InspectorCallbacks) -> impl IntoElement {
    routing_combo_trigger(
        "inspector-midi-channel-combo",
        midi_channel_combo_label(track.routing.midi_channel),
        InspectorRoutingCombo::MidiChannel,
        callbacks.open_routing_combo,
        callbacks.on_toggle_routing_combo.clone(),
    )
}

fn midi_output_selector(
    track: &TrackState,
    instrument_targets: &[(String, String)],
    callbacks: &InspectorCallbacks,
) -> impl IntoElement {
    routing_combo_trigger(
        "inspector-midi-output-combo",
        midi_output_combo_label(&track.routing.output, instrument_targets),
        InspectorRoutingCombo::MidiOut,
        callbacks.open_routing_combo,
        callbacks.on_toggle_routing_combo.clone(),
    )
}

type CloseRoutingComboCb = Arc<dyn Fn(&mut App) + 'static>;

/// Dropdown overlay for Inspector MIDI routing ComboBoxes. Rendered above the
/// main chrome so menus stay anchored to their trigger, not the mount point.
pub(crate) fn inspector_routing_combo_overlay(
    track: &TrackState,
    connections: &AudioConnectionRegistry,
    open_combo: InspectorRoutingCombo,
    anchor: OverlayAnchor,
    window: &Window,
    callbacks: &InspectorCallbacks,
    on_close: CloseRoutingComboCb,
    // Current hardware inventory, used only for the secondary detail line —
    // never to build the list of choices.
    available_ports: crate::audio_connections::AvailablePorts,
    // Available Bus/Return output targets as `(track_id, display_name)`.
    audio_output_buses: Vec<(String, String)>,
    // Selected output device `(name, channel_count)` for hardware output routes.
    audio_output_device: Option<(String, u32)>,
    // Instrument (VSTi) tracks in the project as `(track_id, display_name)` —
    // the real MIDI Out destinations for a plain MIDI track.
    instrument_targets: Vec<(String, String)>,
    // Real, Preferences-enabled MIDI hardware/virtual ports from the shared
    // `device_registry` cache (same source Settings → MIDI renders from).
    detected_midi_inputs: Vec<String>,
    detected_midi_outputs: Vec<String>,
) -> impl IntoElement {
    let position =
        inspector_combo_menu_position(anchor, INSPECTOR_WIDTH, ROUTING_COMBO_MENU_HEIGHT, window);

    let track_id = track.id.clone();
    let menu = match open_combo {
        InspectorRoutingCombo::AudioFormat => {
            let selected = track.routing.audio_format.label().to_string();
            let options = audio_format_options();
            let cb = callbacks.on_set_audio_format.clone();
            let close = on_close.clone();
            combo_box_string_menu(
                "inspector-audio-format-menu",
                position,
                &selected,
                &options,
                Arc::new(move |value, window, cx| {
                    let format = parse_audio_format_option(&value);
                    cb(&(track_id.clone(), format), window, cx);
                    close(cx);
                }),
            )
            .into_any_element()
        }
        InspectorRoutingCombo::AudioInput => {
            // Logical Input Audio Connections only. Physical devices and ports
            // are never enumerated here — the footer action opens the window
            // where hardware mapping actually lives.
            let _ = &available_ports;
            let routing_options = crate::input_routing::track_input_options(
                connections,
                track.routing.audio_format.channel_count(),
                track.routing.audio_input_connection_id.as_ref(),
            );
            let selected = audio_input_combo_label(track, connections);
            let labels: Vec<String> = routing_options
                .iter()
                .map(crate::input_routing::InputChoice::label)
                .collect();
            let cb = callbacks.on_set_input_routing.clone();
            let close = on_close.clone();
            combo_box_string_menu(
                "inspector-audio-input-menu",
                position,
                &selected,
                &labels,
                Arc::new(move |value, window, cx| {
                    let selection = routing_options
                        .iter()
                        .find(|choice| choice.label() == value)
                        .map(crate::input_routing::InputChoice::selection)
                        .unwrap_or(crate::input_routing::TrackInputSelection::NoInput);
                    cb(&(track_id.clone(), selection), window, cx);
                    close(cx);
                }),
            )
            .into_any_element()
        }
        InspectorRoutingCombo::AudioOutput => {
            let routing_options = build_audio_output_options(
                track,
                &audio_output_buses,
                audio_output_device.as_ref(),
            );
            let selected = routing_options
                .iter()
                .find(|(_, r)| *r == track.routing.output)
                .map(|(l, _)| l.clone())
                .unwrap_or_else(|| track.routing.output.label());
            let labels: Vec<String> = routing_options.iter().map(|(l, _)| l.clone()).collect();
            let cb = callbacks.on_set_output_routing.clone();
            let close = on_close.clone();
            combo_box_string_menu(
                "inspector-audio-output-menu",
                position,
                &selected,
                &labels,
                Arc::new(move |value, window, cx| {
                    let output = routing_options
                        .iter()
                        .find(|(l, _)| *l == value)
                        .map(|(_, r)| r.clone())
                        .unwrap_or(TrackOutputRouting::None);
                    cb(&(track_id.clone(), output), window, cx);
                    close(cx);
                }),
            )
            .into_any_element()
        }
        InspectorRoutingCombo::VstiOutputs => track
            .instrument_insert()
            .map(|slot| vsti_output_dropdown(track, slot, callbacks, position).into_any_element())
            .unwrap_or_else(|| div().into_any_element()),
        InspectorRoutingCombo::MidiInput => {
            let selected = midi_input_combo_label(&track.routing.midi_input);
            let options = midi_input_options(&detected_midi_inputs);
            let cb = callbacks.on_set_midi_input.clone();
            let close = on_close.clone();
            combo_box_string_menu(
                "inspector-midi-input-menu",
                position,
                &selected,
                &options,
                Arc::new(move |value, window, cx| {
                    let routing = parse_midi_input_option(&value);
                    cb(&(track_id.clone(), routing), window, cx);
                    close(cx);
                }),
            )
            .into_any_element()
        }
        InspectorRoutingCombo::MidiChannel => {
            let selected = midi_channel_combo_label(track.routing.midi_channel);
            let options = midi_channel_options();
            let cb = callbacks.on_set_midi_channel.clone();
            let close = on_close.clone();
            combo_box_string_menu(
                "inspector-midi-channel-menu",
                position,
                &selected,
                &options,
                Arc::new(move |value, window, cx| {
                    let channel = parse_midi_channel_option(&value);
                    cb(&(track_id.clone(), channel), window, cx);
                    close(cx);
                }),
            )
            .into_any_element()
        }
        InspectorRoutingCombo::MidiOut => {
            let routing_options =
                build_midi_output_options(track, &instrument_targets, &detected_midi_outputs);
            let selected = routing_options
                .iter()
                .find(|(_, r)| *r == track.routing.output)
                .map(|(l, _)| l.clone())
                .unwrap_or_else(|| {
                    midi_output_combo_label(&track.routing.output, &instrument_targets)
                });
            let labels: Vec<String> = routing_options.iter().map(|(l, _)| l.clone()).collect();
            let cb = callbacks.on_set_output_routing.clone();
            let close = on_close.clone();
            combo_box_string_menu(
                "inspector-midi-output-menu",
                position,
                &selected,
                &labels,
                Arc::new(move |value, window, cx| {
                    let output = routing_options
                        .iter()
                        .find(|(l, _)| *l == value)
                        .map(|(_, r)| r.clone())
                        .unwrap_or(TrackOutputRouting::None);
                    cb(&(track_id.clone(), output), window, cx);
                    close(cx);
                }),
            )
            .into_any_element()
        }
    };

    div()
        .absolute()
        .inset_0()
        .id("inspector-routing-combo-overlay")
        .on_mouse_down(MouseButton::Left, move |_, _window, cx| {
            on_close(cx);
        })
        .child(menu)
}

fn routing_section(
    track: &TrackState,
    connections: &AudioConnectionRegistry,
    instrument_targets: &[(String, String)],
    callbacks: &InspectorCallbacks,
) -> impl IntoElement {
    let mut rows = section_rows();

    match track.track_type {
        TrackType::Audio => {
            rows = rows
                .child(fb_form_row("Format", format_selector(track, callbacks)))
                .child(fb_form_row(
                    "Input",
                    audio_input_selector(track, connections, callbacks),
                ))
                .child(fb_form_row("Output", output_selector(track, callbacks)));
        }
        TrackType::Instrument => {
            rows = rows
                .child(fb_form_row(
                    "MIDI Input",
                    midi_input_selector(track, callbacks),
                ))
                .child(fb_form_row(
                    "MIDI Ch",
                    midi_channel_selector(track, callbacks),
                ))
                .child(fb_form_row("Output", output_selector(track, callbacks)));
        }
        TrackType::Midi => {
            rows = rows
                .child(fb_form_row(
                    "MIDI Input",
                    midi_input_selector(track, callbacks),
                ))
                .child(fb_form_row(
                    "MIDI Ch",
                    midi_channel_selector(track, callbacks),
                ))
                .child(fb_form_row(
                    "MIDI Out",
                    midi_output_selector(track, instrument_targets, callbacks),
                ));
        }
        TrackType::Bus | TrackType::Return | TrackType::Group | TrackType::Master => {
            rows = rows.child(fb_form_row("Output", output_selector(track, callbacks)));
        }
        // A Video track has no audio path, so it exposes no routing controls.
        TrackType::Video => {}
    }

    section_card("routing", "Routing", callbacks, rows)
}

const MPE_MODE_OPTIONS: &[InspectorSelectOption<MpeOutputMode>] = &[
    InspectorSelectOption {
        label: "Off",
        value: MpeOutputMode::Off,
    },
    InspectorSelectOption {
        label: "Auto Detect",
        value: MpeOutputMode::Auto,
    },
    InspectorSelectOption {
        label: "Lower Zone",
        value: MpeOutputMode::Lower,
    },
    InspectorSelectOption {
        label: "Upper Zone",
        value: MpeOutputMode::Upper,
    },
];

/// Professional-only MPE transport settings. The expression curves remain
/// visible and playable in Community; only editing the track-level transport
/// policy is edition-gated here.
fn mpe_section(track: &TrackState, callbacks: &InspectorCallbacks) -> impl IntoElement {
    let tid = track.id.clone();
    let config = track.routing.mpe.sanitized();
    let enabled = config.mode != MpeOutputMode::Off;
    let cb = callbacks.on_set_mpe_configuration.clone();

    let mode_cb = cb.clone();
    let mode_id = tid.clone();
    let mode = inspector_select(
        "inspector-mpe-mode",
        config.mode,
        MPE_MODE_OPTIONS,
        false,
        move |mode, window, cx| {
            let mut next = config;
            next.mode = mode;
            mode_cb(&(mode_id.clone(), next), window, cx);
        },
    );

    let members_cb = cb.clone();
    let members_id = tid.clone();
    let members = inspector_numeric_stepper(
        "inspector-mpe-members",
        f64::from(config.member_channels),
        format!("{} channels", config.member_channels),
        1.0,
        15.0,
        1.0,
        !enabled,
        move |value, window, cx| {
            let mut next = config;
            next.member_channels = value.round().clamp(1.0, 15.0) as u8;
            members_cb(&(members_id.clone(), next), window, cx);
        },
    );

    let member_range_cb = cb.clone();
    let member_range_id = tid.clone();
    let member_range = inspector_numeric_stepper(
        "inspector-mpe-member-range",
        f64::from(config.member_pitch_range),
        format!("{:.0} st", config.member_pitch_range),
        1.0,
        96.0,
        1.0,
        !enabled,
        move |value, window, cx| {
            let mut next = config;
            next.member_pitch_range = value as f32;
            member_range_cb(&(member_range_id.clone(), next), window, cx);
        },
    );

    let manager_range_cb = cb;
    let manager_range_id = tid;
    let manager_range = inspector_numeric_stepper(
        "inspector-mpe-manager-range",
        f64::from(config.manager_pitch_range),
        format!("{:.0} st", config.manager_pitch_range),
        1.0,
        96.0,
        1.0,
        !enabled,
        move |value, window, cx| {
            let mut next = config;
            next.manager_pitch_range = value as f32;
            manager_range_cb(&(manager_range_id.clone(), next), window, cx);
        },
    );

    let hint = match config.mode {
        MpeOutputMode::Off => "MPE output is disabled; notes use the track MIDI channel.",
        MpeOutputMode::Auto => {
            "Expression notes use the Lower Zone; ordinary notes keep their MIDI channel."
        }
        MpeOutputMode::Lower => "All notes use Lower Zone member channels 2–16.",
        MpeOutputMode::Upper => "All notes use Upper Zone member channels 1–15.",
    };

    section_card(
        "mpe",
        "MPE",
        callbacks,
        section_rows()
            .child(fb_form_row("Mode", mode))
            .child(fb_form_row("Members", members))
            .child(fb_form_row("Member Range", member_range))
            .child(fb_form_row("Manager Range", manager_range))
            .child(inspector_hint_text(hint)),
    )
}

fn compact_action_button(
    id: impl Into<gpui::ElementId>,
    label: impl Into<String>,
    enabled: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    fb_button(id, label, FbButtonKind::Default, enabled, on_click)
}

fn plugin_format_label(slot: &InsertSlotState) -> &'static str {
    slot.plugin_format.map(|fmt| fmt.label()).unwrap_or("-")
}

fn plugin_slot_name(slot: Option<&InsertSlotState>, empty: &'static str) -> String {
    slot.map(|slot| slot.display_name.clone())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| empty.to_string())
}

fn plugin_state_label(slot: &InsertSlotState) -> String {
    match &slot.load_status {
        InsertLoadStatus::Empty => "Empty".to_string(),
        InsertLoadStatus::Loading => "Loading".to_string(),
        InsertLoadStatus::Ready if slot.bypassed => "Bypassed".to_string(),
        InsertLoadStatus::Ready if !slot.enabled => "Disabled".to_string(),
        InsertLoadStatus::Ready => "Ready".to_string(),
        InsertLoadStatus::Missing(message) => format!("Missing: {message}"),
        InsertLoadStatus::Failed(message) => format!("Failed: {message}"),
        InsertLoadStatus::Disabled => "Disabled".to_string(),
    }
}

/// One plug-in in a chain.
///
/// ```txt
///   ⏻  Pro-Q 4                              ⚙  🗑
///   │   │                                   │   └ remove
///   │   └ name — press to open the editor   └ open the editor
///   └ in circuit / bypassed
/// ```
///
/// # Why it is one row
///
/// The old slot was three stacked rows: a header with a grip and a format chip,
/// a "State: Ready" line, and seven wrapped buttons — Open, Replace, Bypass,
/// Enable, Remove, Up, Down. Four slots filled the panel, and the two commands
/// anyone actually reaches for were the hardest to find in it.
///
/// So: the power pip carries bypass, the name carries open, and the two glyphs
/// on the right carry the settings and the bin. Order matters — the destructive
/// one is last, furthest from the name the pointer arrives at. Up and Down are
/// gone because the row *drags*: reordering by dragging the thing you are
/// reordering needs no buttons and no aim, and the drop line shows where it
/// lands. Replace is gone too; removing and adding is the same two gestures
/// without a command that silently discards a plug-in's state.
///
/// Everything a slot can say that is not one of those four lives in the
/// tooltip: the format, the load state, and why it failed if it did.
#[allow(clippy::too_many_arguments)]
fn plugin_slot_row(
    track: &TrackState,
    slot: &InsertSlotState,
    slot_index: usize,
    display_index: usize,
    callbacks: &InspectorCallbacks,
    _can_move_up: bool,
    _can_move_down: bool,
    is_instrument: bool,
) -> impl IntoElement {
    let _ = is_instrument;
    let name = plugin_slot_name(Some(slot), "Empty Slot");
    let bypassed = slot.bypassed || !slot.enabled;
    let failed = matches!(
        slot.load_status,
        InsertLoadStatus::Missing(_) | InsertLoadStatus::Failed(_)
    );

    // Drag source: the whole row, carrying the stable plugin_instance_id so
    // reorder identity follows the instance and never the visual index.
    let drag_payload = FxSlotDrag {
        track_id: track.id.clone(),
        insert_id: slot.id.clone(),
        display_name: name.clone(),
    };
    // Drop target: a drop on this row moves the dragged slot into the gap
    // *above* it. Same-track guarded, and the shared accent line shows where.
    let drop_track = track.id.clone();
    let can_drop_track = track.id.clone();
    let reorder = callbacks.on_reorder_insert.clone();

    let open = callbacks.on_open_insert_editor.clone();
    let open_target = (track.id.clone(), slot_index, slot.id.clone());
    let bypass = callbacks.on_toggle_insert_bypass.clone();
    let bypass_target = (track.id.clone(), slot.id.clone());
    let remove = callbacks.on_remove_insert.clone();
    let remove_target = (track.id.clone(), slot.id.clone());
    let editor_target = open_target.clone();
    let open_editor = callbacks.on_open_insert_editor.clone();

    let tooltip = format!(
        "{name} · {} · {}",
        plugin_format_label(slot),
        plugin_state_label(slot)
    );

    let rest = Colors::composite(Colors::surface_card(), Colors::state_recessed());
    let hover = Colors::composite(rest, Colors::state_hover());

    div()
        .id(("fx-slot", slot_index))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::SNUG))
        .h(px(crate::theme::size::PROMINENT))
        .px(px(space::SNUG))
        .rounded(px(crate::theme::radius::CONTROL))
        .bg(rest)
        .hover(move |style| style.bg(hover))
        .cursor(gpui::CursorStyle::PointingHand)
        .tooltip(crate::components::controls::fb_tooltip(tooltip))
        .can_drop(move |dragged, _window, _cx| {
            dragged
                .downcast_ref::<FxSlotDrag>()
                .is_some_and(|d| d.track_id == can_drop_track)
        })
        .drag_over::<FxSlotDrag>(|style, _drag, _window, _cx| drop_over_highlight(style))
        .on_drop::<FxSlotDrag>(move |drag, window, cx| {
            if drag.track_id == drop_track {
                reorder(
                    &(drop_track.clone(), drag.insert_id.clone(), slot_index),
                    window,
                    cx,
                );
            }
        })
        .on_drag(drag_payload, |drag, _offset, _window, cx| {
            cx.new(|_| drag.clone())
        })
        // Bypass. A power symbol, not a word: it is the one control on the row
        // whose state has to be readable without reading.
        .child(
            div()
                .id(("fx-slot-bypass", slot_index))
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .w(px(20.0))
                .h(px(20.0))
                .rounded(px(crate::theme::radius::CONTROL))
                .cursor(gpui::CursorStyle::PointingHand)
                .hover(|style| style.bg(Colors::state_hover()))
                .child(
                    svg()
                        .path(assets::ICON_POWER_PATH)
                        .w(px(13.0))
                        .h(px(13.0))
                        .text_color(if failed {
                            Colors::status_error()
                        } else if bypassed {
                            Colors::text_disabled()
                        } else {
                            Colors::state_monitor()
                        }),
                )
                .on_click(move |_, w, cx| bypass(&bypass_target, w, cx))
                .occlude(),
        )
        .child(
            div()
                .flex_none()
                .w(px(14.0))
                .text_size(px(typography::DENSE_CAPTION))
                .font_weight(gpui::FontWeight::BOLD)
                .text_color(Colors::text_faint())
                .child(display_index.to_string()),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .truncate()
                .text_size(px(typography::UI_XS))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(if failed {
                    Colors::status_error()
                } else if bypassed {
                    Colors::text_disabled()
                } else {
                    Colors::text_primary()
                })
                .child(name),
        )
        .child(slot_icon_button(
            ("fx-slot-open", slot_index),
            assets::ICON_SLIDERS_HORIZONTAL_PATH,
            Colors::text_secondary(),
            move |w, cx| open_editor(&editor_target, w, cx),
        ))
        .child(slot_icon_button(
            ("fx-slot-remove", slot_index),
            assets::ICON_TRASH_PATH,
            Colors::text_faint(),
            move |w, cx| remove(&remove_target, w, cx),
        ))
        // Pressing anywhere else on the row opens the editor, so the name is a
        // target and not just a label.
        .on_click(move |_, w, cx| open(&open_target, w, cx))
}

/// One glyph button on a plug-in slot.
fn slot_icon_button(
    id: impl Into<gpui::ElementId>,
    icon: &'static str,
    tone: gpui::Rgba,
    on_click: impl Fn(&mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .w(px(20.0))
        .h(px(20.0))
        .rounded(px(crate::theme::radius::CONTROL))
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(|style| style.bg(Colors::state_hover()))
        .child(svg().path(icon).w(px(13.0)).h(px(13.0)).text_color(tone))
        .on_click(move |_, w, cx| on_click(w, cx))
        .occlude()
}

fn instrument_section(track: &TrackState, callbacks: &InspectorCallbacks) -> gpui::AnyElement {
    let slot = track.instrument_insert();
    let slot_name = if track.solfege.is_some() {
        "Solfege Engine".to_string()
    } else if slot.is_none() && track.builtin_soundfont_player {
        "Built-in Soundfont Player".to_string()
    } else {
        plugin_slot_name(slot, "No Instrument")
    };
    let mut section = section_rows()
        .child(kv_row("Plugin", slot_name))
        .child(kv_row(
            "Format",
            slot.map(plugin_format_label).unwrap_or("-").to_string(),
        ))
        .child(kv_row(
            "State",
            slot.map(plugin_state_label)
                .unwrap_or_else(|| "Empty".to_string()),
        ))
        .child(kv_row("MIDI Input", track.routing.midi_input.label()))
        .child(kv_row(
            "MIDI Ch",
            track
                .routing
                .midi_channel
                .map(|ch| ch.to_string())
                .unwrap_or_else(|| "All".to_string()),
        ))
        .child(kv_row("Output", track.routing.output.label()));

    if let Some(slot) = slot {
        section = section
            .child(fb_form_row(
                "VSTi Outputs",
                vsti_output_selector(slot, callbacks),
            ))
            // The instrument is a chain of one, so it gets the same row every
            // effect gets: power, name, editor, bin.
            .child(plugin_slot_row(
                track, slot, 0, 1, callbacks, false, false, true,
            ));
    } else if track.solfege.is_some() {
        section = section.child(kv_row("Details", "Open the Solfege tab"));
    } else if track.builtin_soundfont_player {
        let track_id = track.id.clone();
        let open = callbacks.on_open_soundfont_player.clone();
        section = section.child(compact_action_button(
            "soundfont-player-open",
            "Open",
            true,
            move |_, w, cx| open(&track_id.clone(), w, cx),
        ));
    } else {
        let track_id = track.id.clone();
        let picker = callbacks.on_open_insert_picker.clone();
        section = section.child(compact_action_button(
            "instrument-add",
            "Add Instrument",
            true,
            move |_, w, cx| picker(&(track_id.clone(), 0, true), w, cx),
        ));
    }

    section_card("instrument", "Instrument", callbacks, section).into_any_element()
}

/// Dedicated right-dock panel for the native Solfege instrument. Keeping this
/// outside the regular Inspector prevents the model/voice details from making
/// every track inspector excessively tall, while still following the same
/// track selection and cached FBMX metadata path.
pub fn solfege_panel(
    tracks: &[TrackState],
    selected_track_id: Option<&str>,
    active: bool,
    pitch: Option<SolfegePitchSummary>,
) -> impl IntoElement {
    let selected = selected_track_id
        .and_then(|id| tracks.iter().find(|track| track.id == id))
        .and_then(|track| track.solfege.as_ref().map(|solfege| (track, solfege)));

    let body = if let Some((track, solfege)) = selected {
        scroll_body()
            .child(inspector_header(track.color, track.name.clone(), "SOLFEGE"))
            .child(solfege_instrument_section(track, solfege))
            .children(pitch.map(solfege_note_pitch_section))
    } else {
        // The dock-wide empty state, so Solfege reads like Inspect and Chords
        // when it has nothing to show rather than like a third design.
        scroll_body().child(inspector_kit::ins_empty(
            assets::ICON_AUDIO_LINES_PATH,
            "Select a Solfege Engine track",
            "FBMX model, voice, preset, and performance details appear here.",
        ))
    };

    div()
        .flex()
        .flex_col()
        .h_full()
        .bg(Colors::surface_panel())
        .border_l(px(1.0))
        .border_color(if active {
            Colors::panel_border_focused()
        } else {
            Colors::border_subtle()
        })
        .child(body)
}

/// Concise pitch facts for the note the Pitch tab is editing.
///
/// Shown only while the Pitch tab is open with a note selected, so it does not
/// duplicate the piano roll's own note inspector rail during MIDI editing. Uses
/// the Inspector's `kv_row` scale, not the piano roll's smaller one, so the
/// dock keeps a single type scale.
fn solfege_note_pitch_section(pitch: SolfegePitchSummary) -> impl IntoElement {
    let (low, high) = pitch.range_cents;
    let mut note = inspector_kit::ins_section_container()
        .child(inspector_kit::ins_section_header(
            assets::ICON_MUSIC_PATH,
            "NOTE",
        ))
        .child(kv_row("Pitch", pitch.name.clone()))
        .child(kv_row("Start", format!("{:.3}", pitch.start_beats)))
        .child(kv_row("Length", format!("{:.3}", pitch.length_beats)));
    if let Some(articulation) = pitch.articulation {
        note = note.child(kv_row("Artic.", articulation));
    }
    let mut section = inspector_kit::ins_section_container()
        .child(inspector_kit::ins_section_header(
            assets::ICON_AUDIO_LINES_PATH,
            "PITCH",
        ))
        .child(kv_row(
            "Deviation",
            format!("{:+.0} ct", pitch.deviation_cents),
        ))
        .child(kv_row("Range", format!("{low:+.0} / {high:+.0} ct")));
    if pitch.point_count == 0 {
        // An empty curve is not "no pitch" — the note still sounds at its
        // notated pitch, and the Pitch tab draws that line.
        section = section.child(kv_row("Curve", "Baseline"));
    } else {
        section = section.child(kv_row("Curve", format!("{} points", pitch.point_count)));
    }
    // Two cards, stacked: NOTE and PITCH answer different questions and used to
    // share one container, which is why the second header read as a divider
    // rather than as a section of its own.
    div()
        .flex()
        .flex_col()
        .gap(px(inspector_kit::SECTION_GAP))
        .child(note)
        .child(section)
}

fn solfege_instrument_section(
    track: &TrackState,
    solfege: &crate::solfege::SolfegeTrackState,
) -> impl IntoElement {
    let model_path = solfege.model_path.as_deref();
    let model_file = model_path
        .and_then(|path| std::path::Path::new(path).file_name())
        .and_then(|name| name.to_str())
        .unwrap_or("No Solfege model");
    // Opening a packaged model is a 146 MB read and ~293 MB of SHA-256. This
    // only asks where that work has got to; the work itself runs on a worker
    // thread started by the first call.
    let model_state = model_path
        .map(std::path::Path::new)
        .map(crate::solfege::model_load_state);

    let mut section = inspector_kit::ins_section_container()
        .child(inspector_kit::ins_section_header(
            assets::ICON_AUDIO_LINES_PATH,
            "SOLFEGE ENGINE",
        ))
        .child(kv_row("Instrument", solfege.instrument.clone()))
        .child(kv_row("Voice", solfege.voice.clone()))
        .child(kv_row("Preset", solfege.preset.clone()))
        .child(kv_row("Format", "SFM / FBMX / native"))
        .child(kv_row("Model", model_file.to_string()))
        .child(kv_row(
            "Model Path",
            model_path.unwrap_or("Documents/Futureboard Studio/Utilities/Model/Solfage"),
        ))
        .child(kv_row("MIDI Input", track.routing.midi_input.label()))
        .child(kv_row(
            "MIDI Ch",
            track
                .routing
                .midi_channel
                .map(|ch| ch.to_string())
                .unwrap_or_else(|| "All".to_string()),
        ))
        .child(kv_row("Output", track.routing.output.label()));

    match model_state {
        Some(ModelLoadState::Ready(info)) => {
            // Built before the chain so the row can read the whole `info` while
            // the rows after it still move fields out of it.
            let architecture = solfege_runtime_architecture(model_path, &info);
            section = section
                .child(kv_row("State", "Loaded & verified"))
                .child(kv_row("Model Type", info.model_type))
                .child(kv_row("Architecture", architecture))
                .child(kv_row(
                    "Voicebank",
                    if info.voicebank_entries > 0 {
                        format!(
                            "{} profiles, {} clips, {:.1} MB PCM from {} source files",
                            info.voicebank_profiles,
                            info.voicebank_entries,
                            info.voicebank_audio_bytes as f32 / 1_000_000.0,
                            info.voicebank_source_files,
                        )
                    } else {
                        "None (physical-only)".to_string()
                    },
                ))
                .child(kv_row(
                    "Performer",
                    // Said plainly, because "yes" is not the useful part: a
                    // Performer changes what the track does before it makes any
                    // sound, and a user reading the inspector needs to know the
                    // notes may not be played exactly as written.
                    match info.performer.as_ref() {
                        Some(performer) => format!(
                            "{} — {} in, {} out, {:.1} kB{}",
                            performer.mode(),
                            performer.input_size,
                            performer.output_size,
                            performer.weight_bytes as f32 / 1000.0,
                            // Whether the Accent lane can reach it at all. A
                            // Performer built before accent existed still loads
                            // and still plays; it just plays the same way
                            // whatever the accents say, and a control that does
                            // nothing must say so rather than look available.
                            if performer.reads_accent() {
                                ", reads accent"
                            } else {
                                ", ignores accent (pre-v2 model)"
                            },
                        ),
                        None => "None (plays the score as written)".to_string(),
                    },
                ))
                .child(kv_row(
                    "Accent Analyzer",
                    // "None" here is the designed state, not a missing piece:
                    // Analyze Accent runs on a fitted rule built into the app,
                    // and a package only carries a model where one has been
                    // shown to beat it. Saying "None" alone would read as a
                    // broken instrument.
                    match info.accent.as_ref() {
                        Some(accent) => accent.summary(),
                        None => "Built-in rule (no model in this package)".to_string(),
                    },
                ))
                .child(kv_row("Sample Rate", format!("{} Hz", info.sample_rate)))
                .child(kv_row("Source", info.source_type))
                .child(kv_row(
                    "Validation",
                    if info.validated {
                        "Reference checked"
                    } else {
                        "Unvalidated"
                    },
                ))
                .child(kv_row(
                    "Parameters",
                    format!(
                        "{} ({:.1} MB)",
                        info.parameter_count,
                        info.file_size_bytes as f32 / 1_000_000.0
                    ),
                ));
        }
        Some(ModelLoadState::Loading(progress)) => {
            section = section.child(solfege_model_loading_rows(model_path, progress));
        }
        Some(ModelLoadState::Failed(error)) => {
            section = section.child(solfege_model_failure_rows(model_path, &error));
        }
        Some(ModelLoadState::Cancelled) => {
            section = section
                .child(kv_row("State", "Load cancelled"))
                .children(model_path.map(|path| solfege_model_retry_row(path, "Load model")));
        }
        None => {
            section = section.child(kv_row("State", "No model selected"));
        }
    }

    section
        .child(inspector_kit::ins_section_header(
            assets::ICON_GAUGE_PATH,
            "PERFORMANCE PARAMETERS",
        ))
        .child(kv_row(
            "Bow Pressure",
            format!("{:.0}%", solfege.bow_pressure * 100.0),
        ))
        .child(kv_row(
            "Vibrato",
            format!("{:.0}%", solfege.vibrato * 100.0),
        ))
        .child(kv_row(
            "Dynamics",
            format!("{:.0}%", solfege.dynamics * 100.0),
        ))
        .child(kv_row(
            "Expression",
            format!("{:.0}%", solfege.expression * 100.0),
        ))
}

/// What Studio will actually instantiate for this model, rather than which
/// sections the file happens to contain.
///
/// `SolfegeModelInfo::architecture` describes the *file*: `solfege::loading`
/// builds it from the sections present in the `.sfm`, so a packaged violin
/// reads "Neural voicebank + BowedString + embedded FBMX residual" even though
/// two of those three never run. The DAW loads every `.sfm` through
/// `sphere_direct_audio_engine::runtime::prepare_sfm_runtime` with
/// `SfmMode::VoicebankOnly`, and `solfege_engine::SamplerEngine::
/// prepare_sfm_staged` leaves both `instrument` and the embedded residual
/// `None` in that mode. The BowedString profile and the `FBMX` section are read
/// and verified by the loader, then not instantiated.
///
/// The other two shapes are equally real:
///
/// - an `.sfm` with no `INDX`/`AUDO` voicebank fails `prepare_sfm_runtime`
///   outright, and `RuntimeSolfegeEngine::new` keeps the default BowedString
///   engine it built as a fallback;
/// - a standalone `.fbmx` runs on that same fallback, with the model applied
///   over it by `RuntimeSolfegeEngine::render_segment_stereo`.
///
/// `has_voicebank` is recovered from the "Neural voicebank" prefix that
/// `solfege::loading::load_sfm` writes when `INDX` and `AUDO` are both present
/// — the same condition `prepare_sfm_runtime` requires. Coupling through prose
/// is the weak part of this: the durable fix is a typed flag on
/// [`SolfegeModelInfo`], which lives outside the files this change owns. The
/// prefix test fails safe — an unrecognised string falls into the
/// missing-voicebank branch, which describes the fallback engine.
fn solfege_runtime_architecture(model_path: Option<&str>, info: &SolfegeModelInfo) -> String {
    let is_sfm = model_path
        .and_then(|path| std::path::Path::new(path).extension())
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("sfm"));
    if !is_sfm {
        // Standalone `.fbmx`: `architecture` here is the model's own
        // architecture name, and it does run — over the fallback instrument.
        return format!("BowedString physical + {} residual", info.architecture);
    }
    if !info.architecture.starts_with("Neural voicebank") {
        return "BowedString physical fallback — packaged voicebank missing".to_string();
    }
    // `parameter_count` is set only from the embedded residual section, so it
    // says whether one is packaged — not whether it is used.
    if info.parameter_count > 0 {
        "Neural voicebank playback (BowedString + FBMX residual present, not instantiated)"
            .to_string()
    } else {
        "Neural voicebank playback (BowedString present, not instantiated)".to_string()
    }
}

/// The load in progress: which stage, how far through the whole load, and a
/// way out of it.
///
/// The percentage is bytes read and hashed, not a stage counter — on the
/// shipped violin the `AUDO` digest is the load, so a stage counter would sit
/// still for the part that actually takes the time.
fn solfege_model_loading_rows(
    model_path: Option<&str>,
    progress: crate::solfege::ModelLoadProgress,
) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .child(kv_row(
            "State",
            format!(
                "{} — {:.0}%",
                progress.stage.label(),
                progress.percent().floor()
            ),
        ))
        .child(
            div()
                .my(px(3.0))
                .h(px(3.0))
                .rounded(px(crate::theme::radius::PILL))
                .overflow_hidden()
                .bg(Colors::border_subtle())
                .child(
                    div()
                        .h_full()
                        .w(gpui::relative(progress.fraction))
                        .rounded(px(crate::theme::radius::PILL))
                        .bg(Colors::accent_primary()),
                ),
        )
        .children(model_path.map(|path| {
            let path = std::path::PathBuf::from(path);
            div().pt(px(2.0)).child(compact_action_button(
                "solfege-model-cancel",
                "Cancel Load",
                true,
                move |_, _, _| crate::solfege::loading::cancel_model_load(&path),
            ))
        }))
        // Progress is published from a worker thread and changes no entity, so
        // the panel schedules its own repaint for as long as a load is running.
        .child(
            gpui::canvas(
                |_, _, cx: &mut App| crate::solfege::loading::ensure_progress_repaint(cx),
                |_, _, _, _| {},
            )
            .h(px(0.0)),
        )
}

/// A failure names the stage and the cause, because "failed to load" does not
/// tell a truncated copy apart from a corrupt section.
fn solfege_model_failure_rows(
    model_path: Option<&str>,
    error: &crate::solfege::ModelLoadError,
) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .child(kv_row(
            "State",
            format!("Failed while {}", error.stage.label().to_lowercase()),
        ))
        .child(
            div()
                .py(px(3.0))
                .text_size(px(10.5))
                .text_color(Colors::status_error())
                .child(error.to_string()),
        )
        .children(model_path.map(|path| solfege_model_retry_row(path, "Retry Load")))
}

fn solfege_model_retry_row(path: &str, label: &'static str) -> impl IntoElement {
    let path = std::path::PathBuf::from(path);
    div().pt(px(2.0)).child(compact_action_button(
        "solfege-model-retry",
        label,
        true,
        move |_, _, _| crate::solfege::loading::retry_model_load(&path),
    ))
}

fn insert_effects_section(track: &TrackState, callbacks: &InspectorCallbacks) -> impl IntoElement {
    let effect_start = if track.track_type == TrackType::Instrument {
        1
    } else {
        0
    };
    let mut rows = section_rows();

    let effects = track.effect_inserts();
    if effects.is_empty() {
        rows = rows.child(inspector_kit::ins_value_muted("No effects"));
    } else {
        for (offset, slot) in effects.iter().enumerate() {
            let slot_index = effect_start + offset;
            rows = rows.child(plugin_slot_row(
                track,
                slot,
                slot_index,
                offset + 1,
                callbacks,
                slot_index > effect_start,
                slot_index + 1 < track.inserts.len(),
                false,
            ));
        }
        // Trailing drop zone so a dragged slot can land at the very end of the
        // chain (insertion gap == inserts.len()); rows above only cover the
        // gaps before each slot. Same-track guarded; shows the accent line.
        let end_track = track.id.clone();
        let end_can_track = track.id.clone();
        let end_reorder = callbacks.on_reorder_insert.clone();
        let end_gap = track.inserts.len();
        rows = rows.child(
            div()
                .id("fx-drop-end")
                .h(px(8.0))
                .can_drop(move |dragged, _window, _cx| {
                    dragged
                        .downcast_ref::<FxSlotDrag>()
                        .is_some_and(|d| d.track_id == end_can_track)
                })
                .drag_over::<FxSlotDrag>(|style, _drag, _window, _cx| drop_over_highlight(style))
                .on_drop::<FxSlotDrag>(move |drag, window, cx| {
                    if drag.track_id == end_track {
                        end_reorder(
                            &(end_track.clone(), drag.insert_id.clone(), end_gap),
                            window,
                            cx,
                        );
                    }
                }),
        );
    }

    let track_id = track.id.clone();
    let next_slot = track.inserts.len().max(effect_start);
    let picker = callbacks.on_open_insert_picker.clone();
    section_card(
        "inserts",
        "Insert Effects",
        callbacks,
        rows.child(compact_action_button(
            "effect-add",
            "Add Effect",
            true,
            move |_, w, cx| picker(&(track_id.clone(), next_slot, false), w, cx),
        )),
    )
}

#[allow(clippy::too_many_arguments)]
fn track_inspector(
    track: &TrackState,
    connections: &AudioConnectionRegistry,
    name_input: &TextInputState,
    name_focused: bool,
    name_callbacks: TextInputCallbacks,
    instrument_targets: &[(String, String)],
    color_picker: &InspectorColorPicker<'_>,
    callbacks: &InspectorCallbacks,
    i18n: I18n,
) -> impl IntoElement {
    let automation_points: usize = track
        .automation_lanes
        .iter()
        .map(|lane| lane.points.len())
        .sum();
    let tid = track.id.clone();

    // ── Volume slider + dB readout ──────────────────────────────────────
    // When Track Volume automation is reading, the slider/readout follow the
    // effective (automation) value and an `[A]` marker plus a separate base
    // readout make the manual value clear. Otherwise it shows the base only.
    let volume_row = {
        let start = callbacks.on_volume_drag_start.clone();
        let preview = callbacks.on_volume_drag_preview.clone();
        let commit = callbacks.on_volume_drag_commit.clone();
        let tid_start = tid.clone();
        let tid_preview = tid.clone();
        let tid_commit = tid.clone();
        let has_volume_automation = track.has_active_volume_automation();
        let automation_active = track.volume_automation_read && has_volume_automation;
        let display_vol = track.display_volume();
        // `[A]` automation-read toggle — only meaningful when a volume lane
        // exists. Lit when reading, dim when bypassed.
        let auto_toggle = has_volume_automation.then(|| {
            let read_cb = callbacks.on_toggle_volume_automation_read.clone();
            let tid_a = tid.clone();
            let on = track.volume_automation_read;
            div()
                .id("inspector-vol-automation-read")
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .w(px(18.0))
                .h(px(16.0))
                .rounded(px(crate::theme::radius::CONTROL))
                .border(px(1.0))
                .border_color(if on {
                    Colors::accent_primary()
                } else {
                    Colors::border_default()
                })
                .bg(if on {
                    Colors::with_alpha(Colors::accent_primary(), 0.18)
                } else {
                    Colors::with_alpha(Colors::surface_canvas(), 0.3)
                })
                .text_size(px(typography::DENSE_CAPTION))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(if on {
                    Colors::accent_primary()
                } else {
                    Colors::text_muted()
                })
                .cursor(gpui::CursorStyle::PointingHand)
                .child("A")
                .on_mouse_down(gpui::MouseButton::Left, move |_ev, w, cx| {
                    read_cb(&tid_a, w, cx)
                })
        });
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .child(slider_with_drag_callbacks(
                "inspector-volume",
                display_vol,
                track.color,
                Some(move |v: &f32, w: &mut Window, cx: &mut App| {
                    start(&(tid_start.clone(), *v), w, cx)
                }),
                Some(move |v: &f32, w: &mut Window, cx: &mut App| {
                    preview(&(tid_preview.clone(), *v), w, cx)
                }),
                Some(move |w: &mut Window, cx: &mut App| commit(&tid_commit, w, cx)),
                None::<fn(&mut Window, &mut App)>,
            ))
            .child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .flex_col()
                    .items_end()
                    .min_w(px(48.0))
                    .text_size(px(typography::DENSE_LABEL))
                    .text_color(Colors::text_secondary())
                    .child({
                        let mut label = i18n.tr_vars(
                            "inspector.volume.db",
                            &[("db", volume::format_db(display_vol))],
                        );
                        if automation_active {
                            label.push_str(" [A]");
                        }
                        label
                    })
                    .when(has_volume_automation, |this| {
                        this.child(
                            div()
                                .text_size(px(8.0))
                                .text_color(Colors::text_faint())
                                .child(format!("Base {} dB", volume::format_db(track.volume))),
                        )
                    }),
            )
            .when_some(auto_toggle, |row, toggle| row.child(toggle))
    };

    // ── Pan slider (bipolar, centre-detented) + readout ─────────────────
    //
    // The value travels in real -1..1 pan units end to end: the control fills
    // outward from the rail centre, so neutral reads as an empty rail rather
    // than a half-full one, and no 0..1 remapping happens at this call site.
    let pan_row = {
        let start = callbacks.on_pan_drag_start.clone();
        let preview = callbacks.on_pan_drag_preview.clone();
        let commit = callbacks.on_pan_drag_commit.clone();
        // `on_pan` runs one `EditCommand::SetTrackPan` through the edit stack —
        // exactly what a double-click reset should record, and the same path
        // the TrackHeader pan pill resets through.
        let reset = callbacks.on_pan.clone();
        let tid_start = tid.clone();
        let tid_preview = tid.clone();
        let tid_commit = tid.clone();
        let tid_reset = tid.clone();
        let pan_readout = format_pan(i18n, track.pan);
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(space::BASE))
            .child(bipolar_slider_with_drag_callbacks(
                "inspector-pan",
                track.pan,
                track.color,
                Some(pan_readout.clone()),
                Some(move |_: &f32, w: &mut Window, cx: &mut App| start(&tid_start, w, cx)),
                Some(move |v: &f32, w: &mut Window, cx: &mut App| {
                    preview(&(tid_preview.clone(), *v), w, cx);
                }),
                Some(move |w: &mut Window, cx: &mut App| commit(&tid_commit, w, cx)),
                Some(move |w: &mut Window, cx: &mut App| reset(&(tid_reset.clone(), 0.0), w, cx)),
            ))
            .child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .justify_end()
                    // Right-aligned on the same 48 px column as the volume
                    // readout above, so the two numbers in this form line up.
                    .min_w(px(48.0))
                    .text_size(px(typography::DENSE_LABEL))
                    .text_color(Colors::text_secondary())
                    .child(pan_readout),
            )
    };

    // ── M / S / I / R ───────────────────────────────────────────────────
    //
    // The console's colours, the same four this track already wears in the
    // mixer: blue mute, amber solo, green input, red record. A player reads the
    // colour before the letter, so the two views must not disagree about which
    // is which.
    let state_row = {
        let mute = callbacks.on_toggle_mute.clone();
        let solo = callbacks.on_toggle_solo.clone();
        let arm = callbacks.on_toggle_arm.clone();
        let input = callbacks.on_toggle_input.clone();
        let (t1, t2, t3, t4) = (tid.clone(), tid.clone(), tid.clone(), tid.clone());
        div()
            .flex()
            .flex_row()
            .gap(px(space::SNUG))
            .child(inspector_kit::ins_state_button(
                "inspector-mute".into(),
                i18n.tr("inspector.badge.mute"),
                track.muted,
                Colors::state_mute(),
                move |_, w, cx| mute(&t1, w, cx),
            ))
            .child(inspector_kit::ins_state_button(
                "inspector-solo".into(),
                i18n.tr("inspector.badge.solo"),
                track.solo,
                Colors::state_solo(),
                move |_, w, cx| solo(&t2, w, cx),
            ))
            .child(inspector_kit::ins_state_button(
                "inspector-input".into(),
                i18n.tr("inspector.badge.input"),
                track.input_monitor.is_active(track.armed),
                Colors::state_monitor(),
                move |_, w, cx| input(&t4, w, cx),
            ))
            .child(inspector_kit::ins_state_button(
                "inspector-arm".into(),
                i18n.tr("inspector.badge.record"),
                track.armed,
                Colors::state_arm(),
                move |_, w, cx| arm(&t3, w, cx),
            ))
    };

    let type_label = track_type_label(i18n, track.track_type);
    scroll_body()
        .child(inspector_header(
            track_type_color(track.track_type),
            track.name.clone(),
            type_label.clone(),
        ))
        .child(section_card(
            "track",
            &i18n.tr("inspector.section.track"),
            callbacks,
            section_rows()
                .child(fb_form_row(
                    i18n.tr("inspector.field.type"),
                    inspector_kit::ins_value(type_label),
                ))
                .child(fb_form_row(
                    "Name",
                    text_field_with_callbacks(name_input, name_focused, name_callbacks),
                ))
                .child(fb_form_row(i18n.tr("inspector.field.volume"), volume_row))
                .child(fb_form_row(i18n.tr("inspector.field.pan"), pan_row))
                .child(fb_form_row("Color", color_field(color_picker)))
                .child(fb_form_row(i18n.tr("inspector.section.state"), state_row)),
        ))
        .child(routing_section(
            track,
            connections,
            instrument_targets,
            callbacks,
        ))
        .when(
            crate::edition::professional_features_available()
                && matches!(track.track_type, TrackType::Midi | TrackType::Instrument),
            |this| this.child(mpe_section(track, callbacks)),
        )
        .when(track.track_type == TrackType::Instrument, |this| {
            this.child(instrument_section(track, callbacks))
        })
        .when(
            matches!(track.track_type, TrackType::Audio | TrackType::Instrument),
            |this| this.child(insert_effects_section(track, callbacks)),
        )
        .child(section_card(
            "contents",
            "Contents",
            callbacks,
            section_rows()
                .child(kv_row(
                    i18n.tr("inspector.field.clips"),
                    track.clips.len().to_string(),
                ))
                .child(kv_row("Inserts", track.effect_inserts().len().to_string()))
                .child(kv_row("Sends", track.sends.len().to_string()))
                .child(kv_row(
                    "Automation Lanes",
                    track.automation_lanes.len().to_string(),
                ))
                .child(kv_row("Automation Points", automation_points.to_string())),
        ))
}

/// A clip-inspector section.
///
/// Same card as a track's, minus the fold: a clip inspector is three short
/// sections that all fit at once, so a switch on each would be a control with
/// nothing to gain by pressing it.
fn inspector_section(label: impl Into<String>, child: impl IntoElement) -> impl IntoElement {
    inspector_kit::ins_card(label, child)
}

fn compact_property_row(label: impl Into<String>, child: impl IntoElement) -> impl IntoElement {
    inspector_kit::ins_row(label, child)
}

fn readonly_value(text: impl Into<String>) -> impl IntoElement {
    // The old version reserved 64 px of right padding to dodge a stepper that
    // is not in every row, which pulled the value away from the column every
    // other readout aligns on.
    inspector_kit::ins_value(text)
}

fn beat_stepper(
    id: &'static str,
    clip_id: &str,
    value: f32,
    preview_callback: ClipF32Cb,
    callbacks: &InspectorCallbacks,
    min_value: f32,
) -> impl IntoElement {
    let clip_id = clip_id.to_string();
    let start_id = clip_id.clone();
    let preview_id = clip_id.clone();
    let commit_id = clip_id.clone();
    let begin = callbacks.on_begin_clip_property_drag.clone();
    let commit = callbacks.on_commit_clip_property_drag.clone();
    inspector_numeric_stepper_with_drag_callbacks(
        id,
        value as f64,
        format!("{value:.2} bt"),
        min_value as f64,
        99_999.0,
        0.25,
        false,
        Some(Arc::new(move |w, cx| begin(&start_id, w, cx))),
        Arc::new(move |next, w, cx| preview_callback(&(preview_id.clone(), next as f32), w, cx)),
        Some(Arc::new(move |w, cx| commit(&commit_id, w, cx))),
    )
}

fn linear_gain_to_db(gain: f32) -> f32 {
    if gain <= 0.000_001 {
        -60.0
    } else {
        20.0 * gain.log10()
    }
}

fn db_to_linear_gain(db: f32) -> f32 {
    10.0_f32.powf(db / 20.0)
}

fn gain_stepper(
    clip_id: &str,
    gain: f32,
    preview_callback: ClipF32Cb,
    callbacks: &InspectorCallbacks,
) -> impl IntoElement {
    let db = linear_gain_to_db(gain);
    let clip_id = clip_id.to_string();
    let start_id = clip_id.clone();
    let preview_id = clip_id.clone();
    let commit_id = clip_id.clone();
    let begin = callbacks.on_begin_clip_property_drag.clone();
    let commit = callbacks.on_commit_clip_property_drag.clone();
    inspector_numeric_stepper_with_drag_callbacks(
        "clip-gain",
        db as f64,
        format!("{db:.1} dB"),
        -60.0,
        12.0,
        1.0,
        false,
        Some(Arc::new(move |w, cx| begin(&start_id, w, cx))),
        Arc::new(move |next, w, cx| {
            preview_callback(&(preview_id.clone(), db_to_linear_gain(next as f32)), w, cx)
        }),
        Some(Arc::new(move |w, cx| commit(&commit_id, w, cx))),
    )
}

#[allow(clippy::too_many_arguments)]
fn clip_stretch_stepper(
    id: &'static str,
    clip_id: &str,
    value: f64,
    display: impl Into<String>,
    min: f64,
    max: f64,
    step: f64,
    disabled: bool,
    callbacks: &InspectorCallbacks,
    build_next: impl Fn(f64) -> AudioClipStretchState + 'static,
) -> impl IntoElement {
    let start_id = clip_id.to_string();
    let preview_id = start_id.clone();
    let commit_id = start_id.clone();
    let begin = callbacks.on_begin_clip_property_drag.clone();
    let preview = callbacks.on_preview_clip_stretch.clone();
    let commit = callbacks.on_commit_clip_property_drag.clone();
    inspector_numeric_stepper_with_drag_callbacks(
        id,
        value,
        display,
        min,
        max,
        step,
        disabled,
        Some(Arc::new(move |w, cx| begin(&start_id, w, cx))),
        Arc::new(move |next, w, cx| preview(&(preview_id.clone(), build_next(next)), w, cx)),
        Some(Arc::new(move |w, cx| commit(&commit_id, w, cx))),
    )
}

fn truncate_value(text: impl Into<String>) -> impl IntoElement {
    div()
        .min_w(px(0.0))
        .truncate()
        .text_size(px(11.0))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(Colors::text_primary())
        .child(text.into())
}

// ── Audio-clip stretch inspector controls (Slice 2) ─────────────────────────
//
// Every control produces a fully-formed next `AudioClipStretchState` and routes
// it through the single `on_set_clip_stretch` callback, so each edit is one
// undo entry. The controls edit real persisted state; the audio engine wires it
// to playback/export in a later slice (the Stretch section says so honestly).

#[derive(Clone, Copy, PartialEq, Eq)]
enum StretchUiMode {
    Off,
    Resample,
    TempoSync,
    Manual,
    Warp,
}

const STRETCH_MODE_OPTIONS: &[InspectorSelectOption<StretchUiMode>] = &[
    InspectorSelectOption {
        label: "Off",
        value: StretchUiMode::Off,
    },
    InspectorSelectOption {
        label: "Resample",
        value: StretchUiMode::Resample,
    },
    InspectorSelectOption {
        label: "Tempo Sync",
        value: StretchUiMode::TempoSync,
    },
    InspectorSelectOption {
        label: "Manual",
        value: StretchUiMode::Manual,
    },
    InspectorSelectOption {
        label: "Warp",
        value: StretchUiMode::Warp,
    },
];

fn mode_supports_preserve_pitch(mode: StretchMode) -> bool {
    matches!(
        mode,
        StretchMode::Manual | StretchMode::TempoSync | StretchMode::Warp
    )
}

fn stretch_ui_mode(s: &AudioClipStretchState) -> StretchUiMode {
    match s.mode {
        StretchMode::Off => StretchUiMode::Off,
        StretchMode::Resample => StretchUiMode::Resample,
        StretchMode::TempoSync => StretchUiMode::TempoSync,
        StretchMode::Manual => StretchUiMode::Manual,
        StretchMode::Warp => StretchUiMode::Warp,
    }
}

fn with_ui_mode(s: &AudioClipStretchState, mode: StretchUiMode) -> AudioClipStretchState {
    let mut n = s.clone();
    match mode {
        StretchUiMode::Off => {
            n.mode = StretchMode::Off;
            n.preserve_pitch = false;
            n.algorithm = StretchAlgorithm::Auto;
        }
        StretchUiMode::Resample => {
            n.mode = StretchMode::Resample;
            n.preserve_pitch = false;
            n.algorithm = StretchAlgorithm::ResampleOnly;
        }
        StretchUiMode::TempoSync => {
            n.mode = StretchMode::TempoSync;
            n.preserve_pitch = true;
            n.algorithm = StretchAlgorithm::PhaseVocoder;
        }
        StretchUiMode::Manual => {
            n.mode = StretchMode::Manual;
            if matches!(n.algorithm, StretchAlgorithm::Auto) {
                n.algorithm = if n.preserve_pitch {
                    StretchAlgorithm::PhaseVocoder
                } else {
                    StretchAlgorithm::ResampleOnly
                };
            }
        }
        StretchUiMode::Warp => {
            n.mode = StretchMode::Warp;
            n.preserve_pitch = true;
            n.algorithm = StretchAlgorithm::PhaseVocoder;
        }
    }
    n.clip_timeline_duration_beats = 0.0;
    n.dirty = true;
    n
}

fn with_preserve_pitch(s: &AudioClipStretchState, enabled: bool) -> AudioClipStretchState {
    let mut next = s.clone();
    next.preserve_pitch = enabled && mode_supports_preserve_pitch(next.mode);
    next.algorithm = if next.preserve_pitch {
        StretchAlgorithm::PhaseVocoder
    } else if next.mode == StretchMode::Off {
        StretchAlgorithm::Auto
    } else {
        StretchAlgorithm::ResampleOnly
    };
    next.dirty = true;
    next
}

fn with_mode(s: &AudioClipStretchState, mode: StretchMode) -> AudioClipStretchState {
    let ui_mode = match mode {
        StretchMode::Off => StretchUiMode::Off,
        StretchMode::Resample => StretchUiMode::Resample,
        StretchMode::TempoSync => StretchUiMode::TempoSync,
        StretchMode::Manual => StretchUiMode::Manual,
        StretchMode::Warp => StretchUiMode::Warp,
    };
    with_ui_mode(s, ui_mode)
}

fn stretch_backend_summary(s: &AudioClipStretchState) -> &'static str {
    if s.mode == StretchMode::Off {
        "Off"
    } else if s.preserve_pitch && mode_supports_preserve_pitch(s.mode) {
        "Signalsmith"
    } else {
        "Internal RePitch"
    }
}

fn seconds_to_time_label(seconds: f64) -> String {
    let seconds = seconds.max(0.0);
    let minutes = (seconds / 60.0).floor() as u64;
    let rem = seconds - minutes as f64 * 60.0;
    format!("{minutes:02}:{rem:06.3}")
}

fn stretch_length_summary(
    s: &AudioClipStretchState,
    project_bpm: f64,
    fallback_duration_seconds: Option<f64>,
) -> String {
    let sample_rate = s.project_sample_rate.max(s.original_sample_rate).max(1) as f64;
    let source_seconds = if s.source_len_samples() > 0 {
        s.source_len_samples() as f64 / sample_rate
    } else {
        fallback_duration_seconds.unwrap_or(0.0)
    };
    let new_seconds = source_seconds * s.effective_time_ratio(project_bpm).max(0.0);
    format!(
        "{} -> {}",
        seconds_to_time_label(source_seconds),
        seconds_to_time_label(new_seconds)
    )
}

fn stretch_field_block(label: impl Into<String>, control: impl IntoElement) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(3.0))
        .py(px(2.0))
        .child(
            div()
                .text_size(px(typography::DENSE_LABEL))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(Colors::text_muted())
                .child(label.into()),
        )
        .child(control)
}

fn stretch_metric_row(label: impl Into<String>, value: impl Into<String>) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(8.0))
        .min_w(px(0.0))
        .child(
            div()
                .text_size(px(typography::DENSE_LABEL))
                .text_color(Colors::text_muted())
                .child(label.into()),
        )
        .child(
            div()
                .min_w(px(0.0))
                .truncate()
                .text_size(px(10.5))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(Colors::text_secondary())
                .child(value.into()),
        )
}

/// STRETCH section body — the common time/pitch controls plus tempo and warp
/// affordances. Every control writes the authoritative clip state.
fn stretch_section_body(
    clip: &SelectedClipSummary<'_>,
    s: &AudioClipStretchState,
    project_bpm: f64,
    tempo: &StretchTempoUiSnapshot,
    cb: &ClipStretchCb,
    callbacks: &InspectorCallbacks,
) -> impl IntoElement {
    let clip_id = clip.clip_id.to_string();
    let stretch_enabled = s.mode != StretchMode::Off;
    let ui_mode = stretch_ui_mode(s);
    let preserve_mode = s.preserve_pitch && mode_supports_preserve_pitch(s.mode);
    let preserve_pitch_available = stretch_enabled && mode_supports_preserve_pitch(s.mode);
    let warp_mode = s.mode == StretchMode::Warp;
    let (semi, fine) = s.pitch_semi_and_cents();
    let cur_src_bpm = s
        .bpm_source
        .or(tempo.suggested_bpm.map(f64::from))
        .unwrap_or(project_bpm);
    let src_bpm_label = s
        .bpm_source
        .map(|b| format!("{b:.2}"))
        .or_else(|| tempo.suggested_bpm.map(|b| format!("{b:.2} ?")))
        .unwrap_or_else(|| "—".to_string());
    let target_display = if matches!(s.mode, StretchMode::TempoSync) || s.bpm_target.is_none() {
        format!("Project {project_bpm:.2}")
    } else {
        format!("Manual {:.2}", s.bpm_target.unwrap_or(project_bpm))
    };
    let ratio = s.effective_time_ratio(project_bpm);
    let amount_percent = if s.stretch_percent().is_finite() {
        s.stretch_percent().clamp(5.0, 2_000.0)
    } else {
        100.0
    };
    let length_summary = stretch_length_summary(s, project_bpm, clip.source_duration_seconds);
    let backend = stretch_backend_summary(s);
    let pitch_summary = format!("{:+.2} st / {:+.0} ct", semi, fine);
    let mut fit_selection = s.clone();
    let fit_selection_enabled = clip
        .selection_duration_beats
        .map(|beats| fit_selection.fit_to_timeline_beats(beats as f64, project_bpm))
        .unwrap_or(false);
    let mut fit_clip = s.clone();
    let fit_clip_enabled = fit_clip.fit_to_timeline_beats(clip.duration_beats as f64, project_bpm);
    let mut reset = s.clone();
    reset.reset_stretch_defaults();
    let auto_find = callbacks.on_clip_stretch_auto_find_bpm.clone();
    let fit_project = callbacks.on_clip_stretch_fit_project.clone();
    let auto_find_id = clip_id.clone();
    let fit_project_id = clip_id.clone();
    let finding = tempo.finding;
    let auto_find_label = if finding { "Finding..." } else { "Auto Find" };

    div()
        .flex()
        .flex_col()
        .gap(px(7.0))
        .child(stretch_field_block(
            "Enable Stretch",
            shared_inspector_checkbox(
                "clip-stretch-enabled",
                stretch_enabled,
                false,
                if stretch_enabled { "On" } else { "Off" },
                {
                    let s = s.clone();
                    let cb = cb.clone();
                    let clip_id = clip_id.clone();
                    move |checked, w, cx| {
                        let next = if checked {
                            with_ui_mode(&s, StretchUiMode::Manual)
                        } else {
                            with_ui_mode(&s, StretchUiMode::Off)
                        };
                        cb(&(clip_id.clone(), next), w, cx);
                    }
                },
            ),
        ))
        .child(stretch_field_block(
            "Mode",
            inspector_select("clip-stretch-mode", ui_mode, STRETCH_MODE_OPTIONS, false, {
                let s = s.clone();
                let cb = cb.clone();
                let clip_id = clip_id.clone();
                move |mode, w, cx| {
                    cb(&(clip_id.clone(), with_ui_mode(&s, mode)), w, cx);
                }
            }),
        ))
        .child(stretch_field_block(
            "Amount",
            clip_stretch_stepper(
                "clip-stretch-amount",
                &clip_id,
                amount_percent,
                format!("{amount_percent:.1}%"),
                5.0,
                2_000.0,
                1.0,
                !stretch_enabled || s.mode == StretchMode::TempoSync,
                callbacks,
                {
                    let s = s.clone();
                    move |percent| {
                        let mut next = s.clone();
                        next.set_stretch_percent(percent);
                        next.clip_timeline_duration_beats = 0.0;
                        next
                    }
                },
            ),
        ))
        .child(
            div()
                .text_size(px(typography::DENSE_LABEL))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(Colors::text_muted())
                .child("Tempo"),
        )
        .child(stretch_field_block(
            "Source BPM",
            div()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(4.0))
                        .child(inspector_mini_button(
                            "clip-stretch-auto-tempo",
                            auto_find_label,
                            !finding,
                            move |_, w, cx| auto_find(&auto_find_id, w, cx),
                        ))
                        .child(clip_stretch_stepper(
                            "clip-stretch-srcbpm",
                            &clip_id,
                            cur_src_bpm,
                            src_bpm_label,
                            1.0,
                            999.0,
                            1.0,
                            !stretch_enabled,
                            callbacks,
                            {
                                let s = s.clone();
                                move |bpm| {
                                    let mut next = s.clone();
                                    next.bpm_source = Some(bpm);
                                    next.clip_timeline_duration_beats = 0.0;
                                    next.dirty = true;
                                    next
                                }
                            },
                        )),
                )
                .children(
                    tempo
                        .error
                        .as_ref()
                        .map(|error| inspector_hint_text(format!("{error}"))),
                )
                .children(tempo.confidence.map(|confidence| {
                    inspector_hint_text(format!(
                        "Detected confidence: {:.0}%{}",
                        confidence * 100.0,
                        if tempo.low_confidence { " (low)" } else { "" }
                    ))
                }))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap(px(4.0))
                        .child({
                            let s = s.clone();
                            let cb = cb.clone();
                            let clip_id = clip_id.clone();
                            inspector_mini_button(
                                "clip-stretch-bpm-half",
                                "x0.5",
                                s.bpm_source.is_some(),
                                move |_, w, cx| {
                                    let Some(bpm) = s.bpm_source else {
                                        return;
                                    };
                                    let mut next = s.clone();
                                    next.bpm_source = Some((bpm * 0.5).clamp(1.0, 999.0));
                                    next.clip_timeline_duration_beats = 0.0;
                                    next.dirty = true;
                                    cb(&(clip_id.clone(), next), w, cx);
                                },
                            )
                        })
                        .child({
                            let s = s.clone();
                            let cb = cb.clone();
                            let clip_id = clip_id.clone();
                            inspector_mini_button(
                                "clip-stretch-bpm-double",
                                "x2",
                                s.bpm_source.is_some(),
                                move |_, w, cx| {
                                    let Some(bpm) = s.bpm_source else {
                                        return;
                                    };
                                    let mut next = s.clone();
                                    next.bpm_source = Some((bpm * 2.0).clamp(1.0, 999.0));
                                    next.clip_timeline_duration_beats = 0.0;
                                    next.dirty = true;
                                    cb(&(clip_id.clone(), next), w, cx);
                                },
                            )
                        })
                        .child({
                            let s = s.clone();
                            let cb = cb.clone();
                            let clip_id = clip_id.clone();
                            inspector_mini_button(
                                "clip-stretch-bpm-match-project",
                                "Match Project",
                                true,
                                move |_, w, cx| {
                                    let mut next = s.clone();
                                    next.bpm_source = Some(project_bpm);
                                    next.clip_timeline_duration_beats = 0.0;
                                    next.dirty = true;
                                    cb(&(clip_id.clone(), next), w, cx);
                                },
                            )
                        }),
                )
                .children((!tempo.alternatives.is_empty()).then(|| {
                    let cb = cb.clone();
                    let s = s.clone();
                    let clip_id = clip_id.clone();
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(3.0))
                        .child(
                            div()
                                .text_size(px(typography::DENSE_LABEL))
                                .text_color(Colors::text_faint())
                                .child("Alternatives"),
                        )
                        .child(div().flex().flex_row().flex_wrap().gap(px(4.0)).children(
                            tempo.alternatives.iter().map(|alt| {
                                let alt = *alt as f64;
                                let label = format!("{alt:.1}");
                                let cb = cb.clone();
                                let s = s.clone();
                                let clip_id = clip_id.clone();
                                inspector_mini_button(
                                    format!("clip-stretch-alt-{alt:.1}"),
                                    label,
                                    true,
                                    move |_, w, cx| {
                                        let mut next = s.clone();
                                        next.bpm_source = Some(alt);
                                        next.clip_timeline_duration_beats = 0.0;
                                        next.dirty = true;
                                        cb(&(clip_id.clone(), next), w, cx);
                                    },
                                )
                            }),
                        ))
                })),
        ))
        .child(stretch_field_block(
            "Target BPM",
            div()
                .h(px(24.0))
                .flex()
                .items_center()
                .px(px(7.0))
                .rounded(px(crate::theme::radius::CONTROL))
                .border(px(1.0))
                .border_color(Colors::border_subtle())
                .bg(Colors::surface_input())
                .text_size(px(11.0))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(Colors::text_secondary())
                .child(target_display.clone()),
        ))
        .child(stretch_field_block(
            "Fit",
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap(px(4.0))
                .child(inspector_mini_button(
                    "clip-fit-project-tempo",
                    "Fit Project",
                    !finding,
                    move |_, w, cx| fit_project(&fit_project_id, w, cx),
                ))
                .child(inspector_mini_button(
                    "clip-fit-selection",
                    "Fit Selection",
                    fit_selection_enabled,
                    {
                        let cb = cb.clone();
                        let clip_id = clip_id.clone();
                        move |_, w, cx| {
                            cb(&(clip_id.clone(), fit_selection.clone()), w, cx);
                        }
                    },
                ))
                .child(inspector_mini_button(
                    "clip-fit-length",
                    "Fit Clip",
                    fit_clip_enabled,
                    {
                        let cb = cb.clone();
                        let clip_id = clip_id.clone();
                        move |_, w, cx| {
                            cb(&(clip_id.clone(), fit_clip.clone()), w, cx);
                        }
                    },
                )),
        ))
        .child(
            div()
                .text_size(px(typography::DENSE_LABEL))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(Colors::text_muted())
                .child("Pitch"),
        )
        .children(preserve_pitch_available.then(|| {
            let s = s.clone();
            let cb = cb.clone();
            let clip_id = clip_id.clone();
            shared_inspector_checkbox(
                "clip-stretch-preserve-pitch",
                preserve_mode,
                false,
                if preserve_mode {
                    "Preserve Pitch"
                } else {
                    "Pitch follows speed"
                },
                move |checked, w, cx| {
                    cb(&(clip_id.clone(), with_preserve_pitch(&s, checked)), w, cx);
                },
            )
        }))
        .children(
            (!preserve_mode && stretch_enabled).then(|| {
                inspector_hint_text("Turn on Preserve Pitch to shift pitch independently.")
            }),
        )
        .child(stretch_field_block(
            "Semi",
            clip_stretch_stepper(
                "clip-pitch-semi",
                &clip_id,
                semi as f64,
                format!("{:+.2}", semi),
                -48.0,
                48.0,
                1.0,
                !stretch_enabled || !preserve_mode,
                callbacks,
                {
                    let s = s.clone();
                    move |semi| {
                        let (_, fine) = s.pitch_semi_and_cents();
                        let mut next = s.clone();
                        next.set_pitch_semi_and_cents(semi as f32, fine);
                        next
                    }
                },
            ),
        ))
        .child(stretch_field_block(
            "Fine",
            clip_stretch_stepper(
                "clip-pitch-fine",
                &clip_id,
                fine as f64,
                format!("{fine:+.0} ct"),
                -99.0,
                99.0,
                50.0,
                !stretch_enabled || !preserve_mode,
                callbacks,
                {
                    let s = s.clone();
                    move |fine| {
                        let (semi, _) = s.pitch_semi_and_cents();
                        let mut next = s.clone();
                        next.set_pitch_semi_and_cents(semi, fine as f32);
                        next
                    }
                },
            ),
        ))
        .child(stretch_field_block(
            "",
            inspector_mini_button(
                "clip-reset-pitch",
                "Reset Pitch",
                stretch_enabled && preserve_mode,
                {
                    let s = s.clone();
                    let cb = cb.clone();
                    let clip_id = clip_id.clone();
                    move |_, w, cx| {
                        let mut next = s.clone();
                        next.reset_pitch();
                        cb(&(clip_id.clone(), next), w, cx);
                    }
                },
            ),
        ))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(px(8.0))
                .pt(px(5.0))
                .border_t(px(1.0))
                .border_color(Colors::divider())
                .child(
                    div()
                        .text_size(px(typography::DENSE_LABEL))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(Colors::text_muted())
                        .child("Result"),
                )
                .child(inspector_mini_button(
                    "clip-reset-stretch",
                    "Reset Stretch",
                    true,
                    {
                        let cb = cb.clone();
                        let clip_id = clip_id.clone();
                        move |_, w, cx| {
                            cb(&(clip_id.clone(), reset.clone()), w, cx);
                        }
                    },
                )),
        )
        .child(stretch_metric_row("Ratio", format!("{ratio:.3}x")))
        .children(
            s.bpm_source
                .map(|b| stretch_metric_row("Source", format!("{b:.2} BPM"))),
        )
        .child(stretch_metric_row("Target", target_display))
        .child(stretch_metric_row("Pitch", pitch_summary))
        .child(stretch_metric_row("Length", length_summary))
        .child(stretch_metric_row("Backend", backend))
        .children((stretch_enabled && !preserve_mode).then(|| {
            inspector_hint_text(format!("Pitch follows speed. Extra pitch: {semi:+.2} st"))
        }))
        .children(
            (stretch_enabled && preserve_mode).then(|| {
                inspector_hint_text(format!("Pitch preserved. Pitch shift: {:+.2} st", semi))
            }),
        )
        .children(
            (!stretch_enabled)
                .then(|| inspector_hint_text("Stretch is off; playback uses default params.")),
        )
        .children((!fit_selection_enabled).then(|| {
            inspector_hint_text("Fit Selection enables when an arrangement time range is selected.")
        }))
        .children(warp_mode.then(|| warp_section_body(&clip_id, s, callbacks)))
}

/// PITCH section body — semitone / fine-cents / formant.
fn pitch_section_body(
    clip_id: &str,
    s: &AudioClipStretchState,
    cb: &ClipStretchCb,
) -> impl IntoElement {
    let semi = s.pitch_shift_semitones.trunc() as f64;
    let fine = ((s.pitch_shift_semitones as f64 - semi) * 100.0).round();
    let pitch_note = (s.preserve_pitch && mode_supports_preserve_pitch(s.mode))
        .then_some("Independent pitch shift pending in preserve mode");

    div()
        .flex()
        .flex_col()
        .gap(px(3.0))
        .children(pitch_note.map(inspector_hint_text))
        .child(shared_inspector_row(
            "Semi",
            false,
            inspector_numeric_stepper(
                "clip-pitch-semi",
                semi,
                format!("{:+.2} st", s.pitch_shift_semitones),
                -48.0,
                48.0,
                1.0,
                false,
                {
                    let s = s.clone();
                    let cb = cb.clone();
                    let clip_id = clip_id.to_string();
                    move |semi, w, cx| {
                        let fine = (s.pitch_shift_semitones as f64
                            - s.pitch_shift_semitones.trunc() as f64)
                            as f32;
                        let mut next = s.clone();
                        next.pitch_shift_semitones = (semi as f32 + fine).clamp(-48.0, 48.0);
                        next.dirty = true;
                        cb(&(clip_id.clone(), next), w, cx);
                    }
                },
            ),
        ))
        .child(shared_inspector_row(
            "Fine",
            false,
            inspector_numeric_stepper(
                "clip-pitch-fine",
                fine,
                format!("{fine:+.0} ct"),
                -99.0,
                99.0,
                50.0,
                false,
                {
                    let s = s.clone();
                    let cb = cb.clone();
                    let clip_id = clip_id.to_string();
                    move |fine, w, cx| {
                        let semi = s.pitch_shift_semitones.trunc();
                        let mut next = s.clone();
                        next.pitch_shift_semitones =
                            (semi + (fine as f32 / 100.0)).clamp(-48.0, 48.0);
                        next.dirty = true;
                        cb(&(clip_id.clone(), next), w, cx);
                    }
                },
            ),
        ))
        .child(shared_inspector_row(
            "Formant",
            false,
            shared_inspector_checkbox(
                "clip-pitch-formant",
                s.formant_preserve,
                false,
                if s.formant_preserve { "On" } else { "Off" },
                {
                    let s = s.clone();
                    let cb = cb.clone();
                    let clip_id = clip_id.to_string();
                    move |checked, w, cx| {
                        let mut next = s.clone();
                        next.formant_preserve = checked;
                        next.dirty = true;
                        cb(&(clip_id.clone(), next), w, cx);
                    }
                },
            ),
        ))
}

/// TRANSIENT section body — preserve + sensitivity.
fn transient_section_body(
    clip_id: &str,
    s: &AudioClipStretchState,
    cb: &ClipStretchCb,
) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(3.0))
        .child(shared_inspector_row(
            "Preserve",
            false,
            shared_inspector_checkbox(
                "clip-trans-preserve",
                s.transient_preserve,
                false,
                if s.transient_preserve { "On" } else { "Off" },
                {
                    let s = s.clone();
                    let cb = cb.clone();
                    let clip_id = clip_id.to_string();
                    move |checked, w, cx| {
                        let mut next = s.clone();
                        next.transient_preserve = checked;
                        next.dirty = true;
                        cb(&(clip_id.clone(), next), w, cx);
                    }
                },
            ),
        ))
        .child(shared_inspector_row(
            "Sensitivity",
            false,
            inspector_numeric_stepper(
                "clip-trans-sens",
                s.transient_sensitivity as f64,
                format!("{:.2}", s.transient_sensitivity),
                0.0,
                1.0,
                0.05,
                false,
                {
                    let s = s.clone();
                    let cb = cb.clone();
                    let clip_id = clip_id.to_string();
                    move |value, w, cx| {
                        let mut next = s.clone();
                        next.transient_sensitivity = value as f32;
                        next.dirty = true;
                        cb(&(clip_id.clone(), next), w, cx);
                    }
                },
            ),
        ))
}

/// WARP section body — marker count + add/clear and a compact readback list.
fn warp_section_body(
    clip_id: &str,
    s: &AudioClipStretchState,
    callbacks: &InspectorCallbacks,
) -> impl IntoElement {
    let add = callbacks.on_clip_warp_add_at_playhead.clone();
    let clear = callbacks.on_clip_warp_clear.clone();
    let add_id = clip_id.to_string();
    let clear_id = clip_id.to_string();
    let has_markers = !s.warp_markers.is_empty();

    div()
        .flex()
        .flex_col()
        .gap(px(3.0))
        .child(shared_inspector_row(
            "Markers",
            false,
            inspector_value(s.warp_markers.len().to_string()),
        ))
        .child(shared_inspector_row(
            "",
            false,
            div()
                .flex()
                .flex_row()
                .gap(px(4.0))
                .child(inspector_mini_button(
                    "clip-warp-add",
                    "Add at Playhead",
                    true,
                    move |_, w, cx| add(&add_id, w, cx),
                ))
                .child(inspector_mini_button(
                    "clip-warp-clear",
                    "Clear",
                    has_markers,
                    move |_, w, cx| clear(&clear_id, w, cx),
                )),
        ))
        .children(s.warp_markers.iter().take(32).map(|marker| {
            shared_inspector_row(
                format!("#{:02}", marker.id),
                false,
                inspector_value(format!(
                    "beat {:.2}  ·  src {}{}",
                    marker.timeline_beat,
                    marker.source_sample,
                    if marker.locked { "  locked" } else { "" }
                )),
            )
        }))
        .child(inspector_hint_text(
            "Add at the playhead. Markers are preserved with the clip.",
        ))
}

fn file_name_from_path(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path)
        .to_string()
}

fn clip_inspector(
    clip: SelectedClipSummary<'_>,
    clip_name_input: &TextInputState,
    clip_name_focused: bool,
    clip_name_callbacks: TextInputCallbacks,
    tempo: StretchTempoUiSnapshot,
    callbacks: &InspectorCallbacks,
    i18n: I18n,
) -> impl IntoElement {
    let clip_id = clip.clip_id.to_string();
    let open_bottom = callbacks.on_open_clip_bottom_editor.clone();
    let open_external = callbacks.on_open_clip_external_midi_editor.clone();

    if clip.kind == "Audio" {
        let source_duration = clip
            .source_duration_seconds
            .map(|seconds| format!("{seconds:.2} s"))
            .unwrap_or_else(|| "Pending".to_string());
        let file_name = clip
            .source_path
            .map(file_name_from_path)
            .unwrap_or_else(|| "Missing source".to_string());
        let path = clip.source_path.unwrap_or("-");
        let gain_db = linear_gain_to_db(clip.gain);
        let muted_id = clip.clip_id.to_string();
        let mute_cb = callbacks.on_set_clip_muted.clone();
        let s = clip.stretch;
        let stretch_cb = callbacks.on_set_clip_stretch.clone();
        // Precomputed next-states for the inline AUDIO-section stretch controls.
        let reverse_next = {
            let mut n = s.clone();
            n.reverse = !s.reverse;
            n.dirty = true;
            n
        };
        let mut body = scroll_body()
            .gap(px(10.0))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .w(px(4.0))
                                    .h(px(30.0))
                                    .rounded(px(crate::theme::radius::CONTROL))
                                    .bg(Colors::accent_primary()),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(13.0))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(Colors::text_primary())
                                    .child(clip.name.to_string()),
                            )
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .px(px(7.0))
                                    .py(px(2.0))
                                    .rounded(px(crate::theme::radius::CONTROL))
                                    .bg(Colors::with_alpha(Colors::accent_primary(), 0.16))
                                    .text_size(px(typography::DENSE_CAPTION))
                                    .font_weight(gpui::FontWeight::BOLD)
                                    .text_color(Colors::accent_primary())
                                    .child("Audio Clip"),
                            ),
                    )
                    .child(
                        div()
                            .pl(px(12.0))
                            .min_w_0()
                            .truncate()
                            .text_size(px(10.5))
                            .text_color(Colors::text_muted())
                            .child(format!(
                                "{} • {} source • Gain {:.1} dB",
                                clip.track_name, source_duration, gain_db
                            )),
                    ),
            )
            .child(inspector_section(
                i18n.tr("inspector.section.clip"),
                div()
                    .flex()
                    .flex_col()
                    .gap(px(3.0))
                    .child(compact_property_row(
                        "Name",
                        text_field_with_callbacks(
                            clip_name_input,
                            clip_name_focused,
                            clip_name_callbacks.clone(),
                        ),
                    )),
            ))
            .child(inspector_section(
                "TIMING",
                div()
                    .flex()
                    .flex_col()
                    .gap(px(3.0))
                    .child(compact_property_row(
                        i18n.tr("inspector.clip.start"),
                        beat_stepper(
                            "clip-start",
                            clip.clip_id,
                            clip.start_beat,
                            callbacks.on_preview_clip_start.clone(),
                            callbacks,
                            0.0,
                        ),
                    ))
                    .child(compact_property_row(
                        i18n.tr("inspector.clip.length"),
                        beat_stepper(
                            "clip-length",
                            clip.clip_id,
                            clip.duration_beats,
                            callbacks.on_preview_clip_length.clone(),
                            callbacks,
                            0.25,
                        ),
                    ))
                    .child(compact_property_row(
                        "End",
                        readonly_value(format!("{:.2} bt", clip.start_beat + clip.duration_beats)),
                    )),
            ))
            .child(inspector_section(
                "AUDIO",
                div()
                    .flex()
                    .flex_col()
                    .gap(px(3.0))
                    .child(compact_property_row(
                        "Muted",
                        fb_checkbox("clip-muted", "Muted", clip.muted, true, move |_, w, cx| {
                            mute_cb(&(muted_id.clone(), !clip.muted), w, cx)
                        }),
                    ))
                    .child(compact_property_row(
                        "Gain",
                        gain_stepper(
                            clip.clip_id,
                            clip.gain,
                            callbacks.on_preview_clip_gain.clone(),
                            callbacks,
                        ),
                    ))
                    .child(compact_property_row(
                        "Reverse",
                        shared_inspector_checkbox(
                            "clip-reverse",
                            s.reverse,
                            false,
                            if s.reverse { "On" } else { "Off" },
                            {
                                let clip_id = clip.clip_id.to_string();
                                let stretch_cb = stretch_cb.clone();
                                move |_, w, cx| {
                                    stretch_cb(&(clip_id.clone(), reverse_next.clone()), w, cx);
                                }
                            },
                        ),
                    ))
                    .child(compact_property_row(
                        "Fade In",
                        clip_stretch_stepper(
                            "clip-fade-in",
                            clip.clip_id,
                            s.fade_in_ms as f64,
                            format!("{:.0} ms", s.fade_in_ms),
                            0.0,
                            60_000.0,
                            5.0,
                            false,
                            callbacks,
                            {
                                let s = s.clone();
                                move |value| {
                                    let mut next = s.clone();
                                    next.fade_in_ms = value as f32;
                                    next.dirty = true;
                                    next
                                }
                            },
                        ),
                    ))
                    .child(compact_property_row(
                        "Fade Out",
                        clip_stretch_stepper(
                            "clip-fade-out",
                            clip.clip_id,
                            s.fade_out_ms as f64,
                            format!("{:.0} ms", s.fade_out_ms),
                            0.0,
                            60_000.0,
                            5.0,
                            false,
                            callbacks,
                            {
                                let s = s.clone();
                                move |value| {
                                    let mut next = s.clone();
                                    next.fade_out_ms = value as f32;
                                    next.dirty = true;
                                    next
                                }
                            },
                        ),
                    )),
            ))
            .child(shared_inspector_section(
                "Audio Stretch",
                None::<String>,
                stretch_section_body(&clip, s, clip.project_bpm, &tempo, &stretch_cb, callbacks),
            ))
            .child(shared_inspector_section(
                "Source",
                None::<String>,
                div()
                    .flex()
                    .flex_col()
                    .gap(px(3.0))
                    .child(shared_inspector_row(
                        "File",
                        false,
                        truncate_value(file_name),
                    ))
                    .child(shared_inspector_row(
                        "Duration",
                        false,
                        truncate_value(source_duration),
                    ))
                    .child(shared_inspector_row(
                        "Path",
                        false,
                        truncate_value(path.to_string()),
                    ))
                    .child(shared_inspector_row(
                        "",
                        false,
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(4.0))
                            // TODO(source-actions): reveal/replace need shell + relink callbacks.
                            .child(compact_action_button(
                                "clip-reveal",
                                "Reveal",
                                false,
                                |_, _, _| {},
                            ))
                            .child(compact_action_button(
                                "clip-replace",
                                "Replace",
                                false,
                                |_, _, _| {},
                            )),
                    )),
            ));

        if std::env::var_os("FUTUREBOARD_INSPECTOR_DEBUG").is_some() {
            body = body.child(inspector_section(
                "DEBUG",
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(kv_row("Track ID", clip.track_id.to_string())),
            ));
        }

        return body;
    }

    let clip_type_label = match clip.kind {
        "Audio" => i18n.tr("inspector.clip.type.audio"),
        "MIDI" => i18n.tr("inspector.clip.type.midi"),
        other => other.to_string(),
    };
    let mut body = scroll_body()
        .child(inspector_header(
            Colors::accent_primary(),
            clip.name.to_string(),
            "Clip",
        ))
        .child(
            inspector_kit::ins_section_container()
                .child(inspector_kit::ins_section_header(
                    assets::ICON_LIST_MUSIC_PATH,
                    i18n.tr("inspector.section.clip"),
                ))
                .child(kv_row(i18n.tr("inspector.clip.type"), clip_type_label))
                .child(kv_row(
                    i18n.tr("inspector.clip.track"),
                    clip.track_name.to_string(),
                ))
                .child(kv_row("Track ID", clip.track_id.to_string()))
                .child(fb_form_row(
                    "Name",
                    text_field_with_callbacks(
                        clip_name_input,
                        clip_name_focused,
                        clip_name_callbacks,
                    ),
                ))
                .child(fb_form_row(
                    i18n.tr("inspector.clip.start"),
                    beat_stepper(
                        "clip-start",
                        clip.clip_id,
                        clip.start_beat,
                        callbacks.on_preview_clip_start.clone(),
                        callbacks,
                        0.0,
                    ),
                ))
                .child(fb_form_row(
                    i18n.tr("inspector.clip.length"),
                    beat_stepper(
                        "clip-length",
                        clip.clip_id,
                        clip.duration_beats,
                        callbacks.on_preview_clip_length.clone(),
                        callbacks,
                        0.25,
                    ),
                ))
                .child(kv_row(
                    "End",
                    format!("{:.2} bt", clip.start_beat + clip.duration_beats),
                ))
                .child(kv_row(
                    "Muted",
                    if clip.muted { "Yes" } else { "No" }.to_string(),
                )),
        );

    if clip.kind == "MIDI" {
        let bottom_id = clip_id.clone();
        body = body.child(
            inspector_kit::ins_section_container()
                .child(inspector_kit::ins_section_header(
                    assets::ICON_MUSIC_PATH,
                    "MIDI CLIP",
                ))
                .child(kv_row(
                    "Notes",
                    clip.note_count.unwrap_or_default().to_string(),
                ))
                .child(kv_row(
                    "Local Length",
                    format!("{:.2} bt", clip.duration_beats),
                ))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap(px(4.0))
                        .child(compact_action_button(
                            "clip-open-bottom-midi",
                            "Bottom Editor",
                            true,
                            move |_, w, cx| open_bottom(&bottom_id, w, cx),
                        ))
                        .child(compact_action_button(
                            "clip-open-floating-midi",
                            "MIDI Window",
                            true,
                            move |_, w, cx| open_external(&clip_id, w, cx),
                        )),
                ),
        );
    } else {
        body = body.child(
            inspector_kit::ins_section_container()
                .child(inspector_kit::ins_section_header(
                    assets::ICON_AUDIO_LINES_PATH,
                    "AUDIO CLIP",
                ))
                .child(kv_row(
                    "File",
                    clip.source_path
                        .map(file_name_from_path)
                        .unwrap_or_else(|| "Missing source".to_string()),
                ))
                .child(kv_row(
                    "Path",
                    clip.source_path
                        .map(|path| path.to_string())
                        .unwrap_or_else(|| "-".to_string()),
                ))
                .child(kv_row(
                    "Source Duration",
                    clip.source_duration_seconds
                        .map(|seconds| format!("{seconds:.2} s"))
                        .unwrap_or_else(|| "Pending".to_string()),
                ))
                .child(kv_row("Gain", format!("{:.2}", clip.gain))),
        );
    }

    body
}

/// Helper retained for later phases: classify a clip's type label from its
/// stored `ClipType`. Kept here so the clip inspector and summary share one
/// source of truth.
pub fn clip_type_label(clip_type: &ClipType) -> &'static str {
    match clip_type {
        ClipType::Audio { .. } => "Audio",
        ClipType::Midi { .. } => "MIDI",
        ClipType::Video { .. } => "Video",
    }
}

#[cfg(test)]
mod input_routing_tests {
    use super::*;
    use crate::components::timeline::timeline_state::{
        CreateTrackOptions, InputMonitorMode, TimelineState,
    };

    fn audio_track(format: TrackAudioFormat) -> TrackState {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let id = state.create_track(CreateTrackOptions {
            track_type: TrackType::Audio,
            name: "Audio".to_string(),
            color: gpui::Rgba {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 1.0,
            },
            volume: 1.0,
            pan: 0.0,
            armed: false,
            input_monitor: InputMonitorMode::Off,
        });
        let track = state
            .tracks
            .iter_mut()
            .find(|track| track.id == id)
            .expect("audio track");
        track.routing.audio_format = format;
        track.clone()
    }

    /// One mono input bus on `dev-1`, built the way the Audio Connections
    /// window builds one — no migration bridge involved.
    fn mono_input_registry(
        ports: &crate::audio_connections::AvailablePorts,
    ) -> (
        AudioConnectionRegistry,
        crate::audio_connections::AudioConnectionId,
    ) {
        use crate::audio_connections::{AudioConnection, AudioConnectionDirection, ChannelLayout};

        let mut registry = AudioConnectionRegistry::new();
        let id = registry.add(
            AudioConnection::new(
                "Mono Input 1",
                AudioConnectionDirection::Input,
                ChannelLayout::Mono,
            )
            .bind_consecutive("dev-1", 0, |i| format!("Input {}", i + 1)),
        );
        registry.revalidate(ports);
        (registry, id)
    }

    /// The Inspector label comes from the registry, so renaming a connection
    /// changes the display without touching the track's assignment.
    #[test]
    fn inspector_label_resolves_the_connection_name_and_survives_rename() {
        let ports =
            crate::audio_connections::AvailablePorts::for_device("dev-1", "Interface", 2, 2);
        let (mut registry, id) = mono_input_registry(&ports);

        let mut track = audio_track(TrackAudioFormat::Mono);
        track.routing.audio_input_connection_id = Some(id.clone());
        // A logical bus name; the endpoint lives in the Audio Device column.
        assert_eq!(audio_input_combo_label(&track, &registry), "Mono Input 1");

        assert!(registry.rename(&id, "Microphone"));
        assert_eq!(audio_input_combo_label(&track, &registry), "Microphone");
        assert_eq!(
            track.routing.audio_input_connection_id.as_ref(),
            Some(&id),
            "the track keeps the same id across a rename"
        );
    }

    /// A disconnected device is shown as unavailable, never cleared.
    #[test]
    fn inspector_label_marks_an_unavailable_connection_without_clearing_it() {
        let ports =
            crate::audio_connections::AvailablePorts::for_device("dev-1", "Interface", 2, 2);
        let (mut registry, id) = mono_input_registry(&ports);
        registry.revalidate(&crate::audio_connections::AvailablePorts::default());

        let mut track = audio_track(TrackAudioFormat::Mono);
        track.routing.audio_input_connection_id = Some(id.clone());
        assert!(audio_input_combo_label(&track, &registry).contains("Unavailable"));
        assert_eq!(track.routing.audio_input_connection_id.as_ref(), Some(&id));
    }

    #[test]
    fn inspector_label_reports_a_reference_that_no_longer_resolves() {
        use crate::audio_connections::{AudioConnectionId, AudioConnectionRegistry};

        let mut track = audio_track(TrackAudioFormat::Mono);
        track.routing.audio_input_connection_id =
            Some(AudioConnectionId::from_stored("ac-removed"));
        assert_eq!(
            audio_input_combo_label(&track, &AudioConnectionRegistry::new()),
            "Missing connection"
        );
    }

    #[test]
    fn no_input_is_labelled_no_input() {
        use crate::audio_connections::AudioConnectionRegistry;

        let track = audio_track(TrackAudioFormat::Mono);
        assert!(track.routing.audio_input_connection_id.is_none());
        assert_eq!(
            audio_input_combo_label(&track, &AudioConnectionRegistry::new()),
            "No Input"
        );
    }
}

/// The Architecture row must describe the engine Studio builds, not the
/// sections the loader read on the way there. These run without a GPUI window
/// because `solfege_runtime_architecture` is a pure string function.
#[cfg(test)]
mod solfege_inspector_tests {
    use super::*;

    fn info(architecture: &str, parameter_count: u64) -> SolfegeModelInfo {
        SolfegeModelInfo {
            name: "Solo Violin".to_string(),
            model_type: "SFM v1 / neural voicebank".to_string(),
            architecture: architecture.to_string(),
            sample_rate: 48_000,
            source_type: "recorded".to_string(),
            validated: true,
            parameter_count,
            file_size_bytes: 146_000_000,
            voicebank_profiles: 4,
            voicebank_entries: 512,
            voicebank_audio_bytes: 145_000_000,
            voicebank_source_files: 512,
            performer: None,
            accent: None,
        }
    }

    /// A model that carries a Performer says so, and one that does not says
    /// what it does instead — "None" alone would read as a missing feature
    /// rather than as "this track plays the notes you wrote".
    #[test]
    fn the_inspector_distinguishes_a_performer_from_its_absence() {
        let without = info("Neural voicebank + BowedString", 0);
        assert!(without.performer.is_none());

        let mut with = info("Neural voicebank + BowedString", 0);
        with.performer = Some(crate::solfege::SolfegePerformerInfo {
            input_size: 20,
            output_size: 9,
            bidirectional: true,
            weight_bytes: 11_604,
            feature_schema_version: 2,
        });
        let performer = with.performer.as_ref().expect("a performer");
        assert_eq!(performer.mode(), "Studio (reads the whole phrase)");
        assert!(performer.reads_accent());

        let live = crate::solfege::SolfegePerformerInfo {
            input_size: 16,
            output_size: 9,
            bidirectional: false,
            weight_bytes: 10_660,
            feature_schema_version: 1,
        };
        assert_eq!(live.mode(), "Live (causal)");
    }

    /// A package built before accent existed must still load, still play, and
    /// say plainly that the Accent lane cannot reach it — a sixteen-input
    /// Performer has nowhere to put the four accent columns, and quietly
    /// pretending otherwise is how a control becomes decorative.
    #[test]
    fn a_pre_accent_performer_is_reported_as_not_reading_accent() {
        let old = crate::solfege::SolfegePerformerInfo {
            input_size: 16,
            output_size: 9,
            bidirectional: true,
            weight_bytes: 10_660,
            feature_schema_version: 1,
        };
        assert!(!old.reads_accent());
    }

    /// No embedded Accent Analyzer is the designed state, not a defect: the
    /// analysis runs on the built-in fitted rule.
    #[test]
    fn an_embedded_accent_analyzer_is_optional_and_reports_its_usability() {
        let mut model = info("Neural voicebank + BowedString", 0);
        assert!(model.accent.is_none());

        model.accent = Some(crate::solfege::SolfegeAccentInfo {
            input_size: 33,
            output_size: 5,
            weight_bytes: 5_061,
            usable: true,
        });
        assert!(model.accent.as_ref().unwrap().summary().contains("33 in"));

        model.accent = Some(crate::solfege::SolfegeAccentInfo {
            input_size: 0,
            output_size: 0,
            weight_bytes: 4_096,
            usable: false,
        });
        assert!(model
            .accent
            .as_ref()
            .unwrap()
            .summary()
            .contains("uses the rule"));
    }

    #[test]
    fn packaged_sfm_reports_voicebank_playback_not_the_file_contents() {
        let architecture = solfege_runtime_architecture(
            Some("C:\\Models\\violin.sfm"),
            &info(
                "Neural voicebank + BowedString + embedded FBMX residual",
                4_200_000,
            ),
        );
        assert!(
            architecture.starts_with("Neural voicebank playback"),
            "{architecture}"
        );
        // The two sections the DAW's VoicebankOnly load never instantiates must
        // not read as active architecture.
        assert!(architecture.contains("not instantiated"), "{architecture}");
    }

    #[test]
    fn sfm_without_a_packaged_residual_does_not_claim_one() {
        let architecture = solfege_runtime_architecture(
            Some("C:\\Models\\violin.sfm"),
            &info("Neural voicebank + BowedString", 0),
        );
        assert!(!architecture.contains("FBMX"), "{architecture}");
        assert!(architecture.contains("not instantiated"), "{architecture}");
    }

    #[test]
    fn sfm_without_a_voicebank_reports_the_fallback_engine() {
        // `prepare_sfm_runtime` refuses this file, so the track runs on the
        // default BowedString engine `RuntimeSolfegeEngine::new` already built.
        let architecture = solfege_runtime_architecture(
            Some("C:\\Models\\violin.sfm"),
            &info("BowedString physical", 0),
        );
        assert!(architecture.contains("fallback"), "{architecture}");
        assert!(
            !architecture.contains("voicebank playback"),
            "{architecture}"
        );
    }

    #[test]
    fn standalone_fbmx_keeps_the_model_architecture_over_the_fallback() {
        let architecture =
            solfege_runtime_architecture(Some("C:\\Models\\violin.fbmx"), &info("LSTM", 4_200_000));
        assert_eq!(architecture, "BowedString physical + LSTM residual");
    }
}

#[cfg(test)]
mod stretch_inspector_tests {
    use super::*;

    #[test]
    fn preserve_pitch_availability_matches_mode() {
        assert!(!mode_supports_preserve_pitch(StretchMode::Resample));
        assert!(mode_supports_preserve_pitch(StretchMode::Manual));
        assert!(mode_supports_preserve_pitch(StretchMode::TempoSync));
        assert!(mode_supports_preserve_pitch(StretchMode::Warp));
    }

    #[test]
    fn switching_to_resample_clears_preserve_pitch() {
        let state = AudioClipStretchState {
            preserve_pitch: true,
            ..AudioClipStretchState::default()
        };
        let next = with_mode(&state, StretchMode::Resample);
        assert!(!next.preserve_pitch);
    }

    #[test]
    fn ui_modes_map_to_real_stretch_params() {
        let state = AudioClipStretchState::default();
        let repitch = with_ui_mode(&state, StretchUiMode::Resample);
        assert_eq!(repitch.mode, StretchMode::Resample);
        assert_eq!(repitch.algorithm, StretchAlgorithm::ResampleOnly);
        assert!(!repitch.preserve_pitch);

        let preserve = with_ui_mode(&state, StretchUiMode::Manual);
        let preserve = with_preserve_pitch(&preserve, true);
        assert_eq!(preserve.mode, StretchMode::Manual);
        assert_eq!(preserve.algorithm, StretchAlgorithm::PhaseVocoder);
        assert!(preserve.preserve_pitch);

        let warp = with_ui_mode(&state, StretchUiMode::Warp);
        assert_eq!(warp.mode, StretchMode::Warp);
        assert!(warp.preserve_pitch);
    }
}

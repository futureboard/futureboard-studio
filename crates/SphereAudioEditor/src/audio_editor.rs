//! Native audio clip editor frontend.
//!
//! This crate owns editor vocabulary, coordinate math, and rendering. The
//! host supplies the authoritative project view model and translates the
//! semantic events below into Futureboard commands.

use std::sync::Arc;

use gpui::{
    App, AppContext, Context, DragMoveEvent, Empty, InteractiveElement, IntoElement, ParentElement,
    Render, ScrollWheelEvent, StatefulInteractiveElement, Styled, StyledImage, Window, deferred,
    div, img, prelude::FluentBuilder, px,
};

use crate::audio_editor_state::AudioEditorState;
use crate::audio_ruler::{audio_ruler, ruler_height};
use crate::editing::{
    AudioChannelMode, AudioEditorSnap, AudioEditorTool, AudioFadeEdge, ClipChannelMode,
    ClipEnvelope, DisplaySmoothing, SpectralSelection, WarpMarkerView,
};
use crate::spectrogram::{
    AmplitudeScale, AudioEditorViewMode, FrequencyScale, SpectrogramViewModel, frequency_position,
};
use crate::tools::AudioToolKind;
use crate::waveform_view::{WaveformViewModel, waveform_view};

/// Theme tokens passed from the host shell (Futureboard dark DAW palette).
#[derive(Debug, Clone, Copy)]
pub struct AudioEditorTheme {
    pub surface_base: gpui::Rgba,
    pub surface_panel: gpui::Rgba,
    pub text_primary: gpui::Rgba,
    pub text_secondary: gpui::Rgba,
    pub text_muted: gpui::Rgba,
    pub border_subtle: gpui::Rgba,
    pub accent: gpui::Rgba,
    pub playhead: gpui::Rgba,
    pub error: gpui::Rgba,
    pub selection: gpui::Rgba,
}

/// Semantic actions emitted by the panel. No event contains a screen-space
/// project command: the host resolves pixels into the authoritative timeline
/// domain before touching project state.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AudioEditorEvent {
    ToggleDropdown(AudioEditorDropdown),
    SetTool(AudioEditorTool),
    SetSnap(AudioEditorSnap),
    SetChannelMode(AudioChannelMode),
    SetClipChannel(ClipChannelMode),
    SetViewMode(AudioEditorViewMode),
    SetAmplitudeScale(AmplitudeScale),
    SetFrequencyScale(FrequencyScale),
    SetDisplaySmoothing(DisplaySmoothing),
    AdjustVerticalZoom { factor: f32 },
    ResetVerticalZoom,
    ZoomBy { factor: f32, anchor_x: Option<f32> },
    FitClip,
    Seek { x: f32 },
    PointerDown { x: f32, y: f32, shift: bool },
    PointerMove { x: f32, y: f32, shift: bool },
    PointerUp { x: f32, y: f32, shift: bool },
    AdjustGain { delta_db: f32 },
    AdjustPitch { delta_semitones: f32 },
    AdjustFinePitch { delta_cents: f32 },
    ToggleFollowTempo,
    ToggleReverse,
    SetFollowTempo(bool),
    SetReverse(bool),
    SetDenoiseAmount(f32),
    OpenTool(AudioToolKind),
}

/// One of the compact editor controls that opens a menu. Keeping this in the
/// semantic editor layer means the host owns the open/close state while the
/// visual frontend stays independent of project state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioEditorDropdown {
    Tool,
    View,
    Amplitude,
    Frequency,
    Snap,
    Smooth,
    Channel,
    FollowTempo,
    Reverse,
    Denoise,
    Tools,
}

pub type AudioEditorEventHandler = Arc<dyn Fn(AudioEditorEvent, &mut Window, &mut App) + 'static>;

#[derive(Clone, Default)]
pub struct AudioEditorCallbacks {
    pub on_event: Option<AudioEditorEventHandler>,
}

fn emit(
    callbacks: &AudioEditorCallbacks,
    event: AudioEditorEvent,
    window: &mut Window,
    cx: &mut App,
) {
    if let Some(callback) = callbacks.on_event.as_ref() {
        callback(event, window, cx);
    }
}

/// Read-only view model built from project/timeline state each frame.
#[derive(Debug, Clone)]
pub struct AudioEditorViewModel {
    pub clip_id: String,
    pub clip_name: String,
    pub file_label: Option<String>,
    /// Immutable source path used only by the host's background analysis job.
    pub source_path: Option<String>,
    /// Inclusive source-frame window this clip currently plays.
    pub source_start_frame: i64,
    /// Exclusive source-frame window end. Equal to `source_start_frame` when unknown.
    pub source_end_frame: i64,
    pub start_beat: f32,
    pub duration_beats: f32,
    pub offset_beats: f32,
    pub beats_per_bar: f32,
    pub bpm: f32,
    pub track_color: gpui::Rgba,
    pub waveform: WaveformViewModel,
    pub spectrogram: SpectrogramViewModel,
    pub view_mode: AudioEditorViewMode,
    pub amplitude_scale: AmplitudeScale,
    pub frequency_scale: FrequencyScale,
    /// Playhead position relative to clip start, if visible.
    pub playhead_in_clip: Option<f32>,
    /// Selection overlay relative to clip start (beats).
    pub selection_range: Option<(f32, f32)>,
    /// Optional source-domain time-frequency selection.
    pub spectral_selection: Option<SpectralSelection>,
    /// Non-destructive clip gain envelope, relative to clip gain.
    pub gain_envelope: ClipEnvelope,
    pub gain_db: f32,
    pub pitch_semitones: f32,
    pub fine_cents: f32,
    pub original_bpm: Option<f64>,
    pub follow_tempo: bool,
    pub stretch_mode: &'static str,
    pub stretch_percent: f64,
    pub reverse: bool,
    pub denoise_amount: f32,
    pub channel_mode: AudioChannelMode,
    pub fade_in_beats: f32,
    pub fade_out_beats: f32,
    pub warp_markers: Vec<WarpMarkerView>,
    pub clip_channel: ClipChannelMode,
    pub display_smoothing: DisplaySmoothing,
    pub sample_rate: u32,
    pub selection_start_label: String,
    pub selection_end_label: String,
    pub selection_duration_label: String,
    pub peak_label: String,
    pub source_summary: String,
    pub theme: AudioEditorTheme,
}

const TOOLBAR_H: f32 = 32.0;
const STATUS_H: f32 = 24.0;
const INSPECTOR_W: f32 = 174.0;
pub const AUDIO_EDITOR_INSPECTOR_WIDTH: f32 = INSPECTOR_W;
pub const AUDIO_EDITOR_TOOLS_WIDTH: f32 = 148.0;
const GRID_SUBDIV: f32 = 0.25;

#[derive(Clone, Copy, Debug, Default)]
pub struct AudioEditorCanvasDrag;

impl Render for AudioEditorCanvasDrag {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

fn with_alpha(color: gpui::Rgba, alpha: f32) -> gpui::Rgba {
    gpui::Rgba {
        a: color.a * alpha,
        ..color
    }
}

fn toolbar_button(
    id: &'static str,
    label: &'static str,
    selected: bool,
    theme: &AudioEditorTheme,
    callbacks: &AudioEditorCallbacks,
    event: AudioEditorEvent,
) -> impl IntoElement {
    let callbacks = callbacks.clone();
    let fill = if selected {
        with_alpha(theme.accent, 0.18)
    } else {
        gpui::Rgba {
            a: 0.0,
            ..gpui::Rgba::default()
        }
    };
    div()
        .id(id)
        .h(px(24.0))
        .px(px(7.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(4.0))
        .bg(fill)
        .text_size(px(11.0))
        .text_color(if selected {
            theme.accent
        } else {
            theme.text_secondary
        })
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(|s| s.bg(theme.surface_base))
        .on_click(move |_, window, cx| emit(&callbacks, event, window, cx))
        .child(label)
}

#[derive(Clone, Copy)]
struct DropdownOption {
    label: &'static str,
    selected: bool,
    heading: bool,
    event: Option<AudioEditorEvent>,
}

fn dropdown_option(label: &'static str, selected: bool, event: AudioEditorEvent) -> DropdownOption {
    DropdownOption {
        label,
        selected,
        heading: false,
        event: Some(event),
    }
}

fn dropdown_heading(label: &'static str) -> DropdownOption {
    DropdownOption {
        label,
        selected: false,
        heading: true,
        event: None,
    }
}

fn tool_options(active: AudioEditorTool) -> Vec<DropdownOption> {
    [
        AudioEditorTool::Pointer,
        AudioEditorTool::Range,
        AudioEditorTool::SpectralRange,
        AudioEditorTool::Split,
        AudioEditorTool::Trim,
        AudioEditorTool::Fade,
        AudioEditorTool::Marker,
        AudioEditorTool::Draw,
        AudioEditorTool::Warp,
        AudioEditorTool::Scrub,
        AudioEditorTool::Audition,
    ]
    .into_iter()
    .map(|tool| {
        dropdown_option(
            tool.label(),
            active == tool,
            AudioEditorEvent::SetTool(tool),
        )
    })
    .collect()
}

fn tools_menu_options() -> Vec<DropdownOption> {
    vec![
        dropdown_heading("Analysis"),
        dropdown_option(
            "Spectrum Analyzer",
            false,
            AudioEditorEvent::OpenTool(AudioToolKind::SpectrumAnalyzer),
        ),
        dropdown_option(
            "Loudness",
            false,
            AudioEditorEvent::OpenTool(AudioToolKind::Loudness),
        ),
        dropdown_option(
            "BPM",
            false,
            AudioEditorEvent::OpenTool(AudioToolKind::BpmAnalysis),
        ),
        dropdown_option(
            "Key",
            false,
            AudioEditorEvent::OpenTool(AudioToolKind::KeyAnalysis),
        ),
        dropdown_option(
            "Transients",
            false,
            AudioEditorEvent::OpenTool(AudioToolKind::TransientDetector),
        ),
        dropdown_heading("Process"),
        dropdown_option(
            "Normalize",
            false,
            AudioEditorEvent::OpenTool(AudioToolKind::Normalize),
        ),
        dropdown_option(
            "Time & Pitch",
            false,
            AudioEditorEvent::OpenTool(AudioToolKind::TimePitch),
        ),
        dropdown_option(
            "Resample",
            false,
            AudioEditorEvent::OpenTool(AudioToolKind::Resample),
        ),
        dropdown_option(
            "Channel Tools",
            false,
            AudioEditorEvent::OpenTool(AudioToolKind::ChannelTools),
        ),
        dropdown_option(
            "DC Offset",
            false,
            AudioEditorEvent::OpenTool(AudioToolKind::DcOffset),
        ),
        dropdown_option(
            "Audio Repair",
            false,
            AudioEditorEvent::OpenTool(AudioToolKind::AudioRepair),
        ),
        dropdown_option(
            "Spectral Processing",
            false,
            AudioEditorEvent::OpenTool(AudioToolKind::SpectralProcessor),
        ),
        dropdown_heading("View"),
        dropdown_option(
            "Spectrogram Settings",
            false,
            AudioEditorEvent::OpenTool(AudioToolKind::SpectrogramSettings),
        ),
        dropdown_option(
            "Phase Analyzer",
            false,
            AudioEditorEvent::OpenTool(AudioToolKind::PhaseAnalyzer),
        ),
    ]
}

#[allow(
    clippy::too_many_arguments,
    reason = "a flat GPUI element builder; each argument is one visual property"
)]
fn dropdown_button(
    id: &'static str,
    prefix: &'static str,
    selected_label: &'static str,
    open: bool,
    width: f32,
    theme: &AudioEditorTheme,
    callbacks: &AudioEditorCallbacks,
    dropdown: AudioEditorDropdown,
    options: Vec<DropdownOption>,
) -> impl IntoElement {
    let callbacks_for_toggle = callbacks.clone();
    let callbacks_for_options = callbacks.clone();
    let menu_width = width.max(64.0);
    let trigger = div()
        .id(id)
        .h(px(24.0))
        .w(px(width))
        .px(px(7.0))
        .flex()
        .items_center()
        .justify_between()
        .gap(px(5.0))
        .rounded(px(4.0))
        .bg(if open {
            with_alpha(theme.accent, 0.16)
        } else {
            with_alpha(theme.surface_base, 0.55)
        })
        .border(px(1.0))
        .border_color(if open {
            with_alpha(theme.accent, 0.72)
        } else {
            with_alpha(theme.border_subtle, 0.72)
        })
        .text_size(px(10.5))
        .text_color(if open {
            theme.accent
        } else {
            theme.text_secondary
        })
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(|s| s.bg(with_alpha(theme.surface_base, 0.9)))
        .on_mouse_down(gpui::MouseButton::Left, move |_, window, cx| {
            cx.stop_propagation();
            emit(
                &callbacks_for_toggle,
                AudioEditorEvent::ToggleDropdown(dropdown),
                window,
                cx,
            );
            window.prevent_default();
        })
        .child(
            div()
                .min_w(px(0.0))
                .flex()
                .items_center()
                .gap(px(4.0))
                .truncate()
                .child(format!("{prefix}:")),
        )
        .child(
            div()
                .min_w(px(0.0))
                .flex_1()
                .truncate()
                .text_color(theme.text_primary)
                .child(selected_label),
        )
        .child(div().text_color(theme.text_muted).child("▾"));

    let menu = div()
        .absolute()
        .left_0()
        .top(px(27.0))
        .w(px(menu_width))
        .max_h(px(360.0))
        .overflow_hidden()
        .p(px(4.0))
        .rounded(px(6.0))
        .border(px(1.0))
        .border_color(theme.border_subtle)
        .bg(theme.surface_panel)
        .shadow(vec![gpui::BoxShadow {
            color: gpui::Rgba {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.45,
            }
            .into(),
            offset: gpui::point(px(0.0), px(8.0)),
            blur_radius: px(18.0),
            spread_radius: px(0.0),
            inset: false,
        }])
        .id((gpui::ElementId::from(id), "menu"))
        .occlude()
        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .children(options.into_iter().enumerate().map(move |(index, option)| {
            let callbacks = callbacks_for_options.clone();
            let heading = option.heading;
            let event = option.event;
            div()
                .id((id, index))
                .min_h(px(if heading { 20.0 } else { 24.0 }))
                .w_full()
                .px(px(7.0))
                .flex()
                .items_center()
                .justify_between()
                .rounded(px(4.0))
                .bg(if option.selected {
                    with_alpha(theme.accent, 0.18)
                } else {
                    gpui::transparent_black().into()
                })
                .text_size(px(if heading { 9.5 } else { 10.5 }))
                .font_weight(if heading {
                    gpui::FontWeight::SEMIBOLD
                } else {
                    gpui::FontWeight::NORMAL
                })
                .text_color(if heading {
                    theme.text_muted
                } else if option.selected {
                    theme.text_primary
                } else {
                    theme.text_secondary
                })
                .when(!heading && event.is_some(), |this| {
                    this.cursor(gpui::CursorStyle::PointingHand)
                        .hover(|s| s.bg(with_alpha(theme.surface_base, 0.9)))
                })
                .when_some(event.filter(|_| !heading), |this, event| {
                    this.on_click(move |_, window, cx| {
                        cx.stop_propagation();
                        emit(&callbacks, event, window, cx);
                        window.prevent_default();
                    })
                })
                .child(option.label)
                .child(if option.selected { "✓" } else { "" })
        }));

    div()
        .relative()
        .h(px(26.0))
        .w(px(width))
        .child(trigger)
        .when(open, move |root| {
            root.child(deferred(menu).with_priority(110))
        })
}

fn toolbar_separator(theme: &AudioEditorTheme) -> impl IntoElement {
    div()
        .w(px(1.0))
        .h(px(18.0))
        .mx(px(3.0))
        .bg(with_alpha(theme.border_subtle, 0.8))
}

fn tiny_action(
    id: &'static str,
    label: &'static str,
    theme: &AudioEditorTheme,
    callbacks: &AudioEditorCallbacks,
    event: AudioEditorEvent,
) -> impl IntoElement {
    toolbar_button(id, label, false, theme, callbacks, event)
}

fn inspector_label(text: &'static str, theme: &AudioEditorTheme) -> impl IntoElement {
    div()
        .text_size(px(10.0))
        .text_color(theme.text_muted)
        .child(text)
}

fn inspector_value(value: impl Into<String>, theme: &AudioEditorTheme) -> impl IntoElement {
    div()
        .text_size(px(11.0))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(theme.text_primary)
        .child(value.into())
}

fn inspector_row(
    label: &'static str,
    value: impl Into<String>,
    theme: &AudioEditorTheme,
) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .justify_between()
        .h(px(24.0))
        .child(inspector_label(label, theme))
        .child(inspector_value(value, theme))
}

fn inspector_stepper(
    id: &'static str,
    label: &'static str,
    value: String,
    minus: AudioEditorEvent,
    plus: AudioEditorEvent,
    theme: &AudioEditorTheme,
    callbacks: &AudioEditorCallbacks,
) -> impl IntoElement {
    let minus_cb = callbacks.clone();
    let plus_cb = callbacks.clone();
    div()
        .flex()
        .items_center()
        .justify_between()
        .h(px(28.0))
        .child(inspector_label(label, theme))
        .child(
            div()
                .id(id)
                .flex()
                .items_center()
                .gap(px(3.0))
                .child(
                    div()
                        .id(format!("{id}-minus"))
                        .w(px(16.0))
                        .h(px(18.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(3.0))
                        .bg(theme.surface_base)
                        .text_size(px(12.0))
                        .text_color(theme.text_secondary)
                        .cursor(gpui::CursorStyle::PointingHand)
                        .hover(|s| s.bg(theme.border_subtle))
                        .on_click(move |_, window, cx| emit(&minus_cb, minus, window, cx))
                        .child("−"),
                )
                .child(
                    div()
                        .min_w(px(58.0))
                        .flex()
                        .justify_center()
                        .child(inspector_value(value, theme)),
                )
                .child(
                    div()
                        .id(format!("{id}-plus"))
                        .w(px(16.0))
                        .h(px(18.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(3.0))
                        .bg(theme.surface_base)
                        .text_size(px(12.0))
                        .text_color(theme.text_secondary)
                        .cursor(gpui::CursorStyle::PointingHand)
                        .hover(|s| s.bg(theme.border_subtle))
                        .on_click(move |_, window, cx| emit(&plus_cb, plus, window, cx))
                        .child("+"),
                ),
        )
}

fn build_grid_lines(
    duration_beats: f32,
    pixels_per_beat: f32,
    scroll_x: f32,
    viewport_width: f32,
    view_h: f32,
    theme: &AudioEditorTheme,
) -> Vec<gpui::Div> {
    let mut lines = Vec::new();
    let ppb = pixels_per_beat.max(0.0001);
    let start = (scroll_x / ppb).floor();
    let end = start + (viewport_width / ppb).ceil() + 1.0;
    let mut beat = (start / GRID_SUBDIV).floor() * GRID_SUBDIV;
    while beat <= end.min(duration_beats + GRID_SUBDIV) {
        let x = beat * ppb - scroll_x;
        if (0.0..=viewport_width).contains(&x) {
            let is_bar = (beat % 4.0).abs() < 0.001;
            lines.push(
                div()
                    .absolute()
                    .left(px(x.round()))
                    .top(px(0.0))
                    .w(px(1.0))
                    .h(px(view_h))
                    .bg(if is_bar {
                        with_alpha(theme.border_subtle, 0.85)
                    } else {
                        with_alpha(theme.border_subtle, 0.28)
                    }),
            );
        }
        beat += GRID_SUBDIV;
    }
    lines
}

fn selection_overlay(x0: f32, x1: f32, view_h: f32, theme: &AudioEditorTheme) -> impl IntoElement {
    let left = x0.min(x1);
    let width = (x1 - x0).abs().max(1.0);
    let edge = with_alpha(theme.selection, 0.92);
    let guide = with_alpha(theme.selection, 0.62);
    let handle = |x: f32| {
        div()
            .absolute()
            .left(px(x - 3.0))
            .top(px(0.0))
            .w(px(6.0))
            .h(px(12.0))
            .rounded(px(2.0))
            .bg(edge)
            .border(px(1.0))
            .border_color(theme.surface_base)
    };
    div()
        .absolute()
        .left(px(left))
        .top(px(0.0))
        .w(px(width))
        .h(px(view_h))
        .bg(with_alpha(theme.selection, 0.16))
        .border_l(px(1.0))
        .border_r(px(1.0))
        .border_color(edge)
        // RX-style selection guides make the active range legible even over a
        // dense waveform: a bright top/bottom rail plus draggable-looking
        // anchor caps at both boundaries.
        .child(
            div()
                .absolute()
                .left_0()
                .top_0()
                .w(px(width))
                .h(px(2.0))
                .bg(guide),
        )
        .child(
            div()
                .absolute()
                .left_0()
                .bottom_0()
                .w(px(width))
                .h(px(2.0))
                .bg(guide),
        )
        .child(handle(0.0))
        .child(handle(width))
}

fn frequency_to_y(hz: f32, max_frequency_hz: f32, view_h: f32, scale: FrequencyScale) -> f32 {
    view_h * (1.0 - frequency_position(hz, max_frequency_hz, scale))
}

fn spectral_selection_overlay(
    view_h: f32,
    viewport_width: f32,
    ppb: f32,
    scroll_x: f32,
    vm: &AudioEditorViewModel,
) -> Option<impl IntoElement> {
    let selection = vm.spectral_selection?.normalized();
    let total_frames = (vm.spectrogram.duration_seconds * vm.spectrogram.sample_rate as f32)
        .round()
        .max(1.0);
    let clip_width = (vm.duration_beats * ppb).max(1.0);
    let x0 = selection.start_frame.max(0) as f32 / total_frames * clip_width - scroll_x;
    let x1 = selection.end_frame.max(0) as f32 / total_frames * clip_width - scroll_x;
    let y0 = frequency_to_y(
        selection.max_hz,
        vm.spectrogram.max_frequency_hz,
        view_h,
        vm.frequency_scale,
    );
    let y1 = frequency_to_y(
        selection.min_hz,
        vm.spectrogram.max_frequency_hz,
        view_h,
        vm.frequency_scale,
    );
    Some(
        div()
            .absolute()
            .left(px(x0.min(x1)))
            .top(px(y0.min(y1)))
            .w(px((x1 - x0).abs().max(1.0)))
            .h(px((y1 - y0).abs().max(1.0)))
            .bg(with_alpha(vm.theme.selection, 0.2))
            .border(px(1.0))
            .border_color(with_alpha(vm.theme.selection, 0.85))
            .when(x0.min(x1) < 0.0, |this| this.left(px(0.0)))
            .when(x0.max(x1) > viewport_width, |this| {
                this.w(px((viewport_width - x0.min(x1)).max(1.0)))
            }),
    )
}

fn envelope_y(value_db: f32, view_h: f32) -> f32 {
    let t = ((value_db.clamp(-60.0, 12.0) + 60.0) / 72.0).clamp(0.0, 1.0);
    view_h - 10.0 - t * (view_h - 20.0).max(1.0)
}

fn gain_envelope_layer(
    view_h: f32,
    ppb: f32,
    scroll_x: f32,
    vm: &AudioEditorViewModel,
) -> impl IntoElement {
    let clip_width = (vm.duration_beats * ppb).max(1.0);
    let points = &vm.gain_envelope.points;
    let mut children = Vec::new();
    for pair in points.windows(2) {
        let left = pair[0];
        let right = pair[1];
        let x0 = left.time.clamp(0.0, 1.0) * clip_width - scroll_x;
        let x1 = right.time.clamp(0.0, 1.0) * clip_width - scroll_x;
        let y0 = envelope_y(left.value_db, view_h);
        let y1 = envelope_y(right.value_db, view_h);
        // GPUI's retained divs do not need a canvas/path dependency here: a
        // short run of sub-pixel horizontal strokes is stable at every zoom
        // and keeps the editor render path allocation-free beyond this small
        // point list.
        let segments = 32usize;
        for segment in 0..segments {
            let t = segment as f32 / segments as f32;
            children.push(
                div()
                    .absolute()
                    .left(px(x0 + (x1 - x0) * t))
                    .top(px(y0 + (y1 - y0) * t - 0.5))
                    .w(px(((x1 - x0).abs() / segments as f32 + 1.5).max(1.0)))
                    .h(px(2.0))
                    .bg(with_alpha(vm.theme.accent, 0.95)),
            );
        }
    }
    for point in points {
        let x = point.time.clamp(0.0, 1.0) * clip_width - scroll_x;
        let y = envelope_y(point.value_db, view_h);
        children.push(
            div()
                .absolute()
                .left(px(x - 4.0))
                .top(px(y - 4.0))
                .w(px(8.0))
                .h(px(8.0))
                .rounded_full()
                .bg(vm.theme.accent)
                .border(px(1.0))
                .border_color(vm.theme.surface_base),
        );
    }
    div().absolute().inset_0().children(children)
}

fn fade_overlay(
    edge: AudioFadeEdge,
    x: f32,
    width: f32,
    view_h: f32,
    theme: &AudioEditorTheme,
) -> impl IntoElement {
    let handle_left = if matches!(edge, AudioFadeEdge::In) {
        width.max(1.0) - 2.0
    } else {
        0.0
    };
    let segments = 20;
    let curve = (0..segments)
        .map(|index| {
            let t = index as f32 / (segments - 1) as f32;
            let curve_t = if matches!(edge, AudioFadeEdge::In) {
                t
            } else {
                1.0 - t
            };
            div()
                .absolute()
                .left(px((t * width).round()))
                .top(px((view_h * (1.0 - curve_t) * 0.9).round()))
                .w(px((width / segments as f32 + 1.0).max(1.0)))
                .h(px(1.0))
                .bg(with_alpha(theme.accent, 0.85))
        })
        .collect::<Vec<_>>();
    div()
        .absolute()
        .left(px(x))
        .w(px(width.max(1.0)))
        .h(px(view_h))
        .bg(with_alpha(theme.accent, 0.08))
        .border_color(with_alpha(theme.accent, 0.7))
        .when(matches!(edge, AudioFadeEdge::In), |this| {
            this.border_r(px(1.0))
        })
        .when(matches!(edge, AudioFadeEdge::Out), |this| {
            this.border_l(px(1.0))
        })
        .child(
            div()
                .absolute()
                .left(px(handle_left))
                .top_0()
                .w(px(2.0))
                .h_full()
                .bg(with_alpha(theme.accent, 0.9)),
        )
        .child(
            div()
                .absolute()
                .left(px(0.0))
                .top(px(8.0))
                .w(px(1.0))
                .h(px(24.0))
                .bg(with_alpha(theme.accent, 0.45)),
        )
        .children(curve)
}

fn playhead_overlay(x: f32, view_h: f32, theme: &AudioEditorTheme) -> impl IntoElement {
    div()
        .absolute()
        .left(px(x - 0.5))
        .top(px(0.0))
        .w(px(1.0))
        .h(px(view_h + ruler_height()))
        .bg(theme.playhead)
}

fn warp_markers_layer(
    view_h: f32,
    ppb: f32,
    scroll_x: f32,
    vm: &AudioEditorViewModel,
) -> impl IntoElement {
    let markers = vm
        .warp_markers
        .iter()
        .map(|marker| {
            let x = marker.rel_beat * ppb - scroll_x;
            div()
                .absolute()
                .left(px(x - 0.5))
                .top_0()
                .w(px(1.0))
                .h(px(view_h))
                .bg(with_alpha(
                    vm.theme.accent,
                    if marker.locked { 0.95 } else { 0.7 },
                ))
                .child(
                    div()
                        .absolute()
                        .left(px(-3.0))
                        .top(px(2.0))
                        .w(px(7.0))
                        .h(px(7.0))
                        .bg(vm.theme.accent),
                )
        })
        .collect::<Vec<_>>();
    div().absolute().inset_0().children(markers)
}

fn toolbar(
    state: &AudioEditorState,
    vm: &AudioEditorViewModel,
    callbacks: &AudioEditorCallbacks,
) -> impl IntoElement {
    div()
        .flex_none()
        .h(px(TOOLBAR_H))
        .flex()
        .items_center()
        .gap(px(2.0))
        .px(px(8.0))
        .border_b(px(1.0))
        .border_color(vm.theme.border_subtle)
        .bg(vm.theme.surface_panel)
        .child(
            div()
                .h(px(22.0))
                .px(px(7.0))
                .flex()
                .items_center()
                .rounded(px(4.0))
                .bg(with_alpha(vm.track_color, 0.18))
                .text_size(px(11.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(vm.track_color)
                .child("AUDIO"),
        )
        .child(toolbar_separator(&vm.theme))
        .child(dropdown_button(
            "audio-tool-dropdown",
            "Tool",
            state.active_tool.label(),
            state.open_dropdown == Some(AudioEditorDropdown::Tool),
            122.0,
            &vm.theme,
            callbacks,
            AudioEditorDropdown::Tool,
            tool_options(state.active_tool),
        ))
        .child(toolbar_separator(&vm.theme))
        .child(dropdown_button(
            "audio-view-dropdown",
            "View",
            vm.view_mode.label(),
            state.open_dropdown == Some(AudioEditorDropdown::View),
            118.0,
            &vm.theme,
            callbacks,
            AudioEditorDropdown::View,
            vec![
                dropdown_option(
                    "Waveform",
                    vm.view_mode == AudioEditorViewMode::Waveform,
                    AudioEditorEvent::SetViewMode(AudioEditorViewMode::Waveform),
                ),
                dropdown_option(
                    "Spectrogram",
                    vm.view_mode == AudioEditorViewMode::Spectrogram,
                    AudioEditorEvent::SetViewMode(AudioEditorViewMode::Spectrogram),
                ),
                dropdown_option(
                    "Waveform Overlay",
                    vm.view_mode == AudioEditorViewMode::WaveformOverlay,
                    AudioEditorEvent::SetViewMode(AudioEditorViewMode::WaveformOverlay),
                ),
                dropdown_option(
                    "Spectrum",
                    vm.view_mode == AudioEditorViewMode::Spectrum,
                    AudioEditorEvent::SetViewMode(AudioEditorViewMode::Spectrum),
                ),
                dropdown_option(
                    "Samples",
                    vm.view_mode == AudioEditorViewMode::Samples,
                    AudioEditorEvent::SetViewMode(AudioEditorViewMode::Samples),
                ),
            ],
        ))
        .child(dropdown_button(
            "audio-amplitude-dropdown",
            "Amplitude",
            vm.amplitude_scale.label(),
            state.open_dropdown == Some(AudioEditorDropdown::Amplitude),
            82.0,
            &vm.theme,
            callbacks,
            AudioEditorDropdown::Amplitude,
            vec![
                dropdown_option(
                    "Linear",
                    vm.amplitude_scale == AmplitudeScale::Linear,
                    AudioEditorEvent::SetAmplitudeScale(AmplitudeScale::Linear),
                ),
                dropdown_option(
                    "dB",
                    vm.amplitude_scale == AmplitudeScale::Decibels,
                    AudioEditorEvent::SetAmplitudeScale(AmplitudeScale::Decibels),
                ),
            ],
        ))
        .child(dropdown_button(
            "audio-frequency-dropdown",
            "Frequency",
            vm.frequency_scale.label(),
            state.open_dropdown == Some(AudioEditorDropdown::Frequency),
            88.0,
            &vm.theme,
            callbacks,
            AudioEditorDropdown::Frequency,
            vec![
                dropdown_option(
                    "Linear Hz",
                    vm.frequency_scale == FrequencyScale::Linear,
                    AudioEditorEvent::SetFrequencyScale(FrequencyScale::Linear),
                ),
                dropdown_option(
                    "Log Hz",
                    vm.frequency_scale == FrequencyScale::Logarithmic,
                    AudioEditorEvent::SetFrequencyScale(FrequencyScale::Logarithmic),
                ),
            ],
        ))
        .child(toolbar_separator(&vm.theme))
        .child(dropdown_button(
            "audio-snap-dropdown",
            "Snap",
            state.snap.label(),
            state.open_dropdown == Some(AudioEditorDropdown::Snap),
            88.0,
            &vm.theme,
            callbacks,
            AudioEditorDropdown::Snap,
            vec![
                dropdown_option(
                    "Off",
                    state.snap == AudioEditorSnap::Off,
                    AudioEditorEvent::SetSnap(AudioEditorSnap::Off),
                ),
                dropdown_option(
                    "Grid",
                    state.snap == AudioEditorSnap::Grid,
                    AudioEditorEvent::SetSnap(AudioEditorSnap::Grid),
                ),
                dropdown_option(
                    "Zero Cross",
                    state.snap == AudioEditorSnap::ZeroCrossing,
                    AudioEditorEvent::SetSnap(AudioEditorSnap::ZeroCrossing),
                ),
            ],
        ))
        .child(dropdown_button(
            "audio-smooth-dropdown",
            "Smooth",
            vm.display_smoothing.label(),
            state.open_dropdown == Some(AudioEditorDropdown::Smooth),
            78.0,
            &vm.theme,
            callbacks,
            AudioEditorDropdown::Smooth,
            DisplaySmoothing::ALL
                .into_iter()
                .map(|mode| {
                    dropdown_option(
                        mode.label(),
                        vm.display_smoothing == mode,
                        AudioEditorEvent::SetDisplaySmoothing(mode),
                    )
                })
                .collect(),
        ))
        .child(div().flex_1())
        .child(tiny_action(
            "audio-fit",
            "Fit",
            &vm.theme,
            callbacks,
            AudioEditorEvent::FitClip,
        ))
        .child(tiny_action(
            "audio-vzoom-out",
            "V−",
            &vm.theme,
            callbacks,
            AudioEditorEvent::AdjustVerticalZoom { factor: 0.8 },
        ))
        .child(
            div()
                .min_w(px(36.0))
                .flex()
                .justify_center()
                .text_size(px(10.0))
                .text_color(vm.theme.text_secondary)
                .child(format!("V {:.0}%", state.viewport.vertical_zoom * 100.0)),
        )
        .child(tiny_action(
            "audio-vzoom-in",
            "V+",
            &vm.theme,
            callbacks,
            AudioEditorEvent::AdjustVerticalZoom { factor: 1.25 },
        ))
        .child(tiny_action(
            "audio-zoom-out",
            "−",
            &vm.theme,
            callbacks,
            AudioEditorEvent::ZoomBy {
                factor: 0.8,
                anchor_x: None,
            },
        ))
        .child(
            div()
                .min_w(px(42.0))
                .flex()
                .justify_center()
                .text_size(px(10.0))
                .text_color(vm.theme.text_secondary)
                .child(format!(
                    "{:.0}%",
                    state.viewport.pixels_per_beat / 48.0 * 100.0
                )),
        )
        .child(tiny_action(
            "audio-zoom-in",
            "+",
            &vm.theme,
            callbacks,
            AudioEditorEvent::ZoomBy {
                factor: 1.25,
                anchor_x: None,
            },
        ))
}

fn inspector(
    state: &AudioEditorState,
    vm: &AudioEditorViewModel,
    callbacks: &AudioEditorCallbacks,
) -> impl IntoElement {
    div()
        .id("audio-editor-inspector")
        .flex_none()
        .w(px(INSPECTOR_W))
        .h_full()
        .min_h_0()
        .px(px(10.0))
        .py(px(8.0))
        .border_r(px(1.0))
        .border_color(vm.theme.border_subtle)
        .bg(vm.theme.surface_panel)
        .overflow_y_scroll()
        .child(
            div()
                .pb(px(5.0))
                .text_size(px(10.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(vm.theme.text_secondary)
                .child("AUDIO CLIP"),
        )
        .child(inspector_stepper(
            "audio-gain",
            "Gain",
            format_db(vm.gain_db),
            AudioEditorEvent::AdjustGain { delta_db: -1.0 },
            AudioEditorEvent::AdjustGain { delta_db: 1.0 },
            &vm.theme,
            callbacks,
        ))
        .child(inspector_stepper(
            "audio-pitch",
            "Pitch",
            format!("{:+.0} st", vm.pitch_semitones),
            AudioEditorEvent::AdjustPitch {
                delta_semitones: -1.0,
            },
            AudioEditorEvent::AdjustPitch {
                delta_semitones: 1.0,
            },
            &vm.theme,
            callbacks,
        ))
        .child(inspector_stepper(
            "audio-fine-pitch",
            "Fine",
            format!("{:+.0} ct", vm.fine_cents),
            AudioEditorEvent::AdjustFinePitch { delta_cents: -10.0 },
            AudioEditorEvent::AdjustFinePitch { delta_cents: 10.0 },
            &vm.theme,
            callbacks,
        ))
        .child(
            div()
                .h(px(1.0))
                .my(px(5.0))
                .bg(with_alpha(vm.theme.border_subtle, 0.65)),
        )
        .child(inspector_row(
            "Orig. BPM",
            vm.original_bpm
                .map_or("—".into(), |bpm| format!("{bpm:.2}")),
            &vm.theme,
        ))
        .child(inspector_row(
            "Stretch",
            format!("{} · {:.0}%", vm.stretch_mode, vm.stretch_percent),
            &vm.theme,
        ))
        .child(inspector_row(
            "Fade In",
            format_beats(vm.fade_in_beats),
            &vm.theme,
        ))
        .child(inspector_row(
            "Fade Out",
            format_beats(vm.fade_out_beats),
            &vm.theme,
        ))
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .h(px(26.0))
                .child(inspector_label("Follow Tempo", &vm.theme))
                .child(dropdown_button(
                    "audio-follow-tempo-dropdown",
                    "Mode",
                    if vm.follow_tempo { "On" } else { "Off" },
                    state.open_dropdown == Some(AudioEditorDropdown::FollowTempo),
                    72.0,
                    &vm.theme,
                    callbacks,
                    AudioEditorDropdown::FollowTempo,
                    vec![
                        dropdown_option(
                            "On",
                            vm.follow_tempo,
                            AudioEditorEvent::SetFollowTempo(true),
                        ),
                        dropdown_option(
                            "Off",
                            !vm.follow_tempo,
                            AudioEditorEvent::SetFollowTempo(false),
                        ),
                    ],
                )),
        )
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .h(px(26.0))
                .child(inspector_label("Reverse", &vm.theme))
                .child(dropdown_button(
                    "audio-reverse-dropdown",
                    "Mode",
                    if vm.reverse { "On" } else { "Off" },
                    state.open_dropdown == Some(AudioEditorDropdown::Reverse),
                    72.0,
                    &vm.theme,
                    callbacks,
                    AudioEditorDropdown::Reverse,
                    vec![
                        dropdown_option("On", vm.reverse, AudioEditorEvent::SetReverse(true)),
                        dropdown_option("Off", !vm.reverse, AudioEditorEvent::SetReverse(false)),
                    ],
                )),
        )
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .h(px(26.0))
                .child(inspector_label("Channel", &vm.theme))
                .child(dropdown_button(
                    "audio-channel-dropdown",
                    "Mode",
                    vm.clip_channel.label(),
                    state.open_dropdown == Some(AudioEditorDropdown::Channel),
                    88.0,
                    &vm.theme,
                    callbacks,
                    AudioEditorDropdown::Channel,
                    ClipChannelMode::INSPECTOR
                        .into_iter()
                        .map(|mode| {
                            dropdown_option(
                                mode.label(),
                                vm.clip_channel == mode,
                                AudioEditorEvent::SetClipChannel(mode),
                            )
                        })
                        .collect(),
                )),
        )
        .child(
            div()
                .pt(px(7.0))
                .text_size(px(9.0))
                .text_color(vm.theme.text_muted)
                .truncate()
                .child(
                    vm.file_label
                        .clone()
                        .unwrap_or_else(|| "Source unavailable".to_string()),
                ),
        )
}

fn tools_sidebar(theme: &AudioEditorTheme, callbacks: &AudioEditorCallbacks) -> impl IntoElement {
    div()
        .id("audio-tools-sidebar")
        .flex_none()
        .w(px(AUDIO_EDITOR_TOOLS_WIDTH))
        .h_full()
        .px(px(8.0))
        .py(px(8.0))
        .border_l(px(1.0))
        .border_color(theme.border_subtle)
        .bg(theme.surface_panel)
        .overflow_y_scroll()
        .child(
            div()
                .pb(px(6.0))
                .text_size(px(10.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(theme.text_secondary)
                .child("TOOLS"),
        )
        .children(
            tools_menu_options()
                .into_iter()
                .enumerate()
                .map(|(index, option)| tool_sidebar_row(index, option, theme, callbacks)),
        )
}

fn tool_sidebar_row(
    index: usize,
    option: DropdownOption,
    theme: &AudioEditorTheme,
    callbacks: &AudioEditorCallbacks,
) -> impl IntoElement {
    if option.heading {
        return div()
            .id(("audio-tool-sidebar-heading", index))
            .pt(px(8.0))
            .pb(px(3.0))
            .text_size(px(9.0))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(theme.text_muted)
            .child(option.label)
            .into_any_element();
    }
    let event = option.event;
    let callbacks = callbacks.clone();
    div()
        .id(("audio-tool-sidebar-row", index))
        .h(px(22.0))
        .px(px(6.0))
        .rounded(px(4.0))
        .flex()
        .items_center()
        .text_size(px(10.5))
        .text_color(theme.text_secondary)
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(|style| {
            style
                .bg(with_alpha(theme.accent, 0.14))
                .text_color(theme.accent)
        })
        .when_some(event, |this, event| {
            this.on_mouse_down(gpui::MouseButton::Left, move |_, window, cx| {
                cx.stop_propagation();
                emit(&callbacks, event, window, cx);
                window.prevent_default();
            })
        })
        .child(option.label)
        .into_any_element()
}

fn status_bar(vm: &AudioEditorViewModel) -> impl IntoElement {
    div()
        .flex_none()
        .h(px(STATUS_H))
        .flex()
        .items_center()
        .gap(px(12.0))
        .px(px(10.0))
        .border_t(px(1.0))
        .border_color(vm.theme.border_subtle)
        .bg(vm.theme.surface_panel)
        .text_size(px(10.0))
        .text_color(vm.theme.text_muted)
        .child(status_item(
            "Selection",
            format!("{} → {}", vm.selection_start_label, vm.selection_end_label),
            &vm.theme,
        ))
        .child(status_item(
            "Duration",
            vm.selection_duration_label.clone(),
            &vm.theme,
        ))
        .child(status_item("Peak", vm.peak_label.clone(), &vm.theme))
        .children(
            (!vm.warp_markers.is_empty())
                .then(|| status_item("Warp", format!("{}", vm.warp_markers.len()), &vm.theme)),
        )
        .children({
            let first = vm.waveform.samples.first().map(|s| s.frame);
            let last = vm.waveform.samples.last().map(|s| s.frame);
            match (first, last) {
                (Some(a), Some(b)) => Some(status_item("Samples", format!("{a}–{b}"), &vm.theme)),
                _ => None,
            }
        })
        .child(div().flex_1())
        .child(status_item("Source", vm.source_summary.clone(), &vm.theme))
}

fn status_item(label: &'static str, value: String, theme: &AudioEditorTheme) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap(px(4.0))
        .child(div().text_color(theme.text_muted).child(label))
        .child(div().text_color(theme.text_secondary).child(value))
}

fn format_db(db: f32) -> String {
    if db <= -59.5 {
        "−∞ dB".to_string()
    } else {
        format!("{db:+.1} dB")
    }
}

fn format_beats(beats: f32) -> String {
    if beats.abs() < 0.005 {
        "—".to_string()
    } else {
        format!("{beats:.2} bt")
    }
}

fn spectrogram_layer(
    view_h: f32,
    viewport_width: f32,
    ppb: f32,
    scroll_x: f32,
    vm: &AudioEditorViewModel,
) -> impl IntoElement {
    let duration_seconds = vm.spectrogram.duration_seconds.max(0.001);
    let clip_width = (vm.duration_beats * ppb).max(1.0);
    let tiles = vm
        .spectrogram
        .tiles
        .iter()
        .filter_map(|tile| {
            let x = tile.start_seconds / duration_seconds * clip_width - scroll_x;
            let width = (tile.duration_seconds / duration_seconds * clip_width).max(1.0);
            if x + width < 0.0 || x > viewport_width {
                return None;
            }
            Some(
                img(Arc::clone(&tile.image))
                    .absolute()
                    .left(px(x))
                    .top_0()
                    .w(px(width))
                    .h(px(view_h))
                    .object_fit(gpui::ObjectFit::Fill),
            )
        })
        .collect::<Vec<_>>();
    let status = (!vm.spectrogram.ready).then(|| {
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .rounded_sm()
                    .border(px(1.0))
                    .border_color(if vm.spectrogram.is_error {
                        vm.theme.error
                    } else {
                        vm.theme.border_subtle
                    })
                    .bg(with_alpha(vm.theme.surface_base, 0.78))
                    .px(px(8.0))
                    .py(px(3.0))
                    .text_size(px(10.0))
                    .text_color(if vm.spectrogram.is_error {
                        vm.theme.error
                    } else {
                        vm.theme.text_muted
                    })
                    .child(vm.spectrogram.status_label.clone()),
            )
    });
    div()
        .absolute()
        .inset_0()
        .overflow_hidden()
        .bg(with_alpha(vm.theme.surface_base, 0.92))
        .children(tiles)
        .children(status)
}

fn spectrum_layer(view_h: f32, viewport_width: f32, vm: &AudioEditorViewModel) -> impl IntoElement {
    let bucket_count = vm.spectrogram.spectrum_db.len().clamp(1, 256);
    let bars = (0..bucket_count)
        .map(|bucket| {
            let start = bucket * vm.spectrogram.spectrum_db.len() / bucket_count;
            let end = ((bucket + 1) * vm.spectrogram.spectrum_db.len() / bucket_count)
                .max(start + 1)
                .min(vm.spectrogram.spectrum_db.len());
            let db = vm.spectrogram.spectrum_db[start..end]
                .iter()
                .copied()
                .fold(-120.0_f32, f32::max);
            let amount = ((db + 120.0) / 120.0).clamp(0.0, 1.0);
            let height = (amount * view_h * 0.94).max(1.0);
            div()
                .absolute()
                .left(px(bucket as f32 / bucket_count as f32 * viewport_width))
                .top(px(view_h - height))
                .w(px((viewport_width / bucket_count as f32).max(1.0)))
                .h(px(height))
                .bg(with_alpha(vm.track_color, 0.72))
        })
        .collect::<Vec<_>>();
    div()
        .absolute()
        .inset_0()
        .overflow_hidden()
        .bg(with_alpha(vm.theme.surface_base, 0.92))
        .children(bars)
}

fn visualization_ruler(
    view_h: f32,
    vm: &AudioEditorViewModel,
    mode: AudioEditorViewMode,
) -> impl IntoElement {
    let labels: Vec<(f32, String)> = if matches!(
        mode,
        AudioEditorViewMode::Spectrogram | AudioEditorViewMode::WaveformOverlay
    ) || vm.spectral_selection.is_some()
    {
        [0.0_f32, 0.25, 0.5, 0.75, 1.0]
            .into_iter()
            .map(|position| {
                let hz = vm.spectrogram.max_frequency_hz * (1.0 - position);
                (
                    position,
                    if hz >= 1000.0 {
                        format!("{:.1}k", hz / 1000.0)
                    } else {
                        format!("{hz:.0}")
                    },
                )
            })
            .collect()
    } else {
        [(0.0, "+1.0"), (0.5, "0"), (1.0, "−1.0")]
            .into_iter()
            .map(|(position, label)| (position, label.to_string()))
            .collect()
    };
    let labels = labels
        .into_iter()
        .map(|(position, label)| {
            div()
                .absolute()
                .left(px(5.0))
                .top(px((view_h * position - 6.0).max(0.0)))
                .text_size(px(9.0))
                .text_color(vm.theme.text_muted)
                .child(label)
        })
        .collect::<Vec<_>>();
    div()
        .absolute()
        .left_0()
        .top_0()
        .w(px(38.0))
        .h(px(view_h))
        .bg(with_alpha(vm.theme.surface_panel, 0.5))
        .children(labels)
}

/// Render the native audio editor body for `vm`. The panel does not own any
/// project state; all semantic edits leave through `callbacks`.
pub fn audio_editor_panel(
    vm: &AudioEditorViewModel,
    state: &AudioEditorState,
    viewport_width: f32,
    viewport_height: f32,
    callbacks: &AudioEditorCallbacks,
) -> impl IntoElement {
    let ppb = state.viewport.pixels_per_beat;
    let scroll_x = state.viewport.scroll_x;
    let viewport_width = viewport_width.max(1.0);
    let view_h = viewport_height.max(180.0);
    let grid = build_grid_lines(
        vm.duration_beats,
        ppb,
        scroll_x,
        viewport_width,
        view_h,
        &vm.theme,
    );

    let spectrogram = spectrogram_layer(view_h, viewport_width, ppb, scroll_x, vm);
    let spectrum = spectrum_layer(view_h, viewport_width, vm);
    let waveform = waveform_view(
        view_h,
        viewport_width,
        &vm.waveform,
        &vm.theme,
        vm.track_color,
        state.amplitude_scale,
        state.viewport.vertical_zoom,
        vm.display_smoothing,
    );
    let selection = vm
        .selection_range
        .map(|(a, b)| selection_overlay(a * ppb - scroll_x, b * ppb - scroll_x, view_h, &vm.theme));
    let spectral_selection = spectral_selection_overlay(view_h, viewport_width, ppb, scroll_x, vm);
    let gain_envelope = gain_envelope_layer(view_h, ppb, scroll_x, vm);
    let warp_markers = warp_markers_layer(view_h, ppb, scroll_x, vm);
    let playhead = vm
        .playhead_in_clip
        .map(|rel| playhead_overlay(rel * ppb - scroll_x, view_h, &vm.theme));
    let fade_in_w = (vm.fade_in_beats.max(0.0) * ppb).min(viewport_width);
    let fade_out_w = (vm.fade_out_beats.max(0.0) * ppb).min(viewport_width);
    let fade_in = (fade_in_w > 1.0)
        .then(|| fade_overlay(AudioFadeEdge::In, -scroll_x, fade_in_w, view_h, &vm.theme));
    let fade_out_x = (vm.duration_beats * ppb - fade_out_w - scroll_x).max(0.0);
    let fade_out = (fade_out_w > 1.0).then(|| {
        fade_overlay(
            AudioFadeEdge::Out,
            fade_out_x,
            fade_out_w,
            view_h,
            &vm.theme,
        )
    });
    let canvas_callbacks = callbacks.clone();
    let mouse_move_callbacks = callbacks.clone();
    let drag_callbacks = callbacks.clone();
    let up_callbacks = callbacks.clone();
    let up_out_callbacks = callbacks.clone();
    let ruler_callbacks = callbacks.clone();

    let ruler = div()
        .relative()
        .flex_none()
        .h(px(ruler_height()))
        .on_mouse_down(gpui::MouseButton::Left, move |event, window, cx| {
            cx.stop_propagation();
            emit(
                &ruler_callbacks,
                AudioEditorEvent::Seek {
                    x: event.position.x.into(),
                },
                window,
                cx,
            );
        })
        .child(audio_ruler(
            vm.duration_beats,
            vm.start_beat,
            vm.beats_per_bar,
            ppb,
            scroll_x,
            viewport_width,
            &vm.theme,
        ));

    let canvas = div()
        .id("audio-editor-waveform-canvas")
        .relative()
        .flex_1()
        .min_h_0()
        .overflow_hidden()
        .bg(vm.theme.surface_base)
        .on_mouse_down(gpui::MouseButton::Left, move |event, window, cx| {
            cx.stop_propagation();
            emit(
                &canvas_callbacks,
                AudioEditorEvent::PointerDown {
                    x: event.position.x.into(),
                    y: event.position.y.into(),
                    shift: event.modifiers.shift,
                },
                window,
                cx,
            );
        })
        // Keep a direct move listener alongside the typed drag listener. Some
        // platform backends begin a native drag without dispatching the typed
        // payload until after the first move; the host's explicit drag state
        // makes these move events idempotent and keeps selection/envelope
        // editing responsive on both paths.
        .on_mouse_move(move |event, window, cx| {
            emit(
                &mouse_move_callbacks,
                AudioEditorEvent::PointerMove {
                    x: event.position.x.into(),
                    y: event.position.y.into(),
                    shift: event.modifiers.shift,
                },
                window,
                cx,
            );
        })
        .on_drag(AudioEditorCanvasDrag, |_drag, _offset, _window, cx| {
            cx.new(|_| AudioEditorCanvasDrag)
        })
        .on_drag_move::<AudioEditorCanvasDrag>(
            move |event: &DragMoveEvent<AudioEditorCanvasDrag>, window, cx| {
                emit(
                    &drag_callbacks,
                    AudioEditorEvent::PointerMove {
                        x: event.event.position.x.into(),
                        y: event.event.position.y.into(),
                        shift: event.event.modifiers.shift,
                    },
                    window,
                    cx,
                );
            },
        )
        .on_mouse_up(gpui::MouseButton::Left, move |event, window, cx| {
            emit(
                &up_callbacks,
                AudioEditorEvent::PointerUp {
                    x: event.position.x.into(),
                    y: event.position.y.into(),
                    shift: event.modifiers.shift,
                },
                window,
                cx,
            );
        })
        .on_mouse_up_out(gpui::MouseButton::Left, move |event, window, cx| {
            emit(
                &up_out_callbacks,
                AudioEditorEvent::PointerUp {
                    x: event.position.x.into(),
                    y: event.position.y.into(),
                    shift: event.modifiers.shift,
                },
                window,
                cx,
            );
        })
        .child(
            div()
                .relative()
                .w(px(viewport_width))
                .h(px(view_h))
                .when(vm.view_mode.shows_spectrogram(), |this| {
                    this.child(spectrogram)
                })
                .when(vm.view_mode == AudioEditorViewMode::Spectrum, |this| {
                    this.child(spectrum)
                })
                .children(grid)
                .when(vm.view_mode.shows_waveform(), |this| this.child(waveform))
                .child(gain_envelope)
                .child(warp_markers)
                .child(visualization_ruler(view_h, vm, vm.view_mode))
                .children(fade_in)
                .children(fade_out)
                .children(selection)
                .children(spectral_selection)
                .children(playhead),
        );

    div()
        .flex()
        .flex_col()
        .size_full()
        .bg(vm.theme.surface_base)
        .child(toolbar(state, vm, callbacks))
        .child(
            div()
                .flex()
                .flex_1()
                .min_h_0()
                .child(inspector(state, vm, callbacks))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .min_h_0()
                        .child(ruler)
                        .child(canvas),
                )
                .child(tools_sidebar(&vm.theme, callbacks)),
        )
        .child(status_bar(vm))
}

/// Empty state when no audio clip is selected.
pub fn empty_audio_editor(theme: &AudioEditorTheme) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .justify_center()
        .size_full()
        .bg(theme.surface_base)
        .text_size(px(11.0))
        .text_color(theme.text_muted)
        .child("Select an audio clip to edit")
}

/// Default scroll/zoom handler for the audio editor waveform view.
pub fn default_wheel_handler(event: &ScrollWheelEvent, state: &mut AudioEditorState) {
    default_wheel_handler_at(event, state, event.position.x.into());
}

/// Apply wheel navigation using an anchor already converted into the audio
/// canvas' local coordinates. Hosts use this variant when the editor is
/// docked beside an inspector or another left-side panel.
pub fn default_wheel_handler_at(
    event: &ScrollWheelEvent,
    state: &mut AudioEditorState,
    anchor_x: f32,
) {
    let (dx, dy) = match event.delta {
        gpui::ScrollDelta::Pixels(p) => (f32::from(p.x), f32::from(p.y)),
        gpui::ScrollDelta::Lines(p) => (p.x * 36.0, p.y * 36.0),
    };
    if event.modifiers.shift {
        state.viewport.scroll_x = (state.viewport.scroll_x - dy - dx).max(0.0);
    } else if event.modifiers.control || event.modifiers.platform {
        let factor = (1.0015_f32).powf(dy);
        state.viewport.zoom_around_x(factor, anchor_x);
    } else {
        state.viewport.scroll_x = (state.viewport.scroll_x - dx).max(0.0);
    }
}

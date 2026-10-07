//! The built-in Quick Sampler's panel.
//!
//! Laid out the way a hardware or software sampler is read, top to bottom:
//!
//! * the **sample display** — a large waveform, scaled to the sample's own
//!   peak so a quiet one-shot still reads, with the play region bright and
//!   what is skipped dimmed, the loop banded, a flag on each marker (drag one
//!   to move it) and a time ruler;
//! * the **sample strip** — loop mode, direction, normalize, and the marker
//!   positions as numbers;
//! * four **modules** — pitch, filter (with its response curve), amp (with
//!   the envelope's shape) and output;
//! * a keyboard across the full width that plays through the track, with the
//!   root key marked.
//!
//! Everything drawn is GPUI paint on the GPU: antialiased paths with gradient
//! fills, no per-frame work. This file only renders: the state is the
//! insert's params plus the decoded sample's description, both
//! held by [`crate::components::quick_sampler_window::QuickSamplerEditorWindow`].
//!
//! The Slicer is the same instrument cut into slices, so its panel
//! ([`crate::components::slicer_panel`]) is built from the parts here that
//! are `pub(crate)`: the header, the display's frame and waveform, the voice
//! modules and the keyboard.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, App, Bounds, InteractiveElement, IntoElement, ParentElement, PathBuilder, Pixels,
    Rgba, StatefulInteractiveElement, Styled, Window, canvas, div, fill, linear_color_stop,
    linear_gradient, point, px, relative, size, svg,
};

use crate::assets;
use crate::components::controls::{
    FbButtonKind, FbSegment, fb_button, fb_checkbox, fb_segment, fb_segmented_track,
    fb_stepper_button,
};
use crate::components::knob::{format_pan_label, knob_bipolar, knob_with_default};
use crate::components::soundfont_player_mdi::note_label;
use crate::theme::{Colors, radius, size as ui_size, space, typography};
use quicksampler::{
    FilterMode as QuickSamplerFilter, LoopMode as QuickSamplerLoop, QuickSamplerParams,
};

pub const QUICK_SAMPLER_TITLE: &str = "Quick Sampler";

/// A loaded sample, as the panel describes and draws it.
#[derive(Clone, Debug, PartialEq)]
pub struct SampleSummary {
    pub file_name: String,
    pub sample_rate: u32,
    pub channels: usize,
    pub seconds: f64,
    /// `(min, max)` per column bucket, for the waveform.
    pub peaks: Arc<Vec<(f32, f32)>>,
}

/// The four markers on the waveform.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleMarker {
    Start,
    End,
    LoopStart,
    LoopEnd,
}

impl SampleMarker {
    /// `params` with this marker moved to `fraction` of the sample, kept on
    /// its side of its partner.
    pub fn moved(self, params: QuickSamplerParams, fraction: f32) -> QuickSamplerParams {
        let f = fraction.clamp(0.0, 1.0);
        let mut next = params;
        match self {
            SampleMarker::Start => next.start = f.min(params.end),
            SampleMarker::End => next.end = f.max(params.start),
            SampleMarker::LoopStart => next.loop_start = f.min(params.loop_end),
            SampleMarker::LoopEnd => next.loop_end = f.max(params.loop_start),
        }
        next
    }

    /// The marker nearest `fraction` among those drawn for `params`.
    pub fn nearest(params: &QuickSamplerParams, fraction: f32) -> SampleMarker {
        let mut candidates = vec![
            (SampleMarker::Start, params.start),
            (SampleMarker::End, params.end),
        ];
        if params.loop_mode != QuickSamplerLoop::Off {
            candidates.push((SampleMarker::LoopStart, params.loop_start));
            candidates.push((SampleMarker::LoopEnd, params.loop_end));
        }
        candidates
            .into_iter()
            .min_by(|a, b| (a.1 - fraction).abs().total_cmp(&(b.1 - fraction).abs()))
            .map(|(marker, _)| marker)
            .unwrap_or(SampleMarker::Start)
    }
}

/// What the panel draws: the track's sampler, the loaded sample's summary, and
/// the window's transient state.
#[derive(Clone)]
pub struct QuickSamplerPanelState {
    /// The insert's params — what its DSP in the plug-in host plays with.
    pub params: QuickSamplerParams,
    pub sample: Option<SampleSummary>,
    pub loading: bool,
    pub status: Option<String>,
    pub keyboard_root: u8,
    pub active_notes: Vec<u8>,
    pub dragging: Option<SampleMarker>,
}

impl Default for QuickSamplerPanelState {
    fn default() -> Self {
        Self {
            params: QuickSamplerParams::default(),
            sample: None,
            loading: false,
            status: None,
            keyboard_root: KEYBOARD_DEFAULT_ROOT,
            active_notes: Vec::new(),
            dragging: None,
        }
    }
}

impl QuickSamplerPanelState {
    pub fn is_playable(&self) -> bool {
        self.sample.is_some() && !self.loading
    }

    pub fn shift_keyboard_octave(&mut self, delta: i32) {
        let root = self.keyboard_root as i32 + delta * 12;
        self.keyboard_root = root.clamp(0, KEYBOARD_HIGHEST_ROOT as i32) as u8;
    }
}

pub(crate) type VoidCb = Arc<dyn Fn(&mut Window, &mut App) + 'static>;
pub(crate) type ParamsCb = Arc<dyn Fn(&QuickSamplerParams, &mut Window, &mut App) + 'static>;
pub(crate) type NoteCb = Arc<dyn Fn(&u8, &mut Window, &mut App) + 'static>;
pub(crate) type I32Cb = Arc<dyn Fn(&i32, &mut Window, &mut App) + 'static>;
pub(crate) type F32Cb = Arc<dyn Fn(&f32, &mut Window, &mut App) + 'static>;
pub(crate) type PointCb = Arc<dyn Fn(&(f32, f32), &mut Window, &mut App) + 'static>;

#[derive(Clone)]
pub struct QuickSamplerCallbacks {
    pub on_browse: VoidCb,
    /// One complete params value, so a knob drag cannot land a partial edit.
    pub on_set_params: ParamsCb,
    /// A press on the waveform, at this window-space x. The window picks the
    /// nearest marker and drags it until the button comes up.
    pub on_waveform_press: F32Cb,
    /// A right-click on the waveform, at this window-space point: the
    /// window opens its menu there.
    pub on_waveform_menu: PointCb,
    pub on_note_on: NoteCb,
    pub on_note_off: NoteCb,
    pub on_all_notes_off: VoidCb,
    pub on_shift_octave: I32Cb,
}

/// Four octaves from C2: a bass line and a lead both fit under the hands.
const KEYBOARD_DEFAULT_ROOT: u8 = 36;
const KEYBOARD_OCTAVES: usize = 4;
const KEYBOARD_WHITE_KEYS: usize = KEYBOARD_OCTAVES * 7;
pub(crate) const KEYBOARD_HIGHEST_ROOT: u8 = 127 - (KEYBOARD_OCTAVES as u8 * 12);
const KEY_H: f32 = 64.0;
/// Black key height as a share of the white key's.
const BLACK_KEY_DEPTH: f32 = 0.6;
const BLACK_KEY_WIDTH: f32 = 0.62;

pub(crate) const DISPLAY_H: f32 = 196.0;
/// Strip along the bottom of the display holding the time ruler.
pub(crate) const RULER_H: f32 = 18.0;
/// Flags on the markers: the grab handle, and where their letters sit.
pub(crate) const FLAG_W: f32 = 14.0;
pub(crate) const FLAG_H: f32 = 14.0;
pub(crate) const GRAPH_H: f32 = 58.0;
pub(crate) const KNOB_SIZE: f32 = 34.0;
const KNOB_CELL_W: f32 = 58.0;
const ENVELOPE_KNOB_MAX_MS: f32 = 4_000.0;

/// The whole panel. `waveform_bounds` is filled in by the display's paint so
/// the window can turn a pointer x into a position in the sample.
pub fn quick_sampler_panel(
    panel: &QuickSamplerPanelState,
    cb: QuickSamplerCallbacks,
    waveform_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
) -> AnyElement {
    let p = panel.params;
    div()
        .relative()
        .group(SAMPLE_DROP_GROUP)
        .flex()
        .flex_col()
        .size_full()
        .bg(Colors::surface_window())
        .child(header(panel, &cb))
        .when_some(panel.status.clone(), |root, status| {
            root.child(status_banner(status))
        })
        .child(
            div()
                .id("quick-sampler-body")
                .flex()
                .flex_col()
                .flex_1()
                .min_h(px(0.0))
                .overflow_y_scroll()
                .px(px(space::SECTION))
                .pt(px(space::LOOSE))
                .pb(px(space::SECTION))
                .gap(px(space::BASE))
                .child(sample_display(panel, &cb, waveform_bounds))
                .child(sample_strip(panel, &cb))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap(px(space::BASE))
                        .mt(px(space::TIGHT))
                        .child(pitch_module(p, &cb.on_set_params))
                        .child(filter_module(p, &cb.on_set_params))
                        .child(amp_module(p, &cb.on_set_params))
                        .child(output_module(p, &cb.on_set_params)),
                ),
        )
        .child(keyboard_footer(panel, &cb))
        .child(sample_drop_overlay("Drop to load the sample"))
        .into_any_element()
}

fn header(panel: &QuickSamplerPanelState, cb: &QuickSamplerCallbacks) -> AnyElement {
    sampler_header(
        "quick-sampler-browse",
        panel.sample.as_ref(),
        panel.loading,
        "Load or drop a WAV, FLAC, MP3, OGG or AIFF to play it across the keys",
        cb.on_browse.clone(),
    )
}

/// The strip across the top of a sampler's panel: the file's name and what
/// it is, and the button that loads another. `empty_hint` stands in for the
/// description until a file is loaded.
pub(crate) fn sampler_header(
    browse_id: &'static str,
    sample: Option<&SampleSummary>,
    loading: bool,
    empty_hint: &'static str,
    on_browse: VoidCb,
) -> AnyElement {
    let playable = sample.is_some() && !loading;
    let title = sample
        .map(|sample| sample.file_name.clone())
        .unwrap_or_else(|| "No sample".to_string());
    let subtitle = if loading {
        "Loading…".to_string()
    } else {
        match sample {
            Some(sample) => format!(
                "{:.1} kHz · {} · {}",
                sample.sample_rate as f32 / 1000.0,
                if sample.channels == 1 {
                    "mono"
                } else {
                    "stereo"
                },
                format_seconds(sample.seconds)
            ),
            None => empty_hint.to_string(),
        }
    };
    let browse = on_browse;
    div()
        .flex()
        .flex_row()
        .items_center()
        .flex_shrink_0()
        .gap(px(space::LOOSE))
        .px(px(space::SECTION))
        .py(px(space::BASE))
        .border_b(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_base())
        .child(
            div()
                .flex()
                .items_center()
                .justify_center()
                .flex_shrink_0()
                .size(px(ui_size::PROMINENT))
                .rounded(px(radius::CONTROL))
                .bg(Colors::surface_card())
                .child(
                    svg()
                        .path(assets::ICON_MUSIC_PATH)
                        .size(px(16.0))
                        .text_color(if playable {
                            Colors::accent_primary()
                        } else {
                            Colors::text_muted()
                        }),
                ),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(0.0))
                .gap(px(space::HAIR))
                .child(
                    div()
                        .truncate()
                        .text_size(px(typography::UI_MD))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(Colors::text_primary())
                        .child(title),
                )
                .child(
                    div()
                        .truncate()
                        .text_size(px(typography::UI_XS))
                        .text_color(Colors::text_muted())
                        .child(subtitle),
                ),
        )
        .child(fb_button(
            browse_id,
            "Load Sample…",
            FbButtonKind::Default,
            !loading,
            move |_, w, cx| browse(w, cx),
        ))
        .into_any_element()
}

/// The group a sampler panel's root carries, so its drop overlay shows while
/// a file is dragged anywhere over the panel.
pub(crate) const SAMPLE_DROP_GROUP: &str = "sampler-sample-drop";

/// Shown over the whole panel while a file from outside is dragged over it:
/// the window's drop handler takes it wherever it lands. Invisible (and
/// never hit-tested — it has no handlers) the rest of the time.
pub(crate) fn sample_drop_overlay(message: &'static str) -> AnyElement {
    let accent = Colors::accent_primary();
    div()
        .absolute()
        .inset_0()
        .p(px(space::SECTION))
        .opacity(0.0)
        .group_drag_over::<gpui::ExternalPaths>(SAMPLE_DROP_GROUP, |style| style.opacity(1.0))
        .child(
            div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(space::SNUG))
                .rounded(px(radius::SURFACE))
                .border_2()
                .border_dashed()
                .border_color(accent)
                .bg(Colors::with_alpha(Colors::surface_window(), 0.88))
                .child(
                    svg()
                        .path(assets::ICON_MUSIC_PATH)
                        .size(px(28.0))
                        .text_color(accent),
                )
                .child(
                    div()
                        .text_size(px(typography::UI_MD))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(Colors::text_primary())
                        .child(message),
                )
                .child(
                    div()
                        .text_size(px(typography::UI_XS))
                        .text_color(Colors::text_muted())
                        .child("WAV, FLAC, MP3, OGG, AIFF or M4A"),
                ),
        )
        .into_any_element()
}

/// What an empty sample display shows: a drop target with the way in.
pub(crate) fn empty_sample_drop_zone(loading: bool, hint: &'static str) -> AnyElement {
    div()
        .absolute()
        .inset_0()
        .p(px(space::BASE))
        .child(
            div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(space::TIGHT))
                .rounded(px(radius::CONTROL))
                .border_1()
                .border_dashed()
                .border_color(Colors::border_strong())
                .text_size(px(typography::UI_XS))
                .text_color(Colors::text_muted())
                .child(
                    svg()
                        .path(assets::ICON_MUSIC_PATH)
                        .size(px(20.0))
                        .text_color(Colors::text_faint()),
                )
                .child(if loading {
                    "Decoding…"
                } else {
                    "Drop an audio file here"
                })
                .when(!loading, |empty| {
                    empty.child(
                        div()
                            .text_size(px(typography::DENSE_CAPTION))
                            .text_color(Colors::text_faint())
                            .child(hint),
                    )
                }),
        )
        .into_any_element()
}

pub(crate) fn status_banner(message: String) -> AnyElement {
    div()
        .flex_shrink_0()
        .mx(px(space::SECTION))
        .mt(px(space::BASE))
        .px(px(space::BASE))
        .py(px(space::SNUG))
        .rounded(px(radius::CONTROL))
        .border(px(1.0))
        .border_color(Colors::status_error())
        .bg(Colors::with_alpha(Colors::status_error(), 0.12))
        .text_size(px(typography::DENSE_LABEL))
        .text_color(Colors::status_error())
        .child(message)
        .into_any_element()
}

pub(crate) fn format_seconds(seconds: f64) -> String {
    if seconds < 1.0 {
        format!("{:.0} ms", seconds * 1_000.0)
    } else if seconds < 60.0 {
        format!("{seconds:.2} s")
    } else {
        format!("{}:{:05.2}", (seconds / 60.0).floor(), seconds % 60.0)
    }
}

// ── Sample display ─────────────────────────────────────────────────────────

/// Colours the display paints with, resolved once per render (theme lookups
/// stay out of the paint closure).
#[derive(Clone, Copy)]
pub(crate) struct DisplayColors {
    pub(crate) background: Rgba,
    pub(crate) grid: Rgba,
    pub(crate) ruler: Rgba,
    pub(crate) wave_edge: Rgba,
    pub(crate) wave_peak: Rgba,
    pub(crate) wave_core: Rgba,
    pub(crate) skipped: Rgba,
    pub(crate) loop_band: Rgba,
    pub(crate) marker: Rgba,
    pub(crate) loop_marker: Rgba,
}

impl DisplayColors {
    pub(crate) fn resolve() -> Self {
        let accent = Colors::accent_primary();
        Self {
            background: Colors::surface_canvas(),
            grid: Colors::with_alpha(Colors::text_primary(), 0.05),
            ruler: Colors::surface_panel_alt(),
            wave_edge: accent,
            wave_peak: Colors::with_alpha(accent, 0.62),
            wave_core: Colors::with_alpha(accent, 0.16),
            skipped: Colors::with_alpha(Colors::surface_canvas(), 0.72),
            loop_band: Colors::with_alpha(accent, 0.09),
            marker: Colors::text_primary(),
            loop_marker: accent,
        }
    }
}

/// The large waveform display: the sample, its region, its loop, a flag on
/// each marker, and a time ruler.
fn sample_display(
    panel: &QuickSamplerPanelState,
    cb: &QuickSamplerCallbacks,
    bounds_out: Rc<Cell<Option<Bounds<Pixels>>>>,
) -> AnyElement {
    let p = panel.params;
    let sample = panel.sample.clone();
    let has_sample = sample.is_some();
    let looping = p.loop_mode != QuickSamplerLoop::Off;
    let press = cb.on_waveform_press.clone();
    let menu = cb.on_waveform_menu.clone();
    let colors = DisplayColors::resolve();
    let seconds = sample.as_ref().map(|s| s.seconds).unwrap_or(0.0);
    let peaks = sample.as_ref().map(|s| s.peaks.clone());

    let mut display = div()
        .id("quick-sampler-display")
        .relative()
        .flex_shrink_0()
        .h(px(DISPLAY_H))
        .w_full()
        .rounded(px(radius::SURFACE))
        .overflow_hidden()
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .when(has_sample, |display| {
            display
                .cursor(gpui::CursorStyle::ResizeLeftRight)
                .on_mouse_down(gpui::MouseButton::Left, move |event, w, cx| {
                    press(&f32::from(event.position.x), w, cx)
                })
                .on_mouse_down(gpui::MouseButton::Right, move |event, w, cx| {
                    menu(
                        &(f32::from(event.position.x), f32::from(event.position.y)),
                        w,
                        cx,
                    )
                })
        })
        .child(
            canvas(
                move |bounds, _, _| bounds_out.set(Some(bounds)),
                move |bounds, _, window, _| {
                    paint_display(
                        window,
                        bounds,
                        peaks.as_deref(),
                        &p,
                        looping,
                        seconds,
                        &colors,
                    )
                },
            )
            .absolute()
            .inset_0(),
        );

    if has_sample {
        // Marker letters, on the flags the paint drew.
        let mut flags = vec![(p.start, "S", false), (p.end, "E", false)];
        if looping {
            flags.push((p.loop_start, "L", true));
            flags.push((p.loop_end, "L", true));
        }
        for (fraction, letter, is_loop) in flags {
            display = display.child(marker_letter(fraction, letter, is_loop));
        }
        for (fraction, label) in ruler_labels(seconds) {
            display = display.child(
                div()
                    .absolute()
                    .bottom(px(2.0))
                    .left(relative(fraction))
                    .pl(px(3.0))
                    .text_size(px(typography::DENSE_CAPTION))
                    .text_color(Colors::text_faint())
                    .child(label),
            );
        }
    } else {
        display = display.child(empty_sample_drop_zone(
            panel.loading,
            "or use Load Sample… above",
        ));
    }
    display.into_any_element()
}

/// The letter on a marker's flag: start and end flags hang from the top, the
/// loop's stand on the ruler.
fn marker_letter(fraction: f32, letter: &'static str, is_loop: bool) -> AnyElement {
    let label = div()
        .absolute()
        .left(relative(fraction.clamp(0.0, 1.0)))
        .w(px(FLAG_W))
        .h(px(FLAG_H))
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(9.0))
        .font_weight(gpui::FontWeight::BOLD)
        .text_color(Colors::text_inverse())
        .child(letter);
    // The flag sits on the inside of the line, so it stays on screen at
    // either end of the sample.
    let label = if fraction > 0.5 {
        label.ml(px(-FLAG_W))
    } else {
        label
    };
    if is_loop {
        label.bottom(px(RULER_H)).into_any_element()
    } else {
        label.top(px(0.0)).into_any_element()
    }
}

/// A tick every "nice" step that leaves room for its label.
fn ruler_step(seconds: f64) -> f64 {
    const STEPS: [f64; 16] = [
        0.005, 0.01, 0.02, 0.05, 0.1, 0.2, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0,
    ];
    let target = seconds / 8.0;
    STEPS
        .into_iter()
        .find(|step| *step >= target)
        .unwrap_or(300.0)
}

/// `(fraction of the sample, label)` for each ruler tick after zero.
pub(crate) fn ruler_labels(seconds: f64) -> Vec<(f32, String)> {
    if seconds <= 0.0 {
        return Vec::new();
    }
    let step = ruler_step(seconds);
    let mut out = Vec::new();
    let mut t = step;
    while t < seconds * 0.97 && out.len() < 24 {
        out.push(((t / seconds) as f32, format_seconds(t)));
        t += step;
    }
    out
}

pub(crate) fn rect(x: f32, y: f32, w: f32, h: f32) -> Bounds<Pixels> {
    Bounds::new(point(px(x), px(y)), size(px(w.max(0.0)), px(h.max(0.0))))
}

pub(crate) fn vertical(top: Rgba, bottom: Rgba) -> gpui::Background {
    linear_gradient(
        180.0,
        linear_color_stop(top, 0.0),
        linear_color_stop(bottom, 1.0),
    )
}

/// Where a sample display's waveform sits, in window pixels.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DisplayFrame {
    pub(crate) x0: f32,
    pub(crate) y0: f32,
    pub(crate) w: f32,
    /// Height above the ruler.
    pub(crate) wave_h: f32,
    pub(crate) mid: f32,
    /// Pixels from the centre line to full scale.
    pub(crate) half: f32,
}

impl DisplayFrame {
    /// The x of `fraction` of the sample.
    pub(crate) fn at(&self, fraction: f32) -> f32 {
        self.x0 + fraction.clamp(0.0, 1.0) * self.w
    }
}

/// A sample display's background, ruler strip and grid: centre line, ±6 dB
/// guides, one line per ruler tick.
pub(crate) fn paint_display_frame(
    window: &mut Window,
    bounds: Bounds<Pixels>,
    seconds: f64,
    c: &DisplayColors,
) -> DisplayFrame {
    window.paint_quad(fill(bounds, c.background));
    let x0 = f32::from(bounds.origin.x);
    let y0 = f32::from(bounds.origin.y);
    let w = f32::from(bounds.size.width).max(1.0);
    let h = f32::from(bounds.size.height).max(1.0);
    let wave_h = h - RULER_H;
    let mid = y0 + wave_h * 0.5;
    let frame = DisplayFrame {
        x0,
        y0,
        w,
        wave_h,
        mid,
        half: wave_h * 0.5 - FLAG_H * 0.5 - 4.0,
    };

    window.paint_quad(fill(rect(x0, y0 + wave_h, w, RULER_H), c.ruler));
    window.paint_quad(fill(rect(x0, mid, w, 1.0), c.grid));
    for level in [0.5_f32, -0.5] {
        window.paint_quad(fill(rect(x0, mid - level * frame.half, w, 1.0), c.grid));
    }
    if seconds > 0.0 {
        let step = ruler_step(seconds);
        let mut t = step;
        while t < seconds && t / step < 64.0 {
            let x = frame.at((t / seconds) as f32);
            window.paint_quad(fill(rect(x, y0, 1.0, wave_h), c.grid));
            window.paint_quad(fill(rect(x, y0 + wave_h, 1.0, 5.0), c.marker));
            t += step;
        }
    }
    frame
}

fn paint_display(
    window: &mut Window,
    bounds: Bounds<Pixels>,
    peaks: Option<&Vec<(f32, f32)>>,
    p: &QuickSamplerParams,
    looping: bool,
    seconds: f64,
    c: &DisplayColors,
) {
    let frame = paint_display_frame(window, bounds, seconds, c);
    let DisplayFrame {
        x0, y0, w, wave_h, ..
    } = frame;
    let at = |fraction: f32| frame.at(fraction);

    if looping {
        let (from, to) = (at(p.loop_start), at(p.loop_end));
        window.paint_quad(fill(rect(from, y0, to - from, wave_h), c.loop_band));
    }

    if !paint_waveform(window, &frame, peaks, c) {
        return;
    }

    // What the region skips is dimmed over, so the bright part is what plays.
    let (start, end) = (at(p.start), at(p.end));
    window.paint_quad(fill(rect(x0, y0, start - x0, wave_h), c.skipped));
    window.paint_quad(fill(rect(end, y0, x0 + w - end, wave_h), c.skipped));

    // Markers: a line and a flag. Start and end flags hang from the top, the
    // loop's stand on the ruler; each flag sits on the inside of its line.
    let mut marker = |fraction: f32, color: Rgba, top: bool| {
        let x = at(fraction).clamp(x0, x0 + w - 1.5);
        window.paint_quad(fill(rect(x, y0, 1.5, wave_h), color));
        let flag_x = if fraction > 0.5 { x - FLAG_W + 1.5 } else { x };
        let flag_y = if top { y0 } else { y0 + wave_h - FLAG_H };
        window.paint_quad(fill(rect(flag_x, flag_y, FLAG_W, FLAG_H), color).corner_radii(px(2.0)));
    };
    marker(p.start, c.marker, true);
    marker(p.end, c.marker, true);
    if looping {
        marker(p.loop_start, c.loop_marker, false);
        marker(p.loop_end, c.loop_marker, false);
    }
}

/// The waveform, scaled to the sample's own peak. `false` when there is
/// nothing to draw.
pub(crate) fn paint_waveform(
    window: &mut Window,
    frame: &DisplayFrame,
    peaks: Option<&Vec<(f32, f32)>>,
    c: &DisplayColors,
) -> bool {
    let DisplayFrame {
        x0, w, mid, half, ..
    } = *frame;
    let Some(peaks) = peaks.filter(|peaks| !peaks.is_empty()) else {
        return false;
    };
    // Scaled to the sample's own peak: a quiet one-shot fills the display
    // like a loud one, the way a sampler shows its zone.
    let loudest = peaks.iter().fold(0.0_f32, |peak, (low, high)| {
        peak.max(low.abs()).max(high.abs())
    });
    let scale = if loudest > 1.0e-5 {
        (1.0 / loudest).min(64.0)
    } else {
        1.0
    };
    let columns = (w.floor() as usize).max(2);
    let mut tops = Vec::with_capacity(columns);
    let mut bottoms = Vec::with_capacity(columns);
    for column in 0..columns {
        let fraction = column as f32 / (columns - 1) as f32;
        let bucket = ((fraction * peaks.len() as f32) as usize).min(peaks.len() - 1);
        let (low, high) = peaks[bucket];
        let x = x0 + column as f32 * w / (columns - 1) as f32;
        tops.push((x, mid - (high * scale).clamp(-1.0, 1.0) * half));
        bottoms.push((x, mid - (low * scale).clamp(-1.0, 1.0) * half));
    }

    // Two halves, each a gradient from the peak to the centre line: bright at
    // the edge of the waveform, quiet at its core.
    let half_path = |edge: &[(f32, f32)]| {
        let mut path = PathBuilder::fill();
        path.move_to(point(px(edge[0].0), px(mid)));
        for &(x, y) in edge {
            path.line_to(point(px(x), px(y)));
        }
        path.line_to(point(px(edge[edge.len() - 1].0), px(mid)));
        path.close();
        path.build().ok()
    };
    if let Some(path) = half_path(&tops) {
        window.paint_path(path, vertical(c.wave_peak, c.wave_core));
    }
    if let Some(path) = half_path(&bottoms) {
        window.paint_path(path, vertical(c.wave_core, c.wave_peak));
    }
    for edge in [&tops, &bottoms] {
        let mut path = PathBuilder::stroke(px(1.2));
        path.move_to(point(px(edge[0].0), px(edge[0].1)));
        for &(x, y) in edge.iter().skip(1) {
            path.line_to(point(px(x), px(y)));
        }
        if let Ok(path) = path.build() {
            window.paint_path(path, c.wave_edge);
        }
    }
    true
}

// ── Sample strip ───────────────────────────────────────────────────────────

/// Under the display: how the region plays, and where its markers sit.
fn sample_strip(panel: &QuickSamplerPanelState, cb: &QuickSamplerCallbacks) -> AnyElement {
    let p = panel.params;
    let enabled = panel.is_playable();
    let seconds = panel.sample.as_ref().map(|s| s.seconds).unwrap_or(0.0);
    let toggle =
        |id: &'static str, label: &'static str, on: bool, apply: fn(&mut QuickSamplerParams)| {
            let on_change = cb.on_set_params.clone();
            fb_checkbox(id, label, on, enabled, move |_, w, cx| {
                let mut next = p;
                apply(&mut next);
                on_change(&next, w, cx);
            })
        };
    let mut loop_modes = fb_segmented_track();
    let last = QuickSamplerLoop::ALL.len() - 1;
    for (index, mode) in QuickSamplerLoop::ALL.into_iter().enumerate() {
        let on_change = cb.on_set_params.clone();
        loop_modes = loop_modes.child(fb_segment(
            ("quick-sampler-loop", index),
            mode.label(),
            p.loop_mode == mode,
            match index {
                0 => FbSegment::First,
                i if i == last => FbSegment::Last,
                _ => FbSegment::Middle,
            },
            move |_, w, cx| on_change(&with(p, |p| p.loop_mode = mode), w, cx),
        ));
    }
    let position = |label: &'static str, fraction: f32, accent: bool| {
        div()
            .flex()
            .flex_row()
            .items_baseline()
            .gap(px(space::TIGHT))
            .child(
                div()
                    .text_size(px(typography::DENSE_CAPTION))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(if accent {
                        Colors::accent_primary()
                    } else {
                        Colors::text_faint()
                    })
                    .child(label),
            )
            .child(
                div()
                    .text_size(px(typography::UI_XS))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(Colors::text_secondary())
                    .child(format_seconds(fraction as f64 * seconds)),
            )
    };
    let looping = p.loop_mode != QuickSamplerLoop::Off;
    div()
        .flex()
        .flex_row()
        .flex_wrap()
        .items_center()
        .gap(px(space::LOOSE))
        .child(loop_modes.w(px(232.0)))
        .child(toggle("quick-sampler-reverse", "Reverse", p.reverse, |p| {
            p.reverse = !p.reverse
        }))
        .child(toggle(
            "quick-sampler-normalize",
            "Normalize",
            p.normalize,
            |p| p.normalize = !p.normalize,
        ))
        .child(div().flex_1())
        .when(enabled, |strip| {
            strip
                .child(position("START", p.start, false))
                .child(position("END", p.end, false))
                .when(looping, |strip| {
                    strip
                        .child(position("LOOP", p.loop_start, true))
                        .child(position("→", p.loop_end, true))
                })
        })
        .into_any_element()
}

// ── Modules ────────────────────────────────────────────────────────────────

/// A module: a caption, an optional graph, then one row of controls, on a
/// card that grows to share the row with the others.
pub(crate) fn module(
    title: &'static str,
    graph: Option<AnyElement>,
    controls: impl IntoElement,
) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w(px(200.0))
        .gap(px(space::SNUG))
        .p(px(space::LOOSE))
        .rounded(px(radius::SURFACE))
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_panel())
        .child(
            div()
                .text_size(px(typography::DENSE_CAPTION))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(Colors::text_faint())
                .child(title),
        )
        .children(graph)
        .child(controls)
        .into_any_element()
}

pub(crate) fn controls_row() -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .flex_wrap()
        .items_start()
        .justify_between()
        .gap(px(space::TIGHT))
}

/// One knob with its caption and readout.
pub(crate) fn knob_cell(
    caption: &'static str,
    readout: String,
    control: impl IntoElement,
) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .items_center()
        .w(px(KNOB_CELL_W))
        .gap(px(space::HAIR))
        .child(control)
        .child(
            div()
                .text_size(px(typography::DENSE_CAPTION))
                .text_color(Colors::text_muted())
                .child(caption),
        )
        .child(
            div()
                .text_size(px(typography::DENSE_CAPTION))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(Colors::text_secondary())
                .child(readout),
        )
        .into_any_element()
}

/// A unipolar knob that writes one field of the params.
#[allow(clippy::too_many_arguments)]
pub(crate) fn param_knob(
    id: &'static str,
    caption: &'static str,
    readout: String,
    p: QuickSamplerParams,
    value: f32,
    (min, max, default): (f32, f32, f32),
    on_set: &ParamsCb,
    apply: impl Fn(&mut QuickSamplerParams, f32) + 'static,
) -> AnyElement {
    let on_change = on_set.clone();
    knob_cell(
        caption,
        readout,
        knob_with_default(
            id,
            value,
            min,
            max,
            KNOB_SIZE,
            Colors::accent_primary(),
            default,
            move |value, w, cx| {
                let mut next = p;
                apply(&mut next, *value);
                on_change(&next.sanitized(), w, cx);
            },
        ),
    )
}

pub(crate) fn bipolar_knob(
    id: &'static str,
    caption: &'static str,
    readout: String,
    (value, min, max): (f32, f32, f32),
    on_change: impl Fn(&f32, &mut Window, &mut App) + 'static,
) -> AnyElement {
    knob_cell(
        caption,
        readout,
        knob_bipolar(
            id,
            value,
            min,
            max,
            KNOB_SIZE,
            Colors::accent_primary(),
            None,
            0.0,
            on_change,
        ),
    )
}

pub(crate) fn percent(value: f32) -> String {
    format!("{:.0}%", value * 100.0)
}

pub(crate) fn format_ms(ms: f32) -> String {
    if ms <= 0.0 {
        "0 ms".to_string()
    } else if ms < 1_000.0 {
        format!("{ms:.0} ms")
    } else {
        format!("{:.2} s", ms / 1_000.0)
    }
}

pub(crate) fn with(
    p: QuickSamplerParams,
    apply: impl FnOnce(&mut QuickSamplerParams),
) -> QuickSamplerParams {
    let mut next = p;
    apply(&mut next);
    next.sanitized()
}

/// A `− value +` stepper for a small integer setting.
pub(crate) fn stepper(
    id: &'static str,
    caption: &'static str,
    readout: String,
    dec: QuickSamplerParams,
    inc: QuickSamplerParams,
    on_set: &ParamsCb,
) -> AnyElement {
    let on_dec = on_set.clone();
    let on_inc = on_set.clone();
    div()
        .flex()
        .flex_col()
        .items_center()
        .gap(px(space::HAIR))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::HAIR))
                .child(fb_stepper_button((id, 0usize), "−", move |_, w, cx| {
                    on_dec(&dec, w, cx)
                }))
                .child(
                    div()
                        .w(px(40.0))
                        .flex()
                        .justify_center()
                        .text_size(px(typography::UI_SM))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(Colors::text_primary())
                        .child(readout),
                )
                .child(fb_stepper_button((id, 1usize), "+", move |_, w, cx| {
                    on_inc(&inc, w, cx)
                })),
        )
        .child(
            div()
                .text_size(px(typography::DENSE_CAPTION))
                .text_color(Colors::text_muted())
                .child(caption),
        )
        .into_any_element()
}

fn pitch_module(p: QuickSamplerParams, on_set: &ParamsCb) -> AnyElement {
    let on_fine = on_set.clone();
    let on_keytrack = on_set.clone();
    module(
        "PITCH",
        None,
        div()
            .flex()
            .flex_col()
            .gap(px(space::BASE))
            .child(
                controls_row()
                    .child(stepper(
                        "quick-sampler-root",
                        "Root key",
                        note_label(p.root_note),
                        with(p, |p| p.root_note = p.root_note.saturating_sub(1)),
                        with(p, |p| p.root_note = p.root_note.saturating_add(1)),
                        on_set,
                    ))
                    .child(stepper(
                        "quick-sampler-transpose",
                        "Transpose",
                        format!("{:+}", p.transpose),
                        with(p, |p| p.transpose = p.transpose.saturating_sub(1)),
                        with(p, |p| p.transpose = p.transpose.saturating_add(1)),
                        on_set,
                    )),
            )
            .child(
                controls_row()
                    .items_center()
                    .child(bipolar_knob(
                        "quick-sampler-fine",
                        "Fine",
                        format!("{:+.0} ct", p.fine_cents),
                        (p.fine_cents, -100.0, 100.0),
                        move |value, w, cx| {
                            on_fine(&with(p, |p| p.fine_cents = value.round()), w, cx)
                        },
                    ))
                    .child(stepper(
                        "quick-sampler-bend",
                        "Bend range",
                        format!("±{}", p.pitch_bend_range),
                        with(p, |p| {
                            p.pitch_bend_range = p.pitch_bend_range.saturating_sub(1)
                        }),
                        with(p, |p| {
                            p.pitch_bend_range = p.pitch_bend_range.saturating_add(1)
                        }),
                        on_set,
                    )),
            )
            .child(fb_checkbox(
                "quick-sampler-keytrack",
                "Key tracking",
                p.keytrack,
                true,
                move |_, w, cx| on_keytrack(&with(p, |p| p.keytrack = !p.keytrack), w, cx),
            )),
    )
}

// Filter ----------------------------------------------------------------------

/// Cutoff knob position (0..1) ↔ Hz, logarithmic: each tenth of the sweep is
/// the same musical interval.
pub(crate) fn cutoff_from_position(position: f32) -> f32 {
    let (lo, hi) = (quicksampler::CUTOFF_MIN_HZ, quicksampler::CUTOFF_MAX_HZ);
    lo * (hi / lo).powf(position.clamp(0.0, 1.0))
}

pub(crate) fn cutoff_position(hz: f32) -> f32 {
    let (lo, hi) = (quicksampler::CUTOFF_MIN_HZ, quicksampler::CUTOFF_MAX_HZ);
    ((hz.clamp(lo, hi) / lo).ln() / (hi / lo).ln()).clamp(0.0, 1.0)
}

pub(crate) fn format_hz(hz: f32) -> String {
    if hz >= 1_000.0 {
        format!("{:.1} kHz", hz / 1_000.0)
    } else {
        format!("{hz:.0} Hz")
    }
}

/// Magnitude in dB of the sampler's state-variable filter at `hz`: the
/// analog prototype of the same topology, with the same resonance mapping as
/// the DSP (`k = 2 - 1.94 · resonance`).
fn filter_response_db(mode: QuickSamplerFilter, cutoff: f32, resonance: f32, hz: f32) -> f32 {
    let k = 2.0 - 1.94 * resonance.clamp(0.0, 1.0);
    let x = hz / cutoff.max(1.0);
    let denominator = ((1.0 - x * x).powi(2) + (k * x).powi(2)).sqrt().max(1.0e-9);
    let magnitude = match mode {
        QuickSamplerFilter::Off => 1.0,
        QuickSamplerFilter::LowPass => 1.0 / denominator,
        QuickSamplerFilter::HighPass => x * x / denominator,
        QuickSamplerFilter::BandPass => k * x / denominator,
    };
    20.0 * magnitude.max(1.0e-6).log10()
}

/// The filter's response across the audible range, drawn as a curve.
fn filter_graph(p: QuickSamplerParams) -> AnyElement {
    let colors = GraphColors::resolve(p.filter != QuickSamplerFilter::Off);
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            paint_graph_frame(window, bounds, &colors);
            let x0 = f32::from(bounds.origin.x);
            let y0 = f32::from(bounds.origin.y);
            let w = f32::from(bounds.size.width).max(1.0);
            let h = f32::from(bounds.size.height).max(1.0);
            // -36 dB at the bottom, +18 dB at the top; 0 dB a third down.
            let y_for = |db: f32| y0 + ((18.0 - db.clamp(-36.0, 18.0)) / 54.0) * h;
            let columns = (w as usize).max(2);
            let curve: Vec<(f32, f32)> = (0..columns)
                .map(|i| {
                    let t = i as f32 / (columns - 1) as f32;
                    let hz = 20.0 * 1_000.0_f32.powf(t);
                    (
                        x0 + t * w,
                        y_for(filter_response_db(p.filter, p.cutoff_hz, p.resonance, hz)),
                    )
                })
                .collect();
            paint_curve(window, &curve, y0 + h, &colors);
            // The cutoff, marked.
            if p.filter != QuickSamplerFilter::Off {
                let x = x0 + cutoff_position(p.cutoff_hz) * w;
                window.paint_quad(fill(rect(x, y0, 1.0, h), colors.guide));
            }
        },
    )
    .w_full()
    .h(px(GRAPH_H))
    .into_any_element()
}

pub(crate) fn filter_module(p: QuickSamplerParams, on_set: &ParamsCb) -> AnyElement {
    let mut modes = fb_segmented_track();
    let last = QuickSamplerFilter::ALL.len() - 1;
    for (index, mode) in QuickSamplerFilter::ALL.into_iter().enumerate() {
        let on_change = on_set.clone();
        modes = modes.child(fb_segment(
            ("quick-sampler-filter", index),
            mode.label(),
            p.filter == mode,
            match index {
                0 => FbSegment::First,
                i if i == last => FbSegment::Last,
                _ => FbSegment::Middle,
            },
            move |_, w, cx| on_change(&with(p, |p| p.filter = mode), w, cx),
        ));
    }
    let active = p.filter != QuickSamplerFilter::Off;
    module(
        "FILTER",
        Some(filter_graph(p)),
        div()
            .flex()
            .flex_col()
            .gap(px(space::BASE))
            .child(modes.w_full())
            .child(
                controls_row()
                    .justify_around()
                    .opacity(if active { 1.0 } else { 0.48 })
                    .child(param_knob(
                        "quick-sampler-cutoff",
                        "Cutoff",
                        format_hz(p.cutoff_hz),
                        p,
                        cutoff_position(p.cutoff_hz),
                        (0.0, 1.0, 1.0),
                        on_set,
                        |p, v| p.cutoff_hz = cutoff_from_position(v),
                    ))
                    .child(param_knob(
                        "quick-sampler-resonance",
                        "Resonance",
                        percent(p.resonance),
                        p,
                        p.resonance,
                        (0.0, 1.0, 0.0),
                        on_set,
                        |p, v| p.resonance = v,
                    )),
            ),
    )
}

// Amp -------------------------------------------------------------------------

/// The envelope's corner points across `width`: attack, decay and release
/// each take a share of the width that grows with their time, and the
/// sustain holds a fixed stretch so it is always visible.
fn envelope_points(p: &QuickSamplerParams, width: f32, height: f32) -> [(f32, f32); 5] {
    // Square-root time so a 10 ms attack is visible next to a 4 s release.
    let span = |ms: f32| (ms.max(0.0) / ENVELOPE_KNOB_MAX_MS).sqrt().min(1.0);
    let sustain_w = 0.22;
    let (a, d, r) = (span(p.attack_ms), span(p.decay_ms), span(p.release_ms));
    let total = (a + d + r).max(1.0e-6);
    let share = (1.0 - sustain_w) * (a + d + r).min(2.4) / 2.4;
    let unit = share / total;
    let xa = a * unit;
    let xd = xa + d * unit;
    let xs = xd + sustain_w;
    let xr = xs + r * unit;
    let level = |value: f32| height * (1.0 - value.clamp(0.0, 1.0));
    [
        (0.0, height),
        (xa * width, level(1.0)),
        (xd * width, level(p.sustain)),
        (xs * width, level(p.sustain)),
        (xr * width, height),
    ]
}

fn amp_graph(p: QuickSamplerParams) -> AnyElement {
    let colors = GraphColors::resolve(true);
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            paint_graph_frame(window, bounds, &colors);
            let x0 = f32::from(bounds.origin.x);
            let y0 = f32::from(bounds.origin.y);
            let w = f32::from(bounds.size.width).max(1.0);
            let h = f32::from(bounds.size.height).max(1.0);
            let inset = 4.0;
            let points = envelope_points(&p, w - inset * 2.0, h - inset * 2.0);
            let curve: Vec<(f32, f32)> = points
                .iter()
                .map(|&(x, y)| (x0 + inset + x, y0 + inset + y))
                .collect();
            paint_curve(window, &curve, y0 + h, &colors);
            for &(x, y) in &curve[1..4] {
                window.paint_quad(
                    fill(rect(x - 2.5, y - 2.5, 5.0, 5.0), colors.line).corner_radii(px(2.5)),
                );
            }
        },
    )
    .w_full()
    .h(px(GRAPH_H))
    .into_any_element()
}

pub(crate) fn amp_module(p: QuickSamplerParams, on_set: &ParamsCb) -> AnyElement {
    module(
        "AMP ENVELOPE",
        Some(amp_graph(p)),
        controls_row()
            .child(param_knob(
                "quick-sampler-attack",
                "Attack",
                format_ms(p.attack_ms),
                p,
                p.attack_ms,
                (0.0, ENVELOPE_KNOB_MAX_MS, 0.0),
                on_set,
                |p, v| p.attack_ms = v,
            ))
            .child(param_knob(
                "quick-sampler-decay",
                "Decay",
                format_ms(p.decay_ms),
                p,
                p.decay_ms,
                (0.0, ENVELOPE_KNOB_MAX_MS, 0.0),
                on_set,
                |p, v| p.decay_ms = v,
            ))
            .child(param_knob(
                "quick-sampler-sustain",
                "Sustain",
                percent(p.sustain),
                p,
                p.sustain,
                (0.0, 1.0, 1.0),
                on_set,
                |p, v| p.sustain = v,
            ))
            .child(param_knob(
                "quick-sampler-release",
                "Release",
                format_ms(p.release_ms),
                p,
                p.release_ms,
                (0.0, ENVELOPE_KNOB_MAX_MS, 30.0),
                on_set,
                |p, v| p.release_ms = v,
            )),
    )
}

pub(crate) fn output_module(p: QuickSamplerParams, on_set: &ParamsCb) -> AnyElement {
    let on_pan = on_set.clone();
    module(
        "OUTPUT",
        None,
        div()
            .flex()
            .flex_col()
            .gap(px(space::BASE))
            .child(
                controls_row()
                    .child(param_knob(
                        "quick-sampler-volume",
                        "Volume",
                        percent(p.volume),
                        p,
                        p.volume,
                        (0.0, 1.0, 0.8),
                        on_set,
                        |p, v| p.volume = v,
                    ))
                    .child(bipolar_knob(
                        "quick-sampler-pan",
                        "Pan",
                        format_pan_label(p.pan),
                        (p.pan, -1.0, 1.0),
                        move |value, w, cx| on_pan(&with(p, |p| p.pan = *value), w, cx),
                    ))
                    .child(param_knob(
                        "quick-sampler-velocity",
                        "Velocity",
                        percent(p.velocity),
                        p,
                        p.velocity,
                        (0.0, 1.0, 1.0),
                        on_set,
                        |p, v| p.velocity = v,
                    )),
            )
            .child(stepper(
                "quick-sampler-voices",
                "Voices",
                if p.polyphony == 1 {
                    "Mono".to_string()
                } else {
                    p.polyphony.to_string()
                },
                with(p, |p| p.polyphony = p.polyphony.saturating_sub(1)),
                with(p, |p| p.polyphony = p.polyphony.saturating_add(1)),
                on_set,
            )),
    )
}

// Graph painting ----------------------------------------------------------------

#[derive(Clone, Copy)]
pub(crate) struct GraphColors {
    pub(crate) background: Rgba,
    pub(crate) grid: Rgba,
    pub(crate) guide: Rgba,
    pub(crate) line: Rgba,
    pub(crate) fill_top: Rgba,
    pub(crate) fill_bottom: Rgba,
}

impl GraphColors {
    pub(crate) fn resolve(active: bool) -> Self {
        let tone = if active {
            Colors::accent_primary()
        } else {
            Colors::text_faint()
        };
        Self {
            background: Colors::surface_canvas(),
            grid: Colors::with_alpha(Colors::text_primary(), 0.05),
            guide: Colors::with_alpha(tone, 0.4),
            line: tone,
            fill_top: Colors::with_alpha(tone, 0.32),
            fill_bottom: Colors::with_alpha(tone, 0.02),
        }
    }
}

pub(crate) fn paint_graph_frame(window: &mut Window, bounds: Bounds<Pixels>, c: &GraphColors) {
    window.paint_quad(fill(bounds, c.background).corner_radii(px(radius::CONTROL)));
    let x0 = f32::from(bounds.origin.x);
    let y0 = f32::from(bounds.origin.y);
    let w = f32::from(bounds.size.width);
    let h = f32::from(bounds.size.height);
    for step in 1..4 {
        let y = y0 + h * step as f32 / 4.0;
        window.paint_quad(fill(rect(x0, y, w, 1.0), c.grid));
    }
}

/// A curve with a gradient fill down to `floor`.
pub(crate) fn paint_curve(window: &mut Window, curve: &[(f32, f32)], floor: f32, c: &GraphColors) {
    if curve.len() < 2 {
        return;
    }
    let mut body = PathBuilder::fill();
    body.move_to(point(px(curve[0].0), px(floor)));
    for &(x, y) in curve {
        body.line_to(point(px(x), px(y)));
    }
    body.line_to(point(px(curve[curve.len() - 1].0), px(floor)));
    body.close();
    if let Ok(path) = body.build() {
        window.paint_path(path, vertical(c.fill_top, c.fill_bottom));
    }
    let mut line = PathBuilder::stroke(px(1.6));
    line.move_to(point(px(curve[0].0), px(curve[0].1)));
    for &(x, y) in curve.iter().skip(1) {
        line.line_to(point(px(x), px(y)));
    }
    if let Ok(path) = line.build() {
        window.paint_path(path, c.line);
    }
}

// ── Keyboard ───────────────────────────────────────────────────────────────

const WHITE_SEMITONES: [u8; 7] = [0, 2, 4, 5, 7, 9, 11];
/// The black keys, by the white key each sits after.
const BLACK_SEMITONES: [(usize, u8); 5] = [(0, 1), (1, 3), (3, 6), (4, 8), (5, 10)];

fn keyboard_footer(panel: &QuickSamplerPanelState, cb: &QuickSamplerCallbacks) -> AnyElement {
    let root = panel.params.root_note;
    let board = piano_keyboard(
        "quick-sampler",
        panel.keyboard_root,
        &panel.active_notes,
        panel.is_playable(),
        &|pitch| (pitch == root).then(|| "ROOT".to_string()),
        cb.on_note_on.clone(),
        cb.on_note_off.clone(),
    );
    sampler_keyboard_footer(
        "quick-sampler-octave",
        panel.keyboard_root,
        &panel.active_notes,
        format!("Root {} plays the sample as recorded", note_label(root)),
        cb.on_shift_octave.clone(),
        board,
    )
}

/// The footer under a sampler's panel: a caption, what is playing, the
/// octave buttons, and `board` (a [`piano_keyboard`]).
pub(crate) fn sampler_keyboard_footer(
    octave_id: &'static str,
    keyboard_root: u8,
    active_notes: &[u8],
    hint: String,
    on_shift_octave: I32Cb,
    board: AnyElement,
) -> AnyElement {
    let down = on_shift_octave.clone();
    let up = on_shift_octave;
    let highest = keyboard_root
        .saturating_add((KEYBOARD_OCTAVES * 12) as u8 - 1)
        .min(127);
    let status = if active_notes.is_empty() {
        format!("{} – {}", note_label(keyboard_root), note_label(highest))
    } else {
        let names: Vec<String> = active_notes.iter().copied().map(note_label).collect();
        format!("Playing {}", names.join(" "))
    };
    div()
        .flex()
        .flex_col()
        .flex_shrink_0()
        .gap(px(space::SNUG))
        .px(px(space::SECTION))
        .pt(px(space::BASE))
        .pb(px(space::LOOSE))
        .border_t(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_base())
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::BASE))
                .child(
                    div()
                        .text_size(px(typography::DENSE_CAPTION))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(Colors::text_faint())
                        .child("KEYBOARD"),
                )
                .child(
                    div()
                        .flex_1()
                        .text_size(px(typography::DENSE_LABEL))
                        .text_color(Colors::text_muted())
                        .child(hint),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(typography::DENSE_LABEL))
                        .text_color(if active_notes.is_empty() {
                            Colors::text_muted()
                        } else {
                            Colors::accent_primary()
                        })
                        .child(status),
                )
                .child(fb_stepper_button(
                    (octave_id, 0usize),
                    "−",
                    move |_, w, cx| down(&-1, w, cx),
                ))
                .child(fb_stepper_button(
                    (octave_id, 1usize),
                    "+",
                    move |_, w, cx| up(&1, w, cx),
                )),
        )
        .child(board)
        .into_any_element()
}

/// Four octaves from `keyboard_root` across the full width: white keys share
/// it equally, and each black key sits on the seam after its white key, in
/// fractions of the width so the board scales with the window. A key `mark`
/// labels carries that label and a bar along its foot.
pub(crate) fn piano_keyboard(
    id: &'static str,
    keyboard_root: u8,
    active_notes: &[u8],
    playable: bool,
    mark: &dyn Fn(u8) -> Option<String>,
    note_on: NoteCb,
    note_off: NoteCb,
) -> AnyElement {
    let key_for = |index: usize, pitch: u8, black: bool| {
        key(
            (id, index * 2 + black as usize),
            pitch,
            black,
            active_notes.contains(&pitch),
            mark(pitch),
            playable,
            &note_on,
            &note_off,
        )
    };
    let mut white_row = div().flex().flex_row().size_full();
    for index in 0..KEYBOARD_WHITE_KEYS {
        let pitch =
            keyboard_root as u16 + ((index / 7) * 12) as u16 + WHITE_SEMITONES[index % 7] as u16;
        if pitch > 127 {
            break;
        }
        white_row = white_row.child(key_for(index, pitch as u8, false).flex_1().h_full());
    }
    let mut board = div()
        .relative()
        .w_full()
        .h(px(KEY_H))
        .rounded(px(radius::CONTROL))
        .overflow_hidden()
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_muted())
        .child(white_row);
    let white = 1.0 / KEYBOARD_WHITE_KEYS as f32;
    for index in 0..KEYBOARD_WHITE_KEYS {
        let Some((_, semitone)) = BLACK_SEMITONES
            .iter()
            .find(|(after, _)| *after == index % 7)
        else {
            continue;
        };
        let pitch = keyboard_root as u16 + ((index / 7) * 12) as u16 + *semitone as u16;
        if pitch > 127 || index + 1 >= KEYBOARD_WHITE_KEYS {
            continue;
        }
        let left = white * (index as f32 + 1.0 - BLACK_KEY_WIDTH / 2.0);
        board = board.child(
            key_for(index, pitch as u8, true)
                .absolute()
                .top(px(0.0))
                .left(relative(left))
                .w(relative(white * BLACK_KEY_WIDTH))
                .h(px(KEY_H * BLACK_KEY_DEPTH)),
        );
    }
    board.into_any_element()
}

#[allow(clippy::too_many_arguments)]
fn key(
    id: impl Into<gpui::ElementId>,
    pitch: u8,
    black: bool,
    active: bool,
    mark: Option<String>,
    playable: bool,
    note_on: &NoteCb,
    note_off: &NoteCb,
) -> gpui::Stateful<gpui::Div> {
    let note_on = note_on.clone();
    let note_off = note_off.clone();
    let marked = mark.is_some();
    let rest = if active {
        Colors::accent_primary()
    } else if black {
        Colors::piano_black_key()
    } else {
        Colors::piano_white_key()
    };
    let label = match mark {
        Some(label) => label,
        None if !black && pitch % 12 == 0 => note_label(pitch),
        None => String::new(),
    };
    let mut key = div()
        .id(id)
        .flex()
        .flex_col()
        .items_center()
        .justify_end()
        .pb(px(space::TIGHT))
        .border_r(px(1.0))
        .border_color(Colors::piano_key_seam())
        .bg(rest)
        .text_size(px(8.0))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(if active {
            Colors::on_accent()
        } else if marked {
            Colors::accent_pressed()
        } else {
            Colors::piano_key_label()
        })
        // A marked key carries a bar along its foot: two channels, colour
        // and shape, never colour alone.
        .when(marked && !active, |key| {
            key.child(
                div()
                    .w(relative(0.7))
                    .h(px(3.0))
                    .mb(px(2.0))
                    .rounded(px(1.5))
                    .bg(Colors::accent_primary()),
            )
        })
        .child(label);
    if black {
        key = key.rounded_b(px(radius::CONTROL_SM)).border(px(1.0));
    }
    if playable {
        let hover = Colors::composite(rest, Colors::state_hover());
        key = key
            .cursor(gpui::CursorStyle::PointingHand)
            .when(!active, |key| key.hover(move |s| s.bg(hover)))
            .on_mouse_down(gpui::MouseButton::Left, move |_, w, cx| {
                note_on(&pitch, w, cx)
            })
            .on_mouse_up(gpui::MouseButton::Left, move |_, w, cx| {
                note_off(&pitch, w, cx)
            });
    }
    key
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_marker_never_crosses_its_partner() {
        let p = QuickSamplerParams {
            start: 0.2,
            end: 0.6,
            ..QuickSamplerParams::default()
        };
        assert_eq!(SampleMarker::Start.moved(p, 0.9).start, 0.6);
        assert_eq!(SampleMarker::End.moved(p, 0.1).end, 0.2);
        assert_eq!(SampleMarker::End.moved(p, 2.0).end, 1.0);
    }

    #[test]
    fn the_nearest_marker_ignores_a_loop_that_is_off() {
        let p = QuickSamplerParams {
            start: 0.1,
            end: 0.9,
            loop_start: 0.5,
            loop_end: 0.55,
            ..QuickSamplerParams::default()
        };
        assert_eq!(SampleMarker::nearest(&p, 0.45), SampleMarker::Start);
        let looping = QuickSamplerParams {
            loop_mode: QuickSamplerLoop::Forward,
            ..p
        };
        assert_eq!(
            SampleMarker::nearest(&looping, 0.49),
            SampleMarker::LoopStart
        );
        assert_eq!(SampleMarker::nearest(&looping, 0.56), SampleMarker::LoopEnd);
    }

    #[test]
    fn the_cutoff_knob_is_logarithmic_end_to_end() {
        assert!((cutoff_from_position(0.0) - 20.0).abs() < 1.0e-3);
        assert!((cutoff_from_position(1.0) - 20_000.0).abs() < 0.5);
        assert!((cutoff_from_position(cutoff_position(1_000.0)) - 1_000.0).abs() < 0.5);
    }

    #[test]
    fn the_filter_curve_matches_the_filter() {
        let lp = |hz| filter_response_db(QuickSamplerFilter::LowPass, 1_000.0, 0.0, hz);
        // Flat below the cutoff, -6 dB at it (k = 2), falling 12 dB/octave.
        assert!(lp(50.0).abs() < 0.1);
        assert!((lp(1_000.0) + 6.02).abs() < 0.1);
        assert!((lp(8_000.0) - lp(16_000.0) - 12.0).abs() < 0.5);
        let hp = |hz| filter_response_db(QuickSamplerFilter::HighPass, 1_000.0, 0.0, hz);
        assert!(hp(16_000.0).abs() < 0.1 && hp(50.0) < -40.0);
        // Resonance lifts the peak at the cutoff.
        assert!(filter_response_db(QuickSamplerFilter::LowPass, 1_000.0, 0.9, 1_000.0) > 10.0);
    }

    #[test]
    fn the_envelope_graph_keeps_its_order_and_sustain() {
        let p = QuickSamplerParams {
            attack_ms: 10.0,
            decay_ms: 300.0,
            sustain: 0.5,
            release_ms: 4_000.0,
            ..QuickSamplerParams::default()
        };
        let points = envelope_points(&p, 200.0, 100.0);
        assert!(points.windows(2).all(|pair| pair[0].0 <= pair[1].0));
        assert_eq!(points[2].1, 50.0);
        assert_eq!(points[3].1, 50.0);
        assert!(points[4].0 <= 200.0 + 1.0e-3);
    }

    #[test]
    fn the_ruler_ticks_at_a_readable_step() {
        assert_eq!(ruler_step(3.69), 0.5);
        assert_eq!(ruler_step(0.2), 0.05);
        let labels = ruler_labels(3.69);
        assert_eq!(labels.len(), 7);
        assert_eq!(labels[0].1, "500 ms");
        assert_eq!(labels[1].1, "1.00 s");
    }
}

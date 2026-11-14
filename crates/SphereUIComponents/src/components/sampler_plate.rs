//! What Quick Sampler's and Slicer's editors share, laid out as the Quick
//! Sampler design: the Sample section (header with the mode tabs, sample
//! stepper, Load file and Bypass; the display; the line under it), the
//! Keyboard section, and the Controls section's columns — headings, help
//! lines, segmented choices, toggles, steppers, knobs, and the filter and
//! envelope graphs.
//!
//! Both instruments keep their state and wire path in their windows; the
//! render here only reads values and hands each edit to a callback. Sizes
//! are the design's (1024 × 768) and scaled by `s`; the displays paint in
//! window pixels, as the windows' hit-testing expects.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    canvas, div, prelude::FluentBuilder, px, AnyElement, App, Bounds, FontWeight,
    InteractiveElement, IntoElement, ParentElement, Pixels, Rgba, SharedString,
    StatefulInteractiveElement, Styled, Window,
};
use quicksampler::{FilterMode, QuickSamplerParams, CUTOFF_MAX_HZ, CUTOFF_MIN_HZ};

use crate::components::knob::format_pan_label;
use crate::components::plate::{
    self, color, color_a, icon, numbers, words, Dial, DialUnit, KnobCell, KnobLook, KnobSet, WHITE,
};
use crate::components::quick_sampler_panel::{
    filter_response_db, format_hz, format_ms, format_seconds, percent, with, SampleSummary,
    ENVELOPE_KNOB_MAX_MS, SAMPLE_DROP_GROUP,
};
use crate::components::soundfont_player_mdi::note_label;

// ── The design's palette ───────────────────────────────────────────────────

/// A pressed choice: light face, dark ink.
pub const ON_FACE: u32 = 0xe6e6e6;
pub const ON_INK: u32 = 0x111111;
/// A control's face and rim on the Controls section.
pub const FACE: u32 = 0x262626;
pub const FACE_EDGE: u32 = 0x4a4a4a;
/// The header's buttons, a step lighter than the section.
const HEAD_FACE: u32 = 0x222222;
const HEAD_EDGE: u32 = 0x3a3a3a;
/// Ink for text and controls.
pub const INK: u32 = 0xe6e6e6;
pub const MUTED: u32 = 0xcfcfcf;
const QUIET: u32 = 0xa6a6a6;
const ERROR: u32 = 0xff8f8f;
/// What is sounding: keys, pads, playheads.
pub const PLAYING: u32 = 0xf2a23a;
const DIVIDER: u32 = 0x484848;
const SECTION: u32 = 0x111111;
const SECTION_EDGE: u32 = 0x3a3a3a;
const CONTROLS: u32 = 0x353535;
pub const DISPLAY: u32 = 0x0b0b0b;
pub const CENTRE: u32 = 0x262626;
pub const WAVE: u32 = 0xc4c4c4;
const GRAPH: u32 = 0x2c2c2c;

// ── The design's layout ────────────────────────────────────────────────────

const SAMPLE_TOP: f32 = 6.0;
const SAMPLE_H: f32 = 396.0;
const KEYS_TOP: f32 = 408.0;
const KEYS_H: f32 = 70.0;
const CONTROLS_TOP: f32 = 484.0;
const CONTROLS_H: f32 = 278.0;
const SECTION_W: f32 = 1012.0;
/// The display inside the Sample section.
pub const DISPLAY_W: f32 = 988.0;
pub const DISPLAY_H: f32 = 300.0;
/// The keyboard: four octaves, 36 px per white key.
pub const KEYBOARD_OCTAVES: usize = 4;
const WHITE_PITCH: f32 = 36.0;

/// One action.
pub type Act = Rc<dyn Fn(&mut Window, &mut App)>;
/// A choice by index.
pub type Pick = Rc<dyn Fn(usize, &mut Window, &mut App)>;
/// A step down (−1) or up (+1).
pub type Step = Rc<dyn Fn(i32, &mut Window, &mut App)>;
/// A key or pad pressed: pitch and velocity.
pub type Press = Rc<dyn Fn(u8, u8, &mut Window, &mut App)>;
/// A key or pad let go.
pub type Release = Rc<dyn Fn(u8, &mut Window, &mut App)>;
/// A voice edit: one complete params value.
pub type SetVoice = Rc<dyn Fn(QuickSamplerParams, &mut App)>;

// ── The plate ──────────────────────────────────────────────────────────────

/// The plate at scale `s` with `children` on it, and over the whole of it a
/// drop target that shows while a file is dragged in (the window's drop
/// handler takes the file wherever it lands).
pub fn plate_root(s: f32, drop_message: &'static str, children: Vec<AnyElement>) -> AnyElement {
    div()
        .relative()
        .size_full()
        .group(SAMPLE_DROP_GROUP)
        .child(plate::plate_frame(s, children))
        .child(drop_overlay(drop_message, s))
        .into_any_element()
}

/// Invisible, and never hit-tested (it has no handlers), until a file from
/// outside is dragged over the plate.
fn drop_overlay(message: &'static str, s: f32) -> AnyElement {
    div()
        .absolute()
        .inset_0()
        .p(px(18.0 * s))
        .opacity(0.0)
        .font(plate::outfit(FontWeight::NORMAL))
        .group_drag_over::<gpui::ExternalPaths>(SAMPLE_DROP_GROUP, |style| style.opacity(1.0))
        .child(
            div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(8.0 * s))
                .rounded(px(6.0 * s))
                .border_2()
                .border_dashed()
                .border_color(color(INK))
                .bg(color_a(plate::GROUND, 0.92))
                .child(words(message, 18.0, WHITE, s).font_weight(FontWeight::SEMIBOLD))
                .child(words("WAV, FLAC, MP3, OGG, AIFF or M4A", 12.0, MUTED, s)),
        )
        .into_any_element()
}

fn section(s: f32, top: f32, height: f32, ground: u32, edge: bool) -> gpui::Div {
    div()
        .absolute()
        .left(px(6.0 * s))
        .top(px(top * s))
        .w(px(SECTION_W * s))
        .h(px(height * s))
        .rounded(px(6.0 * s))
        .bg(color(ground))
        .when(edge, |section| {
            section.border(px(1.0)).border_color(color(SECTION_EDGE))
        })
}

/// The Sample section: the header, then `display` and `info` placed by the
/// design.
pub fn sample_section(
    s: f32,
    header: AnyElement,
    display: AnyElement,
    info: AnyElement,
) -> AnyElement {
    section(s, SAMPLE_TOP, SAMPLE_H, SECTION, true)
        .child(header)
        .child(display)
        .child(info)
        .into_any_element()
}

/// The Controls section with its columns.
pub fn controls_section(s: f32, columns: Vec<AnyElement>) -> AnyElement {
    section(s, CONTROLS_TOP, CONTROLS_H, CONTROLS, false)
        .children(columns)
        .into_any_element()
}

// ── The header ─────────────────────────────────────────────────────────────

/// `name`, shortened in the middle to `max` characters.
pub fn shortened(name: &str, max: usize) -> String {
    let count = name.chars().count();
    if count <= max {
        return name.to_string();
    }
    let head: String = name.chars().take(max * 2 / 3).collect();
    let tail: String = name.chars().skip(count - (max - max * 2 / 3 - 1)).collect();
    format!("{head}\u{2026}{tail}")
}

/// What the header shows and does.
pub struct Header<'a> {
    pub title: &'static str,
    /// The mode tabs: labels, the one picked, and what a pick does.
    pub tabs: Option<(&'a [&'static str], usize, Pick)>,
    pub sample: Option<&'a SampleSummary>,
    pub loading: bool,
    /// Where the sample sits among the plug-in's samples: `(index, count)`.
    pub position: Option<(usize, usize)>,
    pub on_step_sample: Step,
    pub on_load: Act,
    /// Whether the insert is bypassed, and the toggle; `None` where the host
    /// cannot bypass it (the button shows, inert).
    pub bypass: Option<(bool, Act)>,
}

fn head_button(
    id: impl Into<gpui::ElementId>,
    child: impl IntoElement,
    on: bool,
    enabled: bool,
    s: f32,
    click: Act,
) -> gpui::Stateful<gpui::Div> {
    let (face, edge, ink) = if on {
        (ON_FACE, ON_FACE, ON_INK)
    } else {
        (HEAD_FACE, HEAD_EDGE, INK)
    };
    let button = div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .h(px(28.0 * s))
        .rounded(px(4.0 * s))
        .border(px(1.0))
        .border_color(color(edge))
        .bg(color(face))
        .text_color(color(ink))
        .text_size(plate::type_px(12.5, s))
        .child(child);
    if !enabled {
        return button.opacity(0.45);
    }
    button
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(|style| style.opacity(0.88))
        .on_click(move |_, window, cx| click(window, cx))
}

fn chevron(path: &'static str, s: f32) -> AnyElement {
    icon(path, (12.0, 12.0), (10.0, 10.0), 1.5, color(INK), s).into_any_element()
}

/// The section's header row.
pub fn header(h: Header<'_>, s: f32) -> AnyElement {
    let tabs = h.tabs.map(|(labels, picked, pick)| {
        div()
            .flex()
            .flex_row()
            .flex_shrink_0()
            .p(px(2.0 * s))
            .gap(px(2.0 * s))
            .rounded(px(5.0 * s))
            .bg(color(plate::GROUND))
            .border(px(1.0))
            .border_color(color(HEAD_EDGE))
            .children(labels.iter().enumerate().map(|(index, label)| {
                let on = index == picked;
                let pick = pick.clone();
                div()
                    .id(("sampler-tab", index))
                    .flex()
                    .items_center()
                    .h(px(26.0 * s))
                    .px(px(12.0 * s))
                    .rounded(px(3.0 * s))
                    .text_size(plate::type_px(12.5, s))
                    .text_color(color(if on { ON_INK } else { INK }))
                    .when(on, |tab| tab.bg(color(ON_FACE)))
                    .when(!on, |tab| {
                        tab.cursor(gpui::CursorStyle::PointingHand)
                            .hover(|style| style.bg(color(0x2a2a2a)))
                    })
                    .on_click(move |_, window, cx| pick(index, window, cx))
                    .child(*label)
            }))
    });

    let name = if h.loading {
        "Loading\u{2026}".to_string()
    } else {
        h.sample
            .map(|sample| shortened(&sample.file_name, 26))
            .unwrap_or_else(|| "No sample".to_string())
    };
    let index = h
        .position
        .map(|(index, count)| format!("{} / {count}", index + 1));
    let can_step = h.position.is_some_and(|(_, count)| count > 1) && !h.loading;
    let step = |delta: i32| {
        let on_step = h.on_step_sample.clone();
        Rc::new(move |window: &mut Window, cx: &mut App| on_step(delta, window, cx)) as Act
    };
    let name_box = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .flex_shrink_0()
        .w(px(220.0 * s))
        .h(px(28.0 * s))
        .px(px(10.0 * s))
        .rounded(px(4.0 * s))
        .bg(color(plate::GROUND))
        .border(px(1.0))
        .border_color(color(HEAD_EDGE))
        .child(
            words(name, 12.5, WHITE, s)
                .flex_shrink(1.0)
                .min_w(px(0.0))
                .overflow_hidden(),
        )
        .children(index.map(|index| numbers(index, 11.0, 0x9a9a9a, s).ml(px(8.0 * s))));
    let sample_group = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(4.0 * s))
        .child(
            head_button(
                "sampler-prev",
                chevron(plate::CHEVRON_LEFT, s),
                false,
                can_step,
                s,
                step(-1),
            )
            .w(px(28.0 * s)),
        )
        .child(name_box)
        .child(
            head_button(
                "sampler-next",
                chevron(plate::CHEVRON_RIGHT, s),
                false,
                can_step,
                s,
                step(1),
            )
            .w(px(28.0 * s)),
        )
        .child(
            head_button("sampler-load", "Load file", false, !h.loading, s, h.on_load)
                .px(px(12.0 * s)),
        );
    let (bypassed, toggle, can_bypass) = match h.bypass {
        Some((on, toggle)) => (on, toggle, true),
        None => (
            false,
            Rc::new(|_: &mut Window, _: &mut App| {}) as Act,
            false,
        ),
    };
    let bypass =
        head_button("sampler-bypass", "Bypass", bypassed, can_bypass, s, toggle).px(px(12.0 * s));

    div()
        .flex()
        .flex_row()
        .items_center()
        .h(px(44.0 * s))
        .pl(px(14.0 * s))
        .pr(px(10.0 * s))
        .gap(px(12.0 * s))
        .border_b(px(1.0))
        .border_color(color(0x2a2a2a))
        .child(
            words(h.title, 15.0, WHITE, s)
                .font_weight(FontWeight::SEMIBOLD)
                .line_height(px(20.0 * s)),
        )
        .children(tabs)
        .child(div().flex_1())
        .child(sample_group)
        .child(bypass)
        .into_any_element()
}

// ── The display ────────────────────────────────────────────────────────────

/// The display's box, where the design puts it; its canvas fills it.
pub fn display_box(id: &'static str, s: f32) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .absolute()
        .left(px(11.0 * s))
        .top(px(52.0 * s))
        .w(px(DISPLAY_W * s))
        .h(px(DISPLAY_H * s))
        .overflow_hidden()
        .rounded(px(4.0 * s))
        .bg(color(DISPLAY))
}

/// What an empty display shows: the way in.
pub fn empty_display(loading: bool, hint: &'static str, s: f32) -> AnyElement {
    div()
        .absolute()
        .inset_0()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(6.0 * s))
        .child(words(
            if loading {
                "Decoding\u{2026}"
            } else {
                "Drop an audio file here"
            },
            15.0,
            INK,
            s,
        ))
        .when(!loading, |zone| zone.child(words(hint, 12.0, MUTED, s)))
        .into_any_element()
}

/// The note over a bypassed display.
pub fn bypass_notice(s: f32) -> AnyElement {
    div()
        .absolute()
        .top(px(134.0 * s))
        .left_0()
        .right_0()
        .flex()
        .justify_center()
        .child(
            div()
                .px(px(12.0 * s))
                .py(px(6.0 * s))
                .rounded(px(4.0 * s))
                .bg(color(HEAD_FACE))
                .border(px(1.0))
                .border_color(color(HEAD_EDGE))
                .child(words(
                    "Bypassed. Keys light up but make no sound.",
                    12.5,
                    WHITE,
                    s,
                )),
        )
        .into_any_element()
}

/// The waveform in `bounds` (window pixels), filled, centred on `mid` and
/// `half` tall either side at the sample's own peak.
pub fn paint_wave(
    window: &mut Window,
    bounds: Bounds<Pixels>,
    peaks: &[(f32, f32)],
    mid: f32,
    half: f32,
) {
    let x0 = f32::from(bounds.origin.x);
    let w = f32::from(bounds.size.width).max(2.0);
    plate::paint_rect(window, (x0, mid, w, 1.0), color(CENTRE));
    if peaks.is_empty() {
        return;
    }
    let loudest = peaks.iter().fold(0.0_f32, |peak, (low, high)| {
        peak.max(low.abs()).max(high.abs())
    });
    let scale = if loudest > 1.0e-5 {
        (1.0 / loudest).min(64.0)
    } else {
        1.0
    };
    let columns = (w.floor() as usize).max(2);
    let mut outline: Vec<(f32, f32)> = Vec::with_capacity(columns * 2);
    let mut bottom: Vec<(f32, f32)> = Vec::with_capacity(columns);
    for column in 0..columns {
        let fraction = column as f32 / (columns - 1) as f32;
        let bucket = ((fraction * peaks.len() as f32) as usize).min(peaks.len() - 1);
        let (low, high) = peaks[bucket];
        let x = x0 + fraction * w;
        let top = (high * scale).clamp(-1.0, 1.0).max(0.004);
        let low = (low * scale).clamp(-1.0, 1.0).min(-0.004);
        outline.push((x, mid - top * half));
        bottom.push((x, mid - low * half));
    }
    outline.extend(bottom.into_iter().rev());
    plate::paint_polygon(window, &outline, color(WAVE));
}

/// A filled box in window pixels.
pub fn paint_box(window: &mut Window, (x, y, w, h): (f32, f32, f32, f32), ink: Rgba) {
    plate::paint_rect(window, (x, y, w.max(0.0), h.max(0.0)), ink);
}

/// The line under the display: what the sample is on the left, a hint or a
/// message on the right (`error` inks it as one).
pub fn info_line(left: String, right: String, error: bool, s: f32) -> AnyElement {
    div()
        .absolute()
        .left(px(12.0 * s))
        .right(px(12.0 * s))
        .top(px(358.0 * s))
        .h(px(30.0 * s))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(16.0 * s))
        .child(numbers(left, 11.5, MUTED, s))
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .overflow_hidden()
                .flex()
                .justify_end()
                .child(words(right, 12.0, if error { ERROR } else { QUIET }, s)),
        )
        .into_any_element()
}

/// What the plug-in's sample is: `2.60 s · 44.10 kHz`.
pub fn sample_facts(sample: &SampleSummary) -> String {
    format!(
        "{} \u{00B7} {:.2} kHz",
        format_seconds(sample.seconds),
        sample.sample_rate as f32 / 1_000.0
    )
}

/// The computer-keyboard hint, from `base`.
pub fn computer_keys_hint(base: u8) -> String {
    format!(
        "Computer keys {} to {}: A W S E D F T G Y H U J K O L P \u{00B7} Z and X shift octave",
        note_label(base),
        note_label(base.saturating_add(15).min(127))
    )
}

// ── The keyboard ───────────────────────────────────────────────────────────

/// How one key looks.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct KeyFace {
    /// Sounding: lit.
    pub held: bool,
    /// Plays something; an unmapped key is shaded.
    pub mapped: bool,
    /// The line over a white key's octave name, or a black key's label.
    pub sub: Option<String>,
}

const BLACK_OF: [bool; 12] = [
    false, true, false, true, false, false, true, false, true, false, true, false,
];

/// The Keyboard section: four octaves from `low`, each key drawn by `face`.
/// A press plays harder the lower it lands on the key.
pub fn keyboard(
    s: f32,
    low: u8,
    face: impl Fn(u8) -> KeyFace,
    press: Press,
    release: Release,
) -> AnyElement {
    let area: Rc<Cell<Option<Bounds<Pixels>>>> = Rc::new(Cell::new(None));
    let mut whites: Vec<AnyElement> = Vec::new();
    let mut blacks: Vec<AnyElement> = Vec::new();
    let mut white_index = 0usize;
    for offset in 0..(KEYBOARD_OCTAVES * 12) as u8 {
        let Some(pitch) = low.checked_add(offset).filter(|pitch| *pitch <= 127) else {
            break;
        };
        let black = BLACK_OF[pitch as usize % 12];
        let key = face(pitch);
        let (x, w, h) = if black {
            (white_index as f32 * WHITE_PITCH - 11.0, 22.0, 38.0)
        } else {
            (white_index as f32 * WHITE_PITCH, 35.0, 64.0)
        };
        let fill = match (key.held, key.mapped, black) {
            (true, _, _) => PLAYING,
            (false, true, false) => 0xd9d9d9,
            (false, false, false) => 0x8a8a8a,
            (false, true, true) => 0x2a2a2a,
            (false, false, true) => 0x1a1a1a,
        };
        let press = press.clone();
        let release = release.clone();
        let release_out = release.clone();
        let area = area.clone();
        let element = div()
            .id(("sampler-key", pitch as usize))
            .absolute()
            .left(px(x * s))
            .top_0()
            .w(px(w * s))
            .h(px(h * s))
            .flex()
            .flex_col()
            .justify_end()
            .items_center()
            .gap(px(1.0 * s))
            .pb(px(if black { 3.0 } else { 4.0 } * s))
            .rounded_b(px(3.0 * s))
            .bg(color(fill))
            .when(black, |key| {
                key.border_l(px(1.0))
                    .border_r(px(1.0))
                    .border_b(px(1.0))
                    .border_color(color(0x111111))
            })
            .cursor(gpui::CursorStyle::PointingHand)
            .on_mouse_down(gpui::MouseButton::Left, move |event, window, cx| {
                let depth = area
                    .get()
                    .map(|bounds| {
                        let top = f32::from(bounds.origin.y);
                        ((f32::from(event.position.y) - top) / (h * s)).clamp(0.0, 1.0)
                    })
                    .unwrap_or(0.6);
                let velocity = (50.0 + 77.0 * depth).round() as u8;
                press(pitch, velocity, window, cx);
            })
            .on_mouse_up(gpui::MouseButton::Left, move |_, window, cx| {
                release(pitch, window, cx)
            })
            .on_mouse_up_out(gpui::MouseButton::Left, move |_, window, cx| {
                release_out(pitch, window, cx)
            });
        let ink = if black {
            if key.held {
                ON_INK
            } else {
                MUTED
            }
        } else {
            ON_INK
        };
        let element = if black {
            element.children(key.sub.map(|sub| numbers(sub, 9.5, ink, s)))
        } else {
            let label = (pitch % 12 == 0).then(|| note_label(pitch));
            element
                .children(
                    key.sub
                        .map(|sub| numbers(sub, 10.0, ink, s).font_weight(FontWeight::SEMIBOLD)),
                )
                .children(label.map(|label| numbers(label, 10.0, ink, s)))
        };
        if black {
            blacks.push(element.into_any_element());
        } else {
            white_index += 1;
            whites.push(element.into_any_element());
        }
    }
    let record = area.clone();
    section(s, KEYS_TOP, KEYS_H, SECTION, true)
        .child(
            div()
                .absolute()
                .left(px(1.0 * s))
                .top(px(2.0 * s))
                .w(px(1008.0 * s))
                .h(px(64.0 * s))
                .child(
                    canvas(
                        move |bounds, _, _| record.set(Some(bounds)),
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .size_full(),
                )
                .children(whites)
                .children(blacks),
        )
        .into_any_element()
}

// ── The Controls section ───────────────────────────────────────────────────

/// One column of the Controls section, `left` and `width` as the design
/// places it; every column after the first has a rule on its left.
pub fn column(s: f32, left: f32, width: f32, first: bool) -> gpui::Div {
    div()
        .absolute()
        .top_0()
        .bottom_0()
        .left(px(left * s))
        .w(px(width * s))
        .px(px(14.0 * s))
        .py(px(12.0 * s))
        .flex()
        .flex_col()
        .gap(px(10.0 * s))
        .when(!first, |column| {
            column.border_l(px(1.0)).border_color(color(DIVIDER))
        })
}

/// A column's heading.
pub fn heading(text: impl Into<SharedString>, s: f32) -> gpui::Div {
    words(text, 13.0, WHITE, s)
        .font_weight(FontWeight::SEMIBOLD)
        .line_height(px(18.0 * s))
}

/// A heading with something at its right end.
pub fn heading_row(text: impl Into<SharedString>, right: AnyElement, s: f32) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .child(heading(text, s))
        .child(right)
}

/// A line or two of help.
pub fn help(text: impl Into<SharedString>, s: f32) -> gpui::Div {
    div()
        .text_size(plate::type_px(12.0, s))
        .line_height(px(17.4 * s))
        .text_color(color(MUTED))
        .child(text.into())
}

/// A small reading in mono.
pub fn reading(text: impl Into<SharedString>, ink: u32, s: f32) -> gpui::Div {
    numbers(text, 11.5, ink, s)
}

/// A segmented choice: `labels`, `picked` lit, each press a pick.
pub fn segmented(
    id: &'static str,
    labels: &[&str],
    picked: Option<usize>,
    enabled: bool,
    s: f32,
    pick: Pick,
) -> AnyElement {
    div()
        .flex()
        .flex_row()
        .flex_shrink_0()
        .p(px(2.0 * s))
        .gap(px(2.0 * s))
        .rounded(px(4.0 * s))
        .bg(color(FACE))
        .border(px(1.0))
        .border_color(color(FACE_EDGE))
        .when(!enabled, |group| group.opacity(0.45))
        .children(labels.iter().enumerate().map(|(index, label)| {
            let on = picked == Some(index);
            let pick = pick.clone();
            div()
                .id((id, index))
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .h(px(24.0 * s))
                .px(px(4.0 * s))
                .rounded(px(3.0 * s))
                .whitespace_nowrap()
                .text_size(plate::type_px(12.0, s))
                .text_color(color(if on { ON_INK } else { INK }))
                .when(on, |item| item.bg(color(ON_FACE)))
                .when(enabled && !on, |item| {
                    item.cursor(gpui::CursorStyle::PointingHand)
                        .hover(|style| style.bg(color(0x303030)))
                        .on_click(move |_, window, cx| pick(index, window, cx))
                })
                .child(label.to_string())
                .into_any_element()
        }))
        .into_any_element()
}

/// A toggle button, `height` tall: lit while `on`.
#[allow(clippy::too_many_arguments)]
pub fn toggle(
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
    on: bool,
    enabled: bool,
    height: f32,
    size: f32,
    s: f32,
    click: Act,
) -> gpui::Stateful<gpui::Div> {
    let (face, edge, ink) = if on {
        (ON_FACE, ON_FACE, ON_INK)
    } else {
        (FACE, FACE_EDGE, INK)
    };
    let button = div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .h(px(height * s))
        .px(px(if height < 24.0 { 10.0 } else { 12.0 } * s))
        .rounded(px(if height < 24.0 { 3.0 } else { 4.0 } * s))
        .border(px(1.0))
        .border_color(color(edge))
        .bg(color(face))
        .text_color(color(ink))
        .text_size(plate::type_px(size, s))
        .child(label.into());
    if !enabled {
        return button.opacity(0.45);
    }
    button
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(|style| style.opacity(0.88))
        .on_click(move |_, window, cx| click(window, cx))
}

fn step_button(
    id: impl Into<gpui::ElementId>,
    glyph: &'static str,
    enabled: bool,
    s: f32,
    click: Act,
) -> AnyElement {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .size(px(26.0 * s))
        .rounded(px(4.0 * s))
        .bg(color(FACE))
        .border(px(1.0))
        .border_color(color(FACE_EDGE))
        .text_size(plate::type_px(14.0, s))
        .text_color(color(INK))
        .child(glyph)
        .when(!enabled, |button| button.opacity(0.45))
        .when(enabled, |button| {
            button
                .cursor(gpui::CursorStyle::PointingHand)
                .hover(|style| style.bg(color(0x303030)))
                .on_click(move |_, window, cx| click(window, cx))
        })
        .into_any_element()
}

/// `Label  −  value  +` on one row.
pub fn stepper_row(
    id: &'static str,
    label: &str,
    value: impl Into<SharedString>,
    s: f32,
    step: Step,
) -> AnyElement {
    let down = step.clone();
    let up = step;
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0 * s))
        .child(div().w(px(64.0 * s)).flex_shrink_0().child(words(
            label.to_string(),
            12.0,
            MUTED,
            s,
        )))
        .child(step_button(
            (id, 0usize),
            "\u{2212}",
            true,
            s,
            Rc::new(move |window, cx| down(-1, window, cx)),
        ))
        .child(
            div()
                .w(px(50.0 * s))
                .flex()
                .justify_center()
                .child(numbers(value, 13.0, WHITE, s)),
        )
        .child(step_button(
            (id, 1usize),
            "+",
            true,
            s,
            Rc::new(move |window, cx| up(1, window, cx)),
        ))
        .into_any_element()
}

/// A plain button: a one-off action.
pub fn action(
    id: &'static str,
    label: &'static str,
    enabled: bool,
    s: f32,
    click: Act,
) -> AnyElement {
    toggle(id, label, false, enabled, 28.0, 12.0, s, click)
        .px(px(10.0 * s))
        .into_any_element()
}

/// The knobs' row at a column's foot.
pub fn knob_row(knobs: Vec<AnyElement>, spread: bool, s: f32) -> gpui::Div {
    div()
        .mt_auto()
        .flex()
        .flex_row()
        .items_end()
        .when(spread, |row| row.justify_between())
        .when(!spread, |row| row.gap(px(16.0 * s)))
        .children(knobs)
}

const KNOB_LOOK: KnobLook =
    KnobLook::small(44.0).styled(4.0, 17.3, 0x2a2a2a, FACE_EDGE, 1.0, 8.0, 2.0);
const KNOB_CELL: KnobCell = KnobCell::new(64.0, 12.0, 11.5)
    .label_below()
    .gap(3.0)
    .label(0.0, INK)
    .plain_label();

/// A knob: label and reading under it, `set` taking each new value.
#[allow(clippy::too_many_arguments)]
pub fn knob(
    key: impl Into<SharedString>,
    dial: Dial,
    value: f32,
    readout: String,
    enabled: bool,
    s: f32,
    set: KnobSet,
) -> AnyElement {
    plate::knob_with(
        key,
        dial,
        value,
        Some(readout),
        KNOB_LOOK,
        KNOB_CELL,
        color(INK),
        enabled,
        s,
        set,
    )
}

/// A knob over one voice param.
#[allow(clippy::too_many_arguments)]
pub fn voice_knob(
    key: &'static str,
    dial: Dial,
    value: f32,
    readout: String,
    enabled: bool,
    s: f32,
    p: QuickSamplerParams,
    set: &SetVoice,
    apply: impl Fn(&mut QuickSamplerParams, f32) + 'static,
) -> AnyElement {
    let set = set.clone();
    knob(
        key,
        dial,
        value,
        readout,
        enabled,
        s,
        Rc::new(move |value: f32, cx: &mut App| set(with(p, |p| apply(p, value)), cx)),
    )
}

/// A whole-number stepper row over one voice param.
pub fn voice_stepper(
    id: &'static str,
    label: &str,
    value: impl Into<SharedString>,
    s: f32,
    p: QuickSamplerParams,
    set: &SetVoice,
    apply: impl Fn(&mut QuickSamplerParams, i32) + 'static,
) -> AnyElement {
    let set = set.clone();
    stepper_row(
        id,
        label,
        value,
        s,
        Rc::new(move |delta: i32, _: &mut Window, cx: &mut App| {
            set(with(p, |p| apply(p, delta)), cx)
        }),
    )
}

/// The pitch wheel's reach, semitones either way.
pub fn bend_row(p: QuickSamplerParams, set: &SetVoice, s: f32) -> AnyElement {
    voice_stepper(
        "sampler-bend",
        "Bend",
        format!("\u{00B1}{}", p.pitch_bend_range),
        s,
        p,
        set,
        |p, delta| p.pitch_bend_range = (p.pitch_bend_range as i32 + delta).clamp(0, 255) as u8,
    )
}

/// Decibels for a 0..1 level.
pub fn level_db(level: f32) -> f32 {
    if level <= 0.0 {
        GAIN_FLOOR_DB
    } else {
        (20.0 * level.log10()).max(GAIN_FLOOR_DB)
    }
}

/// The level for a gain in decibels; the floor is silence.
pub fn db_level(db: f32) -> f32 {
    if db <= GAIN_FLOOR_DB {
        0.0
    } else {
        10.0_f32.powf(db / 20.0).min(1.0)
    }
}

const GAIN_FLOOR_DB: f32 = -48.0;

/// `0.0 dB`, `−6.0 dB`, `−∞ dB`.
pub fn fmt_gain(db: f32) -> String {
    if db <= GAIN_FLOOR_DB {
        "\u{2212}\u{221E} dB".to_string()
    } else {
        format!("{db:.1} dB").replace('-', "\u{2212}")
    }
}

/// Transpose and Fine, the Pitch column's first knobs.
pub fn transpose_knob(p: QuickSamplerParams, set: &SetVoice, s: f32) -> AnyElement {
    voice_knob(
        "sampler-transpose",
        Dial::new("transpose", "Transpose", -24.0, 24.0, 0.0, DialUnit::Plain).bipolar(),
        p.transpose as f32,
        format!("{} st", p.transpose).replace('-', "\u{2212}"),
        true,
        s,
        p,
        set,
        |p, value| p.transpose = value.round() as i8,
    )
}

pub fn fine_knob(p: QuickSamplerParams, set: &SetVoice, s: f32) -> AnyElement {
    voice_knob(
        "sampler-fine",
        Dial::new("fine", "Fine", -100.0, 100.0, 0.0, DialUnit::Plain).bipolar(),
        p.fine_cents,
        format!("{:.0} ct", p.fine_cents).replace('-', "\u{2212}"),
        true,
        s,
        p,
        set,
        |p, value| p.fine_cents = value.round(),
    )
}

// ── Filter ─────────────────────────────────────────────────────────────────

/// The filter types a press picks, in the design's order.
pub const FILTER_TYPES: [(FilterMode, &str); 3] = [
    (FilterMode::LowPass, "Low pass"),
    (FilterMode::BandPass, "Band"),
    (FilterMode::HighPass, "High pass"),
];

/// The Filter column. `remembered` is the type a switched-off filter comes
/// back on as.
pub fn filter_column(
    p: QuickSamplerParams,
    remembered: FilterMode,
    set: &SetVoice,
    s: f32,
    left: f32,
) -> AnyElement {
    let on = p.filter != FilterMode::Off;
    let kind = if on { p.filter } else { remembered };
    let kind = if kind == FilterMode::Off {
        FilterMode::LowPass
    } else {
        kind
    };
    let toggle_set = set.clone();
    let toggle_button = toggle(
        "sampler-filter-on",
        if on { "On" } else { "Off" },
        on,
        true,
        22.0,
        11.5,
        s,
        Rc::new(move |_, cx| {
            toggle_set(
                with(p, |p| p.filter = if on { FilterMode::Off } else { kind }),
                cx,
            )
        }),
    )
    .into_any_element();
    let pick_set = set.clone();
    let types: Vec<&str> = FILTER_TYPES.iter().map(|(_, label)| *label).collect();
    let picked = FILTER_TYPES.iter().position(|(mode, _)| *mode == kind);
    let cutoff = Dial::new(
        "cutoff",
        "Cutoff",
        CUTOFF_MIN_HZ,
        CUTOFF_MAX_HZ,
        CUTOFF_MAX_HZ,
        DialUnit::Hz,
    )
    .log();
    column(s, left, 260.0, false)
        .child(heading_row("Filter", toggle_button, s))
        .child(segmented(
            "sampler-filter-type",
            &types,
            picked,
            on,
            s,
            Rc::new(move |index, _, cx| {
                if let Some((mode, _)) = FILTER_TYPES.get(index) {
                    pick_set(with(p, |p| p.filter = *mode), cx);
                }
            }),
        ))
        .child(filter_graph(kind, p.cutoff_hz, p.resonance, on, s))
        .child(
            knob_row(
                vec![
                    voice_knob(
                        "sampler-cutoff",
                        cutoff,
                        p.cutoff_hz,
                        format_hz(p.cutoff_hz),
                        on,
                        s,
                        p,
                        set,
                        |p, value| p.cutoff_hz = value,
                    ),
                    voice_knob(
                        "sampler-resonance",
                        Dial::new("resonance", "Resonance", 0.0, 1.0, 0.0, DialUnit::Q),
                        p.resonance,
                        percent(p.resonance),
                        on,
                        s,
                        p,
                        set,
                        |p, value| p.resonance = value,
                    ),
                ],
                true,
                s,
            )
            .justify_around(),
        )
        .into_any_element()
}

/// The filter's response, 20 Hz to 20 kHz, the 0 dB line a third down.
fn filter_graph(mode: FilterMode, cutoff: f32, resonance: f32, on: bool, s: f32) -> AnyElement {
    div()
        .w(px(232.0 * s))
        .h(px(46.0 * s))
        .flex_shrink_0()
        .rounded(px(3.0 * s))
        .bg(color(GRAPH))
        .overflow_hidden()
        .when(!on, |graph| graph.opacity(0.35))
        .child(
            canvas(
                |_, _, _| (),
                move |bounds, _, window, _| {
                    let x0 = f32::from(bounds.origin.x);
                    let y0 = f32::from(bounds.origin.y);
                    let w = f32::from(bounds.size.width);
                    let h = f32::from(bounds.size.height);
                    let k = h / 46.0;
                    let zero = y0 + 15.5 * k;
                    plate::paint_rect(window, (x0, zero, w, 1.0), color(0x444444));
                    let points: Vec<(f32, f32)> = (0..=116)
                        .map(|i| {
                            let t = i as f32 / 116.0;
                            let hz = 20.0 * 1_000.0_f32.powf(t);
                            let db = filter_response_db(mode, cutoff, resonance, hz);
                            let y = (zero - db * 1.1 * k).clamp(y0 + 1.0, y0 + h - 1.0);
                            (x0 + t * w, y)
                        })
                        .collect();
                    plate::paint_polyline(window, &points, 1.6, color(INK), None);
                },
            )
            .size_full(),
        )
        .into_any_element()
}

// ── Amp envelope ───────────────────────────────────────────────────────────

/// The envelope's outline in a 272 × 60 box: attack, decay, a stretch of
/// sustain, release — or, for a one-shot, attack then full level to the end.
pub fn envelope_outline(p: &QuickSamplerParams, one_shot: bool) -> Vec<(f32, f32)> {
    let root = |ms: f32| ms.max(0.0).sqrt();
    let (a, d, r) = (root(p.attack_ms), root(p.decay_ms), root(p.release_ms));
    if one_shot {
        let xa = 4.0 + 240.0 * a / (a + 40.0);
        return vec![(4.0, 56.0), (xa, 6.0), (262.0, 6.0), (268.0, 56.0)];
    }
    let total = (a + d + r).max(1.0e-3);
    let scale = 200.0 / total;
    let xa = 4.0 + a * scale;
    let xd = xa + d * scale;
    let ys = 6.0 + (1.0 - p.sustain.clamp(0.0, 1.0)) * 50.0;
    vec![
        (4.0, 56.0),
        (xa, 6.0),
        (xd, ys),
        ((xd + 60.0).min(266.0), ys),
        (268.0, 56.0),
    ]
}

fn envelope_graph(p: QuickSamplerParams, one_shot: bool, s: f32) -> AnyElement {
    let outline = envelope_outline(&p, one_shot);
    div()
        .w(px(272.0 * s))
        .h(px(60.0 * s))
        .flex_shrink_0()
        .rounded(px(3.0 * s))
        .bg(color(GRAPH))
        .overflow_hidden()
        .child(
            canvas(
                |_, _, _| (),
                move |bounds, _, window, _| {
                    let x0 = f32::from(bounds.origin.x);
                    let y0 = f32::from(bounds.origin.y);
                    let k = f32::from(bounds.size.width) / 272.0;
                    let points: Vec<(f32, f32)> = outline
                        .iter()
                        .map(|(x, y)| (x0 + x * k, y0 + y * k))
                        .collect();
                    plate::paint_polygon(window, &points, color_a(0xffffff, 0.08));
                    plate::paint_polyline(window, &points, 1.6, color(INK), None);
                },
            )
            .size_full(),
        )
        .into_any_element()
}

/// The Amp envelope column. `corner` is the button at the heading's end (the
/// loop switch), `one_shot` dims what a one-shot skips.
pub fn amp_column(
    p: QuickSamplerParams,
    corner: Option<AnyElement>,
    one_shot: bool,
    help_text: &str,
    set: &SetVoice,
    s: f32,
    left: f32,
) -> AnyElement {
    let time = |id: &'static str, label: &'static str, default: f32| {
        Dial::new(
            id,
            label,
            0.0,
            ENVELOPE_KNOB_MAX_MS,
            default,
            DialUnit::Time,
        )
    };
    let heading_line = match corner {
        Some(corner) => heading_row("Amp envelope", corner, s),
        None => div().child(heading("Amp envelope", s)),
    };
    column(s, left, 300.0, false)
        .child(heading_line)
        .child(envelope_graph(p, one_shot, s))
        .child(help(help_text.to_string(), s))
        .child(knob_row(
            vec![
                voice_knob(
                    "sampler-attack",
                    time("attack", "Attack", 0.0),
                    p.attack_ms,
                    format_ms(p.attack_ms),
                    true,
                    s,
                    p,
                    set,
                    |p, value| p.attack_ms = value,
                ),
                voice_knob(
                    "sampler-decay",
                    time("decay", "Decay", 0.0),
                    p.decay_ms,
                    format_ms(p.decay_ms),
                    !one_shot,
                    s,
                    p,
                    set,
                    |p, value| p.decay_ms = value,
                ),
                voice_knob(
                    "sampler-sustain",
                    Dial::new("sustain", "Sustain", 0.0, 1.0, 1.0, DialUnit::Q),
                    p.sustain,
                    percent(p.sustain),
                    !one_shot,
                    s,
                    p,
                    set,
                    |p, value| p.sustain = value,
                ),
                voice_knob(
                    "sampler-release",
                    time("release", "Release", 30.0),
                    p.release_ms,
                    format_ms(p.release_ms),
                    !one_shot,
                    s,
                    p,
                    set,
                    |p, value| p.release_ms = value,
                ),
            ],
            true,
            s,
        ))
        .into_any_element()
}

// ── Output ─────────────────────────────────────────────────────────────────

/// A gain knob over the voice's level, in decibels.
pub fn gain_knob(p: QuickSamplerParams, set: &SetVoice, s: f32) -> AnyElement {
    let db = level_db(p.volume);
    voice_knob(
        "sampler-gain",
        Dial::new(
            "gain",
            "Gain",
            GAIN_FLOOR_DB,
            0.0,
            level_db(0.8),
            DialUnit::Db,
        ),
        db,
        fmt_gain(db),
        true,
        s,
        p,
        set,
        |p, value| p.volume = db_level(value),
    )
}

/// The Output column: velocity, gain, pan.
pub fn output_column(p: QuickSamplerParams, set: &SetVoice, s: f32, left: f32) -> AnyElement {
    column(s, left, 222.0, false)
        .child(heading("Output", s))
        .child(help(
            "Velocity sets how much softer a light hit plays. Click lower on a key to play \
             harder.",
            s,
        ))
        .child(knob_row(
            vec![
                voice_knob(
                    "sampler-velocity",
                    Dial::new("velocity", "Velocity", 0.0, 1.0, 1.0, DialUnit::Q),
                    p.velocity,
                    percent(p.velocity),
                    true,
                    s,
                    p,
                    set,
                    |p, value| p.velocity = value,
                ),
                gain_knob(p, set, s),
                voice_knob(
                    "sampler-pan",
                    Dial::new("pan", "Pan", -1.0, 1.0, 0.0, DialUnit::Bias).bipolar(),
                    p.pan,
                    format_pan_label(p.pan),
                    true,
                    s,
                    p,
                    set,
                    |p, value| p.pan = value,
                ),
            ],
            true,
            s,
        ))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gain_reads_in_decibels_and_round_trips() {
        assert_eq!(fmt_gain(level_db(1.0)), "0.0 dB");
        assert!((db_level(level_db(0.5)) - 0.5).abs() < 1.0e-5);
        assert_eq!(db_level(GAIN_FLOOR_DB), 0.0);
        assert_eq!(fmt_gain(level_db(0.0)), "\u{2212}\u{221E} dB");
    }

    #[test]
    fn the_envelope_outline_stays_in_its_box() {
        let p = QuickSamplerParams {
            attack_ms: 4_000.0,
            decay_ms: 4_000.0,
            release_ms: 4_000.0,
            sustain: 0.0,
            ..QuickSamplerParams::default()
        };
        for one_shot in [false, true] {
            for (x, y) in envelope_outline(&p, one_shot) {
                assert!((0.0..=272.0).contains(&x) && (0.0..=60.0).contains(&y));
            }
        }
        let zero = QuickSamplerParams {
            attack_ms: 0.0,
            decay_ms: 0.0,
            release_ms: 0.0,
            ..QuickSamplerParams::default()
        };
        assert!(envelope_outline(&zero, false)
            .iter()
            .all(|(x, _)| x.is_finite()));
    }

    #[test]
    fn the_keyboard_hint_names_the_sixteen_keys() {
        assert!(computer_keys_hint(48).starts_with("Computer keys C3 to D#4"));
    }
}

/// The note offset a computer key plays from the computer keyboard's base:
/// the home row and the row above it, as a piano's whites and blacks.
pub fn computer_key_offset(key: &str) -> Option<u8> {
    const KEYS: [&str; 16] = [
        "a", "w", "s", "e", "d", "f", "t", "g", "y", "h", "u", "j", "k", "o", "l", "p",
    ];
    KEYS.iter().position(|k| *k == key).map(|index| index as u8)
}

/// The computer keyboard's base after Z (−1) or X (+1): an octave down or
/// up, kept so all sixteen keys stay on the MIDI range.
pub fn shifted_computer_base(base: u8, octaves: i32) -> u8 {
    (base as i32 + octaves * 12).clamp(0, 112) as u8
}

/// Where the on-screen keyboard starts so the computer keyboard's sixteen
/// keys are on it: unchanged when they already are.
pub fn keyboard_low_for(low: u8, base: u8, highest_low: u8) -> u8 {
    let span = (KEYBOARD_OCTAVES * 12) as u8;
    if base >= low && base as u32 + 15 < low as u32 + span as u32 {
        return low;
    }
    base.saturating_sub(12).min(highest_low) / 12 * 12
}

#[cfg(test)]
mod computer_key_tests {
    use super::*;

    #[test]
    fn the_computer_keys_play_a_piano_octave_and_a_bit() {
        assert_eq!(computer_key_offset("a"), Some(0));
        assert_eq!(computer_key_offset("w"), Some(1));
        assert_eq!(computer_key_offset("k"), Some(12));
        assert_eq!(computer_key_offset("p"), Some(15));
        assert_eq!(computer_key_offset("z"), None);
        assert_eq!(shifted_computer_base(48, -5), 0);
        assert_eq!(shifted_computer_base(108, 1), 112);
        // Already on screen: the keyboard stays put; off it, it follows.
        assert_eq!(keyboard_low_for(36, 48, 79), 36);
        assert_eq!(keyboard_low_for(36, 84, 79), 72);
        assert_eq!(keyboard_low_for(36, 12, 79), 0);
    }
}

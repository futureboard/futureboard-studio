//! The plate the redesigned built-in editors are drawn on — BurnLimit,
//! EQUZ8, EQUZX and Zapp.
//!
//! Each design is a fixed [`PLATE_W`] × [`PLATE_H`] artboard. [`plate_frame`]
//! lays it out at that size and scales it to the window by one factor `s`,
//! letterboxed on the plate's own ground; every size below is at the design
//! size and multiplied by `s`.
//!
//! The designs carry their own look rather than Studio's tokens: their
//! palette, Outfit for words and IBM Plex Mono for numbers — both embedded
//! and registered once ([`ensure_fonts`]) — and the knobs, pills, switches,
//! cards and header they share. The editors are [`KitModel`]s: every edit is
//! a wire param through [`KitEditor`], and the live displays are painted by
//! the kit window's overlay into [`LiveSlot::Plate`] bounds.

use std::f32::consts::PI;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};

use gpui::{
    anchored, canvas, deferred, div, fill, point, px, rgb, rgba, Anchor, AnyElement, App,
    AppContext, Bounds, Context, ElementId, Font, FontFallbacks, FontWeight, Hsla,
    InteractiveElement, IntoElement, ParentElement, PathBuilder, Pixels, Point, Rgba,
    ScrollWheelEvent, SharedString, StatefulInteractiveElement, Styled, TextAlign, TextRun, Window,
};

use crate::components::fader::{drag_to, is_reset_click, FaderDrag};
use crate::components::plugin_kit::{Cx, KitEditor, KitModel, LiveBounds, LiveSlot};
use crate::components::title_bar::TITLEBAR_HEIGHT;

pub const PLATE_W: f32 = 1024.0;
pub const PLATE_H: f32 = 768.0;
/// The title bar and the identity strip the plug-in shell puts over the
/// plate.
pub const SHELL_CHROME_H: f32 = TITLEBAR_HEIGHT + 30.0;
/// The window at the design size, and the smallest it may get.
pub const WINDOW_SIZE: (f32, f32) = (PLATE_W, PLATE_H + SHELL_CHROME_H);
pub const MIN_WINDOW_SIZE: (f32, f32) = (640.0, 480.0 + SHELL_CHROME_H);

// ── Palette ────────────────────────────────────────────────────────────────

/// The ground the plate stands on, and the letterbox around it.
pub const GROUND: u32 = 0x1c1c1c;
/// The display well along the top, and its edge.
pub const WELL: u32 = 0x111111;
pub const WELL_EDGE: u32 = 0x3a3a3a;
/// The control deck under it, and the cards on the deck.
pub const DECK: u32 = 0x353535;
pub const CARD: u32 = 0x2c2c2c;
pub const CARD_EDGE: u32 = 0x424242;
/// Text, brightest to faintest.
pub const WHITE: u32 = 0xffffff;
pub const INK: u32 = 0xe6e6e6;
pub const PALE: u32 = 0xc4c4c4;
pub const SOFT: u32 = 0xb0b0b0;
pub const FAINT: u32 = 0x8f8f8f;
pub const GHOST: u32 = 0x6f6f6f;
pub const OFF: u32 = 0x6a6a6a;
/// Icon strokes at rest.
pub const ICON: u32 = 0xcfcfcf;
/// The dynamic-EQ blue, and the even harmonics.
pub const DYN: u32 = 0x5db7f5;
/// A knob's track, its face and the face's rim.
pub const TRACK: u32 = 0x1a1a1a;
pub const FACE: u32 = 0x1f1f1f;
pub const FACE_EDGE: u32 = 0x4a4a4a;

pub fn color(hex: u32) -> Rgba {
    rgb(hex)
}

/// `hex` (0xRRGGBB) at `alpha`.
pub fn color_a(hex: u32, alpha: f32) -> Rgba {
    let a = (alpha.clamp(0.0, 1.0) * 255.0).round() as u32;
    rgba((hex << 8) | a)
}

// ── Type ───────────────────────────────────────────────────────────────────

const OUTFIT_FAMILY: &str = "Outfit";
const PLEX_MONO_FAMILY: &str = "IBM Plex Mono";
static FACES: [&[u8]; 8] = [
    include_bytes!("../../assets/fonts/outfit/Outfit-Light.ttf"),
    include_bytes!("../../assets/fonts/outfit/Outfit-Regular.ttf"),
    include_bytes!("../../assets/fonts/outfit/Outfit-Medium.ttf"),
    include_bytes!("../../assets/fonts/outfit/Outfit-SemiBold.ttf"),
    include_bytes!("../../assets/fonts/outfit/Outfit-Bold.ttf"),
    include_bytes!("../../assets/fonts/ibm-plex-mono/IBMPlexMono-Regular.ttf"),
    include_bytes!("../../assets/fonts/ibm-plex-mono/IBMPlexMono-Medium.ttf"),
    include_bytes!("../../assets/fonts/ibm-plex-mono/IBMPlexMono-SemiBold.ttf"),
];
static REGISTERED: AtomicBool = AtomicBool::new(false);

/// Adds the embedded Outfit (300–700) and IBM Plex Mono (400–600) faces to
/// the text system, once per process.
pub fn ensure_fonts(cx: &App) {
    if REGISTERED.swap(true, Ordering::AcqRel) {
        return;
    }
    let faces = FACES.map(std::borrow::Cow::Borrowed).to_vec();
    if let Err(error) = cx.text_system().add_fonts(faces) {
        eprintln!("[plate] Outfit / IBM Plex Mono could not be registered: {error}");
        REGISTERED.store(false, Ordering::Release);
    }
}

/// `family` at `weight`, falling back to Studio's UI chain for anything it
/// lacks (the hair space, non-Latin text).
fn embedded(family: &'static str, weight: FontWeight) -> Font {
    let mut font = crate::theme::ui_font_weight(weight);
    let mut chain = vec![font.family.to_string()];
    if let Some(fallbacks) = font.fallbacks.as_ref() {
        chain.extend(fallbacks.fallback_list().iter().cloned());
    }
    font.family = family.into();
    font.fallbacks = Some(FontFallbacks::from_fonts(chain));
    font
}

/// The designs' word face.
pub fn outfit(weight: FontWeight) -> Font {
    embedded(OUTFIT_FAMILY, weight)
}

/// The designs' number face.
pub fn mono(weight: FontWeight) -> Font {
    embedded(PLEX_MONO_FAMILY, weight)
}

/// A div font size at the design size `size`, scaled by `s`, undoing
/// Studio's global text scale so the plate's type keeps the mockup's sizes.
pub fn type_px(size: f32, s: f32) -> Pixels {
    px(size * s / crate::theme::UI_TEXT_SCALE)
}

/// `text` tracked out by about `em`, the way CSS letter-spacing spaces every
/// character: GPUI text has no letter spacing, so hair spaces stand in —
/// none below 0.07 em, then one per 0.08 em.
pub fn spaced(text: &str, em: f32) -> String {
    let gaps = if em < 0.07 {
        0
    } else {
        (em / 0.08).round() as usize
    };
    let mut out = String::with_capacity(text.len() * (1 + 3 * gaps));
    for (i, ch) in text.chars().enumerate() {
        if i > 0 {
            for _ in 0..gaps {
                out.push('\u{200A}');
            }
        }
        out.push(ch);
    }
    out
}

/// [`spaced`], upper-cased: the designs' captions.
pub fn tracked(text: &str, em: f32) -> String {
    spaced(&text.to_uppercase(), em)
}
/// A line of words.
pub fn words(text: impl Into<SharedString>, size: f32, ink: u32, s: f32) -> gpui::Div {
    div()
        .flex_shrink_0()
        .whitespace_nowrap()
        .text_size(type_px(size, s))
        .line_height(px(size * 1.26 * s))
        .text_color(color(ink))
        .child(text.into())
}

/// A line of numbers.
pub fn numbers(text: impl Into<SharedString>, size: f32, ink: u32, s: f32) -> gpui::Div {
    div()
        .flex_shrink_0()
        .whitespace_nowrap()
        .font(mono(FontWeight::NORMAL))
        .text_size(type_px(size, s))
        .line_height(px(size * 1.3 * s))
        .text_color(color(ink))
        .child(text.into())
}

/// A tracked, medium-weight caption: `LIMITER`, `STYLE`, `FREQ`.
pub fn caption(text: &str, size: f32, em: f32, ink: u32, s: f32) -> gpui::Div {
    words(tracked(text, em), size, ink, s).font_weight(FontWeight::MEDIUM)
}

/// A card's title: 13 px semibold, tracked.
pub fn card_title(text: &str, s: f32) -> gpui::Div {
    words(tracked(text, 0.1), 13.0, WHITE, s)
        .font_weight(FontWeight::SEMIBOLD)
        .line_height(px(22.0 * s))
}

// ── Formatting, as the designs print values ────────────────────────────────

/// `+1.5`, `−3.0`, `0.0`: a true minus, no sign at zero.
pub fn signed(v: f32) -> String {
    let sign = if v < -0.05 {
        "\u{2212}"
    } else if v > 0.05 {
        "+"
    } else {
        ""
    };
    format!("{sign}{:.1}", v.abs())
}

pub fn fmt_db(v: f32) -> String {
    format!("{} dB", signed(v))
}

pub fn fmt_ms(v: f32) -> String {
    if v < 10.0 {
        format!("{v:.1} ms")
    } else {
        format!("{} ms", v.round())
    }
}

/// `31.5 Hz`, `240 Hz`, `2.60 kHz`, `11.0 kHz`.
pub fn fmt_hz(v: f32) -> String {
    if v < 100.0 {
        format!("{v:.1} Hz")
    } else if v < 1_000.0 {
        format!("{} Hz", v.round())
    } else if v < 10_000.0 {
        format!("{:.2} kHz", v / 1_000.0)
    } else {
        format!("{:.1} kHz", v / 1_000.0)
    }
}

/// [`fmt_hz`] in whole hertz below a kilohertz.
pub fn fmt_hz_round(v: f32) -> String {
    if v < 1_000.0 {
        format!("{} Hz", v.round())
    } else {
        fmt_hz(v)
    }
}

/// How a dial prints and rounds its value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialUnit {
    Db,
    Ms,
    Hz,
    HzRound,
    Percent,
    Ratio,
    Q,
    /// −1…1 shown as a signed percentage.
    Bias,
    Seconds,
    /// A bare number to one decimal: a 0–10 knob.
    Plain,
}

impl DialUnit {
    pub fn format(self, v: f32) -> String {
        match self {
            Self::Db => fmt_db(v),
            Self::Ms => fmt_ms(v),
            Self::Hz => fmt_hz(v),
            Self::HzRound => fmt_hz_round(v),
            Self::Percent => format!("{}%", v.round()),
            Self::Ratio => format!("{v:.1}:1"),
            Self::Q => format!("{v:.2}"),
            Self::Bias => format!("{}%", signed(v * 100.0).replace(".0", "")),
            Self::Seconds => format!("{v:.1} s"),
            Self::Plain => format!("{v:.1}"),
        }
    }

    /// `v` rounded to what the readout shows, so a drag never leaves a value
    /// behind that the readout cannot.
    pub fn round(self, v: f32) -> f32 {
        let step = match self {
            Self::Db => 0.1,
            Self::Ms if v < 10.0 => 0.1,
            Self::Ms => 1.0,
            Self::Hz if v < 100.0 => 0.1,
            Self::Hz | Self::HzRound if v < 1_000.0 => 1.0,
            Self::Hz | Self::HzRound if v < 10_000.0 => 10.0,
            Self::Hz | Self::HzRound => 100.0,
            Self::Percent => 1.0,
            Self::Ratio | Self::Plain => 0.1,
            Self::Seconds => 0.01,
            Self::Q | Self::Bias => 0.01,
        };
        (v / step).round() * step
    }
}

/// One knob's param: range, taper, readout and reset value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Dial {
    pub id: &'static str,
    pub label: &'static str,
    pub min: f32,
    pub max: f32,
    pub log: bool,
    /// Its arc grows from the top (zero) either way.
    pub bipolar: bool,
    pub default: f32,
    pub unit: DialUnit,
}

impl Dial {
    pub const fn new(
        id: &'static str,
        label: &'static str,
        min: f32,
        max: f32,
        default: f32,
        unit: DialUnit,
    ) -> Self {
        Self {
            id,
            label,
            min,
            max,
            log: false,
            bipolar: false,
            default,
            unit,
        }
    }

    pub const fn log(self) -> Self {
        Self { log: true, ..self }
    }

    pub const fn bipolar(self) -> Self {
        Self {
            bipolar: true,
            ..self
        }
    }

    pub const fn with_default(self, default: f32) -> Self {
        Self { default, ..self }
    }

    /// The knob's position, 0–1, for `value`.
    pub fn norm(&self, value: f32) -> f32 {
        let v = value.clamp(self.min, self.max);
        let n = if self.log {
            (v / self.min).ln() / (self.max / self.min).ln()
        } else {
            (v - self.min) / (self.max - self.min)
        };
        n.clamp(0.0, 1.0)
    }

    /// The value at knob position `n`, rounded to the readout.
    pub fn at(&self, n: f32) -> f32 {
        let n = n.clamp(0.0, 1.0);
        let raw = if self.log {
            self.min * (self.max / self.min).powf(n)
        } else {
            self.min + n * (self.max - self.min)
        };
        self.unit.round(raw).clamp(self.min, self.max)
    }

    pub fn text(&self, value: f32) -> String {
        self.unit.format(value)
    }
}

// ── Painting ───────────────────────────────────────────────────────────────

/// A filled rectangle in window space.
pub fn rect(x: f32, y: f32, w: f32, h: f32) -> Bounds<Pixels> {
    Bounds::new(
        point(px(x), px(y)),
        gpui::size(px(w.max(0.0)), px(h.max(0.0))),
    )
}

pub fn paint_rect(window: &mut Window, (x, y, w, h): (f32, f32, f32, f32), ink: Rgba) {
    window.paint_quad(fill(rect(x, y, w, h), ink));
}

/// A dot of radius `r` at `(x, y)`.
pub fn paint_dot(window: &mut Window, x: f32, y: f32, r: f32, ink: Rgba) {
    window.paint_quad(fill(rect(x - r, y - r, 2.0 * r, 2.0 * r), ink).corner_radii(px(r)));
}

/// A polyline through `points`, dashed `dash` (on, off) when given.
pub fn paint_polyline(
    window: &mut Window,
    points: &[(f32, f32)],
    width: f32,
    ink: Rgba,
    dash: Option<(f32, f32)>,
) {
    if points.len() < 2 {
        return;
    }
    let mut line = PathBuilder::stroke(px(width));
    if let Some((on, off)) = dash {
        line = line.dash_array(&[px(on), px(off)]);
    }
    line.move_to(point(px(points[0].0), px(points[0].1)));
    for &(x, y) in &points[1..] {
        line.line_to(point(px(x), px(y)));
    }
    if let Ok(path) = line.build() {
        window.paint_path(path, ink);
    }
}

/// The polygon through `points`, filled.
pub fn paint_polygon(window: &mut Window, points: &[(f32, f32)], ink: Rgba) {
    if points.len() < 3 {
        return;
    }
    let mut area = PathBuilder::fill();
    area.move_to(point(px(points[0].0), px(points[0].1)));
    for &(x, y) in &points[1..] {
        area.line_to(point(px(x), px(y)));
    }
    area.close();
    if let Ok(path) = area.build() {
        window.paint_path(path, ink);
    }
}

/// How a painted line of text sits on its `x`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
}

/// One line of text at `(x, y)` — its top — in `font` at `size` window
/// pixels.
#[allow(clippy::too_many_arguments)]
pub fn paint_text(
    window: &mut Window,
    cx: &mut App,
    text: &str,
    font: Font,
    size: f32,
    ink: Rgba,
    (x, y): (f32, f32),
    align: Align,
) {
    if text.is_empty() {
        return;
    }
    let ink: Hsla = ink.into();
    let line = window.text_system().shape_line(
        SharedString::from(text.to_string()),
        px(size),
        &[TextRun {
            len: text.len(),
            font,
            color: ink,
            background_color: None,
            underline: None,
            strikethrough: None,
        }],
        None,
    );
    let width = f32::from(line.width);
    let left = match align {
        Align::Left => x,
        Align::Center => x - width * 0.5,
        Align::Right => x - width,
    };
    let _ = line.paint(
        point(px(left), px(y)),
        px(size * 1.3),
        TextAlign::Left,
        None,
        window,
        cx,
    );
}

/// The subset of SVG path data the designs' icons use — absolute `M L H V
/// C A Z` — traced into `builder`, the view box mapped by `k` from `origin`.
pub fn trace_svg(d: &str, origin: (f32, f32), k: f32, builder: &mut PathBuilder) {
    let at = |x: f32, y: f32| point(px(origin.0 + x * k), px(origin.1 + y * k));
    let mut numbers: Vec<f32> = Vec::new();
    let mut command = ' ';
    let mut current = (0.0f32, 0.0f32);
    let mut start = (0.0f32, 0.0f32);
    let mut run = |command: char, numbers: &mut Vec<f32>, current: &mut (f32, f32)| {
        let n = std::mem::take(numbers);
        match command {
            'M' => {
                for (i, pair) in n.chunks_exact(2).enumerate() {
                    *current = (pair[0], pair[1]);
                    if i == 0 {
                        builder.move_to(at(pair[0], pair[1]));
                        start = *current;
                    } else {
                        builder.line_to(at(pair[0], pair[1]));
                    }
                }
            }
            'L' => {
                for pair in n.chunks_exact(2) {
                    *current = (pair[0], pair[1]);
                    builder.line_to(at(pair[0], pair[1]));
                }
            }
            'H' => {
                for &x in &n {
                    current.0 = x;
                    builder.line_to(at(current.0, current.1));
                }
            }
            'V' => {
                for &y in &n {
                    current.1 = y;
                    builder.line_to(at(current.0, current.1));
                }
            }
            'C' => {
                for c in n.chunks_exact(6) {
                    *current = (c[4], c[5]);
                    builder.cubic_bezier_to(at(c[4], c[5]), at(c[0], c[1]), at(c[2], c[3]));
                }
            }
            'A' => {
                for a in n.chunks_exact(7) {
                    *current = (a[5], a[6]);
                    builder.arc_to(
                        point(px(a[0] * k), px(a[1] * k)),
                        px(a[2]),
                        a[3] != 0.0,
                        a[4] != 0.0,
                        at(a[5], a[6]),
                    );
                }
            }
            'Z' => {
                builder.close();
                *current = start;
            }
            _ => {}
        }
    };
    let mut token = String::new();
    let flush = |token: &mut String, numbers: &mut Vec<f32>| {
        if !token.is_empty() {
            if let Ok(value) = token.parse::<f32>() {
                numbers.push(value);
            }
            token.clear();
        }
    };
    for ch in d.chars() {
        match ch {
            'M' | 'L' | 'H' | 'V' | 'C' | 'A' | 'Z' => {
                flush(&mut token, &mut numbers);
                run(command, &mut numbers, &mut current);
                command = ch;
            }
            '-' => {
                // A sign starts a new number unless it follows an exponent.
                if !token.ends_with('e') {
                    flush(&mut token, &mut numbers);
                }
                token.push(ch);
            }
            '0'..='9' | '.' | 'e' => token.push(ch),
            _ => flush(&mut token, &mut numbers),
        }
    }
    flush(&mut token, &mut numbers);
    run(command, &mut numbers, &mut current);
}

/// Strokes SVG path `d` drawn in a `view` box into `bounds`, fitted and
/// centred, with a `width` (view units) line.
pub fn paint_svg_stroke(
    window: &mut Window,
    bounds: Bounds<Pixels>,
    d: &str,
    view: (f32, f32),
    width: f32,
    ink: Rgba,
) {
    let (bx, by) = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
    let (bw, bh) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
    let k = (bw / view.0).min(bh / view.1);
    let origin = (bx + (bw - view.0 * k) * 0.5, by + (bh - view.1 * k) * 0.5);
    let mut path = PathBuilder::stroke(px(width * k));
    trace_svg(d, origin, k, &mut path);
    if let Ok(path) = path.build() {
        window.paint_path(path, ink);
    }
}

/// An icon element: SVG path `d` in a `view` box, `w` × `h` at the design
/// size.
pub fn icon(
    d: &'static str,
    view: (f32, f32),
    (w, h): (f32, f32),
    width: f32,
    ink: Rgba,
    s: f32,
) -> gpui::Canvas<()> {
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| paint_svg_stroke(window, bounds, d, view, width, ink),
    )
    .w(px(w * s))
    .h(px(h * s))
    .flex_shrink_0()
}

pub const CHEVRON_LEFT: &str = "M7.5 2.5 L4 6 L7.5 9.5";
pub const CHEVRON_RIGHT: &str = "M4.5 2.5 L8 6 L4.5 9.5";
pub const CHEVRON_DOWN: &str = "M2 3.5 L5 6.5 L8 3.5";
pub const UNDO: &str = "M5 3 L2.5 5.5 L5 8 M2.5 5.5 H9 A3 3 0 0 1 9 11.5 H6";
pub const REDO: &str = "M9 3 L11.5 5.5 L9 8 M11.5 5.5 H5 A3 3 0 0 0 5 11.5 H8";
pub const POWER: &str = "M7 1.8 V6.5 M4.2 3.6 A4.6 4.6 0 1 0 9.8 3.6";
pub const CROSS: &str = "M3 3 L9 9 M9 3 L3 9";
pub const PLUS: &str = "M6 2 V10 M2 6 H10";

// ── Knobs ──────────────────────────────────────────────────────────────────

/// A knob's drawing, in its 58-unit view box.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KnobLook {
    /// Its size on the plate.
    pub size: f32,
    pub track_w: f32,
    pub face_r: f32,
    pub face_w: f32,
    pub needle_w: f32,
    /// The face's fill and rim, and where the needle starts from the centre.
    pub face: u32,
    pub face_edge: u32,
    pub needle_from: f32,
}

impl KnobLook {
    /// The small knob every card uses, at `size`.
    pub const fn small(size: f32) -> Self {
        Self {
            size,
            track_w: 4.0,
            face_r: 17.0,
            face_w: 1.0,
            needle_w: 2.0,
            face: FACE,
            face_edge: FACE_EDGE,
            needle_from: 7.0,
        }
    }

    /// The thin-ringed lead knob, at `size`.
    pub const fn hero(size: f32, track_w: f32, needle_w: f32) -> Self {
        Self {
            size,
            track_w,
            face_r: 18.0,
            face_w: 0.7,
            needle_w,
            face: FACE,
            face_edge: FACE_EDGE,
            needle_from: 7.0,
        }
    }

    /// The same drawing with its own track, face and needle.
    pub const fn styled(
        self,
        track_w: f32,
        face_r: f32,
        face: u32,
        face_edge: u32,
        face_w: f32,
        needle_from: f32,
        needle_w: f32,
    ) -> Self {
        Self {
            track_w,
            face_r,
            face,
            face_edge,
            face_w,
            needle_from,
            needle_w,
            ..self
        }
    }
}

/// A knob cell's frame: its width, the gap between label, knob and value,
/// and the two lines' sizes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KnobCell {
    pub width: f32,
    pub gap: f32,
    pub label_size: f32,
    pub label_em: f32,
    pub label_ink: u32,
    pub value_size: f32,
    /// The label under the knob rather than over it.
    pub label_below: bool,
}

impl KnobCell {
    pub const fn new(width: f32, label_size: f32, value_size: f32) -> Self {
        Self {
            width,
            gap: 2.0,
            label_size,
            label_em: 0.06,
            label_ink: SOFT,
            value_size,
            label_below: false,
        }
    }

    pub const fn label_below(self) -> Self {
        Self {
            label_below: true,
            ..self
        }
    }

    pub const fn gap(self, gap: f32) -> Self {
        Self { gap, ..self }
    }

    pub const fn label(self, em: f32, ink: u32) -> Self {
        Self {
            label_em: em,
            label_ink: ink,
            ..self
        }
    }
}

fn angle_point(cx: f32, cy: f32, deg: f32, r: f32) -> (f32, f32) {
    let t = deg * PI / 180.0;
    (cx + r * t.sin(), cy - r * t.cos())
}

/// An arc of radius `r` from `from` to `to` degrees (0 at the top,
/// clockwise), with round ends.
fn paint_arc(
    window: &mut Window,
    (cx, cy): (f32, f32),
    r: f32,
    (from, to): (f32, f32),
    width: f32,
    ink: Rgba,
) {
    let to = if to - from < 0.5 { from + 0.5 } else { to };
    let steps = ((to - from) / 3.0).ceil().max(2.0) as usize;
    let points: Vec<(f32, f32)> = (0..=steps)
        .map(|i| angle_point(cx, cy, from + (to - from) * i as f32 / steps as f32, r))
        .collect();
    paint_polyline(window, &points, width, ink, None);
    for end in [points[0], points[points.len() - 1]] {
        paint_dot(window, end.0, end.1, width * 0.5, ink);
    }
}

/// A knob into `bounds`: the 270° track, the value's arc in `ink` (from the
/// top when `bipolar`), the face and the needle.
pub fn paint_knob(
    window: &mut Window,
    bounds: Bounds<Pixels>,
    n: f32,
    bipolar: bool,
    look: KnobLook,
    ink: Rgba,
) {
    let k = f32::from(bounds.size.width) / 58.0;
    let centre = bounds.center();
    let (cx, cy) = (f32::from(centre.x), f32::from(centre.y));
    let a = -135.0 + 270.0 * n.clamp(0.0, 1.0);
    let a0: f32 = if bipolar { 0.0 } else { -135.0 };
    paint_arc(
        window,
        (cx, cy),
        24.0 * k,
        (-135.0, 135.0),
        look.track_w * k,
        color(TRACK),
    );
    paint_arc(
        window,
        (cx, cy),
        24.0 * k,
        (a0.min(a), a0.max(a)),
        look.track_w * k,
        ink,
    );
    let r = look.face_r * k;
    window.paint_quad(
        fill(rect(cx - r, cy - r, 2.0 * r, 2.0 * r), color(look.face))
            .corner_radii(px(r))
            .border_widths(px(look.face_w * k))
            .border_color(color(look.face_edge)),
    );
    let p1 = angle_point(cx, cy, a, look.needle_from * k);
    let p2 = angle_point(cx, cy, a, 15.0 * k);
    let w = look.needle_w * k;
    paint_polyline(window, &[p1, p2], w, color(WHITE), None);
    paint_dot(window, p1.0, p1.1, w * 0.5, color(WHITE));
    paint_dot(window, p2.0, p2.1, w * 0.5, color(WHITE));
}

/// The vertical travel, at the design size, that sweeps a knob end to end.
const KNOB_DRAG_SPAN: f32 = 180.0;

/// `value` after `dy` pixels of wheel (positive up) on `dial`, through its
/// taper: 600 px turns it end to end, 2000 px when `fine` (Shift). Shared
/// by the knobs and anything else the wheel turns a dial on, so both feel
/// the same.
pub fn wheel_turn(dial: Dial, value: f32, dy: f32, fine: bool) -> f32 {
    let span = if fine { 2_000.0 } else { 600.0 };
    dial.at((dial.norm(value) + dy / span).clamp(0.0, 1.0))
}

/// A knob cell — label, knob, value — for `dial` at `value`. Dragging up
/// turns it up (Shift for fine), the wheel nudges it, a double-click resets
/// it to `dial.default`; `set` takes each new value. Greyed and inert when
/// not `enabled`.
#[allow(clippy::too_many_arguments)]
pub fn knob<T: 'static>(
    cx: &mut Context<T>,
    key: impl Into<SharedString>,
    dial: Dial,
    value: f32,
    look: KnobLook,
    cell: KnobCell,
    ink: Rgba,
    enabled: bool,
    s: f32,
    set: impl Fn(&mut T, f32, &mut Context<T>) + 'static,
) -> AnyElement {
    knob_reading(cx, key, dial, value, None, look, cell, ink, enabled, s, set)
}

/// [`knob`], its value printed as `readout` when given — a reading the
/// dial's own format cannot show.
#[allow(clippy::too_many_arguments)]
pub fn knob_reading<T: 'static>(
    cx: &mut Context<T>,
    key: impl Into<SharedString>,
    dial: Dial,
    value: f32,
    readout: Option<String>,
    look: KnobLook,
    cell: KnobCell,
    ink: Rgba,
    enabled: bool,
    s: f32,
    set: impl Fn(&mut T, f32, &mut Context<T>) + 'static,
) -> AnyElement {
    let key: SharedString = key.into();
    let n = dial.norm(value);
    let face = canvas(
        |_, _, _| (),
        move |bounds, _, window, _| paint_knob(window, bounds, n, dial.bipolar, look, ink),
    )
    .size_full();
    let mut button = div()
        .id(ElementId::Name(key.clone()))
        .size(px(look.size * s))
        .flex_shrink_0()
        .child(face);
    if enabled {
        let set = Rc::new(set);
        let entity = cx.entity().clone();
        let on_drag = entity.clone();
        let on_wheel = entity.clone();
        let drag_set = set.clone();
        let wheel_set = set.clone();
        let drag_id = key.to_string();
        button = button
            .cursor(gpui::CursorStyle::ResizeUpDown)
            .on_drag(FaderDrag::new(drag_id.clone()), |drag, _, _, cx| {
                cx.new(|_| FaderDrag::new(drag.id.clone()))
            })
            .on_drag_move::<FaderDrag>(move |event, _window, app| {
                let drag = event.drag(app);
                if drag.id != drag_id {
                    return;
                }
                let y = f32::from(event.event.position.y);
                let next = drag_to(
                    drag,
                    n,
                    y,
                    event.event.modifiers.shift,
                    KNOB_DRAG_SPAN * s,
                    |dy| -dy,
                );
                let set = drag_set.clone();
                let _ = on_drag.update(app, |this, cx| set(this, dial.at(next), cx));
            })
            .on_scroll_wheel(move |event: &ScrollWheelEvent, _window, app| {
                let dy = f32::from(event.delta.pixel_delta(px(20.0)).y);
                if dy == 0.0 {
                    return;
                }
                let set = wheel_set.clone();
                let _ = on_wheel.update(app, |this, cx| {
                    set(this, wheel_turn(dial, value, dy, event.modifiers.shift), cx)
                });
            })
            .on_click(move |event, _window, app| {
                if is_reset_click(event) {
                    let set = set.clone();
                    let _ = entity.update(app, |this, cx| set(this, dial.default, cx));
                }
            });
    }
    let cell_div = div()
        .flex()
        .flex_col()
        .items_center()
        .flex_shrink_0()
        .w(px(cell.width * s))
        .gap(px(cell.gap * s));
    let label = caption(
        dial.label,
        cell.label_size,
        cell.label_em,
        cell.label_ink,
        s,
    );
    let reading = numbers(
        readout.unwrap_or_else(|| dial.text(value)),
        cell.value_size,
        WHITE,
        s,
    );
    let cell_div = if cell.label_below {
        cell_div.child(button).child(label).child(reading)
    } else {
        cell_div.child(label).child(button).child(reading)
    };
    if enabled {
        cell_div.into_any_element()
    } else {
        cell_div.opacity(0.35).into_any_element()
    }
}

// ── Buttons ────────────────────────────────────────────────────────────────

/// How a pill is sized: height, side padding, type size and face.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PillLook {
    pub h: f32,
    pub pad_x: f32,
    pub size: f32,
    pub mono: bool,
}

impl PillLook {
    pub const fn words(h: f32, pad_x: f32, size: f32) -> Self {
        Self {
            h,
            pad_x,
            size,
            mono: false,
        }
    }

    pub const fn numbers(h: f32, pad_x: f32, size: f32) -> Self {
        Self {
            h,
            pad_x,
            size,
            mono: true,
        }
    }
}

/// The colours a pill takes: lit with the accent's border, or at rest.
pub fn pill_colors(on: bool, accent: Rgba) -> (Rgba, Rgba, Rgba) {
    if on {
        (color(0x1d1d1d), accent, color(WHITE))
    } else {
        (color(0x262626), color(0x484848), color(PALE))
    }
}

/// A choice button: lit when `on`. Greyed and inert when not `enabled`.
#[allow(clippy::too_many_arguments)]
pub fn pill(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    on: bool,
    accent: Rgba,
    look: PillLook,
    enabled: bool,
    s: f32,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let (bg, border, fg) = pill_colors(on, accent);
    let mut base = div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .justify_center()
        .h(px(look.h * s))
        .px(px(look.pad_x * s))
        .rounded(px(3.0 * s))
        .border(px(1.0))
        .border_color(border)
        .bg(bg)
        .text_color(fg)
        .text_size(type_px(look.size, s))
        .child(label.into());
    if look.mono {
        base = base.font(mono(FontWeight::NORMAL));
    }
    if !enabled {
        return base.opacity(0.4);
    }
    let hover = if on { 0x222222 } else { 0x2e2e2e };
    base.cursor(gpui::CursorStyle::PointingHand)
        .hover(move |style| style.bg(color(hover)))
        .on_click(on_click)
}

/// The header's square icon button.
pub fn square(
    id: impl Into<ElementId>,
    size: f32,
    child: impl IntoElement,
    enabled: bool,
    s: f32,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let base = div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .size(px(size * s))
        .rounded(px(3.0 * s))
        .border(px(1.0))
        .border_color(color(WELL_EDGE))
        .bg(color(GROUND))
        .child(child);
    if !enabled {
        return base.opacity(0.35);
    }
    base.cursor(gpui::CursorStyle::PointingHand)
        .hover(|style| style.bg(color(0x262626)))
        .on_click(on_click)
}

/// An on/off switch: a 40 × 22 track with its knob at the end it is set to.
pub fn switch(
    id: impl Into<ElementId>,
    on: bool,
    accent: Rgba,
    enabled: bool,
    s: f32,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let (bg, border, knob, left) = if on {
        (accent, accent, color(WELL), 20.0)
    } else {
        (color(FACE), color(0x555555), color(FAINT), 2.0)
    };
    let base = div()
        .id(id)
        .relative()
        .flex_shrink_0()
        .w(px(40.0 * s))
        .h(px(22.0 * s))
        .rounded(px(11.0 * s))
        .border(px(1.0))
        .border_color(border)
        .bg(bg)
        .child(
            div()
                .absolute()
                .top(px(2.0 * s))
                .left(px(left * s))
                .size(px(16.0 * s))
                .rounded_full()
                .bg(knob),
        );
    if !enabled {
        return base.opacity(0.45);
    }
    base.cursor(gpui::CursorStyle::PointingHand)
        .on_click(on_click)
}

// ── Frame, regions and cards ───────────────────────────────────────────────

/// The plate's scale for a window `viewport`: the design fitted under the
/// shell's chrome.
pub fn plate_scale((width, height): (f32, f32)) -> f32 {
    let height = (height - SHELL_CHROME_H).max(1.0);
    (width / PLATE_W).min(height / PLATE_H).clamp(0.4, 3.0)
}

/// The plate at scale `s`, centred on its ground.
pub fn plate_frame(s: f32, children: Vec<AnyElement>) -> AnyElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .overflow_hidden()
        .bg(color(GROUND))
        .font(outfit(FontWeight::NORMAL))
        .text_color(color(WHITE))
        .child(
            div()
                .relative()
                .flex_shrink_0()
                .w(px(PLATE_W * s))
                .h(px(PLATE_H * s))
                .overflow_hidden()
                .bg(color(GROUND))
                .children(children),
        )
        .into_any_element()
}

/// The display well: an absolute panel at `top`, `height` tall, its header
/// row first.
pub fn well(s: f32, top: f32, height: f32) -> gpui::Div {
    div()
        .absolute()
        .left(px(6.0 * s))
        .top(px(top * s))
        .w(px(1011.0 * s))
        .h(px(height * s))
        .flex()
        .flex_col()
        .bg(color(WELL))
        .border(px(1.0))
        .border_color(color(WELL_EDGE))
}

/// The control deck under the well.
pub fn deck(s: f32, top: f32, height: f32) -> gpui::Div {
    div()
        .absolute()
        .left(px(6.0 * s))
        .top(px(top * s))
        .w(px(1011.0 * s))
        .h(px(height * s))
        .flex()
        .bg(color(DECK))
}

/// A card on the deck: its title first, its rows spread down it.
pub fn card(s: f32) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .justify_between()
        .min_w(px(0.0))
        .px(px(16.0 * s))
        .py(px(14.0 * s))
        .rounded(px(4.0 * s))
        .border(px(1.0))
        .border_color(color(CARD_EDGE))
        .bg(color(CARD))
}

/// A row of `n` equal columns, `gap` apart.
pub fn columns(gap: f32, s: f32, cells: Vec<AnyElement>) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .w_full()
        .gap(px(gap * s))
        .children(cells.into_iter().map(|cell| {
            div()
                .flex()
                .flex_col()
                .flex_basis(px(0.0))
                .flex_grow_1()
                .min_w(px(0.0))
                .child(cell)
        }))
}

/// A canvas that records its bounds as plate display `slot` for the live
/// overlay, and paints nothing itself.
pub fn live_slot(bounds: &LiveBounds, slot: usize) -> gpui::Canvas<()> {
    bounds.slot(LiveSlot::Plate(slot))
}

/// `element` painted after the live overlay: a label or legend that sits
/// over a live display.
pub fn over_live(element: impl IntoElement) -> AnyElement {
    deferred(element).into_any_element()
}

/// A legend swatch and its label.
pub fn legend(swatch: gpui::Div, label: &str, s: f32) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(5.0 * s))
        .child(swatch)
        .child(words(label.to_string(), 11.0, SOFT, s))
}

// ── Header ─────────────────────────────────────────────────────────────────

/// The well's header row: the name and what it is, the preset bar, A/B,
/// undo and redo, then `right` and the bypass switch.
pub fn header<M: KitModel>(
    e: &KitEditor<M>,
    cx: &mut Cx<M>,
    s: f32,
    subtitle: &str,
    accent: Rgba,
    right: Vec<AnyElement>,
) -> AnyElement {
    let key = e.model.key();
    let name = div()
        .flex()
        .flex_row()
        .items_baseline()
        .gap(px(10.0 * s))
        .flex_shrink_0()
        .child(
            words(e.model.title(), 22.0, WHITE, s)
                .font_weight(FontWeight::SEMIBOLD)
                .line_height(px(22.0 * s)),
        )
        .child(caption(subtitle, 10.5, 0.16, FAINT, s));

    let presets = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(4.0 * s))
        .child(square(
            (key, 900usize),
            28.0,
            icon(
                CHEVRON_LEFT,
                (12.0, 12.0),
                (12.0, 12.0),
                1.5,
                color(ICON),
                s,
            ),
            true,
            s,
            e.click_cb(cx, |this, cx| this.step_preset(-1, cx)),
        ))
        .child(preset_picker(e, cx, s, accent))
        .child(square(
            (key, 901usize),
            28.0,
            icon(
                CHEVRON_RIGHT,
                (12.0, 12.0),
                (12.0, 12.0),
                1.5,
                color(ICON),
                s,
            ),
            true,
            s,
            e.click_cb(cx, |this, cx| this.step_preset(1, cx)),
        ));

    let on_b = e.compare.on_b;
    let letter = |label: &'static str, lit: bool, index: usize, cx: &mut Cx<M>| {
        let (bg, border, fg) = if lit {
            (color(0x2a2a2a), accent, color(WHITE))
        } else {
            (color(GROUND), color(WELL_EDGE), color(SOFT))
        };
        div()
            .id((key, 902 + index))
            .flex()
            .items_center()
            .justify_center()
            .w(px(30.0 * s))
            .h(px(28.0 * s))
            .rounded(px(3.0 * s))
            .border(px(1.0))
            .border_color(border)
            .bg(bg)
            .text_color(fg)
            .text_size(type_px(12.0, s))
            .font_weight(FontWeight::SEMIBOLD)
            .cursor(gpui::CursorStyle::PointingHand)
            .child(label)
            .on_click(e.click_cb(cx, move |this, cx| {
                if (index == 1) != this.compare.on_b {
                    this.swap_compare(cx);
                }
            }))
    };
    let compare = div()
        .flex()
        .flex_row()
        .gap(px(2.0 * s))
        .child(letter("A", !on_b, 0, cx))
        .child(letter("B", on_b, 1, cx));

    let history = div()
        .flex()
        .flex_row()
        .gap(px(2.0 * s))
        .child(square(
            (key, 904usize),
            28.0,
            icon(UNDO, (14.0, 14.0), (14.0, 14.0), 1.4, color(ICON), s),
            e.can_undo(),
            s,
            e.click_cb(cx, |this, cx| this.undo(cx)),
        ))
        .child(square(
            (key, 905usize),
            28.0,
            icon(REDO, (14.0, 14.0), (14.0, 14.0), 1.4, color(ICON), s),
            e.can_redo(),
            s,
            e.click_cb(cx, |this, cx| this.redo(cx)),
        ));

    let power_ink = if e.power() { accent } else { color(OFF) };
    let bypass = square(
        (key, 906usize),
        28.0,
        icon(POWER, (14.0, 14.0), (14.0, 14.0), 1.6, power_ink, s),
        true,
        s,
        e.click_cb(cx, |this, cx| this.toggle("power", cx)),
    );

    let notice = e
        .notice()
        .map(|text| words(text.to_string(), 11.0, SOFT, s).into_any_element());

    div()
        .flex()
        .flex_row()
        .items_center()
        .flex_shrink_0()
        .h(px(44.0 * s))
        .pl(px(13.0 * s))
        .pr(px(12.0 * s))
        .gap(px(10.0 * s))
        .child(name)
        .child(div().flex_1())
        .child(presets)
        .child(compare)
        .child(history)
        .child(div().flex_1())
        .children(notice)
        .children(right)
        .child(bypass)
        .into_any_element()
}

/// The preset chip, and its list under it while open.
fn preset_picker<M: KitModel>(
    e: &KitEditor<M>,
    cx: &mut Cx<M>,
    s: f32,
    accent: Rgba,
) -> AnyElement {
    let key = e.model.key();
    let open = e.preset_open;
    let chip = div()
        .id((key, 910usize))
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .w(px(168.0 * s))
        .h(px(28.0 * s))
        .px(px(10.0 * s))
        .rounded(px(3.0 * s))
        .border(px(1.0))
        .border_color(if open { accent } else { color(WELL_EDGE) })
        .bg(color(GROUND))
        .text_size(type_px(12.5, s))
        .text_color(color(INK))
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(|style| style.bg(color(0x262626)))
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .truncate()
                .child(e.preset_label()),
        )
        .child(icon(
            CHEVRON_DOWN,
            (10.0, 10.0),
            (10.0, 10.0),
            1.4,
            color(FAINT),
            s,
        ))
        .on_click(e.click_cb(cx, |this, cx| this.toggle_preset_menu(cx)));

    let mut picker = div().relative().flex_shrink_0().child(chip);
    if open {
        let bank = e.model.presets();
        let current = e.preset;
        let entity = cx.entity().clone();
        let close = entity.clone();
        let items: Vec<AnyElement> = bank
            .iter()
            .enumerate()
            .map(|(index, preset)| {
                let entity = entity.clone();
                let selected = current == Some(index);
                let item = div()
                    .id((key, 1_000 + index))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0 * s))
                    .w_full()
                    .h(px(28.0 * s))
                    .px(px(10.0 * s))
                    .rounded(px(3.0 * s))
                    .text_size(type_px(12.5, s))
                    .cursor(gpui::CursorStyle::PointingHand)
                    .child(div().size(px(6.0 * s)).rounded_full().bg(if selected {
                        accent
                    } else {
                        color_a(0, 0.0)
                    }))
                    .child(preset.name)
                    .on_click(move |_, _, app| {
                        let _ = entity.update(app, |this, cx| this.load_preset(index as isize, cx));
                    });
                if selected {
                    item.text_color(color(WHITE)).bg(color(0x262626))
                } else {
                    item.text_color(color(PALE))
                        .hover(|style| style.text_color(color(WHITE)).bg(color(0x262626)))
                }
                .into_any_element()
            })
            .collect();
        let menu = div()
            .id((key, 911usize))
            .occlude()
            .flex()
            .flex_col()
            .w(px(220.0 * s))
            .max_h(px(380.0 * s))
            .overflow_y_scroll()
            .p(px(4.0 * s))
            .gap(px(1.0 * s))
            .rounded(px(4.0 * s))
            .border(px(1.0))
            .border_color(color(CARD_EDGE))
            .bg(color(GROUND))
            .shadow(vec![gpui::BoxShadow {
                color: color_a(0, 0.55).into(),
                offset: point(px(0.0), px(14.0 * s)),
                blur_radius: px(36.0 * s),
                spread_radius: px(0.0),
                inset: false,
            }])
            .font(outfit(FontWeight::NORMAL))
            .children(items)
            .on_mouse_down_out(move |_, _, app| {
                let _ = close.update(app, |this, cx| this.close_preset_menu(cx));
            });
        picker = picker.child(
            div().absolute().left_0().bottom_0().child(
                deferred(
                    anchored()
                        .anchor(Anchor::TopLeft)
                        .offset(point(px(0.0), px(4.0 * s)))
                        .snap_to_window_with_margin(px(8.0))
                        .child(menu),
                )
                // Over the live displays and a floating band panel.
                .with_priority(2),
            ),
        );
    }
    picker.into_any_element()
}

/// A point on a plate display, relative to its recorded bounds and scaled.
pub fn at(bounds: Bounds<Pixels>, s: f32) -> impl Fn(f32, f32) -> (f32, f32) {
    let (x0, y0) = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
    move |x, y| (x0 + x * s, y0 + y * s)
}

/// `point` as plain window coordinates.
pub fn xy(p: Point<Pixels>) -> (f32, f32) {
    (f32::from(p.x), f32::from(p.y))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_print_the_way_the_designs_do() {
        assert_eq!(fmt_db(-3.0), "\u{2212}3.0 dB");
        assert_eq!(fmt_db(0.02), "0.0 dB");
        assert_eq!(fmt_db(6.0), "+6.0 dB");
        assert_eq!(fmt_ms(2.0), "2.0 ms");
        assert_eq!(fmt_ms(120.0), "120 ms");
        assert_eq!(fmt_hz(31.5), "31.5 Hz");
        assert_eq!(fmt_hz(240.0), "240 Hz");
        assert_eq!(fmt_hz(2_600.0), "2.60 kHz");
        assert_eq!(fmt_hz(11_000.0), "11.0 kHz");
        assert_eq!(fmt_hz_round(20.0), "20 Hz");
        assert_eq!(DialUnit::Bias.format(0.25), "+25%");
        assert_eq!(DialUnit::Bias.format(0.0), "0%");
        assert_eq!(DialUnit::Ratio.format(2.5), "2.5:1");
    }

    #[test]
    fn a_log_dial_round_trips_its_position() {
        let dial = Dial::new("f", "FREQ", 20.0, 20_000.0, 1_000.0, DialUnit::Hz).log();
        assert!((dial.norm(20.0)).abs() < 1e-6);
        assert!((dial.norm(20_000.0) - 1.0).abs() < 1e-6);
        assert!((dial.at(dial.norm(632.0)) - 632.0).abs() <= 1.0);
        assert_eq!(dial.at(2.0), 20_000.0);
    }

    #[test]
    fn the_wheel_turns_a_log_dial_up_and_down_and_stops_at_its_ends() {
        let q = Dial::new("q", "Q", 0.1, 18.0, 0.71, DialUnit::Q).log();
        let up = wheel_turn(q, 1.0, 60.0, false);
        let down = wheel_turn(q, 1.0, -60.0, false);
        assert!(up > 1.0 && down < 1.0, "up {up}, down {down}");
        // Fine moves less for the same travel.
        assert!(wheel_turn(q, 1.0, 60.0, true) < up);
        assert_eq!(wheel_turn(q, 18.0, 600.0, false), 18.0);
        assert_eq!(wheel_turn(q, 0.1, -600.0, false), 0.1);
    }

    #[test]
    fn captions_are_tracked_upper_case() {
        assert_eq!(tracked("Mix", 0.1), "M\u{200A}I\u{200A}X");
        assert_eq!(
            tracked("in out", 0.1),
            "I\u{200A}N\u{200A} \u{200A}O\u{200A}U\u{200A}T"
        );
        assert_eq!(tracked("Mix", 0.16), "M\u{200A}\u{200A}I\u{200A}\u{200A}X");
        assert_eq!(tracked("Mix", 0.06), "MIX");
    }
}

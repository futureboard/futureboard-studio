//! What the instruments' plate editors share — WrapSynth, Drum Sampler,
//! Quick Sampler and Slicer.
//!
//! The instruments keep their own windows (sample loading, drops, pads,
//! slices and the preview keyboard live there), so they are not
//! [`KitModel`](crate::components::plugin_kit::KitModel)s; their panels hand
//! every edit to a callback. These are the plate kit's pieces in that shape:
//! a header without the preset kit's A/B and undo, page tabs, steppers and
//! choosers, the keyboard strip along the plate's foot, and the colours a
//! sample display paints in.
//!
//! An instrument's plate is the standard [`PLATE_W`] × [`PLATE_H`] artboard:
//! the well from [`WELL_TOP`], the deck under it down to [`DECK_BOTTOM`], and
//! the keyboard strip from [`KEYBOARD_TOP`] to the foot.

use std::rc::Rc;

use gpui::{
    div, prelude::FluentBuilder, px, relative, AnyElement, App, ClickEvent, ElementId, FontWeight,
    InteractiveElement, IntoElement, ParentElement, Rgba, SharedString, Styled, Window,
};

use crate::components::plate::{
    self, caption, color, color_a, icon, numbers, square, type_px, words, CHEVRON_LEFT,
    CHEVRON_RIGHT, FAINT, GROUND, ICON, OFF, PALE, POWER, SOFT, WELL_EDGE, WHITE,
};
use crate::components::quick_sampler_panel::DisplayColors;
use crate::components::soundfont_player_mdi::note_label;

pub use crate::components::plate::{PLATE_H, PLATE_W, WINDOW_SIZE};

/// Where an instrument plate's regions sit, in design units.
pub const WELL_TOP: f32 = 7.0;
/// The header row at the top of the well.
pub const HEADER_H: f32 = 44.0;
/// The deck ends here; the keyboard strip starts at [`KEYBOARD_TOP`].
pub const DECK_BOTTOM: f32 = 664.0;
pub const KEYBOARD_TOP: f32 = 668.0;
pub const KEYBOARD_H: f32 = PLATE_H - KEYBOARD_TOP - 6.0;

/// The octaves the keyboard strip shows.
pub const KEYBOARD_OCTAVES: usize = 4;
const WHITE_KEYS: usize = KEYBOARD_OCTAVES * 7;
const WHITE_SEMITONES: [u8; 7] = [0, 2, 4, 5, 7, 9, 11];
/// The black keys, by the white key each sits after.
const BLACK_SEMITONES: [(usize, u8); 5] = [(0, 1), (1, 3), (3, 6), (4, 8), (5, 10)];
const BLACK_DEPTH: f32 = 0.6;
const BLACK_WIDTH: f32 = 0.62;

/// A click handler that can be shared between elements.
pub type Click = Rc<dyn Fn(&mut Window, &mut App)>;
/// A handler for a step (`-1` / `+1`) or an index.
pub type Step = Rc<dyn Fn(i32, &mut Window, &mut App)>;
/// A key's handler: its pitch.
pub type Note = Rc<dyn Fn(u8, &mut Window, &mut App)>;

fn on_click(click: &Click) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
    let click = click.clone();
    move |_, window, cx| click(window, cx)
}

// ── Header ─────────────────────────────────────────────────────────────────

/// The well's header row: the instrument's name and what it is, `middle`
/// (page tabs, a preset bar) centred, then `right` and the power button.
#[allow(clippy::too_many_arguments)]
pub fn header(
    id: &'static str,
    s: f32,
    title: &str,
    subtitle: &str,
    accent: Rgba,
    middle: Vec<AnyElement>,
    right: Vec<AnyElement>,
    power: Option<(bool, Click)>,
) -> AnyElement {
    let name = div()
        .flex()
        .flex_row()
        .items_baseline()
        .gap(px(10.0 * s))
        .flex_shrink_0()
        .child(
            words(title.to_string(), 22.0, WHITE, s)
                .font_weight(FontWeight::SEMIBOLD)
                .line_height(px(22.0 * s)),
        )
        .child(caption(subtitle, 10.5, 0.16, FAINT, s));
    let power = power.map(|(on, click)| {
        let ink = if on { accent } else { color(OFF) };
        square(
            (id, 0usize),
            28.0,
            icon(POWER, (14.0, 14.0), (14.0, 14.0), 1.6, ink, s),
            true,
            s,
            on_click(&click),
        )
    });
    div()
        .flex()
        .flex_row()
        .items_center()
        .flex_shrink_0()
        .h(px(HEADER_H * s))
        .pl(px(13.0 * s))
        .pr(px(12.0 * s))
        .gap(px(10.0 * s))
        .child(name)
        .child(div().flex_1())
        .children(middle)
        .child(div().flex_1())
        .children(right)
        .children(power)
        .into_any_element()
}

/// A row of page tabs, the `selected` one lit; `on_select` takes the index.
pub fn tabs(
    id: &'static str,
    labels: &[&'static str],
    selected: usize,
    accent: Rgba,
    s: f32,
    on_select: Step,
) -> AnyElement {
    div()
        .flex()
        .flex_row()
        .gap(px(2.0 * s))
        .children(labels.iter().enumerate().map(|(index, label)| {
            let on_select = on_select.clone();
            plate::pill(
                (id, index),
                *label,
                index == selected,
                accent,
                plate::PillLook::words(28.0, 14.0, 12.5),
                true,
                s,
                move |_, window, cx| on_select(index as i32, window, cx),
            )
            .into_any_element()
        }))
        .into_any_element()
}

/// A row of choice pills, `selected` lit, each `width` design units wide
/// (`None`: as wide as its label). `on_select` takes the index.
#[allow(clippy::too_many_arguments)]
pub fn choices(
    id: &'static str,
    labels: &[&str],
    selected: Option<usize>,
    accent: Rgba,
    width: Option<f32>,
    enabled: bool,
    s: f32,
    on_select: Step,
) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .gap(px(4.0 * s))
        .children(labels.iter().enumerate().map(|(index, label)| {
            let on_select = on_select.clone();
            let pill = plate::pill(
                (id, index),
                label.to_string(),
                selected == Some(index),
                accent,
                plate::PillLook::words(26.0, 8.0, 11.5),
                enabled,
                s,
                move |_, window, cx| on_select(index as i32, window, cx),
            );
            match width {
                Some(width) => pill.w(px(width * s)).into_any_element(),
                None => pill.into_any_element(),
            }
        }))
}

/// A pill that is lit while `on`: a toggle.
pub fn toggle(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    on: bool,
    accent: Rgba,
    enabled: bool,
    s: f32,
    click: Click,
) -> gpui::Stateful<gpui::Div> {
    plate::pill(
        id,
        label,
        on,
        accent,
        plate::PillLook::words(26.0, 10.0, 11.5),
        enabled,
        s,
        on_click(&click),
    )
}

// ── Steppers and choosers ──────────────────────────────────────────────────

fn step_button(
    id: impl Into<ElementId>,
    path: &'static str,
    enabled: bool,
    s: f32,
    step: i32,
    on_step: &Step,
) -> gpui::Stateful<gpui::Div> {
    let on_step = on_step.clone();
    square(
        id,
        24.0,
        icon(path, (12.0, 12.0), (12.0, 12.0), 1.5, color(ICON), s),
        enabled,
        s,
        move |_, window, cx| on_step(step, window, cx),
    )
}

/// `−  value  +` with its label under it: a whole-number param (a root key,
/// a transpose, a voice count).
pub fn stepper(
    id: &'static str,
    label: &str,
    value: impl Into<SharedString>,
    enabled: bool,
    s: f32,
    on_step: Step,
) -> AnyElement {
    const MINUS: &str = "M2 6 H10";
    let row = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0 * s))
        .child(step_button((id, 0usize), MINUS, enabled, s, -1, &on_step))
        .child(
            div()
                .min_w(px(40.0 * s))
                .flex()
                .justify_center()
                .child(numbers(value, 14.0, WHITE, s)),
        )
        .child(step_button(
            (id, 1usize),
            plate::PLUS,
            enabled,
            s,
            1,
            &on_step,
        ));
    labelled(row, label, s, enabled)
}

/// `‹  value  ›` with its label under it: a choice from a long list (a
/// wavetable, a modulation source, a rate).
pub fn chooser(
    id: &'static str,
    label: &str,
    value: impl Into<SharedString>,
    width: f32,
    enabled: bool,
    s: f32,
    on_step: Step,
) -> AnyElement {
    let row = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(4.0 * s))
        .child(step_button(
            (id, 0usize),
            CHEVRON_LEFT,
            enabled,
            s,
            -1,
            &on_step,
        ))
        .child(
            div()
                .w(px(width * s))
                .h(px(24.0 * s))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(3.0 * s))
                .border(px(1.0))
                .border_color(color(WELL_EDGE))
                .bg(color(GROUND))
                .text_size(type_px(12.0, s))
                .text_color(color(WHITE))
                .child(div().truncate().child(value.into())),
        )
        .child(step_button(
            (id, 1usize),
            CHEVRON_RIGHT,
            enabled,
            s,
            1,
            &on_step,
        ));
    labelled(row, label, s, enabled)
}

fn labelled(row: gpui::Div, label: &str, s: f32, enabled: bool) -> AnyElement {
    let cell = div()
        .flex()
        .flex_col()
        .items_center()
        .gap(px(5.0 * s))
        .flex_shrink_0()
        .child(row)
        .child(caption(label, 10.0, 0.06, SOFT, s));
    if enabled {
        cell.into_any_element()
    } else {
        cell.opacity(0.4).into_any_element()
    }
}

// ── Keyboard strip ─────────────────────────────────────────────────────────

/// What the keyboard strip shows and does.
pub struct Keyboard<'a> {
    pub id: &'static str,
    /// The lowest C shown; four octaves from it.
    pub root: u8,
    pub active: &'a [u8],
    pub playable: bool,
    /// A key's label and foot bar — the root, a slice, a pad.
    pub mark: &'a dyn Fn(u8) -> Option<String>,
    /// Whether a key plays something (a pad, a slice); keys that don't are
    /// shaded. Every key does when `None`.
    pub mapped: Option<&'a dyn Fn(u8) -> bool>,
    /// The line beside the caption: what the keys do.
    pub hint: String,
    pub accent: Rgba,
    pub note_on: Note,
    pub note_off: Note,
    /// `-1` / `+1` octave.
    pub on_shift: Step,
}

/// The highest C the strip can start on.
pub const KEYBOARD_HIGHEST_ROOT: u8 = 127 - (KEYBOARD_OCTAVES as u8 * 12);

/// The keyboard strip along the plate's foot: its caption, what it plays,
/// the octave buttons, and four octaves of keys. Absolute, at
/// [`KEYBOARD_TOP`].
pub fn keyboard(k: Keyboard<'_>, s: f32) -> AnyElement {
    let highest = k
        .root
        .saturating_add((KEYBOARD_OCTAVES * 12) as u8 - 1)
        .min(127);
    let status = if k.active.is_empty() {
        format!("{} \u{2013} {}", note_label(k.root), note_label(highest))
    } else {
        let names: Vec<String> = k.active.iter().copied().map(note_label).collect();
        format!("Playing {}", names.join(" "))
    };
    let status_ink = if k.active.is_empty() { FAINT } else { WHITE };
    let top_row = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(10.0 * s))
        .h(px(24.0 * s))
        .child(caption("Keyboard", 10.5, 0.16, SOFT, s))
        .child(div().flex_1().min_w(px(0.0)).truncate().child(words(
            k.hint.clone(),
            11.0,
            FAINT,
            s,
        )))
        .child(numbers(status, 11.0, status_ink, s))
        .child(step_button(
            (k.id, 900usize),
            "M2 6 H10",
            k.root >= 12,
            s,
            -1,
            &k.on_shift,
        ))
        .child(step_button(
            (k.id, 901usize),
            plate::PLUS,
            k.root < KEYBOARD_HIGHEST_ROOT,
            s,
            1,
            &k.on_shift,
        ));

    let key_h = KEYBOARD_H - 30.0;
    let key_for = |index: usize, pitch: u8, black: bool| {
        key(
            (k.id, index * 2 + black as usize),
            pitch,
            black,
            k.active.contains(&pitch),
            (k.mark)(pitch),
            k.mapped.map_or(true, |mapped| mapped(pitch)),
            k.playable,
            k.accent,
            s,
            &k.note_on,
            &k.note_off,
        )
    };
    let mut white_row = div().flex().flex_row().size_full();
    for index in 0..WHITE_KEYS {
        let pitch = k.root as u16 + ((index / 7) * 12) as u16 + WHITE_SEMITONES[index % 7] as u16;
        if pitch > 127 {
            break;
        }
        white_row = white_row.child(key_for(index, pitch as u8, false).flex_1().h_full());
    }
    let mut board = div()
        .relative()
        .w_full()
        .h(px(key_h * s))
        .rounded(px(3.0 * s))
        .overflow_hidden()
        .border(px(1.0))
        .border_color(color(WELL_EDGE))
        .bg(color(0x0d0d0d))
        .child(white_row);
    let white = 1.0 / WHITE_KEYS as f32;
    for index in 0..WHITE_KEYS {
        let Some((_, semitone)) = BLACK_SEMITONES
            .iter()
            .find(|(after, _)| *after == index % 7)
        else {
            continue;
        };
        let pitch = k.root as u16 + ((index / 7) * 12) as u16 + *semitone as u16;
        if pitch > 127 || index + 1 >= WHITE_KEYS {
            continue;
        }
        let left = white * (index as f32 + 1.0 - BLACK_WIDTH / 2.0);
        board = board.child(
            key_for(index, pitch as u8, true)
                .absolute()
                .top(px(0.0))
                .left(relative(left))
                .w(relative(white * BLACK_WIDTH))
                .h(px(key_h * BLACK_DEPTH * s)),
        );
    }

    div()
        .absolute()
        .left(px(6.0 * s))
        .top(px(KEYBOARD_TOP * s))
        .w(px(1011.0 * s))
        .h(px(KEYBOARD_H * s))
        .flex()
        .flex_col()
        .gap(px(6.0 * s))
        .child(top_row)
        .child(board)
        .into_any_element()
}

#[allow(clippy::too_many_arguments)]
fn key(
    id: impl Into<ElementId>,
    pitch: u8,
    black: bool,
    active: bool,
    mark: Option<String>,
    mapped: bool,
    playable: bool,
    accent: Rgba,
    s: f32,
    note_on: &Note,
    note_off: &Note,
) -> gpui::Stateful<gpui::Div> {
    let marked = mark.is_some();
    let rest = match (black, mapped) {
        (true, true) => color(0x1b1b1b),
        (true, false) => color(0x141414),
        (false, true) => color(0xd8d8d8),
        (false, false) => color(0x8a8a8a),
    };
    let fill = if active { accent } else { rest };
    let label = match mark {
        Some(label) => label,
        None if !black && pitch % 12 == 0 => note_label(pitch),
        None => String::new(),
    };
    let label_ink = if active {
        color(plate::WELL)
    } else if black {
        color(PALE)
    } else {
        color(0x3a3a3a)
    };
    let mut key = div()
        .id(id)
        .flex()
        .flex_col()
        .items_center()
        .justify_end()
        .pb(px(3.0 * s))
        .border_r(px(1.0))
        .border_color(color(0x5a5a5a))
        .bg(fill)
        .text_size(type_px(8.5, s))
        .font(plate::mono(FontWeight::MEDIUM))
        .text_color(label_ink)
        // A marked key carries a bar along its foot: colour and shape, never
        // colour alone.
        .when(marked && !active, |key| {
            key.child(
                div()
                    .w(relative(0.7))
                    .h(px(3.0 * s))
                    .mb(px(2.0 * s))
                    .rounded(px(1.5 * s))
                    .bg(accent),
            )
        })
        .child(label);
    if black {
        key = key
            .rounded_b(px(2.0 * s))
            .border(px(1.0))
            .border_color(color(0x050505));
    }
    if playable {
        let hover = if black {
            color(0x2a2a2a)
        } else {
            color(0xeeeeee)
        };
        let on = note_on.clone();
        let off = note_off.clone();
        key = key
            .cursor(gpui::CursorStyle::PointingHand)
            .when(!active, |key| key.hover(move |style| style.bg(hover)))
            .on_mouse_down(gpui::MouseButton::Left, move |_, window, cx| {
                on(pitch, window, cx)
            })
            .on_mouse_up(gpui::MouseButton::Left, move |_, window, cx| {
                off(pitch, window, cx)
            });
    }
    key
}

// ── Displays ───────────────────────────────────────────────────────────────

/// The colours a sample display paints in on a plate: the waveform in the
/// instrument's `accent`, the markers in white.
pub(crate) fn display_colors(accent: Rgba) -> DisplayColors {
    let tint = |alpha: f32| Rgba { a: alpha, ..accent };
    DisplayColors {
        background: color(crate::components::time_plate::BOX_GROUND),
        grid: color(crate::components::time_plate::GRID),
        ruler: color(0x161616),
        wave_edge: accent,
        wave_peak: tint(0.62),
        wave_core: tint(0.16),
        skipped: color_a(0x0d0d0d, 0.74),
        loop_band: tint(0.09),
        marker: color(WHITE),
        loop_marker: accent,
    }
}

/// A grid of `columns` × `rows` cells across `bounds`, in the displays'
/// grid colour.
pub fn paint_grid(
    window: &mut Window,
    bounds: gpui::Bounds<gpui::Pixels>,
    columns: usize,
    rows: usize,
) {
    let (x0, y0) = plate::xy(bounds.origin);
    let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
    let ink = color(crate::components::time_plate::GRID);
    for i in 1..columns {
        plate::paint_rect(
            window,
            (x0 + w * i as f32 / columns as f32, y0, 1.0, h),
            ink,
        );
    }
    for i in 1..rows {
        plate::paint_rect(window, (x0, y0 + h * i as f32 / rows as f32, w, 1.0), ink);
    }
}

/// A curve over an area shaded down to `floor` (window y): an envelope, a
/// filter response, a wave.
pub fn paint_curve(window: &mut Window, points: &[(f32, f32)], floor: f32, accent: Rgba, s: f32) {
    if points.len() < 2 {
        return;
    }
    let mut area = points.to_vec();
    area.push((points[points.len() - 1].0, floor));
    area.push((points[0].0, floor));
    plate::paint_polygon(window, &area, Rgba { a: 0.16, ..accent });
    plate::paint_polyline(window, points, 1.6 * s, accent, None);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_strip_fits_under_the_deck_and_on_the_plate() {
        assert!(DECK_BOTTOM < KEYBOARD_TOP);
        assert!(KEYBOARD_TOP + KEYBOARD_H <= PLATE_H);
        assert_eq!(KEYBOARD_HIGHEST_ROOT, 79);
    }
}

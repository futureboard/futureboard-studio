//! What VerbSpace's and EchoSpace's plate editors share: dials read off the
//! DSP crates' own descriptors, the time and frequency ladders their
//! displays are drawn on, the display boxes, and the In / Out meter rows.
//!
//! The displays are painted from the params when the panel renders — on an
//! edit, a rate or a tempo change — never per frame; only the meters ride the
//! live overlay.

use gpui::{
    canvas, div, px, AnyElement, App, Bounds, FontWeight, IntoElement, ParentElement, Pixels,
    Styled, Window,
};

use crate::components::plate::{
    self, caption, color, live_slot, numbers, paint_rect, paint_text, Align, Dial, DialUnit, SOFT,
};
use crate::components::plugin_kit::LiveBounds;

/// The display boxes' height, in design units, and the plot inside it: the
/// caption row above, the axis labels below.
pub const BOX_H: f32 = 250.0;
pub const PLOT_TOP: f32 = 38.0;
pub const PLOT_BOTTOM: f32 = 224.0;
/// The two boxes across the well: the wide one and the narrow one.
pub const WIDE_W: f32 = 600.0;
pub const NARROW_W: f32 = 364.0;

/// A grid line, a box's ground and its rim.
pub const GRID: u32 = 0x1d1d1d;
pub const BOX_GROUND: u32 = 0x0d0d0d;
pub const BOX_EDGE: u32 = 0x242424;
pub const AXIS_INK: u32 = 0x6f6f6f;

/// A dial over a descriptor row — `(id, min, max, default)`, the DSP's own
/// range and reset value — labelled `label` and read out in `unit`.
pub fn range_dial(
    (id, min, max, default): (&'static str, f32, f32, f32),
    label: &'static str,
    unit: DialUnit,
) -> Dial {
    Dial::new(id, label, min, max, default, unit)
}

/// 1, 2 or 5 times a power of ten, giving about `ticks` steps across `span`.
pub fn nice_step(span: f32, ticks: usize) -> f32 {
    let raw = span / ticks.max(1) as f32;
    let magnitude = 10f32.powf(raw.log10().floor());
    let scaled = raw / magnitude;
    let nice = if scaled < 1.5 {
        1.0
    } else if scaled < 3.5 {
        2.0
    } else if scaled < 7.5 {
        5.0
    } else {
        10.0
    };
    nice * magnitude
}

/// `value` rounded up to the next tidy figure on a 1-2-5 ladder.
pub fn nice_ceiling(value: f32) -> f32 {
    let magnitude = 10f32.powf(value.max(1.0e-3).log10().floor());
    [1.0, 1.5, 2.0, 2.5, 3.0, 4.0, 5.0, 6.0, 8.0, 10.0]
        .iter()
        .map(|m| m * magnitude)
        .find(|v| *v >= value)
        .unwrap_or(value)
}

/// `375 ms`, `1.25 s`, `2 s`: a time axis label or readout.
pub fn fmt_time(ms: f32) -> String {
    if ms >= 1_000.0 {
        let s = ms / 1_000.0;
        if (s - s.round()).abs() < 1.0e-3 {
            format!("{s:.0} s")
        } else if s >= 10.0 {
            format!("{s:.1} s")
        } else {
            format!("{s:.2} s")
        }
    } else if ms < 10.0 {
        format!("{ms:.1} ms")
    } else {
        format!("{ms:.0} ms")
    }
}

/// `2.40 s`, `12.5 s`, `∞`: a decay time.
pub fn fmt_seconds(value: f32) -> String {
    if !value.is_finite() {
        "\u{221E}".to_string()
    } else if value < 10.0 {
        format!("{value:.2} s")
    } else {
        format!("{value:.1} s")
    }
}

/// A box's x for `hz`, 20 Hz to 20 kHz across `width`.
pub fn x_of_hz(hz: f32, width: f32) -> f32 {
    width * (hz.max(1.0) / 20.0).log10() / 3.0
}

/// The frequency at `x` across `width`.
pub fn hz_at_x(x: f32, width: f32) -> f32 {
    20.0 * 1_000f32.powf(x / width)
}

/// The frequencies a box labels along its foot.
pub const FREQ_TICKS: [(f32, &str); 4] = [
    (100.0, "100"),
    (1_000.0, "1k"),
    (5_000.0, "5k"),
    (10_000.0, "10k"),
];

/// The vertical lines of the shared frequency grid.
pub const FREQ_GRID: [f32; 8] = [
    50.0, 100.0, 200.0, 500.0, 1_000.0, 2_000.0, 5_000.0, 10_000.0,
];

/// A display box at `left`, `width` design units wide, its plot painted by
/// `paint` from the box's bounds and the design-to-window scale.
pub fn display_box(
    s: f32,
    left: f32,
    width: f32,
    paint: impl Fn(Bounds<Pixels>, f32, &mut Window, &mut App) + 'static,
) -> gpui::Div {
    div()
        .absolute()
        .left(px(left * s))
        .top(px(2.0 * s))
        .w(px(width * s))
        .h(px(BOX_H * s))
        .bg(color(BOX_GROUND))
        .border(px(1.0))
        .border_color(color(BOX_EDGE))
        .overflow_hidden()
        .child(
            canvas(
                |_, _, _| (),
                move |bounds, _, window, cx| {
                    let k = f32::from(bounds.size.width) / width;
                    paint(bounds, k, window, cx);
                },
            )
            .absolute()
            .size_full(),
        )
}

/// `child` at `(left, top)` in a box, in design units.
pub fn at(s: f32, left: f32, top: f32, child: impl IntoElement) -> gpui::Div {
    div()
        .absolute()
        .left(px(left * s))
        .top(px(top * s))
        .child(child)
}

/// `child` at the box's top right, `right` and `top` in from the corner.
pub fn at_right(s: f32, right: f32, top: f32, child: impl IntoElement) -> gpui::Div {
    div()
        .absolute()
        .right(px(right * s))
        .top(px(top * s))
        .child(child)
}

/// Axis labels painted along a box's foot: `(x, text)` in design units.
pub fn paint_axis(
    window: &mut Window,
    cx: &mut App,
    bounds: Bounds<Pixels>,
    k: f32,
    labels: &[(f32, String)],
) {
    let (x0, y0) = plate::xy(bounds.origin);
    let font = plate::mono(FontWeight::NORMAL);
    let width = f32::from(bounds.size.width) / k;
    for (x, text) in labels {
        // The labels at the edges sit inside the box.
        let align = if *x < 16.0 {
            Align::Left
        } else if *x > width - 24.0 {
            Align::Right
        } else {
            Align::Center
        };
        paint_text(
            window,
            cx,
            text,
            font.clone(),
            10.0 * k,
            color(AXIS_INK),
            (x0 + x * k, y0 + (PLOT_BOTTOM + 8.0) * k),
            align,
        );
    }
}

/// The frequency grid and its labels across a box `width` wide.
pub fn paint_freq_grid(
    window: &mut Window,
    cx: &mut App,
    bounds: Bounds<Pixels>,
    k: f32,
    width: f32,
) {
    let (x0, y0) = plate::xy(bounds.origin);
    for hz in FREQ_GRID {
        let x = x0 + x_of_hz(hz, width) * k;
        paint_rect(
            window,
            (x, y0 + PLOT_TOP * k, 1.0, (PLOT_BOTTOM - PLOT_TOP) * k),
            color(GRID),
        );
    }
    let labels: Vec<(f32, String)> = FREQ_TICKS
        .iter()
        .map(|(hz, text)| (x_of_hz(*hz, width), (*text).to_string()))
        .collect();
    paint_axis(window, cx, bounds, k, &labels);
}

/// A row of `IN` / `OUT` meters, live slots `slot` and `slot + 1`.
pub fn io_meters(bounds: &LiveBounds, slot: usize, s: f32) -> gpui::Div {
    let meter = |index: usize, label: &str| {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0 * s))
            .child(
                div()
                    .w(px(26.0 * s))
                    .child(caption(label, 10.0, 0.08, SOFT, s)),
            )
            .child(
                div()
                    .flex_1()
                    .h(px(14.0 * s))
                    .child(live_slot(bounds, slot + index).size_full()),
            )
    };
    div()
        .flex()
        .flex_col()
        .gap(px(6.0 * s))
        .child(meter(0, "In"))
        .child(meter(1, "Out"))
}

/// An In / Out meter's bar and reading, −48 to +6 dB, its hot end in
/// `accent`.
pub fn paint_meter(
    window: &mut Window,
    cx: &mut App,
    b: Bounds<Pixels>,
    db: f32,
    accent: gpui::Rgba,
    s: f32,
) {
    let (x, y) = plate::xy(b.origin);
    let (w, h) = (f32::from(b.size.width), f32::from(b.size.height));
    let bar_w = (w - 44.0 * s).max(0.0);
    let bar_y = y + (h - 7.0 * s) * 0.5;
    window.paint_quad(
        gpui::fill(plate::rect(x, bar_y, bar_w, 7.0 * s), color(0x111111))
            .corner_radii(px(2.0 * s)),
    );
    let fraction = ((db + 48.0) / 54.0).clamp(0.0, 1.0);
    let ink = if db > -1.0 { accent } else { color(0xd9d9d9) };
    paint_rect(window, (x, bar_y, bar_w * fraction, 7.0 * s), ink);
    paint_text(
        window,
        cx,
        &plate::signed(db.max(-120.0)),
        plate::mono(FontWeight::NORMAL),
        10.5 * s,
        color(plate::INK),
        (x + w, y + (h - 13.0 * s) * 0.5),
        Align::Right,
    );
}

/// A caption and a reading under it, right-aligned: a box's headline value.
pub fn headline(label: &str, value: String, ink: u32, s: f32) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .items_end()
        .child(caption(label, 9.5, 0.12, SOFT, s))
        .child(numbers(value, 18.0, ink, s))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ladders_land_on_tidy_figures() {
        assert_eq!(nice_step(1_000.0, 5), 200.0);
        assert_eq!(nice_step(3_000.0, 5), 500.0);
        assert_eq!(nice_ceiling(2_430.0), 2_500.0);
        assert_eq!(nice_ceiling(0.9), 1.0);
    }

    #[test]
    fn times_read_like_a_studio() {
        assert_eq!(fmt_time(375.0), "375 ms");
        assert_eq!(fmt_time(1_250.0), "1.25 s");
        assert_eq!(fmt_time(2_000.0), "2 s");
        assert_eq!(fmt_time(4.5), "4.5 ms");
        assert_eq!(fmt_seconds(2.4), "2.40 s");
        assert_eq!(fmt_seconds(f32::INFINITY), "\u{221E}");
    }

    #[test]
    fn the_frequency_axis_round_trips() {
        assert!(x_of_hz(20.0, 600.0).abs() < 1.0e-3);
        assert!((x_of_hz(20_000.0, 600.0) - 600.0).abs() < 1.0e-2);
        assert!((hz_at_x(x_of_hz(4_000.0, 364.0), 364.0) - 4_000.0).abs() < 1.0);
    }
}

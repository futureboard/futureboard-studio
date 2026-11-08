//! Pieces the WayGate, Compressor and MixStation plate editors share: a
//! card's notes and sections, a canvas painted over the live overlay, the
//! analyser's spectrum across a display box, and the reduction bar.
//!
//! Like the rest of the plate kit, sizes are at the design size and scaled
//! by `s` (or by a box's own `k`, its width over its design width).

use gpui::{
    canvas, div, px, AnyElement, App, Bounds, FontWeight, IntoElement, ParentElement, Pixels, Rgba,
    SharedString, Styled, Window,
};
use SpherePluginHost::spectrum::{CEIL_DB, FLOOR_DB, MAX_HZ, MIN_HZ, SPECTRUM_BINS};

use crate::components::plate::{
    self, caption, color, color_a, paint_polygon, paint_polyline, paint_rect, paint_text, type_px,
    words, Align, SOFT,
};
use crate::components::plugin_live::Live;
use crate::components::time_plate::x_of_hz;

/// A short explanation under a control: soft words that wrap.
pub fn note(text: impl Into<SharedString>, s: f32) -> gpui::Div {
    div()
        .text_size(type_px(11.0, s))
        .line_height(px(14.0 * s))
        .text_color(color(SOFT))
        .child(text.into())
}

/// A captioned group inside a card.
pub fn section(label: &str, s: f32) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap(px(7.0 * s))
        .child(caption(label, 10.5, 0.1, SOFT, s))
}

/// A row of knob cells spread across a card.
pub fn knob_row(cells: Vec<AnyElement>) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .justify_between()
        .items_start()
        .children(cells)
}

/// A label and a switch on one line.
pub fn switch_row(label: &str, switch: impl IntoElement, s: f32) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .child(words(label.to_string(), 12.0, plate::PALE, s))
        .child(switch)
}

/// A canvas filling its parent that paints after the live overlay: a
/// curve or handle drawn from the params over a live display. It takes no
/// pointer events.
pub fn over_canvas(paint: impl Fn(Bounds<Pixels>, &mut Window, &mut App) + 'static) -> gpui::Div {
    div().absolute().inset_0().child(plate::over_live(
        canvas(
            |_, _, _| (),
            move |bounds, _, window, cx| paint(bounds, window, cx),
        )
        .size_full(),
    ))
}

/// A linear level in dB, `-120` for silence.
pub fn level_db(linear: f32) -> f32 {
    if linear <= 1.0e-6 {
        -120.0
    } else {
        20.0 * linear.log10()
    }
}

/// The analyser's spectrum of the insert's input across a box `width`
/// design units wide, between `top` and `bottom` — nothing until the host
/// has published one.
pub fn paint_spectrum(
    window: &mut Window,
    live: &Live,
    b: Bounds<Pixels>,
    k: f32,
    width: f32,
    (top, bottom): (f32, f32),
    dim: f32,
) {
    let Some(bins) = live.spectrum_bins() else {
        return;
    };
    let (x0, y0) = plate::xy(b.origin);
    let at = |x: f32, y: f32| (x0 + x * k, y0 + y * k);
    let points: Vec<(f32, f32)> = bins
        .iter()
        .enumerate()
        .map(|(i, db)| {
            let hz = MIN_HZ * (MAX_HZ / MIN_HZ).powf(i as f32 / (SPECTRUM_BINS - 1) as f32);
            let level = ((db - FLOOR_DB) / (CEIL_DB - FLOOR_DB)).clamp(0.0, 1.0);
            at(x_of_hz(hz, width), bottom - level * (bottom - top))
        })
        .collect();
    let mut area = points.clone();
    area.push(at(width, bottom));
    area.push(at(0.0, bottom));
    paint_polygon(window, &area, color_a(0xffffff, 0.05 * dim));
    paint_polyline(window, &points, 1.0, color_a(0x6a6a6a, dim), None);
}

/// A reduction bar into `b` that grows from the right: `gr` dB of `span`,
/// nothing drawn on it while there is no reading.
pub fn paint_gr_bar(
    window: &mut Window,
    b: Bounds<Pixels>,
    gr: Option<f32>,
    span: f32,
    ink: Rgba,
    s: f32,
) {
    let (x, y) = plate::xy(b.origin);
    let (w, h) = (f32::from(b.size.width), f32::from(b.size.height));
    window
        .paint_quad(gpui::fill(plate::rect(x, y, w, h), color(0x1a1a1a)).corner_radii(px(2.0 * s)));
    if let Some(gr) = gr {
        let fill = (gr / span).clamp(0.0, 1.0) * w;
        paint_rect(window, (x + w - fill, y, fill, h), ink);
    }
}

/// `text` in numbers at `size` window pixels, right-aligned to the top
/// right of `b`.
pub fn paint_reading(
    window: &mut Window,
    cx: &mut App,
    b: Bounds<Pixels>,
    text: &str,
    size: f32,
    ink: Rgba,
) {
    let (x, y) = plate::xy(b.origin);
    let w = f32::from(b.size.width);
    paint_text(
        window,
        cx,
        text,
        plate::mono(FontWeight::NORMAL),
        size,
        ink,
        (x + w, y),
        Align::Right,
    );
}

/// `−3.2 dB` for a reduction, `0.0 dB` for none.
pub fn fmt_reduction(gr: f32) -> String {
    if gr > 0.05 {
        format!("\u{2212}{gr:.1} dB")
    } else {
        "0.0 dB".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_and_reductions_read_as_the_plates_print_them() {
        assert_eq!(level_db(0.0), -120.0);
        assert!((level_db(0.5) + 6.02).abs() < 0.01);
        assert_eq!(fmt_reduction(3.26), "\u{2212}3.3 dB");
        assert_eq!(fmt_reduction(0.01), "0.0 dB");
    }
}

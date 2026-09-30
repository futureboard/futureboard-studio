//! Rotary knob: the mixer's pan, and the SoundFont player's controls.
//!
//! Drawn, not assembled. The first version built its arc out of ninety-one
//! 2 px dots — one element each, per knob, per frame — and at the sizes the
//! mixer uses the dots never quite joined, so every arc read as a dotted,
//! slightly lumpy line. This paints the same knob from one canvas with real
//! strokes:
//!
//! * a recessed **track** ring over the full 270° sweep, so the travel is
//!   visible before anything is set;
//! * the **value** arc on it — from 12 o'clock for a bipolar knob (pan), from
//!   the start for a unipolar one — with round ends;
//! * a **body** disc inside the ring, and a **pointer** line from near its
//!   centre to its edge, which is what the eye reads the angle from;
//! * for a bipolar knob, a **detent** mark at 12 o'clock.
//!
//! Interaction matches the faders ([`crate::components::fader`]): a vertical
//! drag moves the value by how far the pointer travels — never to where it is
//! pressed — with Shift for fine adjustment, and a double-click or Alt-click
//! puts it back to its default.

use std::f32::consts::PI;

use gpui::{
    canvas, div, point, px, App, AppContext, Bounds, InteractiveElement, IntoElement,
    ParentElement, PathBuilder, Pixels, Point, StatefulInteractiveElement, Styled, Window,
};

use crate::components::fader::{drag_to, is_reset_click, FaderDrag};
use crate::theme::Colors;

/// Default knob diameter.
pub const KNOB_DEFAULT_SIZE: f32 = 30.0;

/// Sweep half-angle in degrees: 7 o'clock to 5 o'clock.
const SWEEP_DEG: f32 = 135.0;
/// Pointer travel for the whole range, in pixels of vertical drag.
const DRAG_SPAN_PX: f32 = 150.0;
/// Width of the track and value arcs.
const ARC_W: f32 = 2.5;
/// Angular step of the arc polylines. Tessellated as one stroke, so the
/// segments join cleanly at any step; 4° is smooth at mixer sizes.
const ARC_STEP_DEG: f32 = 4.0;

/// Render a unipolar knob (value sweeps from `min` at 7 o'clock to `max` at
/// 5 o'clock). The arc fills from the start.
pub fn knob(
    id: impl Into<gpui::SharedString>,
    value: f32,
    min: f32,
    max: f32,
    accent: gpui::Rgba,
    label: Option<gpui::SharedString>,
    on_change: impl Fn(&f32, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    render_knob(
        id.into(),
        value,
        min,
        max,
        KNOB_DEFAULT_SIZE,
        accent,
        false,
        label,
        min,
        on_change,
    )
}

/// Render a bipolar knob centred on zero: the arc runs from 12 o'clock to the
/// value, left for negative, right for positive. Pan uses this.
#[allow(clippy::too_many_arguments)]
pub fn knob_bipolar(
    id: impl Into<gpui::SharedString>,
    value: f32,
    min: f32,
    max: f32,
    size: f32,
    accent: gpui::Rgba,
    label: Option<gpui::SharedString>,
    default_value: f32,
    on_change: impl Fn(&f32, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    render_knob(
        id.into(),
        value,
        min,
        max,
        size,
        accent,
        true,
        label,
        default_value,
        on_change,
    )
}

/// Format a bipolar pan value `[-1, 1]` into the conventional mixer label.
/// `0.0` → "C", negative → "Lxx", positive → "Rxx".
pub fn format_pan_label(pan: f32) -> String {
    if pan.abs() < 0.005 {
        "C".to_string()
    } else {
        let p = (pan.abs() * 100.0).round().clamp(1.0, 100.0) as i32;
        if pan < 0.0 {
            format!("L{}", p)
        } else {
            format!("R{}", p)
        }
    }
}

/// Angle of a normalized position, in degrees clockwise from 12 o'clock.
fn angle_of(norm: f32) -> f32 {
    norm.clamp(0.0, 1.0) * 2.0 * SWEEP_DEG - SWEEP_DEG
}

#[allow(clippy::too_many_arguments)]
fn render_knob(
    id: gpui::SharedString,
    value: f32,
    min: f32,
    max: f32,
    size: f32,
    accent: gpui::Rgba,
    bipolar: bool,
    label: Option<gpui::SharedString>,
    default_value: f32,
    on_change: impl Fn(&f32, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let range = (max - min).max(0.0001);
    let value = value.clamp(min, max);
    let norm = ((value - min) / range).clamp(0.0, 1.0);
    // Where the value arc starts: zero for a bipolar knob, the bottom of the
    // travel for a unipolar one.
    let origin_norm = if bipolar {
        ((0.0 - min) / range).clamp(0.0, 1.0)
    } else {
        0.0
    };

    let id_string = id.to_string();
    let on_change = std::rc::Rc::new(on_change);
    let on_reset = on_change.clone();
    // The body disc and its rim, under the canvas: a rounded div draws a
    // crisper circle than a tessellated one, and it takes the hover rim.
    let inset = ARC_W + 2.5;
    let body = div()
        .absolute()
        .top(px(inset))
        .left(px(inset))
        .size(px(size - inset * 2.0))
        .rounded(px(crate::theme::radius::PILL))
        .bg(Colors::knob_bg())
        .border(px(1.0))
        .border_color(Colors::border_strong())
        .group_hover("fb-knob", |style| style.border_color(Colors::text_muted()));

    let disk = div()
        .id(gpui::ElementId::Name(id.clone()))
        .group("fb-knob")
        .relative()
        .flex_none()
        .size(px(size))
        .cursor(gpui::CursorStyle::ResizeUpDown)
        .child(body)
        .child(
            canvas(
                |_bounds, _window, _cx| (),
                move |bounds, _state, window, _cx| {
                    paint_knob(bounds, norm, origin_norm, bipolar, accent, window);
                },
            )
            .absolute()
            .top_0()
            .left_0()
            .size_full(),
        )
        .on_drag(
            FaderDrag::new(id_string.clone()),
            |drag, _offset, _window, cx| cx.new(|_| FaderDrag::new(drag.id.clone())),
        )
        .on_drag_move::<FaderDrag>(move |event, window, cx| {
            let drag = event.drag(cx);
            if drag.id != id_string {
                return;
            }
            let y: f32 = event.event.position.y.into();
            // Up turns it clockwise.
            let next = drag_to(
                drag,
                norm,
                y,
                event.event.modifiers.shift,
                DRAG_SPAN_PX,
                |dy| -dy,
            );
            let next = min + next * range;
            let next = (next * 1000.0).round() / 1000.0;
            on_change(&next, window, cx);
        })
        .on_click(move |event, window, cx| {
            if is_reset_click(event) {
                on_reset(&default_value, window, cx);
            }
        });

    let mut stack = div()
        .flex()
        .flex_col()
        .items_center()
        .w(px(size))
        .child(disk);

    if let Some(label_text) = label {
        stack = stack.child(
            div()
                .mt(px(2.0))
                .text_size(px(10.0))
                .text_color(Colors::text_faint())
                .child(label_text),
        );
    }

    stack
}

/// Paint the ring, the value arc, the detent and the pointer into `bounds`.
fn paint_knob(
    bounds: Bounds<Pixels>,
    norm: f32,
    origin_norm: f32,
    bipolar: bool,
    accent: gpui::Rgba,
    window: &mut Window,
) {
    let size = f32::from(bounds.size.width).min(f32::from(bounds.size.height));
    let center = point(
        f32::from(bounds.origin.x) + f32::from(bounds.size.width) / 2.0,
        f32::from(bounds.origin.y) + f32::from(bounds.size.height) / 2.0,
    );
    let ring_r = size / 2.0 - ARC_W / 2.0 - 0.5;

    // Track: the whole travel, recessed.
    stroke_arc(
        window,
        center,
        ring_r,
        -SWEEP_DEG,
        SWEEP_DEG,
        ARC_W,
        Colors::fader_groove(),
    );

    // Value: from the origin to the value, round-ended.
    let (from, to) = (angle_of(origin_norm), angle_of(norm));
    if (to - from).abs() > 0.5 {
        stroke_arc(
            window,
            center,
            ring_r,
            from.min(to),
            from.max(to),
            ARC_W,
            accent,
        );
        dot(window, polar(center, ring_r, from), ARC_W / 2.0, accent);
        dot(window, polar(center, ring_r, to), ARC_W / 2.0, accent);
    }

    // Detent: where zero is, over the track.
    if bipolar {
        let at = polar(center, ring_r, angle_of(origin_norm));
        dot(
            window,
            at,
            ARC_W / 2.0,
            if (to - from).abs() > 0.5 {
                accent
            } else {
                Colors::text_muted()
            },
        );
    }

    // Pointer: from near the body's centre out to its edge.
    let body_r = ring_r - ARC_W / 2.0 - 2.5;
    let mut pointer = PathBuilder::stroke(px(1.75));
    pointer.move_to(polar(center, body_r * 0.3, to));
    pointer.line_to(polar(center, body_r - 1.5, to));
    if let Ok(path) = pointer.build() {
        window.paint_path(path, Colors::text_primary());
    }
}

/// A point `radius` from `center` at `deg` clockwise from 12 o'clock.
fn polar(center: Point<f32>, radius: f32, deg: f32) -> Point<Pixels> {
    let rad = deg * PI / 180.0;
    point(
        px(center.x + radius * rad.sin()),
        px(center.y - radius * rad.cos()),
    )
}

fn stroke_arc(
    window: &mut Window,
    center: Point<f32>,
    radius: f32,
    from_deg: f32,
    to_deg: f32,
    width: f32,
    color: gpui::Rgba,
) {
    let steps = ((to_deg - from_deg).abs() / ARC_STEP_DEG).ceil().max(1.0) as usize;
    let mut path = PathBuilder::stroke(px(width));
    path.move_to(polar(center, radius, from_deg));
    for i in 1..=steps {
        let deg = from_deg + (to_deg - from_deg) * i as f32 / steps as f32;
        path.line_to(polar(center, radius, deg));
    }
    if let Ok(path) = path.build() {
        window.paint_path(path, color);
    }
}

/// A filled disc — the round end of an arc, or the detent.
fn dot(window: &mut Window, at: Point<Pixels>, radius: f32, color: gpui::Rgba) {
    const SEGMENTS: usize = 12;
    let center = point(f32::from(at.x), f32::from(at.y));
    let mut path = PathBuilder::fill();
    path.move_to(polar(center, radius, 0.0));
    for i in 1..SEGMENTS {
        path.line_to(polar(center, radius, i as f32 * 360.0 / SEGMENTS as f32));
    }
    path.close();
    if let Ok(path) = path.build() {
        window.paint_path(path, color);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sweep_runs_seven_to_five_oclock() {
        assert_eq!(angle_of(0.0), -SWEEP_DEG);
        assert_eq!(angle_of(0.5), 0.0);
        assert_eq!(angle_of(1.0), SWEEP_DEG);
        assert_eq!(angle_of(2.0), SWEEP_DEG);
    }

    #[test]
    fn pan_labels_are_a_side_and_an_amount() {
        assert_eq!(format_pan_label(0.0), "C");
        assert_eq!(format_pan_label(-0.5), "L50");
        assert_eq!(format_pan_label(1.0), "R100");
    }
}

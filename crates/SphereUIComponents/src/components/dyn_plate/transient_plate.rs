//! Transient's well and deck: the history and a sketch of a hit before and
//! after the shaping; Shape, Detector and Output cards.

use gpui::{div, px, AnyElement, App, Bounds, FontWeight, ParentElement, Pixels, Styled, Window};

use super::*;
use crate::components::plate::{card, card_title, deck, header, well};
use crate::components::time_plate::{at, at_right, display_box};

/// What `amount` percent does at the peak: `unchanged`, `+9.9 dB`.
fn describe(amount: f32) -> String {
    let db = amount / 100.0 * transient::MAX_SHAPE_DB;
    if db.abs() < 0.05 {
        "unchanged".to_string()
    } else {
        plate::fmt_db(db)
    }
}

/// `+55%`, `−15%`, `0%`: a shaping amount.
fn fmt_amount(amount: f32) -> String {
    let rounded = amount.round();
    let sign = if rounded < 0.0 {
        "\u{2212}"
    } else if rounded > 0.0 {
        "+"
    } else {
        ""
    };
    format!("{sign}{}%", rounded.abs())
}

/// A sketch of a hit: a fast rise and an exponential fall, the shaped one
/// moving from the attack's gain to the sustain's as the hit gives way to
/// its body. A picture of the settings at full depth, not a measurement.
fn paint_sketch(
    window: &mut Window,
    cx: &mut App,
    b: Bounds<Pixels>,
    k: f32,
    (attack, sustain): (f32, f32),
    dim: f32,
) {
    let (x0, y0) = plate::xy(b.origin);
    let at = |x: f32, y: f32| (x0 + x * k, y0 + y * k);
    let (left, right) = (14.0, NARROW_W - 14.0);
    let (top, bottom) = (PLOT_TOP + 8.0, PLOT_BOTTOM);
    let w = right - left;
    let h = bottom - top;
    let gain = |amount: f32| 10f32.powf(amount / 100.0 * transient::MAX_SHAPE_DB / 20.0);
    let (attack_gain, sustain_gain) = (gain(attack), gain(sustain));
    // The hit, dry and shaped, as levels over 0…1 of its length.
    let level = |t: f32, shaped: bool| {
        let env = if t < 0.05 {
            t / 0.05
        } else {
            (-(t - 0.05) / 0.35).exp()
        };
        let blend = ((t - 0.10) / 0.15).clamp(0.0, 1.0);
        let g = if shaped {
            attack_gain * (1.0 - blend) + sustain_gain * blend
        } else {
            1.0
        };
        env * g
    };
    const STEPS: usize = 160;
    let peak = (0..=STEPS)
        .map(|i| level(i as f32 / STEPS as f32, true))
        .fold(1.0f32, f32::max);
    // The taller of the two hits fills the plot; unity is the dry peak.
    let scale = h / peak;
    let points = |shaped: bool| -> Vec<(f32, f32)> {
        (0..=STEPS)
            .map(|i| {
                let t = i as f32 / STEPS as f32;
                at(left + t * w, bottom - level(t, shaped) * scale)
            })
            .collect()
    };
    paint_polyline(
        window,
        &[at(left, bottom - scale), at(right, bottom - scale)],
        1.0,
        color(GRID),
        None,
    );
    paint_polyline(
        window,
        &[at(left, bottom), at(right, bottom)],
        1.0,
        color(0x3a3a3a),
        None,
    );
    // Where the hit gives way to its body.
    let split = left + 0.175 * w;
    paint_polyline(
        window,
        &[at(split, top), at(split, bottom)],
        1.0,
        color_a(0xffffff, 0.12),
        Some((3.0 * k, 3.0 * k)),
    );
    let font = plate::outfit(FontWeight::MEDIUM);
    paint_text(
        window,
        cx,
        &plate::tracked("Attack", 0.1),
        font.clone(),
        9.5 * k,
        color(AXIS_INK),
        at(split - 6.0, bottom + 8.0),
        Align::Right,
    );
    paint_text(
        window,
        cx,
        &plate::tracked("Sustain", 0.1),
        font,
        9.5 * k,
        color(AXIS_INK),
        at(split + 6.0, bottom + 8.0),
        Align::Left,
    );
    paint_text(
        window,
        cx,
        "0 dB",
        plate::mono(FontWeight::NORMAL),
        10.0 * k,
        color(AXIS_INK),
        at(right, bottom - scale - 14.0),
        Align::Right,
    );
    let dry = points(false);
    paint_polyline(
        window,
        &dry,
        1.2 * k,
        color_a(0xffffff, 0.45),
        Some((3.0 * k, 3.0 * k)),
    );
    let shaped = points(true);
    let mut area = shaped.clone();
    area.push(at(right, bottom));
    area.push(at(left, bottom));
    paint_polygon(window, &area, color_a(TRANSIENT_ACCENT, 0.16 * dim));
    paint_polyline(
        window,
        &shaped,
        2.0 * k,
        color_a(TRANSIENT_ACCENT, dim),
        None,
    );
}

pub(super) fn display(e: &KitEditor<DynKind>, cx: &mut Cx<DynKind>, s: f32) -> AnyElement {
    let DynParams::Transient(p) = &e.params else {
        return div().into_any_element();
    };
    let history = history_box(e, s, WIDE_LEFT, WIDE_W);
    // Bypassed, the shaped hit is the dry one.
    let amounts = if p.power {
        (p.attack, p.sustain)
    } else {
        (0.0, 0.0)
    };
    let dim = if p.power { 1.0 } else { 0.4 };
    let dash = div()
        .flex()
        .flex_row()
        .gap(px(2.0 * s))
        .child(swatch(color_a(0xffffff, 0.45), s).w(px(4.0 * s)))
        .child(swatch(color_a(0xffffff, 0.45), s).w(px(4.0 * s)));
    let sketch = display_box(s, NARROW_LEFT, NARROW_W, move |b, k, window, cx| {
        paint_sketch(window, cx, b, k, amounts, dim)
    })
    .child(at(s, 10.0, 8.0, caption("Shape", 10.5, 0.1, FAINT, s)))
    .child(at_right(
        s,
        10.0,
        8.0,
        numbers(
            format!(
                "Attack {} \u{00B7} Sustain {}",
                describe(p.attack),
                describe(p.sustain)
            ),
            11.0,
            SOFT,
            s,
        ),
    ))
    .child(at_right(
        s,
        14.0,
        PLOT_BOTTOM + 5.0,
        div()
            .flex()
            .flex_row()
            .gap(px(14.0 * s))
            .child(legend(dash, "Dry", s))
            .child(legend(swatch(color(TRANSIENT_ACCENT), s), "Shaped", s)),
    ));
    well(s, 7.0, 327.0)
        .child(header(
            e,
            cx,
            s,
            "Attack and sustain shaper",
            color(TRANSIENT_ACCENT),
            Vec::new(),
        ))
        .child(div().relative().flex_1().child(history).child(sketch))
        .into_any_element()
}

pub(super) fn controls(e: &KitEditor<DynKind>, cx: &mut Cx<DynKind>, s: f32) -> AnyElement {
    deck(s, 341.0, 420.0)
        .p(px(14.0 * s))
        .gap(px(12.0 * s))
        .child(shape_card(e, cx, s).w(px(400.0 * s)).flex_shrink_0())
        .child(detector_card(e, cx, s).w(px(206.0 * s)).flex_shrink_0())
        .child(output_card(e, cx, s).flex_1())
        .into_any_element()
}

fn shape_card(e: &KitEditor<DynKind>, cx: &mut Cx<DynKind>, s: f32) -> gpui::Div {
    let DynParams::Transient(p) = &e.params else {
        return card(s);
    };
    let kind = DynKind::Transient;
    let ink = color(TRANSIENT_ACCENT);
    let look = hero_look(104.0);
    let cell = KnobCell::new(160.0, 11.0, 14.0);
    card(s)
        .child(card_title("Shape", s))
        .child(
            div()
                .flex()
                .flex_row()
                .justify_around()
                .child(knob_cell(
                    cx,
                    kind,
                    "attack",
                    p.attack,
                    Some(fmt_amount(p.attack)),
                    look,
                    cell,
                    ink,
                    true,
                    s,
                ))
                .child(knob_cell(
                    cx,
                    kind,
                    "sustain",
                    p.sustain,
                    Some(fmt_amount(p.sustain)),
                    look,
                    cell,
                    ink,
                    true,
                    s,
                )),
        )
        .child(note(
            format!(
                "Attack lifts or softens the hit, Sustain the body after it: \
                 \u{00B1}100 % is \u{00B1}{:.0} dB at the peak.",
                transient::MAX_SHAPE_DB
            ),
            s,
        ))
}

fn detector_card(e: &KitEditor<DynKind>, cx: &mut Cx<DynKind>, s: f32) -> gpui::Div {
    let DynParams::Transient(p) = &e.params else {
        return card(s);
    };
    let look = KnobLook::small(64.0);
    let cell = KnobCell::new(120.0, 10.5, 12.5);
    card(s)
        .child(card_title("Detector", s))
        .child(div().flex().flex_row().justify_center().child(knob_cell(
            cx,
            DynKind::Transient,
            "speed",
            p.speed,
            None,
            look,
            cell,
            color(TRANSIENT_ACCENT),
            true,
            s,
        )))
        .child(note(
            "How fast the envelopes follow: higher catches shorter hits and \
             hands over to the body sooner.",
            s,
        ))
}

fn output_card(e: &KitEditor<DynKind>, cx: &mut Cx<DynKind>, s: f32) -> gpui::Div {
    let DynParams::Transient(p) = &e.params else {
        return card(s);
    };
    let look = output_look(56.0);
    let cell = KnobCell::new(90.0, 10.0, 12.0);
    card(s)
        .child(card_title("Output", s))
        .child(div().flex().flex_row().justify_center().child(knob_cell(
            cx,
            DynKind::Transient,
            "mix",
            p.mix,
            None,
            look,
            cell,
            color(INK),
            true,
            s,
        )))
        .child(switch_row(e, cx, "stereoLink", "Stereo link", s))
        .child(meter_rows(DynKind::Transient, &e.live_bounds, s))
}

//! Z-Comp's well and deck: the VU on the circuit's face, the transfer curve
//! and the history; Circuit, Compression, Detector and Output cards.

use gpui::{div, px, AnyElement, App, Bounds, FontWeight, ParentElement, Pixels, Styled, Window};
use zcomp::CompModel;

use super::*;
use crate::components::plate::{card, card_title, columns, deck, header, well};
use crate::components::time_plate::{at, at_right, display_box};

/// The VU's sweep either side of vertical, in radians: 40°.
const SWEEP: f32 = 0.70;

/// A VU face's paper, top to bottom, its ink and its red.
#[derive(Debug, Clone, Copy)]
struct Face {
    top: u32,
    bottom: u32,
    ink: u32,
    hot: u32,
}

/// The face each circuit wears.
fn face_of(model: CompModel) -> Face {
    let (top, bottom, ink, hot) = match model {
        CompModel::Comp2500 => (0x66bac3, 0x1d5963, 0xf2f6fa, 0xd8492f),
        CompModel::Distressor => (0xdfb873, 0x765221, 0x1d1509, 0x8f2517),
        CompModel::Avalon => (0x9bc2a0, 0x466149, 0x101710, 0x8d3419),
        CompModel::Ssl => (0x4076b4, 0x17406e, 0xf2f6fa, 0xd8492f),
    };
    Face {
        top,
        bottom,
        ink,
        hot,
    }
}

/// The face's rectangle in the VU box, and the needle's pivot and the
/// scale's radius, in design units. The pivot sits under the face's foot.
fn vu_geometry() -> ((f32, f32, f32, f32), (f32, f32), f32) {
    let (x, y, w, h) = (10.0, 34.0, VU_W - 20.0, BOX_H - 44.0);
    let radius = (w * 0.60).min(h * 0.95);
    let pivot = (x + w * 0.5, y + h * 0.20 + radius);
    ((x, y, w, h), pivot, radius)
}

/// The full scale of the reduction face, in dB.
const GR_FULL_SCALE: f32 = 24.0;

/// The needle's position, −1 (left stop) to 1 (right stop): reduction rests
/// at the right, output reads −20 to +3 VU.
fn vu_position(output: bool, value: f32) -> f32 {
    let unit = if output {
        ((value + 20.0) / 23.0).clamp(0.0, 1.0)
    } else {
        1.0 - (value / GR_FULL_SCALE).clamp(0.0, 1.0).powf(0.6)
    };
    unit * 2.0 - 1.0
}

/// A point on the VU at `position` and radius `r`, in design units.
fn vu_point(position: f32, r: f32) -> (f32, f32) {
    let (_, pivot, _) = vu_geometry();
    let angle = position * SWEEP;
    (pivot.0 + r * angle.sin(), pivot.1 - r * angle.cos())
}

/// The face: paper, the scale for what it reads, and its titles.
fn paint_face(
    window: &mut Window,
    cx: &mut App,
    b: Bounds<Pixels>,
    k: f32,
    face: Face,
    output: bool,
    circuit: &'static str,
    dim: f32,
) {
    let (x0, y0) = plate::xy(b.origin);
    let at = |x: f32, y: f32| (x0 + x * k, y0 + y * k);
    let ((fx, fy, fw, fh), pivot, radius) = vu_geometry();
    let (left, top) = at(fx, fy);
    window.paint_quad(
        gpui::fill(
            plate::rect(left, top, fw * k, fh * k),
            gpui::linear_gradient(
                180.0,
                gpui::linear_color_stop(color(face.top), 0.0),
                gpui::linear_color_stop(color(face.bottom), 1.0),
            ),
        )
        .corner_radii(px(5.0 * k)),
    );
    let ink = color_a(face.ink, 0.85);
    let (ticks, labelled, hot_from): (&[f32], &[f32], Option<f32>) = if output {
        (
            &[
                -20.0, -10.0, -7.0, -5.0, -3.0, -2.0, -1.0, 0.0, 1.0, 2.0, 3.0,
            ],
            &[-20.0, -10.0, -7.0, -5.0, -3.0, 0.0, 3.0],
            Some(0.0),
        )
    } else {
        (
            &[0.0, 1.5, 3.0, 4.5, 6.0, 9.0, 12.0, 18.0, 24.0],
            &[0.0, 3.0, 6.0, 12.0, 24.0],
            None,
        )
    };
    let arc: Vec<(f32, f32)> = (0..=40)
        .map(|i| {
            let (x, y) = vu_point(i as f32 / 20.0 - 1.0, radius);
            at(x, y)
        })
        .collect();
    paint_polyline(window, &arc, 1.2 * k, ink, None);
    if let Some(from) = hot_from {
        let start = vu_position(true, from);
        let hot: Vec<(f32, f32)> = (0..=12)
            .map(|i| {
                let (x, y) = vu_point(start + (1.0 - start) * i as f32 / 12.0, radius + 2.5);
                at(x, y)
            })
            .collect();
        paint_polyline(window, &hot, 4.0 * k, color(face.hot), None);
    }
    let font = plate::mono(FontWeight::NORMAL);
    for tick in ticks {
        let position = vu_position(output, *tick);
        let major = labelled.contains(tick);
        let length = if major { 9.0 } else { 5.0 };
        let tick_ink = if hot_from.is_some_and(|from| *tick > from) {
            color(face.hot)
        } else {
            ink
        };
        let (ax, ay) = vu_point(position, radius);
        let (bx, by) = vu_point(position, radius + length);
        paint_polyline(
            window,
            &[at(ax, ay), at(bx, by)],
            if major { 1.4 } else { 1.0 } * k,
            tick_ink,
            None,
        );
        if major {
            let (lx, ly) = vu_point(position, radius + length + 10.0);
            let text = if output && *tick > 0.0 {
                format!("+{tick:.0}")
            } else {
                format!("{tick:.0}").replace('-', "\u{2212}")
            };
            paint_text(
                window,
                cx,
                &text,
                font.clone(),
                10.0 * k,
                tick_ink,
                at(lx, ly - 7.0),
                Align::Center,
            );
        }
    }
    let title_y = pivot.1 - radius * 0.46;
    paint_text(
        window,
        cx,
        &plate::tracked(if output { "VU" } else { "Gain reduction" }, 0.1),
        plate::outfit(FontWeight::MEDIUM),
        10.5 * k,
        color_a(face.ink, 0.85),
        at(fx + fw * 0.5, title_y),
        Align::Center,
    );
    paint_text(
        window,
        cx,
        &plate::tracked(circuit, 0.16),
        plate::outfit(FontWeight::SEMIBOLD),
        9.5 * k,
        color_a(face.ink, 0.6),
        at(fx + fw * 0.5, title_y + 16.0),
        Align::Center,
    );
    if dim < 1.0 {
        let (left, top) = at(fx, fy);
        window.paint_quad(
            gpui::fill(
                plate::rect(left, top, fw * k, fh * k),
                color_a(0x000000, 1.0 - dim),
            )
            .corner_radii(px(5.0 * k)),
        );
    }
}

/// The needle on the live reading — parked at rest, faint, while no
/// telemetry arrives — and the face's clip lamp.
pub(super) fn paint_needle(
    window: &mut Window,
    cx: &mut App,
    b: Bounds<Pixels>,
    live: &Live,
    params: &DynParams,
    output: bool,
) {
    let DynParams::Zcomp(p) = params else {
        return;
    };
    let k = f32::from(b.size.width) / VU_W;
    let (x0, y0) = plate::xy(b.origin);
    let at = |(x, y): (f32, f32)| (x0 + x * k, y0 + y * k);
    let ((fx, fy, fw, fh), pivot, radius) = vu_geometry();
    // The needle is drawn in the face's own ink, dark or light.
    let ink = face_of(p.model).ink;
    let live_now = live.frame.is_some();
    let value = if live_now {
        live.needle(output)
    } else if output {
        -20.0
    } else {
        0.0
    };
    let position = vu_position(output, value);
    let tip = vu_point(position, radius + 6.0);
    // From just inside the face's foot: the pivot sits under it.
    let below = pivot.1 - (fy + fh - 4.0);
    let base = vu_point(position, below / (position * SWEEP).cos());
    let needle = if live_now {
        color(ink)
    } else {
        color_a(ink, 0.4)
    };
    paint_polyline(window, &[at(base), at(tip)], 1.8 * k, needle, None);
    if !live_now {
        paint_text(
            window,
            cx,
            "no signal",
            plate::mono(FontWeight::NORMAL),
            10.0 * k,
            color_a(ink, 0.6),
            at((fx + fw * 0.5, pivot.1 - radius * 0.46 + 34.0)),
            Align::Center,
        );
    }
    if live.frame.is_some_and(|frame| frame.out_clip) {
        let (lx, ly) = at((fx + fw - 44.0, fy + 8.0));
        window.paint_quad(
            gpui::fill(plate::rect(lx, ly, 36.0 * k, 15.0 * k), color(0xd8492f))
                .corner_radii(px(3.0 * k)),
        );
        paint_text(
            window,
            cx,
            "CLIP",
            plate::outfit(FontWeight::SEMIBOLD),
            9.5 * k,
            color(0xffffff),
            (lx + 18.0 * k, ly + 1.5 * k),
            Align::Center,
        );
    }
}

pub(super) fn display(e: &KitEditor<DynKind>, cx: &mut Cx<DynKind>, s: f32) -> AnyElement {
    let DynParams::Zcomp(p) = &e.params else {
        return div().into_any_element();
    };
    let dim = if p.power { 1.0 } else { 0.4 };
    let output = e.ui.vu_output;
    let face = face_of(p.model);
    let circuit = p.model.display_name();
    let scale_pick = |label: &'static str, index: usize, cx: &mut Cx<DynKind>| {
        pill(
            ("zcomp-vu", index),
            label,
            (index == 1) == output,
            color(ZCOMP_ACCENT),
            PillLook::numbers(22.0, 8.0, 11.0),
            true,
            s,
            e.click_cb(cx, move |this, cx| {
                this.ui.vu_output = index == 1;
                cx.notify();
            }),
        )
    };
    let scales = div()
        .flex()
        .flex_row()
        .gap(px(3.0 * s))
        .child(scale_pick("GR", 0, cx))
        .child(scale_pick("+4", 1, cx));
    let vu = display_box(s, WIDE_LEFT, VU_W, move |b, k, window, cx| {
        paint_face(window, cx, b, k, face, output, circuit, dim)
    })
    .child(live_slot(&e.live_bounds, SLOT_VU).absolute().size_full())
    .child(at(s, 10.0, 8.0, caption("Meter", 10.5, 0.1, FAINT, s)))
    .child(at_right(s, 8.0, 5.0, scales));

    let c = zcomp::model_coeffs(p);
    let curve = TransferCurve::new(&e.params);
    let transfer = display_box(
        s,
        ZCOMP_TRANSFER_LEFT,
        ZCOMP_TRANSFER_W,
        move |b, k, window, cx| paint_transfer(window, cx, b, k, ZCOMP_TRANSFER_W, &curve),
    )
    .child(
        live_slot(&e.live_bounds, SLOT_TRANSFER)
            .absolute()
            .size_full(),
    )
    .child(at(s, 10.0, 8.0, caption("Transfer", 10.5, 0.1, FAINT, s)))
    .child(at_right(
        s,
        10.0,
        8.0,
        numbers(
            format!(
                "{} dB \u{00B7} {:.1}:1",
                plate::signed(c.threshold_db),
                c.ratio
            ),
            11.0,
            SOFT,
            s,
        ),
    ));

    let history = history_box(e, s, NARROW_LEFT, NARROW_W);
    let listen = listen_button(e, cx, "scListen", "SC Listen", s);
    well(s, 7.0, 327.0)
        .child(header(
            e,
            cx,
            s,
            "Multi-circuit dynamics",
            color(ZCOMP_ACCENT),
            vec![listen],
        ))
        .child(
            div()
                .relative()
                .flex_1()
                .child(vu)
                .child(transfer)
                .child(history),
        )
        .into_any_element()
}

pub(super) fn controls(e: &KitEditor<DynKind>, cx: &mut Cx<DynKind>, s: f32) -> AnyElement {
    deck(s, 341.0, 420.0)
        .p(px(14.0 * s))
        .gap(px(12.0 * s))
        .child(circuit_card(e, cx, s).w(px(236.0 * s)).flex_shrink_0())
        .child(compression_card(e, cx, s).w(px(344.0 * s)).flex_shrink_0())
        .child(detector_card(e, cx, s).w(px(150.0 * s)).flex_shrink_0())
        .child(output_card(e, cx, s).flex_1())
        .into_any_element()
}

fn circuit_card(e: &KitEditor<DynKind>, cx: &mut Cx<DynKind>, s: f32) -> gpui::Div {
    let DynParams::Zcomp(p) = &e.params else {
        return card(s);
    };
    let (_, labels) =
        crate::components::dyn_model::choice_labels(DynKind::Zcomp).unwrap_or(("model", &[]));
    let selected = p.model.to_wire() as usize;
    let mut pills = choice_pills(
        e,
        cx,
        "zcomp-model",
        "model",
        labels,
        selected,
        PillLook::words(30.0, 0.0, 12.5),
        s,
    );
    let second = pills.split_off(2.min(pills.len()));
    let release = match (p.model, p.auto_release) {
        (CompModel::Ssl, true) => "The dual time-constant network sets the release.",
        (_, true) => "The release leans further on the program.",
        (_, false) => "The release follows the knob.",
    };
    card(s)
        .child(card_title("Circuit", s))
        .child(
            section("Model", s)
                .child(columns(3.0, s, pills))
                .child(columns(3.0, s, second))
                .child(note(
                    crate::components::dyn_model::choice_hint(&e.params),
                    s,
                )),
        )
        .child(
            section("Release", s)
                .child(switch_row(e, cx, "autoRelease", "Auto release", s))
                .child(note(release, s)),
        )
}

fn compression_card(e: &KitEditor<DynKind>, cx: &mut Cx<DynKind>, s: f32) -> gpui::Div {
    let DynParams::Zcomp(p) = &e.params else {
        return card(s);
    };
    let kind = DynKind::Zcomp;
    let ink = color(ZCOMP_ACCENT);
    let big = hero_look(76.0);
    let big_cell = KnobCell::new(120.0, 10.5, 13.0);
    let small = KnobLook::small(52.0);
    let small_cell = KnobCell::new(96.0, 10.0, 12.0);
    // SSL's automatic network replaces the knob outright.
    let programmed = p.model == CompModel::Ssl && p.auto_release;
    let release = knob_cell(
        cx,
        kind,
        "releaseMs",
        p.release_ms,
        programmed.then(|| "Auto".to_string()),
        small,
        small_cell,
        ink,
        !programmed,
        s,
    );
    card(s)
        .child(card_title("Compression", s))
        .child(
            div()
                .flex()
                .flex_row()
                .justify_around()
                .child(knob_cell(
                    cx,
                    kind,
                    "thresholdDb",
                    p.threshold_db,
                    None,
                    big,
                    big_cell,
                    ink,
                    true,
                    s,
                ))
                .child(knob_cell(
                    cx, kind, "ratio", p.ratio, None, big, big_cell, ink, true, s,
                )),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .justify_between()
                .child(knob_cell(
                    cx,
                    kind,
                    "attackMs",
                    p.attack_ms,
                    None,
                    small,
                    small_cell,
                    ink,
                    true,
                    s,
                ))
                .child(release)
                .child(knob_cell(
                    cx, kind, "kneeDb", p.knee_db, None, small, small_cell, ink, true, s,
                )),
        )
}

fn detector_card(e: &KitEditor<DynKind>, cx: &mut Cx<DynKind>, s: f32) -> gpui::Div {
    let DynParams::Zcomp(p) = &e.params else {
        return card(s);
    };
    let kind = DynKind::Zcomp;
    let ink = color(ZCOMP_ACCENT);
    let look = KnobLook::small(48.0);
    let cell = KnobCell::new(110.0, 10.0, 12.0);
    card(s).child(card_title("Detector", s)).child(
        div()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(10.0 * s))
            .child(knob_cell(
                cx,
                kind,
                "sidechainHpfHz",
                p.sidechain_hpf_hz,
                None,
                look,
                cell,
                ink,
                true,
                s,
            ))
            .child(knob_cell(
                cx,
                kind,
                "stereoLink",
                p.stereo_link,
                None,
                look,
                cell,
                color(INK),
                true,
                s,
            ))
            .child(knob_cell(
                cx, kind, "color", p.color, None, look, cell, ink, true, s,
            )),
    )
}

fn output_card(e: &KitEditor<DynKind>, cx: &mut Cx<DynKind>, s: f32) -> gpui::Div {
    let DynParams::Zcomp(p) = &e.params else {
        return card(s);
    };
    let kind = DynKind::Zcomp;
    let look = output_look(54.0);
    let cell = KnobCell::new(84.0, 10.0, 12.0);
    card(s)
        .child(card_title("Output", s))
        .child(
            div()
                .flex()
                .flex_row()
                .justify_around()
                .child(knob_cell(
                    cx,
                    kind,
                    "makeupDb",
                    p.makeup_db,
                    None,
                    look,
                    cell,
                    color(INK),
                    true,
                    s,
                ))
                .child(knob_cell(
                    cx,
                    kind,
                    "mix",
                    p.mix,
                    None,
                    look,
                    cell,
                    color(INK),
                    true,
                    s,
                )),
        )
        .child(meter_rows(kind, &e.live_bounds, s))
}

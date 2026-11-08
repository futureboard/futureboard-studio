//! 67Clipper's well and deck: the history with its clip level and ceiling
//! and the transfer curve; Mode, Drive · Clip and Output cards.

use clipper67::{Mode, Oversampling};
use gpui::{div, px, AnyElement, ParentElement, Styled};

use super::*;
use crate::components::plate::{card, card_title, columns, deck, header, well};
use crate::components::time_plate::{at, at_right, display_box};

/// What the clipper's settings cost in delay: the filters' fixed taps and
/// the lookahead, as `clipper67::latency_for` adds them.
fn latency_hint(p: &clipper67::Params) -> String {
    let filters = p.oversampling.factor() > 1;
    let lookahead = p.mode.looks_ahead();
    let ms = clipper67::LOOKAHEAD_SECONDS * 1_000.0;
    match (filters, lookahead) {
        (false, false) => "Zero latency".to_string(),
        (false, true) => format!("Latency {ms:.0} ms lookahead"),
        (true, false) => format!("Latency {} samples", clipper67::TAPS_PER_PHASE),
        (true, true) => format!("Latency {} samples + {ms:.0} ms", clipper67::TAPS_PER_PHASE),
    }
}

pub(super) fn display(e: &KitEditor<DynKind>, cx: &mut Cx<DynKind>, s: f32) -> AnyElement {
    let DynParams::Clipper(p) = &e.params else {
        return div().into_any_element();
    };
    let history = history_box(e, s, WIDE_LEFT, WIDE_W);
    let curve = TransferCurve::new(&e.params);
    let transfer = display_box(s, NARROW_LEFT, NARROW_W, move |b, k, window, cx| {
        paint_transfer(window, cx, b, k, NARROW_W, &curve)
    })
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
                "Clip {} dB \u{00B7} {}",
                plate::signed(clipper67::clip_level_db(p)),
                p.oversampling.label()
            ),
            11.0,
            SOFT,
            s,
        ),
    ));
    let delta = listen_button(e, cx, "delta", "Delta", s);
    well(s, 7.0, 327.0)
        .child(header(
            e,
            cx,
            s,
            "Clipper \u{00B7} peak limiter",
            color(CLIPPER_ACCENT),
            vec![delta],
        ))
        .child(div().relative().flex_1().child(history).child(transfer))
        .into_any_element()
}

pub(super) fn controls(e: &KitEditor<DynKind>, cx: &mut Cx<DynKind>, s: f32) -> AnyElement {
    deck(s, 341.0, 420.0)
        .p(px(14.0 * s))
        .gap(px(12.0 * s))
        .child(mode_card(e, cx, s).w(px(262.0 * s)).flex_shrink_0())
        .child(drive_card(e, cx, s).w(px(390.0 * s)).flex_shrink_0())
        .child(output_card(e, cx, s).flex_1())
        .into_any_element()
}

fn mode_card(e: &KitEditor<DynKind>, cx: &mut Cx<DynKind>, s: f32) -> gpui::Div {
    let DynParams::Clipper(p) = &e.params else {
        return card(s);
    };
    let (_, labels) =
        crate::components::dyn_model::choice_labels(DynKind::Clipper).unwrap_or(("mode", &[]));
    let modes = choice_pills(
        e,
        cx,
        "clipper-mode",
        "mode",
        labels,
        p.mode.to_wire() as usize,
        PillLook::words(30.0, 0.0, 12.5),
        s,
    );
    let factors = Oversampling::ALL.map(Oversampling::label);
    let oversampling = choice_pills(
        e,
        cx,
        "clipper-os",
        "oversampling",
        &factors,
        p.oversampling.to_wire() as usize,
        PillLook::numbers(28.0, 0.0, 11.5),
        s,
    );
    card(s)
        .child(card_title("Mode", s))
        .child(
            section("Process", s)
                .child(columns(3.0, s, modes))
                .child(note(
                    crate::components::dyn_model::choice_hint(&e.params),
                    s,
                )),
        )
        .child(
            section("Oversampling", s)
                .child(columns(3.0, s, oversampling))
                .child(note(latency_hint(p), s)),
        )
}

fn drive_card(e: &KitEditor<DynKind>, cx: &mut Cx<DynKind>, s: f32) -> gpui::Div {
    let DynParams::Clipper(p) = &e.params else {
        return card(s);
    };
    let kind = DynKind::Clipper;
    let ink = color(CLIPPER_ACCENT);
    let look = hero_look(80.0);
    let cell = KnobCell::new(112.0, 10.5, 13.0);
    // Limit has no knee: the limiter holds peaks to the level outright.
    let limiting = p.mode == Mode::Limit;
    card(s)
        .child(card_title("Drive \u{00B7} Clip", s))
        .child(
            div()
                .flex()
                .flex_row()
                .justify_between()
                .child(knob_cell(
                    cx, kind, "inputDb", p.input_db, None, look, cell, ink, true, s,
                ))
                .child(knob_cell(
                    cx,
                    kind,
                    "thresholdDb",
                    p.threshold_db,
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
                    "shape",
                    p.shape,
                    limiting.then(|| "Limit".to_string()),
                    look,
                    cell,
                    ink,
                    !limiting,
                    s,
                )),
        )
        .child(note(
            "Input pushes, Threshold sets where it clips, Shape how round: \
             unity under the knee, 0 % a hard corner, 100 % soft.",
            s,
        ))
}

fn output_card(e: &KitEditor<DynKind>, cx: &mut Cx<DynKind>, s: f32) -> gpui::Div {
    let DynParams::Clipper(p) = &e.params else {
        return card(s);
    };
    let kind = DynKind::Clipper;
    let look = output_look(52.0);
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
                    "ceilingDb",
                    p.ceiling_db,
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
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(8.0 * s))
                .child(switch_row(e, cx, "dcFilter", "DC filter", s))
                .child(switch_row(e, cx, "stereoLink", "Stereo link", s)),
        )
        .child(meter_rows(kind, &e.live_bounds, s))
}

//! The native editors of the dynamics built-ins — FA-2A, FA-76, Z-Comp,
//! BurnLimit, 67Clipper and Transient — as one [`KitModel`] family.
//!
//! Each has a row of displays over a row of control cards:
//!
//! * the hardware emulations get a moving-coil VU, reading gain reduction or
//!   output (a switch in the editor, not a param);
//! * the limiter and the clipper draw their static curve from the DSP
//!   crates' own `transfer_db`, with the live operating point on it;
//! * every one keeps ten seconds of input, reduction and output history and
//!   a bay of In / reduction / Out meters.
//!
//! The panel paints only what follows the params; the live displays are
//! painted by the window's overlay into the bounds recorded here.

use std::sync::Arc;

use gpui::{
    canvas, div, fill, px, AnyElement, App, Bounds, IntoElement, ParentElement, Pixels, Styled,
    Window,
};

use crate::components::builtin_plugin_editor_window::{BuiltinEditorHostOps, PluginInstanceKey};
use crate::components::dyn_model::{
    choice_hint, choice_labels, knob, preset_applied, presets, DynKind, DynParams,
};
use crate::components::eq_graph::{paint_area, paint_line};
use crate::components::fx_model::KnobSpec;
use crate::components::native_plugin_shell::ShellIdentity;
use crate::components::plugin_kit::{
    bypass_notice, caption, card, choice, display_frame, flag, header, hint, knob_card, knob_for,
    open_kit_editor, Cx, KitEditor, KitModel, KitPreset, KitWindow, LiveBounds, LiveSlot,
    HERO_KNOB, KNOB, KNOB_PITCH,
};
use crate::components::plugin_live::{
    dashed, frame_of, label, paint_history, paint_meters, paint_operating_point, paint_vu,
    transfer_plot, Align, Live, Marker, MeterColumn, VuFace, VuScale,
};
use crate::components::quick_sampler_panel::rect;
use crate::theme::{space, typography, Colors};

/// The editor-only state: which scale the VU reads.
#[derive(Default)]
pub struct DynUi {
    pub vu_output: bool,
}

/// What the overlay paints from.
pub struct DynLiveView {
    params: DynParams,
    vu_output: bool,
}

impl KitModel for DynKind {
    type Params = DynParams;
    type Ui = DynUi;
    type LiveView = DynLiveView;

    fn title(self) -> &'static str {
        DynKind::title(self)
    }

    fn subtitle(self, params: &DynParams) -> String {
        match params {
            DynParams::Fa2a(_) => "Optical leveling amplifier".into(),
            DynParams::Fa76(_) => "FET limiting amplifier".into(),
            DynParams::Zcomp(p) => format!("Multi-circuit dynamics · {}", p.model.display_name()),
            DynParams::BurnLimit(_) => "Loudness maximizer".into(),
            DynParams::Clipper(_) => "Clipper and peak limiter".into(),
            DynParams::Transient(_) => "Attack and sustain shaper".into(),
        }
    }

    fn key(self) -> &'static str {
        DynKind::key(self)
    }

    fn defaults(self) -> DynParams {
        DynParams::defaults(self)
    }

    fn from_mirror(self, insert_id: &str) -> Option<DynParams> {
        use crate::components::builtin_plugin_editor as mirror;
        Some(match self {
            DynKind::Fa2a => DynParams::Fa2a(mirror::builtin_fa2a_params(insert_id)?),
            DynKind::Fa76 => DynParams::Fa76(mirror::builtin_fa76_params(insert_id)?),
            DynKind::Zcomp => DynParams::Zcomp(mirror::builtin_zcomp_params(insert_id)?),
            DynKind::BurnLimit => {
                DynParams::BurnLimit(mirror::builtin_burnlimit_params(insert_id)?)
            }
            DynKind::Clipper => DynParams::Clipper(mirror::builtin_clipper67_params(insert_id)?),
            DynKind::Transient => {
                DynParams::Transient(mirror::builtin_transient_params(insert_id)?)
            }
        })
    }

    fn wire_values(self, params: &DynParams) -> Vec<(u32, f32)> {
        params.wire_values()
    }

    fn with(self, params: &DynParams, id: &str, value: f32) -> DynParams {
        params.with(id, value)
    }

    fn value(self, params: &DynParams, id: &str) -> f32 {
        params.value(id)
    }

    fn presets(self) -> Arc<Vec<KitPreset<DynParams>>> {
        presets(self)
    }

    fn preset_applied(self, current: &DynParams, preset: &DynParams) -> DynParams {
        preset_applied(current, preset)
    }

    fn knob(self, id: &str) -> Option<KnobSpec> {
        knob(self, id)
    }

    fn panel(editor: &KitEditor<Self>, cx: &mut Cx<Self>) -> AnyElement {
        dyn_panel(editor, cx)
    }

    fn live_view(editor: &KitEditor<Self>) -> DynLiveView {
        DynLiveView {
            params: editor.params.clone(),
            vu_output: editor.ui.vu_output,
        }
    }

    fn paint_live(
        view: &DynLiveView,
        live: &mut Live,
        bounds: &LiveBounds,
        window: &mut Window,
        cx: &mut App,
    ) {
        paint_dyn_live(view, live, bounds, window, cx);
    }

    fn window_size(self) -> (f32, f32) {
        match self {
            DynKind::Fa2a => (1_000.0, 640.0),
            DynKind::Fa76 => (1_040.0, 640.0),
            DynKind::Zcomp => (1_100.0, 720.0),
            DynKind::BurnLimit => (1_040.0, 620.0),
            DynKind::Clipper => (1_000.0, 620.0),
            DynKind::Transient => (960.0, 600.0),
        }
    }

    fn min_size(self) -> (f32, f32) {
        (860.0, 560.0)
    }
}

pub type DynEditorWindow = KitWindow<DynKind>;

// ── The panel ───────────────────────────────────────────────────────────────

fn dyn_panel(e: &KitEditor<DynKind>, cx: &mut Cx<DynKind>) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .size_full()
        .bg(Colors::surface_window())
        .child(header(e, cx))
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h(px(0.0))
                .p(px(space::LOOSE))
                .gap(px(space::BASE))
                .child(displays(e))
                .child(controls(e, cx)),
        )
        .into_any_element()
}

/// A display column that takes `grow` shares of the row.
fn column(grow: f32, min_w: f32) -> gpui::Div {
    let mut column = div().flex().flex_col().flex_basis(px(0.0)).min_w(px(min_w));
    column.style().flex_grow = Some(grow);
    column
}

/// Room for the display's tags above what the overlay paints.
const TAG_ROOM: f32 = 26.0;

fn displays(e: &KitEditor<DynKind>) -> AnyElement {
    let kind = e.model;
    let bypassed = !e.power();
    let slots = &e.live_bounds;
    let row = div()
        .flex()
        .flex_row()
        .flex_1()
        .min_h(px(200.0))
        .gap(px(space::BASE))
        .opacity(if bypassed { 0.8 } else { 1.0 });

    let vu = || {
        column(3.0, 250.0).child(
            div()
                .flex_1()
                .p(px(space::SNUG))
                .border(px(1.0))
                .border_color(Colors::border_subtle())
                .bg(Colors::surface_canvas())
                .child(slots.slot(LiveSlot::Vu).size_full()),
        )
    };
    let history = |grow: f32| {
        column(grow, 260.0).child(
            display_frame(
                "LEVEL HISTORY",
                Some("10 s".to_string()),
                bypass_notice(e),
                div()
                    .size_full()
                    .pt(px(TAG_ROOM))
                    .child(slots.slot(LiveSlot::History).size_full()),
            )
            .flex_1(),
        )
    };
    let meters = column(0.0, 150.0).w(px(150.0)).flex_grow_0().child(
        div()
            .flex_1()
            .p(px(space::BASE))
            .border(px(1.0))
            .border_color(Colors::border_subtle())
            .bg(Colors::surface_panel())
            .child(slots.slot(LiveSlot::Meters).size_full()),
    );

    let row = match kind {
        DynKind::Fa2a | DynKind::Fa76 => row.child(vu()).child(history(4.0)),
        DynKind::Zcomp => row.child(vu()).child(transfer(e)),
        DynKind::BurnLimit | DynKind::Clipper => row.child(history(4.0)).child(transfer(e)),
        DynKind::Transient => row.child(history(4.0)).child(envelope(e)),
    };
    row.child(meters).into_any_element()
}

/// The input range a transfer display spans, to 0 dBFS.
fn transfer_range(kind: DynKind) -> f32 {
    match kind {
        DynKind::Zcomp => -60.0,
        _ => -24.0,
    }
}

/// The static curve, painted with the panel; the overlay adds the live
/// operating point into the same bounds.
fn transfer(e: &KitEditor<DynKind>) -> gpui::Div {
    let params = e.params.clone();
    let range = transfer_range(e.model);
    let bypassed = !e.power();
    let legend = match &params {
        DynParams::Zcomp(p) => {
            let c = zcomp::model_coeffs(p);
            format!("{:.1} dB · {:.1}:1", c.threshold_db, c.ratio)
        }
        DynParams::BurnLimit(p) => format!("Ceiling {:.1} dB", p.ceiling_db),
        DynParams::Clipper(p) => format!("Ceiling {:.1} dB", p.ceiling_db),
        _ => String::new(),
    };
    let curve = canvas(
        |_, _, _| (),
        move |bounds, _, window, cx| paint_transfer(window, cx, bounds, &params, range, bypassed),
    )
    .absolute()
    .size_full();
    column(2.0, 220.0).child(
        display_frame(
            "TRANSFER",
            Some(legend),
            None,
            div().relative().size_full().child(curve).child(
                e.live_bounds
                    .slot(LiveSlot::Transfer)
                    .absolute()
                    .size_full(),
            ),
        )
        .flex_1(),
    )
}

fn paint_transfer(
    window: &mut Window,
    cx: &mut App,
    bounds: Bounds<Pixels>,
    params: &DynParams,
    range: f32,
    bypassed: bool,
) {
    let (x0, y0, w, h) = transfer_plot(bounds);
    let ink = Colors::text_primary();
    let x_at = |db: f32| x0 + (db - range) / -range * w;
    let y_at = |db: f32| y0 + h - (db.clamp(range, 0.0) - range) / -range * h;
    let step = if range < -30.0 { 12.0 } else { 6.0 };
    let mut db = 0.0;
    while db >= range - 0.01 {
        window.paint_quad(fill(
            rect(x_at(db), y0, 1.0, h),
            Colors::with_alpha(ink, 0.06),
        ));
        window.paint_quad(fill(
            rect(x0, y_at(db), w, 1.0),
            Colors::with_alpha(ink, 0.06),
        ));
        if db < 0.0 && db > range {
            let text = format!("{db:.0}");
            let size = typography::DENSE_CAPTION;
            let faint = Colors::text_faint();
            label(
                window,
                cx,
                &text,
                size,
                faint,
                x_at(db),
                y0 + h + 4.0,
                Align::Center,
            );
            label(
                window,
                cx,
                &text,
                size,
                faint,
                x0 - 6.0,
                y_at(db) - 6.0,
                Align::Right,
            );
        }
        db -= step;
    }
    dashed(
        window,
        (x0, y0 + h),
        (x0 + w, y0),
        1.0,
        Colors::with_alpha(ink, 0.22),
    );

    // Where the action starts.
    let (line_db, line_color) = match params {
        DynParams::Zcomp(p) => (
            Some(zcomp::model_coeffs(p).threshold_db),
            Colors::accent_primary(),
        ),
        DynParams::BurnLimit(p) => (Some(p.ceiling_db), Colors::accent_warning()),
        DynParams::Clipper(p) => (Some(p.ceiling_db), Colors::accent_warning()),
        _ => (None, ink),
    };
    if let Some(db) = line_db {
        let color = Colors::with_alpha(line_color, 0.7);
        match params {
            DynParams::Zcomp(_) => dashed(window, (x_at(db), y0), (x_at(db), y0 + h), 1.0, color),
            _ => dashed(window, (x0, y_at(db)), (x0 + w, y_at(db)), 1.0, color),
        }
    }

    let steps = 120;
    let points: Vec<(f32, f32)> = (0..=steps)
        .map(|i| {
            let input = range - range * i as f32 / steps as f32;
            let output = params.transfer_db(input).unwrap_or(input);
            (x_at(input), y_at(output))
        })
        .collect();
    let accent = Colors::accent_primary();
    let alpha = if bypassed { 0.35 } else { 1.0 };
    paint_area(
        window,
        &points,
        y0 + h,
        Colors::with_alpha(accent, 0.10 * alpha),
    );
    paint_line(window, &points, 2.0, Colors::with_alpha(accent, alpha));
    label(
        window,
        cx,
        "In dB →",
        typography::DENSE_CAPTION,
        Colors::text_faint(),
        x0 + w,
        y0 + h + 4.0,
        Align::Right,
    );
}

/// Transient's sketch of a hit before and after: the dry envelope dashed,
/// the shaped one over it. A picture of the settings, not a measurement.
fn envelope(e: &KitEditor<DynKind>) -> gpui::Div {
    let DynParams::Transient(p) = &e.params else {
        return div();
    };
    let (attack, sustain) = (p.attack, p.sustain);
    let bypassed = !e.power();
    let describe = |amount: f32| {
        let db = amount / 100.0 * transient::MAX_SHAPE_DB;
        if db.abs() < 0.05 {
            "unchanged".to_string()
        } else {
            format!("{db:+.1} dB")
        }
    };
    let legend = format!(
        "Attack {} · Sustain {}",
        describe(attack),
        describe(sustain)
    );
    let sketch = canvas(
        |_, _, _| (),
        move |bounds, _, window, _| paint_envelope(window, bounds, attack, sustain, bypassed),
    )
    .size_full();
    column(2.0, 220.0).child(
        display_frame(
            "SHAPE",
            Some(legend),
            None,
            div().size_full().pt(px(TAG_ROOM)).child(sketch),
        )
        .flex_1(),
    )
}

fn paint_envelope(
    window: &mut Window,
    bounds: Bounds<Pixels>,
    attack: f32,
    sustain: f32,
    bypassed: bool,
) {
    let (x0, y0, w, h) = frame_of(bounds);
    let (x0, y0, w, h) = (x0 + 10.0, y0 + 6.0, w - 20.0, h - 16.0);
    let gain = |amount: f32| 10f32.powf(amount / 100.0 * transient::MAX_SHAPE_DB / 20.0);
    let (attack_gain, sustain_gain) = (gain(attack), gain(sustain));
    let scale = h / 1.6;
    let points = |shaped: bool| -> Vec<(f32, f32)> {
        (0..=120)
            .map(|i| {
                let t = i as f32 / 120.0;
                let env = if t < 0.05 {
                    t / 0.05
                } else {
                    (-(t - 0.05) / 0.35).exp()
                };
                let blend = ((t - 0.10) / 0.15).clamp(0.0, 1.0);
                let g = if shaped && !bypassed {
                    attack_gain * (1.0 - blend) + sustain_gain * blend
                } else {
                    1.0
                };
                (x0 + t * w, y0 + h - (env * g).min(1.6) * scale)
            })
            .collect()
    };
    let ink = Colors::text_primary();
    window.paint_quad(fill(
        rect(x0, y0 + h - scale, w, 1.0),
        Colors::with_alpha(ink, 0.08),
    ));
    let dry = points(false);
    for pair in dry.chunks(2) {
        if let [a, b] = pair {
            paint_line(window, &[*a, *b], 1.0, Colors::with_alpha(ink, 0.35));
        }
    }
    let shaped = points(true);
    let accent = Colors::accent_primary();
    paint_area(window, &shaped, y0 + h, Colors::with_alpha(accent, 0.12));
    paint_line(window, &shaped, 2.0, accent);
}

// ── Controls ────────────────────────────────────────────────────────────────

/// The segmented choice of the kind's mode, with what it does under it.
fn mode_card(
    e: &KitEditor<DynKind>,
    cx: &mut Cx<DynKind>,
    title: &'static str,
) -> Option<AnyElement> {
    let (id, labels) = choice_labels(e.model)?;
    let selected = e.value(id).round().max(0.0) as usize;
    let picker = choice(
        e,
        cx,
        "dyn-mode",
        labels,
        Some(selected),
        move |this, index, cx| this.set_value(id, index as f32, cx),
    );
    let width = crate::components::plugin_kit::track_width(labels);
    Some(card(
        title,
        0.0,
        width + 2.0 * space::BASE + 2.0,
        div()
            .flex()
            .flex_col()
            .gap(px(space::SNUG))
            .child(picker)
            .child(hint(choice_hint(&e.params))),
    ))
}

/// The VU's scale switch: gain reduction or output, under its own caption
/// unless the card it sits in already says what it is.
fn vu_switch(e: &KitEditor<DynKind>, cx: &mut Cx<DynKind>, captioned: bool) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(space::HAIR))
        .children(captioned.then(|| caption("METER")))
        .child(choice(
            e,
            cx,
            "dyn-vu",
            &["GR", "+4"],
            Some(usize::from(e.ui.vu_output)),
            |this, index, cx| {
                this.ui.vu_output = index == 1;
                cx.notify();
            },
        ))
        .into_any_element()
}

fn controls(e: &KitEditor<DynKind>, cx: &mut Cx<DynKind>) -> AnyElement {
    let row = div()
        .flex()
        .flex_row()
        .flex_wrap()
        .flex_shrink_0()
        .gap(px(space::BASE));
    let k = |e: &KitEditor<DynKind>, cx: &mut Cx<DynKind>, id: &'static str| {
        knob_for(e, cx, id, KNOB, None)
    };
    let hero = |e: &KitEditor<DynKind>, cx: &mut Cx<DynKind>, id: &'static str| {
        knob_for(e, cx, id, HERO_KNOB, None)
    };
    let row = match &e.params {
        DynParams::Fa2a(_) => {
            let leveling = div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::LOOSE))
                .child(hero(e, cx, "gainDb"))
                .child(hero(e, cx, "peakReduction"))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(space::SNUG))
                        .child(caption("MODE"))
                        .child(choice(
                            e,
                            cx,
                            "fa2a-mode",
                            &["Compress", "Limit"],
                            Some(e.value("mode").round() as usize),
                            |this, index, cx| this.set_value("mode", index as f32, cx),
                        ))
                        .child(vu_switch(e, cx, true)),
                );
            row.child(card("LEVELING", 3.0, 360.0, leveling))
                .child(knob_card(
                    "CHARACTER",
                    vec![
                        k(e, cx, "emphasis"),
                        k(e, cx, "sidechainLowCutHz"),
                        k(e, cx, "color"),
                    ],
                ))
                .child(knob_card(
                    "OUTPUT",
                    vec![k(e, cx, "mix"), k(e, cx, "outputTrimDb")],
                ))
        }
        DynParams::Fa76(_) => {
            let ratio = mode_card(e, cx, "RATIO");
            row.children(ratio)
                .child(card("METER", 0.0, 110.0, vu_switch(e, cx, false)))
                .child(knob_card(
                    "GAIN",
                    vec![hero(e, cx, "inputDb"), hero(e, cx, "outputDb")],
                ))
                .child(knob_card(
                    "TIMING",
                    vec![k(e, cx, "attackUs"), k(e, cx, "releaseMs")],
                ))
                .child(knob_card(
                    "SIDECHAIN · MIX",
                    vec![k(e, cx, "sidechainHpfHz"), k(e, cx, "mix")],
                ))
        }
        DynParams::Zcomp(p) => {
            let programmed = p.model == zcomp::CompModel::Ssl && p.auto_release;
            let circuit = mode_card(e, cx, "CIRCUIT");
            let modes = div()
                .flex()
                .flex_col()
                .gap(px(space::SNUG))
                .child(flag(e, cx, "autoRelease", "Auto release", true))
                .child(flag(e, cx, "scListen", "SC Listen", true))
                .child(vu_switch(e, cx, true));
            let release = knob_for(e, cx, "releaseMs", KNOB, programmed.then_some("Auto"));
            // Two rows: the circuit and how it listens, then the dial-up.
            let first = div()
                .flex()
                .flex_row()
                .gap(px(space::BASE))
                .children(circuit)
                .child(card("MODES", 0.0, 140.0, modes))
                .child(knob_card(
                    "DETECTOR",
                    vec![
                        k(e, cx, "sidechainHpfHz"),
                        k(e, cx, "stereoLink"),
                        k(e, cx, "color"),
                    ],
                ));
            let second = div()
                .flex()
                .flex_row()
                .gap(px(space::BASE))
                .child(knob_card(
                    "COMPRESSION",
                    vec![
                        k(e, cx, "thresholdDb"),
                        k(e, cx, "ratio"),
                        k(e, cx, "attackMs"),
                        release,
                        k(e, cx, "kneeDb"),
                    ],
                ))
                .child(knob_card(
                    "GAIN",
                    vec![k(e, cx, "makeupDb"), k(e, cx, "mix")],
                ));
            div()
                .flex()
                .flex_col()
                .flex_shrink_0()
                .gap(px(space::BASE))
                .child(first)
                .child(second)
        }
        DynParams::BurnLimit(_) => {
            let style = mode_card(e, cx, "STYLE");
            let options = div()
                .flex()
                .flex_col()
                .gap(px(space::SNUG))
                .child(flag(e, cx, "truePeak", "True Peak", true))
                .child(flag(e, cx, "stereoLink", "Stereo Link", true));
            row.children(style)
                .child(knob_card(
                    "LOUDNESS",
                    vec![hero(e, cx, "gainDb"), hero(e, cx, "ceilingDb")],
                ))
                .child(knob_card(
                    "TIMING",
                    vec![k(e, cx, "releaseMs"), k(e, cx, "lookaheadMs")],
                ))
                .child(card(
                    "OUTPUT",
                    2.0,
                    KNOB_PITCH + 150.0,
                    div()
                        .flex()
                        .flex_row()
                        .items_start()
                        .gap(px(space::LOOSE))
                        .child(k(e, cx, "mix"))
                        .child(options),
                ))
        }
        DynParams::Clipper(_) => {
            let mode = mode_card(e, cx, "MODE");
            let options = div()
                .flex()
                .flex_col()
                .gap(px(space::SNUG))
                .child(flag(e, cx, "dcFilter", "DC Filter", true))
                .child(flag(e, cx, "stereoLink", "Stereo Link", true));
            let clipping = div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::TIGHT))
                .child(hero(e, cx, "thresholdDb"))
                .child(hero(e, cx, "shape"))
                .child(hint("0 % hard · 100 % soft"));
            row.children(mode)
                .child(card("CLIPPING", 2.0, 2.0 * KNOB_PITCH + 130.0, clipping))
                .child(knob_card(
                    "OUTPUT",
                    vec![k(e, cx, "ceilingDb"), k(e, cx, "mix")],
                ))
                .child(card("OPTIONS", 1.0, 140.0, options))
        }
        DynParams::Transient(_) => row
            .child(knob_card(
                "SHAPE",
                vec![hero(e, cx, "attack"), hero(e, cx, "sustain")],
            ))
            .child(knob_card("DETECTOR", vec![k(e, cx, "speed")]))
            .child(card(
                "OUTPUT",
                2.0,
                KNOB_PITCH + 150.0,
                div()
                    .flex()
                    .flex_row()
                    .items_start()
                    .gap(px(space::LOOSE))
                    .child(k(e, cx, "mix"))
                    .child(flag(e, cx, "stereoLink", "Stereo Link", true)),
            )),
    };
    row.into_any_element()
}

// ── Live ────────────────────────────────────────────────────────────────────

/// The VU face a kind wears; Z-Comp's follows its circuit.
fn vu_face(params: &DynParams) -> VuFace {
    match params {
        DynParams::Fa2a(_) => VuFace::cream(),
        DynParams::Fa76(_) => VuFace::blue(),
        DynParams::Zcomp(p) => match p.model {
            zcomp::CompModel::Comp2500 => VuFace::tinted(0x66bac3, 0x1d5963, 0xf2f6fa, 0xd8492f),
            zcomp::CompModel::Distressor => VuFace::tinted(0xdfb873, 0x765221, 0x1d1509, 0x8f2517),
            zcomp::CompModel::Avalon => VuFace::tinted(0x9bc2a0, 0x466149, 0x101710, 0x8d3419),
            zcomp::CompModel::Ssl => VuFace::tinted(0x4076b4, 0x17406e, 0xf2f6fa, 0xd8492f),
        },
        _ => VuFace::blue(),
    }
}

/// History markers: the levels the plug-in holds the output to.
fn markers(params: &DynParams) -> Vec<Marker> {
    match params {
        DynParams::BurnLimit(p) => vec![Marker {
            db: p.ceiling_db,
            label: format!("Ceiling {:.1} dB", p.ceiling_db),
            color: Colors::accent_warning(),
        }],
        DynParams::Clipper(p) => vec![
            Marker {
                db: p.threshold_db,
                label: format!("Threshold {:.1} dB", p.threshold_db),
                color: Colors::accent_primary(),
            },
            Marker {
                db: p.ceiling_db,
                label: format!("Ceiling {:.1} dB", p.ceiling_db),
                color: Colors::accent_warning(),
            },
        ],
        _ => Vec::new(),
    }
}

fn paint_dyn_live(
    view: &DynLiveView,
    live: &mut Live,
    bounds: &LiveBounds,
    window: &mut Window,
    cx: &mut App,
) {
    let params = &view.params;
    let kind = params.kind();
    let bypassed = !params.power();
    let (reduction, sign) = kind.reduction_label();
    if let Some(b) = bounds.get(LiveSlot::History) {
        paint_history(window, cx, b, live, &markers(params), reduction, bypassed);
    }
    if let Some(b) = bounds.get(LiveSlot::Meters) {
        paint_meters(
            window,
            cx,
            b,
            live,
            &[
                MeterColumn::Input,
                MeterColumn::Reduction(reduction, sign),
                MeterColumn::Output,
            ],
            bypassed,
        );
    }
    if let Some(b) = bounds.get(LiveSlot::Vu) {
        let full_scale = if kind == DynKind::Zcomp { 24.0 } else { 20.0 };
        let (scale, title) = if view.vu_output {
            (VuScale::Output, "VU")
        } else {
            (VuScale::Reduction(full_scale), "GAIN REDUCTION dB")
        };
        paint_vu(window, cx, b, live, vu_face(params), scale, title);
    }
    if let Some(b) = bounds.get(LiveSlot::Transfer) {
        if !bypassed {
            paint_operating_point(
                window,
                b,
                live,
                transfer_range(kind),
                params.metered_input_offset_db(),
                |input| params.transfer_db(input).unwrap_or(input),
            );
        }
    }
}

// ── Opening ─────────────────────────────────────────────────────────────────

macro_rules! opener {
    ($name:ident, $kind:expr, $doc:literal) => {
        #[doc = $doc]
        pub fn $name(
            owner_bounds: Option<Bounds<Pixels>>,
            key: PluginInstanceKey,
            identity: ShellIdentity,
            host_ops: BuiltinEditorHostOps,
            on_close: Arc<dyn Fn(&mut Window, &mut App)>,
            cx: &mut App,
        ) -> Result<gpui::WindowHandle<DynEditorWindow>, String> {
            open_kit_editor($kind, owner_bounds, key, identity, host_ops, on_close, cx)
        }
    };
}

opener!(
    open_fa2a_editor,
    DynKind::Fa2a,
    "Opens with a track's FA-2A insert."
);
opener!(
    open_fa76_editor,
    DynKind::Fa76,
    "Opens with a track's FA-76 insert."
);
opener!(
    open_zcomp_editor,
    DynKind::Zcomp,
    "Opens with a track's Z-Comp insert."
);
opener!(
    open_burnlimit_editor,
    DynKind::BurnLimit,
    "Opens with a track's BurnLimit insert."
);
opener!(
    open_clipper67_editor,
    DynKind::Clipper,
    "Opens with a track's 67Clipper insert."
);
opener!(
    open_transient_editor,
    DynKind::Transient,
    "Opens with a track's Transient insert."
);

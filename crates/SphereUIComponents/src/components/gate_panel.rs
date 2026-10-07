//! WayGate's native editor, on the plug-in kit.
//!
//! A row of displays over a row of control cards:
//!
//! * **Gate history** — ten seconds of the input (shaded), the key the
//!   detector hears and the output, under the open and close thresholds with
//!   the hysteresis band between them, and a lane along the bottom showing
//!   how far open the gate is and when the detector is triggered;
//! * **Response** — the static in/out curve, drawn as the hysteresis loop it
//!   is: the gate opens on the way up at the threshold and lets go on the way
//!   down at `threshold − hysteresis`. The live operating point rides it;
//! * **Meters** — In, Key, the gate's gain over its range, Out, and the
//!   detector's state as a lamp and a word.
//!
//! The panel paints only what follows the params; the live displays are
//! painted by the window's overlay into the bounds recorded here. Telemetry
//! comes through the standard level frame: the key and the detector state ride
//! rack position [`waygate::KEY_SLOT`].

use std::sync::Arc;

use gpui::{
    canvas, div, fill, px, AnyElement, App, Bounds, IntoElement, ParentElement, Pixels, Styled,
    Window,
};

use crate::components::builtin_plugin_editor_window::{BuiltinEditorHostOps, PluginInstanceKey};
use crate::components::eq_graph::{paint_area, paint_line};
use crate::components::fx_model::KnobSpec;
use crate::components::gate_model::{
    self, mode_hint, mode_labels, opens_at, preset_applied, presets, readout, response_db,
    GateParams, LOOKAHEAD_ID,
};
use crate::components::knob::knob_with_default;
use crate::components::native_plugin_shell::ShellIdentity;
use crate::components::plugin_kit::{
    bypass_notice, card, choice, display_frame, flag, header, hint, knob_card, open_kit_editor, Cx,
    KitEditor, KitModel, KitPreset, KitWindow, LiveBounds, LiveSlot, HERO_KNOB, KNOB, KNOB_PITCH,
};
use crate::components::plugin_live::{
    dashed, dot, frame_of, label, transfer_plot, Align, HistoryPoint, Live, HISTORY,
};
use crate::components::quick_sampler_panel::{knob_cell, rect};
use crate::theme::{space, typography, Colors};

/// The bottom of every level axis the editor draws: the threshold's own
/// floor, so any threshold sits on the display.
const FLOOR_DB: f32 = waygate::RANGE_FLOOR_DB;
/// Room for the display's tags above what the overlay paints.
const TAG_ROOM: f32 = 26.0;

/// WayGate as a [`KitModel`]: one plug-in, so a unit type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WayGateModel;

/// What the overlay paints from.
pub struct GateLiveView {
    params: GateParams,
}

impl KitModel for WayGateModel {
    type Params = GateParams;
    type Ui = ();
    type LiveView = GateLiveView;

    fn title(self) -> &'static str {
        "WayGate"
    }

    fn subtitle(self, params: &GateParams) -> String {
        let what = match params.mode {
            waygate::Mode::Gate => "Noise gate",
            waygate::Mode::Duck => "Ducker",
        };
        if params.lookahead_ms > 0.0 {
            format!(
                "{what} · {} lookahead",
                readout(LOOKAHEAD_ID, params.lookahead_ms)
            )
        } else {
            what.to_string()
        }
    }

    fn key(self) -> &'static str {
        "waygate"
    }

    fn defaults(self) -> GateParams {
        waygate::default_params()
    }

    fn from_mirror(self, insert_id: &str) -> Option<GateParams> {
        crate::components::builtin_plugin_editor::builtin_waygate_params(insert_id)
    }

    fn wire_values(self, params: &GateParams) -> Vec<(u32, f32)> {
        gate_model::wire_values(params)
    }

    fn with(self, params: &GateParams, id: &str, value: f32) -> GateParams {
        gate_model::with(params, id, value)
    }

    fn value(self, params: &GateParams, id: &str) -> f32 {
        gate_model::value(params, id)
    }

    fn presets(self) -> Arc<Vec<KitPreset<GateParams>>> {
        presets()
    }

    fn preset_applied(self, current: &GateParams, preset: &GateParams) -> GateParams {
        preset_applied(current, preset)
    }

    fn knob(self, id: &str) -> Option<KnobSpec> {
        gate_model::knob(id)
    }

    fn panel(editor: &KitEditor<Self>, cx: &mut Cx<Self>) -> AnyElement {
        gate_panel(editor, cx)
    }

    fn live_view(editor: &KitEditor<Self>) -> GateLiveView {
        GateLiveView {
            params: editor.params.clone(),
        }
    }

    fn paint_live(
        view: &GateLiveView,
        live: &mut Live,
        bounds: &LiveBounds,
        window: &mut Window,
        cx: &mut App,
    ) {
        let params = &view.params;
        let bypassed = !params.power;
        if let Some(b) = bounds.get(LiveSlot::History) {
            paint_gate_history(window, cx, b, live, params, bypassed);
        }
        if let Some(b) = bounds.get(LiveSlot::Meters) {
            paint_gate_meters(window, cx, b, live, params, bypassed);
        }
        if let Some(b) = bounds.get(LiveSlot::Transfer) {
            if !bypassed {
                paint_response_point(window, b, live);
            }
        }
    }

    fn window_size(self) -> (f32, f32) {
        (1_060.0, 660.0)
    }

    fn min_size(self) -> (f32, f32) {
        (920.0, 580.0)
    }
}

pub type WayGateEditorWindow = KitWindow<WayGateModel>;

/// Opens with a track's WayGate insert.
pub fn open_waygate_editor(
    owner_bounds: Option<Bounds<Pixels>>,
    key: PluginInstanceKey,
    identity: ShellIdentity,
    host_ops: BuiltinEditorHostOps,
    on_close: Arc<dyn Fn(&mut Window, &mut App)>,
    cx: &mut App,
) -> Result<gpui::WindowHandle<WayGateEditorWindow>, String> {
    open_kit_editor(
        WayGateModel,
        owner_bounds,
        key,
        identity,
        host_ops,
        on_close,
        cx,
    )
}

// ── The panel ───────────────────────────────────────────────────────────────

fn gate_panel(e: &KitEditor<WayGateModel>, cx: &mut Cx<WayGateModel>) -> AnyElement {
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

fn db_text(db: f32) -> String {
    if db.abs() < 0.05 {
        "0.0".to_string()
    } else {
        format!("{db:.1}")
    }
}

fn displays(e: &KitEditor<WayGateModel>) -> AnyElement {
    let bypassed = !e.power();
    let slots = &e.live_bounds;
    let p = &e.params;
    let close_db = p.threshold_db - p.hysteresis_db;
    let legend = if p.hysteresis_db > 0.05 {
        format!(
            "Open {} · Close {} dB",
            db_text(p.threshold_db),
            db_text(close_db)
        )
    } else {
        format!("Threshold {} dB", db_text(p.threshold_db))
    };

    let history = column(4.0, 320.0).child(
        display_frame(
            "GATE HISTORY",
            Some("10 s".to_string()),
            bypass_notice(e),
            div()
                .size_full()
                .pt(px(TAG_ROOM))
                .child(slots.slot(LiveSlot::History).size_full()),
        )
        .flex_1(),
    );

    let params = p.clone();
    let curve = canvas(
        |_, _, _| (),
        move |bounds, _, window, cx| paint_response(window, cx, bounds, &params, bypassed),
    )
    .absolute()
    .size_full();
    let response = column(2.0, 230.0).child(
        display_frame(
            "RESPONSE",
            Some(legend),
            None,
            div()
                .relative()
                .size_full()
                .child(curve)
                .child(slots.slot(LiveSlot::Transfer).absolute().size_full()),
        )
        .flex_1(),
    );

    let meters = column(0.0, METER_BAY_W)
        .w(px(METER_BAY_W))
        .flex_grow_0()
        .child(
            div()
                .flex_1()
                .p(px(space::BASE))
                .border(px(1.0))
                .border_color(Colors::border_subtle())
                .bg(Colors::surface_panel())
                .child(slots.slot(LiveSlot::Meters).size_full()),
        );

    div()
        .flex()
        .flex_row()
        .flex_1()
        .min_h(px(220.0))
        .gap(px(space::BASE))
        .opacity(if bypassed { 0.8 } else { 1.0 })
        .child(history)
        .child(response)
        .child(meters)
        .into_any_element()
}

/// The knob for `id`, with the gate's own readout (−∞, Off).
fn gate_knob(
    e: &KitEditor<WayGateModel>,
    cx: &mut Cx<WayGateModel>,
    id: &'static str,
    size: f32,
) -> AnyElement {
    let Some(spec) = gate_model::knob(id) else {
        return div().into_any_element();
    };
    let value = e.value(id);
    let default = gate_model::value(&waygate::default_params(), id);
    let (min, max) = spec.knob_range();
    let control = knob_with_default(
        format!("waygate-{id}"),
        spec.to_knob(value),
        min,
        max,
        size,
        Colors::accent_primary(),
        spec.to_knob(default),
        e.knob_cb(cx, spec),
    );
    knob_cell(spec.label, readout(id, value), control)
}

fn controls(e: &KitEditor<WayGateModel>, cx: &mut Cx<WayGateModel>) -> AnyElement {
    let labels = mode_labels();
    let mode = choice(
        e,
        cx,
        "waygate-mode",
        labels,
        Some(e.params.mode.to_wire() as usize),
        |this, index, cx| this.set_value("mode", index as f32, cx),
    );
    let mode_card = card(
        "MODE",
        0.0,
        MODE_CARD_W,
        div()
            .flex()
            .flex_col()
            .gap(px(space::SNUG))
            .child(mode)
            .child(hint(mode_hint(&e.params))),
    );

    let levels = knob_card(
        "LEVELS",
        vec![
            gate_knob(e, cx, "thresholdDb", HERO_KNOB),
            gate_knob(e, cx, "rangeDb", HERO_KNOB),
            gate_knob(e, cx, "hysteresisDb", KNOB),
        ],
    );
    let timing = knob_card(
        "TIMING",
        vec![
            gate_knob(e, cx, "attackMs", KNOB),
            gate_knob(e, cx, "holdMs", KNOB),
            gate_knob(e, cx, "releaseMs", KNOB),
        ],
    );
    let lookahead = card(
        "LOOKAHEAD",
        1.0,
        KNOB_PITCH + 2.0 * space::BASE + 2.0,
        div()
            .flex()
            .flex_col()
            .gap(px(space::HAIR))
            .child(gate_knob(e, cx, LOOKAHEAD_ID, KNOB))
            .child(hint(if e.params.lookahead_ms > 0.0 {
                "Adds latency"
            } else {
                "No latency"
            })),
    );
    let options = div()
        .flex()
        .flex_col()
        .gap(px(space::SNUG))
        .child(flag(e, cx, "keyListen", "Key Listen", true))
        .child(flag(e, cx, "stereoLink", "Stereo Link", true));
    let key = card(
        "KEY FILTER",
        2.0,
        2.0 * KNOB_PITCH + KEY_OPTIONS_W,
        div()
            .flex()
            .flex_row()
            .items_start()
            .gap(px(space::LOOSE))
            .child(gate_knob(e, cx, "keyHpfHz", KNOB))
            .child(gate_knob(e, cx, "keyLpfHz", KNOB))
            .child(options),
    );

    div()
        .flex()
        .flex_row()
        .flex_wrap()
        .flex_shrink_0()
        .gap(px(space::BASE))
        .child(mode_card)
        .child(levels)
        .child(timing)
        .child(lookahead)
        .child(key)
        .into_any_element()
}

/// The mode card's width: room for its hint on two lines.
const MODE_CARD_W: f32 = 170.0;
/// The key card's column of switches.
const KEY_OPTIONS_W: f32 = 120.0;
/// The meter bay's width: four columns.
const METER_BAY_W: f32 = 200.0;

// ── The response ────────────────────────────────────────────────────────────

/// The axes the response is drawn on: key dB across, output dB up, both
/// from [`FLOOR_DB`] to 0.
fn response_axes(bounds: Bounds<Pixels>) -> (impl Fn(f32) -> f32, impl Fn(f32) -> f32) {
    let (x0, y0, w, h) = transfer_plot(bounds);
    let x_at = move |db: f32| x0 + (db.clamp(FLOOR_DB, 0.0) - FLOOR_DB) / -FLOOR_DB * w;
    let y_at = move |db: f32| y0 + h - (db.clamp(FLOOR_DB, 0.0) - FLOOR_DB) / -FLOOR_DB * h;
    (x_at, y_at)
}

fn paint_response(
    window: &mut Window,
    cx: &mut App,
    bounds: Bounds<Pixels>,
    params: &GateParams,
    bypassed: bool,
) {
    let (x0, y0, w, h) = transfer_plot(bounds);
    let (x_at, y_at) = response_axes(bounds);
    let ink = Colors::text_primary();
    let faint = Colors::text_faint();
    let mut db = 0.0;
    while db >= FLOOR_DB - 0.01 {
        window.paint_quad(fill(
            rect(x_at(db), y0, 1.0, h),
            Colors::with_alpha(ink, 0.06),
        ));
        window.paint_quad(fill(
            rect(x0, y_at(db), w, 1.0),
            Colors::with_alpha(ink, 0.06),
        ));
        if db < 0.0 && db > FLOOR_DB {
            let text = format!("{db:.0}");
            let size = typography::DENSE_CAPTION;
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
        db -= 20.0;
    }
    dashed(
        window,
        (x0, y0 + h),
        (x0 + w, y0),
        1.0,
        Colors::with_alpha(ink, 0.22),
    );

    let accent = Colors::accent_primary();
    let alpha = if bypassed { 0.35 } else { 1.0 };
    // The hysteresis band, where the branch taken depends on the way in.
    let close_db = params.threshold_db - params.hysteresis_db;
    if params.power && params.hysteresis_db > 0.05 {
        let left = x_at(close_db);
        window.paint_quad(fill(
            rect(left, y0, x_at(params.threshold_db) - left, h),
            Colors::with_alpha(accent, 0.08 * alpha),
        ));
    }

    // Each branch: from the floor up (rising) and from the top down
    // (falling), sampled finely enough to show the jump as a step.
    let branch = |rising: bool| -> Vec<(f32, f32)> {
        let steps = 320;
        (0..=steps)
            .map(|i| {
                let key = FLOOR_DB - FLOOR_DB * i as f32 / steps as f32;
                let open = opens_at(params, key, rising);
                (x_at(key), y_at(response_db(params, key, open)))
            })
            .collect()
    };
    let rising = branch(true);
    paint_area(
        window,
        &rising,
        y0 + h,
        Colors::with_alpha(accent, 0.10 * alpha),
    );
    paint_line(window, &rising, 2.0, Colors::with_alpha(accent, alpha));
    if params.power && params.hysteresis_db > 0.05 {
        let falling = branch(false);
        for pair in falling.chunks(2) {
            if let [a, b] = pair {
                paint_line(
                    window,
                    &[*a, *b],
                    1.5,
                    Colors::with_alpha(accent, 0.6 * alpha),
                );
            }
        }
    }
    label(
        window,
        cx,
        "Key dB →",
        typography::DENSE_CAPTION,
        faint,
        x0 + w,
        y0 + h + 4.0,
        Align::Right,
    );
}

/// The live operating point on the response: the key against what comes
/// out of the gate at it.
fn paint_response_point(window: &mut Window, bounds: Bounds<Pixels>, live: &Live) {
    let Some(frame) = live.frame else {
        return;
    };
    let key = frame.slot_in_peak[waygate::KEY_SLOT];
    if key <= 1.0e-5 {
        return;
    }
    let (_, y0, _, h) = transfer_plot(bounds);
    let (x_at, y_at) = response_axes(bounds);
    let key_db = 20.0 * key.log10();
    let x = x_at(key_db);
    let y = y_at(key_db - frame.gain_reduction_db.max(0.0));
    let color = Colors::accent_primary_hover();
    dashed(
        window,
        (x, y0 + h),
        (x, y),
        1.0,
        Colors::with_alpha(color, 0.55),
    );
    dot(window, x, y, 4.0, color);
}

// ── The history ─────────────────────────────────────────────────────────────

/// How open the gate is at a history point, `0` shut … `1` open: its
/// reduction over the range it ramps across. In Duck mode, how far *out* of
/// the duck it is.
fn openness(point: &HistoryPoint, params: &GateParams) -> f32 {
    let span = -params.range_db.clamp(FLOOR_DB, 0.0);
    if span < 0.05 {
        return 1.0;
    }
    1.0 - (point.reduction_db / span).clamp(0.0, 1.0)
}

fn paint_gate_history(
    window: &mut Window,
    cx: &mut App,
    bounds: Bounds<Pixels>,
    live: &Live,
    params: &GateParams,
    bypassed: bool,
) {
    const GUTTER: f32 = 34.0;
    const PAD_Y: f32 = 8.0;
    const LANE_H: f32 = 14.0;
    const LANE_GAP: f32 = 6.0;
    const LEGEND_H: f32 = 16.0;
    let (x0, y0, w, h) = frame_of(bounds);
    let plot_w = (w - GUTTER).max(1.0);
    let lane_y = y0 + h - LEGEND_H - LANE_H;
    let plot_h = (lane_y - LANE_GAP - y0 - PAD_Y).max(1.0);
    let plot_top = y0 + PAD_Y;
    let ink = Colors::text_primary();
    let accent = Colors::accent_primary();
    let y_at = |db: f32| plot_top + (db / FLOOR_DB).clamp(0.0, 1.0) * plot_h;
    let x_at = |age: usize| x0 + plot_w - age as f32 * plot_w / (HISTORY - 1) as f32;

    for db in [0.0, -12.0, -24.0, -36.0, -48.0, -60.0, -72.0] {
        let y = y_at(db);
        window.paint_quad(fill(
            rect(x0, y, plot_w, 1.0),
            Colors::with_alpha(ink, 0.06),
        ));
        label(
            window,
            cx,
            &format!("{db:.0}"),
            typography::DENSE_CAPTION,
            Colors::text_faint(),
            x0 + plot_w + 6.0,
            y - 6.0,
            Align::Left,
        );
    }

    // The hysteresis band, under the levels.
    let close_db = params.threshold_db - params.hysteresis_db;
    let alpha = if bypassed { 0.45 } else { 1.0 };
    if params.hysteresis_db > 0.05 {
        let top = y_at(params.threshold_db);
        window.paint_quad(fill(
            rect(x0, top, plot_w, y_at(close_db) - top),
            Colors::with_alpha(accent, 0.07 * alpha),
        ));
    }

    let count = live.history_len();
    if count > 1 {
        let points = |read: &dyn Fn(&HistoryPoint) -> f32| -> Vec<(f32, f32)> {
            (0..count)
                .map(|age| (x_at(age), y_at(read(&live.history_at(age)))))
                .collect()
        };
        let input = points(&|p| p.in_db);
        paint_area(
            window,
            &input,
            plot_top + plot_h,
            Colors::with_alpha(Colors::text_muted(), 0.20 * alpha),
        );
        paint_line(
            window,
            &input,
            1.0,
            Colors::with_alpha(Colors::text_muted(), 0.6 * alpha),
        );
        let key = points(&|p| p.slot_in_db);
        paint_line(
            window,
            &key,
            1.0,
            Colors::with_alpha(Colors::text_secondary(), 0.85 * alpha),
        );
        let output = points(&|p| p.out_db);
        paint_line(window, &output, 1.25, Colors::with_alpha(ink, 0.9 * alpha));
    }

    // The thresholds, over the levels.
    let marker = |window: &mut Window, cx: &mut App, db: f32, text: String, strength: f32| {
        let y = y_at(db);
        let color = Colors::with_alpha(accent, strength);
        dashed(window, (x0, y), (x0 + plot_w, y), 1.0, color);
        let tag_y = if y - 14.0 < y0 + 4.0 {
            y + 3.0
        } else {
            y - 14.0
        };
        label(
            window,
            cx,
            &text,
            typography::DENSE_CAPTION,
            color,
            x0 + 8.0,
            tag_y,
            Align::Left,
        );
    };
    marker(
        window,
        cx,
        params.threshold_db,
        format!("Open {} dB", db_text(params.threshold_db)),
        alpha,
    );
    if params.hysteresis_db > 0.05 {
        let y_gap = y_at(close_db) - y_at(params.threshold_db);
        // Its tag goes under the line when the band is too thin for two.
        let text = format!("Close {} dB", db_text(close_db));
        let y = y_at(close_db);
        let color = Colors::with_alpha(accent, 0.6 * alpha);
        dashed(window, (x0, y), (x0 + plot_w, y), 1.0, color);
        let tag_x = if y_gap < 16.0 { x0 + 110.0 } else { x0 + 8.0 };
        label(
            window,
            cx,
            &text,
            typography::DENSE_CAPTION,
            color,
            tag_x,
            y + 3.0,
            Align::Left,
        );
    }

    // The gate lane: how open it is, and a tick line while triggered.
    window.paint_quad(fill(rect(x0, lane_y, plot_w, LANE_H), Colors::meter_bg()));
    if count > 1 {
        let column_w = plot_w / (HISTORY - 1) as f32;
        for age in 0..count {
            let point = live.history_at(age);
            let open = if bypassed {
                1.0
            } else {
                openness(&point, params)
            };
            let x = x_at(age) - column_w;
            if open > 0.01 {
                let height = open * LANE_H;
                window.paint_quad(fill(
                    rect(x, lane_y + LANE_H - height, column_w + 0.5, height),
                    Colors::with_alpha(accent, 0.55 * alpha),
                ));
            }
            if point.slot_lit && !bypassed {
                window.paint_quad(fill(rect(x, lane_y, column_w + 0.5, 2.0), ink));
            }
        }
    }
    let lane_caption = match params.mode {
        waygate::Mode::Gate => "GATE",
        waygate::Mode::Duck => "DUCK",
    };
    label(
        window,
        cx,
        lane_caption,
        typography::DENSE_CAPTION,
        Colors::text_faint(),
        x0 + plot_w + 6.0,
        lane_y + 1.0,
        Align::Left,
    );
    label(
        window,
        cx,
        "In · Key · Out · lane: gain, bar: triggered",
        typography::DENSE_CAPTION,
        Colors::text_faint(),
        x0 + 8.0,
        lane_y + LANE_H + 2.0,
        Align::Left,
    );
}

// ── Meters ──────────────────────────────────────────────────────────────────

fn paint_gate_meters(
    window: &mut Window,
    cx: &mut App,
    bounds: Bounds<Pixels>,
    live: &Live,
    params: &GateParams,
    bypassed: bool,
) {
    const STATE_H: f32 = 22.0;
    const CAPTION_H: f32 = 16.0;
    const READOUT_H: f32 = 18.0;
    const BAR_W: f32 = 12.0;
    let (x0, y0, w, h) = frame_of(bounds);
    let has = live.frame.is_some() && live.history_len() > 0;

    // The detector's state: a lamp and a word, so it never rests on hue.
    let triggered = has && !bypassed && live.history_at(0).slot_lit;
    let word = match (bypassed, params.mode, triggered) {
        (true, ..) => "Bypassed",
        (false, _, _) if !has => "No signal",
        (false, waygate::Mode::Gate, true) => "Open",
        (false, waygate::Mode::Gate, false) => "Closed",
        (false, waygate::Mode::Duck, true) => "Ducking",
        (false, waygate::Mode::Duck, false) => "Idle",
    };
    let lamp = if triggered {
        Colors::accent_primary()
    } else {
        Colors::with_alpha(Colors::text_faint(), 0.6)
    };
    dot(window, x0 + 6.0, y0 + 7.0, 4.0, lamp);
    label(
        window,
        cx,
        word,
        typography::UI_XS,
        if triggered {
            Colors::text_primary()
        } else {
            Colors::text_muted()
        },
        x0 + 16.0,
        y0,
        Align::Left,
    );

    let top = y0 + STATE_H;
    let columns = 4;
    let column_w = w / columns as f32;
    let bar_top = top + CAPTION_H;
    let bar_h = (y0 + h - READOUT_H - bar_top).max(1.0);
    let level_unit = |db: f32| ((db - FLOOR_DB) / -FLOOR_DB).clamp(0.0, 1.0);
    let span = -params.range_db.clamp(FLOOR_DB, 0.0);

    for i in 0..columns {
        let centre = x0 + column_w * (i as f32 + 0.5);
        let caption = ["In", "Key", "Gain", "Out"][i];
        label(
            window,
            cx,
            caption,
            typography::DENSE_CAPTION,
            Colors::text_muted(),
            centre,
            top,
            Align::Center,
        );
        let x = centre - BAR_W * 0.5;
        window.paint_quad(fill(rect(x, bar_top, BAR_W, bar_h), Colors::meter_bg()));
        let text = if i == 2 {
            // The gain, hanging from the top over the range.
            let read = |p: &HistoryPoint| if bypassed { 0.0 } else { p.reduction_db };
            let now = if has { read(&live.history_at(0)) } else { 0.0 };
            let unit = if span > 0.05 {
                (now / span).clamp(0.0, 1.0)
            } else {
                0.0
            };
            window.paint_quad(fill(
                rect(x, bar_top, BAR_W, unit * bar_h),
                Colors::accent_primary(),
            ));
            if !has {
                "—".to_string()
            } else if now < 0.05 {
                "0.0".to_string()
            } else if waygate::is_full_mute(params.range_db) && now >= span - 0.05 {
                "−∞".to_string()
            } else {
                format!("−{now:.1}")
            }
        } else {
            let read = |p: &HistoryPoint| match i {
                0 => p.in_db,
                1 => p.slot_in_db,
                _ => p.out_db,
            };
            let now = if has {
                read(&live.history_at(0))
            } else {
                -120.0
            };
            let held = if has { live.held(read) } else { -120.0 };
            let unit = level_unit(now);
            let fill_top = bar_top + (1.0 - unit) * bar_h;
            let color = if unit >= 1.0 {
                Colors::meter_high()
            } else {
                Colors::text_muted()
            };
            window.paint_quad(fill(
                rect(x, fill_top, BAR_W, bar_top + bar_h - fill_top),
                color,
            ));
            let hold = level_unit(held);
            if hold > 0.0 {
                window.paint_quad(fill(
                    rect(x, bar_top + (1.0 - hold) * bar_h, BAR_W, 1.5),
                    Colors::text_primary(),
                ));
            }
            // The thresholds the key is compared with, on the In and Key bars.
            if i < 2 && !bypassed {
                let mut tick = |db: f32, strength: f32| {
                    let y = bar_top + (1.0 - level_unit(db)) * bar_h;
                    window.paint_quad(fill(
                        rect(x - 3.0, y, BAR_W + 6.0, 1.0),
                        Colors::with_alpha(Colors::accent_primary(), strength),
                    ));
                };
                tick(params.threshold_db, 1.0);
                if params.hysteresis_db > 0.05 {
                    tick(params.threshold_db - params.hysteresis_db, 0.55);
                }
            }
            if !has || held <= -119.0 {
                "—".to_string()
            } else {
                db_text(held)
            }
        };
        label(
            window,
            cx,
            &text,
            typography::UI_XS,
            Colors::text_secondary(),
            centre,
            bar_top + bar_h + 3.0,
            Align::Center,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openness_reads_the_reduction_over_the_range() {
        let params = gate_model::with(&waygate::default_params(), "rangeDb", -40.0);
        let point = |reduction_db: f32| HistoryPoint {
            reduction_db,
            ..HistoryPoint::default()
        };
        assert_eq!(openness(&point(0.0), &params), 1.0);
        assert_eq!(openness(&point(20.0), &params), 0.5);
        assert_eq!(openness(&point(40.0), &params), 0.0);
        let none = gate_model::with(&params, "rangeDb", 0.0);
        assert_eq!(openness(&point(0.0), &none), 1.0);
    }

    #[test]
    fn the_subtitle_names_the_mode_and_the_lookahead() {
        let params = waygate::default_params();
        assert_eq!(WayGateModel.subtitle(&params), "Noise gate");
        let ahead = gate_model::with(&gate_model::with(&params, "mode", 1.0), "lookaheadMs", 5.0);
        assert_eq!(WayGateModel.subtitle(&ahead), "Ducker · 5.0 ms lookahead");
    }
}

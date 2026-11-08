//! WayGate's editor, on the plate kit: ten seconds of the gate at work — the
//! input, the key the detector hears and the output under the open and close
//! thresholds, over a lane of how far open the gate is — and the static
//! response drawn as the hysteresis loop it is, in the well; Detector,
//! Timing, Key filter and Levels cards on the deck.
//!
//! The response is the gate's own law (`gate_model::response_db`, through
//! `waygate::gain_db_at`), painted when the panel renders. Everything that
//! moves is painted by the live overlay from the host's level frame: the key
//! rides rack position [`waygate::KEY_SLOT`]'s input level and the
//! detector's state its output level.

use std::sync::{Arc, OnceLock};

use gpui::{
    div, px, AnyElement, App, Bounds, FontWeight, InteractiveElement, IntoElement, ParentElement,
    Pixels, Rgba, StatefulInteractiveElement, Styled, Window,
};
use waygate::{Mode, Params};

use crate::components::builtin_plugin_editor::builtin_waygate_params;
use crate::components::builtin_plugin_editor_window::{BuiltinEditorHostOps, PluginInstanceKey};
use crate::components::fx_model::KnobSpec;
use crate::components::gate_model::{self, mode_hint, response_db};
use crate::components::native_plugin_shell::ShellIdentity;
use crate::components::plate::{
    self, caption, card, card_title, color, color_a, columns, deck, fmt_db, fmt_hz, fmt_hz_round,
    fmt_ms, header, icon, knob_reading, legend, live_slot, numbers, paint_dot, paint_polygon,
    paint_polyline, paint_rect, paint_text, pill, plate_frame, plate_scale, switch, type_px, well,
    Align, Dial, DialUnit, KnobCell, KnobLook, PillLook, FACE, FACE_EDGE, FAINT, GROUND, INK, SOFT,
    WELL_EDGE, WHITE,
};
use crate::components::plate_parts::{knob_row, level_db, note, section, switch_row};
use crate::components::plugin_kit::{
    open_kit_editor, Cx, KitEditor, KitModel, KitPreset, KitWindow, LiveBounds, LiveSlot,
};
use crate::components::plugin_live::{HistoryPoint, Live, HISTORY};
use crate::components::time_plate::{
    at, at_right, display_box, fmt_time, AXIS_INK, GRID, NARROW_W, WIDE_W,
};

/// WayGate's accent.
pub const ACCENT: u32 = 0x6ee07a;

const SLOT_HISTORY: usize = 0;
const SLOT_RESPONSE: usize = 1;
const SLOT_LEVELS: usize = 2;

/// The bottom of every level axis: the range's own floor, so any threshold
/// sits on the displays.
const FLOOR_DB: f32 = waygate::RANGE_FLOOR_DB;

/// The history plot in its box: the levels, then the gate lane under them;
/// the dB scale in the gutter to the right.
const H_PLOT_W: f32 = WIDE_W - 40.0;
const H_TOP: f32 = 32.0;
const H_BOTTOM: f32 = 190.0;
const LANE_TOP: f32 = 200.0;
const LANE_H: f32 = 14.0;

/// The response plot in its box.
const R_LEFT: f32 = 46.0;
const R_RIGHT: f32 = NARROW_W - 18.0;
const R_TOP: f32 = 50.0;
const R_BOTTOM: f32 = 212.0;

const HEADPHONES: &str = "M2 9 V6 A5 5 0 0 1 12 6 V9 M2 9 H4 V12 H2 Z M10 9 H12 V12 H10 Z";

fn accent() -> Rgba {
    color(ACCENT)
}

/// The dial over WayGate's descriptor row `id`: the DSP's own range and
/// reset value.
fn dial(id: &'static str, label: &'static str, unit: DialUnit) -> Dial {
    let row = waygate::descriptor()
        .params
        .iter()
        .find(|row| row.id == id)
        .map(|row| (row.min, row.max, row.default_value))
        .unwrap_or_else(|| panic!("WayGate has no param {id}"));
    Dial::new(id, label, row.0, row.1, row.2, unit)
}

struct Dials {
    threshold: Dial,
    range: Dial,
    hysteresis: Dial,
    /// Turned in hundredths of a millisecond: attacks reach down to 10 µs.
    attack: Dial,
    /// Turned over the square root of the hold, so the short holds a drum
    /// wants get most of the travel; the knob's value is `√ms`.
    hold: Dial,
    release: Dial,
    key_hpf: Dial,
    key_lpf: Dial,
    lookahead: Dial,
}

fn dials() -> &'static Dials {
    static DIALS: OnceLock<Dials> = OnceLock::new();
    DIALS.get_or_init(|| {
        let hold = dial("holdMs", "HOLD", DialUnit::Q);
        Dials {
            threshold: dial("thresholdDb", "THRESHOLD", DialUnit::Db),
            range: dial("rangeDb", "RANGE", DialUnit::Db),
            hysteresis: dial("hysteresisDb", "HYSTERESIS", DialUnit::Db),
            attack: dial("attackMs", "ATTACK", DialUnit::Q).log(),
            hold: Dial::new(
                hold.id,
                hold.label,
                hold.min.max(0.0).sqrt(),
                hold.max.sqrt(),
                hold.default.max(0.0).sqrt(),
                DialUnit::Q,
            ),
            release: dial("releaseMs", "RELEASE", DialUnit::Ms).log(),
            key_hpf: dial("keyHpfHz", "KEY HPF", DialUnit::HzRound).log(),
            key_lpf: dial("keyLpfHz", "KEY LPF", DialUnit::Hz).log(),
            lookahead: dial("lookaheadMs", "LOOKAHEAD", DialUnit::Ms),
        }
    })
}

/// A knob's reading in the gate's own words: a mute is −∞, a filter at its
/// end stop and a zero lookahead are off.
fn readout(id: &str, value: f32) -> String {
    match id {
        "rangeDb" if waygate::is_full_mute(value) => "\u{2212}\u{221E} dB".to_string(),
        "thresholdDb" | "rangeDb" => fmt_db(value),
        "hysteresisDb" => format!("{value:.1} dB"),
        "attackMs" if value < 1.0 => format!("{value:.2} ms"),
        "attackMs" => fmt_ms(value),
        "holdMs" | "releaseMs" => fmt_time(value),
        "keyHpfHz" if value <= waygate::KEY_HPF_OFF_HZ + 0.5 => "Off".to_string(),
        "keyHpfHz" => fmt_hz_round(value),
        "keyLpfHz" if value >= waygate::KEY_LPF_OFF_HZ - 0.5 => "Off".to_string(),
        "keyLpfHz" => fmt_hz(value),
        "lookaheadMs" if value <= 0.0 => "Off".to_string(),
        "lookaheadMs" => fmt_ms(value),
        _ => format!("{value:.1}"),
    }
}

/// `−40.0`: a threshold on a display.
fn db_short(db: f32) -> String {
    plate::signed(db)
}

/// How open the gate is at a history point, `0` shut … `1` open: its
/// reduction over the range it ramps across. In Duck mode, how far *out* of
/// the duck it is.
fn openness(point: &HistoryPoint, params: &Params) -> f32 {
    let span = -params.range_db.clamp(FLOOR_DB, 0.0);
    if span < 0.05 {
        return 1.0;
    }
    1.0 - (point.reduction_db / span).clamp(0.0, 1.0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WayGatePlate;

/// A WayGate insert's editor window.
pub type WayGateWindow = KitWindow<WayGatePlate>;

impl KitModel for WayGatePlate {
    type Params = Params;
    type Ui = ();
    type LiveView = Params;

    fn title(self) -> &'static str {
        "WayGate"
    }

    fn subtitle(self, params: &Params) -> String {
        let what = match params.mode {
            Mode::Gate => "Noise gate",
            Mode::Duck => "Ducker",
        };
        if params.lookahead_ms > 0.0 {
            format!(
                "{what} \u{00B7} {} lookahead",
                gate_model::readout(gate_model::LOOKAHEAD_ID, params.lookahead_ms)
            )
        } else {
            what.to_string()
        }
    }

    fn key(self) -> &'static str {
        "waygate-plate"
    }

    fn defaults(self) -> Params {
        waygate::default_params()
    }

    fn from_mirror(self, insert_id: &str) -> Option<Params> {
        builtin_waygate_params(insert_id)
    }

    fn wire_values(self, params: &Params) -> Vec<(u32, f32)> {
        gate_model::wire_values(params)
    }

    fn with(self, params: &Params, id: &str, value: f32) -> Params {
        gate_model::with(params, id, value)
    }

    fn value(self, params: &Params, id: &str) -> f32 {
        gate_model::value(params, id)
    }

    fn presets(self) -> Arc<Vec<KitPreset<Params>>> {
        gate_model::presets()
    }

    fn preset_applied(self, current: &Params, preset: &Params) -> Params {
        gate_model::preset_applied(current, preset)
    }

    fn knob(self, id: &str) -> Option<KnobSpec> {
        gate_model::knob(id)
    }

    fn panel(e: &KitEditor<Self>, cx: &mut Cx<Self>) -> AnyElement {
        let s = plate_scale(e.viewport);
        plate_frame(s, vec![display(e, cx, s), controls(e, cx, s)])
    }

    fn live_view(e: &KitEditor<Self>) -> Params {
        e.params.clone()
    }

    fn paint_live(
        view: &Params,
        live: &mut Live,
        bounds: &LiveBounds,
        window: &mut Window,
        cx: &mut App,
    ) {
        let viewport = window.viewport_size();
        let s = plate_scale((f32::from(viewport.width), f32::from(viewport.height)));
        if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_HISTORY)) {
            let k = f32::from(b.size.width) / WIDE_W;
            paint_history(window, cx, b, k, live, view);
        }
        if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_RESPONSE)) {
            let k = f32::from(b.size.width) / NARROW_W;
            paint_operating_point(window, b, k, live, view);
        }
        if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_LEVELS)) {
            paint_levels(window, cx, b, live, view, s);
        }
    }

    fn window_size(self) -> (f32, f32) {
        plate::WINDOW_SIZE
    }

    fn min_size(self) -> (f32, f32) {
        plate::MIN_WINDOW_SIZE
    }
}

// ── The well ───────────────────────────────────────────────────────────────

fn display(e: &KitEditor<WayGatePlate>, cx: &mut Cx<WayGatePlate>, s: f32) -> AnyElement {
    let p = e.params.clone();
    let listen = {
        let on = p.key_listen;
        let (bg, border, fg) = if on {
            (accent(), accent(), color(0x111111))
        } else {
            (color(GROUND), color(WELL_EDGE), color(INK))
        };
        div()
            .id("waygate-key-listen")
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0 * s))
            .h(px(28.0 * s))
            .px(px(10.0 * s))
            .rounded(px(3.0 * s))
            .border(px(1.0))
            .border_color(border)
            .bg(bg)
            .text_color(fg)
            .text_size(type_px(12.0, s))
            .cursor(gpui::CursorStyle::PointingHand)
            .child(icon(HEADPHONES, (14.0, 14.0), (13.0, 13.0), 1.3, fg, s))
            .child("Key listen")
            .on_click(e.click_cb(cx, |this, cx| this.toggle("keyListen", cx)))
            .into_any_element()
    };

    // The history: its grid and scale now, everything that moves live.
    let mode = p.mode;
    let swatch = |ink: u32| div().w(px(12.0 * s)).h(px(2.0 * s)).bg(color(ink));
    let history = display_box(s, 16.0, WIDE_W, move |b, k, window, cx| {
        paint_history_grid(window, cx, b, k, mode)
    })
    .child(
        live_slot(&e.live_bounds, SLOT_HISTORY)
            .absolute()
            .size_full(),
    )
    .child(at(
        s,
        10.0,
        8.0,
        plate::over_live(caption("Gate history", 10.5, 0.1, FAINT, s)),
    ))
    .child(at_right(
        s,
        48.0,
        10.0,
        plate::over_live(
            div()
                .flex()
                .flex_row()
                .gap(px(14.0 * s))
                .child(legend(swatch(0x6a6a6a), "In", s))
                .child(legend(swatch(0x9a9a9a), "Key", s))
                .child(legend(swatch(0xe6e6e6), "Out", s))
                .child(legend(
                    div()
                        .w(px(10.0 * s))
                        .h(px(8.0 * s))
                        .bg(color_a(ACCENT, 0.55)),
                    "Open",
                    s,
                )),
        ),
    ));

    // The response, painted from the params.
    let close_db = p.threshold_db - p.hysteresis_db;
    let headline = if p.hysteresis_db > 0.05 {
        format!("{} / {} dB", db_short(p.threshold_db), db_short(close_db))
    } else {
        format!("{} dB", db_short(p.threshold_db))
    };
    let curve = p.clone();
    let response = display_box(s, 630.0, NARROW_W, move |b, k, window, cx| {
        paint_response(window, cx, b, k, &curve)
    })
    .child(
        live_slot(&e.live_bounds, SLOT_RESPONSE)
            .absolute()
            .size_full(),
    )
    .child(at(s, 10.0, 8.0, caption("Response", 10.5, 0.1, FAINT, s)))
    .child(at_right(
        s,
        10.0,
        4.0,
        div()
            .flex()
            .flex_col()
            .items_end()
            .child(caption(
                if p.hysteresis_db > 0.05 {
                    "Open / close"
                } else {
                    "Threshold"
                },
                9.5,
                0.12,
                SOFT,
                s,
            ))
            .child(numbers(headline, 16.0, WHITE, s)),
    ));

    let subtitle = e.model.subtitle(&e.params);
    let right = vec![listen];
    well(s, 7.0, 327.0)
        .child(header(e, cx, s, &subtitle, accent(), right))
        .child(div().relative().flex_1().child(history).child(response))
        .into_any_element()
}

/// The history's level grid, its dB scale in the gutter, and the lane's
/// and time axis's labels.
fn paint_history_grid(window: &mut Window, cx: &mut App, b: Bounds<Pixels>, k: f32, mode: Mode) {
    let (x0, y0) = plate::xy(b.origin);
    let at = |x: f32, y: f32| (x0 + x * k, y0 + y * k);
    let mono = plate::mono(FontWeight::NORMAL);
    for db in [0.0, -20.0, -40.0, -60.0, -80.0] {
        let y = history_y(db);
        paint_polyline(
            window,
            &[at(0.0, y), at(H_PLOT_W, y)],
            1.0,
            color(GRID),
            None,
        );
        paint_text(
            window,
            cx,
            &plate::signed(db).replace(".0", ""),
            mono.clone(),
            10.0 * k,
            color(AXIS_INK),
            at(H_PLOT_W + 8.0, y - 6.5),
            Align::Left,
        );
    }
    paint_text(
        window,
        cx,
        &plate::tracked(
            match mode {
                Mode::Gate => "Gate",
                Mode::Duck => "Duck",
            },
            0.1,
        ),
        plate::outfit(FontWeight::MEDIUM),
        9.5 * k,
        color(AXIS_INK),
        at(H_PLOT_W + 8.0, LANE_TOP + 1.0),
        Align::Left,
    );
    let foot = LANE_TOP + LANE_H + 8.0;
    paint_text(
        window,
        cx,
        "\u{2212}10 s",
        mono.clone(),
        10.0 * k,
        color(AXIS_INK),
        at(0.0 + 6.0, foot),
        Align::Left,
    );
    paint_text(
        window,
        cx,
        "lane: how open \u{00B7} tick: triggered",
        plate::outfit(FontWeight::NORMAL),
        10.0 * k,
        color(AXIS_INK),
        at(H_PLOT_W * 0.5, foot),
        Align::Center,
    );
    paint_text(
        window,
        cx,
        "now",
        mono,
        10.0 * k,
        color(AXIS_INK),
        at(H_PLOT_W, foot),
        Align::Right,
    );
}

fn history_y(db: f32) -> f32 {
    H_TOP + (db / FLOOR_DB).clamp(0.0, 1.0) * (H_BOTTOM - H_TOP)
}

/// The levels, the thresholds and the gate lane, newest at the right.
fn paint_history(
    window: &mut Window,
    cx: &mut App,
    b: Bounds<Pixels>,
    k: f32,
    live: &Live,
    p: &Params,
) {
    let (x0, y0) = plate::xy(b.origin);
    let at = |x: f32, y: f32| (x0 + x * k, y0 + y * k);
    let bypassed = !p.power;
    let dim = if bypassed { 0.45 } else { 1.0 };
    let step = H_PLOT_W / (HISTORY - 1) as f32;
    let x_age = |age: usize| H_PLOT_W - age as f32 * step;
    let close_db = p.threshold_db - p.hysteresis_db;

    // The hysteresis band, under the levels.
    if p.hysteresis_db > 0.05 {
        let top = history_y(p.threshold_db);
        let (bx, by) = at(0.0, top);
        paint_rect(
            window,
            (bx, by, H_PLOT_W * k, (history_y(close_db) - top) * k),
            color_a(ACCENT, 0.07 * dim),
        );
    }

    let count = live.history_len();
    if count > 1 {
        let line = |read: &dyn Fn(&HistoryPoint) -> f32| -> Vec<(f32, f32)> {
            (0..count)
                .rev()
                .map(|age| at(x_age(age), history_y(read(&live.history_at(age)))))
                .collect()
        };
        let input = line(&|point| point.in_db);
        let mut area = input.clone();
        area.push(at(H_PLOT_W, H_BOTTOM));
        area.push(at(x_age(count - 1), H_BOTTOM));
        paint_polygon(window, &area, color_a(0xffffff, 0.06 * dim));
        paint_polyline(window, &input, 1.0, color_a(0x6a6a6a, dim), None);
        let key = line(&|point| point.slot_in_db);
        paint_polyline(window, &key, 1.0 * k, color_a(0x9a9a9a, dim), None);
        let output = line(&|point| point.out_db);
        paint_polyline(window, &output, 1.4 * k, color_a(0xe6e6e6, dim), None);
    }

    // The thresholds, over the levels, each with its tag.
    let mono = plate::mono(FontWeight::NORMAL);
    let open_y = history_y(p.threshold_db);
    paint_polyline(
        window,
        &[at(0.0, open_y), at(H_PLOT_W, open_y)],
        1.0,
        color_a(ACCENT, dim),
        Some((5.0 * k, 3.0 * k)),
    );
    let open_tag = if open_y - 15.0 < H_TOP - 4.0 {
        open_y + 3.0
    } else {
        open_y - 15.0
    };
    paint_text(
        window,
        cx,
        &format!("open {}", db_short(p.threshold_db)),
        mono.clone(),
        10.0 * k,
        color_a(ACCENT, dim),
        at(8.0, open_tag),
        Align::Left,
    );
    if p.hysteresis_db > 0.05 {
        let y = history_y(close_db);
        paint_polyline(
            window,
            &[at(0.0, y), at(H_PLOT_W, y)],
            1.0,
            color_a(ACCENT, 0.55 * dim),
            Some((3.0 * k, 3.0 * k)),
        );
        // Its tag steps aside when the band is too thin for two lines.
        let tag_x = if y - open_y < 15.0 { 112.0 } else { 8.0 };
        paint_text(
            window,
            cx,
            &format!("close {}", db_short(close_db)),
            mono,
            10.0 * k,
            color_a(ACCENT, 0.7 * dim),
            at(tag_x, y + 3.0),
            Align::Left,
        );
    }

    // The gate lane: how open the gate is, and a tick while triggered.
    let (lx, ly) = at(0.0, LANE_TOP);
    paint_rect(window, (lx, ly, H_PLOT_W * k, LANE_H * k), color(0x1a1a1a));
    if count > 1 {
        for age in 0..count {
            let point = live.history_at(age);
            let open = if bypassed { 1.0 } else { openness(&point, p) };
            let x = x_age(age) - step;
            if open > 0.01 {
                let (cx0, cy0) = at(x, LANE_TOP + LANE_H * (1.0 - open));
                paint_rect(
                    window,
                    (cx0, cy0, step * k + 0.5, LANE_H * open * k),
                    color_a(ACCENT, 0.55 * dim),
                );
            }
            if point.slot_lit && !bypassed {
                let (tx, ty) = at(x, LANE_TOP);
                paint_rect(window, (tx, ty, step * k + 0.5, 2.0 * k), color(WHITE));
            }
        }
    }
}

fn response_x(db: f32) -> f32 {
    R_LEFT + (db.clamp(FLOOR_DB, 0.0) - FLOOR_DB) / -FLOOR_DB * (R_RIGHT - R_LEFT)
}

fn response_y(db: f32) -> f32 {
    R_BOTTOM - (db.clamp(FLOOR_DB, 0.0) - FLOOR_DB) / -FLOOR_DB * (R_BOTTOM - R_TOP)
}

/// The rising branch of the response — up from the floor, opening at the
/// threshold — as its four corners: the gate's law is flat on either side
/// of the jump.
fn rising_branch(p: &Params) -> [(f32, f32); 4] {
    let t = p.threshold_db;
    [
        (FLOOR_DB, response_db(p, FLOOR_DB, false)),
        (t, response_db(p, t, false)),
        (t, response_db(p, t, true)),
        (0.0, response_db(p, 0.0, true)),
    ]
}

/// What the falling branch adds: held open from the threshold down to
/// `threshold − hysteresis`, where it lets go.
fn falling_branch(p: &Params) -> [(f32, f32); 3] {
    let (t, c) = (p.threshold_db, p.threshold_db - p.hysteresis_db);
    [
        (t, response_db(p, t, true)),
        (c, response_db(p, c, true)),
        (c, response_db(p, c, false)),
    ]
}

/// `points` — `(key, out)` in dB — with each segment cut where it crosses
/// the floor, so a branch that leaves the display runs along its foot at the
/// right slope instead of bending to meet it.
fn clip_to_floor(points: &[(f32, f32)]) -> Vec<(f32, f32)> {
    let mut clipped = Vec::with_capacity(points.len() * 2);
    for (i, &(key, out)) in points.iter().enumerate() {
        if let Some(&(prev_key, prev_out)) = i.checked_sub(1).map(|j| &points[j]) {
            if (prev_out < FLOOR_DB) != (out < FLOOR_DB) && (out - prev_out).abs() > 1.0e-6 {
                let t = (FLOOR_DB - prev_out) / (out - prev_out);
                clipped.push((prev_key + (key - prev_key) * t, FLOOR_DB));
            }
        }
        clipped.push((key, out));
    }
    clipped
}

/// The static response: key level across, output up, the hysteresis band
/// shaded, the rising branch solid and the falling one dashed where it
/// differs.
fn paint_response(window: &mut Window, cx: &mut App, b: Bounds<Pixels>, k: f32, p: &Params) {
    let (x0, y0) = plate::xy(b.origin);
    let at = |x: f32, y: f32| (x0 + x * k, y0 + y * k);
    let mono = plate::mono(FontWeight::NORMAL);
    for db in [-80.0, -60.0, -40.0, -20.0, 0.0] {
        let (x, y) = (response_x(db), response_y(db));
        paint_polyline(
            window,
            &[at(x, R_TOP), at(x, R_BOTTOM)],
            1.0,
            color(GRID),
            None,
        );
        paint_polyline(
            window,
            &[at(R_LEFT, y), at(R_RIGHT, y)],
            1.0,
            color(GRID),
            None,
        );
        let text = plate::signed(db).replace(".0", "");
        paint_text(
            window,
            cx,
            &text,
            mono.clone(),
            10.0 * k,
            color(AXIS_INK),
            at(x, R_BOTTOM + 6.0),
            Align::Center,
        );
        paint_text(
            window,
            cx,
            &text,
            mono.clone(),
            10.0 * k,
            color(AXIS_INK),
            at(R_LEFT - 6.0, y - 6.5),
            Align::Right,
        );
    }
    paint_polyline(
        window,
        &[
            at(response_x(FLOOR_DB), response_y(FLOOR_DB)),
            at(response_x(0.0), response_y(0.0)),
        ],
        1.0,
        color(0x3a3a3a),
        Some((3.0 * k, 3.0 * k)),
    );

    let dim = if p.power { 1.0 } else { 0.4 };
    let hysteresis = p.power && p.hysteresis_db > 0.05;
    if hysteresis {
        let left = response_x(p.threshold_db - p.hysteresis_db);
        let (bx, by) = at(left, R_TOP);
        paint_rect(
            window,
            (
                bx,
                by,
                (response_x(p.threshold_db) - left) * k,
                (R_BOTTOM - R_TOP) * k,
            ),
            color_a(ACCENT, 0.08 * dim),
        );
    }
    let to_px = |points: &[(f32, f32)]| -> Vec<(f32, f32)> {
        clip_to_floor(points)
            .into_iter()
            .map(|(key, out)| at(response_x(key), response_y(out)))
            .collect()
    };
    let rising = to_px(&rising_branch(p));
    let mut area = rising.clone();
    area.push(at(R_RIGHT, R_BOTTOM));
    area.push(at(R_LEFT, R_BOTTOM));
    paint_polygon(window, &area, color_a(ACCENT, 0.12 * dim));
    paint_polyline(window, &rising, 2.0 * k, color_a(ACCENT, dim), None);
    if hysteresis {
        paint_polyline(
            window,
            &to_px(&falling_branch(p)),
            1.5 * k,
            color_a(ACCENT, 0.7 * dim),
            Some((4.0 * k, 3.0 * k)),
        );
    }
    paint_text(
        window,
        cx,
        "key dB \u{2192}",
        plate::outfit(FontWeight::NORMAL),
        10.0 * k,
        color(AXIS_INK),
        at(R_RIGHT, R_BOTTOM + 20.0),
        Align::Right,
    );
}

/// The key against what leaves the gate at it, riding the response.
fn paint_operating_point(window: &mut Window, b: Bounds<Pixels>, k: f32, live: &Live, p: &Params) {
    let Some(frame) = live.frame else {
        return;
    };
    let key = frame.slot_in_peak[waygate::KEY_SLOT];
    if !p.power || key <= 1.0e-5 {
        return;
    }
    let (x0, y0) = plate::xy(b.origin);
    let key_db = level_db(key);
    let x = x0 + response_x(key_db) * k;
    let y = y0 + response_y(key_db - frame.gain_reduction_db.max(0.0)) * k;
    paint_polyline(
        window,
        &[(x, y0 + R_BOTTOM * k), (x, y)],
        1.0,
        color_a(0xffffff, 0.35),
        Some((3.0 * k, 3.0 * k)),
    );
    paint_dot(window, x, y, 5.0 * k, color(0x111111));
    paint_dot(window, x, y, 3.5 * k, color(WHITE));
}

// ── The deck ───────────────────────────────────────────────────────────────

fn controls(e: &KitEditor<WayGatePlate>, cx: &mut Cx<WayGatePlate>, s: f32) -> AnyElement {
    deck(s, 341.0, 420.0)
        .p(px(14.0 * s))
        .gap(px(12.0 * s))
        .child(detector_card(e, cx, s).w(px(310.0 * s)).flex_shrink_0())
        .child(timing_card(e, cx, s).w(px(264.0 * s)).flex_shrink_0())
        .child(key_card(e, cx, s).w(px(196.0 * s)).flex_shrink_0())
        .child(levels_card(e, s).flex_1())
        .into_any_element()
}

/// A knob cell for `dial`, its value sent through `to_param` — the identity
/// for every knob but Hold, whose knob turns over `√ms`.
#[allow(clippy::too_many_arguments)]
fn cell(
    cx: &mut Cx<WayGatePlate>,
    dial: Dial,
    value: f32,
    reading: String,
    look: KnobLook,
    cell: KnobCell,
    to_param: fn(f32) -> f32,
    s: f32,
) -> AnyElement {
    let id = dial.id;
    knob_reading(
        cx,
        format!("waygate-{id}"),
        dial,
        value,
        Some(reading),
        look,
        cell,
        accent(),
        true,
        s,
        move |this: &mut KitEditor<WayGatePlate>, value, cx| {
            this.set_value(id, to_param(value), cx)
        },
    )
}

fn same(value: f32) -> f32 {
    value
}

/// Hold's knob value, `√ms`, back to milliseconds, to a tenth.
fn hold_ms(root: f32) -> f32 {
    ((root * root) * 10.0).round() / 10.0
}

fn detector_card(e: &KitEditor<WayGatePlate>, cx: &mut Cx<WayGatePlate>, s: f32) -> gpui::Div {
    let p = &e.params;
    let d = dials();
    let modes: Vec<AnyElement> = [(Mode::Gate, "Gate"), (Mode::Duck, "Duck")]
        .into_iter()
        .enumerate()
        .map(|(index, (mode, label))| {
            pill(
                ("waygate-mode", index),
                label,
                p.mode == mode,
                accent(),
                PillLook::words(30.0, 0.0, 12.5),
                true,
                s,
                e.click_cb(cx, move |this, cx| {
                    this.set_value("mode", mode.to_wire(), cx)
                }),
            )
            .into_any_element()
        })
        .collect();
    let look = KnobLook::small(58.0).styled(3.6, 17.5, FACE, FACE_EDGE, 0.8, 7.0, 1.8);
    let k = KnobCell::new(92.0, 10.5, 12.0);
    card(s)
        .child(card_title("Detector", s))
        .child(
            section("Mode", s)
                .child(columns(3.0, s, modes))
                .child(note(mode_hint(p), s)),
        )
        .child(knob_row(vec![
            cell(
                cx,
                d.threshold,
                p.threshold_db,
                readout("thresholdDb", p.threshold_db),
                look,
                k,
                same,
                s,
            ),
            cell(
                cx,
                d.range,
                p.range_db,
                readout("rangeDb", p.range_db),
                look,
                k,
                same,
                s,
            ),
            cell(
                cx,
                d.hysteresis,
                p.hysteresis_db,
                readout("hysteresisDb", p.hysteresis_db),
                look,
                k,
                same,
                s,
            ),
        ]))
}

fn timing_card(e: &KitEditor<WayGatePlate>, cx: &mut Cx<WayGatePlate>, s: f32) -> gpui::Div {
    let p = &e.params;
    let d = dials();
    let look = KnobLook::small(50.0);
    let k = KnobCell::new(76.0, 10.0, 11.5);
    let lookahead_note = if p.lookahead_ms > 0.0 {
        "Delays the signal so the gate is open before a hit lands. Adds latency."
    } else {
        "Off: the gate adds no latency."
    };
    card(s)
        .child(card_title("Timing", s))
        .child(knob_row(vec![
            cell(
                cx,
                d.attack,
                p.attack_ms,
                readout("attackMs", p.attack_ms),
                look,
                k,
                same,
                s,
            ),
            cell(
                cx,
                d.hold,
                p.hold_ms.max(0.0).sqrt(),
                readout("holdMs", p.hold_ms),
                look,
                k,
                hold_ms,
                s,
            ),
            cell(
                cx,
                d.release,
                p.release_ms,
                readout("releaseMs", p.release_ms),
                look,
                k,
                same,
                s,
            ),
        ]))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.0 * s))
                .child(cell(
                    cx,
                    d.lookahead,
                    p.lookahead_ms,
                    readout("lookaheadMs", p.lookahead_ms),
                    look,
                    k,
                    same,
                    s,
                ))
                .child(div().flex_1().min_w(px(0.0)).child(note(lookahead_note, s))),
        )
}

fn key_card(e: &KitEditor<WayGatePlate>, cx: &mut Cx<WayGatePlate>, s: f32) -> gpui::Div {
    let p = &e.params;
    let d = dials();
    let look = KnobLook::small(50.0);
    let k = KnobCell::new(76.0, 10.0, 11.5);
    let link = switch(
        "waygate-stereo-link",
        p.stereo_link,
        accent(),
        true,
        s,
        e.click_cb(cx, |this, cx| this.toggle("stereoLink", cx)),
    );
    card(s)
        .child(card_title("Key filter", s))
        .child(note("Shapes only what the detector hears.", s))
        .child(knob_row(vec![
            cell(
                cx,
                d.key_hpf,
                p.key_hpf_hz,
                readout("keyHpfHz", p.key_hpf_hz),
                look,
                k,
                same,
                s,
            ),
            cell(
                cx,
                d.key_lpf,
                p.key_lpf_hz,
                readout("keyLpfHz", p.key_lpf_hz),
                look,
                k,
                same,
                s,
            ),
        ]))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(6.0 * s))
                .child(switch_row("Stereo link", link, s))
                .child(note(
                    if p.stereo_link {
                        "Both sides open together."
                    } else {
                        "Each side gates on its own."
                    },
                    s,
                )),
        )
}

fn levels_card(e: &KitEditor<WayGatePlate>, s: f32) -> gpui::Div {
    card(s).child(card_title("Levels", s)).child(
        div()
            .flex_1()
            .w_full()
            .min_h(px(0.0))
            .mt(px(10.0 * s))
            .child(live_slot(&e.live_bounds, SLOT_LEVELS).size_full()),
    )
}

/// The detector's state as a lamp and a word, then In, Key, the gate's gain
/// over its range, and Out.
fn paint_levels(
    window: &mut Window,
    cx: &mut App,
    b: Bounds<Pixels>,
    live: &Live,
    p: &Params,
    s: f32,
) {
    let (x, y) = plate::xy(b.origin);
    let (w, h) = (f32::from(b.size.width), f32::from(b.size.height));
    let bypassed = !p.power;
    let has = live.frame.is_some() && live.history_len() > 0;
    let triggered = has && !bypassed && live.history_at(0).slot_lit;
    let word = match (bypassed, p.mode, triggered) {
        (true, ..) => "Bypassed",
        (false, _, _) if !has => "No signal",
        (false, Mode::Gate, true) => "Open",
        (false, Mode::Gate, false) => "Closed",
        (false, Mode::Duck, true) => "Ducking",
        (false, Mode::Duck, false) => "Idle",
    };
    paint_dot(
        window,
        x + 6.0 * s,
        y + 9.0 * s,
        5.0 * s,
        if triggered { accent() } else { color(0x3e3e3e) },
    );
    paint_text(
        window,
        cx,
        word,
        plate::outfit(FontWeight::MEDIUM),
        13.0 * s,
        if triggered { color(WHITE) } else { color(SOFT) },
        (x + 18.0 * s, y + 0.5 * s),
        Align::Left,
    );

    let caption_y = y + 32.0 * s;
    let bar_top = caption_y + 18.0 * s;
    let bar_h = (y + h - 20.0 * s - bar_top).max(1.0);
    let bar_w = 10.0 * s;
    let column_w = w / 4.0;
    let unit = |db: f32| ((db - FLOOR_DB) / -FLOOR_DB).clamp(0.0, 1.0);
    let span = -p.range_db.clamp(FLOOR_DB, 0.0);
    let mono = plate::mono(FontWeight::NORMAL);
    for (i, label) in ["In", "Key", "Gain", "Out"].into_iter().enumerate() {
        let centre = x + column_w * (i as f32 + 0.5);
        paint_text(
            window,
            cx,
            &plate::tracked(label, 0.08),
            plate::outfit(FontWeight::MEDIUM),
            9.5 * s,
            color(SOFT),
            (centre, caption_y),
            Align::Center,
        );
        let left = centre - bar_w * 0.5;
        paint_rect(window, (left, bar_top, bar_w, bar_h), color(0x1a1a1a));
        let reading = if i == 2 {
            // The gain, hanging from the top over the range.
            let now = if has && !bypassed {
                live.history_at(0).reduction_db
            } else {
                0.0
            };
            let fill = if span > 0.05 {
                (now / span).clamp(0.0, 1.0)
            } else {
                0.0
            };
            paint_rect(window, (left, bar_top, bar_w, fill * bar_h), accent());
            if !has {
                "\u{2014}".to_string()
            } else if now < 0.05 {
                "0.0".to_string()
            } else if waygate::is_full_mute(p.range_db) && now >= span - 0.05 {
                "\u{2212}\u{221E}".to_string()
            } else {
                format!("\u{2212}{now:.1}")
            }
        } else {
            let read = |point: &HistoryPoint| match i {
                0 => point.in_db,
                1 => point.slot_in_db,
                _ => point.out_db,
            };
            let now = if has {
                read(&live.history_at(0))
            } else {
                -120.0
            };
            let held = if has { live.held(read) } else { -120.0 };
            let level = unit(now);
            let ink = if level >= 1.0 {
                accent()
            } else {
                color(0xd9d9d9)
            };
            paint_rect(
                window,
                (left, bar_top + (1.0 - level) * bar_h, bar_w, level * bar_h),
                ink,
            );
            let hold = unit(held);
            if hold > 0.0 {
                paint_rect(
                    window,
                    (left, bar_top + (1.0 - hold) * bar_h, bar_w, 1.5 * s),
                    color(WHITE),
                );
            }
            // The thresholds the key is held against, on In and Key.
            if i < 2 && !bypassed {
                let mut tick = |db: f32, strength: f32| {
                    let ty = bar_top + (1.0 - unit(db)) * bar_h;
                    paint_rect(
                        window,
                        (left - 4.0 * s, ty, bar_w + 8.0 * s, 1.0 * s.max(1.0)),
                        color_a(ACCENT, strength),
                    );
                };
                tick(p.threshold_db, 1.0);
                if p.hysteresis_db > 0.05 {
                    tick(p.threshold_db - p.hysteresis_db, 0.55);
                }
            }
            if !has || held <= -119.0 {
                "\u{2014}".to_string()
            } else {
                plate::signed(held)
            }
        };
        paint_text(
            window,
            cx,
            &reading,
            mono.clone(),
            10.0 * s,
            color(INK),
            (centre, bar_top + bar_h + 5.0 * s),
            Align::Center,
        );
    }
}

/// Opens with a track's WayGate insert.
pub fn open_waygate_editor(
    owner_bounds: Option<Bounds<Pixels>>,
    key: PluginInstanceKey,
    identity: ShellIdentity,
    host_ops: BuiltinEditorHostOps,
    on_close: Arc<dyn Fn(&mut Window, &mut App)>,
    cx: &mut App,
) -> Result<gpui::WindowHandle<WayGateWindow>, String> {
    plate::ensure_fonts(cx);
    open_kit_editor(
        WayGatePlate,
        owner_bounds,
        key,
        identity,
        host_ops,
        on_close,
        cx,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_dial_spans_its_descriptor_row() {
        let d = dials();
        for dial in [
            d.threshold,
            d.range,
            d.hysteresis,
            d.attack,
            d.release,
            d.key_hpf,
            d.key_lpf,
            d.lookahead,
        ] {
            let (min, max, default) = waygate::descriptor()
                .params
                .iter()
                .find(|row| row.id == dial.id)
                .map(|row| (row.min, row.max, row.default_value))
                .unwrap();
            assert_eq!((dial.min, dial.max), (min, max), "{}", dial.id);
            assert!(
                (dial.at(dial.norm(default)) - default).abs() <= 0.011 * default.abs().max(1.0),
                "{} default does not round-trip",
                dial.id
            );
            // The end stops are reachable.
            assert_eq!(dial.at(0.0), min, "{}", dial.id);
            assert_eq!(dial.at(1.0), max, "{}", dial.id);
        }
    }

    #[test]
    fn the_hold_knob_turns_over_the_root_of_its_range() {
        let hold = dials().hold;
        assert_eq!(hold_ms(hold.at(0.0)), 0.0);
        assert!((hold_ms(hold.at(1.0)) - 2_000.0).abs() < 1.0);
        let default = waygate::default_params().hold_ms;
        assert!((hold_ms(hold.default) - default).abs() < 0.2);
        // The short holds get most of the travel.
        assert!(hold.norm(100f32.sqrt()) > 0.2);
    }

    #[test]
    fn readouts_say_what_the_gate_does() {
        assert_eq!(readout("rangeDb", -80.0), "\u{2212}\u{221E} dB");
        assert_eq!(readout("rangeDb", -24.0), "\u{2212}24.0 dB");
        assert_eq!(readout("keyLpfHz", 20_000.0), "Off");
        assert_eq!(readout("keyLpfHz", 150.0), "150 Hz");
        assert_eq!(readout("keyHpfHz", 20.0), "Off");
        assert_eq!(readout("lookaheadMs", 0.0), "Off");
        assert_eq!(readout("hysteresisDb", 4.0), "4.0 dB");
        assert_eq!(readout("attackMs", 0.05), "0.05 ms");
        assert_eq!(readout("holdMs", 20.0), "20 ms");
    }

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
    fn the_branches_are_the_gates_law() {
        let p = waygate::default_params();
        let rising = rising_branch(&p);
        // Shut below the threshold, open above it.
        assert_eq!(rising[0].1, FLOOR_DB + waygate::RANGE_FLOOR_DB);
        assert_eq!(rising[2], (p.threshold_db, p.threshold_db));
        assert_eq!(rising[3], (0.0, 0.0));
        let falling = falling_branch(&p);
        let close = p.threshold_db - p.hysteresis_db;
        assert_eq!(falling[1], (close, close));
        // Ducking turns the law over.
        let duck = gate_model::with(&p, "mode", 1.0);
        assert_eq!(rising_branch(&duck)[0], (FLOOR_DB, FLOOR_DB));
    }

    #[test]
    fn a_branch_leaving_the_display_is_cut_at_the_floor() {
        // Shut by 40 dB: the closed branch meets the floor at a −40 dB key.
        let clipped = clip_to_floor(&[(-80.0, -120.0), (-30.0, -70.0), (-30.0, -30.0)]);
        assert_eq!(clipped.len(), 4);
        assert!((clipped[1].0 + 40.0).abs() < 1.0e-3);
        assert_eq!(clipped[1].1, FLOOR_DB);
        assert_eq!(clip_to_floor(&[(-60.0, -60.0), (0.0, 0.0)]).len(), 2);
    }

    #[test]
    fn the_subtitle_names_the_mode_and_the_lookahead() {
        let params = waygate::default_params();
        assert_eq!(WayGatePlate.subtitle(&params), "Noise gate");
        let ahead = gate_model::with(&gate_model::with(&params, "mode", 1.0), "lookaheadMs", 5.0);
        assert_eq!(
            WayGatePlate.subtitle(&ahead),
            "Ducker \u{00B7} 5.0 ms lookahead"
        );
    }
}

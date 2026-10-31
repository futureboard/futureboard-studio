//! Duckstep's editor, after its design: a Sidechain / Shaper switch in the
//! header; the ducker's level history or the shaper's curve in the well,
//! with In / GR / Out meters beside it; Detector and Envelope, or Trigger
//! and Shape, then Output on the deck.
//!
//! Everything moving is real: the history is the host's meter frames (the
//! detector rides slot 0's input peak), the playhead, the trigger light and
//! the tempo come from `duckstep::telemetry`, and the curve is
//! `duckstep::shaper_gain`, the function the DSP runs. What the host cannot
//! do yet — a sidechain key, MIDI into an effect insert — is shown disabled
//! with the reason.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use duckstep::{
    Band, Feel, MAX_POINTS, Mode, Monitor, Params, Rate, ReleaseSync, SHAPE_PRESETS, Source,
    Trigger, shape_at, shape_position, shaper_gain, telemetry,
};
use gpui::{
    Anchor, AnyElement, App, AppContext, ElementId, FontWeight, InteractiveElement, IntoElement,
    MouseButton, MouseMoveEvent, ParentElement, Rgba, ScrollWheelEvent, SharedString,
    StatefulInteractiveElement, Styled, Window, anchored, canvas, deferred, div, point, px,
};

use crate::components::builtin_plugin_editor::builtin_duckstep_params;
use crate::components::builtin_plugin_editor_window::{BuiltinEditorHostOps, PluginInstanceKey};
use crate::components::fader::{FaderDrag, drag_to, is_reset_click};
use crate::components::fx_model::KnobSpec;
use crate::components::native_plugin_shell::ShellIdentity;
use crate::components::plate::{
    self, Align, CARD_EDGE, Dial, DialUnit, GROUND, ICON, INK, KnobLook, OFF, PALE, SOFT,
    WELL_EDGE, WHITE, color, color_a, deck, icon, live_slot, numbers, paint_dot, paint_polygon,
    paint_polyline, paint_text, plate_frame, plate_scale, square, type_px, well, words,
};
use crate::components::plugin_kit::{
    Cx, KitEditor, KitModel, KitPreset, KitWindow, LiveBounds, LiveSlot, bank, open_kit_editor,
};
use crate::components::plugin_live::{HistoryPoint, Live};

/// Duckstep's accent: gain reduction only. Selection is a light fill.
pub const ACCENT: u32 = 0xa78bfa;

/// The design's selected fill and its text.
const LIGHT: u32 = 0xe6e6e6;
const DARK: u32 = 0x111111;
/// Help lines and the legend.
const HELP: u32 = 0xa6a6a6;
const LEGEND: u32 = 0x9a9a9a;
/// The displays' ground and rim.
const SCREEN: u32 = 0x0d0d0d;
const SCREEN_EDGE: u32 = 0x242424;
const DIVIDER: u32 = 0x2a2a2a;
/// The deck's knobs' arc.
const KNOB_INK: u32 = 0xd6d6d6;

/// The displays' view box.
const GW: f32 = 820.0;
const GH: f32 = 244.0;
/// History frames across the sidechain display: about four seconds at the
/// telemetry rate.
const SHOWN: usize = 120;

const SLOT_GRAPH: usize = 0;
/// In, GR, Out bars; then their readings.
const SLOT_BAR: usize = 1;
const SLOT_VALUE: usize = 4;
const SLOT_GR_NOW: usize = 7;
const SLOT_STATUS: usize = 8;
const SLOT_LED: usize = 9;
const SLOT_ENV: usize = 10;

/// The headphones on the Listen button, in a 12 × 12 box.
const HEADPHONES: &str =
    "M2 8 V6.5 A4 4 0 0 1 10 6.5 V8 M1.8 7.2 H3.4 V10.2 H1.8 Z M8.6 7.2 H10.2 V10.2 H8.6 Z";

const KNOB: KnobLook = KnobLook::small(56.0).styled(4.0, 17.0, 0x242424, 0x4a4a4a, 1.0, 7.0, 2.0);

fn accent() -> Rgba {
    color(ACCENT)
}

// ── Readouts, as the design prints them ────────────────────────────────────

fn f_hz(v: f32) -> String {
    if v < 1_000.0 {
        format!("{} Hz", v.round())
    } else if v < 10_000.0 {
        format!("{:.1} kHz", v / 1_000.0)
    } else {
        format!("{:.0} kHz", v / 1_000.0)
    }
}

fn f_ratio(v: f32) -> String {
    if v < 10.0 {
        format!("{v:.1}:1")
    } else {
        format!("{}:1", v.round())
    }
}

fn f_range(v: f32) -> String {
    format!("\u{2212}{} dB", v.round())
}

fn f_pct(v: f32) -> String {
    format!("{}%", v.round())
}

fn f_bi(v: f32) -> String {
    let sign = if v > 0.5 {
        "+"
    } else if v < -0.5 {
        "\u{2212}"
    } else {
        ""
    };
    format!("{sign}{}%", v.round().abs())
}

fn f_knee(v: f32) -> String {
    format!("{v:.1} dB")
}

/// A deck knob: its param and how its value reads.
#[derive(Clone, Copy)]
struct Knob {
    dial: Dial,
    text: fn(f32) -> String,
}

const fn knob_def(dial: Dial, text: fn(f32) -> String) -> Knob {
    Knob { dial, text }
}

const THRESHOLD: Knob = knob_def(
    Dial::new("thresholdDb", "Threshold", -60.0, 0.0, -24.0, DialUnit::Db),
    plate::fmt_db,
);
const RATIO: Knob = knob_def(
    Dial::new("ratio", "Ratio", 1.0, 20.0, 8.0, DialUnit::Ratio).log(),
    f_ratio,
);
const KNEE: Knob = knob_def(
    Dial::new("kneeDb", "Knee", 0.0, 24.0, 6.0, DialUnit::Db),
    f_knee,
);
const SC_HPF: Knob = knob_def(
    Dial::new(
        "scHpfHz",
        "SC high-pass",
        20.0,
        500.0,
        30.0,
        DialUnit::HzRound,
    )
    .log(),
    f_hz,
);
const SC_LPF: Knob = knob_def(
    Dial::new(
        "scLpfHz",
        "SC low-pass",
        100.0,
        20_000.0,
        20_000.0,
        DialUnit::HzRound,
    )
    .log(),
    f_hz,
);
const ATTACK: Knob = knob_def(
    Dial::new("attackMs", "Attack", 0.1, 50.0, 1.0, DialUnit::Ms).log(),
    plate::fmt_ms,
);
const HOLD: Knob = knob_def(
    Dial::new("holdMs", "Hold", 0.0, 200.0, 20.0, DialUnit::Ms),
    plate::fmt_ms,
);
const RELEASE: Knob = knob_def(
    Dial::new("releaseMs", "Release", 10.0, 1_000.0, 180.0, DialUnit::Ms).log(),
    plate::fmt_ms,
);
const LOOKAHEAD: Knob = knob_def(
    Dial::new("lookaheadMs", "Lookahead", 0.0, 10.0, 2.0, DialUnit::Ms),
    plate::fmt_ms,
);
const RANGE: Knob = knob_def(
    Dial::new("rangeDb", "Range", 0.0, 48.0, 24.0, DialUnit::Db),
    f_range,
);
const DEPTH: Knob = knob_def(
    Dial::new("depth", "Depth", 0.0, 100.0, 100.0, DialUnit::Percent),
    f_pct,
);
const CURVE: Knob = knob_def(
    Dial::new("curve", "Curve", -100.0, 100.0, 45.0, DialUnit::Percent).bipolar(),
    f_bi,
);
const SMOOTH: Knob = knob_def(
    Dial::new("smoothMs", "Smooth", 0.0, 50.0, 4.0, DialUnit::Ms),
    plate::fmt_ms,
);
const OFFSET: Knob = knob_def(
    Dial::new("offset", "Offset", -50.0, 50.0, 0.0, DialUnit::Percent).bipolar(),
    f_bi,
);
const SHAPER_THRESHOLD: Knob = knob_def(
    Dial::new(
        "shaperThresholdDb",
        "Threshold",
        -60.0,
        0.0,
        -20.0,
        DialUnit::Db,
    ),
    plate::fmt_db,
);
const XOVER: Knob = knob_def(
    Dial::new(
        "xoverHz",
        "Crossover",
        40.0,
        2_000.0,
        150.0,
        DialUnit::HzRound,
    )
    .log(),
    f_hz,
);
const MIX: Knob = knob_def(
    Dial::new("mix", "Mix", 0.0, 100.0, 100.0, DialUnit::Percent),
    f_pct,
);
const OUTPUT: Knob = knob_def(
    Dial::new("outputDb", "Output", -12.0, 12.0, 0.0, DialUnit::Db).bipolar(),
    plate::fmt_db,
);

/// The sidechain display's level scale: 0 dB near the top, −60 at the foot.
fn db_y(db: f32) -> f32 {
    6.0 + (-db / 60.0).clamp(0.0, 1.0) * (GH - 12.0)
}

/// The shaper display's gain scale.
fn gain_y(v: f32) -> f32 {
    (GH - 10.0) - v * (GH - 20.0)
}

fn rate_label(params: &Params) -> String {
    let feel = match params.feel {
        Feel::Straight => "",
        Feel::Dotted => "D",
        Feel::Triplet => "T",
    };
    format!("{}{feel}", params.rate.label())
}

// ── The model ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DuckstepPlate;

/// A Duckstep insert's editor window.
pub type DuckstepWindow = KitWindow<DuckstepPlate>;

/// What a drag on the well moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Drag {
    Threshold,
    Point(usize),
}

/// The displays as they were when Pause was pressed.
struct Frozen {
    history: Vec<HistoryPoint>,
    phase: Option<f32>,
}

#[derive(Default)]
pub struct DuckUi {
    drag: Option<Drag>,
    paused: bool,
    frozen: Rc<RefCell<Option<Frozen>>>,
}

#[derive(Clone)]
pub struct DuckView {
    params: Params,
    paused: bool,
    frozen: Rc<RefCell<Option<Frozen>>>,
}

impl KitModel for DuckstepPlate {
    type Params = Params;
    type Ui = DuckUi;
    type LiveView = DuckView;

    fn title(self) -> &'static str {
        "Duckstep"
    }

    fn subtitle(self, params: &Params) -> String {
        params.mode.label().to_uppercase()
    }

    fn key(self) -> &'static str {
        "duckstep-plate"
    }

    fn defaults(self) -> Params {
        duckstep::default_params()
    }

    fn from_mirror(self, insert_id: &str) -> Option<Params> {
        builtin_duckstep_params(insert_id)
    }

    fn wire_values(self, params: &Params) -> Vec<(u32, f32)> {
        duckstep::ipc::ui_values(params)
            .into_iter()
            .filter_map(|(id, value)| duckstep::ui_param_index(id).map(|index| (index, value)))
            .collect()
    }

    fn with(self, params: &Params, id: &str, value: f32) -> Params {
        let mut next = params.clone();
        duckstep::ipc::apply_ui_param(&mut next, id, value);
        next
    }

    fn value(self, params: &Params, id: &str) -> f32 {
        duckstep::ipc::ui_values(params)
            .into_iter()
            .find(|(candidate, _)| *candidate == id)
            .map_or(0.0, |(_, value)| value)
    }

    fn presets(self) -> Arc<Vec<KitPreset<Params>>> {
        static BANK: std::sync::OnceLock<Arc<Vec<KitPreset<Params>>>> = std::sync::OnceLock::new();
        BANK.get_or_init(|| bank(duckstep::factory_presets(), |p| (p.name, p.params)))
            .clone()
    }

    /// Listening state stays as it is: power and Listen.
    fn preset_applied(self, current: &Params, preset: &Params) -> Params {
        let mut next = preset.clone();
        next.power = current.power;
        next.listen = current.listen;
        next
    }

    fn knob(self, _id: &str) -> Option<KnobSpec> {
        None
    }

    fn panel(e: &KitEditor<Self>, cx: &mut Cx<Self>) -> AnyElement {
        let s = plate_scale(e.viewport);
        plate_frame(s, vec![display(e, cx, s), controls(e, cx, s)])
    }

    fn live_view(e: &KitEditor<Self>) -> DuckView {
        DuckView {
            params: e.params.clone(),
            paused: e.ui.paused,
            frozen: e.ui.frozen.clone(),
        }
    }

    fn paint_live(
        view: &DuckView,
        live: &mut Live,
        bounds: &LiveBounds,
        window: &mut Window,
        cx: &mut App,
    ) {
        let viewport = window.viewport_size();
        let s = plate_scale((f32::from(viewport.width), f32::from(viewport.height)));
        let p = &view.params;
        let pads = live.pads().copied();
        let live_phase = pads.map(|block| block[telemetry::PHASE]);
        let (history, phase) = if view.paused {
            let mut frozen = view.frozen.borrow_mut();
            let frozen = frozen.get_or_insert_with(|| Frozen {
                history: recent_history(live),
                phase: live_phase,
            });
            (frozen.history.clone(), frozen.phase)
        } else {
            view.frozen.borrow_mut().take();
            (recent_history(live), live_phase)
        };

        if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_GRAPH)) {
            match p.mode {
                Mode::Sidechain => paint_sidechain(window, p, &history, b),
                Mode::Shaper => paint_shaper(window, p, phase, b),
            }
        }

        let mono = plate::mono(FontWeight::NORMAL);
        let gr = live
            .frame
            .map(|frame| frame.gain_reduction_db.max(0.0))
            .unwrap_or(0.0);
        if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_GR_NOW)) {
            if live.frame.is_some() {
                paint_text(
                    window,
                    cx,
                    &format!("GR {}", plate::fmt_db(-gr)),
                    mono.clone(),
                    12.0 * s,
                    accent(),
                    plate::xy(b.origin),
                    Align::Left,
                );
            }
        }
        if let Some(frame) = live.frame {
            let in_db = db20(frame.in_peak);
            let out_db = db20(frame.out_peak);
            let bars = [
                (in_db, color(0x9a9a9a), false, INK),
                (-gr, accent(), true, ACCENT),
                (out_db, color(LIGHT), false, WHITE),
            ];
            for (index, (db, ink, hangs, text_ink)) in bars.into_iter().enumerate() {
                if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_BAR + index)) {
                    paint_bar(window, b, db, ink, hangs, s);
                }
                if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_VALUE + index)) {
                    let (x, y) = plate::xy(b.center());
                    paint_text(
                        window,
                        cx,
                        &plate::signed(db.max(-99.9)),
                        mono.clone(),
                        10.5 * s,
                        color(text_ink),
                        (x, y - 7.0 * s),
                        Align::Center,
                    );
                }
            }
        }
        if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_STATUS)) {
            if let Some(text) = status_text(p, pads.as_ref()) {
                let (x, y) = plate::xy(b.origin);
                paint_text(
                    window,
                    cx,
                    &text,
                    mono.clone(),
                    11.0 * s,
                    color(LEGEND),
                    (x + f32::from(b.size.width), y),
                    Align::Right,
                );
            }
        }
        if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_LED)) {
            let lit = pads.is_some_and(|block| block[telemetry::TRIGGER] > 0.3);
            let (x, y) = plate::xy(b.center());
            paint_dot(
                window,
                x,
                y,
                4.0 * s,
                color(if lit { WHITE } else { 0x4a4a4a }),
            );
        }
        if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_ENV)) {
            let tempo = pads
                .map(|block| block[telemetry::TEMPO_BPM])
                .filter(|tempo| *tempo > 0.0)
                .unwrap_or(duckstep::FALLBACK_TEMPO_BPM);
            paint_envelope(window, cx, p, tempo, b, s);
        }
    }

    fn window_size(self) -> (f32, f32) {
        plate::WINDOW_SIZE
    }

    fn min_size(self) -> (f32, f32) {
        plate::MIN_WINDOW_SIZE
    }

    fn mouse_move(e: &mut KitEditor<Self>, event: &MouseMoveEvent, cx: &mut Cx<Self>) {
        let Some(drag) = e.ui.drag else {
            return;
        };
        if event.pressed_button != Some(MouseButton::Left) {
            e.ui.drag = None;
            return;
        }
        let Some(b) = e.live_bounds.get(LiveSlot::Plate(SLOT_GRAPH)) else {
            return;
        };
        let (fx, fy) = graph_fraction(b, plate::xy(event.position));
        match drag {
            Drag::Threshold => {
                let units = fy * GH;
                let db = -((units - 6.0) / (GH - 12.0)) * 60.0;
                let db = DialUnit::Db.round(db).clamp(-60.0, 0.0);
                e.set_value("thresholdDb", db, cx);
            }
            Drag::Point(index) => {
                let mut next = e.params.clone();
                let count = (next.point_count as usize).clamp(2, MAX_POINTS);
                if index >= count {
                    return;
                }
                let y = 1.0 - ((fy * GH - 10.0) / (GH - 20.0)).clamp(0.0, 1.0);
                next.point_y[index] = (y * 1_000.0).round() / 1_000.0;
                if index > 0 && index < count - 1 {
                    let x = (fx * 1_000.0).round() / 1_000.0;
                    next.point_x[index] = x.clamp(
                        next.point_x[index - 1] + 0.005,
                        next.point_x[index + 1] - 0.005,
                    );
                }
                e.set_params(next, cx);
            }
        }
    }

    fn mouse_up(e: &mut KitEditor<Self>, _cx: &mut Cx<Self>) {
        e.ui.drag = None;
    }
}

fn db20(v: f32) -> f32 {
    20.0 * v.max(1.0e-9).log10()
}

/// The newest history frames, oldest first.
fn recent_history(live: &Live) -> Vec<HistoryPoint> {
    let count = live.history_len().min(SHOWN);
    (0..count).rev().map(|age| live.history_at(age)).collect()
}

/// A window point as a share of the display's width and height.
fn graph_fraction(b: gpui::Bounds<gpui::Pixels>, (x, y): (f32, f32)) -> (f32, f32) {
    let (bx, by) = plate::xy(b.origin);
    let w = f32::from(b.size.width).max(1.0);
    let h = f32::from(b.size.height).max(1.0);
    (
        ((x - bx) / w).clamp(0.0, 1.0),
        ((y - by) / h).clamp(0.0, 1.0),
    )
}

/// The line under the display: what drives the gain, at the host's tempo.
fn status_text(p: &Params, pads: Option<&[f32; telemetry::SLOTS]>) -> Option<String> {
    let block = pads?;
    if !p.power {
        return Some("Bypassed".into());
    }
    let tempo = block[telemetry::TEMPO_BPM];
    let tempo = if tempo > 0.0 {
        format!("{} BPM", (tempo * 10.0).round() / 10.0)
    } else {
        "No tempo yet".into()
    };
    Some(match p.mode {
        Mode::Sidechain => format!("Self-detect \u{00B7} {tempo}"),
        Mode::Shaper => {
            let trigger = match p.trigger {
                Trigger::Host if block[telemetry::PLAYING] >= 0.5 => "Host",
                Trigger::Host => "Host \u{00B7} stopped",
                Trigger::Midi => "Free-running",
                Trigger::Audio => "Audio",
            };
            format!("{tempo} \u{00B7} {} \u{00B7} {trigger}", rate_label(p))
        }
    })
}

// ── Painting the live layers ───────────────────────────────────────────────

/// The display's design units to window points.
fn graph_at(b: gpui::Bounds<gpui::Pixels>) -> impl Fn(f32, f32) -> (f32, f32) {
    let (bx, by) = plate::xy(b.origin);
    let kx = f32::from(b.size.width) / GW;
    let ky = f32::from(b.size.height) / GH;
    move |x, y| (bx + x * kx, by + y * ky)
}

fn paint_sidechain(
    window: &mut Window,
    p: &Params,
    history: &[HistoryPoint],
    b: gpui::Bounds<gpui::Pixels>,
) {
    let at = graph_at(b);
    for i in 1..8 {
        let x = i as f32 * GW / 8.0;
        let ink = color(if i % 4 == 0 { 0x262626 } else { 0x181818 });
        paint_polyline(window, &[at(x, 0.0), at(x, GH)], 1.0, ink, None);
    }
    for db in [-12.0, -24.0, -36.0, -48.0] {
        let y = db_y(db);
        paint_polyline(window, &[at(0.0, y), at(GW, y)], 1.0, color(0x262626), None);
    }
    let listening = p.listen;
    if history.len() >= 2 {
        let step = GW / (SHOWN - 1) as f32;
        let n = history.len();
        let x_of = |i: usize| GW - (n - 1 - i) as f32 * step;
        let line = |read: fn(&HistoryPoint) -> f32| -> Vec<(f32, f32)> {
            history
                .iter()
                .enumerate()
                .map(|(i, point)| at(x_of(i), db_y(read(point))))
                .collect()
        };
        let out = line(|point| point.out_db);
        let detector = line(|point| point.slot_in_db);
        let reduction = line(|point| -point.reduction_db.max(0.0));
        let (first, last) = (x_of(0), x_of(n - 1));

        let mut fill = out.clone();
        fill.push(at(last, GH));
        fill.push(at(first, GH));
        paint_polygon(
            window,
            &fill,
            color_a(LIGHT, if listening { 0.03 } else { 0.08 }),
        );
        paint_polyline(
            window,
            &out,
            1.3,
            color_a(0xd6d6d6, if listening { 0.35 } else { 1.0 }),
            None,
        );
        paint_polyline(
            window,
            &detector,
            if listening { 1.6 } else { 1.0 },
            color(if listening { LIGHT } else { 0x8a8a8a }),
            None,
        );
        let mut hang = reduction.clone();
        hang.push(at(last, 0.0));
        hang.push(at(first, 0.0));
        paint_polygon(window, &hang, color_a(ACCENT, 0.22));
        paint_polyline(window, &reduction, 2.0, accent(), None);
    }
    let y = db_y(p.threshold_db);
    paint_polyline(
        window,
        &[at(0.0, y), at(GW, y)],
        1.0,
        color_a(WHITE, 0.55),
        Some((5.0, 4.0)),
    );
}

fn paint_shaper(
    window: &mut Window,
    p: &Params,
    phase: Option<f32>,
    b: gpui::Bounds<gpui::Pixels>,
) {
    let at = graph_at(b);
    for i in 1..16 {
        let x = i as f32 * GW / 16.0;
        let ink = color(if i % 4 == 0 { 0x262626 } else { 0x181818 });
        paint_polyline(window, &[at(x, 0.0), at(x, GH)], 1.0, ink, None);
    }
    for v in [0.0, 0.5, 1.0] {
        let y = gain_y(v);
        paint_polyline(window, &[at(0.0, y), at(GW, y)], 1.0, color(0x262626), None);
    }
    let samples = 205;
    let curve = |f: &dyn Fn(f32) -> f32| -> Vec<(f32, f32)> {
        (0..=samples)
            .map(|i| {
                let x = i as f32 / samples as f32;
                at(x * GW, gain_y(f(x)))
            })
            .collect()
    };
    if p.depth < 99.5 {
        let raw = curve(&|x| shape_at(p, x));
        paint_polyline(window, &raw, 1.0, color(0x6a6a6a), Some((4.0, 3.0)));
    }
    let shape = curve(&|x| shaper_gain(p, x));
    paint_polyline(window, &shape, 2.2, accent(), None);

    let restart = shape_position(p, 0.0) * GW;
    paint_polyline(
        window,
        &[at(restart, 0.0), at(restart, GH)],
        1.0,
        color_a(LIGHT, 0.5),
        Some((2.0, 3.0)),
    );
    if let Some(phase) = phase {
        let x = shape_position(p, phase);
        paint_polyline(
            window,
            &[at(x * GW, 0.0), at(x * GW, GH)],
            1.0,
            color_a(WHITE, 0.35),
            None,
        );
        let (dx, dy) = at(x * GW, gain_y(shaper_gain(p, x)));
        let k = f32::from(b.size.width) / GW;
        paint_dot(window, dx, dy, 4.5 * k, color(WHITE));
    }
    let k = f32::from(b.size.width) / GW;
    for (x, y) in p.points() {
        let (cx, cy) = at(x * GW, gain_y(y));
        paint_dot(window, cx, cy, 5.5 * k, accent());
        paint_dot(window, cx, cy, 3.5 * k, color(DARK));
    }
}

/// A vertical meter: from the foot up over −60…0 dB, or hanging from the
/// top by the reduction.
fn paint_bar(
    window: &mut Window,
    b: gpui::Bounds<gpui::Pixels>,
    db: f32,
    ink: Rgba,
    hangs: bool,
    s: f32,
) {
    let (x, y) = plate::xy(b.origin);
    let w = f32::from(b.size.width);
    let h = f32::from(b.size.height);
    window.paint_quad(gpui::fill(plate::rect(x, y, w, h), color(GROUND)).corner_radii(px(2.0 * s)));
    if hangs {
        let fill = (-db / 60.0).clamp(0.0, 1.0) * h;
        plate::paint_rect(window, (x, y, w, fill), ink);
    } else {
        let fill = ((db + 60.0) / 60.0).clamp(0.0, 1.0) * h;
        plate::paint_rect(window, (x, y + h - fill, w, fill), ink);
    }
}

/// The attack / hold / release sketch, in its 373 × 50 box.
fn paint_envelope(
    window: &mut Window,
    cx: &mut App,
    p: &Params,
    tempo: f32,
    b: gpui::Bounds<gpui::Pixels>,
    s: f32,
) {
    let (bx, by) = plate::xy(b.origin);
    let kx = f32::from(b.size.width) / 373.0;
    let ky = f32::from(b.size.height) / 50.0;
    let at = |x: f32, y: f32| (bx + x * kx, by + y * ky);
    let attack = p.attack_ms.max(0.05);
    let release = p.release_at(tempo).max(1.0);
    let total = attack * 1.2 + p.hold_ms + release * 4.0;
    let ex = |ms: f32| 6.0 + ms / total * 361.0;
    let line: Vec<(f32, f32)> = (0..=120)
        .map(|i| {
            let ms = i as f32 / 120.0 * total;
            let v = if ms < attack * 1.2 {
                1.0 - (-3.0 * ms / attack).exp()
            } else if ms < attack * 1.2 + p.hold_ms {
                0.97
            } else {
                0.97 * (-(ms - attack * 1.2 - p.hold_ms) / release).exp()
            };
            at(ex(ms), 44.0 - v * 36.0)
        })
        .collect();
    let (xa, xh) = (ex(attack * 1.2), ex(attack * 1.2 + p.hold_ms));
    for x in [xa, xh] {
        paint_polyline(
            window,
            &[at(x, 4.0), at(x, 46.0)],
            1.0,
            color(0x4a4a4a),
            None,
        );
    }
    let mut fill = line.clone();
    fill.push(at(ex(total), 44.0));
    fill.push(at(6.0, 44.0));
    paint_polygon(window, &fill, color_a(LIGHT, 0.12));
    paint_polyline(window, &line, 1.6 * s, color(LIGHT), None);
    let tag = |x: f32| bx + (x / 373.0).clamp(0.0, 0.94) * f32::from(b.size.width);
    let font = plate::outfit(FontWeight::SEMIBOLD);
    let y = by + f32::from(b.size.height) - (3.0 + 12.5) * s;
    for (text, x) in [
        ("A", (xa / 2.0 - 3.0).max(6.0)),
        ("H", (xa + xh) / 2.0 - 3.0),
        ("R", xh + 8.0),
    ] {
        paint_text(
            window,
            cx,
            text,
            font.clone(),
            10.0 * s,
            color(HELP),
            (tag(x), y),
            Align::Left,
        );
    }
}

// ── Shared pieces ──────────────────────────────────────────────────────────

/// A choice button the design's way: selected is a light fill with dark
/// text. Greyed and inert when not `enabled`.
#[allow(clippy::too_many_arguments)]
fn choice(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    on: bool,
    h: f32,
    size: f32,
    mono: bool,
    enabled: bool,
    s: f32,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let (bg, border, fg) = if on {
        (LIGHT, LIGHT, DARK)
    } else {
        (0x2c2c2c, 0x4a4a4a, 0xd0d0d0)
    };
    let mut base = div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .justify_center()
        .h(px(h * s))
        .rounded(px(3.0 * s))
        .border(px(1.0))
        .border_color(color(border))
        .bg(color(bg))
        .text_color(color(fg))
        .text_size(type_px(size, s))
        .child(label.into());
    if mono {
        base = base.font(plate::mono(FontWeight::NORMAL));
    }
    if !enabled {
        return base.opacity(0.4);
    }
    let hover = if on { 0xf4f4f4 } else { 0x363636 };
    base.cursor(gpui::CursorStyle::PointingHand)
        .hover(move |style| style.bg(color(hover)))
        .on_click(on_click)
}

/// `cells` in rows of `cols` equal columns, `row_gap` apart.
fn grid(cols: usize, gap: f32, row_gap: f32, cells: Vec<AnyElement>, s: f32) -> gpui::Div {
    let mut rows = div().flex().flex_col().w_full().gap(px(row_gap * s));
    let mut cells = cells.into_iter().peekable();
    while cells.peek().is_some() {
        let mut row: Vec<AnyElement> = cells.by_ref().take(cols).collect();
        while row.len() < cols {
            row.push(div().into_any_element());
        }
        rows = rows.child(plate::columns(gap, s, row));
    }
    rows
}

/// One of the deck's sections: a 376-tall column, a rule on its left.
fn section(left: f32, width: f32, divider: bool, s: f32) -> gpui::Div {
    let base = div()
        .absolute()
        .left(px(left * s))
        .top(px(22.0 * s))
        .w(px(width * s))
        .h(px(376.0 * s))
        .px(px(22.0 * s))
        .flex()
        .flex_col()
        .gap(px(22.0 * s));
    if divider {
        base.border_l(px(1.0)).border_color(color(DIVIDER))
    } else {
        base
    }
}

fn section_head(title: &str, right: Option<AnyElement>, s: f32) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .h(px(22.0 * s))
        .child(
            words(title.to_string(), 14.0, WHITE, s)
                .font_weight(FontWeight::SEMIBOLD)
                .line_height(px(22.0 * s)),
        )
        .children(right)
}

fn label(text: &str, s: f32) -> gpui::Div {
    words(text.to_string(), 11.5, SOFT, s).font_weight(FontWeight::MEDIUM)
}

fn help(text: impl Into<SharedString>, s: f32) -> gpui::Div {
    words(text, 11.0, HELP, s)
}

/// A deck knob cell: label, 56 px knob, value. Dragging up turns it up
/// (Shift for fine), the wheel nudges it, a double-click resets it. Greyed
/// and inert when not `enabled`.
fn dial(cx: &mut Cx<DuckstepPlate>, knob: Knob, value: f32, enabled: bool, s: f32) -> AnyElement {
    let dial = knob.dial;
    let key: SharedString = format!("duckstep-{}", dial.id).into();
    let n = dial.norm(value);
    let face = canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            plate::paint_knob(window, bounds, n, dial.bipolar, KNOB, color(KNOB_INK))
        },
    )
    .size_full();
    let mut button = div()
        .id(ElementId::Name(key.clone()))
        .size(px(56.0 * s))
        .flex_shrink_0()
        .child(face);
    if enabled {
        let entity = cx.entity().clone();
        let on_drag = entity.clone();
        let on_wheel = entity.clone();
        let drag_id = key.to_string();
        button = button
            .cursor(gpui::CursorStyle::ResizeUpDown)
            .on_drag(FaderDrag::new(drag_id.clone()), |drag, _, _, cx| {
                cx.new(|_| FaderDrag::new(drag.id.clone()))
            })
            .on_drag_move::<FaderDrag>(move |event, _window, app| {
                let drag = event.drag(app);
                if drag.id != drag_id {
                    return;
                }
                let y = f32::from(event.event.position.y);
                let next = drag_to(drag, n, y, event.event.modifiers.shift, 180.0 * s, |dy| -dy);
                let _ = on_drag.update(app, |this, cx| this.set_value(dial.id, dial.at(next), cx));
            })
            .on_scroll_wheel(move |event: &ScrollWheelEvent, _window, app| {
                let dy = f32::from(event.delta.pixel_delta(px(20.0)).y);
                if dy == 0.0 {
                    return;
                }
                let next = plate::wheel_turn(dial, value, dy, event.modifiers.shift);
                let _ = on_wheel.update(app, |this, cx| this.set_value(dial.id, next, cx));
            })
            .on_click(move |event, _window, app| {
                if is_reset_click(event) {
                    let _ =
                        entity.update(app, |this, cx| this.set_value(dial.id, dial.default, cx));
                }
            });
    }
    let cell = div()
        .flex()
        .flex_col()
        .items_center()
        .gap(px(3.0 * s))
        .child(label(dial.label, s))
        .child(button)
        .child(numbers((knob.text)(value), 12.0, WHITE, s));
    if enabled {
        cell.into_any_element()
    } else {
        cell.opacity(0.4).into_any_element()
    }
}

/// A row of choice buttons for the choice param `param`.
#[allow(clippy::too_many_arguments)]
fn choices<T: Copy + PartialEq + 'static>(
    e: &KitEditor<DuckstepPlate>,
    cx: &mut Cx<DuckstepPlate>,
    id: &'static str,
    param: &'static str,
    current: T,
    options: &[(T, &'static str, bool)],
    wire: fn(T) -> f32,
    (h, size, mono): (f32, f32, bool),
    s: f32,
) -> gpui::Div {
    let cells: Vec<AnyElement> = options
        .iter()
        .enumerate()
        .map(|(index, &(option, text, enabled))| {
            let value = wire(option);
            choice(
                (id, index),
                text,
                option == current,
                h,
                size,
                mono,
                enabled,
                s,
                e.click_cb(cx, move |this, cx| this.set_value(param, value, cx)),
            )
            .into_any_element()
        })
        .collect();
    plate::columns(3.0, s, cells)
}

// ── The header ─────────────────────────────────────────────────────────────

fn header(e: &KitEditor<DuckstepPlate>, cx: &mut Cx<DuckstepPlate>, s: f32) -> AnyElement {
    let key = e.model.key();
    let mode = e.params.mode;
    let tabs = div()
        .flex()
        .flex_row()
        .flex_shrink_0()
        .p(px(2.0 * s))
        .gap(px(2.0 * s))
        .rounded(px(4.0 * s))
        .border(px(1.0))
        .border_color(color(WELL_EDGE))
        .bg(color(GROUND))
        .children([Mode::Sidechain, Mode::Shaper].into_iter().map(|tab| {
            let on = tab == mode;
            let wire = tab.to_wire();
            let base = div()
                .id(("duckstep-mode", wire as usize))
                .flex()
                .items_center()
                .justify_center()
                .w(px(96.0 * s))
                .h(px(24.0 * s))
                .rounded(px(3.0 * s))
                .text_size(type_px(13.0, s))
                .font_weight(FontWeight::SEMIBOLD)
                .cursor(gpui::CursorStyle::PointingHand)
                .child(tab.label())
                .on_click(e.click_cb(cx, move |this, cx| this.set_value("mode", wire, cx)));
            if on {
                base.bg(color(LIGHT)).text_color(color(DARK))
            } else {
                base.text_color(color(SOFT))
                    .hover(|style| style.bg(color(0x262626)))
            }
        }));

    let chevron = |d: &'static str| icon(d, (12.0, 12.0), (12.0, 12.0), 1.5, color(ICON), s);
    let presets = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(4.0 * s))
        .child(square(
            (key, 900usize),
            28.0,
            chevron(plate::CHEVRON_LEFT),
            true,
            s,
            e.click_cb(cx, |this, cx| this.step_preset(-1, cx)),
        ))
        .child(preset_picker(e, cx, s))
        .child(square(
            (key, 901usize),
            28.0,
            chevron(plate::CHEVRON_RIGHT),
            true,
            s,
            e.click_cb(cx, |this, cx| this.step_preset(1, cx)),
        ));

    let on_b = e.compare.on_b;
    let letter = |text: &'static str, lit: bool, index: usize, cx: &mut Cx<DuckstepPlate>| {
        let (bg, border, fg) = if lit {
            (LIGHT, LIGHT, DARK)
        } else {
            (GROUND, WELL_EDGE, SOFT)
        };
        div()
            .id((key, 902 + index))
            .flex()
            .items_center()
            .justify_center()
            .w(px(30.0 * s))
            .h(px(28.0 * s))
            .rounded(px(3.0 * s))
            .border(px(1.0))
            .border_color(color(border))
            .bg(color(bg))
            .text_color(color(fg))
            .text_size(type_px(12.0, s))
            .font_weight(FontWeight::SEMIBOLD)
            .cursor(gpui::CursorStyle::PointingHand)
            .child(text)
            .on_click(e.click_cb(cx, move |this, cx| {
                if (index == 1) != this.compare.on_b {
                    this.swap_compare(cx);
                }
            }))
    };
    let compare = div()
        .flex()
        .flex_row()
        .gap(px(2.0 * s))
        .child(letter("A", !on_b, 0, cx))
        .child(letter("B", on_b, 1, cx));

    let history = div()
        .flex()
        .flex_row()
        .gap(px(2.0 * s))
        .child(square(
            (key, 904usize),
            28.0,
            icon(plate::UNDO, (14.0, 14.0), (14.0, 14.0), 1.4, color(ICON), s),
            e.can_undo(),
            s,
            e.click_cb(cx, |this, cx| this.undo(cx)),
        ))
        .child(square(
            (key, 905usize),
            28.0,
            icon(plate::REDO, (14.0, 14.0), (14.0, 14.0), 1.4, color(ICON), s),
            e.can_redo(),
            s,
            e.click_cb(cx, |this, cx| this.redo(cx)),
        ));

    // The design lights Bypass when the plug-in is bypassed.
    let bypassed = !e.power();
    let (bg, border, ink) = if bypassed {
        (LIGHT, LIGHT, DARK)
    } else {
        (GROUND, WELL_EDGE, ICON)
    };
    let bypass = div()
        .id((key, 906usize))
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .size(px(28.0 * s))
        .rounded(px(3.0 * s))
        .border(px(1.0))
        .border_color(color(border))
        .bg(color(bg))
        .cursor(gpui::CursorStyle::PointingHand)
        .child(icon(
            plate::POWER,
            (14.0, 14.0),
            (14.0, 14.0),
            1.6,
            color(ink),
            s,
        ))
        .on_click(e.click_cb(cx, |this, cx| this.toggle("power", cx)));

    let notice = e
        .notice()
        .map(|text| words(text.to_string(), 11.0, SOFT, s).into_any_element());

    div()
        .flex()
        .flex_row()
        .items_center()
        .flex_shrink_0()
        .h(px(44.0 * s))
        .pl(px(14.0 * s))
        .pr(px(12.0 * s))
        .gap(px(16.0 * s))
        .child(
            words("Duckstep", 22.0, WHITE, s)
                .font_weight(FontWeight::SEMIBOLD)
                .line_height(px(22.0 * s)),
        )
        .child(tabs)
        .child(div().flex_1())
        .children(notice)
        .child(presets)
        .child(compare)
        .child(history)
        .child(bypass)
        .into_any_element()
}

/// The preset chip — its name and place in the bank — and its list.
fn preset_picker(e: &KitEditor<DuckstepPlate>, cx: &mut Cx<DuckstepPlate>, s: f32) -> AnyElement {
    let key = e.model.key();
    let open = e.preset_open;
    let bank = e.model.presets();
    let place = e.preset.map_or_else(
        || "edited".to_string(),
        |i| format!("{}/{}", i + 1, bank.len()),
    );
    let chip = div()
        .id((key, 910usize))
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(6.0 * s))
        .w(px(176.0 * s))
        .h(px(28.0 * s))
        .px(px(10.0 * s))
        .rounded(px(3.0 * s))
        .border(px(1.0))
        .border_color(color(if open { LIGHT } else { WELL_EDGE }))
        .bg(color(GROUND))
        .text_size(type_px(12.5, s))
        .text_color(color(INK))
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(|style| style.bg(color(0x262626)))
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .truncate()
                .child(if e.preset.is_some() {
                    e.preset_label()
                } else {
                    "Custom".to_string()
                }),
        )
        .child(words(place, 11.0, LEGEND, s))
        .on_click(e.click_cb(cx, |this, cx| this.toggle_preset_menu(cx)));

    let mut picker = div().relative().flex_shrink_0().child(chip);
    if open {
        let current = e.preset;
        let entity = cx.entity().clone();
        let close = entity.clone();
        let items: Vec<AnyElement> = bank
            .iter()
            .enumerate()
            .map(|(index, preset)| {
                let entity = entity.clone();
                let selected = current == Some(index);
                let item = div()
                    .id((key, 1_000 + index))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0 * s))
                    .w_full()
                    .h(px(28.0 * s))
                    .px(px(10.0 * s))
                    .rounded(px(3.0 * s))
                    .text_size(type_px(12.5, s))
                    .cursor(gpui::CursorStyle::PointingHand)
                    .child(div().size(px(6.0 * s)).rounded_full().bg(if selected {
                        color(LIGHT)
                    } else {
                        color_a(0, 0.0)
                    }))
                    .child(preset.name)
                    .on_click(move |_, _, app| {
                        let _ = entity.update(app, |this, cx| this.load_preset(index as isize, cx));
                    });
                if selected {
                    item.text_color(color(WHITE)).bg(color(0x262626))
                } else {
                    item.text_color(color(PALE))
                        .hover(|style| style.text_color(color(WHITE)).bg(color(0x262626)))
                }
                .into_any_element()
            })
            .collect();
        let menu = div()
            .id((key, 911usize))
            .occlude()
            .flex()
            .flex_col()
            .w(px(220.0 * s))
            .max_h(px(380.0 * s))
            .overflow_y_scroll()
            .p(px(4.0 * s))
            .gap(px(1.0 * s))
            .rounded(px(4.0 * s))
            .border(px(1.0))
            .border_color(color(CARD_EDGE))
            .bg(color(GROUND))
            .shadow(vec![gpui::BoxShadow {
                color: color_a(0, 0.55).into(),
                offset: point(px(0.0), px(14.0 * s)),
                blur_radius: px(36.0 * s),
                spread_radius: px(0.0),
                inset: false,
            }])
            .font(plate::outfit(FontWeight::NORMAL))
            .children(items)
            .on_mouse_down_out(move |_, _, app| {
                let _ = close.update(app, |this, cx| this.close_preset_menu(cx));
            });
        picker = picker.child(
            div().absolute().left_0().bottom_0().child(
                deferred(
                    anchored()
                        .anchor(Anchor::TopLeft)
                        .offset(point(px(0.0), px(4.0 * s)))
                        .snap_to_window_with_margin(px(8.0))
                        .child(menu),
                )
                .with_priority(2),
            ),
        );
    }
    picker.into_any_element()
}

// ── The well ───────────────────────────────────────────────────────────────

fn display(e: &KitEditor<DuckstepPlate>, cx: &mut Cx<DuckstepPlate>, s: f32) -> AnyElement {
    let p = &e.params;
    let screen = div()
        .absolute()
        .left(px(14.0 * s))
        .top(px(2.0 * s))
        .w(px(GW * s))
        .h(px(GH * s))
        .bg(color(SCREEN))
        .border(px(1.0))
        .border_color(color(SCREEN_EDGE))
        .overflow_hidden()
        .child(live_slot(&e.live_bounds, SLOT_GRAPH).absolute().size_full());
    let (screen, legend) = match p.mode {
        Mode::Sidechain => (
            sidechain_screen(e, cx, screen, s),
            sidechain_legend(e, cx, s),
        ),
        Mode::Shaper => (shaper_screen(e, cx, screen, s), shaper_legend(e, cx, s)),
    };
    well(s, 7.0, 327.0)
        .child(header(e, cx, s))
        .child(
            div()
                .relative()
                .flex_1()
                .child(screen)
                .child(
                    div()
                        .absolute()
                        .left(px(14.0 * s))
                        .top(px(252.0 * s))
                        .w(px(GW * s))
                        .h(px(24.0 * s))
                        .child(legend),
                )
                .child(meters(e, s)),
        )
        .into_any_element()
}

/// Over the history: the dB scale, the GR reading and the threshold handle.
fn sidechain_screen(
    e: &KitEditor<DuckstepPlate>,
    cx: &mut Cx<DuckstepPlate>,
    screen: gpui::Div,
    s: f32,
) -> gpui::Div {
    let p = &e.params;
    let inner = |y: f32| y * (GH - 2.0) / GH;
    let mut scale = div().absolute().left(px(6.0 * s)).top_0().h_full();
    for db in [0.0f32, -12.0, -24.0, -36.0, -48.0] {
        let text = if db == 0.0 {
            "0".to_string()
        } else {
            format!("\u{2212}{}", -db)
        };
        let y = inner(db_y(db)) + if db == 0.0 { 2.0 } else { -13.0 };
        scale = scale.child(
            div()
                .absolute()
                .left_0()
                .top(px(y * s))
                .child(numbers(text, 10.0, 0x8a8a8a, s)),
        );
    }
    let gr_now = div()
        .absolute()
        .left(px(44.0 * s))
        .top(px(8.0 * s))
        .w(px(120.0 * s))
        .h(px(16.0 * s))
        .child(live_slot(&e.live_bounds, SLOT_GR_NOW).size_full());
    let top = inner(db_y(p.threshold_db)) - 10.0;
    let handle = div()
        .id("duckstep-threshold")
        .absolute()
        .right(px(6.0 * s))
        .top(px(top * s))
        .h(px(20.0 * s))
        .px(px(7.0 * s))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(5.0 * s))
        .rounded(px(3.0 * s))
        .border(px(1.0))
        .border_color(color(OFF))
        .bg(color(GROUND))
        .cursor(gpui::CursorStyle::ResizeUpDown)
        .child(words("Thr", 10.5, SOFT, s))
        .child(numbers(plate::fmt_db(p.threshold_db), 11.0, WHITE, s))
        .on_mouse_down(
            MouseButton::Left,
            e.press_cb(cx, |this, _, event, cx| {
                cx.stop_propagation();
                if event.click_count >= 2 {
                    this.ui.drag = None;
                    this.set_value("thresholdDb", THRESHOLD.dial.default, cx);
                } else {
                    this.ui.drag = Some(Drag::Threshold);
                }
            }),
        )
        .on_scroll_wheel(e.wheel_cb(cx, |this, dy, fine, cx| {
            let next = plate::wheel_turn(THRESHOLD.dial, this.params.threshold_db, dy, fine);
            this.set_value("thresholdDb", next, cx);
        }));
    screen
        .child(plate::over_live(scale))
        .child(gr_now)
        .child(plate::over_live(handle))
}

/// Over the curve: where a double-click adds a point, the restart's label,
/// and a handle on every point.
fn shaper_screen(
    e: &KitEditor<DuckstepPlate>,
    cx: &mut Cx<DuckstepPlate>,
    screen: gpui::Div,
    s: f32,
) -> gpui::Div {
    let p = &e.params;
    let area = div()
        .id("duckstep-shape-area")
        .absolute()
        .size_full()
        .cursor(gpui::CursorStyle::Crosshair)
        .on_mouse_down(
            MouseButton::Left,
            e.press_cb(cx, |this, at, event, cx| {
                if event.click_count >= 2 {
                    add_point(this, at, cx);
                }
            }),
        );
    let restart = shape_position(p, 0.0) * GW;
    let label = match p.trigger {
        Trigger::Host => "Beat",
        Trigger::Midi => "Cycle",
        Trigger::Audio => "Hit",
    };
    let tag = div()
        .absolute()
        .top(px(6.0 * s))
        .left(px((restart + 4.0).clamp(4.0, GW - 60.0) * s))
        .px(px(5.0 * s))
        .py(px(1.0 * s))
        .rounded(px(2.0 * s))
        .bg(color(LIGHT))
        .child(
            words(label, 10.5, DARK, s)
                .font_weight(FontWeight::SEMIBOLD)
                .line_height(px(13.0 * s)),
        );
    let count = (p.point_count as usize).clamp(2, MAX_POINTS);
    let mut screen = screen.child(area).child(plate::over_live(tag));
    for (index, (x, y)) in p.points().enumerate() {
        let left = x * (GW - 2.0) - 11.0;
        let top = gain_y(y) * (GH - 2.0) / GH - 11.0;
        let removable = index > 0 && index < count - 1;
        screen = screen.child(
            div()
                .id(("duckstep-point", index))
                .absolute()
                .left(px(left * s))
                .top(px(top * s))
                .size(px(22.0 * s))
                .rounded_full()
                .cursor(gpui::CursorStyle::OpenHand)
                .on_mouse_down(
                    MouseButton::Left,
                    e.press_cb(cx, move |this, _, event, cx| {
                        cx.stop_propagation();
                        if event.click_count >= 2 {
                            this.ui.drag = None;
                            if removable {
                                remove_point(this, index, cx);
                            }
                        } else {
                            this.ui.drag = Some(Drag::Point(index));
                        }
                    }),
                ),
        );
    }
    screen
}

/// A point where the double-click was, if there is room for one.
fn add_point(e: &mut KitEditor<DuckstepPlate>, at: (f32, f32), cx: &mut Cx<DuckstepPlate>) {
    let Some(b) = e.live_bounds.get(LiveSlot::Plate(SLOT_GRAPH)) else {
        return;
    };
    let count = (e.params.point_count as usize).clamp(2, MAX_POINTS);
    if count >= MAX_POINTS {
        e.show_notice(format!("A shape holds {MAX_POINTS} points"), cx);
        return;
    }
    let (fx, fy) = graph_fraction(b, at);
    let x = ((fx * 1_000.0).round() / 1_000.0).clamp(0.005, 0.995);
    let y = 1.0 - ((fy * GH - 10.0) / (GH - 20.0)).clamp(0.0, 1.0);
    let mut next = e.params.clone();
    let slot = (1..count)
        .find(|&i| next.point_x[i] > x)
        .unwrap_or(count - 1);
    for i in (slot..count).rev() {
        next.point_x[i + 1] = next.point_x[i];
        next.point_y[i + 1] = next.point_y[i];
    }
    next.point_x[slot] = x;
    next.point_y[slot] = (y * 1_000.0).round() / 1_000.0;
    next.point_count = count as u32 + 1;
    e.set_params(next, cx);
}

fn remove_point(e: &mut KitEditor<DuckstepPlate>, index: usize, cx: &mut Cx<DuckstepPlate>) {
    let count = (e.params.point_count as usize).clamp(2, MAX_POINTS);
    if index == 0 || index + 1 >= count {
        return;
    }
    let mut next = e.params.clone();
    for i in index..count - 1 {
        next.point_x[i] = next.point_x[i + 1];
        next.point_y[i] = next.point_y[i + 1];
    }
    next.point_x[count - 1] = 0.0;
    next.point_y[count - 1] = 0.0;
    next.point_count = count as u32 - 1;
    e.set_params(next, cx);
}

fn legend_item(swatch: gpui::Div, text: &str, s: f32) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0 * s))
        .child(swatch)
        .child(words(text.to_string(), 11.0, LEGEND, s))
}

/// A short dashed rule for the legend.
fn dashed_swatch(ink: Rgba, dash: (f32, f32), s: f32) -> gpui::Div {
    div().w(px(14.0 * s)).h(px(2.0 * s)).child(
        canvas(
            |_, _, _| (),
            move |b, _, window, _| {
                let (x, y) = plate::xy(b.origin);
                paint_polyline(
                    window,
                    &[(x, y + 0.5 * s), (x + 14.0 * s, y + 0.5 * s)],
                    1.0,
                    ink,
                    Some((dash.0 * s, dash.1 * s)),
                );
            },
        )
        .size_full(),
    )
}

/// The status reading and the Pause button, at the legend's right.
fn legend_tail(
    e: &KitEditor<DuckstepPlate>,
    cx: &mut Cx<DuckstepPlate>,
    s: f32,
) -> [AnyElement; 3] {
    let paused = e.ui.paused;
    [
        div().flex_1().into_any_element(),
        div()
            .w(px(230.0 * s))
            .h(px(15.0 * s))
            .child(live_slot(&e.live_bounds, SLOT_STATUS).size_full())
            .into_any_element(),
        div()
            .id("duckstep-pause")
            .flex()
            .items_center()
            .h(px(22.0 * s))
            .px(px(8.0 * s))
            .rounded(px(3.0 * s))
            .border(px(1.0))
            .border_color(color(WELL_EDGE))
            .bg(color(GROUND))
            .text_color(color(ICON))
            .text_size(type_px(11.0, s))
            .cursor(gpui::CursorStyle::PointingHand)
            .hover(|style| style.bg(color(0x262626)))
            .child(if paused {
                "Resume display"
            } else {
                "Pause display"
            })
            .on_click(e.click_cb(cx, |this, cx| {
                this.ui.paused = !this.ui.paused;
                cx.notify();
            }))
            .into_any_element(),
    ]
}

fn sidechain_legend(e: &KitEditor<DuckstepPlate>, cx: &mut Cx<DuckstepPlate>, s: f32) -> gpui::Div {
    let p = &e.params;
    let detector_ink = color(if p.listen { LIGHT } else { 0x8a8a8a });
    let rule = |ink: Rgba, h: f32| div().w(px(14.0 * s)).h(px(h)).bg(ink);
    div()
        .flex()
        .flex_row()
        .items_center()
        .size_full()
        .gap(px(18.0 * s))
        .child(legend_item(
            rule(detector_ink, 1.0),
            if p.listen {
                "Detector (listening)"
            } else {
                "Detector"
            },
            s,
        ))
        .child(legend_item(
            rule(color(0xd6d6d6), 2.0 * s),
            if p.monitor == Monitor::Delta {
                "Removed signal"
            } else {
                "Output"
            },
            s,
        ))
        .child(legend_item(
            div()
                .w(px(14.0 * s))
                .h(px(8.0 * s))
                .rounded(px(1.0 * s))
                .bg(color_a(ACCENT, 0.6)),
            "Gain reduction",
            s,
        ))
        .child(legend_item(
            dashed_swatch(color(0xbdbdbd), (3.0, 2.0), s),
            "Threshold",
            s,
        ))
        .children(legend_tail(e, cx, s))
}

fn shaper_legend(e: &KitEditor<DuckstepPlate>, cx: &mut Cx<DuckstepPlate>, s: f32) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .size_full()
        .gap(px(18.0 * s))
        .child(words(
            "Drag points, double-click to add or remove",
            11.0,
            LEGEND,
            s,
        ))
        .child(legend_item(
            dashed_swatch(color(0x6a6a6a), (4.0, 3.0), s),
            "Shape before depth",
            s,
        ))
        .children(legend_tail(e, cx, s))
}

/// In, GR and Out, as tall bars with their labels and readings.
fn meters(e: &KitEditor<DuckstepPlate>, s: f32) -> gpui::Div {
    let p = &e.params;
    let out_label = if p.listen && p.mode == Mode::Sidechain {
        "Det"
    } else if p.monitor == Monitor::Delta {
        "Delta"
    } else {
        "Out"
    };
    let column = |index: usize, text: &str| {
        div()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(6.0 * s))
            .w(px(40.0 * s))
            .child(
                div()
                    .w(px(14.0 * s))
                    .h(px(GH * s))
                    .child(live_slot(&e.live_bounds, SLOT_BAR + index).size_full()),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .child(
                        words(text.to_string(), 10.5, LEGEND, s)
                            .font_weight(FontWeight::MEDIUM)
                            .line_height(px(12.0 * s)),
                    )
                    .child(
                        div()
                            .w(px(40.0 * s))
                            .h(px(12.0 * s))
                            .child(live_slot(&e.live_bounds, SLOT_VALUE + index).size_full()),
                    ),
            )
    };
    div()
        .absolute()
        .left(px(852.0 * s))
        .top(px(2.0 * s))
        .w(px(140.0 * s))
        .h(px(274.0 * s))
        .flex()
        .flex_row()
        .justify_between()
        .child(column(0, "In"))
        .child(column(1, "GR"))
        .child(column(2, out_label))
}

// ── The deck ───────────────────────────────────────────────────────────────

fn controls(e: &KitEditor<DuckstepPlate>, cx: &mut Cx<DuckstepPlate>, s: f32) -> AnyElement {
    let mut deck = deck(s, 341.0, 420.0);
    deck = match e.params.mode {
        Mode::Sidechain => deck
            .child(detector_section(e, cx, s))
            .child(envelope_section(e, cx, s)),
        Mode::Shaper => deck
            .child(trigger_section(e, cx, s))
            .child(shape_section(e, cx, s)),
    };
    deck.child(output_section(e, cx, s)).into_any_element()
}

fn detector_section(e: &KitEditor<DuckstepPlate>, cx: &mut Cx<DuckstepPlate>, s: f32) -> gpui::Div {
    let p = &e.params;
    let (bg, border, fg) = if p.listen {
        (LIGHT, LIGHT, DARK)
    } else {
        (0x2c2c2c, 0x4a4a4a, 0xd0d0d0)
    };
    let listen = div()
        .id("duckstep-listen")
        .flex()
        .flex_row()
        .items_center()
        .gap(px(5.0 * s))
        .h(px(22.0 * s))
        .px(px(8.0 * s))
        .rounded(px(3.0 * s))
        .border(px(1.0))
        .border_color(color(border))
        .bg(color(bg))
        .text_color(color(fg))
        .text_size(type_px(11.5, s))
        .cursor(gpui::CursorStyle::PointingHand)
        .child(icon(
            HEADPHONES,
            (12.0, 12.0),
            (12.0, 12.0),
            1.2,
            color(fg),
            s,
        ))
        .child("Listen")
        .on_click(e.click_cb(cx, |this, cx| this.toggle("listen", cx)))
        .into_any_element();
    // Studio has no sidechain bus: the detector always hears the track.
    let sources = choices(
        e,
        cx,
        "duckstep-source",
        "source",
        p.source,
        &[
            (Source::External, "External key", false),
            (Source::Internal, "Internal", true),
        ],
        Source::to_wire,
        (30.0, 12.5, false),
        s,
    );
    let source_help = if p.listen {
        format!(
            "Hearing the detector \u{00B7} {} to {}",
            f_hz(p.sc_hpf_hz),
            f_hz(p.sc_lpf_hz)
        )
    } else {
        "Own signal \u{00B7} no sidechain input in Studio yet".to_string()
    };
    let knobs = vec![
        dial(cx, THRESHOLD, p.threshold_db, true, s),
        dial(cx, RATIO, p.ratio, true, s),
        dial(cx, KNEE, p.knee_db, true, s),
        dial(cx, SC_HPF, p.sc_hpf_hz, true, s),
        dial(cx, SC_LPF, p.sc_lpf_hz, true, s),
    ];
    section(0.0, 312.0, false, s)
        .child(section_head("Detector", Some(listen), s))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(8.0 * s))
                .h(px(52.0 * s))
                .child(sources)
                .child(help(source_help, s)),
        )
        .child(grid(3, 0.0, 22.0, knobs, s))
}

fn envelope_section(e: &KitEditor<DuckstepPlate>, cx: &mut Cx<DuckstepPlate>, s: f32) -> gpui::Div {
    let p = &e.params;
    let synced = p.release_sync != ReleaseSync::Off;
    let release = if synced {
        p.release_sync.label().to_string()
    } else {
        plate::fmt_ms(p.release_ms)
    };
    let total = format!(
        "A {} \u{00B7} H {} \u{00B7} R {release}",
        plate::fmt_ms(p.attack_ms),
        plate::fmt_ms(p.hold_ms)
    );
    let sketch = div()
        .h(px(52.0 * s))
        .rounded(px(3.0 * s))
        .border(px(1.0))
        .border_color(color(CARD_EDGE))
        .bg(color(plate::CARD))
        .overflow_hidden()
        .child(live_slot(&e.live_bounds, SLOT_ENV).size_full());
    let syncs = choices(
        e,
        cx,
        "duckstep-sync",
        "releaseSync",
        p.release_sync,
        &[
            (ReleaseSync::Off, "Off", true),
            (ReleaseSync::Sixteenth, "1/16", true),
            (ReleaseSync::Eighth, "1/8", true),
            (ReleaseSync::Quarter, "1/4", true),
        ],
        ReleaseSync::to_wire,
        (30.0, 11.5, true),
        s,
    );
    let first = vec![
        dial(cx, ATTACK, p.attack_ms, true, s),
        dial(cx, HOLD, p.hold_ms, true, s),
        dial(cx, RELEASE, p.release_ms, !synced, s),
        dial(cx, LOOKAHEAD, p.lookahead_ms, true, s),
    ];
    let range = dial(cx, RANGE, p.range_db, true, s);
    let sync = div()
        .flex()
        .flex_col()
        .justify_center()
        .gap(px(8.0 * s))
        .pl(px(10.0 * s))
        .child(label("Release sync", s))
        .child(syncs);
    section(312.0, 419.0, true, s)
        .child(section_head(
            "Envelope",
            Some(numbers(total, 11.0, HELP, s).into_any_element()),
            s,
        ))
        .child(sketch)
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(22.0 * s))
                .child(plate::columns(0.0, s, first))
                .child(
                    // Range takes one of the four columns, Release sync the
                    // other three.
                    div()
                        .flex()
                        .flex_row()
                        .child(div().w(px((419.0 - 44.0) / 4.0 * s)).child(range))
                        .child(div().flex_1().min_w(px(0.0)).child(sync)),
                ),
        )
}

fn trigger_section(e: &KitEditor<DuckstepPlate>, cx: &mut Cx<DuckstepPlate>, s: f32) -> gpui::Div {
    let p = &e.params;
    let restart = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0 * s))
        .child(
            div()
                .size(px(8.0 * s))
                .child(live_slot(&e.live_bounds, SLOT_LED).size_full()),
        )
        .child(words("Restart", 11.0, HELP, s))
        .into_any_element();
    let triggers = choices(
        e,
        cx,
        "duckstep-trigger",
        "trigger",
        p.trigger,
        &[
            (Trigger::Host, "Host", true),
            // The engine sends MIDI only to a track's instrument.
            (Trigger::Midi, "MIDI", false),
            (Trigger::Audio, "Audio", true),
        ],
        Trigger::to_wire,
        (30.0, 12.5, false),
        s,
    );
    let trigger_help = match p.trigger {
        Trigger::Host => "Restarts on the host beat grid",
        Trigger::Midi => "Effect inserts receive no MIDI: free-running",
        Trigger::Audio => "Restarts when the input crosses threshold",
    };
    let rates = choices(
        e,
        cx,
        "duckstep-rate",
        "rate",
        p.rate,
        &[
            (Rate::Whole, "1/1", true),
            (Rate::Half, "1/2", true),
            (Rate::Quarter, "1/4", true),
            (Rate::Eighth, "1/8", true),
            (Rate::Sixteenth, "1/16", true),
        ],
        Rate::to_wire,
        (30.0, 11.5, true),
        s,
    );
    let feels = choices(
        e,
        cx,
        "duckstep-feel",
        "feel",
        p.feel,
        &[
            (Feel::Straight, "Straight", true),
            (Feel::Dotted, "Dotted", true),
            (Feel::Triplet, "Triplet", true),
        ],
        Feel::to_wire,
        (26.0, 11.5, false),
        s,
    );
    let knobs = vec![
        dial(cx, OFFSET, p.offset, true, s),
        dial(
            cx,
            SHAPER_THRESHOLD,
            p.shaper_threshold_db,
            p.trigger == Trigger::Audio,
            s,
        ),
    ];
    section(0.0, 312.0, false, s)
        .child(section_head("Trigger", Some(restart), s))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(8.0 * s))
                .h(px(52.0 * s))
                .child(triggers)
                .child(help(trigger_help, s)),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(6.0 * s))
                .h(px(92.0 * s))
                .child(label("Rate", s))
                .child(rates)
                .child(feels),
        )
        .child(grid(3, 0.0, 22.0, knobs, s))
}

/// A preset's shape, small, for its button.
fn shape_icon(index: usize, ink: Rgba, s: f32) -> gpui::Div {
    let mut params = duckstep::default_params();
    params.set_shape(&SHAPE_PRESETS[index]);
    div().w(px(44.0 * s)).h(px(20.0 * s)).child(
        canvas(
            |_, _, _| (),
            move |b, _, window, _| {
                let (x0, y0) = plate::xy(b.origin);
                let k = f32::from(b.size.width) / 44.0;
                let ky = f32::from(b.size.height) / 22.0;
                let line: Vec<(f32, f32)> = (0..=40)
                    .map(|i| {
                        let x = i as f32 / 40.0;
                        (
                            x0 + (2.0 + x * 40.0) * k,
                            y0 + (20.0 - shape_at(&params, x) * 18.0) * ky,
                        )
                    })
                    .collect();
                paint_polyline(window, &line, 1.6 * k, ink, None);
            },
        )
        .size_full(),
    )
}

fn shape_section(e: &KitEditor<DuckstepPlate>, cx: &mut Cx<DuckstepPlate>, s: f32) -> gpui::Div {
    let p = &e.params;
    let current = SHAPE_PRESETS.iter().position(|preset| p.has_shape(preset));
    let name = current.map_or("Custom", |i| SHAPE_PRESETS[i].name);
    let buttons: Vec<AnyElement> = SHAPE_PRESETS
        .iter()
        .enumerate()
        .map(|(index, preset)| {
            let on = current == Some(index);
            let (bg, border, fg) = if on {
                (LIGHT, LIGHT, DARK)
            } else {
                (0x2c2c2c, 0x4a4a4a, 0xd0d0d0)
            };
            div()
                .id(("duckstep-shape", index))
                .flex()
                .flex_col()
                .items_center()
                .justify_between()
                .h(px(52.0 * s))
                .pt(px(6.0 * s))
                .pb(px(4.0 * s))
                .rounded(px(3.0 * s))
                .border(px(1.0))
                .border_color(color(border))
                .bg(color(bg))
                .text_color(color(fg))
                .text_size(type_px(11.5, s))
                .cursor(gpui::CursorStyle::PointingHand)
                .hover(move |style| style.bg(color(if on { 0xf4f4f4 } else { 0x363636 })))
                .child(shape_icon(index, color(fg), s))
                .child(preset.name)
                .on_click(e.click_cb(cx, move |this, cx| {
                    let mut next = this.params.clone();
                    next.set_shape(&SHAPE_PRESETS[index]);
                    this.set_params(next, cx);
                }))
                .into_any_element()
        })
        .collect();
    let knobs = vec![
        dial(cx, DEPTH, p.depth, true, s),
        dial(cx, CURVE, p.curve, true, s),
        dial(cx, SMOOTH, p.smooth_ms, true, s),
    ];
    section(312.0, 419.0, true, s)
        .child(section_head(
            "Shape",
            Some(words(name, 12.0, HELP, s).into_any_element()),
            s,
        ))
        .child(div().h(px(52.0 * s)).child(plate::columns(4.0, s, buttons)))
        .child(grid(4, 0.0, 22.0, knobs, s))
}

fn output_section(e: &KitEditor<DuckstepPlate>, cx: &mut Cx<DuckstepPlate>, s: f32) -> gpui::Div {
    let p = &e.params;
    let bands = choices(
        e,
        cx,
        "duckstep-band",
        "band",
        p.band,
        &[
            (Band::Full, "Full", true),
            (Band::Low, "Lows", true),
            (Band::High, "Highs", true),
        ],
        Band::to_wire,
        (30.0, 12.5, false),
        s,
    );
    let verb = if p.mode == Mode::Sidechain {
        "Ducks"
    } else {
        "Shapes"
    };
    let band_help = match p.band {
        Band::Full => format!("{verb} the whole signal"),
        Band::Low => format!("{verb} below {} only", f_hz(p.xover_hz)),
        Band::High => format!("{verb} above {} only", f_hz(p.xover_hz)),
    };
    let monitors = choices(
        e,
        cx,
        "duckstep-monitor",
        "monitor",
        p.monitor,
        &[
            (Monitor::Output, "Output", true),
            (Monitor::Delta, "Delta", true),
        ],
        Monitor::to_wire,
        (30.0, 12.5, false),
        s,
    );
    let monitor_help = match p.monitor {
        Monitor::Output => "Hear the processed signal",
        Monitor::Delta => "Hear only what the ducking removes",
    };
    let knobs = vec![
        dial(cx, XOVER, p.xover_hz, p.band != Band::Full, s),
        dial(cx, MIX, p.mix, true, s),
        dial(cx, OUTPUT, p.output_db, true, s),
    ];
    section(731.0, 280.0, true, s)
        .child(section_head("Output", None, s))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(8.0 * s))
                .h(px(52.0 * s))
                .child(bands)
                .child(help(band_help, s)),
        )
        .child(grid(3, 0.0, 22.0, knobs, s))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(8.0 * s))
                .child(label("Monitor", s))
                .child(monitors)
                .child(help(monitor_help, s)),
        )
}

/// Opens with a track's Duckstep insert.
pub fn open_duckstep_editor(
    owner_bounds: Option<gpui::Bounds<gpui::Pixels>>,
    key: PluginInstanceKey,
    identity: ShellIdentity,
    host_ops: BuiltinEditorHostOps,
    on_close: Arc<dyn Fn(&mut Window, &mut App)>,
    cx: &mut App,
) -> Result<gpui::WindowHandle<DuckstepWindow>, String> {
    plate::ensure_fonts(cx);
    open_kit_editor(
        DuckstepPlate,
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
    fn the_scales_match_the_design() {
        assert_eq!(db_y(0.0), 6.0);
        assert_eq!(db_y(-60.0), GH - 6.0);
        assert_eq!(gain_y(1.0), 10.0);
        assert_eq!(gain_y(0.0), GH - 10.0);
    }

    #[test]
    fn readouts_print_like_the_design() {
        assert_eq!(f_hz(30.0), "30 Hz");
        assert_eq!(f_hz(4_500.0), "4.5 kHz");
        assert_eq!(f_hz(20_000.0), "20 kHz");
        assert_eq!(f_ratio(8.0), "8.0:1");
        assert_eq!(f_ratio(20.0), "20:1");
        assert_eq!(f_range(24.0), "\u{2212}24 dB");
        assert_eq!(f_bi(45.0), "+45%");
        assert_eq!(f_bi(-20.0), "\u{2212}20%");
        assert_eq!(f_bi(0.0), "0%");
    }

    #[test]
    fn the_status_line_reads_the_host_tempo() {
        let mut block = [0.0; telemetry::SLOTS];
        let p = duckstep::default_params();
        assert_eq!(status_text(&p, None), None);
        assert_eq!(
            status_text(&p, Some(&block)).as_deref(),
            Some("Self-detect \u{00B7} No tempo yet")
        );
        block[telemetry::TEMPO_BPM] = 128.0;
        assert_eq!(
            status_text(&p, Some(&block)).as_deref(),
            Some("Self-detect \u{00B7} 128 BPM")
        );
        let mut shaper = p.clone();
        shaper.mode = Mode::Shaper;
        shaper.feel = Feel::Dotted;
        assert_eq!(
            status_text(&shaper, Some(&block)).as_deref(),
            Some("128 BPM \u{00B7} 1/4D \u{00B7} Host \u{00B7} stopped")
        );
    }
}

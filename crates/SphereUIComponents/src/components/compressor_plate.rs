//! The Compressor's editor, on the plate kit, in both of its modes.
//!
//! * **Single** — ten seconds of the input and output levels with the gain
//!   reduction hanging from the top, and the static transfer curve, whose
//!   threshold is dragged sideways, in the well; Compression, Timing and
//!   Output cards on the deck.
//! * **Multi** — the spectrum split at three crossovers dragged on the
//!   display, each band's live reduction hanging from the top of its
//!   stretch, in the well; a card per band — bypass, solo, its reduction and
//!   its five knobs — and the Output card on the deck.
//!
//! The curve is the DSP's own gain computer (`compresser::curve_reduction_db`),
//! painted when the panel renders. Levels, reductions and the spectrum are
//! painted by the live overlay; crossovers and band names are drawn over it
//! from the params.

use std::sync::Arc;

use compresser::Mode;
use gpui::{
    div, px, AnyElement, App, Bounds, FontWeight, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, MouseMoveEvent, ParentElement, Pixels, Rgba, Styled, Window,
};

use crate::components::band_model::{
    self, band_id, crossover_bounds, short_hz, BandKind, BandParams, BANDS, BAND_NAMES,
    CROSSOVER_IDS,
};
use crate::components::builtin_plugin_editor::builtin_compresser_params;
use crate::components::builtin_plugin_editor_window::{BuiltinEditorHostOps, PluginInstanceKey};
use crate::components::fx_model::KnobSpec;
use crate::components::native_plugin_shell::ShellIdentity;
use crate::components::plate::{
    self, caption, card, card_title, color, color_a, columns, deck, fmt_db, fmt_hz_round, header,
    knob_reading, legend, live_slot, numbers, paint_polygon, paint_polyline, paint_rect,
    paint_text, pill, plate_frame, plate_scale, well, Align, Dial, DialUnit, KnobCell, KnobLook,
    PillLook, FACE, FACE_EDGE, FAINT, INK, SOFT, WHITE,
};
use crate::components::plate_parts::{
    fmt_reduction, knob_row, level_db, note, over_canvas, paint_gr_bar, paint_reading,
    paint_spectrum, section,
};
use crate::components::plugin_kit::{
    open_kit_editor, Cx, KitEditor, KitModel, KitPreset, KitWindow, LiveBounds, LiveSlot,
};
use crate::components::plugin_live::{Live, HISTORY};
use crate::components::time_plate::{
    at, at_right, display_box, fmt_time, hz_at_x, io_meters, paint_meter, x_of_hz, AXIS_INK,
    FREQ_GRID, GRID, NARROW_W, WIDE_W,
};

/// The Compressor's accent, and the solo's amber.
pub const ACCENT: u32 = 0xec6fa8;
const SOLO: u32 = 0xf2c14e;

const SLOT_HISTORY: usize = 0;
const SLOT_TRANSFER: usize = 1;
const SLOT_GR_READOUT: usize = 2;
const SLOT_GR_BAR: usize = 3;
const SLOT_METER: usize = 4;
const SLOT_BANDS: usize = 6;
/// A band card's reduction bar, one per band.
const SLOT_BAND_GR: usize = 7;

/// The level range the history and the transfer span, to 0 dBFS.
const RANGE_DB: f32 = -60.0;
/// The reduction scales' full span.
const GR_SPAN_DB: f32 = 24.0;

/// The history plot in its box.
const H_TOP: f32 = 34.0;
const H_BOTTOM: f32 = 214.0;

/// The transfer plot in its box.
const T_LEFT: f32 = 46.0;
const T_RIGHT: f32 = NARROW_W - 18.0;
const T_TOP: f32 = 46.0;
const T_BOTTOM: f32 = 212.0;

/// The band display: the whole well wide.
const BANDS_W: f32 = 978.0;
const B_TOP: f32 = 30.0;
const B_BOTTOM: f32 = 222.0;
/// How near a press must land to a crossover to take it, and its tag.
const CROSSOVER_HIT: f32 = 9.0;
const TAG_W: f32 = 54.0;
const TAG_H: f32 = 18.0;

fn accent() -> Rgba {
    color(ACCENT)
}

/// The dial over the Compressor's descriptor row `id`.
fn dial(id: &'static str, label: &'static str, unit: DialUnit) -> Dial {
    let row = compresser::descriptor()
        .params
        .iter()
        .find(|row| row.id == id)
        .map(|row| (row.min, row.max, row.default_value))
        .unwrap_or_else(|| panic!("The Compressor has no param {id}"));
    Dial::new(id, label, row.0, row.1, row.2, unit)
}

/// The dial for one of a band's (or the single stage's) fields.
fn stage_dial(id: &'static str) -> Dial {
    let field = id
        .strip_prefix("band")
        .and_then(|rest| rest.get(1..))
        .unwrap_or(id);
    match field {
        "thresholdDb" | "ThresholdDb" => dial(id, "THRESHOLD", DialUnit::Db),
        "ratio" | "Ratio" => dial(id, "RATIO", DialUnit::Ratio).log(),
        "attackMs" | "AttackMs" => dial(id, "ATTACK", DialUnit::Ms).log(),
        "releaseMs" | "ReleaseMs" => dial(id, "RELEASE", DialUnit::Ms).log(),
        "makeupDb" | "MakeupDb" => dial(id, "MAKEUP", DialUnit::Db),
        "sidechainHpfHz" => dial(id, "SC HPF", DialUnit::HzRound),
        "kneeDb" => dial(id, "KNEE", DialUnit::Db),
        "mix" => dial(id, "MIX", DialUnit::Percent),
        "outputDb" => dial(id, "OUTPUT", DialUnit::Db),
        _ => panic!("The Compressor has no knob for {id}"),
    }
}

/// A knob's reading: a ratio past 19.95 is a limiter, the detector filter
/// at its stop is off.
fn readout(id: &str, value: f32) -> String {
    let field = id
        .strip_prefix("band")
        .and_then(|rest| rest.get(1..))
        .unwrap_or(id);
    match field {
        "ratio" | "Ratio" => format!("{value:.1}:1"),
        "attackMs" | "AttackMs" | "releaseMs" | "ReleaseMs" => fmt_time(value),
        "sidechainHpfHz" if value <= compresser::SIDECHAIN_OFF_HZ => "Off".to_string(),
        "sidechainHpfHz" => fmt_hz_round(value),
        "kneeDb" => format!("{value:.1} dB"),
        "mix" => format!("{}%", value.round()),
        _ => fmt_db(value),
    }
}

fn params_of(params: &BandParams) -> &compresser::Params {
    let BandParams::Comp(p) = params;
    p
}

/// A gesture on a display.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CompDrag {
    Crossover(usize),
    Threshold,
}

#[derive(Debug, Default)]
pub struct CompUi {
    pub drag: Option<CompDrag>,
}

/// A Compressor insert's editor window.
pub type CompressorWindow = KitWindow<BandKind>;

impl KitModel for BandKind {
    type Params = BandParams;
    type Ui = CompUi;
    type LiveView = BandParams;

    fn title(self) -> &'static str {
        match self {
            BandKind::Comp => "Compressor",
        }
    }

    fn subtitle(self, params: &BandParams) -> String {
        if params.multiband() {
            "Four-band compression"
        } else {
            "Single-band compression"
        }
        .to_string()
    }

    fn key(self) -> &'static str {
        match self {
            BandKind::Comp => "compresser-plate",
        }
    }

    fn defaults(self) -> BandParams {
        BandParams::defaults(self)
    }

    fn from_mirror(self, insert_id: &str) -> Option<BandParams> {
        Some(match self {
            BandKind::Comp => BandParams::Comp(builtin_compresser_params(insert_id)?),
        })
    }

    fn wire_values(self, params: &BandParams) -> Vec<(u32, f32)> {
        params.wire_values()
    }

    fn with(self, params: &BandParams, id: &str, value: f32) -> BandParams {
        params.with(id, value)
    }

    fn value(self, params: &BandParams, id: &str) -> f32 {
        params.value(id)
    }

    fn presets(self) -> Arc<Vec<KitPreset<BandParams>>> {
        band_model::presets(self)
    }

    fn preset_applied(self, current: &BandParams, preset: &BandParams) -> BandParams {
        band_model::preset_applied(current, preset)
    }

    fn knob(self, id: &str) -> Option<KnobSpec> {
        band_model::knob(self, id)
    }

    fn panel(e: &KitEditor<Self>, cx: &mut Cx<Self>) -> AnyElement {
        let s = plate_scale(e.viewport);
        let (display, controls) = if e.params.multiband() {
            (multi_display(e, cx, s), multi_controls(e, cx, s))
        } else {
            (single_display(e, cx, s), single_controls(e, cx, s))
        };
        plate_frame(s, vec![display, controls])
    }

    fn live_view(e: &KitEditor<Self>) -> BandParams {
        e.params.clone()
    }

    fn paint_live(
        view: &BandParams,
        live: &mut Live,
        bounds: &LiveBounds,
        window: &mut Window,
        cx: &mut App,
    ) {
        let viewport = window.viewport_size();
        let s = plate_scale((f32::from(viewport.width), f32::from(viewport.height)));
        let p = params_of(view);
        let bypassed = !p.power;
        let frame = live.frame;
        let gr = frame.map(|f| {
            if bypassed {
                0.0
            } else {
                f.gain_reduction_db.max(0.0)
            }
        });

        if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_HISTORY)) {
            let k = f32::from(b.size.width) / WIDE_W;
            paint_history(window, b, k, live, p);
        }
        if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_TRANSFER)) {
            let k = f32::from(b.size.width) / NARROW_W;
            paint_operating_point(window, b, k, live, p);
        }
        if let (Some(b), Some(gr)) = (bounds.get(LiveSlot::Plate(SLOT_GR_READOUT)), gr) {
            let (text, ink) = if gr > 0.05 {
                (format!("\u{2212}{gr:.1}"), accent())
            } else {
                ("0.0".to_string(), color(FAINT))
            };
            paint_reading(window, cx, b, &text, 20.0 * s, ink);
        }
        if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_GR_BAR)) {
            paint_gr_bar(window, b, gr, GR_SPAN_DB, accent(), s);
        }
        if let Some(frame) = frame {
            for (index, peak) in [frame.in_peak, frame.out_peak].into_iter().enumerate() {
                if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_METER + index)) {
                    paint_meter(window, cx, b, level_db(peak), accent(), s);
                }
            }
        }
        let reduction = frame.and(live.band_reduction());
        if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_BANDS)) {
            let k = f32::from(b.size.width) / BANDS_W;
            paint_band_live(window, cx, b, k, live, view, reduction);
        }
        for band in 0..BANDS {
            if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_BAND_GR + band)) {
                let off = bypassed || p.bands[band].bypass;
                let value = reduction.map(|r| if off { 0.0 } else { r[band].max(0.0) });
                paint_gr_bar(window, b, value, GR_SPAN_DB, accent(), s);
            }
        }
    }

    fn window_size(self) -> (f32, f32) {
        plate::WINDOW_SIZE
    }

    fn min_size(self) -> (f32, f32) {
        plate::MIN_WINDOW_SIZE
    }

    /// A soloed band is let go: a closed editor never leaves a track playing
    /// one band.
    fn on_close(self, params: &BandParams) -> Option<BandParams> {
        params.solo().map(|_| params.with("soloBand", -1.0))
    }

    fn key_down(e: &mut KitEditor<Self>, key: &str, cx: &mut Cx<Self>) -> bool {
        if key == "escape" && e.params.solo().is_some() {
            e.set_value("soloBand", -1.0, cx);
            return true;
        }
        false
    }

    fn mouse_move(e: &mut KitEditor<Self>, event: &MouseMoveEvent, cx: &mut Cx<Self>) {
        drag_to(e, event, cx);
    }

    fn mouse_up(e: &mut KitEditor<Self>, cx: &mut Cx<Self>) {
        if e.ui.drag.take().is_some() {
            cx.notify();
        }
    }
}

// ── Gestures ───────────────────────────────────────────────────────────────

fn threshold_at(e: &KitEditor<BandKind>, x: f32) -> Option<f32> {
    let b = e.live_bounds.get(LiveSlot::Plate(SLOT_TRANSFER))?;
    let k = f32::from(b.size.width) / NARROW_W;
    let x = (x - f32::from(b.origin.x)) / k;
    let db = RANGE_DB - (x - T_LEFT) / (T_RIGHT - T_LEFT) * RANGE_DB;
    Some((db.clamp(RANGE_DB, 0.0) * 10.0).round() / 10.0)
}

fn press_transfer(
    e: &mut KitEditor<BandKind>,
    (x, _): (f32, f32),
    event: &MouseDownEvent,
    cx: &mut Cx<BandKind>,
) {
    if event.click_count >= 2 {
        let default = BandParams::defaults(e.model).value("thresholdDb");
        e.set_value("thresholdDb", default, cx);
        return;
    }
    e.ui.drag = Some(CompDrag::Threshold);
    if let Some(db) = threshold_at(e, x) {
        e.set_value("thresholdDb", db, cx);
    }
    cx.notify();
}

fn press_crossover(
    e: &mut KitEditor<BandKind>,
    index: usize,
    event: &MouseDownEvent,
    cx: &mut Cx<BandKind>,
) {
    if event.click_count >= 2 {
        // A double-click puts the crossover back where it came from.
        let default = BandParams::defaults(e.model).crossovers()[index];
        e.set_value(CROSSOVER_IDS[index], default, cx);
        return;
    }
    e.ui.drag = Some(CompDrag::Crossover(index));
    cx.notify();
}

fn drag_to(e: &mut KitEditor<BandKind>, event: &MouseMoveEvent, cx: &mut Cx<BandKind>) {
    let Some(drag) = e.ui.drag else {
        return;
    };
    if event.pressed_button != Some(MouseButton::Left) {
        e.ui.drag = None;
        cx.notify();
        return;
    }
    let x = f32::from(event.position.x);
    match drag {
        CompDrag::Threshold => {
            if let Some(db) = threshold_at(e, x) {
                e.set_value("thresholdDb", db, cx);
            }
        }
        CompDrag::Crossover(i) => {
            let Some(b) = e.live_bounds.get(LiveSlot::Plate(SLOT_BANDS)) else {
                return;
            };
            let k = f32::from(b.size.width) / BANDS_W;
            let design_x = ((x - f32::from(b.origin.x)) / k).clamp(0.0, BANDS_W);
            let (low, high) = crossover_bounds(&e.params.crossovers(), i);
            let hz = hz_at_x(design_x, BANDS_W).clamp(low, high).round();
            e.set_value(CROSSOVER_IDS[i], hz, cx);
        }
    }
}

// ── Pieces ─────────────────────────────────────────────────────────────────

fn mode_pills(e: &KitEditor<BandKind>, cx: &mut Cx<BandKind>, s: f32) -> AnyElement {
    let multi = e.params.multiband();
    let pills: Vec<AnyElement> = [(Mode::Single, "Single"), (Mode::Multi, "Multi")]
        .into_iter()
        .enumerate()
        .map(|(index, (mode, label))| {
            pill(
                ("comp-mode", index),
                label,
                (mode == Mode::Multi) == multi,
                accent(),
                PillLook::words(28.0, 12.0, 12.0),
                true,
                s,
                e.click_cb(cx, move |this, cx| {
                    this.set_value("mode", mode.to_wire(), cx)
                }),
            )
            .into_any_element()
        })
        .collect();
    div()
        .flex()
        .flex_row()
        .gap(px(3.0 * s))
        .children(pills)
        .into_any_element()
}

#[allow(clippy::too_many_arguments)]
fn cell(
    cx: &mut Cx<BandKind>,
    id: &'static str,
    value: f32,
    look: KnobLook,
    k: KnobCell,
    ink: Rgba,
    s: f32,
) -> AnyElement {
    knob_reading(
        cx,
        format!("comp-{id}"),
        stage_dial(id),
        value,
        Some(readout(id, value)),
        look,
        k,
        ink,
        true,
        s,
        move |this: &mut KitEditor<BandKind>, value, cx| this.set_value(id, value, cx),
    )
}

fn swatch(ink: Rgba, s: f32) -> gpui::Div {
    div().w(px(12.0 * s)).h(px(2.0 * s)).bg(ink)
}

// ── Single band ────────────────────────────────────────────────────────────

fn single_display(e: &KitEditor<BandKind>, cx: &mut Cx<BandKind>, s: f32) -> AnyElement {
    let p = params_of(&e.params).clone();

    let history = display_box(s, 16.0, WIDE_W, |b, k, window, cx| {
        paint_history_grid(window, cx, b, k)
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
        plate::over_live(caption(
            "Level \u{00B7} Gain reduction",
            10.5,
            0.1,
            FAINT,
            s,
        )),
    ))
    .child(
        div()
            .absolute()
            .right(px(10.0 * s))
            .top(px(4.0 * s))
            .w(px(90.0 * s))
            .h(px(26.0 * s))
            .child(live_slot(&e.live_bounds, SLOT_GR_READOUT).size_full()),
    )
    .child(at(
        s,
        10.0,
        228.0,
        plate::over_live(
            div()
                .flex()
                .flex_row()
                .gap(px(14.0 * s))
                .child(legend(swatch(color(0x6a6a6a), s), "In", s))
                .child(legend(swatch(color(0xd9d9d9), s), "Out", s))
                .child(legend(swatch(accent(), s), "Reduction", s)),
        ),
    ));

    let curve = p.clone();
    let transfer_hit = div()
        .id("comp-transfer")
        .absolute()
        .left(px(T_LEFT * s))
        .top(px(T_TOP * s))
        .w(px((T_RIGHT - T_LEFT) * s))
        .h(px((T_BOTTOM - T_TOP) * s))
        .cursor(gpui::CursorStyle::ResizeLeftRight)
        .on_mouse_down(MouseButton::Left, e.press_cb(cx, press_transfer));
    let transfer = display_box(s, 630.0, NARROW_W, move |b, k, window, cx| {
        paint_transfer(window, cx, b, k, &curve)
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
                "{} \u{00B7} {:.1}:1 \u{00B7} knee {:.1}",
                fmt_db(p.threshold_db),
                p.ratio,
                p.knee_db
            ),
            11.0,
            SOFT,
            s,
        ),
    ))
    .child(transfer_hit);

    let subtitle = e.model.subtitle(&e.params);
    let right = vec![mode_pills(e, cx, s)];
    well(s, 7.0, 327.0)
        .child(header(e, cx, s, &subtitle, accent(), right))
        .child(div().relative().flex_1().child(history).child(transfer))
        .into_any_element()
}

fn history_y(db: f32) -> f32 {
    H_TOP + (db / RANGE_DB).clamp(0.0, 1.0) * (H_BOTTOM - H_TOP)
}

fn reduction_y(gr: f32) -> f32 {
    H_TOP + (gr / GR_SPAN_DB).clamp(0.0, 1.0) * (H_BOTTOM - H_TOP) * 0.5
}

fn paint_history_grid(window: &mut Window, cx: &mut App, b: Bounds<Pixels>, k: f32) {
    let (x0, y0) = plate::xy(b.origin);
    let at = |x: f32, y: f32| (x0 + x * k, y0 + y * k);
    let mono = plate::mono(FontWeight::NORMAL);
    for db in [-12.0, -24.0, -36.0, -48.0] {
        let y = history_y(db);
        paint_polyline(window, &[at(0.0, y), at(WIDE_W, y)], 1.0, color(GRID), None);
        paint_text(
            window,
            cx,
            &plate::signed(db).replace(".0", ""),
            mono.clone(),
            10.0 * k,
            color(AXIS_INK),
            at(WIDE_W - 8.0, y - 13.0),
            Align::Right,
        );
    }
    paint_polyline(
        window,
        &[at(0.0, H_TOP), at(WIDE_W, H_TOP)],
        1.0,
        color(0x3a3a3a),
        None,
    );
}

/// The input as a shaded level, the output over it, the threshold, and the
/// reduction hanging from the top.
fn paint_history(
    window: &mut Window,
    b: Bounds<Pixels>,
    k: f32,
    live: &Live,
    p: &compresser::Params,
) {
    let (x0, y0) = plate::xy(b.origin);
    let at = |x: f32, y: f32| (x0 + x * k, y0 + y * k);
    let dim = if p.power { 1.0 } else { 0.45 };
    let step = WIDE_W / (HISTORY - 1) as f32;
    let count = live.history_len();
    let points: Vec<(f32, f32, f32, f32)> = (0..count)
        .rev()
        .map(|age| {
            let point = live.history_at(age);
            let gr = if p.power { point.reduction_db } else { 0.0 };
            (WIDE_W - age as f32 * step, point.in_db, point.out_db, gr)
        })
        .collect();
    if points.len() >= 2 {
        let first = points[0].0;
        let input: Vec<(f32, f32)> = points
            .iter()
            .map(|&(x, db, _, _)| at(x, history_y(db)))
            .collect();
        let mut area = input.clone();
        area.push(at(WIDE_W, H_BOTTOM));
        area.push(at(first, H_BOTTOM));
        paint_polygon(window, &area, color_a(0xffffff, 0.06 * dim));
        paint_polyline(window, &input, 1.0, color_a(0x6a6a6a, dim), None);
        let output: Vec<(f32, f32)> = points
            .iter()
            .map(|&(x, _, db, _)| at(x, history_y(db)))
            .collect();
        paint_polyline(window, &output, 1.2 * k, color_a(0xd9d9d9, dim), None);
    }
    let y = history_y(p.threshold_db);
    paint_polyline(
        window,
        &[at(0.0, y), at(WIDE_W, y)],
        1.0,
        color_a(ACCENT, dim),
        Some((5.0 * k, 3.0 * k)),
    );
    if points.len() >= 2 {
        let first = points[0].0;
        let reduction: Vec<(f32, f32)> = points
            .iter()
            .map(|&(x, _, _, gr)| at(x, reduction_y(gr)))
            .collect();
        let mut area = reduction.clone();
        area.push(at(WIDE_W, H_TOP));
        area.push(at(first, H_TOP));
        paint_polygon(window, &area, color_a(ACCENT, 0.3 * dim));
        paint_polyline(window, &reduction, 1.6 * k, color_a(ACCENT, dim), None);
    }
}

fn transfer_x(db: f32) -> f32 {
    T_LEFT + (db.clamp(RANGE_DB, 0.0) - RANGE_DB) / -RANGE_DB * (T_RIGHT - T_LEFT)
}

fn transfer_y(db: f32) -> f32 {
    T_BOTTOM - (db.clamp(RANGE_DB, 0.0) - RANGE_DB) / -RANGE_DB * (T_BOTTOM - T_TOP)
}

/// The static curve: the knee shaded, the curve the DSP runs, and the
/// threshold's handle.
fn paint_transfer(
    window: &mut Window,
    cx: &mut App,
    b: Bounds<Pixels>,
    k: f32,
    p: &compresser::Params,
) {
    let (x0, y0) = plate::xy(b.origin);
    let at = |x: f32, y: f32| (x0 + x * k, y0 + y * k);
    let mono = plate::mono(FontWeight::NORMAL);
    for db in [-60.0, -48.0, -36.0, -24.0, -12.0, 0.0] {
        let (x, y) = (transfer_x(db), transfer_y(db));
        paint_polyline(
            window,
            &[at(x, T_TOP), at(x, T_BOTTOM)],
            1.0,
            color(GRID),
            None,
        );
        paint_polyline(
            window,
            &[at(T_LEFT, y), at(T_RIGHT, y)],
            1.0,
            color(GRID),
            None,
        );
        if db > RANGE_DB && db < 0.0 {
            let text = plate::signed(db).replace(".0", "");
            paint_text(
                window,
                cx,
                &text,
                mono.clone(),
                10.0 * k,
                color(AXIS_INK),
                at(x, T_BOTTOM + 6.0),
                Align::Center,
            );
            paint_text(
                window,
                cx,
                &text,
                mono.clone(),
                10.0 * k,
                color(AXIS_INK),
                at(T_LEFT - 6.0, y - 6.5),
                Align::Right,
            );
        }
    }
    paint_polyline(
        window,
        &[at(T_LEFT, T_BOTTOM), at(T_RIGHT, T_TOP)],
        1.0,
        color(0x3a3a3a),
        Some((3.0 * k, 3.0 * k)),
    );
    let dim = if p.power { 1.0 } else { 0.4 };
    let knee_left = transfer_x(p.threshold_db - p.knee_db * 0.5);
    let knee_right = transfer_x(p.threshold_db + p.knee_db * 0.5);
    let (kx, ky) = at(knee_left, T_TOP);
    paint_rect(
        window,
        (
            kx,
            ky,
            (knee_right - knee_left).max(0.0) * k,
            (T_BOTTOM - T_TOP) * k,
        ),
        color_a(ACCENT, 0.06 * dim),
    );
    let points: Vec<(f32, f32)> = (0..=160)
        .map(|i| {
            let input = RANGE_DB - RANGE_DB * i as f32 / 160.0;
            let output = if p.power {
                input - compresser::curve_reduction_db(input, p.threshold_db, p.ratio, p.knee_db)
            } else {
                input
            };
            at(transfer_x(input), transfer_y(output))
        })
        .collect();
    let mut area = points.clone();
    area.push(at(T_RIGHT, T_BOTTOM));
    area.push(at(T_LEFT, T_BOTTOM));
    paint_polygon(window, &area, color_a(ACCENT, 0.1 * dim));
    paint_polyline(window, &points, 2.0 * k, color_a(ACCENT, dim), None);

    // The threshold's handle: a line, and its tag at the foot.
    let x = transfer_x(p.threshold_db);
    paint_polyline(
        window,
        &[at(x, T_TOP), at(x, T_BOTTOM)],
        1.0,
        color_a(ACCENT, 0.7 * dim),
        None,
    );
    let (tx, ty) = at(x - 24.0, T_BOTTOM - 22.0);
    window.paint_quad(
        gpui::fill(
            plate::rect(tx, ty, 48.0 * k, 17.0 * k),
            color_a(ACCENT, dim),
        )
        .corner_radii(px(8.5 * k)),
    );
    paint_text(
        window,
        cx,
        &plate::signed(p.threshold_db),
        plate::mono(FontWeight::SEMIBOLD),
        10.5 * k,
        color(0x111111),
        at(x, T_BOTTOM - 21.0),
        Align::Center,
    );
}

/// The input level against what leaves the compressor, riding the curve.
fn paint_operating_point(
    window: &mut Window,
    b: Bounds<Pixels>,
    k: f32,
    live: &Live,
    p: &compresser::Params,
) {
    let Some(frame) = live.frame else {
        return;
    };
    if !p.power || frame.in_peak <= 1.0e-5 {
        return;
    }
    let (x0, y0) = plate::xy(b.origin);
    let input = level_db(frame.in_peak).clamp(RANGE_DB, 0.0);
    let output = input - compresser::curve_reduction_db(input, p.threshold_db, p.ratio, p.knee_db);
    let x = x0 + transfer_x(input) * k;
    let y = y0 + transfer_y(output) * k;
    paint_polyline(
        window,
        &[(x, y0 + T_BOTTOM * k), (x, y)],
        1.0,
        color_a(0xffffff, 0.35),
        Some((3.0 * k, 3.0 * k)),
    );
    plate::paint_dot(window, x, y, 5.0 * k, color(0x111111));
    plate::paint_dot(window, x, y, 3.5 * k, color(WHITE));
}

fn single_controls(e: &KitEditor<BandKind>, cx: &mut Cx<BandKind>, s: f32) -> AnyElement {
    let p = params_of(&e.params).clone();
    let big = KnobLook::small(60.0).styled(3.6, 17.5, FACE, FACE_EDGE, 0.8, 7.0, 1.8);
    let big_cell = KnobCell::new(100.0, 10.5, 12.5);
    let small = KnobLook::small(50.0).styled(4.5, 16.0, FACE, FACE_EDGE, 1.2, 7.0, 2.4);
    let small_cell = KnobCell::new(80.0, 10.0, 11.5);

    let gr_meter = div()
        .flex()
        .flex_col()
        .gap(px(6.0 * s))
        .child(caption("Gain reduction", 10.5, 0.1, SOFT, s))
        .child(
            div()
                .h(px(14.0 * s))
                .w_full()
                .child(live_slot(&e.live_bounds, SLOT_GR_BAR).size_full()),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .justify_between()
                .child(numbers("\u{2212}24", 10.0, FAINT, s))
                .child(numbers("\u{2212}12", 10.0, FAINT, s))
                .child(numbers("0", 10.0, FAINT, s)),
        );
    let compression = card(s)
        .child(card_title("Compression", s))
        .child(knob_row(vec![
            cell(
                cx,
                "thresholdDb",
                p.threshold_db,
                big,
                big_cell,
                accent(),
                s,
            ),
            cell(cx, "ratio", p.ratio, big, big_cell, accent(), s),
            cell(cx, "kneeDb", p.knee_db, big, big_cell, accent(), s),
        ]))
        .child(note(
            "Drag across the transfer curve to set the threshold; double-click puts it back.",
            s,
        ))
        .child(gr_meter);

    let sc_note = if p.sidechain_hpf_hz <= compresser::SIDECHAIN_OFF_HZ {
        "Off: the detector hears the full signal."
    } else {
        "The detector ignores the lows, so the bass does not pump the mix."
    };
    let timing = card(s)
        .child(card_title("Timing", s))
        .child(knob_row(vec![
            cell(cx, "attackMs", p.attack_ms, big, big_cell, accent(), s),
            cell(cx, "releaseMs", p.release_ms, big, big_cell, accent(), s),
        ]))
        .child(
            section("Sidechain", s).child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.0 * s))
                    .child(cell(
                        cx,
                        "sidechainHpfHz",
                        p.sidechain_hpf_hz,
                        small,
                        small_cell,
                        accent(),
                        s,
                    ))
                    .child(div().flex_1().min_w(px(0.0)).child(note(sc_note, s))),
            ),
        );

    deck(s, 341.0, 420.0)
        .p(px(14.0 * s))
        .gap(px(12.0 * s))
        .child(compression.w(px(350.0 * s)).flex_shrink_0())
        .child(timing.w(px(290.0 * s)).flex_shrink_0())
        .child(output_card(e, cx, s, &p, false).flex_1())
        .into_any_element()
}

/// Makeup (single band only), knee (multiband: shared by every band), mix,
/// output, and the In / Out meters.
fn output_card(
    e: &KitEditor<BandKind>,
    cx: &mut Cx<BandKind>,
    s: f32,
    p: &compresser::Params,
    multi: bool,
) -> gpui::Div {
    let look = KnobLook::small(50.0).styled(4.5, 16.0, FACE, FACE_EDGE, 1.2, 7.0, 2.4);
    let k = KnobCell::new(72.0, 10.0, 11.5);
    let ink = color(INK);
    let knobs = if multi {
        div()
            .flex()
            .flex_col()
            .gap(px(10.0 * s))
            .child(knob_row(vec![
                cell(cx, "kneeDb", p.knee_db, look, k, accent(), s),
                cell(cx, "mix", p.mix, look, k, ink, s),
            ]))
            .child(div().flex().flex_row().justify_center().child(cell(
                cx,
                "outputDb",
                p.output_db,
                look,
                k,
                ink,
                s,
            )))
    } else {
        knob_row(vec![
            cell(cx, "makeupDb", p.makeup_db, look, k, ink, s),
            cell(cx, "mix", p.mix, look, k, ink, s),
            cell(cx, "outputDb", p.output_db, look, k, ink, s),
        ])
    };
    card(s)
        .child(card_title("Output", s))
        .child(knobs)
        .child(io_meters(&e.live_bounds, SLOT_METER, s))
}

// ── Four bands ─────────────────────────────────────────────────────────────

fn band_x(hz: f32) -> f32 {
    x_of_hz(hz, BANDS_W)
}

fn multi_display(e: &KitEditor<BandKind>, cx: &mut Cx<BandKind>, s: f32) -> AnyElement {
    let params = e.params.clone();
    let dragging = match e.ui.drag {
        Some(CompDrag::Crossover(i)) => Some(i),
        _ => None,
    };
    let crossovers = params.crossovers();
    let base = params.clone();
    let over = params.clone();
    let mut bands = display_box(s, 16.0, BANDS_W, move |b, k, window, cx| {
        paint_band_grid(window, cx, b, k, &base)
    })
    .child(live_slot(&e.live_bounds, SLOT_BANDS).absolute().size_full())
    .child(over_canvas(move |b, window, cx| {
        let k = f32::from(b.size.width) / BANDS_W;
        paint_band_marks(window, cx, b, k, &over, dragging)
    }))
    .child(at(
        s,
        10.0,
        8.0,
        plate::over_live(caption("Bands \u{00B7} Reduction", 10.5, 0.1, FAINT, s)),
    ))
    .child(at_right(
        s,
        10.0,
        8.0,
        plate::over_live(note(
            "Drag a crossover to move a band edge \u{00B7} Esc lets a solo go",
            s,
        )),
    ));
    // A hit target per crossover: plain divs, so the root's mouse-move keeps
    // reaching the drag.
    for (index, hz) in crossovers.into_iter().enumerate() {
        let x = band_x(hz);
        bands = bands.child(
            div()
                .id(("comp-crossover", index))
                .absolute()
                .left(px((x - CROSSOVER_HIT) * s))
                .top(px(B_TOP * s))
                .w(px(2.0 * CROSSOVER_HIT * s))
                .h(px((B_BOTTOM - B_TOP) * s))
                .cursor(gpui::CursorStyle::ResizeLeftRight)
                .on_mouse_down(
                    MouseButton::Left,
                    e.press_cb(cx, move |this, _, event, cx| {
                        press_crossover(this, index, event, cx)
                    }),
                ),
        );
    }

    let subtitle = e.model.subtitle(&e.params);
    let right = vec![mode_pills(e, cx, s)];
    well(s, 7.0, 327.0)
        .child(header(e, cx, s, &subtitle, accent(), right))
        .child(div().relative().flex_1().child(bands))
        .into_any_element()
}

/// Under the live display: the frequency grid and its labels, and each band
/// shaded — darker when a solo leaves it out.
fn paint_band_grid(
    window: &mut Window,
    cx: &mut App,
    b: Bounds<Pixels>,
    k: f32,
    params: &BandParams,
) {
    let (x0, y0) = plate::xy(b.origin);
    let at = |x: f32, y: f32| (x0 + x * k, y0 + y * k);
    let edges = params.edges();
    let solo = params.solo();
    for band in 0..BANDS {
        let (left, right) = (band_x(edges[band]), band_x(edges[band + 1]));
        let shade = if solo.is_some_and(|s| s != band) {
            color_a(0x000000, 0.45)
        } else if band % 2 == 1 {
            color_a(0xffffff, 0.018)
        } else {
            color_a(0xffffff, 0.0)
        };
        let (bx, by) = at(left, B_TOP);
        paint_rect(
            window,
            (bx, by, (right - left).max(0.0) * k, (B_BOTTOM - B_TOP) * k),
            shade,
        );
    }
    for hz in FREQ_GRID {
        let x = band_x(hz);
        paint_polyline(
            window,
            &[at(x, B_TOP), at(x, B_BOTTOM)],
            1.0,
            color(GRID),
            None,
        );
    }
    let mono = plate::mono(FontWeight::NORMAL);
    for (hz, text) in [
        (20.0, "20"),
        (50.0, "50"),
        (100.0, "100"),
        (200.0, "200"),
        (500.0, "500"),
        (1_000.0, "1k"),
        (2_000.0, "2k"),
        (5_000.0, "5k"),
        (10_000.0, "10k"),
        (20_000.0, "20k"),
    ] {
        let x = band_x(hz);
        let align = if hz <= 20.0 {
            Align::Left
        } else if hz >= 20_000.0 {
            Align::Right
        } else {
            Align::Center
        };
        let x = if hz <= 20.0 {
            x + 6.0
        } else if hz >= 20_000.0 {
            x - 6.0
        } else {
            x
        };
        paint_text(
            window,
            cx,
            text,
            mono.clone(),
            10.0 * k,
            color(AXIS_INK),
            at(x, B_BOTTOM + 6.0),
            align,
        );
    }
    paint_polyline(
        window,
        &[at(0.0, B_TOP), at(BANDS_W, B_TOP)],
        1.0,
        color(0x3a3a3a),
        None,
    );
}

/// The live layer: the input spectrum and each band's reduction hanging from
/// the top of its stretch, with its reading.
fn paint_band_live(
    window: &mut Window,
    cx: &mut App,
    b: Bounds<Pixels>,
    k: f32,
    live: &Live,
    params: &BandParams,
    reduction: Option<[f32; BANDS]>,
) {
    let p = params_of(params);
    let (x0, y0) = plate::xy(b.origin);
    let at = |x: f32, y: f32| (x0 + x * k, y0 + y * k);
    let dim = if p.power { 1.0 } else { 0.45 };
    paint_spectrum(window, live, b, k, BANDS_W, (B_TOP, B_BOTTOM), dim);
    let edges = params.edges();
    let solo = params.solo();
    let mono = plate::mono(FontWeight::NORMAL);
    for band in 0..BANDS {
        let (left, right) = (band_x(edges[band]), band_x(edges[band + 1]));
        if right - left < 2.0 {
            continue;
        }
        let off = !p.power || p.bands[band].bypass;
        let muted = off || solo.is_some_and(|s| s != band);
        let gr = if off {
            0.0
        } else {
            reduction.map_or(0.0, |r| r[band].max(0.0))
        };
        let ink = if muted { 0x5a5a5a } else { ACCENT };
        let depth = (gr / GR_SPAN_DB).clamp(0.0, 1.0) * (B_BOTTOM - B_TOP) * 0.5;
        if depth > 0.3 {
            let (bx, by) = at(left + 1.0, B_TOP);
            let w = (right - left - 2.0).max(0.0) * k;
            paint_rect(window, (bx, by, w, depth * k), color_a(ink, 0.22));
            let (_, ly) = at(0.0, B_TOP + depth);
            paint_rect(window, (bx, ly - 1.0 * k, w, 1.5 * k), color(ink));
        }
        if right - left > 64.0 && reduction.is_some() && !off {
            paint_text(
                window,
                cx,
                &fmt_reduction(gr),
                mono.clone(),
                12.0 * k,
                if muted || gr <= 0.05 {
                    color(FAINT)
                } else {
                    color(WHITE)
                },
                at((left + right) * 0.5, B_TOP + 40.0),
                Align::Center,
            );
        }
    }
}

/// Over the live layer: each band's name and range, and the crossovers with
/// their tags — the one being dragged in the accent.
fn paint_band_marks(
    window: &mut Window,
    cx: &mut App,
    b: Bounds<Pixels>,
    k: f32,
    params: &BandParams,
    dragging: Option<usize>,
) {
    let p = params_of(params);
    let (x0, y0) = plate::xy(b.origin);
    let at = |x: f32, y: f32| (x0 + x * k, y0 + y * k);
    let edges = params.edges();
    let solo = params.solo();
    let mono = plate::mono(FontWeight::NORMAL);
    for band in 0..BANDS {
        let (left, right) = (band_x(edges[band]), band_x(edges[band + 1]));
        if right - left < 64.0 {
            continue;
        }
        let centre = (left + right) * 0.5;
        let (name, ink) = if p.bands[band].bypass {
            (format!("{} \u{00B7} bypassed", BAND_NAMES[band]), FAINT)
        } else if solo == Some(band) {
            (format!("{} \u{00B7} solo", BAND_NAMES[band]), SOLO)
        } else {
            (BAND_NAMES[band].to_string(), INK)
        };
        paint_text(
            window,
            cx,
            &plate::tracked(&name, 0.08),
            plate::outfit(FontWeight::MEDIUM),
            10.5 * k,
            color(ink),
            at(centre, B_TOP + 6.0),
            Align::Center,
        );
        paint_text(
            window,
            cx,
            &format!(
                "{}\u{2013}{} Hz",
                short_hz(edges[band]),
                short_hz(edges[band + 1])
            ),
            mono.clone(),
            10.0 * k,
            color(FAINT),
            at(centre, B_TOP + 21.0),
            Align::Center,
        );
    }
    for (index, hz) in params.crossovers().into_iter().enumerate() {
        let x = band_x(hz);
        let held = dragging == Some(index);
        let line = if held {
            accent()
        } else {
            color_a(0xffffff, 0.45)
        };
        paint_polyline(window, &[at(x, B_TOP), at(x, B_BOTTOM)], 1.0, line, None);
        let (tx, ty) = at(x - TAG_W * 0.5, B_BOTTOM - TAG_H - 6.0);
        window.paint_quad(
            gpui::fill(plate::rect(tx, ty, TAG_W * k, TAG_H * k), color(0x1d1d1d))
                .corner_radii(px(TAG_H * 0.5 * k))
                .border_widths(px(1.0))
                .border_color(if held { accent() } else { color(0x5a5a5a) }),
        );
        paint_text(
            window,
            cx,
            &short_hz(hz),
            mono.clone(),
            10.5 * k,
            color(if held { WHITE } else { INK }),
            at(x, B_BOTTOM - TAG_H - 4.5),
            Align::Center,
        );
    }
}

fn multi_controls(e: &KitEditor<BandKind>, cx: &mut Cx<BandKind>, s: f32) -> AnyElement {
    let p = params_of(&e.params).clone();
    let mut row = deck(s, 341.0, 420.0).p(px(14.0 * s)).gap(px(12.0 * s));
    for band in 0..BANDS {
        row = row.child(
            band_card(e, cx, s, &p, band)
                .w(px(186.0 * s))
                .flex_shrink_0(),
        );
    }
    row.child(output_card(e, cx, s, &p, true).flex_1())
        .into_any_element()
}

/// One band's card: its name and range, Bypass and Solo, its reduction and
/// its knobs. A soloed card stands out; while a band is soloed, the others
/// step back.
fn band_card(
    e: &KitEditor<BandKind>,
    cx: &mut Cx<BandKind>,
    s: f32,
    p: &compresser::Params,
    band: usize,
) -> gpui::Div {
    let edges = e.params.edges();
    let solo = e.params.solo();
    let soloed = solo == Some(band);
    let bypass = p.bands[band].bypass;
    let switches = columns(
        3.0,
        s,
        vec![
            pill(
                ("comp-band-bypass", band),
                "Bypass",
                bypass,
                color(0xb0b0b0),
                PillLook::words(26.0, 0.0, 11.5),
                true,
                s,
                e.click_cb(cx, move |this, cx| this.toggle(band_id(band, "Bypass"), cx)),
            )
            .into_any_element(),
            pill(
                ("comp-band-solo", band),
                "Solo",
                soloed,
                color(SOLO),
                PillLook::words(26.0, 0.0, 11.5),
                true,
                s,
                e.click_cb(cx, move |this, cx| {
                    let next = if this.params.solo() == Some(band) {
                        -1.0
                    } else {
                        band as f32
                    };
                    this.set_value("soloBand", next, cx)
                }),
            )
            .into_any_element(),
        ],
    );
    let look = KnobLook::small(44.0);
    let k = KnobCell::new(72.0, 9.5, 11.0);
    let b = p.bands[band];
    let knob = |cx: &mut Cx<BandKind>, field: &str, value: f32| {
        cell(cx, band_id(band, field), value, look, k, accent(), s)
    };
    let knobs =
        div()
            .flex()
            .flex_col()
            .gap(px(8.0 * s))
            .opacity(if bypass { 0.45 } else { 1.0 })
            .child(knob_row(vec![
                knob(cx, "ThresholdDb", b.threshold_db),
                knob(cx, "Ratio", b.ratio),
            ]))
            .child(knob_row(vec![
                knob(cx, "AttackMs", b.attack_ms),
                knob(cx, "ReleaseMs", b.release_ms),
            ]))
            .child(div().flex().flex_row().justify_center().child(knob(
                cx,
                "MakeupDb",
                b.makeup_db,
            )));
    let title = div()
        .flex()
        .flex_col()
        .child(card_title(BAND_NAMES[band], s))
        .child(numbers(
            format!(
                "{}\u{2013}{} Hz",
                short_hz(edges[band]),
                short_hz(edges[band + 1])
            ),
            10.5,
            FAINT,
            s,
        ));
    let card = card(s)
        .child(title)
        .child(switches)
        .child(
            div()
                .h(px(10.0 * s))
                .w_full()
                .child(live_slot(&e.live_bounds, SLOT_BAND_GR + band).size_full()),
        )
        .child(knobs);
    let card = if soloed {
        card.border_color(color(SOLO))
    } else {
        card
    };
    if solo.is_some() && !soloed {
        card.opacity(0.55)
    } else {
        card
    }
}

/// Opens with a track's Compressor insert.
pub fn open_compresser_editor(
    owner_bounds: Option<Bounds<Pixels>>,
    key: PluginInstanceKey,
    identity: ShellIdentity,
    host_ops: BuiltinEditorHostOps,
    on_close: Arc<dyn Fn(&mut Window, &mut App)>,
    cx: &mut App,
) -> Result<gpui::WindowHandle<CompressorWindow>, String> {
    plate::ensure_fonts(cx);
    open_kit_editor(
        BandKind::Comp,
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
    fn every_knob_spans_its_descriptor_row() {
        let ids = [
            "thresholdDb",
            "ratio",
            "attackMs",
            "releaseMs",
            "makeupDb",
            "sidechainHpfHz",
            "kneeDb",
            "mix",
            "outputDb",
        ]
        .into_iter()
        .chain((0..BANDS).flat_map(|b| {
            ["ThresholdDb", "Ratio", "AttackMs", "ReleaseMs", "MakeupDb"].map(|f| band_id(b, f))
        }));
        for id in ids {
            let dial = stage_dial(id);
            let (min, max, default) = compresser::descriptor()
                .params
                .iter()
                .find(|row| row.id == id)
                .map(|row| (row.min, row.max, row.default_value))
                .unwrap();
            assert_eq!((dial.min, dial.max), (min, max), "{id}");
            assert!(
                (dial.at(dial.norm(default)) - default).abs() <= 0.011 * default.abs().max(1.0),
                "{id} default does not round-trip"
            );
            assert_eq!(dial.at(0.0), min, "{id}");
            assert_eq!(dial.at(1.0), max, "{id}");
        }
    }

    #[test]
    fn readouts_read_like_a_compressor() {
        assert_eq!(readout("ratio", 4.0), "4.0:1");
        assert_eq!(readout("band2Ratio", 3.0), "3.0:1");
        assert_eq!(readout("sidechainHpfHz", 0.0), "Off");
        assert_eq!(readout("sidechainHpfHz", 120.0), "120 Hz");
        assert_eq!(readout("band1AttackMs", 20.0), "20 ms");
        assert_eq!(readout("band4MakeupDb", -2.0), "\u{2212}2.0 dB");
        assert_eq!(readout("mix", 100.0), "100%");
    }

    #[test]
    fn the_transfer_axes_cover_the_range() {
        assert_eq!(transfer_x(RANGE_DB), T_LEFT);
        assert_eq!(transfer_x(0.0), T_RIGHT);
        assert_eq!(transfer_y(0.0), T_TOP);
        assert!((band_x(20_000.0) - BANDS_W).abs() < 0.1);
    }

    #[test]
    fn closing_lets_a_solo_go_and_the_subtitle_names_the_mode() {
        let soloed = BandParams::defaults(BandKind::Comp)
            .with("mode", 1.0)
            .with("soloBand", 2.0);
        let closed = BandKind::Comp.on_close(&soloed).unwrap();
        assert_eq!(closed.solo(), None);
        assert!(BandKind::Comp.on_close(&closed).is_none());
        assert_eq!(
            BandKind::Comp.subtitle(&soloed),
            "Four-band compression".to_string()
        );
        assert_eq!(
            BandKind::Comp.subtitle(&BandParams::defaults(BandKind::Comp)),
            "Single-band compression".to_string()
        );
    }
}

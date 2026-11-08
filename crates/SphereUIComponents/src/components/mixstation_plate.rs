//! MixStation's editor, on the plate kit: a channel strip whose six modules
//! run in the order the user stacks them.
//!
//! The well holds the signal path — the rack, first module first, each row
//! with its stage's live levels and its switch; drag a row's grip to move
//! it, add a module from those not yet in the rack — and the selected
//! module's display, drawn from the DSP's own models
//! (`mixstation::filter_response_db`, `eq_response_db`, `comp_transfer_db`,
//! `limiter_transfer_db`, `drive_curve`, `width_of`) with handles to drag.
//! The deck holds that module's knobs and trim, and the strip's input and
//! output trims around its meters.
//!
//! Every rack move is plain slot params, written first slot to last (see
//! `mix_station_model`).

use std::sync::Arc;

use gpui::{
    div, px, AnyElement, App, Bounds, FontWeight, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, MouseMoveEvent, ParentElement, Pixels, Rgba, StatefulInteractiveElement,
    Styled, Window,
};
use mixstation::Params;

use crate::components::builtin_plugin_editor::builtin_mixstation_params;
use crate::components::builtin_plugin_editor_window::{BuiltinEditorHostOps, PluginInstanceKey};
use crate::components::context_menu::ContextMenuEntry;
use crate::components::fx_model::KnobSpec;
use crate::components::mix_station_model::{
    self as model, active, flag, module, order, value, with, with_order, Module, MODULES, SLOTS,
};
use crate::components::native_plugin_shell::ShellIdentity;
use crate::components::plate::{
    self, caption, card, card_title, color, color_a, deck, fmt_db, fmt_hz, fmt_hz_round, header,
    knob_reading, live_slot, numbers, paint_dot, paint_polygon, paint_polyline, paint_rect,
    paint_text, pill, plate_frame, plate_scale, switch, type_px, well, words, Align, Dial,
    DialUnit, KnobCell, KnobLook, PillLook, FACE, FACE_EDGE, FAINT, GHOST, INK, PALE, SOFT, WHITE,
};
use crate::components::plate_parts::{
    fmt_reduction, level_db, note, over_canvas, paint_gr_bar, paint_reading, paint_spectrum,
    section,
};
use crate::components::plugin_kit::{
    open_kit_editor, Cx, KitEditor, KitMenu, KitModel, KitPreset, KitWindow, LiveBounds, LiveSlot,
};
use crate::components::plugin_live::Live;
use crate::components::time_plate::{
    at, at_right, display_box, fmt_time, hz_at_x, io_meters, paint_meter, x_of_hz, AXIS_INK,
    FREQ_GRID, GRID, NARROW_W, WIDE_W,
};

/// MixStation's accent.
pub const ACCENT: u32 = 0x6b8afd;

const SLOT_DISPLAY: usize = 0;
const SLOT_RACK: usize = 1;
/// A rack row's stage levels, one per position.
const SLOT_STAGE: usize = 2;
const SLOT_METER: usize = 10;
const SLOT_GR_BAR: usize = 12;
const SLOT_GR_READOUT: usize = 13;

/// The menu that adds a module.
const ADD_MENU: u8 = 0;

/// The rack in its box: six rows under the caption.
const RACK_LEFT: f32 = 10.0;
const RACK_W: f32 = NARROW_W - 20.0;
const RACK_TOP: f32 = 32.0;
const ROW_H: f32 = 30.0;
const ROW_PITCH: f32 = 35.0;

/// The module display's plots, in its box.
const F_TOP: f32 = 34.0;
const F_BOTTOM: f32 = 218.0;
const T_LEFT: f32 = 186.0;
const T_RIGHT: f32 = 414.0;
const T_TOP: f32 = 36.0;
const T_BOTTOM: f32 = 212.0;
/// The ranges the displays span.
const CUT_TOP_DB: f32 = 6.0;
const CUT_FLOOR_DB: f32 = -36.0;
const EQ_RANGE_DB: f32 = 18.0;
const COMP_RANGE_DB: f32 = -60.0;
const LIMIT_RANGE_DB: f32 = -24.0;
/// Where the limiter's ceiling handle sits on its input axis.
const CEILING_HANDLE_DB: f32 = -2.0;
/// How near a press must land to a handle to take it, and its size.
const HANDLE_HIT: f32 = 12.0;
const HANDLE_R: f32 = 6.0;
/// The reduction scale's full span.
const GR_SPAN_DB: f32 = 24.0;
/// A stage bar's floor.
const STAGE_FLOOR_DB: f32 = -60.0;

fn accent() -> Rgba {
    color(ACCENT)
}

/// A point on a module display that drags a param.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handle {
    LowCut,
    HighCut,
    EqLow,
    EqLowMid,
    EqHighMid,
    EqHigh,
    Threshold,
    Ceiling,
}

impl Handle {
    /// The param a double-click on the handle puts back.
    fn reset_id(self) -> &'static str {
        match self {
            Handle::LowCut => "hpfHz",
            Handle::HighCut => "lpfHz",
            Handle::EqLow => "lowGainDb",
            Handle::EqLowMid => "lowMidGainDb",
            Handle::EqHighMid => "highMidGainDb",
            Handle::EqHigh => "highGainDb",
            Handle::Threshold => "compThresholdDb",
            Handle::Ceiling => "limiterCeilingDb",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MixDrag {
    /// A rack row held by its grip, and the position it would drop at.
    Reorder {
        code: u8,
        target: usize,
    },
    Handle(Handle),
}

#[derive(Debug, Default)]
pub struct MixUi {
    /// The module the display and the module card edit.
    pub selected: Option<u8>,
    pub drag: Option<MixDrag>,
}

/// What the overlay paints from: the params and the module on display.
pub struct MixView {
    params: Params,
    selected: Option<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MixStationPlate;

/// A MixStation insert's editor window.
pub type MixStationWindow = KitWindow<MixStationPlate>;

impl KitModel for MixStationPlate {
    type Params = Params;
    type Ui = MixUi;
    type LiveView = MixView;

    fn title(self) -> &'static str {
        "MixStation"
    }

    fn subtitle(self, params: &Params) -> String {
        format!(
            "Channel strip rack \u{00B7} {} / {SLOTS}",
            order(params).len()
        )
    }

    fn key(self) -> &'static str {
        "mixstation-plate"
    }

    fn defaults(self) -> Params {
        mixstation::default_params()
    }

    fn from_mirror(self, insert_id: &str) -> Option<Params> {
        builtin_mixstation_params(insert_id)
    }

    fn wire_values(self, params: &Params) -> Vec<(u32, f32)> {
        model::wire_values(params)
    }

    fn with(self, params: &Params, id: &str, value: f32) -> Params {
        with(params, id, value)
    }

    fn value(self, params: &Params, id: &str) -> f32 {
        value(params, id)
    }

    fn presets(self) -> Arc<Vec<KitPreset<Params>>> {
        model::presets()
    }

    fn preset_applied(self, current: &Params, preset: &Params) -> Params {
        model::preset_applied(current, preset)
    }

    fn knob(self, id: &str) -> Option<KnobSpec> {
        model::knob(id)
    }

    fn panel(e: &KitEditor<Self>, cx: &mut Cx<Self>) -> AnyElement {
        let s = plate_scale(e.viewport);
        plate_frame(s, vec![display(e, cx, s), controls(e, cx, s)])
    }

    fn live_view(e: &KitEditor<Self>) -> MixView {
        MixView {
            params: e.params.clone(),
            selected: selected(e),
        }
    }

    fn paint_live(
        view: &MixView,
        live: &mut Live,
        bounds: &LiveBounds,
        window: &mut Window,
        cx: &mut App,
    ) {
        let viewport = window.viewport_size();
        let s = plate_scale((f32::from(viewport.width), f32::from(viewport.height)));
        let p = &view.params;
        let bypassed = !p.power;
        if let (Some(b), Some(code @ (1 | 2))) =
            (bounds.get(LiveSlot::Plate(SLOT_DISPLAY)), view.selected)
        {
            let k = f32::from(b.size.width) / WIDE_W;
            let running = !bypassed && flag(p, module(code).map_or("", |m| m.enabled));
            let dim = if running { 1.0 } else { 0.5 };
            paint_spectrum(window, live, b, k, WIDE_W, (F_TOP, F_BOTTOM), dim);
        }
        let peaks = live.slot_peaks();
        for (position, code) in order(p).into_iter().enumerate() {
            if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_STAGE + position)) {
                let levels = match peaks {
                    Some((inputs, outputs)) if !bypassed && active(p, code) => {
                        Some((inputs[position], outputs[position]))
                    }
                    _ => None,
                };
                paint_stage(window, b, levels, s);
            }
        }
        let frame = live.frame;
        let gr = frame.map(|f| {
            if bypassed {
                0.0
            } else {
                f.gain_reduction_db.max(0.0)
            }
        });
        if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_GR_BAR)) {
            paint_gr_bar(window, b, gr, GR_SPAN_DB, accent(), s);
        }
        if let (Some(b), Some(gr)) = (bounds.get(LiveSlot::Plate(SLOT_GR_READOUT)), gr) {
            let ink = if gr > 0.05 { accent() } else { color(FAINT) };
            paint_reading(window, cx, b, &fmt_reduction(gr), 11.0 * s, ink);
        }
        if let Some(frame) = frame {
            for (index, peak) in [frame.in_peak, frame.out_peak].into_iter().enumerate() {
                if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_METER + index)) {
                    paint_meter(window, cx, b, level_db(peak), accent(), s);
                }
            }
        }
    }

    fn window_size(self) -> (f32, f32) {
        plate::WINDOW_SIZE
    }

    fn min_size(self) -> (f32, f32) {
        plate::MIN_WINDOW_SIZE
    }

    fn mouse_move(e: &mut KitEditor<Self>, event: &MouseMoveEvent, cx: &mut Cx<Self>) {
        drag_to(e, event, cx);
    }

    fn mouse_up(e: &mut KitEditor<Self>, cx: &mut Cx<Self>) {
        match e.ui.drag.take() {
            Some(MixDrag::Reorder { code, target }) => move_module(e, code, target, cx),
            Some(MixDrag::Handle(_)) => cx.notify(),
            None => {}
        }
    }

    fn own_menu(e: &KitEditor<Self>, _menu: u8) -> Vec<ContextMenuEntry> {
        let loaded = order(&e.params);
        let mut entries = vec![ContextMenuEntry::Header("Add module".into())];
        let missing: Vec<&Module> = MODULES
            .iter()
            .filter(|m| !loaded.contains(&m.code))
            .collect();
        if missing.is_empty() {
            entries.push(ContextMenuEntry::disabled_item(
                "All six modules are in the chain",
                "none",
            ));
        }
        entries.extend(missing.into_iter().map(|m| {
            ContextMenuEntry::item(
                format!("{} \u{2014} {}", m.name, m.hint),
                format!("add:{}", m.code),
            )
        }));
        entries
    }

    fn run_own_command(e: &mut KitEditor<Self>, command: &str, cx: &mut Cx<Self>) {
        if let Some(code) = command
            .strip_prefix("add:")
            .and_then(|c| c.parse::<u8>().ok())
        {
            add_module(e, code, cx);
        }
    }
}

// ── The rack ───────────────────────────────────────────────────────────────

/// The module on display: the one picked, else the first in the rack.
fn selected(e: &KitEditor<MixStationPlate>) -> Option<u8> {
    let loaded = order(&e.params);
    e.ui.selected
        .filter(|code| loaded.contains(code))
        .or_else(|| loaded.first().copied())
}

fn add_module(e: &mut KitEditor<MixStationPlate>, code: u8, cx: &mut Cx<MixStationPlate>) {
    let Some(m) = module(code) else {
        return;
    };
    let mut rack = order(&e.params);
    if rack.contains(&code) || rack.len() >= SLOTS {
        return;
    }
    rack.push(code);
    let next = with(&with_order(&e.params, &rack), m.enabled, 1.0);
    e.ui.selected = Some(code);
    e.set_params(next, cx);
}

fn remove_module(e: &mut KitEditor<MixStationPlate>, code: u8, cx: &mut Cx<MixStationPlate>) {
    let Some(m) = module(code) else {
        return;
    };
    let rack: Vec<u8> = order(&e.params)
        .into_iter()
        .filter(|c| *c != code)
        .collect();
    let next = with(&with_order(&e.params, &rack), m.enabled, 0.0);
    e.ui.selected = rack.first().copied();
    e.set_params(next, cx);
}

fn move_module(
    e: &mut KitEditor<MixStationPlate>,
    code: u8,
    target: usize,
    cx: &mut Cx<MixStationPlate>,
) {
    let next = moved(&e.params, code, target);
    e.set_params(next, cx);
    cx.notify();
}

/// The params with `code` moved to rack position `target`.
fn moved(params: &Params, code: u8, target: usize) -> Params {
    let mut rack = order(params);
    let Some(from) = rack.iter().position(|c| *c == code) else {
        return params.clone();
    };
    rack.remove(from);
    rack.insert(target.min(rack.len()), code);
    with_order(params, &rack)
}

/// Puts a module's knobs and trim back to their defaults.
fn reset_module(e: &mut KitEditor<MixStationPlate>, code: u8, cx: &mut Cx<MixStationPlate>) {
    let Some(m) = module(code) else {
        return;
    };
    let defaults = mixstation::default_params();
    let next = m
        .knobs
        .iter()
        .chain([&m.trim])
        .fold(e.params.clone(), |next, id| {
            with(&next, id, value(&defaults, id))
        });
    e.set_params(next, cx);
}

// ── Display geometry ───────────────────────────────────────────────────────

fn freq_x(hz: f32) -> f32 {
    x_of_hz(hz, WIDE_W)
}

fn cut_y(db: f32) -> f32 {
    F_TOP + ((CUT_TOP_DB - db) / (CUT_TOP_DB - CUT_FLOOR_DB)).clamp(0.0, 1.0) * (F_BOTTOM - F_TOP)
}

fn eq_y(db: f32) -> f32 {
    let mid = (F_TOP + F_BOTTOM) * 0.5;
    mid - (db / EQ_RANGE_DB).clamp(-1.0, 1.0) * (F_BOTTOM - F_TOP) * 0.5
}

/// The transfer plot's axis position, 0 at `range` to 1 at 0 dB.
fn transfer_unit(db: f32, range: f32) -> f32 {
    (db.clamp(range, 0.0) - range) / -range
}

fn transfer_point(input: f32, output: f32, range: f32) -> (f32, f32) {
    (
        T_LEFT + transfer_unit(input, range) * (T_RIGHT - T_LEFT),
        T_BOTTOM - transfer_unit(output, range) * (T_BOTTOM - T_TOP),
    )
}

/// The Drive curve's square plot: its left, top and side.
fn drive_plot() -> (f32, f32, f32) {
    let side = T_BOTTOM - T_TOP;
    (WIDE_W * 0.5 - side * 0.5, T_TOP, side)
}

/// Where each of the module's handles sits, in the display's design units.
fn handles(params: &Params, code: u8) -> Vec<(Handle, f32, f32)> {
    match code {
        1 => vec![
            (Handle::LowCut, freq_x(params.hpf_hz), cut_y(-3.0)),
            (Handle::HighCut, freq_x(params.lpf_hz), cut_y(-3.0)),
        ],
        2 => vec![
            (
                Handle::EqLow,
                freq_x(mixstation::LOW_SHELF_HZ),
                eq_y(params.low_gain_db),
            ),
            (
                Handle::EqLowMid,
                freq_x(params.low_mid_freq_hz),
                eq_y(params.low_mid_gain_db),
            ),
            (
                Handle::EqHighMid,
                freq_x(params.high_mid_freq_hz),
                eq_y(params.high_mid_gain_db),
            ),
            (
                Handle::EqHigh,
                freq_x(mixstation::HIGH_SHELF_HZ),
                eq_y(params.high_gain_db),
            ),
        ],
        3 => {
            let threshold = params.comp_threshold_db;
            let (x, y) = transfer_point(
                threshold,
                mixstation::comp_transfer_db(params, threshold),
                COMP_RANGE_DB,
            );
            vec![(Handle::Threshold, x, y)]
        }
        6 => {
            let (x, y) =
                transfer_point(CEILING_HANDLE_DB, params.limiter_ceiling_db, LIMIT_RANGE_DB);
            vec![(Handle::Ceiling, x, y)]
        }
        _ => Vec::new(),
    }
}

/// A window point as design units on the display.
fn display_point(e: &KitEditor<MixStationPlate>, (x, y): (f32, f32)) -> Option<(f32, f32)> {
    let b = e.live_bounds.get(LiveSlot::Plate(SLOT_DISPLAY))?;
    let k = f32::from(b.size.width) / WIDE_W;
    Some((
        (x - f32::from(b.origin.x)) / k,
        (y - f32::from(b.origin.y)) / k,
    ))
}

fn press_display(
    e: &mut KitEditor<MixStationPlate>,
    at: (f32, f32),
    event: &MouseDownEvent,
    cx: &mut Cx<MixStationPlate>,
) {
    let (Some(code), Some((x, y))) = (selected(e), display_point(e, at)) else {
        return;
    };
    let hit = handles(&e.params, code)
        .into_iter()
        .map(|(handle, hx, hy)| (handle, ((hx - x).powi(2) + (hy - y).powi(2)).sqrt()))
        .filter(|(_, distance)| *distance <= HANDLE_HIT)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(handle, _)| handle);
    let Some(handle) = hit else {
        return;
    };
    if event.click_count >= 2 {
        let id = handle.reset_id();
        e.set_value(id, model::default_value(id), cx);
        return;
    }
    e.ui.drag = Some(MixDrag::Handle(handle));
    cx.notify();
}

fn round_to(v: f32, step: f32) -> f32 {
    (v / step).round() * step
}

/// The params after handle `handle` is dragged to design point `(x, y)`.
fn dragged(params: &Params, handle: Handle, (x, y): (f32, f32)) -> Params {
    let hz = hz_at_x(x.clamp(0.0, WIDE_W), WIDE_W);
    let gain = || {
        let mid = (F_TOP + F_BOTTOM) * 0.5;
        let db = (mid - y) / ((F_BOTTOM - F_TOP) * 0.5) * EQ_RANGE_DB;
        round_to(db.clamp(-EQ_RANGE_DB, EQ_RANGE_DB), 0.1)
    };
    match handle {
        Handle::LowCut => with(params, "hpfHz", round_to(hz.clamp(20.0, 500.0), 1.0)),
        Handle::HighCut => with(params, "lpfHz", round_to(hz.clamp(1_000.0, 20_000.0), 10.0)),
        Handle::EqLow => with(params, "lowGainDb", gain()),
        Handle::EqHigh => with(params, "highGainDb", gain()),
        Handle::EqLowMid => with(
            &with(params, "lowMidGainDb", gain()),
            "lowMidFreqHz",
            round_to(hz.clamp(80.0, 2_000.0), 1.0),
        ),
        Handle::EqHighMid => with(
            &with(params, "highMidGainDb", gain()),
            "highMidFreqHz",
            round_to(hz.clamp(500.0, 12_000.0), 1.0),
        ),
        Handle::Threshold => {
            let unit = ((x - T_LEFT) / (T_RIGHT - T_LEFT)).clamp(0.0, 1.0);
            let db = COMP_RANGE_DB * (1.0 - unit);
            with(params, "compThresholdDb", round_to(db, 0.1))
        }
        Handle::Ceiling => {
            let unit = ((T_BOTTOM - y) / (T_BOTTOM - T_TOP)).clamp(0.0, 1.0);
            let db = LIMIT_RANGE_DB * (1.0 - unit);
            with(
                params,
                "limiterCeilingDb",
                round_to(db.clamp(-12.0, 0.0), 0.1),
            )
        }
    }
}

fn drag_to(
    e: &mut KitEditor<MixStationPlate>,
    event: &MouseMoveEvent,
    cx: &mut Cx<MixStationPlate>,
) {
    let Some(drag) = e.ui.drag else {
        return;
    };
    if event.pressed_button != Some(MouseButton::Left) {
        e.ui.drag = None;
        cx.notify();
        return;
    }
    let at = (f32::from(event.position.x), f32::from(event.position.y));
    match drag {
        MixDrag::Reorder { code, target } => {
            let Some(list) = e.live_bounds.get(LiveSlot::Plate(SLOT_RACK)) else {
                return;
            };
            let k = f32::from(list.size.width) / RACK_W;
            let count = order(&e.params).len().max(1);
            let row = ((at.1 - f32::from(list.origin.y)) / (ROW_PITCH * k)).floor();
            let next = (row.max(0.0) as usize).min(count - 1);
            if next != target {
                e.ui.drag = Some(MixDrag::Reorder { code, target: next });
                cx.notify();
            }
        }
        MixDrag::Handle(handle) => {
            let Some(point) = display_point(e, at) else {
                return;
            };
            let next = dragged(&e.params, handle, point);
            e.set_params(next, cx);
        }
    }
}

// ── The well ───────────────────────────────────────────────────────────────

fn display(e: &KitEditor<MixStationPlate>, cx: &mut Cx<MixStationPlate>, s: f32) -> AnyElement {
    let rack = rack_box(e, cx, s);
    let shown = module_box(e, cx, s);
    let subtitle = e.model.subtitle(&e.params);
    let add_open = order(&e.params).len() < SLOTS;
    let add = div()
        .id("mix-add-header")
        .flex()
        .flex_row()
        .items_center()
        .h(px(28.0 * s))
        .px(px(12.0 * s))
        .rounded(px(3.0 * s))
        .border(px(1.0))
        .border_color(color(plate::WELL_EDGE))
        .bg(color(plate::GROUND))
        .text_size(type_px(12.0, s))
        .text_color(color(if add_open { INK } else { GHOST }))
        .child("+ Add module");
    let add = if add_open {
        add.cursor(gpui::CursorStyle::PointingHand)
            .hover(|style| style.bg(color(0x262626)))
            .on_mouse_down(
                MouseButton::Left,
                e.press_cb(cx, |this, (x, y), _, cx| {
                    this.open_menu(KitMenu::Own(ADD_MENU, x, y), cx)
                }),
            )
            .into_any_element()
    } else {
        add.into_any_element()
    };
    let right = vec![add];
    well(s, 7.0, 327.0)
        .child(header(e, cx, s, &subtitle, accent(), right))
        .child(div().relative().flex_1().child(rack).child(shown))
        .into_any_element()
}

/// Six little dots: a row's grip.
fn grip(s: f32) -> gpui::Div {
    let dot = || div().size(px(2.5 * s)).rounded_full().bg(color(0x7a7a7a));
    let column = || {
        div()
            .flex()
            .flex_col()
            .gap(px(2.5 * s))
            .child(dot())
            .child(dot())
            .child(dot())
    };
    div()
        .flex()
        .flex_row()
        .gap(px(2.5 * s))
        .child(column())
        .child(column())
}

fn rack_box(e: &KitEditor<MixStationPlate>, cx: &mut Cx<MixStationPlate>, s: f32) -> gpui::Div {
    let rack = order(&e.params);
    let current = selected(e);
    let power = e.power();
    let dragging = match e.ui.drag {
        Some(MixDrag::Reorder { code, target }) => Some((code, target)),
        _ => None,
    };
    let row_frame = |position: usize| {
        div()
            .absolute()
            .left(px(RACK_LEFT * s))
            .top(px((RACK_TOP + position as f32 * ROW_PITCH) * s))
            .w(px(RACK_W * s))
            .h(px(ROW_H * s))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0 * s))
            .px(px(8.0 * s))
            .rounded(px(3.0 * s))
            .border(px(1.0))
    };

    let mut rows: Vec<AnyElement> = Vec::with_capacity(SLOTS);
    for (position, code) in rack.iter().copied().enumerate() {
        let m = module(code).expect("the rack holds known modules");
        let on = flag(&e.params, m.enabled);
        let is_selected = current == Some(code);
        let held = dragging.is_some_and(|(c, _)| c == code);
        let drop_here = dragging.is_some_and(|(c, target)| c != code && target == position);
        let border = if held || drop_here {
            accent()
        } else if is_selected {
            color_a(ACCENT, 0.6)
        } else {
            color(0x262626)
        };
        let handle = div()
            .id(("mix-grip", position))
            .flex()
            .items_center()
            .justify_center()
            .w(px(12.0 * s))
            .h_full()
            .cursor(gpui::CursorStyle::OpenHand)
            .child(grip(s))
            .on_mouse_down(
                MouseButton::Left,
                e.press_cb(cx, move |this, _, _, cx| {
                    this.ui.drag = Some(MixDrag::Reorder {
                        code,
                        target: position,
                    });
                    this.ui.selected = Some(code);
                    cx.notify();
                }),
            );
        let name_ink = if on && power { WHITE } else { SOFT };
        let row = row_frame(position)
            .id(("mix-row", position))
            .border_color(border)
            .bg(color(if is_selected { 0x1f1f1f } else { 0x161616 }))
            .opacity(if held { 0.6 } else { 1.0 })
            .cursor(gpui::CursorStyle::PointingHand)
            .child(handle)
            .child(div().w(px(14.0 * s)).child(numbers(
                format!("{}", position + 1),
                11.0,
                FAINT,
                s,
            )))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .child(words(m.name, 13.0, name_ink, s).font_weight(FontWeight::MEDIUM)),
            )
            .child(
                div()
                    .w(px(78.0 * s))
                    .h(px(11.0 * s))
                    .child(live_slot(&e.live_bounds, SLOT_STAGE + position).size_full()),
            )
            .child(switch(
                ("mix-on", position),
                on,
                accent(),
                true,
                s,
                e.click_cb(cx, move |this, cx| {
                    if let Some(m) = module(code) {
                        this.toggle(m.enabled, cx)
                    }
                }),
            ))
            .on_click(e.click_cb(cx, move |this, cx| {
                this.ui.selected = Some(code);
                cx.notify();
            }));
        rows.push(row.into_any_element());
    }
    for position in rack.len()..SLOTS {
        let empty = row_frame(position).border_color(color(0x1e1e1e));
        let row = if position == rack.len() {
            empty
                .id("mix-add-row")
                .justify_center()
                .border_color(color(0x333333))
                .cursor(gpui::CursorStyle::PointingHand)
                .hover(|style| style.bg(color(0x1a1a1a)))
                .child(words("+ Add module", 12.0, PALE, s))
                .on_mouse_down(
                    MouseButton::Left,
                    e.press_cb(cx, |this, (x, y), _, cx| {
                        this.open_menu(KitMenu::Own(ADD_MENU, x, y), cx)
                    }),
                )
                .into_any_element()
        } else {
            empty
                .justify_center()
                .child(words("Empty slot", 11.5, 0x4a4a4a, s))
                .into_any_element()
        };
        rows.push(row);
    }

    let hint = if rack.len() > 1 {
        "in \u{2192} out \u{00B7} drag a grip to reorder"
    } else {
        "in \u{2192} out"
    };
    display_box(s, 16.0, NARROW_W, |_, _, _, _| {})
        .child(
            div()
                .absolute()
                .left(px(RACK_LEFT * s))
                .top(px(RACK_TOP * s))
                .w(px(RACK_W * s))
                .h(px(ROW_PITCH * SLOTS as f32 * s))
                .child(live_slot(&e.live_bounds, SLOT_RACK).size_full()),
        )
        .child(at(
            s,
            10.0,
            8.0,
            caption("Signal path", 10.5, 0.1, FAINT, s),
        ))
        .child(at_right(
            s,
            10.0,
            9.0,
            div()
                .flex()
                .flex_row()
                .gap(px(10.0 * s))
                .child(words(hint, 10.5, 0x6f6f6f, s))
                .child(numbers(format!("{} / {SLOTS}", rack.len()), 11.0, SOFT, s)),
        ))
        .children(rows)
}

/// The selected module's display: its picture from the params, its handles,
/// and — for Filters and EQ — the live spectrum under them.
fn module_box(e: &KitEditor<MixStationPlate>, cx: &mut Cx<MixStationPlate>, s: f32) -> gpui::Div {
    let Some(code) = selected(e) else {
        return display_box(s, 394.0, WIDE_W, |_, _, _, _| {}).child(
            div()
                .absolute()
                .inset_0()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(6.0 * s))
                .child(words("No modules in the chain", 15.0, PALE, s))
                .child(words(
                    "Add one from the signal path to start the strip.",
                    11.5,
                    FAINT,
                    s,
                )),
        );
    };
    let p = e.params.clone();
    let m = module(code).expect("selected module is known");
    let running = flag(&p, m.enabled) && p.power;
    let dragging = match e.ui.drag {
        Some(MixDrag::Handle(handle)) => Some(handle),
        _ => None,
    };
    let (title, legend) = match code {
        1 => (
            "Response",
            format!("{} \u{2013} {}", fmt_hz_round(p.hpf_hz), fmt_hz(p.lpf_hz)),
        ),
        2 => ("Response", "Shelves at 100 Hz and 10 kHz".to_string()),
        3 => (
            "Transfer",
            format!(
                "{} \u{00B7} {:.1}:1",
                fmt_db(p.comp_threshold_db),
                p.comp_ratio
            ),
        ),
        4 => ("Curve", "in \u{2192} out".to_string()),
        5 => ("Image", describe_width(p.width_pct).to_string()),
        _ => (
            "Transfer",
            format!("ceiling {}", fmt_db(p.limiter_ceiling_db)),
        ),
    };
    let state = if !p.power {
        Some("MixStation bypassed")
    } else if !flag(&p, m.enabled) {
        Some("Module off")
    } else {
        None
    };
    let picture = module_picture(&p, code, e.sample_rate);
    let marks = handles(&p, code);
    let base = p.clone();
    let mut boxed = display_box(s, 394.0, WIDE_W, move |b, k, window, cx| {
        paint_module_grid(window, cx, b, k, &base, code)
    })
    .child(
        live_slot(&e.live_bounds, SLOT_DISPLAY)
            .absolute()
            .size_full(),
    )
    .child(over_canvas(move |b, window, cx| {
        let k = f32::from(b.size.width) / WIDE_W;
        paint_module_picture(window, cx, b, k, &picture, &marks, running, dragging)
    }))
    .child(at(
        s,
        10.0,
        8.0,
        plate::over_live(caption(
            &format!("{title} \u{00B7} {}", m.name),
            10.5,
            0.1,
            FAINT,
            s,
        )),
    ))
    .child(at_right(
        s,
        10.0,
        8.0,
        plate::over_live(
            div()
                .flex()
                .flex_row()
                .gap(px(10.0 * s))
                .children(state.map(|text| words(text, 11.0, FAINT, s)))
                .child(numbers(legend, 11.0, SOFT, s)),
        ),
    ));
    if !handles(&p, code).is_empty() {
        // A plain hit target over the display: no occlusion, so the root's
        // mouse-move keeps reaching a handle's drag.
        boxed = boxed.child(
            div()
                .id("mix-display")
                .absolute()
                .inset_0()
                .on_mouse_down(MouseButton::Left, e.press_cb(cx, press_display)),
        );
    }
    boxed
}

fn describe_width(width_pct: f32) -> &'static str {
    if width_pct < 0.5 {
        "Mono"
    } else if (width_pct - 100.0).abs() < 0.5 {
        "As the source"
    } else if width_pct > 100.0 {
        "Wider than the source"
    } else {
        "Narrower than the source"
    }
}

/// What a module's display draws, computed once per render from the DSP's
/// own models, in design units.
enum Picture {
    /// A response across frequency, its zero line, and whether to shade it.
    Response {
        points: Vec<(f32, f32)>,
        zero_y: f32,
        shaded: bool,
    },
    /// A transfer curve, and the limiter's ceiling line.
    Transfer {
        points: Vec<(f32, f32)>,
        ceiling_y: Option<f32>,
    },
    /// The waveshaper's curve.
    Curve(Vec<(f32, f32)>),
    /// Where a hard-panned pair lands: left and right, as x.
    Image { left: f32, right: f32 },
}

fn module_picture(p: &Params, code: u8, sample_rate: f32) -> Picture {
    const POINTS: usize = 180;
    match code {
        1 | 2 => {
            let freqs: Vec<f32> = (0..POINTS)
                .map(|i| hz_at_x(i as f32 / (POINTS - 1) as f32 * WIDE_W, WIDE_W))
                .collect();
            let (curve, y_of): (Vec<f32>, fn(f32) -> f32) = if code == 1 {
                (
                    mixstation::filter_response_db(p, sample_rate, &freqs),
                    cut_y,
                )
            } else {
                (mixstation::eq_response_db(p, sample_rate, &freqs), eq_y)
            };
            Picture::Response {
                points: freqs
                    .iter()
                    .zip(&curve)
                    .map(|(hz, db)| (freq_x(*hz), y_of(*db)))
                    .collect(),
                zero_y: y_of(0.0),
                shaded: code == 2,
            }
        }
        3 | 6 => {
            let range = if code == 3 {
                COMP_RANGE_DB
            } else {
                LIMIT_RANGE_DB
            };
            let points = (0..=120)
                .map(|i| {
                    let input = range - range * i as f32 / 120.0;
                    let output = if code == 3 {
                        mixstation::comp_transfer_db(p, input)
                    } else {
                        mixstation::limiter_transfer_db(p, input)
                    };
                    transfer_point(input, output, range)
                })
                .collect();
            let ceiling_y = (code == 6).then(|| transfer_point(0.0, p.limiter_ceiling_db, range).1);
            Picture::Transfer { points, ceiling_y }
        }
        4 => {
            let (left, top, side) = drive_plot();
            let span = 1.2;
            let points = (0..=160)
                .map(|i| {
                    let x = -span + 2.0 * span * i as f32 / 160.0;
                    let y = mixstation::drive_curve(p, x).clamp(-span, span);
                    (
                        left + (x + span) / (2.0 * span) * side,
                        top + side - (y + span) / (2.0 * span) * side,
                    )
                })
                .collect();
            Picture::Curve(points)
        }
        _ => {
            let (left, right) = mixstation::width_of(p, 1.0, -1.0);
            Picture::Image { left, right }
        }
    }
}

/// The width display's centre and how far a hard pan reaches.
const IMAGE_CENTRE: (f32, f32) = (WIDE_W * 0.5, 128.0);
const IMAGE_REACH: f32 = 150.0;

/// Under the live layer: each display's grid, axes and labels.
fn paint_module_grid(
    window: &mut Window,
    cx: &mut App,
    b: Bounds<Pixels>,
    k: f32,
    p: &Params,
    code: u8,
) {
    let (x0, y0) = plate::xy(b.origin);
    let at = |x: f32, y: f32| (x0 + x * k, y0 + y * k);
    let mono = plate::mono(FontWeight::NORMAL);
    let label = |window: &mut Window, cx: &mut App, text: &str, (x, y): (f32, f32), align| {
        paint_text(
            window,
            cx,
            text,
            plate::mono(FontWeight::NORMAL),
            10.0 * k,
            color(AXIS_INK),
            at(x, y),
            align,
        )
    };
    match code {
        1 | 2 => {
            for hz in FREQ_GRID {
                let x = freq_x(hz);
                paint_polyline(
                    window,
                    &[at(x, F_TOP), at(x, F_BOTTOM)],
                    1.0,
                    color(GRID),
                    None,
                );
            }
            for (hz, text) in [
                (50.0, "50"),
                (100.0, "100"),
                (200.0, "200"),
                (500.0, "500"),
                (1_000.0, "1k"),
                (2_000.0, "2k"),
                (5_000.0, "5k"),
                (10_000.0, "10k"),
            ] {
                label(
                    window,
                    cx,
                    text,
                    (freq_x(hz), F_BOTTOM + 7.0),
                    Align::Center,
                );
            }
            let (grid, y_of): (&[f32], fn(f32) -> f32) = if code == 1 {
                (&[0.0, -12.0, -24.0], cut_y)
            } else {
                (&[12.0, 6.0, 0.0, -6.0, -12.0], eq_y)
            };
            for db in grid {
                let y = y_of(*db);
                let ink = if *db == 0.0 { 0x2e2e2e } else { GRID };
                paint_polyline(window, &[at(0.0, y), at(WIDE_W, y)], 1.0, color(ink), None);
                let text = if *db > 0.0 {
                    format!("+{db:.0}")
                } else {
                    plate::signed(*db).replace(".0", "")
                };
                paint_text(
                    window,
                    cx,
                    &text,
                    mono.clone(),
                    10.0 * k,
                    color(AXIS_INK),
                    at(8.0, y - 14.0),
                    Align::Left,
                );
            }
        }
        3 | 6 => {
            let range = if code == 3 {
                COMP_RANGE_DB
            } else {
                LIMIT_RANGE_DB
            };
            let step = if code == 3 { 12.0 } else { 6.0 };
            let mut db = range;
            while db <= 0.01 {
                let (x, y) = transfer_point(db, db, range);
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
                if db < -0.01 && db > range + 0.01 {
                    let text = plate::signed(db).replace(".0", "");
                    label(window, cx, &text, (x, T_BOTTOM + 6.0), Align::Center);
                    label(window, cx, &text, (T_LEFT - 6.0, y - 6.5), Align::Right);
                }
                db += step;
            }
            let (ax, ay) = transfer_point(range, range, range);
            let (bx, by) = transfer_point(0.0, 0.0, range);
            paint_polyline(
                window,
                &[at(ax, ay), at(bx, by)],
                1.0,
                color(0x3a3a3a),
                Some((3.0 * k, 3.0 * k)),
            );
            let what = if code == 3 {
                format!("attack {}", fmt_time(p.comp_attack_ms))
            } else {
                format!("release {}", fmt_time(p.limiter_release_ms))
            };
            label(
                window,
                cx,
                &what,
                (T_RIGHT + 12.0, T_BOTTOM - 14.0),
                Align::Left,
            );
        }
        4 => {
            let (left, top, side) = drive_plot();
            paint_polyline(
                window,
                &[
                    at(left, top + side * 0.5),
                    at(left + side, top + side * 0.5),
                ],
                1.0,
                color(0x2e2e2e),
                None,
            );
            paint_polyline(
                window,
                &[
                    at(left + side * 0.5, top),
                    at(left + side * 0.5, top + side),
                ],
                1.0,
                color(0x2e2e2e),
                None,
            );
            paint_polyline(
                window,
                &[at(left, top + side), at(left + side, top)],
                1.0,
                color(0x3a3a3a),
                Some((3.0 * k, 3.0 * k)),
            );
            label(
                window,
                cx,
                &format!(
                    "drive {}% \u{00B7} character {}%",
                    p.sat_drive_pct.round(),
                    p.sat_character_pct.round()
                ),
                (left + side + 16.0, top + side - 14.0),
                Align::Left,
            );
        }
        _ => {
            let (cx0, cy0) = IMAGE_CENTRE;
            paint_polyline(
                window,
                &[
                    at(cx0 - IMAGE_REACH * 1.25, cy0),
                    at(cx0 + IMAGE_REACH * 1.25, cy0),
                ],
                1.0,
                color(GRID),
                None,
            );
            for side in [-1.0f32, 1.0] {
                let x = cx0 + side * IMAGE_REACH;
                paint_polyline(
                    window,
                    &[at(x, cy0 - 30.0), at(x, cy0 + 30.0)],
                    1.0,
                    color(0x3a3a3a),
                    Some((3.0 * k, 3.0 * k)),
                );
            }
            paint_polyline(
                window,
                &[at(cx0, cy0 - 12.0), at(cx0, cy0 + 12.0)],
                1.0,
                color(0x3a3a3a),
                None,
            );
            label(
                window,
                cx,
                "L",
                (cx0 - IMAGE_REACH, cy0 - 50.0),
                Align::Center,
            );
            label(window, cx, "C", (cx0, cy0 - 50.0), Align::Center);
            label(
                window,
                cx,
                "R",
                (cx0 + IMAGE_REACH, cy0 - 50.0),
                Align::Center,
            );
            label(
                window,
                cx,
                "a hard-panned pair \u{00B7} dashed: unchanged",
                (cx0, F_BOTTOM - 6.0),
                Align::Center,
            );
        }
    }
}

/// Over the live layer: the module's curve and its handles.
#[allow(clippy::too_many_arguments)]
fn paint_module_picture(
    window: &mut Window,
    cx: &mut App,
    b: Bounds<Pixels>,
    k: f32,
    picture: &Picture,
    marks: &[(Handle, f32, f32)],
    running: bool,
    dragging: Option<Handle>,
) {
    let (x0, y0) = plate::xy(b.origin);
    let at = |x: f32, y: f32| (x0 + x * k, y0 + y * k);
    let ink = if running { accent() } else { color(0x6a6a6a) };
    let to_px = |points: &[(f32, f32)]| -> Vec<(f32, f32)> {
        points.iter().map(|&(x, y)| at(x, y)).collect()
    };
    match picture {
        Picture::Response {
            points,
            zero_y,
            shaded,
        } => {
            let line = to_px(points);
            if *shaded {
                let mut area = line.clone();
                area.push(at(WIDE_W, *zero_y));
                area.push(at(0.0, *zero_y));
                paint_polygon(
                    window,
                    &area,
                    color_a(ACCENT, if running { 0.16 } else { 0.06 }),
                );
            }
            paint_polyline(window, &line, 2.0 * k, ink, None);
        }
        Picture::Transfer { points, ceiling_y } => {
            if let Some(y) = ceiling_y {
                paint_polyline(
                    window,
                    &[at(T_LEFT, *y), at(T_RIGHT, *y)],
                    1.0,
                    color_a(ACCENT, 0.6),
                    Some((4.0 * k, 3.0 * k)),
                );
            }
            let line = to_px(points);
            let mut area = line.clone();
            area.push(at(T_RIGHT, T_BOTTOM));
            area.push(at(T_LEFT, T_BOTTOM));
            paint_polygon(
                window,
                &area,
                color_a(ACCENT, if running { 0.1 } else { 0.04 }),
            );
            paint_polyline(window, &line, 2.0 * k, ink, None);
        }
        Picture::Curve(points) => {
            paint_polyline(window, &to_px(points), 2.0 * k, ink, None);
        }
        Picture::Image { left, right } => {
            let (cx0, cy0) = IMAGE_CENTRE;
            let lx = cx0 - left * IMAGE_REACH;
            let rx = cx0 - right * IMAGE_REACH;
            paint_polyline(window, &[at(lx, cy0), at(rx, cy0)], 2.0 * k, ink, None);
            let (dlx, dly) = at(lx, cy0);
            paint_dot(window, dlx, dly, 6.0 * k, ink);
            let (drx, dry) = at(rx, cy0);
            paint_dot(
                window,
                drx,
                dry,
                6.0 * k,
                color_a(ACCENT, if running { 0.7 } else { 0.4 }),
            );
            let font = plate::mono(FontWeight::NORMAL);
            for (x, text) in [(lx, "L"), (rx, "R")] {
                paint_text(
                    window,
                    cx,
                    text,
                    font.clone(),
                    10.0 * k,
                    color(INK),
                    at(x, cy0 + 12.0),
                    Align::Center,
                );
            }
        }
    }
    for &(handle, x, y) in marks {
        let held = dragging == Some(handle);
        let r = if held { HANDLE_R + 1.5 } else { HANDLE_R };
        let (hx, hy) = at(x, y);
        paint_dot(window, hx, hy, (r + 2.0) * k, color(0x0d0d0d));
        paint_dot(window, hx, hy, r * k, if held { color(WHITE) } else { ink });
    }
}

/// A rack row's stage levels: what enters on top, what leaves under it.
fn paint_stage(window: &mut Window, b: Bounds<Pixels>, levels: Option<(f32, f32)>, s: f32) {
    let (x, y) = plate::xy(b.origin);
    let (w, h) = (f32::from(b.size.width), f32::from(b.size.height));
    let bar_h = ((h - 2.0 * s) * 0.5).max(1.0);
    for (row, ink) in [(0, color(0x8a8a8a)), (1, accent())] {
        let top = y + row as f32 * (bar_h + 2.0 * s);
        paint_rect(window, (x, top, w, bar_h), color(0x222222));
        if let Some((input, output)) = levels {
            let db = level_db(if row == 0 { input } else { output });
            let fill = ((db - STAGE_FLOOR_DB) / -STAGE_FLOOR_DB).clamp(0.0, 1.0);
            paint_rect(window, (x, top, w * fill, bar_h), ink);
        }
    }
}

// ── The deck ───────────────────────────────────────────────────────────────

fn controls(e: &KitEditor<MixStationPlate>, cx: &mut Cx<MixStationPlate>, s: f32) -> AnyElement {
    deck(s, 341.0, 420.0)
        .p(px(14.0 * s))
        .gap(px(12.0 * s))
        .child(module_card(e, cx, s).w(px(640.0 * s)).flex_shrink_0())
        .child(strip_card(e, cx, s).flex_1())
        .into_any_element()
}

/// The dial over MixStation's descriptor row `id`, as the module cards and
/// the strip show it.
fn mix_dial(id: &'static str) -> Dial {
    let (label, unit, log, bipolar) = match id {
        "inputTrimDb" => ("INPUT", DialUnit::Db, false, true),
        "outputTrimDb" => ("OUTPUT", DialUnit::Db, false, true),
        "hpfHz" => ("LOW CUT", DialUnit::HzRound, true, false),
        "lpfHz" => ("HIGH CUT", DialUnit::Hz, true, false),
        "lowGainDb" => ("LOW", DialUnit::Db, false, true),
        "lowMidFreqHz" => ("LM FREQ", DialUnit::HzRound, true, false),
        "lowMidGainDb" => ("LM GAIN", DialUnit::Db, false, true),
        "highMidFreqHz" => ("HM FREQ", DialUnit::Hz, true, false),
        "highMidGainDb" => ("HM GAIN", DialUnit::Db, false, true),
        "highGainDb" => ("HIGH", DialUnit::Db, false, true),
        "compThresholdDb" => ("THRESHOLD", DialUnit::Db, false, false),
        "compRatio" => ("RATIO", DialUnit::Ratio, true, false),
        "compAttackMs" => ("ATTACK", DialUnit::Ms, true, false),
        "compReleaseMs" => ("RELEASE", DialUnit::Ms, true, false),
        "compMakeupDb" => ("MAKEUP", DialUnit::Db, false, false),
        "satDrivePct" => ("DRIVE", DialUnit::Percent, false, false),
        "satCharacterPct" => ("CHARACTER", DialUnit::Percent, false, false),
        "widthPct" => ("WIDTH", DialUnit::Percent, false, true),
        "limiterCeilingDb" => ("CEILING", DialUnit::Db, false, false),
        "limiterReleaseMs" => ("RELEASE", DialUnit::Ms, true, false),
        _ if id.ends_with("TrimDb") => ("TRIM", DialUnit::Db, false, true),
        _ => panic!("MixStation has no knob for {id}"),
    };
    let (min, max, default) = mixstation::descriptor()
        .params
        .iter()
        .find(|row| row.id == id)
        .map(|row| (row.min, row.max, row.default_value))
        .unwrap_or_else(|| panic!("MixStation has no param {id}"));
    let dial = Dial::new(id, label, min, max, default, unit);
    let dial = if log { dial.log() } else { dial };
    if bipolar {
        dial.bipolar()
    } else {
        dial
    }
}

fn mix_readout(id: &'static str, value: f32) -> String {
    match id {
        "compAttackMs" | "compReleaseMs" | "limiterReleaseMs" => fmt_time(value),
        "lpfHz" if value >= 19_999.5 => "Off".to_string(),
        "hpfHz" if value <= 20.5 => "Off".to_string(),
        _ => mix_dial_text(id, value),
    }
}

fn mix_dial_text(id: &'static str, value: f32) -> String {
    mix_dial(id).text(value)
}

#[allow(clippy::too_many_arguments)]
fn cell(
    cx: &mut Cx<MixStationPlate>,
    id: &'static str,
    value: f32,
    look: KnobLook,
    k: KnobCell,
    ink: Rgba,
    enabled: bool,
    s: f32,
) -> AnyElement {
    knob_reading(
        cx,
        format!("mix-{id}"),
        mix_dial(id),
        value,
        Some(mix_readout(id, value)),
        look,
        k,
        ink,
        enabled,
        s,
        move |this: &mut KitEditor<MixStationPlate>, value, cx| this.set_value(id, value, cx),
    )
}

fn module_card(e: &KitEditor<MixStationPlate>, cx: &mut Cx<MixStationPlate>, s: f32) -> gpui::Div {
    let Some(code) = selected(e) else {
        let add = div()
            .id("mix-add-card")
            .flex()
            .items_center()
            .justify_center()
            .h(px(32.0 * s))
            .w(px(180.0 * s))
            .rounded(px(3.0 * s))
            .border(px(1.0))
            .border_color(accent())
            .bg(color(0x1d1d1d))
            .text_size(type_px(12.5, s))
            .text_color(color(WHITE))
            .cursor(gpui::CursorStyle::PointingHand)
            .child("+ Add module")
            .on_mouse_down(
                MouseButton::Left,
                e.press_cb(cx, |this, (x, y), _, cx| {
                    this.open_menu(KitMenu::Own(ADD_MENU, x, y), cx)
                }),
            );
        return card(s)
            .child(card_title("Module", s))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(12.0 * s))
                    .child(words("The rack is empty", 15.0, PALE, s))
                    .child(note(
                        "MixStation passes the signal through its input and output trims only.",
                        s,
                    ))
                    .child(add),
            )
            .child(div());
    };
    let p = &e.params;
    let m = module(code).expect("selected module is known");
    let position = order(p).iter().position(|c| *c == code).unwrap_or(0);
    let on = flag(p, m.enabled);
    let running = on && p.power;

    let small = PillLook::words(28.0, 12.0, 12.0);
    let title = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(10.0 * s))
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(0.0))
                .child(card_title(
                    &format!("{} \u{00B7} {}", position + 1, m.name),
                    s,
                ))
                .child(words(m.hint, 11.5, SOFT, s)),
        )
        .child(words(if on { "On" } else { "Off" }, 12.0, PALE, s))
        .child(switch(
            "mix-module-on",
            on,
            accent(),
            true,
            s,
            e.click_cb(cx, move |this, cx| {
                if let Some(m) = module(code) {
                    this.toggle(m.enabled, cx)
                }
            }),
        ))
        .child(pill(
            "mix-module-reset",
            "Reset",
            false,
            accent(),
            small,
            true,
            s,
            e.click_cb(cx, move |this, cx| reset_module(this, code, cx)),
        ))
        .child(pill(
            "mix-module-remove",
            "Remove",
            false,
            accent(),
            small,
            true,
            s,
            e.click_cb(cx, move |this, cx| remove_module(this, code, cx)),
        ));

    let many = m.knobs.len() > 4;
    let (look, k) = if many {
        (KnobLook::small(54.0), KnobCell::new(80.0, 10.0, 11.5))
    } else {
        (
            KnobLook::small(62.0).styled(3.6, 17.5, FACE, FACE_EDGE, 0.8, 7.0, 1.8),
            KnobCell::new(100.0, 10.5, 12.5),
        )
    };
    let ink = if running { accent() } else { color(0x8a8a8a) };
    let knobs: Vec<AnyElement> = m
        .knobs
        .iter()
        .map(|&id| cell(cx, id, value(p, id), look, k, ink, true, s))
        .collect();
    let trim = cell(cx, m.trim, value(p, m.trim), look, k, color(INK), true, s);
    let row = div()
        .flex()
        .flex_row()
        .items_start()
        .gap(px(if many { 4.0 } else { 14.0 } * s))
        .children(knobs)
        .child(div().flex_1())
        .child(
            div()
                .w(px(1.0))
                .h(px(86.0 * s))
                .mx(px(6.0 * s))
                .bg(color(0x444444)),
        )
        .child(trim);
    let status = if !p.power {
        "MixStation is bypassed: the whole strip passes audio through unchanged.".to_string()
    } else if !on {
        format!(
            "{} is off: it keeps its place in the chain and passes audio through.",
            m.name
        )
    } else {
        format!(
            "Position {} of {}. Its trim travels with it when it moves in the chain.",
            position + 1,
            order(p).len()
        )
    };
    card(s)
        .child(title)
        .child(section("Controls", s).child(row))
        .child(note(status, s))
}

fn strip_card(e: &KitEditor<MixStationPlate>, cx: &mut Cx<MixStationPlate>, s: f32) -> gpui::Div {
    let p = &e.params;
    let look = KnobLook::small(56.0).styled(4.5, 16.0, FACE, FACE_EDGE, 1.2, 7.0, 2.4);
    let k = KnobCell::new(90.0, 10.0, 12.0);
    let gr = div()
        .flex()
        .flex_col()
        .gap(px(6.0 * s))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .child(caption("Gain reduction", 10.5, 0.1, SOFT, s))
                .child(
                    div()
                        .w(px(80.0 * s))
                        .h(px(14.0 * s))
                        .child(live_slot(&e.live_bounds, SLOT_GR_READOUT).size_full()),
                ),
        )
        .child(
            div()
                .h(px(12.0 * s))
                .w_full()
                .child(live_slot(&e.live_bounds, SLOT_GR_BAR).size_full()),
        )
        .child(note("Compressor and limiter, combined.", s));
    card(s)
        .child(card_title("Strip", s))
        .child(
            div()
                .flex()
                .flex_row()
                .justify_around()
                .child(cell(
                    cx,
                    "inputTrimDb",
                    p.input_trim_db,
                    look,
                    k,
                    color(INK),
                    true,
                    s,
                ))
                .child(cell(
                    cx,
                    "outputTrimDb",
                    p.output_trim_db,
                    look,
                    k,
                    color(INK),
                    true,
                    s,
                )),
        )
        .child(gr)
        .child(io_meters(&e.live_bounds, SLOT_METER, s))
}

/// Opens with a track's MixStation insert.
pub fn open_mixstation_editor(
    owner_bounds: Option<Bounds<Pixels>>,
    key: PluginInstanceKey,
    identity: ShellIdentity,
    host_ops: BuiltinEditorHostOps,
    on_close: Arc<dyn Fn(&mut Window, &mut App)>,
    cx: &mut App,
) -> Result<gpui::WindowHandle<MixStationWindow>, String> {
    plate::ensure_fonts(cx);
    open_kit_editor(
        MixStationPlate,
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
        let ids = MODULES
            .iter()
            .flat_map(|m| m.knobs.iter().copied().chain([m.trim]))
            .chain(["inputTrimDb", "outputTrimDb"]);
        for id in ids {
            let dial = mix_dial(id);
            let default = model::default_value(id);
            assert!(
                (dial.at(dial.norm(default)) - default).abs() <= 0.011 * default.abs().max(1.0),
                "{id}: {default} does not round-trip"
            );
            let row = mixstation::descriptor()
                .params
                .iter()
                .find(|row| row.id == id)
                .map(|row| (row.min, row.max))
                .unwrap();
            assert_eq!((dial.min, dial.max), row, "{id}");
            assert_eq!(dial.at(0.0), row.0, "{id}");
            assert_eq!(dial.at(1.0), row.1, "{id}");
        }
    }

    #[test]
    fn a_handle_drag_lands_where_it_is_drawn() {
        let params = with_order(&mixstation::default_params(), &[1, 2, 3, 6]);
        for code in [1u8, 2, 3, 6] {
            for (handle, x, y) in handles(&params, code) {
                let landed = dragged(&params, handle, (x, y));
                let (_, lx, ly) = handles(&landed, code)
                    .into_iter()
                    .find(|(h, _, _)| *h == handle)
                    .unwrap();
                assert!(
                    (lx - x).abs() < 1.5 && (ly - y).abs() < 1.5,
                    "{handle:?}: ({x}, {y}) landed at ({lx}, {ly})"
                );
            }
        }
    }

    #[test]
    fn moving_a_module_reorders_the_rack() {
        let params = with_order(&mixstation::default_params(), &[1, 2, 3]);
        assert_eq!(order(&moved(&params, 3, 0)), vec![3, 1, 2]);
        assert_eq!(order(&moved(&params, 1, 9)), vec![2, 3, 1]);
        assert_eq!(order(&moved(&params, 5, 0)), vec![1, 2, 3]);
    }

    #[test]
    fn the_subtitle_counts_the_rack() {
        let params = with_order(&mixstation::default_params(), &[2, 3]);
        assert_eq!(
            MixStationPlate.subtitle(&params),
            "Channel strip rack \u{00B7} 2 / 6"
        );
        assert_eq!(describe_width(100.0), "As the source");
        assert_eq!(mix_readout("lpfHz", 20_000.0), "Off");
    }
}

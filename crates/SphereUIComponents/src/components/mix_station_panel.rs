//! The native MixStation editor: a channel strip whose six modules run in
//! the order the user stacks them.
//!
//! Three columns:
//!
//! * the signal path — the rack, first module first, each row with its
//!   stage's live levels and its switch; drag a row's grip to move it, and
//!   add a module from the list of those not yet in the rack;
//! * the selected module — its display, drawn from the DSP crates' own
//!   models (`filter_response_db`, `eq_response_db`, the static curves), with
//!   handles to drag, and its knobs and trim;
//! * the strip's input and output trims around its meters.
//!
//! Every rack move is plain slot params, written first slot to last (see
//! `mix_station_model`).

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    canvas, div, fill, px, AnyElement, App, Bounds, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, MouseMoveEvent, ParentElement, Pixels, StatefulInteractiveElement, Styled,
    Window,
};
use mixstation::Params;

use crate::components::builtin_plugin_editor_window::{BuiltinEditorHostOps, PluginInstanceKey};
use crate::components::context_menu::ContextMenuEntry;
use crate::components::controls::{fb_button, FbButtonKind};
use crate::components::eq_graph::{freq_at_fraction, freq_fraction, paint_area, paint_line};
use crate::components::fx_model::KnobSpec;
use crate::components::mix_station_model::{
    active, flag, knob, module, order, preset_applied, presets, value, with, with_order, Module,
    MODULES, SLOTS,
};
use crate::components::native_plugin_shell::ShellIdentity;
use crate::components::plugin_kit::{
    bypass_notice, caption, display_frame, header, hint, knob_for, open_kit_editor, Cx, KitEditor,
    KitMenu, KitModel, KitPreset, KitWindow, LiveBounds, LiveSlot, KNOB,
};
use crate::components::plugin_live::{
    dashed, dot, frame_of, label, paint_meters, paint_spectrum, paint_stage_bars, transfer_plot,
    Align, Live, MeterColumn,
};
use crate::components::quick_sampler_panel::rect;
use crate::theme::{radius, space, typography, Colors};

/// A rack row's height, and the pitch rows sit at.
const ROW_H: f32 = 44.0;
const ROW_PITCH: f32 = ROW_H + space::TIGHT;
/// How near a press must land to a handle to take it.
const HANDLE_HIT: f32 = 12.0;
const HANDLE_R: f32 = 6.0;
/// The ranges the module displays span.
const CUT_TOP_DB: f32 = 6.0;
const CUT_FLOOR_DB: f32 = -36.0;
const EQ_RANGE_DB: f32 = 18.0;
const COMP_RANGE_DB: f32 = -60.0;
const LIMIT_RANGE_DB: f32 = -24.0;
/// Where the limiter's ceiling handle sits on its input axis.
const CEILING_HANDLE_DB: f32 = -2.0;
/// The menu that adds a module.
const ADD_MENU: u8 = 0;

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct MixStationModel;

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

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MixDrag {
    /// A rack row held by its grip, and the position it would drop at.
    Reorder {
        code: u8,
        target: usize,
    },
    Handle(Handle),
}

#[derive(Default)]
pub struct MixUi {
    /// The module the centre column edits.
    pub selected: Option<u8>,
    pub drag: Option<MixDrag>,
    /// The rack list's bounds, for a row's drop position.
    pub list_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
}

pub struct MixLiveView {
    params: Params,
}

impl KitModel for MixStationModel {
    type Params = Params;
    type Ui = MixUi;
    type LiveView = MixLiveView;

    fn title(self) -> &'static str {
        "MixStation"
    }

    fn subtitle(self, params: &Params) -> String {
        format!("Channel strip rack · {} / {SLOTS}", order(params).len())
    }

    fn key(self) -> &'static str {
        "mixstation"
    }

    fn defaults(self) -> Params {
        mixstation::default_params()
    }

    fn from_mirror(self, insert_id: &str) -> Option<Params> {
        crate::components::builtin_plugin_editor::builtin_mixstation_params(insert_id)
    }

    fn wire_values(self, params: &Params) -> Vec<(u32, f32)> {
        crate::components::mix_station_model::wire_values(params)
    }

    fn with(self, params: &Params, id: &str, value: f32) -> Params {
        with(params, id, value)
    }

    fn value(self, params: &Params, id: &str) -> f32 {
        value(params, id)
    }

    fn presets(self) -> Arc<Vec<KitPreset<Params>>> {
        presets()
    }

    fn preset_applied(self, current: &Params, preset: &Params) -> Params {
        preset_applied(current, preset)
    }

    fn knob(self, id: &str) -> Option<KnobSpec> {
        knob(id)
    }

    fn panel(editor: &KitEditor<Self>, cx: &mut Cx<Self>) -> AnyElement {
        mix_panel(editor, cx)
    }

    fn live_view(editor: &KitEditor<Self>) -> MixLiveView {
        MixLiveView {
            params: editor.params.clone(),
        }
    }

    fn paint_live(
        view: &MixLiveView,
        live: &mut Live,
        bounds: &LiveBounds,
        window: &mut Window,
        cx: &mut App,
    ) {
        paint_mix_live(&view.params, live, bounds, window, cx);
    }

    fn window_size(self) -> (f32, f32) {
        (1_140.0, 720.0)
    }

    fn min_size(self) -> (f32, f32) {
        (980.0, 620.0)
    }

    fn mouse_move(editor: &mut KitEditor<Self>, event: &MouseMoveEvent, cx: &mut Cx<Self>) {
        drag_to(editor, event, cx);
    }

    fn mouse_up(editor: &mut KitEditor<Self>, cx: &mut Cx<Self>) {
        match editor.ui.drag.take() {
            Some(MixDrag::Reorder { code, target }) => move_module(editor, code, target, cx),
            Some(MixDrag::Handle(_)) => cx.notify(),
            None => {}
        }
    }

    fn own_menu(editor: &KitEditor<Self>, _menu: u8) -> Vec<ContextMenuEntry> {
        let loaded = order(&editor.params);
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
                format!("{} — {}", m.name, m.hint),
                format!("add:{}", m.code),
            )
        }));
        entries
    }

    fn run_own_command(editor: &mut KitEditor<Self>, command: &str, cx: &mut Cx<Self>) {
        if let Some(code) = command
            .strip_prefix("add:")
            .and_then(|c| c.parse::<u8>().ok())
        {
            add_module(editor, code, cx);
        }
    }
}

pub type MixStationWindow = KitWindow<MixStationModel>;

// ── The rack ────────────────────────────────────────────────────────────────

/// The module the centre column shows: the one picked, else the first in
/// the rack.
fn selected(e: &KitEditor<MixStationModel>) -> Option<u8> {
    let loaded = order(&e.params);
    e.ui.selected
        .filter(|code| loaded.contains(code))
        .or_else(|| loaded.first().copied())
}

fn add_module(e: &mut KitEditor<MixStationModel>, code: u8, cx: &mut Cx<MixStationModel>) {
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

fn remove_module(e: &mut KitEditor<MixStationModel>, code: u8, cx: &mut Cx<MixStationModel>) {
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
    e: &mut KitEditor<MixStationModel>,
    code: u8,
    target: usize,
    cx: &mut Cx<MixStationModel>,
) {
    let mut rack = order(&e.params);
    let Some(from) = rack.iter().position(|c| *c == code) else {
        return;
    };
    rack.remove(from);
    rack.insert(target.min(rack.len()), code);
    let next = with_order(&e.params, &rack);
    e.set_params(next, cx);
    cx.notify();
}

/// Puts a module's knobs and trim back to their defaults.
fn reset_module(e: &mut KitEditor<MixStationModel>, code: u8, cx: &mut Cx<MixStationModel>) {
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

// ── Gestures ────────────────────────────────────────────────────────────────

fn freq_plot(bounds: Bounds<Pixels>) -> (f32, f32, f32, f32) {
    let (x0, y0, w, h) = frame_of(bounds);
    (x0, y0 + 26.0, w, (h - 26.0 - 18.0).max(1.0))
}

/// Where each of the module's handles sits, in window space.
fn handles(params: &Params, code: u8, bounds: Bounds<Pixels>) -> Vec<(Handle, f32, f32)> {
    match code {
        1 => {
            let (x0, y0, w, h) = freq_plot(bounds);
            let y = y0 + (CUT_TOP_DB - -3.0) / (CUT_TOP_DB - CUT_FLOOR_DB) * h;
            let x = |hz: f32| x0 + freq_fraction(hz) * w;
            vec![
                (Handle::LowCut, x(params.hpf_hz), y),
                (Handle::HighCut, x(params.lpf_hz), y),
            ]
        }
        2 => {
            let (x0, y0, w, h) = freq_plot(bounds);
            let x = |hz: f32| x0 + freq_fraction(hz) * w;
            let y = |db: f32| y0 + h * 0.5 - db / EQ_RANGE_DB * h * 0.5;
            vec![
                (
                    Handle::EqLow,
                    x(mixstation::LOW_SHELF_HZ),
                    y(params.low_gain_db),
                ),
                (
                    Handle::EqLowMid,
                    x(params.low_mid_freq_hz),
                    y(params.low_mid_gain_db),
                ),
                (
                    Handle::EqHighMid,
                    x(params.high_mid_freq_hz),
                    y(params.high_mid_gain_db),
                ),
                (
                    Handle::EqHigh,
                    x(mixstation::HIGH_SHELF_HZ),
                    y(params.high_gain_db),
                ),
            ]
        }
        3 => {
            let (x0, y0, w, h) = transfer_plot(bounds);
            let at = |db: f32| (db - COMP_RANGE_DB) / -COMP_RANGE_DB;
            let threshold = params.comp_threshold_db;
            let out = mixstation::comp_transfer_db(params, threshold).clamp(COMP_RANGE_DB, 0.0);
            vec![(
                Handle::Threshold,
                x0 + at(threshold) * w,
                y0 + h - at(out) * h,
            )]
        }
        6 => {
            let (x0, y0, w, h) = transfer_plot(bounds);
            let at = |db: f32| (db - LIMIT_RANGE_DB) / -LIMIT_RANGE_DB;
            vec![(
                Handle::Ceiling,
                x0 + at(CEILING_HANDLE_DB) * w,
                y0 + h - at(params.limiter_ceiling_db) * h,
            )]
        }
        _ => Vec::new(),
    }
}

/// The slot a module's display records its bounds as.
fn display_slot(code: u8) -> LiveSlot {
    match code {
        1 | 2 => LiveSlot::Spectrum,
        _ => LiveSlot::Transfer,
    }
}

fn press_display(
    e: &mut KitEditor<MixStationModel>,
    (x, y): (f32, f32),
    event: &MouseDownEvent,
    cx: &mut Cx<MixStationModel>,
) {
    let Some(code) = selected(e) else {
        return;
    };
    let Some(bounds) = e.live_bounds.get(display_slot(code)) else {
        return;
    };
    let hit = handles(&e.params, code, bounds)
        .into_iter()
        .map(|(handle, hx, hy)| (handle, ((hx - x).powi(2) + (hy - y).powi(2)).sqrt()))
        .filter(|(_, distance)| *distance <= HANDLE_HIT)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(handle, _)| handle);
    let Some(handle) = hit else {
        return;
    };
    if event.click_count >= 2 {
        let id = match handle {
            Handle::LowCut => "hpfHz",
            Handle::HighCut => "lpfHz",
            Handle::EqLow => "lowGainDb",
            Handle::EqLowMid => "lowMidGainDb",
            Handle::EqHighMid => "highMidGainDb",
            Handle::EqHigh => "highGainDb",
            Handle::Threshold => "compThresholdDb",
            Handle::Ceiling => "limiterCeilingDb",
        };
        let default = crate::components::mix_station_model::default_value(id);
        e.set_value(id, default, cx);
        return;
    }
    e.ui.drag = Some(MixDrag::Handle(handle));
    cx.notify();
}

fn drag_to(
    e: &mut KitEditor<MixStationModel>,
    event: &MouseMoveEvent,
    cx: &mut Cx<MixStationModel>,
) {
    let Some(drag) = e.ui.drag else {
        return;
    };
    if event.pressed_button.is_none() {
        e.ui.drag = None;
        cx.notify();
        return;
    }
    let (x, y) = (f32::from(event.position.x), f32::from(event.position.y));
    match drag {
        MixDrag::Reorder { code, target } => {
            let Some(list) = e.ui.list_bounds.get() else {
                return;
            };
            let count = order(&e.params).len().max(1);
            let row = ((y - f32::from(list.origin.y)) / ROW_PITCH).floor();
            let next = (row.max(0.0) as usize).min(count - 1);
            if next != target {
                e.ui.drag = Some(MixDrag::Reorder { code, target: next });
                cx.notify();
            }
        }
        MixDrag::Handle(handle) => {
            let Some(code) = selected(e) else {
                return;
            };
            let Some(bounds) = e.live_bounds.get(display_slot(code)) else {
                return;
            };
            let freq = |plot: (f32, f32, f32, f32)| freq_at_fraction((x - plot.0) / plot.2);
            let round = |v: f32, step: f32| (v / step).round() * step;
            match handle {
                Handle::LowCut | Handle::HighCut => {
                    let hz = freq(freq_plot(bounds));
                    let (id, hz) = if handle == Handle::LowCut {
                        ("hpfHz", round(hz.clamp(20.0, 500.0), 1.0))
                    } else {
                        ("lpfHz", round(hz.clamp(1_000.0, 20_000.0), 10.0))
                    };
                    e.set_value(id, hz, cx);
                }
                Handle::EqLow | Handle::EqLowMid | Handle::EqHighMid | Handle::EqHigh => {
                    let plot = freq_plot(bounds);
                    let gain = (plot.1 + plot.3 * 0.5 - y) / (plot.3 * 0.5) * EQ_RANGE_DB;
                    let gain = round(gain.clamp(-EQ_RANGE_DB, EQ_RANGE_DB), 0.1);
                    let mut next = e.params.clone();
                    match handle {
                        Handle::EqLow => next = with(&next, "lowGainDb", gain),
                        Handle::EqHigh => next = with(&next, "highGainDb", gain),
                        Handle::EqLowMid => {
                            next = with(&next, "lowMidGainDb", gain);
                            next = with(
                                &next,
                                "lowMidFreqHz",
                                round(freq(plot).clamp(80.0, 2_000.0), 1.0),
                            );
                        }
                        _ => {
                            next = with(&next, "highMidGainDb", gain);
                            next = with(
                                &next,
                                "highMidFreqHz",
                                round(freq(plot).clamp(500.0, 12_000.0), 1.0),
                            );
                        }
                    }
                    e.set_params(next, cx);
                }
                Handle::Threshold => {
                    let (x0, _, w, _) = transfer_plot(bounds);
                    let db = COMP_RANGE_DB - (x - x0) / w * COMP_RANGE_DB;
                    e.set_value(
                        "compThresholdDb",
                        round(db.clamp(COMP_RANGE_DB, 0.0), 0.1),
                        cx,
                    );
                }
                Handle::Ceiling => {
                    let (_, y0, _, h) = transfer_plot(bounds);
                    let db = LIMIT_RANGE_DB * (y - y0) / h;
                    e.set_value("limiterCeilingDb", round(db.clamp(-12.0, 0.0), 0.1), cx);
                }
            }
        }
    }
}

// ── The panel ───────────────────────────────────────────────────────────────

fn mix_panel(e: &KitEditor<MixStationModel>, cx: &mut Cx<MixStationModel>) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .size_full()
        .bg(Colors::surface_window())
        .child(header(e, cx))
        .child(
            div()
                .flex()
                .flex_row()
                .flex_1()
                .min_h(px(0.0))
                .p(px(space::LOOSE))
                .gap(px(space::BASE))
                .child(chain(e, cx))
                .child(module_editor(e, cx))
                .child(io_column(e, cx)),
        )
        .into_any_element()
}

fn panel_box() -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .p(px(space::BASE))
        .gap(px(space::SNUG))
        .rounded(px(radius::SURFACE))
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_panel())
}

fn flow_marker(text: &'static str) -> AnyElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::SNUG))
        .child(div().w(px(18.0)).h(px(1.0)).bg(Colors::border_strong()))
        .child(caption(text))
        .into_any_element()
}

/// An on/off pill.
fn switch(
    e: &KitEditor<MixStationModel>,
    cx: &mut Cx<MixStationModel>,
    id: (&'static str, usize),
    on: bool,
    text: (&'static str, &'static str),
    action: impl Fn(&mut KitEditor<MixStationModel>, &mut Cx<MixStationModel>) + 'static,
) -> AnyElement {
    let tone = Colors::accent_primary();
    div()
        .id(id)
        .px(px(space::SNUG))
        .h(px(20.0))
        .flex()
        .items_center()
        .flex_shrink_0()
        .rounded(px(radius::CONTROL_SM))
        .border(px(1.0))
        .border_color(if on {
            Colors::with_alpha(tone, 0.6)
        } else {
            Colors::border_normal()
        })
        .bg(if on {
            Colors::with_alpha(tone, 0.16)
        } else {
            Colors::surface_canvas()
        })
        .text_size(px(typography::DENSE_CAPTION))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(if on {
            Colors::accent_primary_hover()
        } else {
            Colors::text_muted()
        })
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(|style| style.border_color(Colors::border_strong()))
        .child(if on { text.0 } else { text.1 })
        .on_click(e.click_cb(cx, action))
        .into_any_element()
}

fn chain(e: &KitEditor<MixStationModel>, cx: &mut Cx<MixStationModel>) -> AnyElement {
    let rack = order(&e.params);
    let current = selected(e);
    let power = e.power();
    let dragging = match e.ui.drag {
        Some(MixDrag::Reorder { code, target }) => Some((code, target)),
        _ => None,
    };
    let list_bounds = e.ui.list_bounds.clone();
    let rows: Vec<AnyElement> =
        rack.iter()
            .enumerate()
            .map(|(position, code)| {
                let code = *code;
                let m = module(code).expect("the rack holds known modules");
                let on = flag(&e.params, m.enabled);
                let is_selected = current == Some(code);
                let held = dragging.is_some_and(|(c, _)| c == code);
                let drop_here = dragging.is_some_and(|(c, target)| c != code && target == position);
                let grip = div()
                    .id(("mix-grip", position))
                    .w(px(14.0))
                    .h_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(typography::UI_SM))
                    .text_color(Colors::text_faint())
                    .cursor(gpui::CursorStyle::OpenHand)
                    .child("⠿")
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
                div()
                    .id(("mix-row", position))
                    .relative()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(space::SNUG))
                    .h(px(ROW_H))
                    .flex_shrink_0()
                    .px(px(space::SNUG))
                    .rounded(px(radius::CONTROL))
                    .border(px(1.0))
                    .border_color(if held || drop_here {
                        Colors::accent_primary()
                    } else if is_selected {
                        Colors::border_normal()
                    } else {
                        Colors::border_subtle()
                    })
                    .bg(if is_selected {
                        Colors::surface_raised()
                    } else {
                        Colors::surface_canvas()
                    })
                    .cursor(gpui::CursorStyle::PointingHand)
                    .child(grip)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w(px(0.0))
                            .gap(px(space::HAIR))
                            .child(
                                div()
                                    .text_size(px(typography::UI_SM))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(if on && power {
                                        Colors::text_primary()
                                    } else {
                                        Colors::text_muted()
                                    })
                                    .child(format!("{}  {}", position + 1, m.name)),
                            )
                            .child(
                                div().h(px(8.0)).w_full().child(
                                    e.live_bounds.slot(LiveSlot::Stage(position)).size_full(),
                                ),
                            ),
                    )
                    .child(switch(
                        e,
                        cx,
                        ("mix-on", position),
                        on,
                        ("On", "Off"),
                        move |this, cx| {
                            let m = module(code).expect("known module");
                            this.toggle(m.enabled, cx)
                        },
                    ))
                    .on_click(e.click_cb(cx, move |this, cx| {
                        this.ui.selected = Some(code);
                        cx.notify();
                    }))
                    .into_any_element()
            })
            .collect();
    let list = div()
        .relative()
        .flex()
        .flex_col()
        .gap(px(space::TIGHT))
        .child(
            canvas(
                move |bounds, _, _| list_bounds.set(Some(bounds)),
                |_, _, _, _| {},
            )
            .absolute()
            .size_full(),
        )
        .children(rows);
    let empty = rack.is_empty().then(|| {
        div()
            .py(px(space::LOOSE))
            .child(hint("The rack is empty. Add a module to start the chain."))
    });
    let add = div()
        .id("mix-add")
        .h(px(30.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(radius::CONTROL))
        .border(px(1.0))
        .border_color(Colors::border_normal())
        .text_size(px(typography::UI_XS))
        .text_color(if rack.len() < SLOTS {
            Colors::text_secondary()
        } else {
            Colors::text_disabled()
        })
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(|style| style.bg(Colors::surface_hover()))
        .child("+  Add module")
        .on_mouse_down(
            MouseButton::Left,
            e.press_cb(cx, |this, (x, y), _, cx| {
                this.open_menu(KitMenu::Own(ADD_MENU, x, y), cx)
            }),
        );
    panel_box()
        .w(px(250.0))
        .flex_shrink_0()
        .child(
            div()
                .flex()
                .flex_row()
                .justify_between()
                .child(caption("SIGNAL PATH"))
                .child(hint(format!("{} / {SLOTS}", rack.len()))),
        )
        .child(flow_marker("IN"))
        .child(list)
        .children(empty)
        .child(add)
        .child(flow_marker("OUT"))
        .child(div().flex_1())
        .child(hint("Drag a row's grip to move it in the chain."))
        .into_any_element()
}

fn module_editor(e: &KitEditor<MixStationModel>, cx: &mut Cx<MixStationModel>) -> AnyElement {
    let column = div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w(px(0.0))
        .gap(px(space::BASE));
    let Some(code) = selected(e) else {
        return column
            .child(
                panel_box()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .child(
                        div()
                            .text_size(px(typography::UI_MD))
                            .text_color(Colors::text_secondary())
                            .child("No modules in the chain"),
                    )
                    .child(hint("Add one from the signal path on the left.")),
            )
            .into_any_element();
    };
    let m = module(code).expect("selected module is known");
    let position = order(&e.params)
        .iter()
        .position(|c| *c == code)
        .unwrap_or(0);
    let on = flag(&e.params, m.enabled);
    let running = on && e.power();
    let title = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::BASE))
        .flex_shrink_0()
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .child(
                    div()
                        .text_size(px(typography::UI_MD))
                        .font_weight(gpui::FontWeight::BOLD)
                        .text_color(Colors::text_primary())
                        .child(format!("{}  {}", position + 1, m.name)),
                )
                .child(hint(m.hint)),
        )
        .child(switch(
            e,
            cx,
            ("mix-module-on", 0),
            on,
            ("On", "Bypassed"),
            move |this, cx| this.toggle(m.enabled, cx),
        ))
        .child(fb_button(
            "mix-module-reset",
            "Reset",
            FbButtonKind::Ghost,
            true,
            e.click_cb(cx, move |this, cx| reset_module(this, code, cx)),
        ))
        .child(fb_button(
            "mix-module-remove",
            "Remove",
            FbButtonKind::Ghost,
            true,
            e.click_cb(cx, move |this, cx| remove_module(this, code, cx)),
        ));
    let size = if m.knobs.len() > 4 { KNOB } else { KNOB + 6.0 };
    let knobs: Vec<AnyElement> = m
        .knobs
        .iter()
        .map(|id| knob_for(e, cx, id, size, None))
        .collect();
    let why = if !e.power() {
        Some("MixStation is bypassed".to_string())
    } else if !on {
        Some(format!("{} is bypassed", m.name))
    } else {
        None
    };
    let controls = div()
        .flex()
        .flex_row()
        .flex_wrap()
        .items_start()
        .gap(px(space::TIGHT))
        .opacity(if running { 1.0 } else { 0.6 })
        .children(knobs)
        .child(
            div()
                .w(px(1.0))
                .h(px(56.0))
                .mx(px(space::SNUG))
                .bg(Colors::border_subtle()),
        )
        .child(knob_for(e, cx, m.trim, KNOB, None));
    column
        .child(title)
        .child(module_display(e, cx, code, running))
        .child(
            panel_box()
                .flex_shrink_0()
                .child(caption("CONTROLS"))
                .child(controls)
                .children(why.map(hint)),
        )
        .into_any_element()
}

fn io_column(e: &KitEditor<MixStationModel>, cx: &mut Cx<MixStationModel>) -> AnyElement {
    panel_box()
        .w(px(170.0))
        .flex_shrink_0()
        .items_center()
        .child(knob_for(e, cx, "inputTrimDb", KNOB + 6.0, None))
        .child(
            div()
                .flex_1()
                .w_full()
                .min_h(px(120.0))
                .child(e.live_bounds.slot(LiveSlot::Meters).size_full()),
        )
        .child(knob_for(e, cx, "outputTrimDb", KNOB + 6.0, None))
        .into_any_element()
}

// ── Module displays ─────────────────────────────────────────────────────────

fn module_display(
    e: &KitEditor<MixStationModel>,
    cx: &mut Cx<MixStationModel>,
    code: u8,
    running: bool,
) -> AnyElement {
    let params = e.params.clone();
    let sample_rate = e.sample_rate;
    let dragging = match e.ui.drag {
        Some(MixDrag::Handle(handle)) => Some(handle),
        _ => None,
    };
    let (title, legend) = match code {
        1 => (
            "RESPONSE",
            format!(
                "{:.0} Hz – {:.1} kHz",
                params.hpf_hz,
                params.lpf_hz / 1_000.0
            ),
        ),
        2 => ("RESPONSE", "Shelves at 100 Hz and 10 kHz".to_string()),
        3 => (
            "TRANSFER",
            format!(
                "{:.1} dB · {:.1}:1",
                params.comp_threshold_db, params.comp_ratio
            ),
        ),
        4 => ("CURVE", "In → Out".to_string()),
        5 => ("IMAGE", describe_width(params.width_pct).to_string()),
        _ => (
            "TRANSFER",
            format!("Ceiling {:.1} dB", params.limiter_ceiling_db),
        ),
    };
    let notice = bypass_notice(e);
    let picture = canvas(
        |_, _, _| (),
        move |bounds, _, window, cx| {
            paint_module(
                window,
                cx,
                bounds,
                &params,
                code,
                sample_rate,
                running,
                dragging,
            )
        },
    )
    .absolute()
    .size_full();
    display_frame(
        title,
        Some(legend),
        notice,
        div()
            .id("mix-display")
            .relative()
            .size_full()
            .child(picture)
            .child(
                e.live_bounds
                    .slot(display_slot(code))
                    .absolute()
                    .size_full(),
            )
            .on_mouse_down(MouseButton::Left, e.press_cb(cx, press_display)),
    )
    .flex_1()
    .min_h(px(170.0))
    .into_any_element()
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

fn frequencies(count: usize) -> Vec<f32> {
    (0..count)
        .map(|i| freq_at_fraction(i as f32 / (count - 1) as f32))
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn paint_module(
    window: &mut Window,
    cx: &mut App,
    bounds: Bounds<Pixels>,
    params: &Params,
    code: u8,
    sample_rate: f32,
    running: bool,
    dragging: Option<Handle>,
) {
    let ink = Colors::text_primary();
    let accent = if running {
        Colors::accent_primary()
    } else {
        Colors::text_disabled()
    };
    let size = typography::DENSE_CAPTION;
    let faint = Colors::text_faint();
    match code {
        1 | 2 => {
            let plot = freq_plot(bounds);
            let (x0, y0, w, h) = plot;
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
                let x = x0 + freq_fraction(hz) * w;
                let major = [100.0, 1_000.0, 10_000.0].contains(&hz);
                window.paint_quad(fill(
                    rect(x, y0, 1.0, h),
                    Colors::with_alpha(ink, if major { 0.09 } else { 0.045 }),
                ));
                label(
                    window,
                    cx,
                    text,
                    size,
                    faint,
                    x,
                    y0 + h + 3.0,
                    Align::Center,
                );
            }
            let freqs = frequencies(160);
            let (curve, y_of): (Vec<f32>, Box<dyn Fn(f32) -> f32>) = if code == 1 {
                let y_of = move |db: f32| {
                    y0 + ((CUT_TOP_DB - db) / (CUT_TOP_DB - CUT_FLOOR_DB)).clamp(0.0, 1.0) * h
                };
                (
                    mixstation::filter_response_db(params, sample_rate, &freqs),
                    Box::new(y_of),
                )
            } else {
                let y_of =
                    move |db: f32| y0 + h * 0.5 - (db / EQ_RANGE_DB).clamp(-1.0, 1.0) * h * 0.5;
                (
                    mixstation::eq_response_db(params, sample_rate, &freqs),
                    Box::new(y_of),
                )
            };
            let grid: &[f32] = if code == 1 {
                &[0.0, -12.0, -24.0]
            } else {
                &[12.0, 6.0, 0.0, -6.0, -12.0]
            };
            for db in grid {
                let y = y_of(*db);
                window.paint_quad(fill(
                    rect(x0, y, w, 1.0),
                    Colors::with_alpha(ink, if *db == 0.0 { 0.12 } else { 0.05 }),
                ));
                label(
                    window,
                    cx,
                    &format!("{db:+.0}"),
                    size,
                    faint,
                    x0 + 4.0,
                    y - 13.0,
                    Align::Left,
                );
            }
            let points: Vec<(f32, f32)> = freqs
                .iter()
                .zip(&curve)
                .map(|(hz, db)| (x0 + freq_fraction(*hz) * w, y_of(*db)))
                .collect();
            if code == 2 {
                paint_area(window, &points, y_of(0.0), Colors::with_alpha(accent, 0.12));
            }
            paint_line(window, &points, 2.0, accent);
        }
        3 | 6 => {
            let range = if code == 3 {
                COMP_RANGE_DB
            } else {
                LIMIT_RANGE_DB
            };
            let (x0, y0, w, h) = transfer_plot(bounds);
            let at = |db: f32| (db.clamp(range, 0.0) - range) / -range;
            let step = if code == 3 { 12.0 } else { 6.0 };
            let mut db = range;
            while db <= 0.01 {
                window.paint_quad(fill(
                    rect(x0 + at(db) * w, y0, 1.0, h),
                    Colors::with_alpha(ink, 0.06),
                ));
                window.paint_quad(fill(
                    rect(x0, y0 + h - at(db) * h, w, 1.0),
                    Colors::with_alpha(ink, 0.06),
                ));
                if db < 0.0 && db > range {
                    let text = format!("{db:.0}");
                    label(
                        window,
                        cx,
                        &text,
                        size,
                        faint,
                        x0 + at(db) * w,
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
                        y0 + h - at(db) * h - 6.0,
                        Align::Right,
                    );
                }
                db += step;
            }
            dashed(
                window,
                (x0, y0 + h),
                (x0 + w, y0),
                1.0,
                Colors::with_alpha(ink, 0.22),
            );
            if code == 6 {
                let y = y0 + h - at(params.limiter_ceiling_db) * h;
                dashed(
                    window,
                    (x0, y),
                    (x0 + w, y),
                    1.0,
                    Colors::with_alpha(Colors::accent_warning(), 0.8),
                );
            }
            let points: Vec<(f32, f32)> = (0..=120)
                .map(|i| {
                    let input = range - range * i as f32 / 120.0;
                    let output = if code == 3 {
                        mixstation::comp_transfer_db(params, input)
                    } else {
                        mixstation::limiter_transfer_db(params, input)
                    };
                    (x0 + at(input) * w, y0 + h - at(output) * h)
                })
                .collect();
            paint_line(window, &points, 2.0, accent);
        }
        4 => {
            let (x0, y0, w, h) = transfer_plot(bounds);
            let side = w.min(h);
            let (cx0, cy0) = (x0 + (w - side) * 0.5, y0 + (h - side) * 0.5);
            let span = 1.2;
            let map = |x: f32, y: f32| {
                (
                    cx0 + (x + span) / (2.0 * span) * side,
                    cy0 + side - (y + span) / (2.0 * span) * side,
                )
            };
            window.paint_quad(fill(
                rect(cx0, cy0 + side * 0.5, side, 1.0),
                Colors::with_alpha(ink, 0.12),
            ));
            window.paint_quad(fill(
                rect(cx0 + side * 0.5, cy0, 1.0, side),
                Colors::with_alpha(ink, 0.12),
            ));
            dashed(
                window,
                map(-span, -span),
                map(span, span),
                1.0,
                Colors::with_alpha(ink, 0.22),
            );
            let points: Vec<(f32, f32)> = (0..=160)
                .map(|i| {
                    let x = -span + 2.0 * span * i as f32 / 160.0;
                    let (px, py) = map(x, mixstation::drive_curve(params, x).clamp(-span, span));
                    (px, py)
                })
                .collect();
            paint_line(window, &points, 2.0, accent);
        }
        5 => {
            let (x0, y0, w, h) = frame_of(bounds);
            let centre = (x0 + w * 0.5, y0 + h * 0.55);
            let reach = (w * 0.5 - 40.0).max(10.0) * 0.5;
            let (left, right) = mixstation::width_of(params, 1.0, -1.0);
            for unity in [-1.0f32, 1.0] {
                let x = centre.0 + unity * reach;
                dashed(
                    window,
                    (x, centre.1 - 24.0),
                    (x, centre.1 + 24.0),
                    1.0,
                    Colors::with_alpha(ink, 0.25),
                );
            }
            label(
                window,
                cx,
                "L",
                size,
                faint,
                centre.0 - reach,
                centre.1 - 40.0,
                Align::Center,
            );
            label(
                window,
                cx,
                "R",
                size,
                faint,
                centre.0 + reach,
                centre.1 - 40.0,
                Align::Center,
            );
            let (lx, rx) = (centre.0 - left * reach, centre.0 - right * reach);
            paint_line(window, &[(lx, centre.1), (rx, centre.1)], 2.0, accent);
            dot(window, lx, centre.1, 6.0, accent);
            dot(window, rx, centre.1, 6.0, Colors::with_alpha(accent, 0.7));
            label(
                window,
                cx,
                "A hard-panned pair · dashed = unchanged",
                size,
                faint,
                centre.0,
                y0 + h - 18.0,
                Align::Center,
            );
        }
        _ => {}
    }
    for (handle, x, y) in handles(params, code, bounds) {
        let held = dragging == Some(handle);
        let r = if held { HANDLE_R + 1.0 } else { HANDLE_R };
        dot(window, x, y, r + 2.0, Colors::surface_canvas());
        dot(
            window,
            x,
            y,
            r,
            if running {
                Colors::accent_primary()
            } else {
                Colors::text_disabled()
            },
        );
    }
}

// ── Live ────────────────────────────────────────────────────────────────────

fn paint_mix_live(
    params: &Params,
    live: &mut Live,
    bounds: &LiveBounds,
    window: &mut Window,
    cx: &mut App,
) {
    let bypassed = !params.power;
    if let Some(b) = bounds.get(LiveSlot::Spectrum) {
        paint_spectrum(window, live, freq_plot(b));
    }
    if let Some(b) = bounds.get(LiveSlot::Meters) {
        paint_meters(
            window,
            cx,
            b,
            live,
            &[
                MeterColumn::Input,
                MeterColumn::Reduction("GR", "−"),
                MeterColumn::Output,
            ],
            bypassed,
        );
    }
    let rack = order(params);
    let peaks = live.slot_peaks();
    for (position, code) in rack.iter().enumerate() {
        if let Some(b) = bounds.get(LiveSlot::Stage(position)) {
            let (input, output) = match peaks {
                Some((inputs, outputs)) if !bypassed && active(params, *code) => {
                    (inputs[position], outputs[position])
                }
                _ => (0.0, 0.0),
            };
            paint_stage_bars(window, b, input, output);
        }
    }
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
    open_kit_editor(
        MixStationModel,
        owner_bounds,
        key,
        identity,
        host_ops,
        on_close,
        cx,
    )
}

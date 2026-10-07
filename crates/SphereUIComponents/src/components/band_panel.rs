//! The native editors of the band built-ins — the Compressor and Imager — as
//! one [`KitModel`] family.
//!
//! Both split the spectrum at three crossovers dragged on a frequency
//! display, with a card per band below:
//!
//! * the Compressor runs single-band (a transfer curve whose threshold is
//!   dragged sideways) or four-band (each band's live reduction hangs from
//!   the top of the display);
//! * the Imager widens or narrows each band — dragged up and down on the
//!   display — beside a vectorscope and correlation meters.
//!
//! The panel paints only what follows the params; spectra, reductions,
//! scopes and meters are painted by the window's overlay into the bounds
//! recorded here.

use std::sync::Arc;

use gpui::{
    canvas, div, fill, px, AnyElement, App, Bounds, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, MouseMoveEvent, ParentElement, Pixels, StatefulInteractiveElement, Styled,
    Window,
};

use crate::components::band_model::{
    band_id, crossover_bounds, knob, preset_applied, presets, short_hz, width_id, width_readout,
    BandKind, BandParams, BANDS, BAND_NAMES, CROSSOVER_IDS,
};
use crate::components::builtin_plugin_editor_window::{BuiltinEditorHostOps, PluginInstanceKey};
use crate::components::context_menu::ContextMenuEntry;
use crate::components::eq_graph::{freq_at_fraction, freq_fraction, paint_area, paint_line};
use crate::components::fx_model::KnobSpec;
use crate::components::native_plugin_shell::ShellIdentity;
use crate::components::plugin_kit::{
    bypass_notice, caption, choice, display_frame, header, hint, knob_card, knob_for,
    open_kit_editor, Cx, KitEditor, KitModel, KitPreset, KitWindow, LiveBounds, LiveSlot, KNOB,
    KNOB_PITCH,
};
use crate::components::plugin_live::{
    dashed, frame_of, label, paint_level_bar, paint_operating_point, paint_reduction_bar,
    paint_spectrum, transfer_plot, Align, Live, ScopeMode,
};
use crate::components::quick_sampler_panel::rect;
use crate::theme::{radius, space, state, typography, Colors};

/// The transfer display's input range, to 0 dBFS.
const TRANSFER_RANGE: f32 = -60.0;
/// How near a press must land to a crossover to take it.
const CROSSOVER_HIT: f32 = 8.0;
/// Drag travel under Shift.
const FINE: f32 = 0.2;
/// The frequencies the band display labels.
const FREQ_TICKS: [(f32, &str); 10] = [
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
];

/// A gesture on a display.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BandDrag {
    Crossover(usize),
    /// A band's width dragged on the display, from where the press began
    /// and every band's width then (Link Bands moves them all).
    Width {
        band: usize,
        origin_y: f32,
        start: [f32; BANDS],
    },
    /// A band's width fader, the same way.
    Fader {
        band: usize,
        origin_y: f32,
        start: [f32; BANDS],
    },
    Threshold,
}

#[derive(Default)]
pub struct BandUi {
    pub drag: Option<BandDrag>,
    /// The Imager's Link Bands: a width gesture moves every band by the
    /// same amount. An editor setting, not a param.
    pub link_bands: bool,
    /// What the Imager's vectorscope draws.
    pub scope: ScopeMode,
}

pub struct BandLiveView {
    params: BandParams,
    scope: ScopeMode,
}

impl KitModel for BandKind {
    type Params = BandParams;
    type Ui = BandUi;
    type LiveView = BandLiveView;

    fn title(self) -> &'static str {
        match self {
            BandKind::Comp => "Compressor",
            BandKind::Imager => "Imager",
        }
    }

    fn subtitle(self, params: &BandParams) -> String {
        match params {
            BandParams::Comp(p) if p.mode == compresser::Mode::Multi => "Four-band compression",
            BandParams::Comp(_) => "Single-band compression",
            BandParams::Imager(p) if p.multiband => "Four-band stereo imaging",
            BandParams::Imager(_) => "Stereo imaging",
        }
        .to_string()
    }

    fn key(self) -> &'static str {
        match self {
            BandKind::Comp => "compresser",
            BandKind::Imager => "imager",
        }
    }

    fn defaults(self) -> BandParams {
        BandParams::defaults(self)
    }

    fn from_mirror(self, insert_id: &str) -> Option<BandParams> {
        use crate::components::builtin_plugin_editor as mirror;
        Some(match self {
            BandKind::Comp => BandParams::Comp(mirror::builtin_compresser_params(insert_id)?),
            BandKind::Imager => BandParams::Imager(mirror::builtin_imager_params(insert_id)?),
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
        presets(self)
    }

    fn preset_applied(self, current: &BandParams, preset: &BandParams) -> BandParams {
        preset_applied(current, preset)
    }

    fn knob(self, id: &str) -> Option<KnobSpec> {
        knob(self, id)
    }

    fn panel(editor: &KitEditor<Self>, cx: &mut Cx<Self>) -> AnyElement {
        band_panel(editor, cx)
    }

    fn live_view(editor: &KitEditor<Self>) -> BandLiveView {
        BandLiveView {
            params: editor.params.clone(),
            scope: editor.ui.scope,
        }
    }

    fn paint_live(
        view: &BandLiveView,
        live: &mut Live,
        bounds: &LiveBounds,
        window: &mut Window,
        cx: &mut App,
    ) {
        match &view.params {
            BandParams::Imager(_) => crate::components::imager_panel::paint_imager_live(
                &view.params,
                view.scope,
                live,
                bounds,
                window,
                cx,
            ),
            BandParams::Comp(_) => paint_band_live(&view.params, live, bounds, window, cx),
        }
    }

    fn window_size(self) -> (f32, f32) {
        match self {
            BandKind::Comp => (1_080.0, 720.0),
            BandKind::Imager => (1_140.0, 760.0),
        }
    }

    fn min_size(self) -> (f32, f32) {
        match self {
            BandKind::Comp => (920.0, 620.0),
            BandKind::Imager => (1_000.0, 680.0),
        }
    }

    /// A soloed band is let go: a closed editor never leaves a track playing
    /// one band.
    fn on_close(self, params: &BandParams) -> Option<BandParams> {
        params.solo().map(|_| params.with("soloBand", -1.0))
    }

    fn key_down(editor: &mut KitEditor<Self>, key: &str, cx: &mut Cx<Self>) -> bool {
        if key == "escape" && editor.params.solo().is_some() {
            editor.set_value("soloBand", -1.0, cx);
            return true;
        }
        false
    }

    fn mouse_move(editor: &mut KitEditor<Self>, event: &MouseMoveEvent, cx: &mut Cx<Self>) {
        drag_to(editor, event, cx);
    }

    fn mouse_up(editor: &mut KitEditor<Self>, cx: &mut Cx<Self>) {
        if editor.ui.drag.take().is_some() {
            cx.notify();
        }
    }

    fn own_menu(_editor: &KitEditor<Self>, _menu: u8) -> Vec<ContextMenuEntry> {
        Vec::new()
    }
}

pub type BandEditorWindow = KitWindow<BandKind>;

// ── Geometry ────────────────────────────────────────────────────────────────

/// The band display's plot within its slot: room for the tags above and the
/// frequency labels below.
pub(crate) fn band_plot(bounds: Bounds<Pixels>) -> (f32, f32, f32, f32) {
    let (x0, y0, w, h) = frame_of(bounds);
    (x0, y0 + 26.0, w, (h - 26.0 - 18.0).max(1.0))
}

pub(crate) fn x_of(hz: f32, plot: (f32, f32, f32, f32)) -> f32 {
    plot.0 + freq_fraction(hz) * plot.2
}

// ── Gestures ────────────────────────────────────────────────────────────────

fn press_bands(
    e: &mut KitEditor<BandKind>,
    (x, y): (f32, f32),
    event: &MouseDownEvent,
    cx: &mut Cx<BandKind>,
) {
    let Some(bounds) = e.live_bounds.get(LiveSlot::Bands) else {
        return;
    };
    let plot = band_plot(bounds);
    let split = e.params.multiband();
    let crossovers = e.params.crossovers();
    let nearest = (0..CROSSOVER_IDS.len())
        .filter(|_| split)
        .map(|i| (i, (x_of(crossovers[i], plot) - x).abs()))
        .filter(|(_, distance)| *distance <= CROSSOVER_HIT)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(i, _)| i);
    let edges = e.params.edges();
    let band = if split {
        (0..BANDS).find(|b| x >= x_of(edges[*b], plot) && x < x_of(edges[b + 1], plot))
    } else {
        // One band works the whole signal.
        Some(0)
    };
    if event.click_count >= 2 {
        // A double-click puts back what it lands on.
        if let Some(i) = nearest {
            let default = BandParams::defaults(e.model).crossovers()[i];
            e.set_value(CROSSOVER_IDS[i], default, cx);
        } else if let (BandKind::Imager, Some(band)) = (e.model, band) {
            reset_width(e, band, cx);
        }
        return;
    }
    e.ui.drag = match (nearest, e.model, band) {
        (Some(i), _, _) => Some(BandDrag::Crossover(i)),
        (None, BandKind::Imager, Some(band)) => Some(BandDrag::Width {
            band,
            origin_y: y,
            start: widths(e),
        }),
        _ => None,
    };
    cx.notify();
}

/// Every band's width now.
pub(crate) fn widths(e: &KitEditor<BandKind>) -> [f32; BANDS] {
    std::array::from_fn(|band| e.value(width_id(band)))
}

/// The bands a width gesture on `band` moves: all of them with Link Bands
/// on and the split in play, else that one.
fn moved_bands(e: &KitEditor<BandKind>, band: usize) -> Vec<usize> {
    if e.ui.link_bands && e.params.multiband() {
        (0..BANDS).collect()
    } else {
        vec![band]
    }
}

/// Moves the gesture's bands `delta` percent from where they started.
fn drag_widths(
    e: &mut KitEditor<BandKind>,
    band: usize,
    start: [f32; BANDS],
    delta: f32,
    cx: &mut Cx<BandKind>,
) {
    let next = moved_bands(e, band)
        .into_iter()
        .fold(e.params.clone(), |next, b| {
            let width = (start[b] + delta).clamp(0.0, imager::MAX_WIDTH).round();
            next.with(width_id(b), width)
        });
    e.set_params(next, cx);
}

/// Puts the gesture's bands back to unchanged.
pub(crate) fn reset_width(e: &mut KitEditor<BandKind>, band: usize, cx: &mut Cx<BandKind>) {
    let next = moved_bands(e, band)
        .into_iter()
        .fold(e.params.clone(), |next, b| {
            next.with(width_id(b), imager::DEFAULT_WIDTH)
        });
    e.set_params(next, cx);
}

/// A press on a band's width fader: a double-click resets it, a press
/// starts dragging it.
pub(crate) fn press_fader(
    e: &mut KitEditor<BandKind>,
    band: usize,
    y: f32,
    event: &MouseDownEvent,
    cx: &mut Cx<BandKind>,
) {
    if event.click_count >= 2 {
        reset_width(e, band, cx);
        return;
    }
    e.ui.drag = Some(BandDrag::Fader {
        band,
        origin_y: y,
        start: widths(e),
    });
    cx.notify();
}

fn press_transfer(
    e: &mut KitEditor<BandKind>,
    (x, _): (f32, f32),
    event: &MouseDownEvent,
    cx: &mut Cx<BandKind>,
) {
    if event.click_count >= 2 {
        let default = BandParams::defaults(BandKind::Comp).value("thresholdDb");
        e.set_value("thresholdDb", default, cx);
        return;
    }
    e.ui.drag = Some(BandDrag::Threshold);
    set_threshold_at(e, x, cx);
}

fn set_threshold_at(e: &mut KitEditor<BandKind>, x: f32, cx: &mut Cx<BandKind>) {
    let Some(bounds) = e.live_bounds.get(LiveSlot::Transfer) else {
        return;
    };
    let (x0, _, w, _) = transfer_plot(bounds);
    let db = TRANSFER_RANGE - (x - x0) / w * TRANSFER_RANGE;
    let db = (db.clamp(TRANSFER_RANGE, 0.0) * 10.0).round() / 10.0;
    e.set_value("thresholdDb", db, cx);
}

fn drag_to(e: &mut KitEditor<BandKind>, event: &MouseMoveEvent, cx: &mut Cx<BandKind>) {
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
        BandDrag::Threshold => set_threshold_at(e, x, cx),
        BandDrag::Crossover(i) => {
            let Some(bounds) = e.live_bounds.get(LiveSlot::Bands) else {
                return;
            };
            let plot = band_plot(bounds);
            let (low, high) = crossover_bounds(&e.params.crossovers(), i);
            let hz = freq_at_fraction((x - plot.0) / plot.2)
                .clamp(low, high)
                .round();
            e.set_value(CROSSOVER_IDS[i], hz, cx);
        }
        BandDrag::Width {
            band,
            origin_y,
            start,
        } => {
            let Some(bounds) = e.live_bounds.get(LiveSlot::Bands) else {
                return;
            };
            let half = (band_plot(bounds).3 * 0.5 - 8.0).max(1.0);
            let scale = if event.modifiers.shift { FINE } else { 1.0 };
            drag_widths(e, band, start, (origin_y - y) / half * 100.0 * scale, cx);
        }
        BandDrag::Fader {
            band,
            origin_y,
            start,
        } => {
            let Some(bounds) = e.live_bounds.get(LiveSlot::Fader(band)) else {
                return;
            };
            let travel = f32::from(bounds.size.height).max(1.0);
            let scale = if event.modifiers.shift { FINE } else { 1.0 };
            drag_widths(
                e,
                band,
                start,
                (origin_y - y) / travel * imager::MAX_WIDTH * scale,
                cx,
            );
        }
    }
}

// ── The panel ───────────────────────────────────────────────────────────────

fn band_panel(e: &KitEditor<BandKind>, cx: &mut Cx<BandKind>) -> AnyElement {
    if e.model == BandKind::Imager {
        return crate::components::imager_panel::imager_panel(e, cx);
    }
    let mode_row = match &e.params {
        BandParams::Comp(p) => Some(
            div()
                .flex()
                .flex_row()
                .items_center()
                .flex_shrink_0()
                .gap(px(space::BASE))
                .child(caption("MODE"))
                .child(choice(
                    e,
                    cx,
                    "comp-mode",
                    &["Single", "Multi"],
                    Some(usize::from(p.mode == compresser::Mode::Multi)),
                    |this, index, cx| this.set_value("mode", index as f32, cx),
                ))
                .child(hint(if p.mode == compresser::Mode::Multi {
                    "Drag a crossover to move a band edge · Esc lets a solo go"
                } else {
                    "Drag across the curve to set the threshold"
                })),
        ),
        BandParams::Imager(_) => None,
    };
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
                .children(mode_row)
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_1()
                        .min_h(px(220.0))
                        .gap(px(space::BASE))
                        .child(main_display(e, cx))
                        .child(aside(e, cx)),
                )
                .child(bottom(e, cx)),
        )
        .into_any_element()
}

pub(crate) fn main_display(e: &KitEditor<BandKind>, cx: &mut Cx<BandKind>) -> AnyElement {
    let params = e.params.clone();
    let bypassed = !e.power();
    let notice = bypass_notice(e);
    let single = match &params {
        BandParams::Comp(p) if p.mode == compresser::Mode::Single => Some(p.clone()),
        _ => None,
    };
    if let Some(p) = single {
        let legend = format!(
            "{:.1} dB · {:.1}:1 · knee {:.1} dB",
            p.threshold_db, p.ratio, p.knee_db
        );
        let (threshold, ratio, knee) = (p.threshold_db, p.ratio, p.knee_db);
        let curve = canvas(
            |_, _, _| (),
            move |bounds, _, window, cx| {
                paint_comp_transfer(window, cx, bounds, threshold, ratio, knee, bypassed)
            },
        )
        .absolute()
        .size_full();
        return display_frame(
            "TRANSFER",
            Some(legend),
            notice,
            div()
                .id("comp-transfer")
                .relative()
                .size_full()
                .cursor(gpui::CursorStyle::ResizeLeftRight)
                .child(curve)
                .child(
                    e.live_bounds
                        .slot(LiveSlot::Transfer)
                        .absolute()
                        .size_full(),
                )
                .on_mouse_down(MouseButton::Left, e.press_cb(cx, press_transfer)),
        )
        .flex_1()
        .into_any_element();
    }
    let legend = match &params {
        BandParams::Comp(_) => "Reduction per band".to_string(),
        BandParams::Imager(p) if p.multiband => {
            "Drag a band up to widen it, a crossover to move it".to_string()
        }
        BandParams::Imager(_) => "Drag up to widen the whole signal".to_string(),
    };
    let dragging = e.ui.drag;
    let static_bands = canvas(
        |_, _, _| (),
        move |bounds, _, window, cx| {
            paint_band_display(window, cx, bounds, &params, dragging, bypassed)
        },
    )
    .absolute()
    .size_full();
    display_frame(
        "BANDS",
        Some(legend),
        notice,
        div()
            .id("band-display")
            .relative()
            .size_full()
            .child(static_bands)
            .child(e.live_bounds.slot(LiveSlot::Bands).absolute().size_full())
            .on_mouse_down(MouseButton::Left, e.press_cb(cx, press_bands)),
    )
    .flex_1()
    .into_any_element()
}

fn aside(e: &KitEditor<BandKind>, cx: &mut Cx<BandKind>) -> AnyElement {
    let well = |slot: LiveSlot, height: Option<f32>| {
        let well = div()
            .p(px(space::BASE))
            .border(px(1.0))
            .border_color(Colors::border_subtle())
            .bg(Colors::surface_panel())
            .rounded(px(radius::SURFACE))
            .child(e.live_bounds.slot(slot).size_full());
        match height {
            Some(h) => well.h(px(h)).flex_shrink_0(),
            None => well.flex_1().min_h(px(120.0)),
        }
    };
    let column = div()
        .flex()
        .flex_col()
        .w(px(250.0))
        .flex_shrink_0()
        .gap(px(space::BASE));
    match &e.params {
        BandParams::Comp(_) => column
            .child(well(LiveSlot::Meters, Some(124.0)))
            .child(knob_card(
                "OUTPUT",
                vec![
                    knob_for(e, cx, "kneeDb", KNOB, None),
                    knob_for(e, cx, "mix", KNOB, None),
                    knob_for(e, cx, "outputDb", KNOB, None),
                ],
            ))
            .into_any_element(),
        // The Imager draws its own scope column (`imager_panel`).
        BandParams::Imager(_) => column.into_any_element(),
    }
}

/// A band card's frame: soloed cards stand out, the rest step back while
/// one is soloed.
fn band_card(params: &BandParams, band: usize, min_w: f32, body: impl IntoElement) -> AnyElement {
    let edges = params.edges();
    let solo = params.solo();
    let soloed = solo == Some(band);
    let mut column = div()
        .flex()
        .flex_col()
        .flex_basis(px(0.0))
        .min_w(px(min_w))
        .gap(px(space::SNUG))
        .p(px(space::BASE))
        .rounded(px(radius::SURFACE))
        .border(px(1.0))
        .border_color(if soloed {
            Colors::with_alpha(Colors::accent_warning(), 0.6)
        } else {
            Colors::border_subtle()
        })
        .bg(Colors::surface_panel())
        .opacity(if solo.is_some() && !soloed { 0.55 } else { 1.0 });
    column.style().flex_grow = Some(1.0);
    column
        .child(
            div()
                .flex()
                .flex_row()
                .justify_between()
                .child(caption(BAND_NAMES[band].to_uppercase()))
                .child(hint(format!(
                    "{} – {} Hz",
                    short_hz(edges[band]),
                    short_hz(edges[band + 1])
                ))),
        )
        .child(body)
        .into_any_element()
}

/// A small switch pill: on lights it in `tone`.
pub(crate) fn pill(
    e: &KitEditor<BandKind>,
    cx: &mut Cx<BandKind>,
    id: (&'static str, usize),
    text: &'static str,
    on: bool,
    tone: gpui::Rgba,
    action: impl Fn(&mut KitEditor<BandKind>, &mut Cx<BandKind>) + 'static,
) -> AnyElement {
    div()
        .id(id)
        .px(px(space::SNUG))
        .h(px(20.0))
        .flex()
        .items_center()
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
        .text_color(if on { tone } else { Colors::text_muted() })
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(|style| style.border_color(Colors::border_strong()))
        .child(text)
        .on_click(e.click_cb(cx, action))
        .into_any_element()
}

fn bottom(e: &KitEditor<BandKind>, cx: &mut Cx<BandKind>) -> AnyElement {
    let row = div().flex().flex_row().flex_shrink_0().gap(px(space::BASE));
    let params = e.params.clone();
    match &params {
        BandParams::Comp(p) if p.mode == compresser::Mode::Single => {
            let k = |e: &KitEditor<BandKind>, cx: &mut Cx<BandKind>, id| {
                knob_for(e, cx, id, KNOB, None)
            };
            row.child(knob_card(
                "COMPRESSION",
                vec![
                    k(e, cx, "thresholdDb"),
                    k(e, cx, "ratio"),
                    k(e, cx, "attackMs"),
                    k(e, cx, "releaseMs"),
                    k(e, cx, "makeupDb"),
                    k(e, cx, "sidechainHpfHz"),
                ],
            ))
            .into_any_element()
        }
        BandParams::Comp(p) => {
            let cards: Vec<AnyElement> =
                (0..BANDS)
                    .map(|band| {
                        let bypass = p.bands[band].bypass;
                        let soloed = params.solo() == Some(band);
                        let k = |e: &KitEditor<BandKind>, cx: &mut Cx<BandKind>, field| {
                            knob_for(e, cx, band_id(band, field), KNOB, None)
                        };
                        let switches = div()
                            .flex()
                            .flex_row()
                            .gap(px(space::TIGHT))
                            .child(pill(
                                e,
                                cx,
                                ("band-bypass", band),
                                "Bypass",
                                bypass,
                                Colors::text_secondary(),
                                move |this, cx| this.toggle(band_id(band, "Bypass"), cx),
                            ))
                            .child(pill(
                                e,
                                cx,
                                ("band-solo", band),
                                "Solo",
                                soloed,
                                Colors::accent_warning(),
                                move |this, cx| {
                                    let next = if soloed { -1.0 } else { band as f32 };
                                    this.set_value("soloBand", next, cx)
                                },
                            ));
                        let knobs = div()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .gap(px(space::TIGHT))
                            .opacity(if bypass { state::DISABLED_CONTENT } else { 1.0 })
                            .child(k(e, cx, "ThresholdDb"))
                            .child(k(e, cx, "Ratio"))
                            .child(k(e, cx, "MakeupDb"))
                            .child(k(e, cx, "AttackMs"))
                            .child(k(e, cx, "ReleaseMs"));
                        band_card(
                            &params,
                            band,
                            3.0 * KNOB_PITCH + 2.0 * space::BASE,
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(space::SNUG))
                                .child(switches)
                                .child(div().h(px(22.0)).child(
                                    e.live_bounds.slot(LiveSlot::Reduction(band)).size_full(),
                                ))
                                .child(knobs),
                        )
                    })
                    .collect();
            row.children(cards).into_any_element()
        }
        // The Imager draws its own strips (`imager_panel`).
        BandParams::Imager(_) => row.into_any_element(),
    }
}

// ── Static painting ─────────────────────────────────────────────────────────

/// The single-band curve: the knee zone shaded, the threshold's handle, and
/// the gain computer the DSP runs (`compresser::curve_reduction_db`).
fn paint_comp_transfer(
    window: &mut Window,
    cx: &mut App,
    bounds: Bounds<Pixels>,
    threshold: f32,
    ratio: f32,
    knee: f32,
    bypassed: bool,
) {
    let (x0, y0, w, h) = transfer_plot(bounds);
    let range = TRANSFER_RANGE;
    let ink = Colors::text_primary();
    let x_at = |db: f32| x0 + (db - range) / -range * w;
    let y_at = |db: f32| y0 + h - (db.clamp(range, 0.0) - range) / -range * h;
    for db in [-48.0, -36.0, -24.0, -12.0, 0.0] {
        window.paint_quad(fill(
            rect(x_at(db), y0, 1.0, h),
            Colors::with_alpha(ink, 0.06),
        ));
        window.paint_quad(fill(
            rect(x0, y_at(db), w, 1.0),
            Colors::with_alpha(ink, 0.06),
        ));
        if db < 0.0 {
            let text = format!("{db:.0}");
            let size = typography::DENSE_CAPTION;
            label(
                window,
                cx,
                &text,
                size,
                Colors::text_faint(),
                x_at(db),
                y0 + h + 4.0,
                Align::Center,
            );
            label(
                window,
                cx,
                &text,
                size,
                Colors::text_faint(),
                x0 - 6.0,
                y_at(db) - 6.0,
                Align::Right,
            );
        }
    }
    let accent = Colors::accent_primary();
    let knee_left = x_at((threshold - knee * 0.5).max(range));
    let knee_right = x_at((threshold + knee * 0.5).min(0.0));
    window.paint_quad(fill(
        rect(knee_left, y0, (knee_right - knee_left).max(0.0), h),
        Colors::with_alpha(accent, 0.06),
    ));
    dashed(
        window,
        (x0, y0 + h),
        (x0 + w, y0),
        1.0,
        Colors::with_alpha(ink, 0.22),
    );
    let points: Vec<(f32, f32)> = (0..=160)
        .map(|i| {
            let input = range - range * i as f32 / 160.0;
            let output = if bypassed {
                input
            } else {
                input - compresser::curve_reduction_db(input, threshold, ratio, knee)
            };
            (x_at(input), y_at(output))
        })
        .collect();
    let alpha = if bypassed { 0.35 } else { 1.0 };
    paint_area(
        window,
        &points,
        y0 + h,
        Colors::with_alpha(accent, 0.08 * alpha),
    );
    paint_line(window, &points, 2.0, Colors::with_alpha(accent, alpha));

    // The threshold's handle: a line and its tag.
    let x = x_at(threshold);
    window.paint_quad(fill(
        rect(x - 0.5, y0, 1.0, h),
        Colors::with_alpha(Colors::accent_warning(), 0.8),
    ));
    let tag = rect(x - 26.0, y0 - 18.0, 52.0, 15.0);
    window.paint_quad(fill(tag, Colors::accent_warning()).corner_radii(px(3.0)));
    label(
        window,
        cx,
        &format!("{threshold:.1}"),
        typography::DENSE_CAPTION,
        Colors::surface_canvas(),
        x,
        y0 - 17.0,
        Align::Center,
    );
}

/// The band display's frame: the frequency grid and labels, the crossover
/// handles and — for the Imager — each band's width as a box around the
/// centre line.
fn paint_band_display(
    window: &mut Window,
    cx: &mut App,
    bounds: Bounds<Pixels>,
    params: &BandParams,
    dragging: Option<BandDrag>,
    bypassed: bool,
) {
    let plot = band_plot(bounds);
    let (x0, y0, w, h) = plot;
    let ink = Colors::text_primary();
    for (hz, text) in FREQ_TICKS {
        let x = x_of(hz, plot);
        let major = [100.0, 1_000.0, 10_000.0].contains(&hz);
        window.paint_quad(fill(
            rect(x, y0, 1.0, h),
            Colors::with_alpha(ink, if major { 0.09 } else { 0.045 }),
        ));
        let align = match hz {
            20.0 => Align::Left,
            20_000.0 => Align::Right,
            _ => Align::Center,
        };
        label(
            window,
            cx,
            text,
            typography::DENSE_CAPTION,
            Colors::text_faint(),
            x,
            y0 + h + 3.0,
            align,
        );
    }
    let edges = params.edges();
    let solo = params.solo();
    if let BandParams::Imager(p) = params {
        let centre = y0 + h * 0.5;
        let half = (h * 0.5 - 8.0).max(1.0);
        dashed(
            window,
            (x0, centre),
            (x0 + w, centre),
            1.0,
            Colors::with_alpha(ink, 0.12),
        );
        for unity in [centre - half * 0.5, centre + half * 0.5] {
            dashed(
                window,
                (x0, unity),
                (x0 + w, unity),
                1.0,
                Colors::with_alpha(ink, 0.08),
            );
        }
        let shown = if p.multiband { BANDS } else { 1 };
        for band in 0..shown {
            let (left, right) = if p.multiband {
                (
                    x_of(edges[band], plot) + 2.0,
                    x_of(edges[band + 1], plot) - 2.0,
                )
            } else {
                (x0 + 2.0, x0 + w - 2.0)
            };
            if right <= left {
                continue;
            }
            let reach = (p.width[band] / imager::MAX_WIDTH * half).max(0.75);
            let muted = solo.is_some_and(|s| s != band);
            let active = matches!(
                dragging,
                Some(BandDrag::Width { band: b, .. } | BandDrag::Fader { band: b, .. }) if b == band
            );
            let tone = if muted {
                Colors::text_disabled()
            } else {
                Colors::accent_primary()
            };
            let (fill_alpha, edge_alpha) = if active { (0.26, 0.9) } else { (0.16, 0.55) };
            let dim = if bypassed { 0.35 } else { 1.0 };
            let body = rect(left, centre - reach, right - left, 2.0 * reach);
            window.paint_quad(
                fill(body, Colors::with_alpha(tone, fill_alpha * dim))
                    .corner_radii(px(3.0))
                    .border_widths(px(1.0))
                    .border_color(Colors::with_alpha(tone, edge_alpha * dim)),
            );
            if right - left > 54.0 {
                label(
                    window,
                    cx,
                    &if p.multiband {
                        format!("{} {}", BAND_NAMES[band], width_readout(p.width[band]))
                    } else {
                        format!("Width {}", width_readout(p.width[band]))
                    },
                    typography::DENSE_CAPTION,
                    Colors::text_secondary(),
                    (left + right) * 0.5,
                    y0 + 4.0,
                    Align::Center,
                );
                if p.stereoize[band] >= 0.5 {
                    label(
                        window,
                        cx,
                        &format!(
                            "Stereoize {} {:.0}%",
                            p.stereoize_mode.label(),
                            p.stereoize[band]
                        ),
                        typography::DENSE_CAPTION,
                        Colors::accent_primary_hover(),
                        (left + right) * 0.5,
                        y0 + 17.0,
                        Align::Center,
                    );
                }
            }
        }
    }
    // With the split off there are no band edges to show.
    let crossovers = if params.multiband() {
        params.crossovers().to_vec()
    } else {
        Vec::new()
    };
    for (i, hz) in crossovers.iter().enumerate() {
        let x = x_of(*hz, plot);
        let active = dragging == Some(BandDrag::Crossover(i));
        let tone = if active {
            Colors::accent_warning()
        } else {
            Colors::with_alpha(Colors::text_secondary(), 0.8)
        };
        window.paint_quad(fill(rect(x - 0.5, y0, 1.0, h), tone));
        let tag = rect(x - 22.0, y0 + h - 17.0, 44.0, 15.0);
        window.paint_quad(
            fill(tag, Colors::surface_raised())
                .corner_radii(px(3.0))
                .border_widths(px(1.0))
                .border_color(tone),
        );
        label(
            window,
            cx,
            &short_hz(*hz),
            typography::DENSE_CAPTION,
            Colors::text_primary(),
            x,
            y0 + h - 16.0,
            Align::Center,
        );
    }
}

// ── Live ────────────────────────────────────────────────────────────────────

fn paint_band_live(
    params: &BandParams,
    live: &mut Live,
    bounds: &LiveBounds,
    window: &mut Window,
    cx: &mut App,
) {
    let bypassed = !params.power();
    if let Some(b) = bounds.get(LiveSlot::Bands) {
        let plot = band_plot(b);
        paint_spectrum(window, live, plot);
        if let BandParams::Comp(p) = params {
            paint_band_reduction(window, cx, plot, params, p, live.band_reduction(), bypassed);
        }
    }
    if let (Some(b), BandParams::Comp(p)) = (bounds.get(LiveSlot::Transfer), params) {
        if !bypassed {
            let (threshold, ratio, knee) = (p.threshold_db, p.ratio, p.knee_db);
            paint_operating_point(window, b, live, TRANSFER_RANGE, 0.0, |input| {
                input - compresser::curve_reduction_db(input, threshold, ratio, knee)
            });
        }
    }
    let frame = live.frame;
    if let Some(b) = bounds.get(LiveSlot::Meters) {
        let (x0, y0, w, h) = frame_of(b);
        let rows: &[usize] = if matches!(params, BandParams::Comp(_)) {
            &[0, 1, 2]
        } else {
            &[1, 2]
        };
        let row_h = h / rows.len() as f32;
        for (i, row) in rows.iter().enumerate() {
            let r = rect(x0, y0 + i as f32 * row_h, w, (row_h - 8.0).max(16.0));
            match row {
                0 => paint_reduction_bar(
                    window,
                    cx,
                    r,
                    "Reduction",
                    frame.map(|f| if bypassed { 0.0 } else { f.gain_reduction_db }),
                ),
                1 => paint_level_bar(
                    window,
                    cx,
                    r,
                    "In",
                    frame.map(|f| f.in_peak),
                    frame.map(|f| f.in_rms),
                ),
                _ => paint_level_bar(
                    window,
                    cx,
                    r,
                    "Out",
                    frame.map(|f| f.out_peak),
                    frame.map(|f| f.out_rms),
                ),
            }
        }
    }
    if let BandParams::Comp(p) = params {
        let reduction = live.band_reduction();
        for band in 0..BANDS {
            if let Some(b) = bounds.get(LiveSlot::Reduction(band)) {
                let off = bypassed || p.bands[band].bypass;
                let value = frame
                    .and(reduction)
                    .map(|r| if off { 0.0 } else { r[band] });
                paint_reduction_bar(window, cx, b, "Reduction", value);
            }
        }
    }
}

/// Each band's reduction hanging from the top of its stretch of the
/// display, over 24 dB, with its name and reading.
fn paint_band_reduction(
    window: &mut Window,
    cx: &mut App,
    plot: (f32, f32, f32, f32),
    params: &BandParams,
    p: &compresser::Params,
    reduction: Option<[f32; BANDS]>,
    bypassed: bool,
) {
    let (_, y0, _, h) = plot;
    let edges = params.edges();
    let solo = params.solo();
    for band in 0..BANDS {
        let (left, right) = (x_of(edges[band], plot), x_of(edges[band + 1], plot));
        if right - left < 2.0 {
            continue;
        }
        let off = bypassed || p.bands[band].bypass;
        let muted = off || solo.is_some_and(|s| s != band);
        let db = if off {
            0.0
        } else {
            reduction.map_or(0.0, |r| r[band])
        };
        let depth = (db / 24.0).clamp(0.0, 1.0) * h;
        let tone = if muted {
            Colors::text_disabled()
        } else {
            Colors::accent_primary()
        };
        if depth > 0.5 {
            window.paint_quad(fill(
                rect(left + 1.0, y0, right - left - 2.0, depth),
                Colors::with_alpha(tone, 0.2),
            ));
            window.paint_quad(fill(
                rect(left + 1.0, y0 + depth - 1.0, right - left - 2.0, 1.5),
                tone,
            ));
        }
        if right - left > 54.0 {
            let text = if p.bands[band].bypass {
                format!("{} bypassed", BAND_NAMES[band])
            } else {
                format!("{} −{db:.1} dB", BAND_NAMES[band])
            };
            label(
                window,
                cx,
                &text,
                typography::DENSE_CAPTION,
                Colors::text_secondary(),
                (left + right) * 0.5,
                y0 + 4.0,
                Align::Center,
            );
        }
    }
}

// ── Opening ─────────────────────────────────────────────────────────────────

/// Opens with a track's Compressor insert.
pub fn open_compresser_editor(
    owner_bounds: Option<Bounds<Pixels>>,
    key: PluginInstanceKey,
    identity: ShellIdentity,
    host_ops: BuiltinEditorHostOps,
    on_close: Arc<dyn Fn(&mut Window, &mut App)>,
    cx: &mut App,
) -> Result<gpui::WindowHandle<BandEditorWindow>, String> {
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

/// Opens with a track's Imager insert.
pub fn open_imager_editor(
    owner_bounds: Option<Bounds<Pixels>>,
    key: PluginInstanceKey,
    identity: ShellIdentity,
    host_ops: BuiltinEditorHostOps,
    on_close: Arc<dyn Fn(&mut Window, &mut App)>,
    cx: &mut App,
) -> Result<gpui::WindowHandle<BandEditorWindow>, String> {
    open_kit_editor(
        BandKind::Imager,
        owner_bounds,
        key,
        identity,
        host_ops,
        on_close,
        cx,
    )
}

//! EQUZ8's editor, after its design: the response graph with its draggable
//! nodes in the well; the band strip, then the Band, Dynamic and Output
//! cards on the deck.
//!
//! The curves come from the DSP's own filter design (`equz8::band_response_*`);
//! a dynamic band's curve, its marker, the detector and gain-change meters
//! and the output meters are live from `equz8::telemetry`.

use std::cell::RefCell;
use std::sync::Arc;

use equz8::telemetry as tm;
use equz8::{BandParams, BandType, DynMode, Params};
use gpui::{
    div, px, AnyElement, App, FontWeight, InteractiveElement, IntoElement, MouseButton,
    MouseMoveEvent, ParentElement, Rgba, StatefulInteractiveElement, Styled, Window,
};

use crate::components::builtin_plugin_editor::builtin_equz8_params;
use crate::components::builtin_plugin_editor_window::{BuiltinEditorHostOps, PluginInstanceKey};
use crate::components::eq_plate::{
    self, drag_target, paint_graph, x_of, Analyzer, BandCurve, GraphBand, GraphView, Scale,
    StaticCurves, CURVE_POINTS, FREQ_LABELS, GRAPH_W,
};
use crate::components::fx_model::KnobSpec;
use crate::components::native_plugin_shell::ShellIdentity;
use crate::components::plate::{
    self, caption, card, card_title, color, columns, deck, header, icon, knob, knob_reading,
    live_slot, numbers, paint_rect, paint_text, pill, plate_frame, plate_scale, switch, type_px,
    well, words, Align, Dial, DialUnit, KnobCell, KnobLook, PillLook, DYN, FAINT, GHOST, INK, OFF,
    PALE, SOFT, WHITE,
};
use crate::components::plugin_kit::{
    bank, open_kit_editor, Cx, KitEditor, KitModel, KitPreset, KitWindow, LiveBounds, LiveSlot,
};
use crate::components::plugin_live::Live;

/// EQUZ8's accent.
pub const ACCENT: u32 = 0xf2a23a;

const GRAPH_H: f32 = 262.0;
const SCALE: Scale = Scale {
    height: GRAPH_H,
    range: 18.0,
};

const SLOT_GRAPH: usize = 0;
const SLOT_DETECTOR: usize = 1;
const SLOT_GAIN_CHANGE: usize = 2;
const SLOT_OUTPUT: usize = 3;

const FREQ: Dial = Dial::new("freq", "FREQ", 20.0, 20_000.0, 1_000.0, DialUnit::Hz).log();
const GAIN: Dial = Dial::new("gain", "GAIN", -18.0, 18.0, 0.0, DialUnit::Db).bipolar();
const Q: Dial = Dial::new("q", "Q", 0.1, 18.0, 0.71, DialUnit::Q).log();
const THRESHOLD: Dial = Dial::new("thr", "THRESHOLD", -60.0, 0.0, -24.0, DialUnit::Db);
const RATIO: Dial = Dial::new("ratio", "RATIO", 1.0, 10.0, 2.0, DialUnit::Ratio).log();
const ATTACK: Dial = Dial::new("att", "ATTACK", 0.1, 200.0, 10.0, DialUnit::Ms).log();
const RELEASE: Dial = Dial::new("rel", "RELEASE", 5.0, 2_000.0, 100.0, DialUnit::Ms).log();
const RANGE: Dial = Dial::new("range", "RANGE", -18.0, 18.0, 0.0, DialUnit::Db).bipolar();
const OUT_GAIN: Dial = Dial::new("outputDb", "GAIN", -18.0, 18.0, 0.0, DialUnit::Db).bipolar();
const MIX: Dial = Dial::new("mix", "MIX", 0.0, 100.0, 100.0, DialUnit::Percent);

const TYPES: [BandType; 6] = [
    BandType::HighPass,
    BandType::LowShelf,
    BandType::Bell,
    BandType::Notch,
    BandType::HighShelf,
    BandType::LowPass,
];

fn type_label(kind: BandType) -> &'static str {
    match kind {
        BandType::HighPass => "Low cut",
        BandType::LowShelf => "Low shelf",
        BandType::Bell => "Bell",
        BandType::Notch => "Notch",
        BandType::HighShelf => "High shelf",
        BandType::LowPass => "High cut",
    }
}

fn type_icon(kind: BandType) -> &'static str {
    match kind {
        BandType::HighPass => eq_plate::ICON_LOW_CUT,
        BandType::LowShelf => eq_plate::ICON_LOW_SHELF,
        BandType::Bell => eq_plate::ICON_BELL,
        BandType::Notch => eq_plate::ICON_NOTCH,
        BandType::HighShelf => eq_plate::ICON_HIGH_SHELF,
        BandType::LowPass => eq_plate::ICON_HIGH_CUT,
    }
}

fn accent() -> Rgba {
    color(ACCENT)
}

/// A band's gain riding its detector: on, dynamic, and with a gain stage.
fn rides(band: &BandParams) -> bool {
    band.active && band.dynamic && band.band_type.has_gain()
}

/// The editor's own state: the selected band, a node drag, what the
/// analyser shows, and the curves of the params last drawn.
pub struct Z8Ui {
    selected: usize,
    dragging: Option<usize>,
    analyzer: Analyzer,
    curves: RefCell<Option<(Vec<(u32, f32)>, f32, Arc<StaticCurves>)>>,
}

impl Z8Ui {
    /// Selects band `slot` (the preview tool's way in).
    pub fn select(&mut self, slot: usize) {
        self.selected = slot.min(equz8::BAND_COUNT - 1);
    }
}

impl Default for Z8Ui {
    fn default() -> Self {
        Self {
            selected: 2,
            dragging: None,
            analyzer: Analyzer::Post,
            curves: RefCell::new(None),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Equz8Plate;

/// An EQUZ8 insert's editor window.
pub type Equz8Window = KitWindow<Equz8Plate>;

/// What the overlay paints from.
#[derive(Clone)]
pub struct Z8Live {
    graph: GraphView,
    params: Params,
    selected: usize,
}

fn band_curve(params: &Params, sample_rate: f32) -> BandCurve {
    let params = params.clone();
    Arc::new(move |slot: usize, gain: f32| {
        (0..CURVE_POINTS)
            .map(|i| {
                equz8::band_response_at_gain_db(
                    &params.bands[slot],
                    gain,
                    eq_plate::sample_hz(i),
                    sample_rate,
                )
            })
            .collect()
    })
}

fn graph_bands(params: &Params) -> Vec<GraphBand> {
    params
        .bands
        .iter()
        .enumerate()
        .map(|(slot, band)| GraphBand {
            slot,
            number: slot + 1,
            on: band.active,
            freq: band.freq,
            gain_db: band.gain_db,
            has_gain: band.band_type.has_gain(),
            dynamic: rides(band),
            range_db: band.range_db,
            tag: None,
        })
        .collect()
}

impl KitModel for Equz8Plate {
    type Params = Params;
    type Ui = Z8Ui;
    type LiveView = Z8Live;

    fn title(self) -> &'static str {
        "EQUZ8"
    }

    fn subtitle(self, _params: &Params) -> String {
        "8-BAND DYNAMIC EQ".into()
    }

    fn key(self) -> &'static str {
        "equz8-plate"
    }

    fn defaults(self) -> Params {
        equz8::default_params()
    }

    fn from_mirror(self, insert_id: &str) -> Option<Params> {
        builtin_equz8_params(insert_id)
    }

    fn wire_values(self, params: &Params) -> Vec<(u32, f32)> {
        equz8::ipc::ui_values(params)
            .into_iter()
            .filter_map(|(id, value)| equz8::ui_param_index(id).map(|index| (index, value)))
            .collect()
    }

    fn with(self, params: &Params, id: &str, value: f32) -> Params {
        let mut next = params.clone();
        equz8::ipc::apply_ui_param(&mut next, id, value);
        next
    }

    fn value(self, params: &Params, id: &str) -> f32 {
        equz8::ipc::ui_values(params)
            .into_iter()
            .find(|(candidate, _)| *candidate == id)
            .map_or(0.0, |(_, value)| value)
    }

    fn presets(self) -> Arc<Vec<KitPreset<Params>>> {
        static BANK: std::sync::OnceLock<Arc<Vec<KitPreset<Params>>>> = std::sync::OnceLock::new();
        BANK.get_or_init(|| {
            bank(equz8::factory_presets().iter().collect(), |p| {
                (p.name, p.params.clone())
            })
        })
        .clone()
    }

    /// Power stays as it is and any audition ends.
    fn preset_applied(self, current: &Params, preset: &Params) -> Params {
        let mut next = preset.clone();
        next.power = current.power;
        next.solo_band = equz8::ipc::SOLO_NONE;
        next
    }

    fn knob(self, _id: &str) -> Option<KnobSpec> {
        None
    }

    fn panel(e: &KitEditor<Self>, cx: &mut Cx<Self>) -> AnyElement {
        let s = plate_scale(e.viewport);
        plate_frame(s, vec![display(e, cx, s), controls(e, cx, s)])
    }

    fn live_view(e: &KitEditor<Self>) -> Z8Live {
        let wire = e.model.wire_values(&e.params);
        let curves = {
            let mut cache = e.ui.curves.borrow_mut();
            match cache.as_ref() {
                Some((key, rate, curves)) if *key == wire && *rate == e.sample_rate => {
                    curves.clone()
                }
                _ => {
                    let curves = Arc::new(StaticCurves::compute(
                        graph_bands(&e.params),
                        &band_curve(&e.params, e.sample_rate),
                    ));
                    *cache = Some((wire, e.sample_rate, curves.clone()));
                    curves
                }
            }
        };
        Z8Live {
            graph: GraphView {
                scale: SCALE,
                analyzer: e.ui.analyzer,
                bypass: !e.params.power,
                curves,
                curve: band_curve(&e.params, e.sample_rate),
                selected: Some(e.ui.selected),
                accent: accent(),
                gain_base: tm::GAIN_BASE,
                hover: None,
                hint: None,
                fill_alpha: 0.06,
                node_text: 11.0,
            },
            params: e.params.clone(),
            selected: e.ui.selected,
        }
    }

    fn paint_live(
        view: &Z8Live,
        live: &mut Live,
        bounds: &LiveBounds,
        window: &mut Window,
        cx: &mut App,
    ) {
        let viewport = window.viewport_size();
        let s = plate_scale((f32::from(viewport.width), f32::from(viewport.height)));
        if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_GRAPH)) {
            paint_graph(window, cx, live, &view.graph, b, s);
        }
        paint_dynamics(window, cx, live, view, bounds, s);
        if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_OUTPUT)) {
            paint_output_meters(window, live, b, s, [tm::OUT_L, tm::OUT_R], 210.0);
        }
    }

    fn window_size(self) -> (f32, f32) {
        plate::WINDOW_SIZE
    }

    fn min_size(self) -> (f32, f32) {
        plate::MIN_WINDOW_SIZE
    }

    /// A closed editor never leaves a track playing one band.
    fn on_close(self, params: &Params) -> Option<Params> {
        (params.solo_band != equz8::ipc::SOLO_NONE).then(|| {
            let mut next = params.clone();
            next.solo_band = equz8::ipc::SOLO_NONE;
            next
        })
    }

    fn mouse_move(e: &mut KitEditor<Self>, event: &MouseMoveEvent, cx: &mut Cx<Self>) {
        let Some(slot) = e.ui.dragging else {
            return;
        };
        if event.pressed_button != Some(MouseButton::Left) {
            e.ui.dragging = None;
            return;
        }
        let Some(b) = e.live_bounds.get(LiveSlot::Plate(SLOT_GRAPH)) else {
            return;
        };
        let (hz, db) = drag_target(b, SCALE, plate::xy(event.position));
        let mut next = e.params.clone();
        let band = &mut next.bands[slot];
        band.freq = DialUnit::Hz.round(hz).clamp(20.0, 20_000.0);
        if band.band_type.has_gain() {
            band.gain_db = DialUnit::Db.round(db).clamp(-18.0, 18.0);
        }
        e.set_params(next, cx);
    }

    fn mouse_up(e: &mut KitEditor<Self>, _cx: &mut Cx<Self>) {
        e.ui.dragging = None;
    }
}

// ── The well ───────────────────────────────────────────────────────────────

fn display(e: &KitEditor<Equz8Plate>, cx: &mut Cx<Equz8Plate>, s: f32) -> AnyElement {
    let analyzer = e.ui.analyzer;
    let analyzer_label = match analyzer {
        Analyzer::Post | Analyzer::PrePost => "Post",
        Analyzer::Pre => "Pre",
        Analyzer::Off => "Off",
    };
    let analyzer_button = div()
        .id("equz8-analyzer")
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0 * s))
        .h(px(28.0 * s))
        .px(px(10.0 * s))
        .rounded(px(3.0 * s))
        .border(px(1.0))
        .border_color(color(plate::WELL_EDGE))
        .bg(color(plate::GROUND))
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(|style| style.bg(color(0x262626)))
        .child(words("Analyzer", 12.0, SOFT, s))
        .child(numbers(
            analyzer_label,
            12.0,
            if analyzer == Analyzer::Off {
                FAINT
            } else {
                WHITE
            },
            s,
        ))
        .on_click(e.click_cb(cx, |this, cx| {
            this.ui.analyzer = match this.ui.analyzer {
                Analyzer::Post | Analyzer::PrePost => Analyzer::Pre,
                Analyzer::Pre => Analyzer::Off,
                Analyzer::Off => Analyzer::Post,
            };
            cx.notify();
        }))
        .into_any_element();
    // EQUZ8 runs its bands as minimum-phase biquads only: the processing
    // mode is shown, not offered.
    let phase = div()
        .flex()
        .items_center()
        .h(px(28.0 * s))
        .px(px(10.0 * s))
        .rounded(px(3.0 * s))
        .border(px(1.0))
        .border_color(color(plate::WELL_EDGE))
        .bg(color(plate::GROUND))
        .opacity(0.6)
        .child(words("Zero Latency", 12.0, INK, s))
        .into_any_element();

    let mut graph = div()
        .absolute()
        .left(px(16.0 * s))
        .top(px(2.0 * s))
        .w(px(GRAPH_W * s))
        .h(px(GRAPH_H * s))
        .child(live_slot(&e.live_bounds, SLOT_GRAPH).absolute().size_full());
    for (slot, band) in e.params.bands.iter().enumerate() {
        let y = SCALE.y_of(if band.band_type.has_gain() {
            band.gain_db
        } else {
            0.0
        });
        graph = graph.child(
            div()
                .id(("equz8-node", slot))
                .absolute()
                .left(px((x_of(band.freq) - 15.0) * s))
                .top(px((y - 15.0) * s))
                .size(px(30.0 * s))
                .rounded_full()
                .cursor(gpui::CursorStyle::OpenHand)
                .on_mouse_down(
                    MouseButton::Left,
                    e.press_cb(cx, move |this, _, _, cx| {
                        this.ui.selected = slot;
                        this.ui.dragging = Some(slot);
                        cx.notify();
                    }),
                ),
        );
    }
    let label = |text: String, left: f32, top: f32, size: f32, ink: u32, centred: bool| {
        let mut cell = div()
            .absolute()
            .left(px(left * s))
            .top(px(top * s))
            .child(numbers(text, size, ink, s));
        if centred {
            cell = cell.w(px(40.0 * s)).flex().justify_center();
        }
        cell
    };
    let mut labels = div().absolute().inset_0();
    for (hz, text) in FREQ_LABELS {
        labels = labels.child(label(
            text.into(),
            16.0 + x_of(hz) - 20.0,
            266.0,
            10.5,
            FAINT,
            true,
        ));
    }
    for v in [12.0, 6.0, 0.0, -6.0, -12.0] {
        labels = labels.child(label(
            eq_plate::db_label(v),
            962.0,
            2.0 + SCALE.y_of(v) - 7.0,
            10.5,
            FAINT,
            false,
        ));
    }
    labels = labels.child(label("dB".into(), 962.0, 4.0, 9.5, GHOST, false));

    well(s, 7.0, 327.0)
        .child(header(
            e,
            cx,
            s,
            "8-BAND DYNAMIC EQ",
            accent(),
            vec![analyzer_button, phase],
        ))
        .child(div().relative().flex_1().child(labels).child(graph))
        .into_any_element()
}

// ── The deck ───────────────────────────────────────────────────────────────

fn controls(e: &KitEditor<Equz8Plate>, cx: &mut Cx<Equz8Plate>, s: f32) -> AnyElement {
    deck(s, 341.0, 420.0)
        .flex_col()
        .p(px(14.0 * s))
        .gap(px(12.0 * s))
        .child(strip(e, cx, s))
        .child(
            div()
                .flex()
                .flex_row()
                .flex_1()
                .min_h(px(0.0))
                .gap(px(12.0 * s))
                .child(band_card(e, cx, s).w(px(300.0 * s)).flex_shrink_0())
                .child(dynamic_card(e, cx, s).w(px(448.0 * s)).flex_shrink_0())
                .child(output_card(e, cx, s).flex_1()),
        )
        .into_any_element()
}

/// What a strip button prints under the frequency.
fn band_detail(band: &BandParams) -> String {
    if band.band_type.has_gain() {
        plate::fmt_db(band.gain_db)
    } else if band.band_type == BandType::Notch {
        format!("Q {:.2}", band.q)
    } else {
        format!("{} dB/oct", band.slope.round())
    }
}

fn strip(e: &KitEditor<Equz8Plate>, cx: &mut Cx<Equz8Plate>, s: f32) -> gpui::Div {
    let buttons: Vec<AnyElement> = e
        .params
        .bands
        .iter()
        .enumerate()
        .map(|(slot, band)| {
            let selected = slot == e.ui.selected;
            let lit = if selected { accent() } else { color(WHITE) };
            let badge = rides(band).then(|| {
                div()
                    .px(px(4.0 * s))
                    .py(px(1.0 * s))
                    .rounded(px(2.0 * s))
                    .bg(color(DYN))
                    .text_color(color(plate::WELL))
                    .text_size(type_px(9.5, s))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(plate::tracked("Dyn", 0.08))
            });
            div()
                .id(("equz8-band", slot))
                .flex()
                .flex_col()
                .justify_between()
                .h(px(60.0 * s))
                .px(px(10.0 * s))
                .py(px(8.0 * s))
                .rounded(px(4.0 * s))
                .border(px(1.0))
                .border_color(if selected { accent() } else { color(0x424242) })
                .bg(color(if selected { 0x1d1d1d } else { 0x2a2a2a }))
                .opacity(if band.active { 1.0 } else { 0.55 })
                .cursor(gpui::CursorStyle::PointingHand)
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0 * s))
                        .child(
                            numbers((slot + 1).to_string(), 12.0, WHITE, s)
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(lit),
                        )
                        .child(icon(
                            type_icon(band.band_type),
                            (20.0, 14.0),
                            (20.0, 14.0),
                            1.5,
                            if selected { accent() } else { color(0xd0d0d0) },
                            s,
                        ))
                        .child(div().flex_1())
                        .children(badge)
                        .child(
                            div()
                                .size(px(6.0 * s))
                                .rounded_full()
                                .bg(color(if band.active { INK } else { 0x555555 })),
                        ),
                )
                .child(numbers(plate::fmt_hz(band.freq), 12.0, WHITE, s))
                .child(numbers(band_detail(band), 11.0, SOFT, s))
                .on_click(e.click_cb(cx, move |this, cx| {
                    this.ui.selected = slot;
                    cx.notify();
                }))
                .into_any_element()
        })
        .collect();
    columns(6.0, s, buttons)
}

/// Edits band `slot` of the params with `edit`.
fn edit_band(
    this: &mut KitEditor<Equz8Plate>,
    slot: usize,
    cx: &mut Cx<Equz8Plate>,
    edit: impl FnOnce(&mut BandParams),
) {
    let mut next = this.params.clone();
    edit(&mut next.bands[slot]);
    this.set_params(next, cx);
}

fn band_card(e: &KitEditor<Equz8Plate>, cx: &mut Cx<Equz8Plate>, s: f32) -> gpui::Div {
    let slot = e.ui.selected;
    let band = e.params.bands[slot];
    let defaults = equz8::default_params().bands[slot];
    let soloed = e.params.solo_band == slot as i32;
    let solo = {
        let (bg, border, fg) = if soloed {
            (accent(), accent(), color(plate::WELL))
        } else {
            (color(0x1f1f1f), color(0x484848), color(PALE))
        };
        div()
            .id("equz8-solo")
            .flex()
            .items_center()
            .h(px(26.0 * s))
            .px(px(10.0 * s))
            .rounded(px(3.0 * s))
            .border(px(1.0))
            .border_color(border)
            .bg(bg)
            .text_color(fg)
            .text_size(type_px(12.0, s))
            .cursor(gpui::CursorStyle::PointingHand)
            .child("Solo")
            .on_click(e.click_cb(cx, move |this, cx| {
                let mut next = this.params.clone();
                next.solo_band = if next.solo_band == slot as i32 {
                    equz8::ipc::SOLO_NONE
                } else {
                    slot as i32
                };
                this.set_params(next, cx);
            }))
    };
    let power = div()
        .id("equz8-band-power")
        .flex()
        .flex_row()
        .items_center()
        .gap(px(5.0 * s))
        .h(px(26.0 * s))
        .px(px(9.0 * s))
        .rounded(px(3.0 * s))
        .border(px(1.0))
        .border_color(color(0x484848))
        .bg(color(0x1f1f1f))
        .text_color(color(if band.active { WHITE } else { SOFT }))
        .text_size(type_px(12.0, s))
        .cursor(gpui::CursorStyle::PointingHand)
        .child(icon(
            plate::POWER,
            (14.0, 14.0),
            (12.0, 12.0),
            1.7,
            if band.active { accent() } else { color(OFF) },
            s,
        ))
        .child(if band.active { "On" } else { "Off" })
        .on_click(e.click_cb(cx, move |this, cx| {
            edit_band(this, slot, cx, |b| b.active = !b.active)
        }));

    let types: Vec<AnyElement> = TYPES
        .iter()
        .map(|&kind| {
            let on = band.band_type == kind;
            div()
                .id(("equz8-type", kind.to_wire() as usize))
                .flex()
                .items_center()
                .justify_center()
                .h(px(34.0 * s))
                .rounded(px(3.0 * s))
                .border(px(1.0))
                .border_color(if on { accent() } else { color(0x454545) })
                .bg(color(if on { 0x1d1d1d } else { 0x242424 }))
                .cursor(gpui::CursorStyle::PointingHand)
                .hover(move |style| style.bg(color(if on { 0x222222 } else { 0x2e2e2e })))
                .child(icon(
                    type_icon(kind),
                    (20.0, 14.0),
                    (22.0, 15.0),
                    1.5,
                    if on { accent() } else { color(plate::ICON) },
                    s,
                ))
                .on_click(e.click_cb(cx, move |this, cx| {
                    edit_band(this, slot, cx, |b| b.band_type = kind)
                }))
                .into_any_element()
        })
        .collect();

    let cut = band.band_type.is_cut();
    let slopes = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0 * s))
        .opacity(if cut { 1.0 } else { 0.4 })
        .child(caption("Slope", 10.5, 0.1, SOFT, s).w(px(48.0 * s)))
        .children(equz8::ipc::SLOPES.iter().map(|&slope| {
            pill(
                ("equz8-slope", slope as usize),
                format!("{}", slope as u32),
                cut && (band.slope - slope).abs() < 0.5,
                accent(),
                PillLook::numbers(26.0, 10.0, 12.0),
                cut,
                s,
                e.click_cb(cx, move |this, cx| {
                    edit_band(this, slot, cx, |b| b.slope = slope)
                }),
            )
        }))
        .child(words("dB/oct", 11.0, SOFT, s));

    let cell = KnobCell::new(76.0, 10.5, 12.0);
    let look = KnobLook::small(58.0);
    let has_gain = band.band_type.has_gain();
    let knobs = [
        (FREQ.with_default(defaults.freq), band.freq, true),
        (GAIN.with_default(defaults.gain_db), band.gain_db, has_gain),
        (Q.with_default(defaults.q), band.q, true),
    ]
    .map(|(dial, value, enabled)| {
        knob(
            cx,
            format!("equz8-{}-{slot}", dial.id),
            dial,
            value,
            look,
            cell,
            accent(),
            enabled,
            s,
            move |this, v, cx| {
                edit_band(this, slot, cx, |b| match dial.id {
                    "freq" => b.freq = v,
                    "gain" => b.gain_db = v,
                    _ => b.q = v,
                })
            },
        )
    });

    card(s)
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0 * s))
                .child(card_title(&format!("Band {}", slot + 1), s))
                .child(words(type_label(band.band_type), 13.0, SOFT, s))
                .child(div().flex_1())
                .child(solo)
                .child(power),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(6.0 * s))
                .child(caption("Filter type", 10.5, 0.1, SOFT, s))
                .child(columns(4.0, s, types)),
        )
        .child(slopes)
        .child(div().flex().flex_row().justify_between().children(knobs))
}

fn dynamic_card(e: &KitEditor<Equz8Plate>, cx: &mut Cx<Equz8Plate>, s: f32) -> gpui::Div {
    let slot = e.ui.selected;
    let band = e.params.bands[slot];
    let defaults = equz8::default_params().bands[slot];
    let has_gain = band.band_type.has_gain();
    let active = has_gain && band.dynamic;
    let dyn_color = color(DYN);

    let mode = |label: &'static str, value: DynMode, cx: &mut Cx<Equz8Plate>| {
        pill(
            ("equz8-mode", value.to_wire() as usize),
            label,
            band.dyn_mode == value,
            accent(),
            PillLook::words(26.0, 10.0, 12.0),
            active,
            s,
            e.click_cb(cx, move |this, cx| {
                edit_band(this, slot, cx, |b| b.dyn_mode = value)
            }),
        )
    };
    let above = mode("Above", DynMode::Above, cx);
    let below = mode("Below", DynMode::Below, cx);
    // The host gives a built-in no sidechain input: the band listens to
    // itself.
    let internal = pill(
        "equz8-sc-internal",
        "Internal",
        true,
        accent(),
        PillLook::words(26.0, 10.0, 12.0),
        active,
        s,
        |_, _, _| {},
    );
    let external = pill(
        "equz8-sc-external",
        "External",
        false,
        accent(),
        PillLook::words(26.0, 10.0, 12.0),
        false,
        s,
        |_, _, _| {},
    );

    let legacy = band.ratio < 0.5;
    let cell = KnobCell::new(76.0, 10.5, 12.0);
    let look = KnobLook::small(58.0);
    let dials = [
        (
            THRESHOLD.with_default(defaults.threshold_db),
            band.threshold_db,
        ),
        (
            RATIO.with_default(equz8::DEFAULT_RATIO),
            if legacy { RATIO.min } else { band.ratio },
        ),
        (ATTACK.with_default(defaults.attack_ms), band.attack_ms),
        (RELEASE.with_default(defaults.release_ms), band.release_ms),
        (RANGE.with_default(defaults.range_db), band.range_db),
    ];
    let knobs: Vec<AnyElement> = dials
        .into_iter()
        .map(|(dial, value)| {
            let readout = (dial.id == "ratio" && legacy).then(|| "Legacy".to_string());
            knob_reading(
                cx,
                format!("equz8-{}-{slot}", dial.id),
                dial,
                value,
                readout,
                look,
                cell,
                dyn_color,
                active,
                s,
                move |this, v, cx| {
                    edit_band(this, slot, cx, |b| match dial.id {
                        "thr" => b.threshold_db = v,
                        "ratio" => b.ratio = v,
                        "att" => b.attack_ms = v,
                        "rel" => b.release_ms = v,
                        _ => b.range_db = v,
                    })
                },
            )
        })
        .collect();

    let meter = |label: &str, slot_id: usize| {
        div()
            .relative()
            .flex()
            .flex_col()
            .gap(px(5.0 * s))
            .child(caption(label, 10.5, 0.1, SOFT, s).h(px(13.0 * s)))
            .child(div().h(px(8.0 * s)))
            .child(live_slot(&e.live_bounds, slot_id).absolute().size_full())
    };
    let (label, label_ink) = if active { ("On", WHITE) } else { ("Off", SOFT) };
    card(s)
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0 * s))
                .child(card_title("Dynamic", s))
                .child(words(
                    if has_gain {
                        format!("Band {}", slot + 1)
                    } else {
                        "Bell / Shelf only".to_string()
                    },
                    13.0,
                    SOFT,
                    s,
                ))
                .child(div().flex_1())
                .child(words(label, 12.0, label_ink, s))
                .child(switch(
                    "equz8-dynamic",
                    active,
                    dyn_color,
                    has_gain,
                    s,
                    e.click_cb(cx, move |this, cx| {
                        edit_band(this, slot, cx, |b| {
                            b.dynamic = !b.dynamic;
                            // A band made dynamic from here gets the ratio law.
                            if b.dynamic && b.ratio < 0.5 {
                                b.ratio = equz8::DEFAULT_RATIO;
                            }
                        })
                    }),
                )),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .justify_between()
                .flex_1()
                .pt(px(12.0 * s))
                .opacity(if active { 1.0 } else { 0.45 })
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0 * s))
                        .child(caption("Mode", 10.5, 0.1, SOFT, s).w(px(44.0 * s)))
                        .child(above)
                        .child(below)
                        .child(div().flex_1())
                        .child(caption("Sidechain", 10.5, 0.1, SOFT, s))
                        .child(internal)
                        .child(external),
                )
                .child(div().flex().flex_row().justify_between().children(knobs))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(10.0 * s))
                        .child(meter("Detector", SLOT_DETECTOR))
                        .child(meter("Gain change", SLOT_GAIN_CHANGE)),
                ),
        )
}

/// The detector and gain-change meters of the selected band.
fn paint_dynamics(
    window: &mut Window,
    cx: &mut App,
    live: &Live,
    view: &Z8Live,
    bounds: &LiveBounds,
    s: f32,
) {
    let band = view.params.bands[view.selected];
    let Some(block) = live.pads() else {
        return;
    };
    let level = block[tm::DETECTOR_BASE + view.selected];
    let gain = block[tm::GAIN_BASE + view.selected];
    let listening = level > tm::SILENT_DB + 1.0;
    let mono = plate::mono(FontWeight::NORMAL);
    let row = |window: &mut Window,
               cx: &mut App,
               slot: usize,
               text: String,
               bar: &dyn Fn(&mut Window, (f32, f32, f32, f32))| {
        let Some(b) = bounds.get(LiveSlot::Plate(slot)) else {
            return;
        };
        let (x, y) = plate::xy(b.origin);
        let w = f32::from(b.size.width);
        let h = f32::from(b.size.height);
        paint_text(
            window,
            cx,
            &text,
            mono.clone(),
            10.5 * s,
            color(INK),
            (x + w, y),
            Align::Right,
        );
        let bar_y = y + h - 8.0 * s;
        window.paint_quad(
            gpui::fill(plate::rect(x, bar_y, w, 8.0 * s), color(0x1a1a1a))
                .corner_radii(px(2.0 * s)),
        );
        bar(window, (x, bar_y, w, 8.0 * s));
    };
    let detector_text = if listening {
        format!(
            "In {} \u{00B7} Thr {} dB",
            plate::signed(level),
            plate::signed(band.threshold_db)
        )
    } else {
        format!("Thr {} dB", plate::signed(band.threshold_db))
    };
    row(
        window,
        cx,
        SLOT_DETECTOR,
        detector_text,
        &|window, (x, y, w, h)| {
            if listening {
                paint_rect(
                    window,
                    (x, y, w * ((level + 60.0) / 60.0).clamp(0.0, 1.0), h),
                    color(0x8c8c8c),
                );
            }
            let mark = ((band.threshold_db + 60.0) / 60.0).clamp(0.0, 1.0);
            paint_rect(window, (x + w * mark - 1.0 * s, y, 2.0 * s, h), color(DYN));
        },
    );
    row(window, cx, SLOT_GAIN_CHANGE, plate::fmt_db(gain), &|window,
                                                             (
        x,
        y,
        w,
        h,
    )| {
        paint_rect(window, (x + w * 0.5, y, 1.0, h), color(0x4a4a4a));
        let width = (gain.abs() / 18.0).min(1.0) * 0.5;
        let left = if gain < 0.0 { 0.5 - width } else { 0.5 };
        paint_rect(window, (x + w * left, y, w * width, h), color(DYN));
    });
}

fn output_card(e: &KitEditor<Equz8Plate>, cx: &mut Cx<Equz8Plate>, s: f32) -> gpui::Div {
    let p = &e.params;
    let cell = KnobCell::new(76.0, 10.5, 12.0);
    let look = KnobLook::small(58.0);
    let knobs = [(OUT_GAIN, p.output_db), (MIX, p.mix)].map(|(dial, value)| {
        knob(
            cx,
            format!("equz8-{}", dial.id),
            dial,
            value,
            look,
            cell,
            color(INK),
            true,
            s,
            move |this, v, cx| this.set_value(dial.id, v, cx),
        )
    });
    let scale = div().relative().w(px(28.0 * s)).h(px(210.0 * s)).children(
        [
            ("0", 17.3),
            ("\u{2212}6", 40.7),
            ("\u{2212}12", 64.0),
            ("\u{2212}24", 110.7),
            ("\u{2212}36", 157.3),
            ("\u{2212}48", 196.0),
        ]
        .map(|(text, top)| {
            div()
                .absolute()
                .left(px(2.0 * s))
                .top(px(top * s))
                .child(numbers(text, 10.0, SOFT, s))
        }),
    );
    let meters = div()
        .flex()
        .flex_col()
        .gap(px(6.0 * s))
        .items_start()
        .child(
            div()
                .flex()
                .flex_row()
                .gap(px(6.0 * s))
                .child(
                    div()
                        .w(px(30.0 * s))
                        .h(px(210.0 * s))
                        .child(live_slot(&e.live_bounds, SLOT_OUTPUT).size_full()),
                )
                .child(scale),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .gap(px(6.0 * s))
                .child(
                    numbers("L", 10.0, INK, s)
                        .w(px(12.0 * s))
                        .flex()
                        .justify_center(),
                )
                .child(
                    numbers("R", 10.0, INK, s)
                        .w(px(12.0 * s))
                        .flex()
                        .justify_center(),
                ),
        );
    card(s)
        .justify_start()
        .gap(px(10.0 * s))
        .child(card_title("Output", s))
        .child(
            div()
                .flex_1()
                .flex()
                .flex_row()
                .items_start()
                .gap(px(14.0 * s))
                .child(div().flex().flex_col().gap(px(18.0 * s)).children(knobs))
                .child(meters),
        )
}

/// Two output bars, left and right, from telemetry slots `slots`, `height`
/// design pixels tall.
pub(crate) fn paint_output_meters(
    window: &mut Window,
    live: &Live,
    b: gpui::Bounds<gpui::Pixels>,
    s: f32,
    slots: [usize; 2],
    height: f32,
) {
    let (x, y) = plate::xy(b.origin);
    for (i, slot) in slots.into_iter().enumerate() {
        let bx = x + i as f32 * 18.0 * s;
        window.paint_quad(
            gpui::fill(plate::rect(bx, y, 12.0 * s, height * s), color(0x111111))
                .corner_radii(px(2.0 * s)),
        );
        let Some(level) = live.pads().map(|block| block[slot]) else {
            continue;
        };
        let db = 20.0 * level.max(1.0e-6).log10();
        let h = ((db + 48.0) / 54.0).clamp(0.0, 1.0) * height;
        let ink = if db > -3.0 { accent() } else { color(0xd9d9d9) };
        paint_rect(window, (bx, y + (height - h) * s, 12.0 * s, h * s), ink);
    }
}

/// Opens with a track's EQUZ8 insert.
pub fn open_equz8_editor(
    owner_bounds: Option<gpui::Bounds<gpui::Pixels>>,
    key: PluginInstanceKey,
    identity: ShellIdentity,
    host_ops: BuiltinEditorHostOps,
    on_close: Arc<dyn Fn(&mut Window, &mut App)>,
    cx: &mut App,
) -> Result<gpui::WindowHandle<Equz8Window>, String> {
    plate::ensure_fonts(cx);
    open_kit_editor(
        Equz8Plate,
        owner_bounds,
        key,
        identity,
        host_ops,
        on_close,
        cx,
    )
}

const _: () = assert!(tm::SLOTS == SpherePluginHost::audio_bridge::BUILTIN_PAD_SLOTS);
const _: () = assert!(equz8::BAND_COUNT == 8);

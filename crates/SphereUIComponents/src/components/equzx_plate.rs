//! EQUZX's editor, after its design: a full-height response graph where a
//! double-click adds a band and a node's own settings float beside it; the
//! band chips, display, analyser and output controls on the deck below.
//!
//! The curves come from the DSP's own filter design (`equzx::band_response_*`);
//! dynamic bands, their markers, the analyser and the output meters are live
//! from the host's spectrum and `equzx::telemetry`. The floating band panel
//! is deferred, so it paints over the live graph.

use std::cell::RefCell;
use std::sync::Arc;

use equzx::telemetry as tm;
use equzx::{BandChannel, BandParams, BandType, Params};
use gpui::{
    deferred, div, point, px, AnyElement, App, FontWeight, InteractiveElement, IntoElement,
    MouseButton, MouseMoveEvent, ParentElement, Rgba, StatefulInteractiveElement, Styled, Window,
};

use crate::components::builtin_plugin_editor::builtin_equzx_params;
use crate::components::builtin_plugin_editor_window::{BuiltinEditorHostOps, PluginInstanceKey};
use crate::components::eq_plate::{
    self, drag_target, note_of, paint_graph, x_of, Analyzer, BandCurve, GraphBand, GraphView,
    Scale, StaticCurves, CURVE_POINTS, FREQ_LABELS, GRAPH_W,
};
use crate::components::fx_model::KnobSpec;
use crate::components::native_plugin_shell::ShellIdentity;
use crate::components::plate::{
    self, caption, color, color_a, icon, knob, knob_reading, live_slot, numbers, paint_rect,
    paint_text, pill, plate_frame, plate_scale, switch, type_px, well, words, Align, Dial,
    DialUnit, KnobCell, KnobLook, PillLook, DYN, FAINT, GHOST, INK, OFF, PALE, SOFT, WHITE,
};
use crate::components::plugin_kit::{
    open_kit_editor, Cx, KitEditor, KitModel, KitPreset, KitWindow, LiveBounds, LiveSlot,
};
use crate::components::plugin_live::Live;

/// EQUZX's accent.
pub const ACCENT: u32 = 0xf2a23a;

const GRAPH_H: f32 = 516.0;
const PANEL_W: f32 = 480.0;
const PANEL_H: f32 = 184.0;
const RANGES: [f32; 4] = [6.0, 12.0, 18.0, 30.0];
const SLOPES: [f32; 5] = [6.0, 12.0, 24.0, 48.0, 96.0];
/// How near a click must land to a node, in design pixels, to be the node's.
const NODE_REACH: f32 = 15.0;

const SLOT_GRAPH: usize = 0;
const SLOT_HOVER: usize = 1;
const SLOT_OUTPUT: usize = 2;
const SLOT_PEAK: usize = 3;

const FREQ: Dial = Dial::new("freq", "FREQ", 20.0, 20_000.0, 1_000.0, DialUnit::Hz).log();
const GAIN: Dial = Dial::new("gain", "GAIN", -30.0, 30.0, 0.0, DialUnit::Db).bipolar();
const Q: Dial = Dial::new("q", "Q", 0.1, 30.0, 1.0, DialUnit::Q).log();
const THRESHOLD: Dial = Dial::new("thr", "THRESHOLD", -60.0, 0.0, -30.0, DialUnit::Db);
const RANGE: Dial = Dial::new("range", "RANGE", -24.0, 24.0, -6.0, DialUnit::Db).bipolar();
const RATIO: Dial = Dial::new("ratio", "RATIO", 1.0, 10.0, 2.5, DialUnit::Ratio).log();
const OUT_GAIN: Dial = Dial::new("outputDb", "GAIN", -18.0, 18.0, 0.0, DialUnit::Db).bipolar();
const MIX: Dial = Dial::new("mix", "MIX", 0.0, 100.0, 100.0, DialUnit::Percent);

const TYPES: [BandType; 6] = [
    BandType::LowCut,
    BandType::LowShelf,
    BandType::Bell,
    BandType::Notch,
    BandType::HighShelf,
    BandType::HighCut,
];
const CHANNELS: [(BandChannel, &str); 5] = [
    (BandChannel::Stereo, "St"),
    (BandChannel::Left, "L"),
    (BandChannel::Right, "R"),
    (BandChannel::Mid, "M"),
    (BandChannel::Side, "S"),
];

fn type_label(kind: BandType) -> &'static str {
    match kind {
        BandType::LowCut => "Low cut",
        BandType::LowShelf => "Low shelf",
        BandType::Bell => "Bell",
        BandType::Notch => "Notch",
        BandType::BandPass => "Band pass",
        BandType::HighShelf => "High shelf",
        BandType::HighCut => "High cut",
    }
}

fn type_icon(kind: BandType) -> &'static str {
    match kind {
        BandType::LowCut => eq_plate::ICON_LOW_CUT,
        BandType::LowShelf => eq_plate::ICON_LOW_SHELF,
        BandType::Bell => eq_plate::ICON_BELL,
        BandType::Notch => eq_plate::ICON_NOTCH,
        BandType::BandPass => eq_plate::ICON_BAND_PASS,
        BandType::HighShelf => eq_plate::ICON_HIGH_SHELF,
        BandType::HighCut => eq_plate::ICON_HIGH_CUT,
    }
}

fn channel_tag(channel: BandChannel) -> Option<&'static str> {
    match channel {
        BandChannel::Stereo => None,
        BandChannel::Left => Some("L"),
        BandChannel::Right => Some("R"),
        BandChannel::Mid => Some("M"),
        BandChannel::Side => Some("S"),
    }
}

fn accent() -> Rgba {
    color(ACCENT)
}

fn rides(band: &BandParams) -> bool {
    band.active && band.dynamic && band.band_type.has_gain()
}

fn empty_band() -> BandParams {
    equzx::default_params().bands[0]
}

/// Whether a slot holds a band: switched on, or set to anything but an
/// empty slot's values — a band switched off keeps its place.
fn in_use(band: &BandParams) -> bool {
    let empty = empty_band();
    band.active
        || band.band_type != empty.band_type
        || band.channel != empty.channel
        || band.freq != empty.freq
        || band.gain_db != empty.gain_db
        || band.q != empty.q
        || band.slope != empty.slope
        || band.dynamic != empty.dynamic
}

/// The slots in use, in slot order: the bands, numbered by position.
fn listed(params: &Params) -> Vec<usize> {
    (0..equzx::BAND_COUNT)
        .filter(|&slot| in_use(&params.bands[slot]))
        .collect()
}

/// The editor's own state.
pub struct ZxUi {
    selected: Option<usize>,
    dragging: Option<usize>,
    hover: Option<(f32, f32)>,
    analyzer: Analyzer,
    range: f32,
    curves: RefCell<Option<(Vec<(u32, f32)>, f32, Arc<StaticCurves>)>>,
}

impl Default for ZxUi {
    fn default() -> Self {
        Self {
            selected: None,
            dragging: None,
            hover: None,
            analyzer: Analyzer::PrePost,
            range: 18.0,
            curves: RefCell::new(None),
        }
    }
}

impl ZxUi {
    /// Selects the band in `slot` (the preview tool's way in).
    pub fn select(&mut self, slot: Option<usize>) {
        self.selected = slot.filter(|&slot| slot < equzx::BAND_COUNT);
    }

    fn scale(&self) -> Scale {
        Scale {
            height: GRAPH_H,
            range: self.range,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EquzxPlate;

/// An EQUZX insert's editor window.
pub type EquzxWindow = KitWindow<EquzxPlate>;

#[derive(Clone)]
pub struct ZxLive {
    graph: GraphView,
    hover: Option<(f32, f32)>,
}

fn band_curve(params: &Params, sample_rate: f32) -> BandCurve {
    let params = params.clone();
    Arc::new(move |slot: usize, gain: f32| {
        (0..CURVE_POINTS)
            .map(|i| {
                equzx::band_response_at_gain_db(
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
    listed(params)
        .into_iter()
        .enumerate()
        .map(|(position, slot)| {
            let band = &params.bands[slot];
            GraphBand {
                slot,
                number: position + 1,
                on: band.active,
                freq: band.freq,
                gain_db: band.gain_db,
                has_gain: band.band_type.has_gain(),
                dynamic: rides(band),
                range_db: band.range_db,
                tag: channel_tag(band.channel),
            }
        })
        .collect()
}

impl KitModel for EquzxPlate {
    type Params = Params;
    type Ui = ZxUi;
    type LiveView = ZxLive;

    const REPAINT_ON_HOVER: bool = true;

    fn title(self) -> &'static str {
        "EQUZX"
    }

    fn subtitle(self, _params: &Params) -> String {
        "24-BAND FREE EQ".into()
    }

    fn key(self) -> &'static str {
        "equzx-plate"
    }

    fn defaults(self) -> Params {
        equzx::default_params()
    }

    fn from_mirror(self, insert_id: &str) -> Option<Params> {
        builtin_equzx_params(insert_id)
    }

    fn wire_values(self, params: &Params) -> Vec<(u32, f32)> {
        equzx::ipc::ui_values(params)
            .into_iter()
            .filter_map(|(id, value)| equzx::ui_param_index(id).map(|index| (index, value)))
            .collect()
    }

    fn with(self, params: &Params, id: &str, value: f32) -> Params {
        let mut next = params.clone();
        equzx::ipc::apply_ui_param(&mut next, id, value);
        next
    }

    fn value(self, params: &Params, id: &str) -> f32 {
        equzx::ipc::ui_values(params)
            .into_iter()
            .find(|(candidate, _)| *candidate == id)
            .map_or(0.0, |(_, value)| value)
    }

    /// EQUZX ships no factory bank: its one preset is the flat start.
    fn presets(self) -> Arc<Vec<KitPreset<Params>>> {
        static BANK: std::sync::OnceLock<Arc<Vec<KitPreset<Params>>>> = std::sync::OnceLock::new();
        BANK.get_or_init(|| {
            Arc::new(vec![KitPreset {
                name: "Flat",
                params: equzx::default_params(),
            }])
        })
        .clone()
    }

    fn preset_applied(self, current: &Params, preset: &Params) -> Params {
        let mut next = preset.clone();
        next.power = current.power;
        next.solo_band = equzx::ipc::SOLO_NONE;
        next
    }

    fn knob(self, _id: &str) -> Option<KnobSpec> {
        None
    }

    fn panel(e: &KitEditor<Self>, cx: &mut Cx<Self>) -> AnyElement {
        let s = plate_scale(e.viewport);
        plate_frame(s, vec![display(e, cx, s), controls(e, cx, s)])
    }

    fn live_view(e: &KitEditor<Self>) -> ZxLive {
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
        let selected =
            e.ui.selected
                .and_then(|slot| curves.bands.iter().position(|b| b.slot == slot));
        ZxLive {
            graph: GraphView {
                scale: e.ui.scale(),
                analyzer: e.ui.analyzer,
                bypass: !e.params.power,
                curves,
                curve: band_curve(&e.params, e.sample_rate),
                selected,
                accent: accent(),
                gain_base: tm::GAIN_BASE,
                hover: e.ui.hover,
                hint: e
                    .ui
                    .selected
                    .is_none()
                    .then_some("Double-click to add a band \u{00B7} drag a node to move it"),
                fill_alpha: 0.05,
                node_text: 10.5,
            },
            hover: e.ui.hover,
        }
    }

    fn paint_live(
        view: &ZxLive,
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
        let mono = plate::mono(FontWeight::NORMAL);
        if let (Some(b), Some((hz, db))) = (bounds.get(LiveSlot::Plate(SLOT_HOVER)), view.hover) {
            let (x, y) = plate::xy(b.origin);
            let text = format!(
                "{} \u{00B7} {} \u{00B7} {}",
                plate::fmt_hz(hz),
                note_of(hz),
                plate::fmt_db(db)
            );
            paint_text(
                window,
                cx,
                &text,
                mono.clone(),
                12.0 * s,
                color(SOFT),
                (x + f32::from(b.size.width), y),
                Align::Right,
            );
        }
        let block = live.pads();
        if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_OUTPUT)) {
            let (x, y) = plate::xy(b.origin);
            let w = f32::from(b.size.width);
            for (row, slot) in [tm::OUT_L, tm::OUT_R].into_iter().enumerate() {
                let by = y + row as f32 * 11.0 * s;
                window.paint_quad(
                    gpui::fill(plate::rect(x, by, w, 7.0 * s), color(0x111111))
                        .corner_radii(px(2.0 * s)),
                );
                if let Some(level) = block.map(|block| block[slot]) {
                    let db = 20.0 * level.max(1.0e-6).log10();
                    let fraction = ((db + 48.0) / 54.0).clamp(0.0, 1.0);
                    let ink = if db > -3.0 { accent() } else { color(0xd9d9d9) };
                    paint_rect(window, (x, by, w * fraction, 7.0 * s), ink);
                }
            }
        }
        if let (Some(b), Some(block)) = (bounds.get(LiveSlot::Plate(SLOT_PEAK)), block) {
            let peak = block[tm::OUT_L].max(block[tm::OUT_R]);
            let db = 20.0 * peak.max(1.0e-6).log10();
            let (x, y) = plate::xy(b.origin);
            paint_text(
                window,
                cx,
                &format!("{} dB", plate::signed(db.max(-120.0))),
                mono,
                10.5 * s,
                color(INK),
                (x + f32::from(b.size.width), y),
                Align::Right,
            );
        }
    }

    fn window_size(self) -> (f32, f32) {
        plate::WINDOW_SIZE
    }

    fn min_size(self) -> (f32, f32) {
        plate::MIN_WINDOW_SIZE
    }

    fn on_close(self, params: &Params) -> Option<Params> {
        (params.solo_band != equzx::ipc::SOLO_NONE).then(|| {
            let mut next = params.clone();
            next.solo_band = equzx::ipc::SOLO_NONE;
            next
        })
    }

    fn mouse_move(e: &mut KitEditor<Self>, event: &MouseMoveEvent, cx: &mut Cx<Self>) {
        let Some(b) = e.live_bounds.get(LiveSlot::Plate(SLOT_GRAPH)) else {
            return;
        };
        let scale = e.ui.scale();
        let pointer = plate::xy(event.position);
        e.ui.hover = b
            .contains(&event.position)
            .then(|| drag_target(b, scale, pointer));
        let Some(slot) = e.ui.dragging else {
            return;
        };
        if event.pressed_button != Some(MouseButton::Left) {
            e.ui.dragging = None;
            return;
        }
        let (hz, db) = drag_target(b, scale, pointer);
        let mut next = e.params.clone();
        let band = &mut next.bands[slot];
        band.freq = DialUnit::Hz.round(hz).clamp(20.0, 20_000.0);
        if band.band_type.has_gain() {
            band.gain_db = DialUnit::Db.round(db).clamp(-30.0, 30.0);
        }
        e.set_params(next, cx);
    }

    fn mouse_up(e: &mut KitEditor<Self>, _cx: &mut Cx<Self>) {
        e.ui.dragging = None;
    }
}

// ── Editing ────────────────────────────────────────────────────────────────

fn edit_band(
    this: &mut KitEditor<EquzxPlate>,
    slot: usize,
    cx: &mut Cx<EquzxPlate>,
    edit: impl FnOnce(&mut BandParams),
) {
    let mut next = this.params.clone();
    edit(&mut next.bands[slot]);
    this.set_params(next, cx);
}

/// Adds a band at `hz` and `db` in the first free slot and selects it: a
/// low cut near the bottom, a high cut near the top, a bell between.
fn add_band(this: &mut KitEditor<EquzxPlate>, hz: f32, db: f32, cx: &mut Cx<EquzxPlate>) {
    let Some(slot) = (0..equzx::BAND_COUNT).find(|&slot| !in_use(&this.params.bands[slot])) else {
        return;
    };
    let kind = if hz < 35.0 {
        BandType::LowCut
    } else if hz > 16_000.0 {
        BandType::HighCut
    } else {
        BandType::Bell
    };
    let mut band = empty_band();
    band.active = true;
    band.band_type = kind;
    band.freq = DialUnit::Hz.round(hz).clamp(20.0, 20_000.0);
    band.gain_db = if kind.has_gain() {
        DialUnit::Db.round(db).clamp(-30.0, 30.0)
    } else {
        0.0
    };
    band.q = if kind == BandType::Bell { 1.0 } else { 0.71 };
    band.slope = 24.0;
    band.threshold_db = THRESHOLD.default;
    band.range_db = RANGE.default;
    band.ratio = RATIO.default;
    let mut next = this.params.clone();
    next.bands[slot] = band;
    this.ui.selected = Some(slot);
    this.set_params(next, cx);
}

/// Empties `slot`.
fn delete_band(this: &mut KitEditor<EquzxPlate>, slot: usize, cx: &mut Cx<EquzxPlate>) {
    let mut next = this.params.clone();
    next.bands[slot] = empty_band();
    if next.solo_band == slot as i32 {
        next.solo_band = equzx::ipc::SOLO_NONE;
    }
    this.ui.selected = None;
    this.set_params(next, cx);
}

/// The listed band whose node is within reach of design point `(x, y)`.
fn node_near(this: &KitEditor<EquzxPlate>, (x, y): (f32, f32)) -> Option<usize> {
    let scale = this.ui.scale();
    listed(&this.params).into_iter().find(|&slot| {
        let band = &this.params.bands[slot];
        let ny = scale.y_of(if band.band_type.has_gain() {
            band.gain_db
        } else {
            0.0
        });
        (x_of(band.freq) - x).abs() <= NODE_REACH && (ny - y).abs() <= NODE_REACH
    })
}

// ── The well ───────────────────────────────────────────────────────────────

fn display(e: &KitEditor<EquzxPlate>, cx: &mut Cx<EquzxPlate>, s: f32) -> AnyElement {
    let hover = div()
        .w(px(300.0 * s))
        .h(px(16.0 * s))
        .child(live_slot(&e.live_bounds, SLOT_HOVER).size_full())
        .into_any_element();
    let scale = e.ui.scale();

    // The display itself: a click lets go of the selection, a double-click
    // adds a band where it lands.
    let entity = cx.entity().clone();
    let background = div()
        .id("equzx-graph")
        .absolute()
        .inset_0()
        .cursor(gpui::CursorStyle::Crosshair)
        .on_click(move |event, _window, app| {
            let gpui::ClickEvent::Mouse(mouse) = event else {
                return;
            };
            let at = plate::xy(mouse.up.position);
            let double = event.click_count() >= 2;
            let _ = entity.update(app, |this, cx| {
                let Some(b) = this.live_bounds.get(LiveSlot::Plate(SLOT_GRAPH)) else {
                    return;
                };
                let s = plate_scale(this.viewport);
                let (bx, by) = plate::xy(b.origin);
                let local = ((at.0 - bx) / s, (at.1 - by) / s);
                if node_near(this, local).is_some() {
                    return;
                }
                if double {
                    let (hz, db) = drag_target(b, this.ui.scale(), at);
                    add_band(this, hz, db, cx);
                } else if this.ui.selected.take().is_some() {
                    cx.notify();
                }
            });
        });

    let mut graph = div()
        .absolute()
        .left(px(16.0 * s))
        .top(px(2.0 * s))
        .w(px(GRAPH_W * s))
        .h(px(GRAPH_H * s))
        .child(live_slot(&e.live_bounds, SLOT_GRAPH).absolute().size_full())
        .child(background);
    for slot in listed(&e.params) {
        let band = &e.params.bands[slot];
        let y = scale.y_of(if band.band_type.has_gain() {
            band.gain_db
        } else {
            0.0
        });
        graph = graph.child(
            div()
                .id(("equzx-node", slot))
                .absolute()
                .left(px((x_of(band.freq) - 15.0) * s))
                .top(px((y - 15.0) * s))
                .size(px(30.0 * s))
                .rounded_full()
                .cursor(gpui::CursorStyle::OpenHand)
                .on_mouse_down(
                    MouseButton::Left,
                    e.press_cb(cx, move |this, _, _, cx| {
                        this.ui.selected = Some(slot);
                        this.ui.dragging = Some(slot);
                        cx.notify();
                    }),
                ),
        );
    }
    if let Some(slot) = e.ui.selected.filter(|&slot| in_use(&e.params.bands[slot])) {
        graph = graph.child(band_panel(e, cx, s, slot));
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
            522.0,
            10.5,
            FAINT,
            true,
        ));
    }
    for v in scale.ticks().1 {
        labels = labels.child(label(
            eq_plate::db_label(v),
            962.0,
            2.0 + scale.y_of(v) - 7.0,
            10.5,
            FAINT,
            false,
        ));
    }
    labels = labels.child(label("dB".into(), 962.0, 4.0, 9.5, GHOST, false));

    well(s, 7.0, 596.0)
        .child(plate::header(
            e,
            cx,
            s,
            "24-BAND FREE EQ",
            accent(),
            vec![hover],
        ))
        .child(div().relative().flex_1().child(labels).child(graph))
        .into_any_element()
}

/// The selected band's settings, floating by its node.
fn band_panel(
    e: &KitEditor<EquzxPlate>,
    cx: &mut Cx<EquzxPlate>,
    s: f32,
    slot: usize,
) -> AnyElement {
    let band = e.params.bands[slot];
    let scale = e.ui.scale();
    let number = listed(&e.params)
        .iter()
        .position(|&candidate| candidate == slot)
        .map_or(0, |position| position + 1);
    let has_gain = band.band_type.has_gain();
    let ny = scale.y_of(if has_gain { band.gain_db } else { 0.0 });
    let left = (x_of(band.freq) - PANEL_W * 0.5).clamp(0.0, GRAPH_W - PANEL_W);
    let top = if ny < 300.0 {
        ny + 22.0
    } else {
        ny - 22.0 - PANEL_H
    };

    let small = |id: &'static str,
                 child: AnyElement,
                 (bg, border): (Rgba, Rgba),
                 on_click: Box<dyn Fn(&gpui::ClickEvent, &mut Window, &mut App)>| {
        div()
            .id(id)
            .flex()
            .items_center()
            .justify_center()
            .flex_shrink_0()
            .size(px(26.0 * s))
            .rounded(px(3.0 * s))
            .border(px(1.0))
            .border_color(border)
            .bg(bg)
            .cursor(gpui::CursorStyle::PointingHand)
            .child(child)
            .on_click(on_click)
    };
    let rest = (color(0x262626), color(0x484848));
    let types = div()
        .flex()
        .flex_row()
        .gap(px(2.0 * s))
        .ml(px(3.0 * s))
        .children(TYPES.iter().map(|&kind| {
            let on = band.band_type == kind;
            div()
                .id(("equzx-type", kind.to_wire() as usize))
                .flex()
                .items_center()
                .justify_center()
                .w(px(30.0 * s))
                .h(px(26.0 * s))
                .rounded(px(3.0 * s))
                .border(px(1.0))
                .border_color(if on { accent() } else { color(0x484848) })
                .bg(color(if on { 0x1d1d1d } else { 0x262626 }))
                .cursor(gpui::CursorStyle::PointingHand)
                .child(icon(
                    type_icon(kind),
                    (20.0, 14.0),
                    (20.0, 14.0),
                    1.5,
                    if on { accent() } else { color(plate::ICON) },
                    s,
                ))
                .on_click(e.click_cb(cx, move |this, cx| {
                    edit_band(this, slot, cx, |b| {
                        b.band_type = kind;
                        if kind.is_cut() && b.slope < 6.0 {
                            b.slope = 24.0;
                        }
                    })
                }))
        }));
    let channels = div()
        .flex()
        .flex_row()
        .gap(px(2.0 * s))
        .children(CHANNELS.iter().map(|&(channel, label)| {
            pill(
                ("equzx-channel", channel.to_wire() as usize),
                label,
                band.channel == channel,
                accent(),
                PillLook::numbers(26.0, 5.0, 11.0),
                true,
                s,
                e.click_cb(cx, move |this, cx| {
                    edit_band(this, slot, cx, |b| b.channel = channel)
                }),
            )
            .min_w(px(24.0 * s))
        }));
    let soloed = e.params.solo_band == slot as i32;
    let solo = small(
        "equzx-solo",
        div()
            .text_size(type_px(11.0, s))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(if soloed {
                color(plate::WELL)
            } else {
                color(PALE)
            })
            .child("S")
            .into_any_element(),
        if soloed { (accent(), accent()) } else { rest },
        Box::new(e.click_cb(cx, move |this, cx| {
            let mut next = this.params.clone();
            next.solo_band = if next.solo_band == slot as i32 {
                equzx::ipc::SOLO_NONE
            } else {
                slot as i32
            };
            this.set_params(next, cx);
        })),
    );
    let power = small(
        "equzx-band-power",
        icon(
            plate::POWER,
            (14.0, 14.0),
            (12.0, 12.0),
            1.7,
            if band.active { accent() } else { color(OFF) },
            s,
        )
        .into_any_element(),
        rest,
        Box::new(e.click_cb(cx, move |this, cx| {
            edit_band(this, slot, cx, |b| b.active = !b.active)
        })),
    );
    let delete = small(
        "equzx-delete",
        icon(
            plate::CROSS,
            (12.0, 12.0),
            (12.0, 12.0),
            1.5,
            color(plate::ICON),
            s,
        )
        .into_any_element(),
        rest,
        Box::new(e.click_cb(cx, move |this, cx| delete_band(this, slot, cx))),
    );

    let cell = KnobCell::new(66.0, 10.0, 11.5).label(0.06, PALE);
    let look = KnobLook::small(50.0);
    let band_knobs = [
        (FREQ, band.freq, true),
        (GAIN, band.gain_db, has_gain),
        (
            Q.with_default(if band.band_type == BandType::Bell {
                1.0
            } else {
                0.71
            }),
            band.q,
            true,
        ),
    ]
    .map(|(dial, value, enabled)| {
        knob(
            cx,
            format!("equzx-{}-{slot}", dial.id),
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
    let active = rides(&band) || (has_gain && band.dynamic);
    let legacy = band.ratio < 0.5;
    let dyn_knobs = [
        (THRESHOLD, band.threshold_db),
        (RANGE, band.range_db),
        (RATIO, if legacy { RATIO.min } else { band.ratio }),
    ]
    .map(|(dial, value)| {
        let readout = (dial.id == "ratio" && legacy).then(|| "Legacy".to_string());
        knob_reading(
            cx,
            format!("equzx-{}-{slot}", dial.id),
            dial,
            value,
            readout,
            look,
            cell,
            color(DYN),
            active,
            s,
            move |this, v, cx| {
                edit_band(this, slot, cx, |b| match dial.id {
                    "thr" => b.threshold_db = v,
                    "range" => b.range_db = v,
                    _ => b.ratio = v,
                })
            },
        )
    });

    let cut = band.band_type.is_cut();
    let slopes = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(4.0 * s))
        .opacity(if cut { 1.0 } else { 0.4 })
        .child(caption("Slope", 10.0, 0.08, PALE, s).mr(px(2.0 * s)))
        .children(SLOPES.iter().map(|&slope| {
            pill(
                ("equzx-slope", slope as usize),
                format!("{}", slope as u32),
                cut && (band.slope - slope).abs() < 0.5,
                accent(),
                PillLook::numbers(24.0, 6.0, 11.0),
                cut,
                s,
                e.click_cb(cx, move |this, cx| {
                    edit_band(this, slot, cx, |b| b.slope = slope)
                }),
            )
        }));

    let panel = div()
        .id("equzx-band-panel")
        .absolute()
        .left(px(left * s))
        .top(px(top * s))
        .w(px(PANEL_W * s))
        .flex()
        .flex_col()
        .gap(px(10.0 * s))
        .p(px(12.0 * s))
        .rounded(px(6.0 * s))
        .border(px(1.0))
        .border_color(color(0x4d4d4d))
        .bg(color(plate::DECK))
        .shadow(vec![gpui::BoxShadow {
            color: color_a(0, 0.55).into(),
            offset: point(px(0.0), px(14.0 * s)),
            blur_radius: px(36.0 * s),
            spread_radius: px(0.0),
            inset: false,
        }])
        .font(plate::outfit(FontWeight::NORMAL))
        // A press on the panel is the panel's, not the display's.
        .occlude()
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(5.0 * s))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .flex_shrink_0()
                        .size(px(24.0 * s))
                        .rounded_full()
                        .bg(accent())
                        .child(
                            numbers(number.to_string(), 11.0, plate::WELL, s)
                                .font_weight(FontWeight::SEMIBOLD),
                        ),
                )
                .child(types)
                .child(div().flex_1())
                .child(channels)
                .child(power)
                .child(solo)
                .child(delete),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_start()
                .justify_between()
                .children(band_knobs)
                .child(div().w(px(1.0)).h(px(84.0 * s)).bg(color(0x4d4d4d)))
                .children(dyn_knobs),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0 * s))
                .child(numbers(note_of(band.freq), 11.5, PALE, s).w(px(78.0 * s)))
                .child(slopes)
                .child(div().flex_1())
                .child(caption(
                    "Dynamic",
                    10.0,
                    0.08,
                    if active { WHITE } else { PALE },
                    s,
                ))
                .child(switch(
                    "equzx-dynamic",
                    active,
                    color(DYN),
                    has_gain,
                    s,
                    e.click_cb(cx, move |this, cx| {
                        edit_band(this, slot, cx, |b| {
                            b.dynamic = !b.dynamic;
                            if b.dynamic && b.ratio < 0.5 {
                                b.ratio = RATIO.default;
                            }
                        })
                    }),
                )),
        );
    deferred(panel).with_priority(1).into_any_element()
}

// ── The deck ───────────────────────────────────────────────────────────────

fn controls(e: &KitEditor<EquzxPlate>, cx: &mut Cx<EquzxPlate>, s: f32) -> AnyElement {
    let bands = listed(&e.params);
    let count = bands.len();
    let chips: Vec<AnyElement> = bands
        .iter()
        .enumerate()
        .map(|(position, &slot)| {
            let band = &e.params.bands[slot];
            let selected = e.ui.selected == Some(slot);
            let marker = if rides(band) { color(DYN) } else { color(INK) };
            div()
                .id(("equzx-chip", slot))
                .relative()
                .flex()
                .items_center()
                .justify_center()
                .flex_shrink_0()
                .size(px(30.0 * s))
                .overflow_hidden()
                .rounded(px(3.0 * s))
                .border(px(1.0))
                .border_color(if selected { accent() } else { color(0x484848) })
                .bg(color(if selected { 0x1d1d1d } else { 0x2a2a2a }))
                .opacity(if band.active { 1.0 } else { 0.5 })
                .cursor(gpui::CursorStyle::PointingHand)
                .child(
                    numbers(
                        (position + 1).to_string(),
                        11.0,
                        if selected { WHITE } else { 0xd0d0d0 },
                        s,
                    )
                    .font_weight(FontWeight::SEMIBOLD),
                )
                .child(
                    div()
                        .absolute()
                        .left_0()
                        .right_0()
                        .bottom_0()
                        .h(px(2.0 * s))
                        .bg(marker),
                )
                .on_click(e.click_cb(cx, move |this, cx| {
                    this.ui.selected = Some(slot);
                    cx.notify();
                }))
                .into_any_element()
        })
        .collect();
    let full = count >= equzx::BAND_COUNT;
    let add = div()
        .id("equzx-add")
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .size(px(30.0 * s))
        .rounded(px(3.0 * s))
        .border(px(1.0))
        .border_dashed()
        .border_color(color(0x6a6a6a))
        .bg(color(0x262626))
        .opacity(if full { 0.4 } else { 1.0 })
        .child(icon(
            plate::PLUS,
            (12.0, 12.0),
            (12.0, 12.0),
            1.5,
            color(INK),
            s,
        ));
    let add = if full {
        add
    } else {
        add.cursor(gpui::CursorStyle::PointingHand)
            .on_click(e.click_cb(cx, |this, cx| add_band(this, 1_000.0, 0.0, cx)))
    };

    let group = |label: &str, body: AnyElement| {
        div()
            .flex()
            .flex_col()
            .gap(px(6.0 * s))
            .child(caption(label, 10.5, 0.1, SOFT, s))
            .child(body)
            .into_any_element()
    };
    let range = e.ui.range;
    let ranges = div()
        .flex()
        .flex_row()
        .gap(px(2.0 * s))
        .children(RANGES.iter().map(|&value| {
            pill(
                ("equzx-range", value as usize),
                format!("\u{00B1}{}", value as u32),
                (range - value).abs() < 0.5,
                accent(),
                PillLook::numbers(28.0, 9.0, 11.5),
                true,
                s,
                e.click_cb(cx, move |this, cx| {
                    this.ui.range = value;
                    cx.notify();
                }),
            )
        }))
        .into_any_element();
    let analyzer = e.ui.analyzer;
    let analyzers = div()
        .flex()
        .flex_row()
        .gap(px(2.0 * s))
        .children(
            [
                (Analyzer::PrePost, "Pre + Post"),
                (Analyzer::Post, "Post"),
                (Analyzer::Off, "Off"),
            ]
            .into_iter()
            .enumerate()
            .map(|(index, (mode, label))| {
                pill(
                    ("equzx-analyzer", index),
                    label,
                    analyzer == mode,
                    accent(),
                    PillLook::words(28.0, 9.0, 12.0),
                    true,
                    s,
                    e.click_cb(cx, move |this, cx| {
                        this.ui.analyzer = mode;
                        cx.notify();
                    }),
                )
            }),
        )
        .into_any_element();
    // Minimum-phase biquads only: shown, not offered.
    let processing = div()
        .flex()
        .items_center()
        .w(px(118.0 * s))
        .h(px(28.0 * s))
        .px(px(10.0 * s))
        .rounded(px(3.0 * s))
        .border(px(1.0))
        .border_color(color(0x484848))
        .bg(color(0x262626))
        .opacity(0.6)
        .child(words("Zero Latency", 12.0, WHITE, s))
        .into_any_element();

    let p = &e.params;
    let cell = KnobCell::new(64.0, 10.0, 11.0).gap(1.0).label(0.06, SOFT);
    let look = KnobLook::small(44.0);
    let out_knobs = [(OUT_GAIN, p.output_db), (MIX, p.mix)].map(|(dial, value)| {
        knob(
            cx,
            format!("equzx-{}", dial.id),
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
    let auto_text = if p.auto_gain {
        plate::fmt_db(equzx::auto_gain_db(p, e.sample_rate))
    } else {
        "Off".to_string()
    };
    let auto = pill(
        "equzx-auto",
        auto_text,
        p.auto_gain,
        accent(),
        PillLook::numbers(28.0, 8.0, 11.5),
        true,
        s,
        e.click_cb(cx, |this, cx| this.toggle("autoGain", cx)),
    )
    .w(px(92.0 * s))
    .into_any_element();
    let output = div()
        .flex()
        .flex_col()
        .gap(px(6.0 * s))
        .w(px(150.0 * s))
        .child(
            div()
                .relative()
                .flex()
                .flex_row()
                .justify_between()
                .child(caption("Output", 10.5, 0.1, SOFT, s))
                .child(
                    div()
                        .w(px(80.0 * s))
                        .h(px(13.0 * s))
                        .child(live_slot(&e.live_bounds, SLOT_PEAK).size_full()),
                ),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .gap(px(6.0 * s))
                .py(px(4.0 * s))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(4.0 * s))
                        .child(numbers("L", 10.0, SOFT, s).w(px(8.0 * s)).h(px(7.0 * s)))
                        .child(numbers("R", 10.0, SOFT, s).w(px(8.0 * s)).h(px(7.0 * s))),
                )
                .child(
                    div()
                        .flex_1()
                        .h(px(18.0 * s))
                        .child(live_slot(&e.live_bounds, SLOT_OUTPUT).size_full()),
                ),
        );

    plate::deck(s, 610.0, 151.0)
        .flex_col()
        .px(px(14.0 * s))
        .py(px(12.0 * s))
        .gap(px(10.0 * s))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.0 * s))
                .child(caption("Bands", 10.5, 0.1, SOFT, s))
                .child(
                    numbers(format!("{count}/{}", equzx::BAND_COUNT), 12.0, WHITE, s)
                        .w(px(48.0 * s))
                        .ml(px(4.0 * s)),
                )
                .children(chips)
                .child(add),
        )
        .child(
            div()
                .flex_1()
                .flex()
                .flex_row()
                .items_end()
                .gap(px(22.0 * s))
                .child(group("Display range", ranges))
                .child(group("Analyzer", analyzers))
                .child(group("Processing", processing))
                .child(div().flex_1())
                .children(out_knobs)
                .child(group("Auto gain", auto))
                .child(output),
        )
        .into_any_element()
}

/// Opens with a track's EQUZX insert.
pub fn open_equzx_editor(
    owner_bounds: Option<gpui::Bounds<gpui::Pixels>>,
    key: PluginInstanceKey,
    identity: ShellIdentity,
    host_ops: BuiltinEditorHostOps,
    on_close: Arc<dyn Fn(&mut Window, &mut App)>,
    cx: &mut App,
) -> Result<gpui::WindowHandle<EquzxWindow>, String> {
    plate::ensure_fonts(cx);
    open_kit_editor(
        EquzxPlate,
        owner_bounds,
        key,
        identity,
        host_ops,
        on_close,
        cx,
    )
}

const _: () = assert!(tm::SLOTS == SpherePluginHost::audio_bridge::BUILTIN_PAD_SLOTS);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_insert_lists_no_bands_and_a_touched_slot_is_listed() {
        let mut params = equzx::default_params();
        assert!(listed(&params).is_empty());
        params.bands[3].active = true;
        params.bands[7].freq = 2_000.0;
        assert_eq!(listed(&params), vec![3, 7]);
    }
}

//! AirBoost's editor, after its design: the response and spectrum in the
//! well; Air, Character and Output cards on the deck.
//!
//! The EQ curve is drawn from `airboost::eq_response_db`, the coefficients
//! the DSP runs. The spectrum is the insert's input as the host measures it,
//! and the output line is that spectrum through the curve. The harmonics the
//! exciter adds, the de-esser's cut, the auto gain and the meters are live
//! from `airboost::telemetry` and the meter frame; with no audio there is no
//! spectrum and nothing is drawn in its place.

use std::sync::Arc;

use SpherePluginHost::spectrum::{MAX_HZ, MIN_HZ, SPECTRUM_BINS};
use airboost::{AirFreq, Params, Shape, StereoMode};
use gpui::{
    AnyElement, App, FontWeight, InteractiveElement, IntoElement, ParentElement, Rgba,
    StatefulInteractiveElement, Styled, Window, div, px,
};

use crate::components::builtin_plugin_editor::builtin_airboost_params;
use crate::components::builtin_plugin_editor_window::{BuiltinEditorHostOps, PluginInstanceKey};
use crate::components::fx_model::KnobSpec;
use crate::components::native_plugin_shell::ShellIdentity;
use crate::components::plate::{
    self, Align, Dial, DialUnit, FAINT, ICON, INK, KnobCell, KnobLook, PillLook, SOFT, caption,
    card, card_title, color, color_a, columns, deck, header, icon, knob, legend, live_slot,
    numbers, paint_dot, paint_polygon, paint_polyline, paint_rect, paint_text, pill, pill_colors,
    plate_frame, plate_scale, signed, switch, type_px, well, words,
};
use crate::components::plugin_kit::{
    Cx, KitEditor, KitModel, KitPreset, KitWindow, LiveBounds, LiveSlot, bank, open_kit_editor,
};
use crate::components::plugin_live::Live;

/// AirBoost's accent.
pub const ACCENT: u32 = 0x8fd3ff;
/// The de-esser's colour.
pub const DEESS: u32 = 0xf2a23a;

const SLOT_GRAPH: usize = 0;
const SLOT_DEESS_BAR: usize = 1;
const SLOT_DEESS_TEXT: usize = 2;
const SLOT_AUTO: usize = 3;
const SLOT_METER: usize = 4;

/// The graph's design size, and its frequency span: 20 Hz to 40 kHz.
const GW: f32 = 940.0;
const GH: f32 = 262.0;
const LOW_HZ: f32 = 20.0;
const DECADES: f32 = 3.301_03;
/// The curve's resolution.
const CURVE_POINTS: usize = 260;

const AIR: Dial = Dial::new("airDb", "AIR", 0.0, 12.0, 4.0, DialUnit::Db);
const PRESENCE: Dial = Dial::new("presenceDb", "PRESENCE", -6.0, 6.0, 0.0, DialUnit::Db).bipolar();
const EXCITE: Dial = Dial::new("excite", "EXCITE", 0.0, 100.0, 25.0, DialUnit::Percent);
const DE_ESS: Dial = Dial::new("deess", "DE-ESS", 0.0, 100.0, 30.0, DialUnit::Percent);
const MIX: Dial = Dial::new("mix", "MIX", 0.0, 100.0, 100.0, DialUnit::Percent);
const OUTPUT: Dial = Dial::new("outputDb", "OUTPUT", -12.0, 12.0, 0.0, DialUnit::Db).bipolar();

const STEREO: [(StereoMode, &str); 3] = [
    (StereoMode::Stereo, "Stereo"),
    (StereoMode::Mid, "Mid"),
    (StereoMode::Side, "Side"),
];

fn shape_icon(shape: Shape) -> &'static str {
    match shape {
        Shape::Shelf => "M1 10 L7 10 C11 10 11 3 15 3 L17 3",
        Shape::Bell => "M1 10 L5 10 C7 10 7.5 2 9 2 C10.5 2 11 10 13 10 L17 10",
    }
}

fn accent() -> Rgba {
    color(ACCENT)
}

fn db20(v: f32) -> f32 {
    20.0 * v.max(1.0e-9).log10()
}

/// The graph's x for `hz`, in design units.
fn xf(hz: f32) -> f32 {
    GW * (hz / LOW_HZ).log10() / DECADES
}

/// The EQ scale's y for `db`: +15 at the top, −9 at the bottom.
fn yd(db: f32) -> f32 {
    ((15.0 - db) / 24.0 * GH).clamp(0.0, GH)
}

/// The spectrum scale's y for `db`: −6 dBFS at the top, −90 at the bottom.
fn ya(db: f32) -> f32 {
    ((-db - 6.0) / 84.0 * GH).clamp(0.0, GH)
}

/// The frequency of curve point `i`.
fn curve_hz(i: usize) -> f32 {
    LOW_HZ * 2_000f32.powf(i as f32 / (CURVE_POINTS - 1) as f32)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AirBoostPlate;

/// An AirBoost insert's editor window.
pub type AirBoostWindow = KitWindow<AirBoostPlate>;

/// What the overlay paints from: the params and the host's rate.
#[derive(Clone)]
pub struct AirView {
    params: Params,
    sample_rate: f32,
}

impl KitModel for AirBoostPlate {
    type Params = Params;
    type Ui = ();
    type LiveView = AirView;

    fn title(self) -> &'static str {
        "AirBoost"
    }

    fn subtitle(self, _params: &Params) -> String {
        "AIR EQ \u{00B7} EXCITER".into()
    }

    fn key(self) -> &'static str {
        "airboost-plate"
    }

    fn defaults(self) -> Params {
        airboost::default_params()
    }

    fn from_mirror(self, insert_id: &str) -> Option<Params> {
        builtin_airboost_params(insert_id)
    }

    fn wire_values(self, params: &Params) -> Vec<(u32, f32)> {
        airboost::ipc::ui_values(params)
            .into_iter()
            .filter_map(|(id, value)| airboost::ui_param_index(id).map(|index| (index, value)))
            .collect()
    }

    fn with(self, params: &Params, id: &str, value: f32) -> Params {
        let mut next = params.clone();
        airboost::ipc::apply_ui_param(&mut next, id, value);
        next
    }

    fn value(self, params: &Params, id: &str) -> f32 {
        airboost::ipc::ui_values(params)
            .into_iter()
            .find(|(candidate, _)| *candidate == id)
            .map_or(0.0, |(_, value)| value)
    }

    fn presets(self) -> Arc<Vec<KitPreset<Params>>> {
        static BANK: std::sync::OnceLock<Arc<Vec<KitPreset<Params>>>> = std::sync::OnceLock::new();
        BANK.get_or_init(|| bank(airboost::factory_presets(), |p| (p.name, p.params)))
            .clone()
    }

    fn preset_applied(self, current: &Params, preset: &Params) -> Params {
        let mut next = preset.clone();
        next.power = current.power;
        next
    }

    fn knob(self, _id: &str) -> Option<KnobSpec> {
        None
    }

    fn panel(e: &KitEditor<Self>, cx: &mut Cx<Self>) -> AnyElement {
        let s = plate_scale(e.viewport);
        plate_frame(s, vec![display(e, cx, s), controls(e, cx, s)])
    }

    fn live_view(e: &KitEditor<Self>) -> AirView {
        AirView {
            params: e.params.clone(),
            sample_rate: e.sample_rate,
        }
    }

    fn paint_live(
        view: &AirView,
        live: &mut Live,
        bounds: &LiveBounds,
        window: &mut Window,
        cx: &mut App,
    ) {
        let viewport = window.viewport_size();
        let s = plate_scale((f32::from(viewport.width), f32::from(viewport.height)));
        let rate = live.sample_rate().unwrap_or(view.sample_rate).max(8_000.0);
        let block = live.pads().copied();
        let read = |slot: usize, floor: f32| block.map_or(floor, |b| b[slot]);
        let power = view.params.power;
        let auto_db = if view.params.auto_gain && power {
            read(airboost::telemetry::AUTO_GAIN_DB, 0.0)
        } else {
            0.0
        };
        let cut_db = if power {
            read(airboost::telemetry::DEESS_DB, 0.0).min(0.0)
        } else {
            0.0
        };
        let excite_db = block
            .filter(|_| power)
            .map(|b| b[airboost::telemetry::EXCITE_DB]);

        if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_GRAPH)) {
            paint_graph(window, cx, view, live, rate, cut_db, excite_db, b, s);
        }
        let mono = plate::mono(FontWeight::NORMAL);
        if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_DEESS_BAR)) {
            let (x, y) = plate::xy(b.origin);
            let (w, h) = (f32::from(b.size.width), f32::from(b.size.height));
            window.paint_quad(
                gpui::fill(plate::rect(x, y, w, h), color(0x1a1a1a)).corner_radii(px(2.0 * s)),
            );
            let fraction = (-cut_db / airboost::DEESS_MAX_DB).clamp(0.0, 1.0);
            if fraction > 0.0 {
                paint_rect(window, (x, y, w * fraction, h), color(DEESS));
            }
        }
        if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_DEESS_TEXT)) {
            let (x, y) = plate::xy(b.origin);
            paint_text(
                window,
                cx,
                &plate::fmt_db(cut_db),
                mono.clone(),
                10.5 * s,
                color(INK),
                (x + f32::from(b.size.width), y),
                Align::Right,
            );
        }
        if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_AUTO)) {
            if view.params.auto_gain && block.is_some() {
                let (x, y) = plate::xy(b.origin);
                paint_text(
                    window,
                    cx,
                    &plate::fmt_db(auto_db),
                    mono.clone(),
                    11.5 * s,
                    color(SOFT),
                    (x, y),
                    Align::Left,
                );
            }
        }
        if let Some(frame) = live.frame {
            for (index, peak) in [frame.in_peak, frame.out_peak].into_iter().enumerate() {
                if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_METER + index)) {
                    paint_meter(window, cx, b, db20(peak), s);
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
}

// ── The well ───────────────────────────────────────────────────────────────

fn display(e: &KitEditor<AirBoostPlate>, cx: &mut Cx<AirBoostPlate>, s: f32) -> AnyElement {
    let line_swatch = |ink: Rgba| div().w(px(12.0 * s)).h(px(2.0 * s)).bg(ink);
    let legends = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(14.0 * s))
        .child(legend(line_swatch(color(0x5a5a5a)), "Input", s))
        .child(legend(line_swatch(accent()), "Output", s))
        .child(legend(
            div().size(px(10.0 * s)).bg(color_a(ACCENT, 0.3)),
            "Added harmonics",
            s,
        ))
        .into_any_element();

    let graph = div()
        .absolute()
        .left(px(16.0 * s))
        .top(px(2.0 * s))
        .w(px(GW * s))
        .h(px(GH * s))
        .child(live_slot(&e.live_bounds, SLOT_GRAPH).absolute().size_full())
        .child(
            div()
                .absolute()
                .left(px(860.0 * s))
                .top(px(6.0 * s))
                .child(plate::over_live(words("above hearing", 10.0, FAINT, s))),
        );

    let mono_label = |text: &str, left: f32, top: f32, w: f32| {
        div()
            .absolute()
            .left(px(left * s))
            .top(px(top * s))
            .w(px(w * s))
            .flex()
            .justify_center()
            .child(numbers(text.to_string(), 10.5, FAINT, s))
    };
    let mut axis: Vec<AnyElement> = [
        ("50", 109.3),
        ("100", 195.0),
        ("200", 280.8),
        ("500", 394.1),
        ("1k", 479.8),
        ("2k", 565.5),
        ("5k", 678.8),
        ("10k", 764.6),
        ("20k", 850.3),
    ]
    .into_iter()
    .map(|(text, left)| mono_label(text, left, 266.0, 40.0).into_any_element())
    .collect();
    axis.push(
        div()
            .absolute()
            .left(px(930.0 * s))
            .top(px(266.0 * s))
            .w(px(30.0 * s))
            .flex()
            .justify_end()
            .child(numbers("40k", 10.5, FAINT, s))
            .into_any_element(),
    );
    for (text, top) in [
        ("+12", 27.8),
        ("+6", 93.3),
        ("0", 158.8),
        ("\u{2212}6", 224.3),
    ] {
        axis.push(
            div()
                .absolute()
                .left(px(962.0 * s))
                .top(px(top * s))
                .child(numbers(text, 10.5, FAINT, s))
                .into_any_element(),
        );
    }
    axis.push(
        div()
            .absolute()
            .left(px(962.0 * s))
            .top(px(4.0 * s))
            .child(numbers("dB", 9.5, 0x6f6f6f, s))
            .into_any_element(),
    );

    well(s, 7.0, 327.0)
        .child(header(
            e,
            cx,
            s,
            "AIR EQ \u{00B7} EXCITER",
            accent(),
            vec![legends],
        ))
        .child(div().relative().flex_1().child(graph).children(axis))
        .into_any_element()
}

/// The response and spectrum display into `b`.
#[allow(clippy::too_many_arguments)]
fn paint_graph(
    window: &mut Window,
    cx: &mut App,
    view: &AirView,
    live: &Live,
    rate: f32,
    cut_db: f32,
    excite_db: Option<f32>,
    b: gpui::Bounds<gpui::Pixels>,
    s: f32,
) {
    let at = plate::at(b, s);
    let params = &view.params;
    let vertical = |window: &mut Window, hz: f32, ink: Rgba| {
        let x = xf(hz);
        paint_polyline(window, &[at(x, 0.0), at(x, GH)], 1.0, ink, None);
    };
    let horizontal = |window: &mut Window, db: f32, ink: Rgba| {
        let y = yd(db);
        paint_polyline(window, &[at(0.0, y), at(GW, y)], 1.0, ink, None);
    };
    let minor = color(0x1b1b1b);
    for hz in [
        30.0, 40.0, 60.0, 70.0, 80.0, 90.0, 300.0, 400.0, 600.0, 700.0, 800.0, 900.0, 3_000.0,
        4_000.0, 6_000.0, 7_000.0, 8_000.0, 9_000.0, 30_000.0,
    ] {
        vertical(window, hz, minor);
    }
    for db in [9.0, 3.0, -3.0] {
        horizontal(window, db, minor);
    }
    let major = color(0x262626);
    for hz in [
        50.0, 100.0, 200.0, 500.0, 1_000.0, 2_000.0, 5_000.0, 10_000.0,
    ] {
        vertical(window, hz, major);
    }
    for db in [12.0, 6.0, -6.0] {
        horizontal(window, db, major);
    }
    let rule = color(0x3a3a3a);
    let x20 = xf(20_000.0);
    paint_polyline(
        window,
        &[at(x20, 0.0), at(x20, GH)],
        1.0,
        rule,
        Some((3.0 * s, 3.0 * s)),
    );
    horizontal(window, 0.0, rule);

    // Nothing exists above Nyquist: the curve stops there.
    let nyquist = rate * 0.5;
    let audible = |hz: f32| hz < nyquist;

    // The spectrum: the input as the host measures it, and through the EQ.
    if let Some(bins) = live.spectrum_bins() {
        let bin_hz =
            |i: usize| MIN_HZ * (MAX_HZ / MIN_HZ).powf(i as f32 / (SPECTRUM_BINS - 1) as f32);
        let corner = airboost::excite_corner_hz(params.air_freq.hz(), rate);
        // The measured level of what the exciter adds sets how far its
        // harmonics lift the bins above its corner, across two octaves.
        let lift = excite_db.map_or(0.0, |db| ((db + 40.0) * 0.5).clamp(0.0, 15.0));
        let mut pre = Vec::with_capacity(SPECTRUM_BINS);
        let mut post = Vec::with_capacity(SPECTRUM_BINS);
        let mut excited = Vec::with_capacity(SPECTRUM_BINS);
        for (i, &db) in bins.iter().enumerate() {
            let hz = bin_hz(i);
            let x = xf(hz);
            let out = db + airboost::eq_response_db(params, hz, rate);
            let ramp = if hz > corner {
                ((hz / corner).log2() / 2.0).clamp(0.0, 1.0)
            } else {
                0.0
            };
            pre.push(at(x, ya(db)));
            post.push(at(x, ya(out)));
            excited.push(at(x, ya(out + lift * ramp)));
        }
        if lift > 0.0 {
            let mut band = excited.clone();
            band.extend(post.iter().rev().copied());
            paint_polygon(window, &band, color_a(ACCENT, 0.22));
        }
        let shown = if lift > 0.0 { &excited } else { &post };
        let mut fill = shown.clone();
        fill.push(at(xf(MAX_HZ), GH));
        fill.push(at(0.0, GH));
        paint_polygon(window, &fill, color_a(ACCENT, 0.07));
        paint_polyline(window, &pre, 1.0, color(0x5a5a5a), None);
        paint_polyline(window, shown, 1.0, color_a(ACCENT, 0.7), None);
    }

    // The EQ curve.
    let curve: Vec<(f32, f32)> = (0..CURVE_POINTS)
        .map(curve_hz)
        .filter(|&hz| audible(hz))
        .map(|hz| at(xf(hz), yd(airboost::eq_response_db(params, hz, rate))))
        .collect();
    if let (Some(&first), Some(&last)) = (curve.first(), curve.last()) {
        let zero = at(0.0, yd(0.0)).1;
        let mut fill = curve.clone();
        fill.push((last.0, zero));
        fill.push((first.0, zero));
        paint_polygon(window, &fill, color_a(0xffffff, 0.05));
        let ink = color_a(0xf2f2f2, if params.power { 1.0 } else { 0.3 });
        paint_polyline(window, &curve, 2.2 * s, ink, None);
    }

    // The de-esser's dip, while it is cutting.
    if cut_db < -0.05 {
        let dip: Vec<(f32, f32)> = (0..CURVE_POINTS)
            .map(curve_hz)
            .filter(|&hz| hz > 3_500.0 && hz < 14_000.0 && audible(hz))
            .map(|hz| {
                at(
                    xf(hz),
                    yd(airboost::eq_response_with_cut_db(params, cut_db, hz, rate)),
                )
            })
            .collect();
        paint_polyline(
            window,
            &dip,
            1.4 * s,
            color(DEESS),
            Some((3.0 * s, 2.0 * s)),
        );
        let text = format!("DE-ESS {}", plate::fmt_db(cut_db));
        let (tx, ty) = at(xf(airboost::DEESS_HZ) - 34.0, 6.0);
        let font = plate::outfit(FontWeight::SEMIBOLD);
        let width = window
            .text_system()
            .shape_line(
                text.clone().into(),
                px(10.0 * s),
                &[gpui::TextRun {
                    len: text.len(),
                    font: font.clone(),
                    color: color(0x111111).into(),
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                }],
                None,
            )
            .width;
        window.paint_quad(
            gpui::fill(
                plate::rect(tx, ty, f32::from(width) + 10.0 * s, 15.0 * s),
                color(DEESS),
            )
            .corner_radii(px(2.0 * s)),
        );
        paint_text(
            window,
            cx,
            &text,
            font,
            10.0 * s,
            color(0x111111),
            (tx + 5.0 * s, ty + 1.0 * s),
            Align::Left,
        );
    }

    // The air band's handle.
    let corner = airboost::air_corner_hz(params.air_freq.hz(), rate);
    let (dx, dy) = at(
        xf(params.air_freq.hz()).min(GW - 8.0),
        yd(airboost::eq_response_db(params, corner, rate)),
    );
    paint_dot(window, dx, dy, 7.1 * s, accent());
    paint_dot(window, dx, dy, 4.9 * s, color(0x111111));
}

/// An IN / OUT meter row's bar and reading: −48 to +6 dB.
fn paint_meter(window: &mut Window, cx: &mut App, b: gpui::Bounds<gpui::Pixels>, db: f32, s: f32) {
    let (x, y) = plate::xy(b.origin);
    let (w, h) = (f32::from(b.size.width), f32::from(b.size.height));
    let bar_w = (w - 44.0 * s).max(0.0);
    let bar_y = y + (h - 7.0 * s) * 0.5;
    window.paint_quad(
        gpui::fill(plate::rect(x, bar_y, bar_w, 7.0 * s), color(0x111111))
            .corner_radii(px(2.0 * s)),
    );
    let fraction = ((db + 48.0) / 54.0).clamp(0.0, 1.0);
    let ink = if db > -1.0 { accent() } else { color(0xd9d9d9) };
    paint_rect(window, (x, bar_y, bar_w * fraction, 7.0 * s), ink);
    paint_text(
        window,
        cx,
        &signed(db.max(-99.9)),
        plate::mono(FontWeight::NORMAL),
        10.5 * s,
        color(INK),
        (x + w, y + (h - 13.65 * s) * 0.5),
        Align::Right,
    );
}

// ── The deck ───────────────────────────────────────────────────────────────

fn controls(e: &KitEditor<AirBoostPlate>, cx: &mut Cx<AirBoostPlate>, s: f32) -> AnyElement {
    deck(s, 341.0, 420.0)
        .p(px(14.0 * s))
        .gap(px(12.0 * s))
        .child(air_card(e, cx, s).w(px(420.0 * s)).flex_shrink_0())
        .child(character_card(e, cx, s).w(px(300.0 * s)).flex_shrink_0())
        .child(output_card(e, cx, s).flex_1())
        .into_any_element()
}

fn set(id: &'static str) -> impl Fn(&mut KitEditor<AirBoostPlate>, f32, &mut Cx<AirBoostPlate>) {
    move |this, value, cx| this.set_value(id, value, cx)
}

fn air_summary(p: &Params) -> String {
    let lead = match p.shape {
        Shape::Shelf => "Shelf from ",
        Shape::Bell => "Bell at ",
    };
    let side = match p.stereo_mode {
        StereoMode::Stereo => String::new(),
        mode => format!(" \u{00B7} {} only", mode.label()),
    };
    format!("{lead}{}Hz{side}", p.air_freq.label())
}

fn choice<T: Copy + PartialEq + 'static>(
    e: &KitEditor<AirBoostPlate>,
    cx: &mut Cx<AirBoostPlate>,
    id: &'static str,
    param: &'static str,
    current: T,
    options: &[(T, &'static str)],
    wire: fn(T) -> f32,
    look: PillLook,
    s: f32,
) -> Vec<AnyElement> {
    options
        .iter()
        .map(|&(option, label)| {
            let value = wire(option);
            pill(
                (id, value as usize),
                label,
                option == current,
                accent(),
                look,
                true,
                s,
                e.click_cb(cx, move |this, cx| this.set_value(param, value, cx)),
            )
            .into_any_element()
        })
        .collect()
}

fn air_card(e: &KitEditor<AirBoostPlate>, cx: &mut Cx<AirBoostPlate>, s: f32) -> gpui::Div {
    let p = &e.params;
    let hero = knob(
        cx,
        "airboost-airDb",
        AIR,
        p.air_db,
        KnobLook::hero(138.0, 2.6, 1.4).styled(
            2.6,
            19.0,
            plate::FACE,
            plate::FACE_EDGE,
            0.6,
            7.0,
            1.4,
        ),
        KnobCell::new(150.0, 11.0, 18.0)
            .label_below()
            .gap(3.0)
            .label(0.12, SOFT),
        accent(),
        true,
        s,
        set(AIR.id),
    );

    let freqs: Vec<(AirFreq, &'static str)> = AirFreq::ALL
        .iter()
        .map(|&freq| (freq, freq.label()))
        .collect();
    let mut cells = choice(
        e,
        cx,
        "airboost-freq",
        "airFreq",
        p.air_freq,
        &freqs,
        AirFreq::to_wire,
        PillLook::numbers(30.0, 0.0, 12.0),
        s,
    )
    .into_iter();
    let freq_grid = div()
        .flex()
        .flex_col()
        .gap(px(3.0 * s))
        .child(columns(3.0, s, cells.by_ref().take(3).collect()))
        .child(columns(3.0, s, cells.collect()));

    let shapes: Vec<AnyElement> = [Shape::Shelf, Shape::Bell]
        .into_iter()
        .map(|shape| {
            let on = shape == p.shape;
            let (bg, border, fg) = pill_colors(on, accent());
            div()
                .id(("airboost-shape", shape.to_wire() as usize))
                .flex()
                .flex_row()
                .items_center()
                .justify_center()
                .gap(px(6.0 * s))
                .h(px(30.0 * s))
                .rounded(px(3.0 * s))
                .border(px(1.0))
                .border_color(border)
                .bg(bg)
                .text_color(fg)
                .text_size(type_px(12.5, s))
                .cursor(gpui::CursorStyle::PointingHand)
                .hover(move |style| style.bg(color(if on { 0x222222 } else { 0x2e2e2e })))
                .child(icon(
                    shape_icon(shape),
                    (18.0, 12.0),
                    (18.0, 12.0),
                    1.5,
                    if on { accent() } else { color(ICON) },
                    s,
                ))
                .child(shape.label())
                .on_click(e.click_cb(cx, move |this, cx| {
                    this.set_value("shape", shape.to_wire(), cx)
                }))
                .into_any_element()
        })
        .collect();

    let group = |label: &str, body: gpui::Div| {
        div()
            .flex()
            .flex_col()
            .gap(px(6.0 * s))
            .child(caption(label, 10.5, 0.1, SOFT, s))
            .child(body)
    };

    card(s)
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0 * s))
                .child(card_title("Air", s))
                .child(words(air_summary(p), 13.0, SOFT, s)),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(18.0 * s))
                .child(hero)
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w(px(0.0))
                        .gap(px(14.0 * s))
                        .child(group("Frequency", freq_grid))
                        .child(group("Shape", columns(3.0, s, shapes))),
                ),
        )
}

/// A Character knob with its help line under it.
fn character_knob(
    cx: &mut Cx<AirBoostPlate>,
    dial: Dial,
    value: f32,
    ink: Rgba,
    help: &'static str,
    s: f32,
) -> AnyElement {
    let cell = knob(
        cx,
        format!("airboost-{}", dial.id),
        dial,
        value,
        KnobLook::small(60.0),
        KnobCell::new(84.0, 10.5, 12.0),
        ink,
        true,
        s,
        set(dial.id),
    );
    div()
        .flex()
        .flex_col()
        .items_center()
        .w(px(84.0 * s))
        .gap(px(2.0 * s))
        .child(cell)
        .child(words(help, 10.0, FAINT, s))
        .into_any_element()
}

fn character_card(e: &KitEditor<AirBoostPlate>, cx: &mut Cx<AirBoostPlate>, s: f32) -> gpui::Div {
    let p = &e.params;
    let reduction = div()
        .flex()
        .flex_col()
        .gap(px(5.0 * s))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .child(caption("De-ess reduction", 10.5, 0.1, SOFT, s))
                .child(
                    div()
                        .w(px(80.0 * s))
                        .h(px(13.0 * s))
                        .child(live_slot(&e.live_bounds, SLOT_DEESS_TEXT).size_full()),
                ),
        )
        .child(
            div()
                .h(px(8.0 * s))
                .w_full()
                .child(live_slot(&e.live_bounds, SLOT_DEESS_BAR).size_full()),
        );
    card(s)
        .child(card_title("Character", s))
        .child(
            div()
                .flex()
                .flex_row()
                .justify_between()
                .child(character_knob(
                    cx,
                    PRESENCE,
                    p.presence_db,
                    accent(),
                    "Bell at 3.5 kHz",
                    s,
                ))
                .child(character_knob(
                    cx,
                    EXCITE,
                    p.excite,
                    accent(),
                    "Adds high harmonics",
                    s,
                ))
                .child(character_knob(
                    cx,
                    DE_ESS,
                    p.deess,
                    color(DEESS),
                    "Tames 5\u{2013}9 kHz",
                    s,
                )),
        )
        .child(reduction)
}

fn output_card(e: &KitEditor<AirBoostPlate>, cx: &mut Cx<AirBoostPlate>, s: f32) -> gpui::Div {
    let p = &e.params;
    let stereo = choice(
        e,
        cx,
        "airboost-stereo",
        "stereoMode",
        p.stereo_mode,
        &STEREO,
        StereoMode::to_wire,
        PillLook::words(28.0, 0.0, 12.0),
        s,
    );
    let small = |cx: &mut Cx<AirBoostPlate>, dial: Dial, value: f32| {
        knob(
            cx,
            format!("airboost-{}", dial.id),
            dial,
            value,
            KnobLook::small(50.0).styled(4.5, 16.0, plate::FACE, plate::FACE_EDGE, 1.2, 7.0, 2.4),
            KnobCell::new(76.0, 10.0, 11.5),
            color(INK),
            true,
            s,
            set(dial.id),
        )
    };
    let auto = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0 * s))
                .child(words("Auto gain", 12.5, INK, s))
                .child(if p.auto_gain {
                    div()
                        .w(px(64.0 * s))
                        .h(px(15.0 * s))
                        .child(live_slot(&e.live_bounds, SLOT_AUTO).size_full())
                        .into_any_element()
                } else {
                    numbers("Off", 11.5, SOFT, s).into_any_element()
                }),
        )
        .child(switch(
            "airboost-auto",
            p.auto_gain,
            accent(),
            true,
            s,
            e.click_cb(cx, |this, cx| this.toggle("autoGain", cx)),
        ));
    let meter = |index: usize, label: &str| {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0 * s))
            .child(
                div()
                    .w(px(26.0 * s))
                    .child(caption(label, 10.0, 0.08, SOFT, s)),
            )
            .child(
                div()
                    .flex_1()
                    .h(px(14.0 * s))
                    .child(live_slot(&e.live_bounds, SLOT_METER + index).size_full()),
            )
    };
    card(s)
        .child(card_title("Output", s))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(6.0 * s))
                .child(caption("Apply to", 10.5, 0.1, SOFT, s))
                .child(columns(3.0, s, stereo)),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .justify_around()
                .child(small(cx, MIX, p.mix))
                .child(small(cx, OUTPUT, p.output_db)),
        )
        .child(auto)
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(6.0 * s))
                .child(meter(0, "In"))
                .child(meter(1, "Out")),
        )
}

/// Opens with a track's AirBoost insert.
pub fn open_airboost_editor(
    owner_bounds: Option<gpui::Bounds<gpui::Pixels>>,
    key: PluginInstanceKey,
    identity: ShellIdentity,
    host_ops: BuiltinEditorHostOps,
    on_close: Arc<dyn Fn(&mut Window, &mut App)>,
    cx: &mut App,
) -> Result<gpui::WindowHandle<AirBoostWindow>, String> {
    plate::ensure_fonts(cx);
    open_kit_editor(
        AirBoostPlate,
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
    fn the_graph_scales_match_the_design() {
        assert!((xf(20_000.0) - 854.3).abs() < 0.2);
        // The design's "1k" label is centred at 499.8 in the body, which the
        // graph starts 16 into.
        assert!((xf(1_000.0) - 483.8).abs() < 0.3);
        assert!((yd(0.0) - 163.75).abs() < 0.01);
        assert_eq!(ya(0.0), 0.0);
        assert_eq!(ya(-200.0), GH);
        assert!((curve_hz(CURVE_POINTS - 1) - 40_000.0).abs() < 1.0);
    }

    #[test]
    fn the_summary_reads_like_the_design() {
        let mut p = airboost::default_params();
        assert_eq!(air_summary(&p), "Shelf from 12kHz");
        p.shape = Shape::Bell;
        p.air_freq = AirFreq::K2_5;
        p.stereo_mode = StereoMode::Side;
        assert_eq!(air_summary(&p), "Bell at 2.5kHz \u{00B7} Side only");
    }
}

//! BurnLimit's editor, after its design: the gain-reduction history and the
//! In / GR / Out meters in the well; the Limiter, Loudness and Output cards
//! on the deck.
//!
//! The history, the meters and every loudness readout are live — painted by
//! the kit window's overlay from the insert's meter frame and its telemetry
//! block (`burnlimit::telemetry`), and blank until the host publishes them.

use std::sync::Arc;

use gpui::{
    div, px, AnyElement, App, FontWeight, InteractiveElement, IntoElement, ParentElement, Rgba,
    StatefulInteractiveElement, Styled, Window,
};
use SpherePluginHost::audio_bridge::BUILTIN_PAD_SLOTS;

use burnlimit::telemetry as tm;
use burnlimit::{Dither, Oversampling, Params, Style};

use crate::components::builtin_plugin_editor::builtin_burnlimit_params;
use crate::components::builtin_plugin_editor_window::{BuiltinEditorHostOps, PluginInstanceKey};
use crate::components::fx_model::KnobSpec;
use crate::components::native_plugin_shell::ShellIdentity;
use crate::components::plate::{
    self, caption, card, card_title, color, color_a, columns, deck, header, knob, legend,
    live_slot, numbers, paint_polygon, paint_polyline, paint_rect, paint_text, pill, plate_frame,
    plate_scale, signed, switch, well, words, Align, Dial, DialUnit, KnobCell, KnobLook, PillLook,
    FAINT, GHOST, INK, SOFT, WHITE,
};
use crate::components::plugin_kit::{
    bank, open_kit_editor, Cx, KitEditor, KitModel, KitPreset, KitWindow, LiveBounds, LiveSlot,
};
use crate::components::plugin_live::Live;

/// BurnLimit's accent.
pub const ACCENT: u32 = 0xff6b35;

/// The history display's size, and the scale its envelopes and reduction
/// are drawn on.
const GRAPH_W: f32 = 800.0;
const GRAPH_H: f32 = 262.0;
const MID_Y: f32 = GRAPH_H * 0.5;
const HALF_H: f32 = 124.0;
/// History points across the display: the newest ~6.7 s at the telemetry
/// rate.
const GRAPH_POINTS: usize = 200;
/// Pixels per dB of reduction, from the top.
const GR_PX_PER_DB: f32 = 110.0 / 12.0;
/// The meters' bars, and the dB span they show.
const METER_H: f32 = 220.0;

/// The plate's live displays.
const SLOT_GRAPH: usize = 0;
const SLOT_METERS: usize = 1;
const SLOT_LUFS: usize = 2;
const SLOT_TARGET: usize = 3;
const SLOT_DELTA: usize = 4;
const SLOT_READOUT: usize = 10;

const GAIN: Dial = Dial::new("gainDb", "GAIN", -12.0, 24.0, 0.0, DialUnit::Db);
const CEILING: Dial = Dial::new("ceilingDb", "CEILING", -12.0, 0.0, -0.3, DialUnit::Db);
const RELEASE: Dial = Dial::new("releaseMs", "RELEASE", 1.0, 2_000.0, 200.0, DialUnit::Ms).log();
const LOOKAHEAD: Dial = Dial::new("lookaheadMs", "LOOKAHEAD", 0.0, 10.0, 2.0, DialUnit::Ms);
const KNEE: Dial = Dial::new("kneeDb", "KNEE", 0.0, 12.0, 0.0, DialUnit::Db);
const LINK: Dial = Dial::new("linkPct", "LINK", 0.0, 100.0, 100.0, DialUnit::Percent);

const TARGETS: [f32; 4] = [-14.0, -11.0, -9.0, -6.0];
const OVERSAMPLING: [(Oversampling, &str); 4] = [
    (Oversampling::Off, "Off"),
    (Oversampling::X2, "2x"),
    (Oversampling::X4, "4x"),
    (Oversampling::X8, "8x"),
];
const DITHER: [(Dither, &str); 3] = [
    (Dither::Off, "Off"),
    (Dither::Bits16, "16-bit"),
    (Dither::Bits24, "24-bit"),
];

fn accent() -> Rgba {
    color(ACCENT)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BurnLimitPlate;

/// A BurnLimit insert's editor window.
pub type BurnLimitWindow = KitWindow<BurnLimitPlate>;

/// What the overlay needs from the params.
#[derive(Clone)]
pub struct BurnLive {
    ceiling_db: f32,
    target_lufs: f32,
    power: bool,
}

impl KitModel for BurnLimitPlate {
    type Params = Params;
    type Ui = ();
    type LiveView = BurnLive;

    fn title(self) -> &'static str {
        "BurnLimit"
    }

    fn subtitle(self, _params: &Params) -> String {
        "TRUE PEAK LIMITER".into()
    }

    fn key(self) -> &'static str {
        "burnlimit-plate"
    }

    fn defaults(self) -> Params {
        burnlimit::default_params()
    }

    fn from_mirror(self, insert_id: &str) -> Option<Params> {
        builtin_burnlimit_params(insert_id)
    }

    fn wire_values(self, params: &Params) -> Vec<(u32, f32)> {
        burnlimit::ipc::ui_values(params)
            .into_iter()
            .filter_map(|(id, value)| burnlimit::ui_param_index(id).map(|index| (index, value)))
            .collect()
    }

    fn with(self, params: &Params, id: &str, value: f32) -> Params {
        let mut next = params.clone();
        burnlimit::ipc::apply_ui_param(&mut next, id, value);
        next
    }

    fn value(self, params: &Params, id: &str) -> f32 {
        burnlimit::ipc::ui_values(params)
            .into_iter()
            .find(|(candidate, _)| *candidate == id)
            .map_or(0.0, |(_, value)| value)
    }

    fn presets(self) -> Arc<Vec<KitPreset<Params>>> {
        static BANK: std::sync::OnceLock<Arc<Vec<KitPreset<Params>>>> = std::sync::OnceLock::new();
        BANK.get_or_init(|| bank(burnlimit::factory_presets(), |p| (p.name, p.params)))
            .clone()
    }

    /// Power and the loudness target — a meter reference, not a sound — stay
    /// as they are.
    fn preset_applied(self, current: &Params, preset: &Params) -> Params {
        let mut next = preset.clone();
        next.power = current.power;
        next.target_lufs = current.target_lufs;
        next
    }

    fn knob(self, _id: &str) -> Option<KnobSpec> {
        None
    }

    fn panel(e: &KitEditor<Self>, cx: &mut Cx<Self>) -> AnyElement {
        let s = plate_scale(e.viewport);
        plate_frame(s, vec![display(e, cx, s), controls(e, cx, s)])
    }

    fn live_view(e: &KitEditor<Self>) -> BurnLive {
        BurnLive {
            ceiling_db: e.params.ceiling_db,
            target_lufs: e.params.target_lufs,
            power: e.params.power,
        }
    }

    fn paint_live(
        view: &BurnLive,
        live: &mut Live,
        bounds: &LiveBounds,
        window: &mut Window,
        cx: &mut App,
    ) {
        let s = plate_scale(xy_size(window));
        if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_GRAPH)) {
            paint_graph(window, live, view, b, s);
        }
        if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_METERS)) {
            paint_meters(window, cx, live, b, s);
        }
        paint_loudness(window, cx, live, view, bounds, s);
    }

    fn window_size(self) -> (f32, f32) {
        plate::WINDOW_SIZE
    }

    fn min_size(self) -> (f32, f32) {
        plate::MIN_WINDOW_SIZE
    }
}

fn xy_size(window: &Window) -> (f32, f32) {
    let viewport = window.viewport_size();
    (f32::from(viewport.width), f32::from(viewport.height))
}

// ── The well ───────────────────────────────────────────────────────────────

/// The latency the insert runs at: the lookahead, plus the resampler's
/// delay when oversampling — what the DSP reports for compensation.
fn latency_ms(params: &Params, sample_rate: f32) -> f32 {
    let samples = burnlimit::latency_samples_for(params, sample_rate);
    samples as f32 / sample_rate.max(1.0) * 1_000.0
}

fn display(e: &KitEditor<BurnLimitPlate>, cx: &mut Cx<BurnLimitPlate>, s: f32) -> AnyElement {
    let latency = numbers(
        format!(
            "Latency {}",
            plate::fmt_ms(latency_ms(&e.params, e.sample_rate))
        ),
        12.0,
        SOFT,
        s,
    )
    .into_any_element();
    let swatch = |w: f32, h: f32, ink: Rgba| div().w(px(w * s)).h(px(h * s)).bg(ink);
    let graph = div()
        .absolute()
        .left(px(16.0 * s))
        .top(px(2.0 * s))
        .w(px(GRAPH_W * s))
        .h(px(GRAPH_H * s))
        .overflow_hidden()
        .child(live_slot(&e.live_bounds, SLOT_GRAPH).absolute().size_full())
        .child(
            div()
                .absolute()
                .left(px(10.0 * s))
                .bottom(px(8.0 * s))
                .child(plate::over_live(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(14.0 * s))
                        .child(legend(swatch(10.0, 10.0, color(0x3a3a3a)), "Input", s))
                        .child(legend(swatch(10.0, 10.0, color(0xd9d9d9)), "Output", s))
                        .child(legend(swatch(10.0, 2.0, accent()), "Gain reduction", s)),
                )),
        );
    let scale_label = |text: &str, top: f32| {
        div()
            .absolute()
            .left_0()
            .top(px(top * s))
            .child(numbers(text.to_string(), 10.0, FAINT, s))
    };
    let gr_scale = div()
        .absolute()
        .left(px(822.0 * s))
        .top(px(2.0 * s))
        .w(px(26.0 * s))
        .h(px(GRAPH_H * s))
        .child(scale_label("0", 0.0))
        .child(scale_label("\u{2212}3", 20.5))
        .child(scale_label("\u{2212}6", 48.0))
        .child(scale_label("\u{2212}9", 75.5))
        .child(scale_label("\u{2212}12", 103.0))
        .child(
            div()
                .absolute()
                .left_0()
                .top(px(122.0 * s))
                .child(numbers("GR", 9.0, GHOST, s)),
        );
    let meters = div()
        .absolute()
        .left(px(858.0 * s))
        .top(px(2.0 * s))
        .w(px(136.0 * s))
        .h(px(270.0 * s))
        .child(
            live_slot(&e.live_bounds, SLOT_METERS)
                .absolute()
                .size_full(),
        );

    well(s, 7.0, 327.0)
        .child(header(
            e,
            cx,
            s,
            "TRUE PEAK LIMITER",
            accent(),
            vec![latency],
        ))
        .child(
            div()
                .relative()
                .flex_1()
                .child(graph)
                .child(gr_scale)
                .child(meters),
        )
        .into_any_element()
}

/// The newest `GRAPH_POINTS` history points, oldest first.
fn recent(live: &Live) -> Vec<crate::components::plugin_live::HistoryPoint> {
    let count = live.history_len().min(GRAPH_POINTS);
    (0..count).rev().map(|age| live.history_at(age)).collect()
}

/// A level as the envelope's half-height.
fn amp(db: f32) -> f32 {
    (10f32.powf(db / 20.0) / 1.25).clamp(0.0, 1.0) * HALF_H
}

fn paint_graph(
    window: &mut Window,
    live: &Live,
    view: &BurnLive,
    b: gpui::Bounds<gpui::Pixels>,
    s: f32,
) {
    let at = plate::at(b, s);
    let grid = color(0x1d1d1d);
    for i in 1..10 {
        let x = i as f32 * 80.0;
        paint_polyline(window, &[at(x, 0.0), at(x, GRAPH_H)], 1.0, grid, None);
    }
    for i in 1..=4 {
        let y = i as f32 * 27.5;
        paint_polyline(window, &[at(0.0, y), at(GRAPH_W, y)], 1.0, grid, None);
    }
    paint_polyline(
        window,
        &[at(0.0, MID_Y), at(GRAPH_W, MID_Y)],
        1.0,
        color(0x2a2a2a),
        None,
    );

    let points = recent(live);
    if points.len() >= 2 {
        // Newest at the right edge, one step per telemetry frame.
        let step = GRAPH_W / (GRAPH_POINTS - 1) as f32;
        let x0 = GRAPH_W - step * (points.len() - 1) as f32;
        let envelope = |read: &dyn Fn(&crate::components::plugin_live::HistoryPoint) -> f32| {
            let mut shape: Vec<(f32, f32)> = points
                .iter()
                .enumerate()
                .map(|(k, p)| at(x0 + k as f32 * step, MID_Y - amp(read(p))))
                .collect();
            shape.extend(
                points
                    .iter()
                    .enumerate()
                    .rev()
                    .map(|(k, p)| at(x0 + k as f32 * step, MID_Y + amp(read(p)))),
            );
            shape
        };
        paint_polygon(window, &envelope(&|p| p.in_db), color_a(0xffffff, 0.13));
        paint_polygon(window, &envelope(&|p| p.out_db), color_a(0xd9d9d9, 0.75));

        let line: Vec<(f32, f32)> = points
            .iter()
            .enumerate()
            .map(|(k, p)| {
                at(
                    x0 + k as f32 * step,
                    (p.reduction_db * GR_PX_PER_DB).min(GRAPH_H),
                )
            })
            .collect();
        let mut area = line.clone();
        area.push(at(GRAPH_W, 0.0));
        area.push(at(x0, 0.0));
        paint_polygon(window, &area, color_a(ACCENT, 0.28));
        paint_polyline(window, &line, 1.6 * s, accent(), None);
    }

    if view.power {
        let cy = amp(view.ceiling_db);
        for y in [MID_Y - cy, MID_Y + cy] {
            paint_polyline(
                window,
                &[at(0.0, y), at(GRAPH_W, y)],
                1.0,
                color_a(ACCENT, 0.85),
                Some((4.0 * s, 3.0 * s)),
            );
        }
    }
}

fn to_db(level: f32) -> f32 {
    if level <= 1.0e-6 {
        -120.0
    } else {
        20.0 * level.log10()
    }
}

/// A bar's lit height for `db`: −48 dB to +6 dB.
fn bar_h(db: f32) -> f32 {
    ((db + 48.0) / 54.0).clamp(0.0, 1.0) * METER_H
}

fn paint_meters(
    window: &mut Window,
    cx: &mut App,
    live: &Live,
    b: gpui::Bounds<gpui::Pixels>,
    s: f32,
) {
    let at = plate::at(b, s);
    let pads = live.pads();
    let frame = live.frame;
    let pad = |slot: usize| pads.map(|block| block[slot]);
    let mono = plate::mono(FontWeight::NORMAL);
    let outfit = plate::outfit(FontWeight::MEDIUM);
    let label = |window: &mut Window, cx: &mut App, text: &str, x: f32| {
        paint_text(
            window,
            cx,
            &plate::tracked(text, 0.1),
            outfit.clone(),
            10.5 * s,
            color(SOFT),
            at(x, 240.0),
            Align::Center,
        );
    };
    let value = |window: &mut Window, cx: &mut App, text: &str, ink: Rgba, x: f32| {
        paint_text(
            window,
            cx,
            text,
            mono.clone(),
            11.0 * s,
            ink,
            at(x, 0.0),
            Align::Center,
        );
    };
    let bar = |window: &mut Window, x: f32, lit: Option<(f32, f32)>, ink: Rgba| {
        let (bx, by) = at(x, 18.0);
        window.paint_quad(
            gpui::fill(plate::rect(bx, by, 12.0 * s, METER_H * s), color(0x1c1c1c))
                .corner_radii(px(2.0 * s)),
        );
        if let Some((top, h)) = lit {
            let (lx, ly) = at(x, 18.0 + top);
            paint_rect(window, (lx, ly, 12.0 * s, h * s), ink);
        }
    };
    let up = |db: f32| {
        let h = bar_h(db);
        Some((METER_H - h, h))
    };

    // In and Out: the per-channel peaks from the telemetry block, or the
    // frame's both-channel peak where there is no block.
    let channel = |slot: usize, both: Option<f32>| pad(slot).or(both).map(to_db);
    let held_in = live.held(|p| p.in_db);
    let held_out = live.held(|p| p.out_db);
    let gr_now = pad(tm::GR_DB).or(frame.map(|f| f.gain_reduction_db));

    // IN: columns 0–27, GR 62–74, OUT 109–136.
    if frame.is_some() {
        let ink = if held_in > 0.0 { accent() } else { color(INK) };
        value(window, cx, &signed(held_in), ink, 13.5);
    }
    let in_l = channel(tm::IN_L, frame.map(|f| f.in_peak));
    let in_r = channel(tm::IN_R, frame.map(|f| f.in_peak));
    bar(window, 0.0, in_l.and_then(up), color(0xbdbdbd));
    bar(window, 15.0, in_r.and_then(up), color(0xbdbdbd));
    label(window, cx, "IN", 13.5);

    if let Some(gr) = gr_now {
        value(window, cx, &signed(-gr.max(0.0)), accent(), 68.0);
    }
    bar(
        window,
        62.0,
        gr_now.map(|gr| (0.0, (gr.max(0.0) / 24.0).clamp(0.0, 1.0) * METER_H)),
        accent(),
    );
    label(window, cx, "GR", 68.0);

    if frame.is_some() {
        value(window, cx, &signed(held_out), color(WHITE), 122.5);
    }
    let out_l = channel(tm::OUT_L, frame.map(|f| f.out_peak));
    let out_r = channel(tm::OUT_R, frame.map(|f| f.out_peak));
    bar(window, 109.0, out_l.and_then(up), color(INK));
    bar(window, 124.0, out_r.and_then(up), color(INK));
    label(window, cx, "OUT", 122.5);
}

// ── The deck ───────────────────────────────────────────────────────────────

fn controls(e: &KitEditor<BurnLimitPlate>, cx: &mut Cx<BurnLimitPlate>, s: f32) -> AnyElement {
    deck(s, 341.0, 420.0)
        .p(px(14.0 * s))
        .gap(px(12.0 * s))
        .child(limiter_card(e, cx, s).w(px(470.0 * s)).flex_shrink_0())
        .child(loudness_card(e, cx, s).w(px(280.0 * s)).flex_shrink_0())
        .child(output_card(e, cx, s).flex_1())
        .into_any_element()
}

fn set(id: &'static str) -> impl Fn(&mut KitEditor<BurnLimitPlate>, f32, &mut Cx<BurnLimitPlate>) {
    move |this, value, cx| this.set_value(id, value, cx)
}

fn limiter_card(e: &KitEditor<BurnLimitPlate>, cx: &mut Cx<BurnLimitPlate>, s: f32) -> gpui::Div {
    let style = e.params.style;
    let styles: Vec<AnyElement> = Style::UI_ORDER
        .iter()
        .map(|&option| {
            pill(
                ("burnlimit-style", option.to_wire() as usize),
                option.label(),
                option == style,
                accent(),
                PillLook::words(32.0, 0.0, 12.5),
                true,
                s,
                e.click_cb(cx, move |this, cx| {
                    this.set_value("style", option.to_wire(), cx)
                }),
            )
            .into_any_element()
        })
        .collect();

    let big = KnobCell::new(116.0, 11.0, 14.0).gap(3.0).label(0.1, SOFT);
    let small = KnobCell::new(70.0, 10.0, 11.5);
    let p = &e.params;
    let hero = KnobLook::hero(100.0, 3.2, 1.6);
    let little = KnobLook::small(46.0);
    let big_knobs = [
        (GAIN, p.gain_db, accent()),
        (CEILING, p.ceiling_db, color(INK)),
    ]
    .map(|(dial, value, ink)| {
        knob(
            cx,
            format!("burnlimit-{}", dial.id),
            dial,
            value,
            hero,
            big,
            ink,
            true,
            s,
            set(dial.id),
        )
    });
    let small_knobs: Vec<AnyElement> = [
        (RELEASE, p.release_ms),
        (LOOKAHEAD, p.lookahead_ms),
        (KNEE, p.knee_db),
        (LINK, p.link_pct),
    ]
    .into_iter()
    .map(|(dial, value)| {
        knob(
            cx,
            format!("burnlimit-{}", dial.id),
            dial,
            value,
            little,
            small,
            accent(),
            true,
            s,
            set(dial.id),
        )
    })
    .collect();
    let mut small_rows = small_knobs.into_iter();
    let mut grid = div().flex().flex_col().gap(px(8.0 * s));
    for _ in 0..2 {
        grid = grid.child(
            div()
                .flex()
                .flex_row()
                .gap(px(4.0 * s))
                .children(small_rows.next())
                .children(small_rows.next()),
        );
    }

    card(s)
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0 * s))
                .child(card_title("Limiter", s))
                .child(words(style.label(), 13.0, SOFT, s)),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(6.0 * s))
                .child(caption("Style", 10.5, 0.1, SOFT, s))
                .child(columns(4.0, s, styles)),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .children(big_knobs)
                .child(grid),
        )
}

fn loudness_card(e: &KitEditor<BurnLimitPlate>, cx: &mut Cx<BurnLimitPlate>, s: f32) -> gpui::Div {
    let readout = |index: usize, label: &str| {
        div()
            .flex()
            .flex_col()
            .gap(px(2.0 * s))
            .px(px(10.0 * s))
            .py(px(8.0 * s))
            .rounded(px(3.0 * s))
            .border(px(1.0))
            .border_color(color(0x3a3a3a))
            .bg(color(0x1f1f1f))
            .child(caption(label, 10.0, 0.08, SOFT, s))
            .child(
                div()
                    .h(px(20.0 * s))
                    .w_full()
                    .child(live_slot(&e.live_bounds, SLOT_READOUT + index).size_full()),
            )
            .into_any_element()
    };
    let labels = [
        "Short-term",
        "Momentary",
        "Range (LRA)",
        "True peak",
        "GR max",
        "PLR",
    ];
    let mut readouts = div().flex().flex_col().gap(px(10.0 * s));
    for row in 0..3 {
        readouts = readouts.child(columns(
            12.0,
            s,
            vec![
                readout(row * 2, labels[row * 2]),
                readout(row * 2 + 1, labels[row * 2 + 1]),
            ],
        ));
    }

    let target = e.params.target_lufs;
    let targets: Vec<AnyElement> = TARGETS
        .iter()
        .map(|&value| {
            pill(
                ("burnlimit-target", (-value) as usize),
                signed(value).replace(".0", ""),
                (target - value).abs() < 0.05,
                accent(),
                PillLook::numbers(24.0, 7.0, 11.0),
                true,
                s,
                e.click_cb(cx, move |this, cx| this.set_value("targetLufs", value, cx)),
            )
            .into_any_element()
        })
        .collect();

    // A click on the integrated reading starts the measurement over.
    let reset = div()
        .id("burnlimit-lufs")
        .w(px(150.0 * s))
        .h(px(44.0 * s))
        .cursor(gpui::CursorStyle::PointingHand)
        .child(live_slot(&e.live_bounds, SLOT_LUFS).size_full())
        .on_click(e.click_cb(cx, |this, cx| {
            let next = this.value("meterReset") + 1.0;
            this.set_value("meterReset", next, cx)
        }));

    card(s)
        .child(card_title("Loudness", s))
        .child(
            div()
                .flex()
                .flex_row()
                .items_end()
                .gap(px(8.0 * s))
                .child(reset)
                .child(words("LUFS integrated", 12.0, SOFT, s).pb(px(4.0 * s))),
        )
        .child(readouts)
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(6.0 * s))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(4.0 * s))
                        .child(caption("Target", 10.5, 0.1, SOFT, s).flex_1())
                        .children(targets),
                )
                .child(
                    div()
                        .h(px(10.0 * s))
                        .w_full()
                        .child(live_slot(&e.live_bounds, SLOT_TARGET).size_full()),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .justify_between()
                        .child(numbers("\u{2212}24", 10.0, SOFT, s))
                        .child(
                            div()
                                .flex_1()
                                .h(px(13.0 * s))
                                .child(live_slot(&e.live_bounds, SLOT_DELTA).size_full()),
                        )
                        .child(numbers("\u{2212}4", 10.0, SOFT, s)),
                ),
        )
}

fn output_card(e: &KitEditor<BurnLimitPlate>, cx: &mut Cx<BurnLimitPlate>, s: f32) -> gpui::Div {
    let row = |label: &'static str, control: AnyElement| {
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .child(words(label, 12.5, INK, s))
            .child(control)
    };
    let p = &e.params;
    let true_peak = switch(
        "burnlimit-truepeak",
        p.true_peak,
        accent(),
        true,
        s,
        e.click_cb(cx, |this, cx| this.toggle("truePeak", cx)),
    )
    .into_any_element();
    let unity = switch(
        "burnlimit-unity",
        p.unity,
        accent(),
        true,
        s,
        e.click_cb(cx, |this, cx| this.toggle("unity", cx)),
    )
    .into_any_element();
    let choice = |id: &'static str,
                  param: &'static str,
                  current: f32,
                  options: Vec<(f32, &'static str)>,
                  cx: &mut Cx<BurnLimitPlate>| {
        let pills: Vec<AnyElement> = options
            .into_iter()
            .map(|(wire, label)| {
                pill(
                    (id, wire as usize),
                    label,
                    (current - wire).abs() < 0.5,
                    accent(),
                    PillLook::numbers(26.0, 0.0, 11.5),
                    true,
                    s,
                    e.click_cb(cx, move |this, cx| this.set_value(param, wire, cx)),
                )
                .into_any_element()
            })
            .collect();
        columns(3.0, s, pills)
    };
    let oversampling = choice(
        "burnlimit-os",
        "oversampling",
        p.oversampling.to_wire(),
        OVERSAMPLING
            .iter()
            .map(|(option, label)| (option.to_wire(), *label))
            .collect(),
        cx,
    );
    let dither = choice(
        "burnlimit-dither",
        "dither",
        p.dither.to_wire(),
        DITHER
            .iter()
            .map(|(option, label)| (option.to_wire(), *label))
            .collect(),
        cx,
    );
    card(s)
        .child(card_title("Output", s))
        .child(row("True peak", true_peak))
        .child(row("Unity monitor", unity))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(6.0 * s))
                .child(caption("Oversampling", 10.5, 0.1, SOFT, s))
                .child(oversampling),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(6.0 * s))
                .child(caption("Dither", 10.5, 0.1, SOFT, s))
                .child(dither),
        )
}

/// The loudness card's live readings.
fn paint_loudness(
    window: &mut Window,
    cx: &mut App,
    live: &Live,
    view: &BurnLive,
    bounds: &LiveBounds,
    s: f32,
) {
    let Some(block) = live.pads() else {
        return;
    };
    let read = |slot: usize| {
        let v = block[slot];
        (v > -119.0).then_some(v)
    };
    let mono = plate::mono(FontWeight::NORMAL);
    let integrated = read(tm::INTEGRATED_LUFS).filter(|_| block[tm::VALID] >= 0.5);
    let short = read(tm::SHORT_LUFS);
    let momentary = read(tm::MOMENTARY_LUFS);
    let true_peak = read(tm::TRUE_PEAK_DBTP);
    let gr_max = block[tm::GR_MAX_DB].max(0.0);
    let dash = "\u{2014}".to_string();

    if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_LUFS)) {
        let (x, y) = plate::xy(b.origin);
        paint_text(
            window,
            cx,
            &integrated.map_or(dash.clone(), signed),
            plate::mono(FontWeight::MEDIUM),
            44.0 * s,
            color(WHITE),
            (x, y - 4.0 * s),
            Align::Left,
        );
    }
    let readouts: [(String, Rgba); 6] = [
        (short.map_or(dash.clone(), signed), color(WHITE)),
        (momentary.map_or(dash.clone(), signed), color(WHITE)),
        (
            format!("{:.1} LU", block[tm::LRA_LU].max(0.0)),
            color(WHITE),
        ),
        (
            true_peak.map_or(dash.clone(), |tp| format!("{} dBTP", signed(tp))),
            if true_peak.is_some_and(|tp| tp > -0.1) {
                accent()
            } else {
                color(WHITE)
            },
        ),
        (format!("{} dB", signed(-gr_max)), color(WHITE)),
        (
            match (true_peak, integrated) {
                (Some(tp), Some(i)) => format!("{:.1} dB", tp - i),
                _ => dash.clone(),
            },
            color(WHITE),
        ),
    ];
    for (index, (text, ink)) in readouts.iter().enumerate() {
        if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_READOUT + index)) {
            paint_text(
                window,
                cx,
                text,
                mono.clone(),
                15.0 * s,
                *ink,
                plate::xy(b.origin),
                Align::Left,
            );
        }
    }

    if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_TARGET)) {
        let (x, y) = plate::xy(b.origin);
        let (w, h) = (f32::from(b.size.width), f32::from(b.size.height));
        window.paint_quad(
            gpui::fill(plate::rect(x, y, w, h), color(0x1a1a1a)).corner_radii(px(2.0 * s)),
        );
        if let Some(short) = short {
            let fraction = ((short + 24.0) / 20.0).clamp(0.0, 1.0);
            paint_rect(window, (x, y, w * fraction, h), color(0x8c8c8c));
        }
        let marker = ((view.target_lufs + 24.0) / 20.0).clamp(0.0, 1.0);
        paint_rect(window, (x + w * marker - 1.0 * s, y, 2.0 * s, h), accent());
    }
    if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_DELTA)) {
        let text = match integrated {
            Some(i) if (i - view.target_lufs).abs() < 0.3 => "On target".to_string(),
            Some(i) => format!("{} LU vs target", signed(i - view.target_lufs)),
            None => String::new(),
        };
        let (x, y) = plate::xy(b.center());
        paint_text(
            window,
            cx,
            &text,
            mono,
            10.0 * s,
            color(SOFT),
            (x, y - 6.5 * s),
            Align::Center,
        );
    }
}

/// Opens with a track's BurnLimit insert.
pub fn open_burnlimit_editor(
    owner_bounds: Option<gpui::Bounds<gpui::Pixels>>,
    key: PluginInstanceKey,
    identity: ShellIdentity,
    host_ops: BuiltinEditorHostOps,
    on_close: Arc<dyn Fn(&mut Window, &mut App)>,
    cx: &mut App,
) -> Result<gpui::WindowHandle<KitWindow<BurnLimitPlate>>, String> {
    plate::ensure_fonts(cx);
    open_kit_editor(
        BurnLimitPlate,
        owner_bounds,
        key,
        identity,
        host_ops,
        on_close,
        cx,
    )
}

const _: () = assert!(tm::SLOTS == BUILTIN_PAD_SLOTS);

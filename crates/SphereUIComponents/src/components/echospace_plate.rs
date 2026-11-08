//! EchoSpace's editor, on the plate kit: the repeats a hit makes — left
//! above the line, right below, each as tall as it is loud, on the beat grid
//! while synced — and how the repeats darken pass by pass, in the well; Time,
//! Repeats, Colour and Output cards on the deck.
//!
//! Both displays are the DSP's own model (`echospace::echo_pattern`,
//! `echospace::tone_response_db`) at the host's rate and tempo, painted when
//! the panel renders; the In / Out meters ride the live overlay.

use std::sync::Arc;

use echospace::{DelayMode, Params};
use gpui::{div, px, AnyElement, App, Bounds, IntoElement, ParentElement, Pixels, Styled, Window};

use crate::components::builtin_plugin_editor::builtin_echospace_params;
use crate::components::builtin_plugin_editor_window::{BuiltinEditorHostOps, PluginInstanceKey};
use crate::components::fx_model::KnobSpec;
use crate::components::native_plugin_shell::ShellIdentity;
use crate::components::plate::{
    self, caption, card, card_title, color, color_a, columns, deck, header, knob_reading, legend,
    numbers, paint_polygon, paint_polyline, paint_rect, paint_text, pill, plate_frame, plate_scale,
    well, Align, Dial, DialUnit, KnobCell, KnobLook, PillLook, FACE, FACE_EDGE, FAINT, INK, SOFT,
    WHITE,
};
use crate::components::plugin_kit::{
    bank, open_kit_editor, Cx, KitEditor, KitModel, KitPreset, KitWindow, LiveBounds, LiveSlot,
};
use crate::components::plugin_live::Live;
use crate::components::time_plate::{
    at, at_right, display_box, fmt_time, headline, hz_at_x, io_meters, nice_ceiling, nice_step,
    paint_axis, paint_freq_grid, paint_meter, range_dial, GRID, NARROW_W, PLOT_BOTTOM, PLOT_TOP,
    WIDE_W,
};

/// EchoSpace's accent, and the right side's ink.
pub const ACCENT: u32 = 0x37c8a8;
const RIGHT: u32 = 0xc9c9c9;

const SLOT_METER: usize = 0;
/// The quietest repeat drawn.
const FLOOR_DB: f32 = -48.0;
const MAX_ECHOES: usize = 96;
/// The tone display's floor.
const TONE_FLOOR_DB: f32 = -36.0;
/// Passes the tone display draws, and how bright each is.
const TONE_PASSES: [(u32, f32); 4] = [(1, 1.0), (2, 0.7), (4, 0.45), (8, 0.25)];
const CURVE_POINTS: usize = 120;

fn accent() -> gpui::Rgba {
    color(ACCENT)
}

/// The dial over EchoSpace's descriptor row `id`.
fn dial(id: &'static str, label: &'static str, unit: DialUnit) -> Dial {
    let row = echospace::descriptor()
        .params
        .iter()
        .find(|row| row.id == id)
        .map(|row| (row.id, row.min, row.max, row.default_value))
        .unwrap_or_else(|| panic!("EchoSpace has no param {id}"));
    range_dial(row, label, unit)
}

struct Dials {
    time_l: Dial,
    time_r: Dial,
    note_l: Dial,
    note_r: Dial,
    feedback: Dial,
    cross: Dial,
    duck: Dial,
    diffusion: Dial,
    wow: Dial,
    rate: Dial,
    low_cut: Dial,
    high_cut: Dial,
    drive: Dial,
    width: Dial,
    mix: Dial,
    output: Dial,
}

fn dials() -> &'static Dials {
    static DIALS: std::sync::OnceLock<Dials> = std::sync::OnceLock::new();
    DIALS.get_or_init(|| Dials {
        time_l: dial("timeMsL", "TIME L", DialUnit::Ms).log(),
        time_r: dial("timeMsR", "TIME R", DialUnit::Ms).log(),
        note_l: dial("divisionL", "NOTE L", DialUnit::Plain),
        note_r: dial("divisionR", "NOTE R", DialUnit::Plain),
        feedback: dial("feedback", "FEEDBACK", DialUnit::Percent),
        cross: dial("crossFeedback", "CROSS", DialUnit::Percent),
        duck: dial("duck", "DUCK", DialUnit::Percent),
        diffusion: dial("diffusion", "DIFFUSION", DialUnit::Percent),
        wow: dial("modDepth", "WOW", DialUnit::Percent),
        rate: dial("modRateHz", "RATE", DialUnit::Q).log(),
        low_cut: dial("lowCutHz", "LOW CUT", DialUnit::HzRound).log(),
        high_cut: dial("highCutHz", "HIGH CUT", DialUnit::Hz).log(),
        drive: dial("saturation", "DRIVE", DialUnit::Percent),
        width: dial("width", "WIDTH", DialUnit::Percent).bipolar(),
        mix: dial("mix", "MIX", DialUnit::Percent),
        output: dial("outputDb", "OUTPUT", DialUnit::Db),
    })
}

/// A division's label, `1/8.`.
fn division_label(division: u8) -> &'static str {
    echospace::DIVISION_LABELS
        .get(usize::from(division).min(echospace::DIVISION_COUNT - 1))
        .copied()
        .unwrap_or("\u{2014}")
}

/// The repeat display's span: past the last repeat drawn.
fn repeat_span(echoes: &[echospace::Echo], times_ms: (f32, f32)) -> f32 {
    let last = echoes.iter().map(|e| e.at_ms).fold(0.0, f32::max);
    let floor = times_ms.0.max(times_ms.1) * 2.0;
    nice_ceiling((last.max(floor) * 1.06).clamp(100.0, 30_000.0))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EchoSpacePlate;

/// An EchoSpace insert's editor window.
pub type EchoSpaceWindow = KitWindow<EchoSpacePlate>;

impl KitModel for EchoSpacePlate {
    type Params = Params;
    type Ui = ();
    type LiveView = ();

    fn title(self) -> &'static str {
        "EchoSpace"
    }

    fn subtitle(self, _params: &Params) -> String {
        "STEREO DELAY".into()
    }

    fn key(self) -> &'static str {
        "echospace-plate"
    }

    fn defaults(self) -> Params {
        echospace::default_params()
    }

    fn from_mirror(self, insert_id: &str) -> Option<Params> {
        builtin_echospace_params(insert_id)
    }

    /// Every param as a wire edit, Link first: while it is on the DSP mirrors
    /// one side's edit onto the other, so turning it off has to arrive before
    /// two sides that differ.
    fn wire_values(self, params: &Params) -> Vec<(u32, f32)> {
        let mut values: Vec<(u32, f32)> = echospace::ipc::ui_values(params)
            .into_iter()
            .filter_map(|(id, value)| echospace::ui_param_index(id).map(|index| (index, value)))
            .collect();
        values.sort_by_key(|(index, _)| *index != echospace::ipc::LINK_INDEX);
        values
    }

    fn with(self, params: &Params, id: &str, value: f32) -> Params {
        let mut next = params.clone();
        echospace::ipc::apply_ui_param(&mut next, id, value);
        next
    }

    fn value(self, params: &Params, id: &str) -> f32 {
        echospace::ipc::ui_values(params)
            .into_iter()
            .find(|(candidate, _)| *candidate == id)
            .map_or(0.0, |(_, value)| value)
    }

    fn presets(self) -> Arc<Vec<KitPreset<Params>>> {
        static BANK: std::sync::OnceLock<Arc<Vec<KitPreset<Params>>>> = std::sync::OnceLock::new();
        BANK.get_or_init(|| bank(echospace::factory_presets(), |p| (p.name, p.params)))
            .clone()
    }

    /// A preset carries no listening state: power and freeze stay.
    fn preset_applied(self, current: &Params, preset: &Params) -> Params {
        let mut next = preset.clone();
        next.power = current.power;
        next.freeze = current.freeze;
        next
    }

    fn knob(self, _id: &str) -> Option<KnobSpec> {
        None
    }

    fn panel(e: &KitEditor<Self>, cx: &mut Cx<Self>) -> AnyElement {
        let s = plate_scale(e.viewport);
        plate_frame(s, vec![display(e, cx, s), controls(e, cx, s)])
    }

    fn live_view(_e: &KitEditor<Self>) {}

    fn paint_live(
        _view: &(),
        live: &mut Live,
        bounds: &LiveBounds,
        window: &mut Window,
        cx: &mut App,
    ) {
        let viewport = window.viewport_size();
        let s = plate_scale((f32::from(viewport.width), f32::from(viewport.height)));
        if let Some(frame) = live.frame {
            for (index, peak) in [frame.in_peak, frame.out_peak].into_iter().enumerate() {
                if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_METER + index)) {
                    paint_meter(window, cx, b, 20.0 * peak.max(1.0e-9).log10(), accent(), s);
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

fn toggle(
    e: &KitEditor<EchoSpacePlate>,
    cx: &mut Cx<EchoSpacePlate>,
    id: &'static str,
    label: &'static str,
    param: &'static str,
    enabled: bool,
    s: f32,
) -> AnyElement {
    pill(
        id,
        label,
        e.flag(param),
        accent(),
        PillLook::words(28.0, 12.0, 12.0),
        enabled,
        s,
        e.click_cb(cx, move |this, cx| this.toggle(param, cx)),
    )
    .into_any_element()
}

fn display(e: &KitEditor<EchoSpacePlate>, cx: &mut Cx<EchoSpacePlate>, s: f32) -> AnyElement {
    let p = &e.params;
    let (sample_rate, tempo) = (e.sample_rate, e.tempo_bpm);
    let dim = if p.power { 1.0 } else { 0.4 };
    let mono = p.mode == DelayMode::Mono;
    let echoes = echospace::echo_pattern(p, tempo, sample_rate, FLOOR_DB, MAX_ECHOES);
    let times = (p.effective_time_ms_l(tempo), p.effective_time_ms_r(tempo));
    let span = repeat_span(&echoes, times);
    let beat_ms = p.sync.then(|| 60_000.0 / tempo.max(1.0));

    let times_line = if mono {
        fmt_time(times.0)
    } else {
        format!("{} \u{00B7} {}", fmt_time(times.0), fmt_time(times.1))
    };
    let times_caption = match (p.sync, mono) {
        (true, true) => format!("Synced {}", division_label(p.division_l)),
        (true, false) => format!(
            "Synced {} \u{00B7} {}",
            division_label(p.division_l),
            division_label(p.division_r)
        ),
        (false, _) => "Time".to_string(),
    };
    let swatch = |ink: u32| div().w(px(12.0 * s)).h(px(2.0 * s)).bg(color(ink));
    let sides = div()
        .flex()
        .flex_row()
        .gap(px(14.0 * s))
        .child(legend(
            swatch(ACCENT),
            if mono { "Both" } else { "Left" },
            s,
        ))
        .children((!mono).then(|| legend(swatch(RIGHT), "Right", s)));
    let repeats_box = display_box(s, 16.0, WIDE_W, move |b, k, window, cx| {
        paint_repeats(window, cx, b, k, &echoes, span, beat_ms, mono, dim)
    })
    .child(at(s, 10.0, 8.0, caption("Repeats", 10.5, 0.1, FAINT, s)))
    .child(at(s, 84.0, 9.0, sides))
    .child(at_right(
        s,
        10.0,
        4.0,
        headline(&times_caption, times_line, WHITE, s),
    ));

    let once: Vec<f32> = (0..CURVE_POINTS)
        .map(|i| {
            let hz = hz_at_x(i as f32 / (CURVE_POINTS - 1) as f32 * NARROW_W, NARROW_W);
            echospace::tone_response_db(p, hz, sample_rate)
        })
        .collect();
    let tone_box = display_box(s, 630.0, NARROW_W, move |b, k, window, cx| {
        paint_tone(window, cx, b, k, &once, dim)
    })
    .child(at(
        s,
        10.0,
        8.0,
        caption("Tone per pass", 10.5, 0.1, FAINT, s),
    ))
    .child(at_right(
        s,
        10.0,
        8.0,
        numbers("1 \u{00B7} 2 \u{00B7} 4 \u{00B7} 8", 11.0, SOFT, s),
    ));

    let toggles = vec![
        toggle(e, cx, "echo-sync", "Sync", "sync", true, s),
        toggle(e, cx, "echo-link", "Link L/R", "link", !mono, s),
        toggle(e, cx, "echo-freeze", "Freeze", "freeze", true, s),
    ];
    well(s, 7.0, 327.0)
        .child(header(e, cx, s, "Stereo delay", accent(), toggles))
        .child(div().relative().flex_1().child(repeats_box).child(tone_box))
        .into_any_element()
}

/// The repeats: left above the centre line, right below, each as tall as it
/// is loud; the dry hit at zero and the beat grid behind them while synced.
#[allow(clippy::too_many_arguments)]
fn paint_repeats(
    window: &mut Window,
    cx: &mut App,
    b: Bounds<Pixels>,
    k: f32,
    echoes: &[echospace::Echo],
    span: f32,
    beat_ms: Option<f32>,
    mono: bool,
    dim: f32,
) {
    let (x0, y0) = plate::xy(b.origin);
    let at = |x: f32, y: f32| (x0 + x * k, y0 + y * k);
    // A little room before zero, so the dry hit shows.
    let lead = 12.0;
    let x_t = |ms: f32| lead + (ms / span).clamp(0.0, 1.0) * (WIDE_W - lead - 8.0);
    let centre = (PLOT_TOP + PLOT_BOTTOM) * 0.5;
    let half = (PLOT_BOTTOM - PLOT_TOP) * 0.5 - 4.0;
    let height = |gain: f32| {
        let db = 20.0 * gain.abs().max(1.0e-6).log10();
        (1.0 - db / FLOOR_DB).clamp(0.0, 1.0) * half
    };

    let mut labels = vec![(x_t(0.0), "0".to_string())];
    match beat_ms {
        Some(beat) if span / beat <= 64.0 => {
            let mut index = 1;
            let mut t = beat;
            while t < span {
                let x = x_t(t);
                let bar = index % 4 == 0;
                paint_polyline(
                    window,
                    &[at(x, PLOT_TOP), at(x, PLOT_BOTTOM)],
                    1.0,
                    color(if bar { 0x2c2c2c } else { GRID }),
                    None,
                );
                t += beat;
                index += 1;
            }
        }
        _ => {}
    }
    let step = nice_step(span, 6);
    let mut t = step;
    while t < span * 0.999 {
        let x = x_t(t);
        if beat_ms.is_none() {
            paint_polyline(
                window,
                &[at(x, PLOT_TOP), at(x, PLOT_BOTTOM)],
                1.0,
                color(GRID),
                None,
            );
        }
        labels.push((x, fmt_time(t)));
        t += step;
    }
    paint_axis(window, cx, b, k, &labels);
    paint_polyline(
        window,
        &[at(0.0, centre), at(WIDE_W, centre)],
        1.0,
        color(0x3a3a3a),
        None,
    );

    // The dry hit.
    let (dx, dy) = at(x_t(0.0) - 1.0, centre - half);
    paint_rect(
        window,
        (dx, dy, 2.0 * k, 2.0 * half * k),
        color_a(WHITE, 0.5),
    );

    for echo in echoes {
        let x = x_t(echo.at_ms);
        let h = height(echo.gain);
        let bar_w = 3.0;
        let fade = (1.0 - (echo.pass.saturating_sub(1)) as f32 * 0.04).clamp(0.45, 1.0);
        if mono {
            let (bx, by) = at(x - bar_w * 0.5, centre - h);
            paint_rect(
                window,
                (bx, by, bar_w * k, 2.0 * h * k),
                color_a(ACCENT, fade * dim),
            );
        } else if echo.right {
            let (bx, by) = at(x - bar_w * 0.5, centre);
            paint_rect(
                window,
                (bx, by, bar_w * k, h * k),
                color_a(RIGHT, 0.85 * fade * dim),
            );
        } else {
            let (bx, by) = at(x - bar_w * 0.5, centre - h);
            paint_rect(
                window,
                (bx, by, bar_w * k, h * k),
                color_a(ACCENT, fade * dim),
            );
        }
    }

    let side = |text: &str, y: f32, window: &mut Window, cx: &mut App| {
        let (tx, ty) = at(WIDE_W - 14.0, y);
        paint_text(
            window,
            cx,
            text,
            plate::mono(gpui::FontWeight::NORMAL),
            10.0 * k,
            color(0x6f6f6f),
            (tx, ty),
            Align::Right,
        );
    };
    if !mono {
        side("L", PLOT_TOP + 2.0, window, cx);
        side("R", PLOT_BOTTOM - 16.0, window, cx);
    }
}

/// The repeats' tone after 1, 2, 4 and 8 passes through the loop's cuts.
fn paint_tone(
    window: &mut Window,
    cx: &mut App,
    b: Bounds<Pixels>,
    k: f32,
    once: &[f32],
    dim: f32,
) {
    let (x0, y0) = plate::xy(b.origin);
    let at = |x: f32, y: f32| (x0 + x * k, y0 + y * k);
    paint_freq_grid(window, cx, b, k, NARROW_W);
    let y_db = |db: f32| {
        PLOT_TOP + 6.0 + (db / TONE_FLOOR_DB).clamp(0.0, 1.0) * (PLOT_BOTTOM - PLOT_TOP - 6.0)
    };
    for db in [-12.0, -24.0] {
        let y = y_db(db);
        paint_polyline(
            window,
            &[at(0.0, y), at(NARROW_W, y)],
            1.0,
            color(GRID),
            None,
        );
    }
    let column = NARROW_W / (CURVE_POINTS - 1) as f32;
    for (passes, alpha) in TONE_PASSES.iter().rev() {
        let points: Vec<(f32, f32)> = once
            .iter()
            .enumerate()
            .map(|(i, db)| at(i as f32 * column, y_db(db * *passes as f32)))
            .collect();
        if *passes == 1 {
            let mut area = points.clone();
            area.push(at(NARROW_W, PLOT_BOTTOM));
            area.push(at(0.0, PLOT_BOTTOM));
            paint_polygon(window, &area, color_a(ACCENT, 0.12 * dim));
        }
        let width = if *passes == 1 { 2.0 } else { 1.3 };
        paint_polyline(
            window,
            &points,
            width * k,
            color_a(ACCENT, alpha * dim),
            None,
        );
    }
    let (tx, ty) = at(6.0, y_db(-12.0) - 14.0);
    paint_text(
        window,
        cx,
        "\u{2212}12",
        plate::mono(gpui::FontWeight::NORMAL),
        10.0 * k,
        color(0x6f6f6f),
        (tx, ty),
        Align::Left,
    );
}

// ── The deck ───────────────────────────────────────────────────────────────

fn controls(e: &KitEditor<EchoSpacePlate>, cx: &mut Cx<EchoSpacePlate>, s: f32) -> AnyElement {
    deck(s, 341.0, 420.0)
        .p(px(14.0 * s))
        .gap(px(12.0 * s))
        .child(time_card(e, cx, s).w(px(280.0 * s)).flex_shrink_0())
        .child(repeats_card(e, cx, s).w(px(242.0 * s)).flex_shrink_0())
        .child(colour_card(e, cx, s).w(px(164.0 * s)).flex_shrink_0())
        .child(output_card(e, cx, s).flex_1())
        .into_any_element()
}

#[allow(clippy::too_many_arguments)]
fn cell(
    cx: &mut Cx<EchoSpacePlate>,
    dial: Dial,
    value: f32,
    readout: Option<String>,
    look: KnobLook,
    cell: KnobCell,
    ink: gpui::Rgba,
    enabled: bool,
    s: f32,
) -> AnyElement {
    let id = dial.id;
    // A note division takes whole steps.
    let whole = matches!(id, "divisionL" | "divisionR");
    knob_reading(
        cx,
        format!("echo-{id}"),
        dial,
        value,
        readout,
        look,
        cell,
        ink,
        enabled,
        s,
        move |this: &mut KitEditor<EchoSpacePlate>, value, cx| {
            let value = if whole { value.round() } else { value };
            this.set_value(id, value, cx)
        },
    )
}

fn row(cells: Vec<AnyElement>) -> gpui::Div {
    div().flex().flex_row().justify_between().children(cells)
}

fn section(label: &str, s: f32) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap(px(8.0 * s))
        .child(caption(label, 10.5, 0.1, SOFT, s))
}

fn note(text: String, s: f32) -> gpui::Div {
    div()
        .text_size(plate::type_px(11.0, s))
        .line_height(px(14.0 * s))
        .text_color(color(SOFT))
        .child(text)
}

fn time_card(e: &KitEditor<EchoSpacePlate>, cx: &mut Cx<EchoSpacePlate>, s: f32) -> gpui::Div {
    let p = &e.params;
    let d = dials();
    let modes: Vec<AnyElement> = DelayMode::ALL
        .into_iter()
        .enumerate()
        .map(|(index, mode)| {
            pill(
                ("echo-mode", index),
                mode.label(),
                p.mode == mode,
                accent(),
                PillLook::words(28.0, 0.0, 12.0),
                true,
                s,
                e.click_cb(cx, move |this, cx| {
                    this.set_value("mode", mode.to_wire(), cx)
                }),
            )
            .into_any_element()
        })
        .collect();
    let look = KnobLook::small(64.0).styled(3.6, 17.5, FACE, FACE_EDGE, 0.8, 7.0, 1.8);
    let k = KnobCell::new(80.0, 10.5, 12.5);
    let right = p.mode.uses_right_time();
    let tempo = e.tempo_bpm;
    let (left_cell, right_cell) = if p.sync {
        (
            cell(
                cx,
                d.note_l,
                f32::from(p.division_l),
                Some(division_label(p.division_l).to_string()),
                look,
                k,
                accent(),
                true,
                s,
            ),
            cell(
                cx,
                d.note_r,
                f32::from(p.division_r),
                Some(division_label(p.division_r).to_string()),
                look,
                k,
                color(RIGHT),
                right,
                s,
            ),
        )
    } else {
        (
            cell(
                cx,
                d.time_l,
                p.time_ms_l,
                Some(fmt_time(p.time_ms_l)),
                look,
                k,
                accent(),
                true,
                s,
            ),
            cell(
                cx,
                d.time_r,
                p.time_ms_r,
                Some(fmt_time(p.time_ms_r)),
                look,
                k,
                color(RIGHT),
                right,
                s,
            ),
        )
    };
    let help = match (p.sync, p.mode) {
        (true, _) => format!("Synced to the host at {tempo:.1} BPM."),
        (false, DelayMode::PingPong) => {
            "The input starts on the left and bounces: left, then right.".to_string()
        }
        (false, DelayMode::Mono) => "One line at the left time, on both sides.".to_string(),
        (false, DelayMode::Stereo) => "Each side repeats on its own.".to_string(),
    };
    card(s)
        .child(card_title("Time", s))
        .child(
            section("Mode", s)
                .child(columns(3.0, s, modes))
                .child(note(help, s)),
        )
        .child(row(vec![
            left_cell,
            right_cell,
            cell(cx, d.feedback, p.feedback, None, look, k, accent(), true, s),
        ]))
}

fn repeats_card(e: &KitEditor<EchoSpacePlate>, cx: &mut Cx<EchoSpacePlate>, s: f32) -> gpui::Div {
    let p = &e.params;
    let d = dials();
    let look = KnobLook::small(48.0);
    let k = KnobCell::new(70.0, 9.5, 11.5);
    card(s)
        .child(card_title("Repeats", s))
        .child(section("Loop", s).child(row(vec![
            cell(
                cx,
                d.cross,
                p.cross_feedback,
                None,
                look,
                k,
                accent(),
                p.mode.uses_cross(),
                s,
            ),
            cell(cx, d.duck, p.duck, None, look, k, accent(), true, s),
            cell(
                cx,
                d.diffusion,
                p.diffusion,
                None,
                look,
                k,
                accent(),
                true,
                s,
            ),
        ])))
        .child(
            section("Wow & flutter", s).child(
                div()
                    .flex()
                    .flex_row()
                    .justify_around()
                    .child(cell(
                        cx,
                        d.wow,
                        p.mod_depth,
                        None,
                        look,
                        k,
                        accent(),
                        true,
                        s,
                    ))
                    .child(cell(
                        cx,
                        d.rate,
                        p.mod_rate_hz,
                        Some(format!("{:.2} Hz", p.mod_rate_hz)),
                        look,
                        k,
                        accent(),
                        true,
                        s,
                    )),
            ),
        )
}

fn colour_card(e: &KitEditor<EchoSpacePlate>, cx: &mut Cx<EchoSpacePlate>, s: f32) -> gpui::Div {
    let p = &e.params;
    let d = dials();
    let look = KnobLook::small(48.0);
    let k = KnobCell::new(64.0, 9.5, 11.5);
    card(s)
        .child(card_title("Colour", s))
        .child(section("Loop filter", s).child(row(vec![
            cell(
                cx,
                d.low_cut,
                p.low_cut_hz,
                None,
                look,
                k,
                color(INK),
                true,
                s,
            ),
            cell(
                cx,
                d.high_cut,
                p.high_cut_hz,
                None,
                look,
                k,
                color(INK),
                true,
                s,
            ),
        ])))
        .child(
            section("Tape", s).child(div().flex().flex_row().justify_center().child(cell(
                cx,
                d.drive,
                p.saturation,
                None,
                look,
                k,
                accent(),
                true,
                s,
            ))),
        )
}

fn output_card(e: &KitEditor<EchoSpacePlate>, cx: &mut Cx<EchoSpacePlate>, s: f32) -> gpui::Div {
    let p = &e.params;
    let d = dials();
    let look = KnobLook::small(52.0).styled(4.5, 16.0, FACE, FACE_EDGE, 1.2, 7.0, 2.4);
    let k = KnobCell::new(72.0, 10.0, 12.0);
    card(s)
        .child(card_title("Output", s))
        .child(row(vec![
            cell(cx, d.width, p.width, None, look, k, color(INK), true, s),
            cell(cx, d.mix, p.mix, None, look, k, color(INK), true, s),
            cell(
                cx,
                d.output,
                p.output_db,
                None,
                look,
                k,
                color(INK),
                true,
                s,
            ),
        ]))
        .child(note(
            "Equal-power balance between the dry and the repeats.".to_string(),
            s,
        ))
        .child(io_meters(&e.live_bounds, SLOT_METER, s))
}

/// Opens with a track's EchoSpace insert.
pub fn open_echospace_editor(
    owner_bounds: Option<gpui::Bounds<gpui::Pixels>>,
    key: PluginInstanceKey,
    identity: ShellIdentity,
    host_ops: BuiltinEditorHostOps,
    on_close: Arc<dyn Fn(&mut Window, &mut App)>,
    cx: &mut App,
) -> Result<gpui::WindowHandle<EchoSpaceWindow>, String> {
    plate::ensure_fonts(cx);
    open_kit_editor(
        EchoSpacePlate,
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
            d.time_l,
            d.time_r,
            d.note_l,
            d.note_r,
            d.feedback,
            d.cross,
            d.duck,
            d.diffusion,
            d.wow,
            d.rate,
            d.low_cut,
            d.high_cut,
            d.drive,
            d.width,
            d.mix,
            d.output,
        ] {
            let (min, max) = echospace::descriptor()
                .params
                .iter()
                .find(|row| row.id == dial.id)
                .map(|row| (row.min, row.max))
                .unwrap();
            assert_eq!((dial.min, dial.max), (min, max), "{}", dial.id);
        }
    }

    #[test]
    fn link_leads_the_wire_edits() {
        let values = EchoSpacePlate.wire_values(&echospace::default_params());
        assert_eq!(values[0].0, echospace::ipc::LINK_INDEX);
    }

    #[test]
    fn divisions_read_as_notes() {
        assert_eq!(division_label(9), "1/8.");
        assert_eq!(division_label(200), "1/1.");
    }

    #[test]
    fn the_repeat_axis_covers_the_repeats() {
        let params = echospace::default_params();
        let echoes = echospace::echo_pattern(&params, 120.0, 48_000.0, FLOOR_DB, MAX_ECHOES);
        let times = (params.time_ms_l, params.time_ms_r);
        let span = repeat_span(&echoes, times);
        assert!(echoes.iter().all(|e| e.at_ms <= span));
    }
}

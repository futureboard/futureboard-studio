//! VerbSpace's editor, on the plate kit: the reverb's level over time — the
//! pre-delay, the early reflections and each band's tail — and its decay
//! across frequency with the wet cuts, in the well; Space, Character,
//! Movement and Output cards on the deck.
//!
//! Both displays are the DSP's own model (`verbspace::decay_profile`,
//! `verbspace::wet_filter_response_db`) at the host's rate, painted when the
//! panel renders; the In / Out meters ride the live overlay.

use std::sync::Arc;

use gpui::{div, px, AnyElement, App, Bounds, IntoElement, ParentElement, Pixels, Styled, Window};
use verbspace::{Params, ReverbMode};

use crate::components::builtin_plugin_editor::builtin_verbspace_params;
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
    at, at_right, display_box, fmt_seconds, fmt_time, headline, hz_at_x, io_meters, nice_ceiling,
    nice_step, paint_axis, paint_freq_grid, paint_meter, range_dial, x_of_hz, GRID, NARROW_W,
    PLOT_BOTTOM, PLOT_TOP, WIDE_W,
};

/// VerbSpace's accent, and the low and high bands' inks.
pub const ACCENT: u32 = 0x9d86ff;
const LOW: u32 = 0x5a8fd6;
const HIGH: u32 = 0xe0b25a;

const SLOT_METER: usize = 0;
/// The decay display's floor.
const FLOOR_DB: f32 = -60.0;
/// Points across the decay-by-frequency curve.
const CURVE_POINTS: usize = 120;

fn accent() -> gpui::Rgba {
    color(ACCENT)
}

/// The dial over VerbSpace's descriptor row `id`.
fn dial(id: &'static str, label: &'static str, unit: DialUnit) -> Dial {
    let row = verbspace::descriptor()
        .params
        .iter()
        .find(|row| row.id == id)
        .map(|row| (row.id, row.min, row.max, row.default_value))
        .unwrap_or_else(|| panic!("VerbSpace has no param {id}"));
    range_dial(row, label, unit)
}

struct Dials {
    predelay: Dial,
    size: Dial,
    decay: Dial,
    bass: Dial,
    bass_freq: Dial,
    damping: Dial,
    damp_freq: Dial,
    diffusion: Dial,
    early_late: Dial,
    low_cut: Dial,
    high_cut: Dial,
    depth: Dial,
    rate: Dial,
    width: Dial,
    mix: Dial,
    output: Dial,
}

fn dials() -> &'static Dials {
    static DIALS: std::sync::OnceLock<Dials> = std::sync::OnceLock::new();
    DIALS.get_or_init(|| Dials {
        predelay: dial("predelayMs", "PRE-DELAY", DialUnit::Ms),
        size: dial("size", "SIZE", DialUnit::Percent),
        decay: dial("decaySec", "DECAY", DialUnit::Seconds).log(),
        bass: dial("bassMult", "BASS", DialUnit::Q).log(),
        bass_freq: dial("bassFreqHz", "BASS FREQ", DialUnit::HzRound).log(),
        damping: dial("damping", "DAMPING", DialUnit::Percent),
        damp_freq: dial("dampFreqHz", "DAMP FREQ", DialUnit::Hz).log(),
        diffusion: dial("diffusion", "DIFFUSION", DialUnit::Percent),
        early_late: dial("earlyLate", "EARLY/LATE", DialUnit::Percent),
        low_cut: dial("lowCutHz", "LOW CUT", DialUnit::HzRound).log(),
        high_cut: dial("highCutHz", "HIGH CUT", DialUnit::Hz).log(),
        depth: dial("modDepth", "DEPTH", DialUnit::Percent),
        rate: dial("modRateHz", "RATE", DialUnit::Q).log(),
        width: dial("width", "WIDTH", DialUnit::Percent).bipolar(),
        mix: dial("mix", "MIX", DialUnit::Percent),
        output: dial("outputDb", "OUTPUT", DialUnit::Db),
    })
}

/// The decay display's time span: past the slowest band's tail, rounded to
/// a tidy figure.
fn decay_span(profile: &verbspace::DecayProfile) -> f32 {
    let tail = if profile.frozen {
        4.0
    } else {
        profile.longest_sec()
    };
    nice_ceiling((profile.predelay_ms + tail * 1_000.0 * 1.08).clamp(200.0, 45_000.0))
}

/// `early 40 · late 60`: how the Early/Late balance reads.
fn fmt_early_late(value: f32) -> String {
    let late = value.round().clamp(0.0, 100.0);
    format!("{:.0}/{late:.0}", 100.0 - late)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VerbSpacePlate;

/// A VerbSpace insert's editor window.
pub type VerbSpaceWindow = KitWindow<VerbSpacePlate>;

impl KitModel for VerbSpacePlate {
    type Params = Params;
    type Ui = ();
    type LiveView = ();

    fn title(self) -> &'static str {
        "VerbSpace"
    }

    fn subtitle(self, _params: &Params) -> String {
        "ALGORITHMIC REVERB".into()
    }

    fn key(self) -> &'static str {
        "verbspace-plate"
    }

    fn defaults(self) -> Params {
        verbspace::default_params()
    }

    fn from_mirror(self, insert_id: &str) -> Option<Params> {
        builtin_verbspace_params(insert_id)
    }

    fn wire_values(self, params: &Params) -> Vec<(u32, f32)> {
        verbspace::ipc::ui_values(params)
            .into_iter()
            .filter_map(|(id, value)| verbspace::ui_param_index(id).map(|index| (index, value)))
            .collect()
    }

    fn with(self, params: &Params, id: &str, value: f32) -> Params {
        let mut next = params.clone();
        verbspace::ipc::apply_ui_param(&mut next, id, value);
        next
    }

    fn value(self, params: &Params, id: &str) -> f32 {
        verbspace::ipc::ui_values(params)
            .into_iter()
            .find(|(candidate, _)| *candidate == id)
            .map_or(0.0, |(_, value)| value)
    }

    fn presets(self) -> Arc<Vec<KitPreset<Params>>> {
        static BANK: std::sync::OnceLock<Arc<Vec<KitPreset<Params>>>> = std::sync::OnceLock::new();
        BANK.get_or_init(|| bank(verbspace::factory_presets(), |p| (p.name, p.params)))
            .clone()
    }

    /// A preset carries no listening state: power, freeze and the Wet Only
    /// routing (a send bus) stay as they are.
    fn preset_applied(self, current: &Params, preset: &Params) -> Params {
        let mut next = preset.clone();
        next.power = current.power;
        next.freeze = current.freeze;
        next.wet_only = current.wet_only;
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
    e: &KitEditor<VerbSpacePlate>,
    cx: &mut Cx<VerbSpacePlate>,
    id: &'static str,
    label: &'static str,
    param: &'static str,
    s: f32,
) -> AnyElement {
    pill(
        id,
        label,
        e.flag(param),
        accent(),
        PillLook::words(28.0, 12.0, 12.0),
        true,
        s,
        e.click_cb(cx, move |this, cx| this.toggle(param, cx)),
    )
    .into_any_element()
}

fn display(e: &KitEditor<VerbSpacePlate>, cx: &mut Cx<VerbSpacePlate>, s: f32) -> AnyElement {
    let p = &e.params;
    let profile = verbspace::decay_profile(p, e.sample_rate);
    let span = decay_span(&profile);
    let dim = if p.power { 1.0 } else { 0.4 };

    let swatch = |ink: u32| div().w(px(12.0 * s)).h(px(2.0 * s)).bg(color(ink));
    let bands = div()
        .flex()
        .flex_row()
        .gap(px(14.0 * s))
        .child(legend(swatch(LOW), "Low", s))
        .child(legend(swatch(ACCENT), "Mid", s))
        .child(legend(swatch(HIGH), "High", s))
        .child(legend(swatch(0xd9d9d9), "Early", s));
    let decay_box = display_box(s, 16.0, WIDE_W, move |b, k, window, cx| {
        paint_decay(window, cx, b, k, &profile, span, dim)
    })
    .child(at(s, 10.0, 8.0, caption("Decay", 10.5, 0.1, FAINT, s)))
    .child(at(s, 72.0, 9.0, bands))
    .child(at_right(
        s,
        10.0,
        4.0,
        headline(
            if profile.frozen { "Frozen" } else { "RT60" },
            fmt_seconds(profile.rt_mid_sec),
            WHITE,
            s,
        ),
    ));

    let sample_rate = e.sample_rate;
    let rt: Vec<f32> = (0..CURVE_POINTS)
        .map(|i| {
            profile.rt_at(hz_at_x(
                i as f32 / (CURVE_POINTS - 1) as f32 * NARROW_W,
                NARROW_W,
            ))
        })
        .collect();
    let cuts: Vec<f32> = (0..CURVE_POINTS)
        .map(|i| {
            let hz = hz_at_x(i as f32 / (CURVE_POINTS - 1) as f32 * NARROW_W, NARROW_W);
            verbspace::wet_filter_response_db(p, hz, sample_rate)
        })
        .collect();
    let top_sec = nice_ceiling(
        rt.iter()
            .copied()
            .filter(|sec| sec.is_finite())
            .fold(0.5, f32::max)
            * 1.15,
    );
    let splits = (p.bass_freq_hz, p.damp_freq_hz);
    let band_line = if profile.frozen {
        "Held".to_string()
    } else {
        format!(
            "L {} \u{00B7} H {}",
            fmt_seconds(profile.rt_low_sec),
            fmt_seconds(profile.rt_high_sec)
        )
    };
    let rt_box = display_box(s, 630.0, NARROW_W, move |b, k, window, cx| {
        paint_rt(window, cx, b, k, &rt, &cuts, top_sec, splits, dim)
    })
    .child(at(
        s,
        10.0,
        8.0,
        caption("Decay by frequency", 10.5, 0.1, FAINT, s),
    ))
    .child(at_right(s, 10.0, 8.0, numbers(band_line, 11.0, SOFT, s)));

    let toggles = vec![
        toggle(e, cx, "verb-wet-only", "Wet only", "wetOnly", s),
        toggle(e, cx, "verb-freeze", "Freeze", "freeze", s),
    ];
    well(s, 7.0, 327.0)
        .child(header(e, cx, s, "Algorithmic reverb", accent(), toggles))
        .child(div().relative().flex_1().child(decay_box).child(rt_box))
        .into_any_element()
}

/// The reverb's level over time: the pre-delay, the early reflections as
/// ticks, and each band's tail falling 60 dB in its RT60.
fn paint_decay(
    window: &mut Window,
    cx: &mut App,
    b: Bounds<Pixels>,
    k: f32,
    profile: &verbspace::DecayProfile,
    span: f32,
    dim: f32,
) {
    let (x0, y0) = plate::xy(b.origin);
    let at = |x: f32, y: f32| (x0 + x * k, y0 + y * k);
    let x_t = |ms: f32| (ms / span).clamp(0.0, 1.0) * WIDE_W;
    let y_db = |db: f32| PLOT_TOP + (db / FLOOR_DB).clamp(0.0, 1.0) * (PLOT_BOTTOM - PLOT_TOP);

    let step = nice_step(span, 6);
    let mut labels = vec![(0.0, "0".to_string())];
    let mut t = step;
    while t < span * 0.999 {
        let x = x_t(t);
        paint_polyline(
            window,
            &[at(x, PLOT_TOP), at(x, PLOT_BOTTOM)],
            1.0,
            color(GRID),
            None,
        );
        labels.push((x, fmt_time(t)));
        t += step;
    }
    for db in [-12.0, -24.0, -36.0, -48.0] {
        let y = y_db(db);
        paint_polyline(window, &[at(0.0, y), at(WIDE_W, y)], 1.0, color(GRID), None);
    }
    paint_axis(window, cx, b, k, &labels);

    // The pre-delay: nothing yet.
    if profile.predelay_ms > 0.5 {
        let x = x_t(profile.predelay_ms);
        let (px0, py0) = at(0.0, PLOT_TOP);
        paint_rect(
            window,
            (px0, py0, x * k, (PLOT_BOTTOM - PLOT_TOP) * k),
            color_a(0xffffff, 0.035),
        );
        paint_polyline(
            window,
            &[at(x, PLOT_TOP), at(x, PLOT_BOTTOM)],
            1.0,
            color_a(ACCENT, 0.55 * dim),
            Some((3.0 * k, 3.0 * k)),
        );
        let (tx, ty) = at(x + 5.0, PLOT_TOP + 2.0);
        paint_text(
            window,
            cx,
            &format!("pre {}", fmt_time(profile.predelay_ms)),
            plate::mono(gpui::FontWeight::NORMAL),
            10.0 * k,
            color(FAINT),
            (tx, ty),
            Align::Left,
        );
    }

    // Each band's tail, from the first late arrival, starting at the level
    // the Early/Late balance gives it.
    let onset = profile.predelay_ms + profile.first_late_ms;
    let start_db = (profile.late_db - 3.0).max(FLOOR_DB);
    let line_of = |rt_sec: f32| -> Vec<(f32, f32)> {
        let from = (x_t(onset), y_db(start_db));
        if !rt_sec.is_finite() {
            return vec![from, (WIDE_W, y_db(start_db))];
        }
        let end = onset + rt_sec * 1_000.0;
        let to = if end <= span {
            (x_t(end), y_db(start_db - 60.0))
        } else {
            (
                WIDE_W,
                y_db(start_db - 60.0 * (span - onset) / (end - onset).max(1.0e-3)),
            )
        };
        vec![from, to]
    };
    let mid = line_of(profile.rt_mid_sec);
    let mut area = mid.clone();
    area.push((mid[1].0, PLOT_BOTTOM));
    area.push((mid[0].0, PLOT_BOTTOM));
    let area: Vec<(f32, f32)> = area.into_iter().map(|(x, y)| at(x, y)).collect();
    paint_polygon(window, &area, color_a(ACCENT, 0.16 * dim));
    let scaled = |points: Vec<(f32, f32)>| -> Vec<(f32, f32)> {
        points.into_iter().map(|(x, y)| at(x, y)).collect()
    };
    paint_polyline(
        window,
        &scaled(line_of(profile.rt_low_sec)),
        1.4 * k,
        color_a(LOW, 0.9 * dim),
        None,
    );
    paint_polyline(
        window,
        &scaled(line_of(profile.rt_high_sec)),
        1.4 * k,
        color_a(HIGH, 0.9 * dim),
        None,
    );
    paint_polyline(window, &scaled(mid), 2.2 * k, color_a(ACCENT, dim), None);

    // The early reflections, as ticks from the floor: the left side a touch
    // left of the right, so coincident pairs both show.
    for reflection in profile.early.iter().filter(|r| r.gain != 0.0) {
        let db = 20.0 * reflection.gain.abs().max(1.0e-6).log10();
        let x = x_t(reflection.at_ms) + if reflection.right { 1.0 } else { -1.0 };
        let top = y_db(db.max(FLOOR_DB));
        let (tx, ty) = at(x, top);
        let ink = if reflection.right {
            color_a(0xa9a9a9, 0.7 * dim)
        } else {
            color_a(0xe6e6e6, 0.7 * dim)
        };
        paint_rect(window, (tx, ty, 1.6 * k, (PLOT_BOTTOM - top) * k), ink);
    }
    paint_polyline(
        window,
        &[at(0.0, PLOT_TOP), at(WIDE_W, PLOT_TOP)],
        1.0,
        color(0x3a3a3a),
        None,
    );
}

/// The decay across frequency, the wet cuts greying what never leaves, and
/// the two crossovers the Bass and Damping knobs work from.
#[allow(clippy::too_many_arguments)]
fn paint_rt(
    window: &mut Window,
    cx: &mut App,
    b: Bounds<Pixels>,
    k: f32,
    rt: &[f32],
    cuts: &[f32],
    top_sec: f32,
    (bass_hz, damp_hz): (f32, f32),
    dim: f32,
) {
    let (x0, y0) = plate::xy(b.origin);
    let at = |x: f32, y: f32| (x0 + x * k, y0 + y * k);
    paint_freq_grid(window, cx, b, k, NARROW_W);
    // What the wet cuts take, hanging from the top: 24 dB of cut reaches
    // the floor.
    let column = NARROW_W / (CURVE_POINTS - 1) as f32;
    let cut_line: Vec<(f32, f32)> = cuts
        .iter()
        .enumerate()
        .map(|(i, db)| {
            let amount = (-db / 24.0).clamp(0.0, 1.0);
            at(
                i as f32 * column,
                PLOT_TOP + amount * (PLOT_BOTTOM - PLOT_TOP),
            )
        })
        .collect();
    let mut cut_area = cut_line.clone();
    cut_area.push(at(NARROW_W, PLOT_TOP));
    cut_area.push(at(0.0, PLOT_TOP));
    paint_polygon(window, &cut_area, color_a(0xffffff, 0.05));
    paint_polyline(
        window,
        &cut_line,
        1.0,
        color_a(0xffffff, 0.22),
        Some((2.0 * k, 3.0 * k)),
    );
    for (hz, ink) in [(bass_hz, LOW), (damp_hz, HIGH)] {
        let x = x_of_hz(hz, NARROW_W);
        paint_polyline(
            window,
            &[at(x, PLOT_TOP), at(x, PLOT_BOTTOM)],
            1.0,
            color_a(ink, 0.6 * dim),
            Some((3.0 * k, 3.0 * k)),
        );
    }
    let span = PLOT_BOTTOM - PLOT_TOP;
    let points: Vec<(f32, f32)> = rt
        .iter()
        .enumerate()
        .map(|(i, sec)| {
            let level = if sec.is_finite() { sec / top_sec } else { 1.0 };
            at(
                i as f32 * column,
                PLOT_BOTTOM - level.clamp(0.0, 1.0) * (span - 4.0),
            )
        })
        .collect();
    let mut area = points.clone();
    area.push(at(NARROW_W, PLOT_BOTTOM));
    area.push(at(0.0, PLOT_BOTTOM));
    paint_polygon(window, &area, color_a(ACCENT, 0.16 * dim));
    paint_polyline(window, &points, 2.0 * k, color_a(ACCENT, dim), None);
    let (tx, ty) = at(6.0, PLOT_TOP + 2.0);
    paint_text(
        window,
        cx,
        &fmt_seconds(top_sec),
        plate::mono(gpui::FontWeight::NORMAL),
        10.0 * k,
        color(0x6f6f6f),
        (tx, ty),
        Align::Left,
    );
}

// ── The deck ───────────────────────────────────────────────────────────────

fn controls(e: &KitEditor<VerbSpacePlate>, cx: &mut Cx<VerbSpacePlate>, s: f32) -> AnyElement {
    deck(s, 341.0, 420.0)
        .p(px(14.0 * s))
        .gap(px(12.0 * s))
        .child(space_card(e, cx, s).w(px(280.0 * s)).flex_shrink_0())
        .child(character_card(e, cx, s).w(px(300.0 * s)).flex_shrink_0())
        .child(movement_card(e, cx, s).w(px(164.0 * s)).flex_shrink_0())
        .child(output_card(e, cx, s).flex_1())
        .into_any_element()
}

#[allow(clippy::too_many_arguments)]
fn cell(
    cx: &mut Cx<VerbSpacePlate>,
    dial: Dial,
    value: f32,
    readout: Option<String>,
    look: KnobLook,
    cell: KnobCell,
    ink: gpui::Rgba,
    s: f32,
) -> AnyElement {
    let id = dial.id;
    knob_reading(
        cx,
        format!("verb-{id}"),
        dial,
        value,
        readout,
        look,
        cell,
        ink,
        true,
        s,
        move |this: &mut KitEditor<VerbSpacePlate>, value, cx| this.set_value(id, value, cx),
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

fn space_card(e: &KitEditor<VerbSpacePlate>, cx: &mut Cx<VerbSpacePlate>, s: f32) -> gpui::Div {
    let p = &e.params;
    let d = dials();
    // A type is a starting point: picking one moves the knobs to its space,
    // and the knobs alone shape the sound.
    let mut types: Vec<AnyElement> = ReverbMode::ALL
        .into_iter()
        .enumerate()
        .map(|(index, mode)| {
            pill(
                ("verb-type", index),
                mode.label(),
                p.mode == mode,
                accent(),
                PillLook::words(28.0, 0.0, 12.0),
                true,
                s,
                e.click_cb(cx, move |this, cx| {
                    let next = mode.starting_point(&this.params);
                    this.set_params(next, cx);
                }),
            )
            .into_any_element()
        })
        .collect();
    let second: Vec<AnyElement> = types.split_off(3);
    let look = KnobLook::small(64.0).styled(3.6, 17.5, FACE, FACE_EDGE, 0.8, 7.0, 1.8);
    let k = KnobCell::new(80.0, 10.5, 12.5);
    card(s)
        .child(card_title("Space", s))
        .child(
            section("Start from", s)
                .child(columns(3.0, s, types))
                .child(columns(3.0, s, second)),
        )
        .child(row(vec![
            cell(cx, d.predelay, p.predelay_ms, None, look, k, accent(), s),
            cell(cx, d.size, p.size, None, look, k, accent(), s),
            cell(
                cx,
                d.decay,
                p.decay_sec,
                Some(fmt_seconds(p.decay_sec)),
                look,
                k,
                accent(),
                s,
            ),
        ]))
}

fn character_card(e: &KitEditor<VerbSpacePlate>, cx: &mut Cx<VerbSpacePlate>, s: f32) -> gpui::Div {
    let p = &e.params;
    let d = dials();
    let look = KnobLook::small(48.0);
    let k = KnobCell::new(66.0, 9.5, 11.5);
    card(s)
        .child(card_title("Character", s))
        .child(section("Bands", s).child(row(vec![
            cell(
                cx,
                d.bass,
                p.bass_mult,
                Some(format!("{:.2}\u{00D7}", p.bass_mult)),
                look,
                k,
                color(LOW),
                s,
            ),
            cell(
                cx,
                d.bass_freq,
                p.bass_freq_hz,
                None,
                look,
                k,
                color(LOW),
                s,
            ),
            cell(cx, d.damping, p.damping, None, look, k, color(HIGH), s),
            cell(
                cx,
                d.damp_freq,
                p.damp_freq_hz,
                None,
                look,
                k,
                color(HIGH),
                s,
            ),
        ])))
        .child(section("Texture", s).child(row(vec![
            cell(cx, d.diffusion, p.diffusion, None, look, k, accent(), s),
            cell(
                cx,
                d.early_late,
                p.early_late,
                Some(fmt_early_late(p.early_late)),
                look,
                k,
                accent(),
                s,
            ),
            cell(cx, d.low_cut, p.low_cut_hz, None, look, k, color(INK), s),
            cell(cx, d.high_cut, p.high_cut_hz, None, look, k, color(INK), s),
        ])))
}

fn movement_card(e: &KitEditor<VerbSpacePlate>, cx: &mut Cx<VerbSpacePlate>, s: f32) -> gpui::Div {
    let p = &e.params;
    let d = dials();
    let look = KnobLook::small(48.0);
    let k = KnobCell::new(64.0, 9.5, 11.5);
    card(s)
        .child(card_title("Movement", s))
        .child(section("Modulation", s).child(row(vec![
            cell(cx, d.depth, p.mod_depth, None, look, k, accent(), s),
            cell(
                cx,
                d.rate,
                p.mod_rate_hz,
                Some(format!("{:.2} Hz", p.mod_rate_hz)),
                look,
                k,
                accent(),
                s,
            ),
        ])))
        .child(
            section("Stereo", s).child(div().flex().flex_row().justify_center().child(cell(
                cx,
                d.width,
                p.width,
                None,
                look,
                k,
                color(INK),
                s,
            ))),
        )
}

fn output_card(e: &KitEditor<VerbSpacePlate>, cx: &mut Cx<VerbSpacePlate>, s: f32) -> gpui::Div {
    let p = &e.params;
    let d = dials();
    let look = KnobLook::small(56.0).styled(4.5, 16.0, FACE, FACE_EDGE, 1.2, 7.0, 2.4);
    let k = KnobCell::new(76.0, 10.0, 12.0);
    let mix_note = if p.wet_only {
        "Wet only: the dry signal is off and the wet plays at full level."
    } else {
        "Equal-power balance between the dry and the wet signal."
    };
    card(s)
        .child(card_title("Output", s))
        .child(row(vec![
            cell(cx, d.mix, p.mix, None, look, k, color(INK), s),
            cell(cx, d.output, p.output_db, None, look, k, color(INK), s),
        ]))
        .child(
            div()
                .text_size(plate::type_px(11.0, s))
                .line_height(px(14.0 * s))
                .text_color(color(SOFT))
                .child(mix_note),
        )
        .child(io_meters(&e.live_bounds, SLOT_METER, s))
}

/// Opens with a track's VerbSpace insert.
pub fn open_verbspace_editor(
    owner_bounds: Option<gpui::Bounds<gpui::Pixels>>,
    key: PluginInstanceKey,
    identity: ShellIdentity,
    host_ops: BuiltinEditorHostOps,
    on_close: Arc<dyn Fn(&mut Window, &mut App)>,
    cx: &mut App,
) -> Result<gpui::WindowHandle<VerbSpaceWindow>, String> {
    plate::ensure_fonts(cx);
    open_kit_editor(
        VerbSpacePlate,
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
            d.predelay,
            d.size,
            d.decay,
            d.bass,
            d.bass_freq,
            d.damping,
            d.damp_freq,
            d.diffusion,
            d.early_late,
            d.low_cut,
            d.high_cut,
            d.depth,
            d.rate,
            d.width,
            d.mix,
            d.output,
        ] {
            let (min, max, default) = verbspace::descriptor()
                .params
                .iter()
                .find(|row| row.id == dial.id)
                .map(|row| (row.min, row.max, row.default_value))
                .unwrap();
            assert_eq!((dial.min, dial.max), (min, max), "{}", dial.id);
            assert!(
                (dial.at(dial.norm(default)) - default).abs() <= 0.011 * default.abs().max(1.0),
                "{} default does not round-trip",
                dial.id
            );
        }
    }

    #[test]
    fn the_width_dial_rests_at_an_untouched_image() {
        assert!((dials().width.norm(100.0) - 0.5).abs() < 1.0e-6);
    }

    #[test]
    fn a_type_is_a_starting_point_that_keeps_the_routing() {
        let current = Params {
            wet_only: true,
            freeze: true,
            ..verbspace::default_params()
        };
        let next = ReverbMode::Plate.starting_point(&current);
        assert_eq!(next.mode, ReverbMode::Plate);
        assert!(next.wet_only);
    }

    #[test]
    fn presets_keep_power_freeze_and_routing() {
        let current = Params {
            power: false,
            wet_only: true,
            ..verbspace::default_params()
        };
        for preset in VerbSpacePlate.presets().iter() {
            let next = VerbSpacePlate.preset_applied(&current, &preset.params);
            assert!(!next.power && next.wet_only, "{}", preset.name);
        }
    }

    #[test]
    fn the_decay_axis_covers_the_tail() {
        let profile = verbspace::decay_profile(&verbspace::default_params(), 48_000.0);
        let span = decay_span(&profile);
        assert!(span >= profile.predelay_ms + profile.longest_sec() * 1_000.0);
        assert_eq!(fmt_early_late(65.0), "35/65");
    }
}

//! The De-Esser's editor, after its design: the input spectrum with the
//! detection band and the cut it takes, and the detector's level against the
//! threshold with the gain reduction over time, in the well; Detection,
//! Reduction and Output cards on the deck.
//!
//! The band and the cut's shape come from the DSP's own band-pass
//! (`deesser::band_coefficients`); the spectrum is the host's analysis of the
//! insert's input, and the history is the detector level and the reduction
//! the DSP reports in its meter frames (`deesser::MeterFrame::detector_peak`
//! rides slot 0's input peak). With no telemetry the displays draw only what
//! the params say.

use std::sync::Arc;

use SpherePluginHost::spectrum::{MAX_HZ, MIN_HZ, SPECTRUM_BINS};
use deesser::{Mode, Params, StereoMode};
use gpui::{
    AnyElement, App, FontWeight, InteractiveElement, IntoElement, MouseButton, MouseMoveEvent,
    ParentElement, Rgba, StatefulInteractiveElement, Styled, Window, div, px,
};

use crate::components::builtin_plugin_editor::builtin_deesser_params;
use crate::components::builtin_plugin_editor_window::{BuiltinEditorHostOps, PluginInstanceKey};
use crate::components::fx_model::KnobSpec;
use crate::components::native_plugin_shell::ShellIdentity;
use crate::components::plate::{
    self, Align, Dial, DialUnit, FACE, FACE_EDGE, FAINT, GROUND, INK, KnobCell, KnobLook, PillLook,
    SOFT, WELL_EDGE, WHITE, caption, card, card_title, color, color_a, columns, deck, header, icon,
    knob_reading, legend, live_slot, numbers, paint_polygon, paint_polyline, paint_rect,
    paint_text, pill, plate_frame, plate_scale, type_px, well,
};
use crate::components::plugin_kit::{
    Cx, KitEditor, KitModel, KitPreset, KitWindow, LiveBounds, LiveSlot, bank, open_kit_editor,
};
use crate::components::plugin_live::Live;

/// The De-Esser's accent.
pub const ACCENT: u32 = 0xff8a3d;

const SLOT_SPECTRUM: usize = 0;
const SLOT_HISTORY: usize = 1;
const SLOT_GR_READOUT: usize = 2;
const SLOT_GR_BAR: usize = 3;
const SLOT_GR_PEAK: usize = 4;
const SLOT_METER: usize = 5;

/// The spectrum box and the history box, in design units.
const SW: f32 = 600.0;
const SH: f32 = 250.0;
const HW: f32 = 364.0;
const HH: f32 = 250.0;
/// History points shown: about four seconds at the telemetry rate.
const HISTORY_POINTS: usize = 120;
/// The reduction scales: the bar's and the history's full span.
const GR_SPAN_DB: f32 = 24.0;
/// The voices' detection frequencies.
const FEMALE_HZ: f32 = 7_500.0;
const MALE_HZ: f32 = 5_500.0;
/// The band pill's size and its row.
const PILL_W: f32 = 64.0;
const PILL_H: f32 = 26.0;
const PILL_TOP: f32 = 196.0;

const FREQ: Dial = Dial::new(
    "freqHz",
    "FREQUENCY",
    2_000.0,
    16_000.0,
    7_500.0,
    DialUnit::Hz,
)
.log();
const WIDTH: Dial = Dial::new("widthOct", "WIDTH", 0.3, 3.0, 1.2, DialUnit::Plain);
const THRESHOLD: Dial = Dial::new("thresholdDb", "THRESHOLD", -60.0, 0.0, -30.0, DialUnit::Db);
const RANGE: Dial = Dial::new("rangeDb", "RANGE", 0.0, 24.0, 10.0, DialUnit::Db);
const RELEASE: Dial = Dial::new("releaseMs", "RELEASE", 5.0, 300.0, 60.0, DialUnit::Ms).log();
const LOOKAHEAD: Dial = Dial::new("lookaheadMs", "LOOKAHEAD", 0.0, 10.0, 2.0, DialUnit::Ms);
const MIX: Dial = Dial::new("mix", "MIX", 0.0, 100.0, 100.0, DialUnit::Percent);
const OUTPUT: Dial = Dial::new("outputDb", "OUTPUT", -12.0, 12.0, 0.0, DialUnit::Db).bipolar();

const HEADPHONES: &str = "M2 9 V6 A5 5 0 0 1 12 6 V9 M2 9 H4 V12 H2 Z M10 9 H12 V12 H10 Z";

const FREQ_LABELS: [(f32, &str); 7] = [
    (92.0, "100"),
    (190.0, "200"),
    (334.0, "1k"),
    (394.0, "2k"),
    (474.0, "5k"),
    (532.0, "10k"),
    (585.0, "20k"),
];

fn accent() -> Rgba {
    color(ACCENT)
}

/// `7.5 kHz`, `940 Hz`: the design's band readout.
fn fmt_band(hz: f32) -> String {
    if hz < 1_000.0 {
        format!("{} Hz", hz.round())
    } else {
        format!("{:.1} kHz", hz / 1_000.0)
    }
}

/// The spectrum box's x for `hz`: 20 Hz to 20 kHz across it.
fn x_of(hz: f32) -> f32 {
    SW * (hz.max(1.0) / 20.0).log10() / 3.0
}

/// The frequency at the spectrum box's `x`.
fn hz_at(x: f32) -> f32 {
    20.0 * 1_000f32.powf(x / SW)
}

/// The spectrum box's y for an analysis level.
fn y_of_level(db: f32) -> f32 {
    (40.0 + (-db - 10.0) / 80.0 * (SH - 40.0)).clamp(34.0, SH)
}

/// The history box's y for a detector level.
fn y_of_detector(db: f32) -> f32 {
    (-db / 60.0 * HH).clamp(0.0, HH)
}

/// The history box's y for a reduction, from the top.
fn y_of_reduction(db: f32) -> f32 {
    (db / GR_SPAN_DB * HH * 0.5).clamp(0.0, HH)
}

/// The voice a frequency reads as, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Voice {
    Female,
    Male,
    Custom,
}

fn voice_of(hz: f32) -> Voice {
    if (hz / FEMALE_HZ - 1.0).abs() < 0.005 {
        Voice::Female
    } else if (hz / MALE_HZ - 1.0).abs() < 0.005 {
        Voice::Male
    } else {
        Voice::Custom
    }
}

/// The band-pass's complex response at `hz`, as (re, im).
fn band_response(params: &Params, hz: f32, sample_rate: f32) -> (f32, f32) {
    let c = deesser::band_coefficients(params.freq_hz, params.width_oct, sample_rate);
    let w = std::f32::consts::TAU * hz / sample_rate.max(1.0);
    let (c1, s1, c2, s2) = (w.cos(), w.sin(), (2.0 * w).cos(), (2.0 * w).sin());
    let (nr, ni) = (c.b0 + c.b1 * c1 + c.b2 * c2, -(c.b1 * s1 + c.b2 * s2));
    let (dr, di) = (1.0 + c.a1 * c1 + c.a2 * c2, -(c.a1 * s1 + c.a2 * s2));
    let den = (dr * dr + di * di).max(1.0e-20);
    ((nr * dr + ni * di) / den, (ni * dr - nr * di) / den)
}

/// What the cut does at `hz`, in dB (≤ 0), for a share `k` of the band taken
/// out — split: `x − k·bp(x)`; wide: the whole signal at `1 − k`.
fn cut_at(params: &Params, hz: f32, k: f32, sample_rate: f32) -> f32 {
    if k <= 0.0 {
        return 0.0;
    }
    match params.mode {
        Mode::Wide => 20.0 * (1.0 - k).max(1.0e-6).log10(),
        Mode::Split => {
            let (re, im) = band_response(params, hz, sample_rate);
            let (re, im) = (1.0 - k * re, -k * im);
            10.0 * (re * re + im * im).max(1.0e-12).log10()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DeEsserPlate;

/// A De-Esser insert's editor window.
pub type DeEsserWindow = KitWindow<DeEsserPlate>;

/// The editor's own state: the band pill being dragged.
#[derive(Debug, Default)]
pub struct DeEsserUi {
    dragging: bool,
}

impl KitModel for DeEsserPlate {
    type Params = Params;
    type Ui = DeEsserUi;
    type LiveView = Params;

    fn title(self) -> &'static str {
        "De-Esser"
    }

    fn subtitle(self, _params: &Params) -> String {
        "SIBILANCE CONTROL".into()
    }

    fn key(self) -> &'static str {
        "deesser-plate"
    }

    fn defaults(self) -> Params {
        deesser::default_params()
    }

    fn from_mirror(self, insert_id: &str) -> Option<Params> {
        builtin_deesser_params(insert_id)
    }

    fn wire_values(self, params: &Params) -> Vec<(u32, f32)> {
        deesser::ipc::ui_values(params)
            .into_iter()
            .filter_map(|(id, value)| deesser::ui_param_index(id).map(|index| (index, value)))
            .collect()
    }

    fn with(self, params: &Params, id: &str, value: f32) -> Params {
        let mut next = params.clone();
        deesser::ipc::apply_ui_param(&mut next, id, value);
        next
    }

    fn value(self, params: &Params, id: &str) -> f32 {
        deesser::ipc::ui_values(params)
            .into_iter()
            .find(|(candidate, _)| *candidate == id)
            .map_or(0.0, |(_, value)| value)
    }

    fn presets(self) -> Arc<Vec<KitPreset<Params>>> {
        static BANK: std::sync::OnceLock<Arc<Vec<KitPreset<Params>>>> = std::sync::OnceLock::new();
        BANK.get_or_init(|| bank(deesser::factory_presets(), |p| (p.name, p.params)))
            .clone()
    }

    fn preset_applied(self, current: &Params, preset: &Params) -> Params {
        let mut next = preset.clone();
        next.power = current.power;
        next.listen = current.listen;
        next
    }

    fn knob(self, _id: &str) -> Option<KnobSpec> {
        None
    }

    /// Listening is let go as the window closes, so a closed editor never
    /// leaves a track playing only its sibilance.
    fn on_close(self, params: &Params) -> Option<Params> {
        params.listen.then(|| Params {
            listen: false,
            ..params.clone()
        })
    }

    fn panel(e: &KitEditor<Self>, cx: &mut Cx<Self>) -> AnyElement {
        let s = plate_scale(e.viewport);
        plate_frame(s, vec![display(e, cx, s), controls(e, cx, s)])
    }

    fn live_view(e: &KitEditor<Self>) -> Params {
        e.params.clone()
    }

    fn paint_live(
        view: &Params,
        live: &mut Live,
        bounds: &LiveBounds,
        window: &mut Window,
        cx: &mut App,
    ) {
        let viewport = window.viewport_size();
        let s = plate_scale((f32::from(viewport.width), f32::from(viewport.height)));
        let sample_rate = live.sample_rate().unwrap_or(48_000.0);
        let block = live.pads().copied();
        let gr = block.map(|b| b[deesser::telemetry::GR_DB].max(0.0));
        let gr_peak = block.map(|b| b[deesser::telemetry::GR_PEAK_DB].max(0.0));

        if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_SPECTRUM)) {
            paint_spectrum(window, view, live, gr.unwrap_or(0.0), sample_rate, b);
        }
        if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_HISTORY)) {
            paint_history(window, view, live, b);
        }
        let mono = plate::mono(FontWeight::NORMAL);
        if let (Some(b), Some(gr)) = (bounds.get(LiveSlot::Plate(SLOT_GR_READOUT)), gr) {
            let (x, y) = plate::xy(b.origin);
            let w = f32::from(b.size.width);
            let (text, ink) = if gr > 0.05 {
                (format!("\u{2212}{gr:.1}"), accent())
            } else {
                ("0.0".to_string(), color(FAINT))
            };
            paint_text(
                window,
                cx,
                &text,
                mono.clone(),
                20.0 * s,
                ink,
                (x + w, y),
                Align::Right,
            );
        }
        if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_GR_BAR)) {
            let (x, y) = plate::xy(b.origin);
            let (w, h) = (f32::from(b.size.width), f32::from(b.size.height));
            window.paint_quad(
                gpui::fill(plate::rect(x, y, w, h), color(0x1a1a1a)).corner_radii(px(2.0 * s)),
            );
            if let Some(gr) = gr {
                let fill = (gr / GR_SPAN_DB).clamp(0.0, 1.0) * w;
                paint_rect(window, (x + w - fill, y, fill, h), accent());
            }
            if let Some(peak) = gr_peak {
                let at = (peak / GR_SPAN_DB).clamp(0.0, 1.0) * w;
                let left = (x + w - at - 2.0 * s).max(x);
                paint_rect(window, (left, y, 2.0 * s, h), color(WHITE));
            }
        }
        if let (Some(b), Some(peak)) = (bounds.get(LiveSlot::Plate(SLOT_GR_PEAK)), gr_peak) {
            let (x, y) = plate::xy(b.origin);
            let w = f32::from(b.size.width);
            paint_text(
                window,
                cx,
                &format!("peak \u{2212}{peak:.1} dB"),
                mono.clone(),
                10.5 * s,
                color(INK),
                (x + w, y),
                Align::Right,
            );
        }
        if let Some(frame) = live.frame {
            for (index, peak) in [frame.in_peak, frame.out_peak].into_iter().enumerate() {
                let Some(b) = bounds.get(LiveSlot::Plate(SLOT_METER + index)) else {
                    continue;
                };
                paint_meter(window, cx, b, 20.0 * peak.max(1.0e-9).log10(), s);
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
        if !e.ui.dragging {
            return;
        }
        if event.pressed_button != Some(MouseButton::Left) {
            e.ui.dragging = false;
            return;
        }
        let Some(b) = e.live_bounds.get(LiveSlot::Plate(SLOT_SPECTRUM)) else {
            return;
        };
        let width = f32::from(b.size.width).max(1.0);
        let x = ((f32::from(event.position.x) - f32::from(b.origin.x)) / width).clamp(0.0, 1.0);
        let hz = DialUnit::Hz.round(hz_at(x * SW)).clamp(FREQ.min, FREQ.max);
        e.set_value("freqHz", hz, cx);
    }

    fn mouse_up(e: &mut KitEditor<Self>, _cx: &mut Cx<Self>) {
        e.ui.dragging = false;
    }
}

// ── The well ───────────────────────────────────────────────────────────────

fn display(e: &KitEditor<DeEsserPlate>, cx: &mut Cx<DeEsserPlate>, s: f32) -> AnyElement {
    let p = &e.params;
    let listen = {
        let on = p.listen;
        let (bg, border, fg) = if on {
            (accent(), accent(), color(0x111111))
        } else {
            (color(GROUND), color(WELL_EDGE), color(INK))
        };
        div()
            .id("deesser-listen")
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0 * s))
            .h(px(28.0 * s))
            .px(px(10.0 * s))
            .rounded(px(3.0 * s))
            .border(px(1.0))
            .border_color(border)
            .bg(bg)
            .text_color(fg)
            .text_size(type_px(12.0, s))
            .cursor(gpui::CursorStyle::PointingHand)
            .child(icon(HEADPHONES, (14.0, 14.0), (13.0, 13.0), 1.3, fg, s))
            .child("Listen")
            .on_click(e.click_cb(cx, |this, cx| this.toggle("listen", cx)))
            .into_any_element()
    };

    let frame_box = |left: f32, w: f32| {
        div()
            .absolute()
            .left(px(left * s))
            .top(px(2.0 * s))
            .w(px(w * s))
            .h(px(250.0 * s))
            .bg(color(0x0d0d0d))
            .border(px(1.0))
            .border_color(color(0x242424))
            .overflow_hidden()
    };
    let at = |left: f32, top: f32, child: gpui::Div| {
        div()
            .absolute()
            .left(px(left * s))
            .top(px(top * s))
            .child(plate::over_live(child))
    };

    // The spectrum box: the band pill's hit target is a plain, transparent
    // div — it must not occlude, or the root's mouse-move stops reaching the
    // drag — and its face is painted over the live display.
    let cx_pill = x_of(p.freq_hz).clamp(PILL_W / 2.0, SW - PILL_W / 2.0);
    let pill_face = div()
        .flex()
        .items_center()
        .justify_center()
        .w(px(PILL_W * s))
        .h(px(PILL_H * s))
        .rounded(px(PILL_H / 2.0 * s))
        .bg(accent())
        .text_color(color(0x111111))
        .font(plate::mono(FontWeight::SEMIBOLD))
        .text_size(type_px(11.0, s))
        .child(fmt_band(p.freq_hz));
    let pill_target = div()
        .id("deesser-band")
        .absolute()
        .left(px((cx_pill - PILL_W / 2.0) * s))
        .top(px(PILL_TOP * s))
        .w(px(PILL_W * s))
        .h(px(PILL_H * s))
        .cursor(gpui::CursorStyle::ResizeLeftRight)
        .on_mouse_down(
            MouseButton::Left,
            e.press_cb(cx, |this, _, _, cx| {
                this.ui.dragging = true;
                cx.notify();
            }),
        )
        .on_scroll_wheel(e.wheel_cb(cx, |this, dy, fine, cx| {
            let hz = plate::wheel_turn(FREQ, this.params.freq_hz, dy, fine);
            this.set_value("freqHz", hz, cx);
        }));
    let mut labels = div().absolute().inset_0();
    for (left, text) in FREQ_LABELS {
        let label = numbers(text, 10.0, 0x6f6f6f, s);
        labels = labels.child(if text == "20k" {
            div()
                .absolute()
                .right(px((SW - left) * s))
                .bottom(px(4.0 * s))
                .child(plate::over_live(label))
        } else {
            div()
                .absolute()
                .left(px(left * s))
                .bottom(px(4.0 * s))
                .child(plate::over_live(label))
        });
    }
    let spectrum = frame_box(16.0, SW)
        .child(
            live_slot(&e.live_bounds, SLOT_SPECTRUM)
                .absolute()
                .size_full(),
        )
        .child(at(
            8.0,
            6.0,
            caption("Spectrum \u{00B7} Reduction", 10.5, 0.1, FAINT, s),
        ))
        .child(
            div()
                .absolute()
                .right(px(8.0 * s))
                .top(px(22.0 * s))
                .child(plate::over_live(numbers("0 dB", 10.0, FAINT, s))),
        )
        .child(labels)
        .child(at(cx_pill - PILL_W / 2.0, PILL_TOP, pill_face))
        .child(pill_target);

    // The history box.
    let thr_y = y_of_detector(p.threshold_db);
    let swatch = |ink: Rgba| div().w(px(12.0 * s)).h(px(2.0 * s)).bg(ink);
    let history = frame_box(630.0, HW)
        .child(
            live_slot(&e.live_bounds, SLOT_HISTORY)
                .absolute()
                .size_full(),
        )
        .child(at(
            8.0,
            6.0,
            caption("Detector \u{00B7} Gain reduction", 10.5, 0.1, FAINT, s),
        ))
        .child(
            div()
                .absolute()
                .right(px(8.0 * s))
                .top(px(4.0 * s))
                .w(px(90.0 * s))
                .h(px(26.0 * s))
                .child(live_slot(&e.live_bounds, SLOT_GR_READOUT).size_full()),
        )
        .child(
            div()
                .absolute()
                .right(px(6.0 * s))
                .top(px((thr_y - 14.0).max(30.0) * s))
                .px(px(4.0 * s))
                .child(plate::over_live(numbers(
                    format!("thr {}", plate::signed(p.threshold_db)),
                    10.0,
                    ACCENT,
                    s,
                ))),
        )
        .child(
            div()
                .absolute()
                .left(px(8.0 * s))
                .bottom(px(6.0 * s))
                .child(plate::over_live(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(14.0 * s))
                        .child(legend(swatch(color(0x9a9a9a)), "Sibilance", s))
                        .child(legend(swatch(accent()), "Reduction", s)),
                )),
        );

    well(s, 7.0, 327.0)
        .child(header(
            e,
            cx,
            s,
            "Sibilance control",
            accent(),
            vec![listen],
        ))
        .child(div().relative().flex_1().child(spectrum).child(history))
        .into_any_element()
}

fn paint_spectrum(
    window: &mut Window,
    params: &Params,
    live: &Live,
    gr: f32,
    sample_rate: f32,
    b: gpui::Bounds<gpui::Pixels>,
) {
    let k = (f32::from(b.size.width) / SW).max(0.01);
    let (x0, y0) = plate::xy(b.origin);
    let at = |x: f32, y: f32| (x0 + x * k, y0 + y * k);
    for hz in [
        50.0, 100.0, 200.0, 500.0, 1_000.0, 2_000.0, 5_000.0, 10_000.0,
    ] {
        let x = x_of(hz);
        paint_polyline(window, &[at(x, 0.0), at(x, SH)], 1.0, color(0x1d1d1d), None);
    }
    // The band: as wide as the width, around the frequency.
    let half = params.width_oct / 2.0;
    let lo = x_of(params.freq_hz / 2f32.powf(half));
    let hi = x_of((params.freq_hz * 2f32.powf(half)).min(20_000.0));
    let (bx, by) = at(lo, 0.0);
    paint_rect(
        window,
        (bx, by, (hi - lo).max(2.0) * k, SH * k),
        color_a(ACCENT, 0.08),
    );
    for x in [lo, hi] {
        paint_polyline(
            window,
            &[at(x, 0.0), at(x, SH)],
            1.0,
            color_a(ACCENT, 0.45),
            Some((3.0 * k, 3.0 * k)),
        );
    }

    // The share of the band taken out now: the cut, through the mix.
    let mix = (params.mix / 100.0).clamp(0.0, 1.0);
    let share = if params.power && !params.listen && mix > 0.0 {
        mix * (1.0 - 10f32.powf(-(gr / mix) / 20.0))
    } else {
        0.0
    };
    let steps = 200;
    let freqs: Vec<f32> = (0..=steps)
        .map(|i| hz_at(i as f32 / steps as f32 * SW))
        .collect();

    if let Some(bins) = live.spectrum_bins() {
        let bin_hz =
            |i: usize| MIN_HZ * (MAX_HZ / MIN_HZ).powf(i as f32 / (SPECTRUM_BINS - 1) as f32);
        let pre: Vec<(f32, f32)> = bins
            .iter()
            .enumerate()
            .map(|(i, &db)| at(x_of(bin_hz(i)), y_of_level(db)))
            .collect();
        let post: Vec<(f32, f32)> = bins
            .iter()
            .enumerate()
            .map(|(i, &db)| {
                let hz = bin_hz(i);
                let change = if !params.power {
                    0.0
                } else if params.listen {
                    let (re, im) = band_response(params, hz, sample_rate);
                    10.0 * (re * re + im * im).max(1.0e-12).log10()
                } else {
                    cut_at(params, hz, share, sample_rate)
                };
                at(x_of(hz), y_of_level(db + change))
            })
            .collect();
        let mut area = pre.clone();
        area.push(at(SW, SH));
        area.push(at(0.0, SH));
        paint_polygon(window, &area, color_a(0xffffff, 0.05));
        paint_polyline(window, &pre, 1.0, color(0x5a5a5a), None);
        paint_polyline(window, &post, 1.2 * k, color(0xd9d9d9), None);
    }

    paint_polyline(
        window,
        &[at(0.0, 30.0), at(SW, 30.0)],
        1.0,
        color(0x3a3a3a),
        None,
    );
    if share > 0.0 {
        let cut: Vec<(f32, f32)> = freqs
            .iter()
            .map(|&hz| {
                at(
                    x_of(hz),
                    30.0 - cut_at(params, hz, share, sample_rate) * 6.0,
                )
            })
            .collect();
        let mut area = cut.clone();
        area.push(at(SW, 30.0));
        area.push(at(0.0, 30.0));
        paint_polygon(window, &area, color_a(ACCENT, 0.3));
        paint_polyline(window, &cut, 2.0 * k, accent(), None);
    }
}

fn paint_history(window: &mut Window, params: &Params, live: &Live, b: gpui::Bounds<gpui::Pixels>) {
    let k = (f32::from(b.size.width) / HW).max(0.01);
    let (x0, y0) = plate::xy(b.origin);
    let at = |x: f32, y: f32| (x0 + x * k, y0 + y * k);
    for y in [62.5, 125.0, 187.5] {
        paint_polyline(window, &[at(0.0, y), at(HW, y)], 1.0, color(0x1d1d1d), None);
    }
    let count = live.history_len().min(HISTORY_POINTS);
    let step = HW / (HISTORY_POINTS - 1) as f32;
    // Oldest first, the newest at the right edge.
    let points: Vec<(f32, f32, f32)> = (0..count)
        .rev()
        .map(|age| {
            let point = live.history_at(age);
            let x = HW - age as f32 * step;
            (x, point.slot_in_db, point.reduction_db)
        })
        .collect();
    if points.len() >= 2 {
        let first = points[0].0;
        let detector: Vec<(f32, f32)> = points
            .iter()
            .map(|&(x, db, _)| at(x, y_of_detector(db)))
            .collect();
        let mut area = detector.clone();
        area.push(at(HW, HH));
        area.push(at(first, HH));
        paint_polygon(window, &area, color_a(0xffffff, 0.07));
        paint_polyline(window, &detector, 1.2 * k, color(0x9a9a9a), None);
    }
    paint_polyline(
        window,
        &[
            at(0.0, y_of_detector(params.threshold_db)),
            at(HW, y_of_detector(params.threshold_db)),
        ],
        1.0,
        accent(),
        Some((5.0 * k, 3.0 * k)),
    );
    if points.len() >= 2 {
        let first = points[0].0;
        let reduction: Vec<(f32, f32)> = points
            .iter()
            .map(|&(x, _, gr)| at(x, y_of_reduction(gr)))
            .collect();
        let mut area = reduction.clone();
        area.push(at(HW, 0.0));
        area.push(at(first, 0.0));
        paint_polygon(window, &area, color_a(ACCENT, 0.3));
        paint_polyline(window, &reduction, 1.6 * k, accent(), None);
    }
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
        &plate::signed(db.max(-120.0)),
        plate::mono(FontWeight::NORMAL),
        10.5 * s,
        color(INK),
        (x + w, y + (h - 13.0 * s) * 0.5),
        Align::Right,
    );
}

// ── The deck ───────────────────────────────────────────────────────────────

fn controls(e: &KitEditor<DeEsserPlate>, cx: &mut Cx<DeEsserPlate>, s: f32) -> AnyElement {
    deck(s, 341.0, 420.0)
        .p(px(14.0 * s))
        .gap(px(12.0 * s))
        .child(detection_card(e, cx, s).w(px(340.0 * s)).flex_shrink_0())
        .child(reduction_card(e, cx, s).w(px(330.0 * s)).flex_shrink_0())
        .child(output_card(e, cx, s).flex_1())
        .into_any_element()
}

fn set(id: &'static str) -> impl Fn(&mut KitEditor<DeEsserPlate>, f32, &mut Cx<DeEsserPlate>) {
    move |this, value, cx| this.set_value(id, value, cx)
}

#[allow(clippy::too_many_arguments)]
fn dial(
    cx: &mut Cx<DeEsserPlate>,
    dial: Dial,
    value: f32,
    readout: Option<String>,
    look: KnobLook,
    cell: KnobCell,
    ink: Rgba,
    s: f32,
) -> AnyElement {
    knob_reading(
        cx,
        format!("deesser-{}", dial.id),
        dial,
        value,
        readout,
        look,
        cell,
        ink,
        true,
        s,
        set(dial.id),
    )
}

fn section(label: &str, s: f32) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap(px(6.0 * s))
        .child(caption(label, 10.5, 0.1, SOFT, s))
}

fn choices(
    e: &KitEditor<DeEsserPlate>,
    cx: &mut Cx<DeEsserPlate>,
    id: &'static str,
    items: Vec<(&'static str, bool, Option<(&'static str, f32)>)>,
    look: PillLook,
    s: f32,
) -> gpui::Div {
    let pills: Vec<AnyElement> = items
        .into_iter()
        .enumerate()
        .map(|(index, (label, on, edit))| {
            pill(
                (id, index),
                label,
                on,
                accent(),
                look,
                true,
                s,
                e.click_cb(cx, move |this, cx| {
                    if let Some((param, value)) = edit {
                        this.set_value(param, value, cx);
                    }
                }),
            )
            .into_any_element()
        })
        .collect();
    columns(3.0, s, pills)
}

fn detection_card(e: &KitEditor<DeEsserPlate>, cx: &mut Cx<DeEsserPlate>, s: f32) -> gpui::Div {
    let p = &e.params;
    let voice = voice_of(p.freq_hz);
    let voices = choices(
        e,
        cx,
        "deesser-voice",
        vec![
            (
                "Female",
                voice == Voice::Female,
                Some(("freqHz", FEMALE_HZ)),
            ),
            ("Male", voice == Voice::Male, Some(("freqHz", MALE_HZ))),
            // Custom names the state a dragged frequency leaves; it sets
            // nothing of its own.
            ("Custom", voice == Voice::Custom, None),
        ],
        PillLook::words(30.0, 0.0, 12.5),
        s,
    );
    let modes = choices(
        e,
        cx,
        "deesser-mode",
        vec![
            (
                "Split band",
                p.mode == Mode::Split,
                Some(("mode", Mode::Split.to_wire())),
            ),
            (
                "Wideband",
                p.mode == Mode::Wide,
                Some(("mode", Mode::Wide.to_wire())),
            ),
        ],
        PillLook::words(30.0, 0.0, 12.5),
        s,
    );
    let help = match p.mode {
        Mode::Split => "Turns down only the detected band. Most transparent.",
        Mode::Wide => {
            "Turns down the whole signal when sibilance hits. More natural on bright voices."
        }
    };
    let look = KnobLook::small(56.0);
    let cell = KnobCell::new(92.0, 10.5, 12.0);
    card(s)
        .child(card_title("Detection", s))
        .child(section("Voice", s).child(voices))
        .child(
            section("Mode", s).child(modes).child(
                div()
                    .text_size(type_px(11.0, s))
                    .line_height(px(14.0 * s))
                    .text_color(color(SOFT))
                    .child(help),
            ),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .justify_between()
                .child(dial(
                    cx,
                    FREQ,
                    p.freq_hz,
                    Some(fmt_band(p.freq_hz)),
                    look,
                    cell,
                    accent(),
                    s,
                ))
                .child(dial(
                    cx,
                    WIDTH,
                    p.width_oct,
                    Some(format!("{:.1} oct", p.width_oct)),
                    look,
                    cell,
                    accent(),
                    s,
                ))
                .child(dial(
                    cx,
                    THRESHOLD,
                    p.threshold_db,
                    None,
                    look,
                    cell,
                    accent(),
                    s,
                )),
        )
}

fn reduction_card(e: &KitEditor<DeEsserPlate>, cx: &mut Cx<DeEsserPlate>, s: f32) -> gpui::Div {
    let p = &e.params;
    let look = KnobLook::small(68.0).styled(3.6, 17.5, FACE, FACE_EDGE, 0.8, 7.0, 1.8);
    let cell = KnobCell::new(92.0, 10.5, 12.5);
    let meter = div()
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
                        .w(px(140.0 * s))
                        .h(px(14.0 * s))
                        .child(live_slot(&e.live_bounds, SLOT_GR_PEAK).size_full()),
                ),
        )
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
    card(s)
        .child(card_title("Reduction", s))
        .child(
            div()
                .flex()
                .flex_row()
                .justify_between()
                .child(dial(
                    cx,
                    RANGE,
                    p.range_db,
                    Some(format!("\u{2212}{:.1} dB", p.range_db)),
                    look,
                    cell,
                    accent(),
                    s,
                ))
                .child(dial(
                    cx,
                    RELEASE,
                    p.release_ms,
                    None,
                    look,
                    cell,
                    accent(),
                    s,
                ))
                .child(dial(
                    cx,
                    LOOKAHEAD,
                    p.lookahead_ms,
                    None,
                    look,
                    cell,
                    accent(),
                    s,
                )),
        )
        .child(meter)
}

fn output_card(e: &KitEditor<DeEsserPlate>, cx: &mut Cx<DeEsserPlate>, s: f32) -> gpui::Div {
    let p = &e.params;
    let stereo = choices(
        e,
        cx,
        "deesser-stereo",
        [
            (StereoMode::Stereo, "Stereo"),
            (StereoMode::Mid, "Mid"),
            (StereoMode::Side, "Side"),
        ]
        .into_iter()
        .map(|(mode, label)| {
            (
                label,
                p.stereo_mode == mode,
                Some(("stereoMode", mode.to_wire())),
            )
        })
        .collect(),
        PillLook::words(28.0, 0.0, 12.0),
        s,
    );
    let look = KnobLook::small(50.0).styled(4.5, 16.0, FACE, FACE_EDGE, 1.2, 7.0, 2.4);
    let cell = KnobCell::new(72.0, 10.0, 11.5);
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
        .child(section("Process", s).child(stereo))
        .child(
            div()
                .flex()
                .flex_row()
                .justify_around()
                .child(dial(cx, MIX, p.mix, None, look, cell, color(INK), s))
                .child(dial(
                    cx,
                    OUTPUT,
                    p.output_db,
                    None,
                    look,
                    cell,
                    color(INK),
                    s,
                )),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(6.0 * s))
                .child(meter(0, "In"))
                .child(meter(1, "Out")),
        )
}

/// Opens with a track's De-Esser insert.
pub fn open_deesser_editor(
    owner_bounds: Option<gpui::Bounds<gpui::Pixels>>,
    key: PluginInstanceKey,
    identity: ShellIdentity,
    host_ops: BuiltinEditorHostOps,
    on_close: Arc<dyn Fn(&mut Window, &mut App)>,
    cx: &mut App,
) -> Result<gpui::WindowHandle<DeEsserWindow>, String> {
    plate::ensure_fonts(cx);
    open_kit_editor(
        DeEsserPlate,
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
    fn the_axis_maps_round_trip() {
        assert!(x_of(20.0).abs() < 1.0e-3);
        assert!((x_of(20_000.0) - SW).abs() < 1.0e-2);
        assert!((hz_at(x_of(7_500.0)) - 7_500.0).abs() < 1.0);
    }

    #[test]
    fn voices_read_from_the_frequency() {
        assert_eq!(voice_of(7_500.0), Voice::Female);
        assert_eq!(voice_of(5_500.0), Voice::Male);
        assert_eq!(voice_of(6_200.0), Voice::Custom);
    }

    #[test]
    fn the_drawn_cut_matches_the_dsp() {
        let params = deesser::default_params();
        // Fully cut at the centre: x − bp(x) is a null there.
        assert!(cut_at(&params, params.freq_hz, 0.5, 48_000.0) < -5.9);
        assert!(cut_at(&params, 200.0, 0.5, 48_000.0) > -0.1);
        let wide = Params {
            mode: Mode::Wide,
            ..params
        };
        assert!((cut_at(&wide, 200.0, 0.5, 48_000.0) + 6.02).abs() < 0.05);
    }
}

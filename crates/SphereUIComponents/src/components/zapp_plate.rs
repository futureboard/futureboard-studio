//! Zapp's editor, after its design: the transfer curve, a 220 Hz test tone
//! through it and that tone's harmonics in the well; Drive, Filter & Stereo
//! and Output cards on the deck.
//!
//! The curve, the tone and the harmonics are computed from the DSP's own
//! static curve (`zapp::saturate`), so they show the shaper that plays. The
//! tone follows the insert's input level when the host publishes it — the
//! dot on the curve sits at that level — and the meters and the auto-gain
//! readout are live from the meter frame and `zapp::telemetry`.

use std::f32::consts::PI;
use std::sync::Arc;

use gpui::{
    div, px, AnyElement, App, FontWeight, InteractiveElement, IntoElement, ParentElement, Rgba,
    StatefulInteractiveElement, Styled, Window,
};
use zapp::{Character, Oversampling, Params, StereoMode};

use crate::components::builtin_plugin_editor::builtin_zapp_params;
use crate::components::builtin_plugin_editor_window::{BuiltinEditorHostOps, PluginInstanceKey};
use crate::components::fx_model::KnobSpec;
use crate::components::native_plugin_shell::ShellIdentity;
use crate::components::plate::{
    self, caption, card, card_title, color, columns, deck, header, icon, knob, legend, live_slot,
    numbers, paint_dot, paint_polyline, paint_rect, paint_text, pill, pill_colors, plate_frame,
    plate_scale, signed, switch, type_px, well, words, Align, Dial, DialUnit, KnobCell, KnobLook,
    PillLook, DYN, FAINT, ICON, INK, SOFT, WELL, WHITE,
};
use crate::components::plugin_kit::{
    bank, open_kit_editor, Cx, KitEditor, KitModel, KitPreset, KitWindow, LiveBounds, LiveSlot,
};
use crate::components::plugin_live::Live;

/// Zapp's accent.
pub const ACCENT: u32 = 0xffd43b;

const SLOT_TRANSFER: usize = 0;
const SLOT_SCOPE: usize = 1;
const SLOT_HARMONICS: usize = 2;
const SLOT_THD: usize = 3;
const SLOT_AUTO: usize = 4;
const SLOT_METER: usize = 5;

/// The test tone's samples per cycle, and the harmonics shown.
const TONE_SAMPLES: usize = 256;
const HARMONICS: usize = 10;
const TONE_HZ: f32 = 220.0;
/// The tone's level with no input to follow.
const NOMINAL_LEVEL: f32 = 0.7;

const DRIVE: Dial = Dial::new("driveDb", "DRIVE", 0.0, 30.0, 9.0, DialUnit::Db);
const BIAS: Dial = Dial::new("bias", "BIAS", -1.0, 1.0, 0.0, DialUnit::Bias).bipolar();
const TONE: Dial = Dial::new("toneDb", "TONE", -12.0, 12.0, 0.0, DialUnit::Db).bipolar();
const LOW_CUT: Dial = Dial::new(
    "lowCutHz",
    "LOW CUT",
    20.0,
    1_000.0,
    20.0,
    DialUnit::HzRound,
)
.log();
const HIGH_CUT: Dial = Dial::new(
    "highCutHz",
    "HIGH CUT",
    1_000.0,
    20_000.0,
    20_000.0,
    DialUnit::HzRound,
)
.log();
const MIX: Dial = Dial::new("mix", "MIX", 0.0, 100.0, 100.0, DialUnit::Percent);
const OUTPUT: Dial = Dial::new("outputDb", "OUTPUT", -18.0, 18.0, 0.0, DialUnit::Db).bipolar();

/// Each character's icon, in an 18 × 14 box.
fn character_icon(character: Character) -> &'static str {
    match character {
        Character::Tape => "M1 12 C5 12 6 2 9 2 C12 2 13 2 17 2",
        Character::Tube => "M1 11 C4 11 6 3 9 3 C12 3 13 4 17 4",
        Character::Transistor => "M1 12 L4 12 L14 2 L17 2",
        Character::Diode => "M1 9 L6 9 L10 2 L17 2",
        Character::Fold => "M1 12 C3 12 4 2 7 2 C9 2 9 9 11 9 C13 9 14 2 17 2",
        Character::Crush => "M1 12 H4 V9 H7 V6 H10 V4 H13 V2 H17",
    }
}

const STEREO: [(StereoMode, &str); 3] = [
    (StereoMode::Stereo, "Stereo"),
    (StereoMode::Mid, "Mid"),
    (StereoMode::Side, "Side"),
];
const OVERSAMPLING: [(Oversampling, &str); 4] = [
    (Oversampling::Off, "Off"),
    (Oversampling::X2, "2x"),
    (Oversampling::X4, "4x"),
    (Oversampling::X8, "8x"),
];

fn accent() -> Rgba {
    color(ACCENT)
}

fn db20(v: f32) -> f32 {
    20.0 * v.max(1.0e-9).log10()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ZappPlate;

/// A Zapp insert's editor window.
pub type ZappWindow = KitWindow<ZappPlate>;

impl KitModel for ZappPlate {
    type Params = Params;
    type Ui = ();
    type LiveView = Params;

    fn title(self) -> &'static str {
        "Zapp"
    }

    fn subtitle(self, _params: &Params) -> String {
        "SATURATION".into()
    }

    fn key(self) -> &'static str {
        "zapp-plate"
    }

    fn defaults(self) -> Params {
        zapp::default_params()
    }

    fn from_mirror(self, insert_id: &str) -> Option<Params> {
        builtin_zapp_params(insert_id)
    }

    fn wire_values(self, params: &Params) -> Vec<(u32, f32)> {
        zapp::ipc::ui_values(params)
            .into_iter()
            .filter_map(|(id, value)| zapp::ui_param_index(id).map(|index| (index, value)))
            .collect()
    }

    fn with(self, params: &Params, id: &str, value: f32) -> Params {
        let mut next = params.clone();
        zapp::ipc::apply_ui_param(&mut next, id, value);
        next
    }

    fn value(self, params: &Params, id: &str) -> f32 {
        zapp::ipc::ui_values(params)
            .into_iter()
            .find(|(candidate, _)| *candidate == id)
            .map_or(0.0, |(_, value)| value)
    }

    fn presets(self) -> Arc<Vec<KitPreset<Params>>> {
        static BANK: std::sync::OnceLock<Arc<Vec<KitPreset<Params>>>> = std::sync::OnceLock::new();
        BANK.get_or_init(|| bank(zapp::factory_presets(), |p| (p.name, p.params)))
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
        let input = live.frame.map(|frame| frame.in_peak.clamp(0.0, 1.25));
        let level = input.filter(|peak| *peak > 0.01).unwrap_or(NOMINAL_LEVEL);
        let auto_db = live
            .pads()
            .map(|block| block[zapp::telemetry::AUTO_GAIN_DB])
            .filter(|_| view.auto_gain && view.power)
            .unwrap_or(0.0);
        let out_gain = 10f32.powf((view.output_db + auto_db) / 20.0);
        let tone = Tone::new(view, level);

        if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_TRANSFER)) {
            paint_transfer(window, view, input, out_gain, b);
        }
        if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_SCOPE)) {
            paint_scope(window, view, level, out_gain, b);
        }
        if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_HARMONICS)) {
            paint_harmonics(window, view, &tone, b, s);
        }
        let mono = plate::mono(FontWeight::NORMAL);
        if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_THD)) {
            let thd = tone.thd_percent();
            let text = if thd < 10.0 {
                format!("{thd:.2}%")
            } else {
                format!("{thd:.1}%")
            };
            let (x, y) = plate::xy(b.origin);
            paint_text(
                window,
                cx,
                &text,
                mono.clone(),
                12.0 * s,
                color(WHITE),
                (x, y),
                Align::Left,
            );
        }
        if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_AUTO)) {
            if view.auto_gain && live.pads().is_some() {
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
                let Some(b) = bounds.get(LiveSlot::Plate(SLOT_METER + index)) else {
                    continue;
                };
                paint_meter(window, cx, b, db20(peak), s);
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

// ── The test tone ──────────────────────────────────────────────────────────

/// One cycle of the test tone at `level` through the shaper, and its first
/// [`HARMONICS`] harmonic amplitudes.
struct Tone {
    harmonics: [f32; HARMONICS],
}

impl Tone {
    fn new(params: &Params, level: f32) -> Self {
        let cycle: Vec<f32> = (0..TONE_SAMPLES)
            .map(|n| {
                zapp::saturate(
                    params,
                    level * (2.0 * PI * n as f32 / TONE_SAMPLES as f32).sin(),
                )
            })
            .collect();
        let mut harmonics = [0.0f32; HARMONICS];
        for (h, magnitude) in harmonics.iter_mut().enumerate() {
            let (mut re, mut im) = (0.0f32, 0.0f32);
            for (n, y) in cycle.iter().enumerate() {
                let phase = 2.0 * PI * (h + 1) as f32 * n as f32 / TONE_SAMPLES as f32;
                re += y * phase.cos();
                im += y * phase.sin();
            }
            *magnitude = 2.0 * (re * re + im * im).sqrt() / TONE_SAMPLES as f32;
        }
        Self { harmonics }
    }

    fn thd_percent(&self) -> f32 {
        let fundamental = self.harmonics[0].max(1.0e-9);
        let rest: f32 = self.harmonics[1..].iter().map(|m| m * m).sum();
        rest.sqrt() / fundamental * 100.0
    }

    /// Harmonic `n`'s level relative to the fundamental, through the tone
    /// tilt and the high cut that follow the shaper.
    fn relative_db(&self, params: &Params, n: usize) -> f32 {
        let fundamental = self.harmonics[0].max(1.0e-9);
        let hz = n as f32 * TONE_HZ;
        let mut db = db20(self.harmonics[n - 1] / fundamental)
            + params.tone_db * (n as f32).log2() / 10f32.log2();
        if hz > params.high_cut_hz {
            db -= 12.0 * (hz / params.high_cut_hz).log2();
        }
        db
    }
}

// ── The well ───────────────────────────────────────────────────────────────

fn display(e: &KitEditor<ZappPlate>, cx: &mut Cx<ZappPlate>, s: f32) -> AnyElement {
    let thd = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0 * s))
        .child(numbers("THD", 12.0, SOFT, s))
        .child(
            div()
                .w(px(48.0 * s))
                .h(px(16.0 * s))
                .child(live_slot(&e.live_bounds, SLOT_THD).size_full()),
        )
        .into_any_element();

    let frame_box = |left: f32, w: f32| {
        div()
            .absolute()
            .left(px(left * s))
            .top(px(4.0 * s))
            .w(px(w * s))
            .h(px(250.0 * s))
            .border(px(1.0))
            .border_color(color(0x262626))
            .overflow_hidden()
    };
    let tag = |text: &str| {
        div()
            .absolute()
            .left(px(8.0 * s))
            .top(px(6.0 * s))
            .child(plate::over_live(caption(text, 10.5, 0.1, FAINT, s)))
    };
    let transfer = frame_box(16.0, 250.0)
        .child(
            live_slot(&e.live_bounds, SLOT_TRANSFER)
                .absolute()
                .size_full(),
        )
        .child(tag("Transfer"))
        .child(
            div()
                .absolute()
                .right(px(8.0 * s))
                .bottom(px(6.0 * s))
                .child(plate::over_live(numbers("in \u{2192}", 10.0, FAINT, s))),
        );
    let dashed = div().w(px(12.0 * s)).h(px(2.0 * s)).child(
        gpui::canvas(
            |_, _, _| (),
            move |b, _, window, _| {
                let (x, y) = plate::xy(b.origin);
                paint_polyline(
                    window,
                    &[(x, y + 0.75 * s), (x + 12.0 * s, y + 0.75 * s)],
                    1.5 * s,
                    color(FAINT),
                    Some((3.0 * s, 2.0 * s)),
                );
            },
        )
        .size_full(),
    );
    let scope = frame_box(282.0, 456.0)
        .child(live_slot(&e.live_bounds, SLOT_SCOPE).absolute().size_full())
        .child(
            div()
                .absolute()
                .left(px(8.0 * s))
                .top(px(6.0 * s))
                .child(plate::over_live(
                    words(
                        plate::spaced(
                            &format!("WAVEFORM \u{00B7} {} Hz test tone", TONE_HZ as u32),
                            0.1,
                        ),
                        10.5,
                        FAINT,
                        s,
                    )
                    .font_weight(FontWeight::MEDIUM),
                )),
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
                        .child(legend(dashed, "Input", s))
                        .child(legend(
                            div().w(px(12.0 * s)).h(px(2.0 * s)).bg(accent()),
                            "Output",
                            s,
                        )),
                )),
        );
    let harmonics = frame_box(754.0, 240.0)
        .child(
            live_slot(&e.live_bounds, SLOT_HARMONICS)
                .absolute()
                .size_full(),
        )
        .child(tag("Harmonics"));
    let swatch = |ink: Rgba| div().size(px(10.0 * s)).rounded(px(2.0 * s)).bg(ink);
    let harmonics_legend = div()
        .absolute()
        .left(px(754.0 * s))
        .top(px(258.0 * s))
        .w(px(240.0 * s))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(14.0 * s))
        .child(legend(swatch(accent()), "Odd", s))
        .child(legend(swatch(color(DYN)), "Even", s))
        .child(div().flex_1())
        .child(numbers("0 to \u{2212}90 dB", 10.0, FAINT, s));

    well(s, 7.0, 327.0)
        .child(header(e, cx, s, "SATURATION", accent(), vec![thd]))
        .child(
            div()
                .relative()
                .flex_1()
                .child(transfer)
                .child(scope)
                .child(harmonics)
                .child(harmonics_legend),
        )
        .into_any_element()
}

fn paint_transfer(
    window: &mut Window,
    params: &Params,
    input: Option<f32>,
    out_gain: f32,
    b: gpui::Bounds<gpui::Pixels>,
) {
    // The box's 250-unit view, inside its 1 px border.
    let k = (f32::from(b.size.width) / 250.0).max(0.01);
    let (x0, y0) = plate::xy(b.origin);
    let at = |x: f32, y: f32| (x0 + x * k, y0 + y * k);
    let minor = color(0x1d1d1d);
    for (from, to) in [
        ((62.5, 0.0), (62.5, 250.0)),
        ((187.5, 0.0), (187.5, 250.0)),
        ((0.0, 75.0), (250.0, 75.0)),
        ((0.0, 175.0), (250.0, 175.0)),
    ] {
        paint_polyline(
            window,
            &[at(from.0, from.1), at(to.0, to.1)],
            1.0,
            minor,
            None,
        );
    }
    let major = color(0x2e2e2e);
    paint_polyline(
        window,
        &[at(125.0, 0.0), at(125.0, 250.0)],
        1.0,
        major,
        None,
    );
    paint_polyline(
        window,
        &[at(0.0, 125.0), at(250.0, 125.0)],
        1.0,
        major,
        None,
    );
    paint_polyline(
        window,
        &[at(0.0, 225.0), at(250.0, 25.0)],
        1.0,
        color(0x4a4a4a),
        Some((3.0 * k, 3.0 * k)),
    );
    let curve: Vec<(f32, f32)> = (0..=160)
        .map(|i| {
            let x = -1.0 + 2.0 * i as f32 / 160.0;
            let y = (zapp::saturate(params, x) * out_gain).clamp(-1.25, 1.25);
            at((x + 1.0) * 125.0, 125.0 - y * 100.0)
        })
        .collect();
    paint_polyline(window, &curve, 2.0 * k, accent(), None);
    if let Some(x) = input.filter(|peak| *peak > 0.0005) {
        let x = x.min(1.0);
        let y = (zapp::saturate(params, x) * out_gain).clamp(-1.25, 1.25);
        let (cx, cy) = at((x + 1.0) * 125.0, 125.0 - y * 100.0);
        paint_dot(window, cx, cy, 5.5 * k, accent());
        paint_dot(window, cx, cy, 3.5 * k, color(WELL));
    }
}

fn paint_scope(
    window: &mut Window,
    params: &Params,
    level: f32,
    out_gain: f32,
    b: gpui::Bounds<gpui::Pixels>,
) {
    let k = (f32::from(b.size.width) / 456.0).max(0.01);
    let (x0, y0) = plate::xy(b.origin);
    let at = |x: f32, y: f32| (x0 + x * k, y0 + y * k);
    let minor = color(0x1d1d1d);
    for x in [114.0, 228.0, 342.0] {
        paint_polyline(window, &[at(x, 0.0), at(x, 250.0)], 1.0, minor, None);
    }
    for y in [28.8, 221.2] {
        paint_polyline(window, &[at(0.0, y), at(456.0, y)], 1.0, minor, None);
    }
    paint_polyline(
        window,
        &[at(0.0, 125.0), at(456.0, 125.0)],
        1.0,
        color(0x2e2e2e),
        None,
    );
    let mut input = Vec::with_capacity(229);
    let mut output = Vec::with_capacity(229);
    for i in 0..=228 {
        let theta = i as f32 / 228.0 * 4.0 * PI;
        let x = level * theta.sin();
        let y = (zapp::saturate(params, x) * out_gain).clamp(-1.3, 1.3);
        input.push(at(i as f32 * 2.0, 125.0 - x * 96.0));
        output.push(at(i as f32 * 2.0, 125.0 - y * 96.0));
    }
    paint_polyline(
        window,
        &input,
        1.2 * k,
        color(0x6a6a6a),
        Some((4.0 * k, 3.0 * k)),
    );
    paint_polyline(window, &output, 2.0 * k, accent(), None);
}

fn paint_harmonics(
    window: &mut Window,
    params: &Params,
    tone: &Tone,
    b: gpui::Bounds<gpui::Pixels>,
    s: f32,
) {
    let at = plate::at(b, s);
    // Ten 14-wide bars spread across 12..228.
    let span = 240.0 - 24.0;
    let gap = (span - 14.0 * HARMONICS as f32) / (HARMONICS - 1) as f32;
    for n in 1..=HARMONICS {
        let x = 12.0 + (n - 1) as f32 * (14.0 + gap);
        let (bx, by) = at(x, 28.0);
        window.paint_quad(
            gpui::fill(plate::rect(bx, by, 14.0 * s, 190.0 * s), color(0x181818))
                .corner_radii(px(2.0 * s)),
        );
        let db = tone.relative_db(params, n);
        let h = ((db + 90.0) / 90.0).clamp(0.0, 1.0) * 190.0;
        if h > 0.5 {
            let ink = if n == 1 {
                color(INK)
            } else if n % 2 == 1 {
                accent()
            } else {
                color(DYN)
            };
            let (lx, ly) = at(x, 28.0 + 190.0 - h);
            window.paint_quad(
                gpui::fill(plate::rect(lx, ly, 14.0 * s, h * s), ink)
                    .corner_radii(px((2.0 * s).min(h * s * 0.5))),
            );
        }
    }
}

/// An Input / Output meter row's bar: −48 to +6 dB.
fn paint_meter(window: &mut Window, cx: &mut App, b: gpui::Bounds<gpui::Pixels>, db: f32, s: f32) {
    let (x, y) = plate::xy(b.origin);
    let w = f32::from(b.size.width);
    // The value sits right-aligned on the label row above the bar.
    paint_text(
        window,
        cx,
        &format!("{} dB", signed(db.max(-120.0))),
        plate::mono(FontWeight::NORMAL),
        10.5 * s,
        color(INK),
        (x + w, y - 17.0 * s),
        Align::Right,
    );
    window.paint_quad(
        gpui::fill(plate::rect(x, y, w, 8.0 * s), color(0x111111)).corner_radii(px(2.0 * s)),
    );
    let fraction = ((db + 48.0) / 54.0).clamp(0.0, 1.0);
    let ink = if db > -1.0 { accent() } else { color(0xd9d9d9) };
    paint_rect(window, (x, y, w * fraction, 8.0 * s), ink);
}

// ── The deck ───────────────────────────────────────────────────────────────

fn controls(e: &KitEditor<ZappPlate>, cx: &mut Cx<ZappPlate>, s: f32) -> AnyElement {
    deck(s, 341.0, 420.0)
        .p(px(14.0 * s))
        .gap(px(12.0 * s))
        .child(drive_card(e, cx, s).w(px(420.0 * s)).flex_shrink_0())
        .child(filter_card(e, cx, s).w(px(300.0 * s)).flex_shrink_0())
        .child(output_card(e, cx, s).flex_1())
        .into_any_element()
}

fn set(id: &'static str) -> impl Fn(&mut KitEditor<ZappPlate>, f32, &mut Cx<ZappPlate>) {
    move |this, value, cx| this.set_value(id, value, cx)
}

fn small_knob(cx: &mut Cx<ZappPlate>, dial: Dial, value: f32, ink: Rgba, s: f32) -> AnyElement {
    knob(
        cx,
        format!("zapp-{}", dial.id),
        dial,
        value,
        KnobLook::small(58.0),
        KnobCell::new(80.0, 10.5, 12.0),
        ink,
        true,
        s,
        set(dial.id),
    )
}

fn drive_card(e: &KitEditor<ZappPlate>, cx: &mut Cx<ZappPlate>, s: f32) -> gpui::Div {
    let current = e.params.character;
    let characters: Vec<AnyElement> = Character::ALL
        .iter()
        .map(|&character| {
            let on = character == current;
            let (bg, border, fg) = pill_colors(on, accent());
            div()
                .id(("zapp-character", character.to_wire() as usize))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0 * s))
                .h(px(34.0 * s))
                .px(px(10.0 * s))
                .rounded(px(3.0 * s))
                .border(px(1.0))
                .border_color(border)
                .bg(bg)
                .text_color(fg)
                .text_size(type_px(12.5, s))
                .cursor(gpui::CursorStyle::PointingHand)
                .hover(move |style| style.bg(color(if on { 0x222222 } else { 0x2e2e2e })))
                .child(icon(
                    character_icon(character),
                    (18.0, 14.0),
                    (18.0, 14.0),
                    1.5,
                    if on { accent() } else { color(ICON) },
                    s,
                ))
                .child(character.label())
                .on_click(e.click_cb(cx, move |this, cx| {
                    this.set_value("character", character.to_wire(), cx)
                }))
                .into_any_element()
        })
        .collect();
    let mut grid = div().flex().flex_col().gap(px(4.0 * s));
    let mut cells = characters.into_iter();
    for _ in 0..2 {
        grid = grid.child(columns(4.0, s, cells.by_ref().take(3).collect()));
    }

    let p = &e.params;
    let drive = knob(
        cx,
        "zapp-driveDb",
        DRIVE,
        p.drive_db,
        KnobLook::hero(112.0, 3.0, 1.5),
        KnobCell::new(140.0, 11.0, 14.0).gap(3.0).label(0.1, SOFT),
        accent(),
        true,
        s,
        set(DRIVE.id),
    );
    card(s)
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0 * s))
                .child(card_title("Drive", s))
                .child(words(current.label(), 13.0, SOFT, s)),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(6.0 * s))
                .child(caption("Character", 10.5, 0.1, SOFT, s))
                .child(grid),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .child(drive)
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(8.0 * s))
                        .child(small_knob(cx, BIAS, p.bias, accent(), s))
                        .child(small_knob(cx, TONE, p.tone_db, accent(), s)),
                ),
        )
}

fn choice_row<T: Copy + PartialEq + 'static>(
    e: &KitEditor<ZappPlate>,
    cx: &mut Cx<ZappPlate>,
    id: &'static str,
    param: &'static str,
    current: T,
    options: &[(T, &'static str)],
    wire: fn(T) -> f32,
    look: PillLook,
    s: f32,
) -> gpui::Div {
    let pills: Vec<AnyElement> = options
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
        .collect();
    columns(3.0, s, pills)
}

fn filter_card(e: &KitEditor<ZappPlate>, cx: &mut Cx<ZappPlate>, s: f32) -> gpui::Div {
    let p = &e.params;
    let stereo = choice_row(
        e,
        cx,
        "zapp-stereo",
        "stereoMode",
        p.stereo_mode,
        &STEREO,
        StereoMode::to_wire,
        PillLook::words(28.0, 0.0, 12.0),
        s,
    );
    let oversampling = choice_row(
        e,
        cx,
        "zapp-os",
        "oversampling",
        p.oversampling,
        &OVERSAMPLING,
        Oversampling::to_wire,
        PillLook::numbers(28.0, 0.0, 11.5),
        s,
    );
    card(s)
        .child(card_title("Filter & Stereo", s))
        .child(
            div()
                .flex()
                .flex_row()
                .justify_around()
                .child(small_knob(cx, LOW_CUT, p.low_cut_hz, color(INK), s))
                .child(small_knob(cx, HIGH_CUT, p.high_cut_hz, color(INK), s)),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(6.0 * s))
                .child(caption("Saturate", 10.5, 0.1, SOFT, s))
                .child(stereo),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(6.0 * s))
                .child(caption("Oversampling", 10.5, 0.1, SOFT, s))
                .child(oversampling),
        )
}

fn output_card(e: &KitEditor<ZappPlate>, cx: &mut Cx<ZappPlate>, s: f32) -> gpui::Div {
    let p = &e.params;
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
            "zapp-auto",
            p.auto_gain,
            accent(),
            true,
            s,
            e.click_cb(cx, |this, cx| this.toggle("autoGain", cx)),
        ));
    let meter = |index: usize, label: &str| {
        div()
            .flex()
            .flex_col()
            .gap(px(4.0 * s))
            .child(caption(label, 10.5, 0.1, SOFT, s).h(px(13.0 * s)))
            .child(
                div()
                    .h(px(8.0 * s))
                    .w_full()
                    .child(live_slot(&e.live_bounds, SLOT_METER + index).size_full()),
            )
    };
    card(s)
        .child(card_title("Output", s))
        .child(
            div()
                .flex()
                .flex_row()
                .justify_around()
                .child(small_knob(cx, MIX, p.mix, color(INK), s))
                .child(small_knob(cx, OUTPUT, p.output_db, color(INK), s)),
        )
        .child(auto)
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(8.0 * s))
                .child(meter(0, "Input"))
                .child(meter(1, "Output")),
        )
}

/// Opens with a track's Zapp insert.
pub fn open_zapp_editor(
    owner_bounds: Option<gpui::Bounds<gpui::Pixels>>,
    key: PluginInstanceKey,
    identity: ShellIdentity,
    host_ops: BuiltinEditorHostOps,
    on_close: Arc<dyn Fn(&mut Window, &mut App)>,
    cx: &mut App,
) -> Result<gpui::WindowHandle<ZappWindow>, String> {
    plate::ensure_fonts(cx);
    open_kit_editor(
        ZappPlate,
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
    fn a_clean_curve_has_no_harmonics_and_drive_adds_them() {
        let mut params = zapp::default_params();
        params.drive_db = 0.0;
        params.character = Character::Fold;
        let quiet = Tone::new(&params, 0.01);
        assert!(quiet.thd_percent() < 1.0);
        params.character = Character::Tape;
        params.drive_db = 24.0;
        let driven = Tone::new(&params, 0.7);
        assert!(driven.thd_percent() > 10.0);
    }

    #[test]
    fn bias_brings_out_even_harmonics() {
        let mut params = zapp::default_params();
        params.drive_db = 12.0;
        params.bias = 0.0;
        let even_free = Tone::new(&params, 0.7);
        params.bias = 0.6;
        let biased = Tone::new(&params, 0.7);
        assert!(biased.harmonics[1] > even_free.harmonics[1] * 10.0 + 1.0e-4);
    }
}

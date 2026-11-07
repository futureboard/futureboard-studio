//! The Imager's editor, after its design ("Imager — main window"): the
//! vectorscope, correlation and balance down the left, the four bands' widths
//! over the mid and side spectra beside them, and a card per band with the
//! global card along the bottom.
//!
//! The design is laid out at [`DESIGN`] and scaled to the window by one
//! factor, on its own black ground, in Mona Sans. Every control is an Imager
//! param through [`KitEditor`]: a band's width and stereoize, its solo and
//! bypass, the global width offset, Mono Bass and its crossover, the
//! crossovers themselves, Multiband, Recover Sides, the stereoize character
//! and the output trim. The one editor setting is which page the band cards
//! show — width or stereoize — flipped by the global card's Stereoize.
//!
//! What moves — the scope, the correlation and balance, the spectra — is
//! painted by the kit window's overlay into [`LiveSlot::Plate`] bounds; the
//! band display is painted there whole so the width bars sit over the
//! spectra the way the design layers them.

use std::sync::Arc;

use gpui::{
    canvas, div, fill, px, AnyElement, App, Bounds, FontWeight, InteractiveElement, IntoElement,
    MouseButton, MouseDownEvent, MouseMoveEvent, ParentElement, Pixels, Rgba,
    StatefulInteractiveElement, Styled, Window,
};
use imager::Params;

use crate::components::band_model::{crossover_bounds, CROSSOVER_IDS};
use crate::components::builtin_plugin_editor::builtin_imager_params;
use crate::components::builtin_plugin_editor_window::{BuiltinEditorHostOps, PluginInstanceKey};
use crate::components::eq_graph::{freq_at_fraction, freq_fraction};
use crate::components::fx_model::KnobSpec;
use crate::components::native_plugin_shell::ShellIdentity;
use crate::components::plate::{
    self, color, color_a, icon, paint_dot, paint_polygon, paint_polyline, paint_rect, paint_text,
    plate_frame_sized, plate_scale_for, preset_menu, rect, tracked, type_px, Align, CHEVRON_DOWN,
    REDO, UNDO,
};
use crate::components::plugin_kit::{
    bank, open_kit_editor, Cx, KitEditor, KitModel, KitPreset, KitWindow, LiveBounds, LiveSlot,
};
use crate::components::plugin_live::Live;

/// The design's artboard.
pub const DESIGN: (f32, f32) = (1_140.0, 699.0);
pub const WINDOW_SIZE: (f32, f32) = (DESIGN.0, DESIGN.1 + plate::SHELL_CHROME_H);
pub const MIN_WINDOW_SIZE: (f32, f32) = (800.0, 490.0 + plate::SHELL_CHROME_H);

const BANDS: usize = imager::BAND_COUNT;
const BAND_NAMES: [&str; BANDS] = ["Low", "Low Mid", "High Mid", "High"];

// ── Palette, as the design sets it ─────────────────────────────────────────

const GROUND: u32 = 0x000000;
const PANEL: u32 = 0x141414;
const CARD: u32 = 0x0e0e0e;
const EDGE: u32 = 0x2a2a2a;
const BUTTON: u32 = 0x111111;
const BUTTON_EDGE: u32 = 0x2e2e2e;
const HOVER: u32 = 0x1c1c1c;
const WHITE: u32 = 0xffffff;
const INK: u32 = 0xededed;
const LABEL: u32 = 0xa3a3a3;
const QUIET: u32 = 0x9a9a9a;
const DIM: u32 = 0x8c8c8c;
const ON_LIGHT: u32 = 0x0a0a0a;
const WELL: u32 = 0x0a0a0a;
const MID_LINE: u32 = 0xcfcfcf;
/// The design's accent, and the warm tone for out of phase and bypass.
const ACCENT: u32 = 0x9cc9ff;
const WARM: u32 = 0xffb089;
/// Each band's colour, lowest first.
const BAND_COLORS: [u32; BANDS] = [0xffb089, 0xffe08a, 0x9ee6c1, 0xb9b2ff];

// ── Live displays ──────────────────────────────────────────────────────────

const SLOT_SCOPE: usize = 0;
const SLOT_CORRELATION: usize = 1;
const SLOT_CORRELATION_VALUE: usize = 2;
const SLOT_BALANCE: usize = 3;
const SLOT_BALANCE_VALUE: usize = 4;
const SLOT_BANDS: usize = 5;
/// A band card's slider, by band; the global card's after them.
const SLOT_SLIDER: usize = 10;
const SLOT_GLOBAL_SLIDER: usize = SLOT_SLIDER + BANDS;

/// The band display at the design size: the plot, its centre line and how
/// far a full width swings from it.
const PLOT_H: f32 = 320.0;
const DISPLAY_H: f32 = 344.0;
const CENTRE_Y: f32 = 160.0;
const SWING: f32 = 1.3;
/// A width bar's inset from its band's edges.
const BAR_INSET: f32 = 14.0;
/// How near a press must land to a crossover to take it.
const CROSSOVER_HIT: f32 = 8.0;
/// The spectra's range on the display: 0 dBFS at the top grid line, this
/// at the bottom of the plot.
const SPECTRUM_FLOOR_DB: f32 = -84.0;
/// The frequencies the display labels and draws a grid line at.
const FREQ_TICKS: [(f32, &str); 8] = [
    (50.0, "50"),
    (100.0, "100"),
    (200.0, "200"),
    (500.0, "500"),
    (1_000.0, "1k"),
    (2_000.0, "2k"),
    (5_000.0, "5k"),
    (10_000.0, "10k"),
];

/// A slider's thumb radius and track height, at the design size.
const THUMB_R: f32 = 7.0;
const TRACK_H: f32 = 4.0;
/// Vertical drag travel: an octave of Mono Bass crossover, and 12 dB of
/// output, per this many design pixels.
const DRAG_SPAN: f32 = 100.0;
/// Drag travel under Shift.
const FINE: f32 = 0.2;

fn ink(hex: u32) -> Rgba {
    color(hex)
}

// ── State ──────────────────────────────────────────────────────────────────

/// What the band cards' sliders set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Page {
    #[default]
    Width,
    Stereoize,
}

/// A slider by what it sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slider {
    Band(usize),
    Global,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Drag {
    Crossover(usize),
    /// A band's width dragged up and down on the display, from where the
    /// press began and the width then.
    Width {
        band: usize,
        origin_y: f32,
        start: f32,
    },
    Slider(Slider),
    MonoHz {
        origin_y: f32,
        start: f32,
    },
    Output {
        origin_y: f32,
        start: f32,
    },
}

#[derive(Debug, Default)]
pub struct ImagerUi {
    /// The band the display and the cards pick out.
    pub selected: usize,
    pub page: Page,
    pub drag: Option<Drag>,
}

/// What the overlay paints from.
pub struct ImagerView {
    params: Params,
    selected: usize,
    crossover: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ImagerPlate;

/// An Imager insert's editor window.
pub type ImagerWindow = KitWindow<ImagerPlate>;

impl KitModel for ImagerPlate {
    type Params = Params;
    type Ui = ImagerUi;
    type LiveView = ImagerView;

    fn title(self) -> &'static str {
        "Imager"
    }

    fn subtitle(self, params: &Params) -> String {
        if params.multiband {
            "Four-band stereo imaging".into()
        } else {
            "Stereo imaging".into()
        }
    }

    fn key(self) -> &'static str {
        "imager-plate"
    }

    fn defaults(self) -> Params {
        imager::default_params()
    }

    fn from_mirror(self, insert_id: &str) -> Option<Params> {
        builtin_imager_params(insert_id)
    }

    fn wire_values(self, params: &Params) -> Vec<(u32, f32)> {
        imager::ipc::ui_values(params)
            .into_iter()
            .filter_map(|(id, value)| imager::ui_param_index(id).map(|index| (index, value)))
            .collect()
    }

    fn with(self, params: &Params, id: &str, value: f32) -> Params {
        let mut next = params.clone();
        imager::ipc::apply_ui_param(&mut next, id, value);
        next
    }

    fn value(self, params: &Params, id: &str) -> f32 {
        imager::ipc::ui_values(params)
            .into_iter()
            .find(|(candidate, _)| *candidate == id)
            .map_or(0.0, |(_, value)| value)
    }

    fn presets(self) -> Arc<Vec<KitPreset<Params>>> {
        static BANK: std::sync::OnceLock<Arc<Vec<KitPreset<Params>>>> = std::sync::OnceLock::new();
        BANK.get_or_init(|| bank(imager::factory_presets(), |p| (p.name, p.params)))
            .clone()
    }

    /// Power and the soloed band stay as they are: a solo is listening, not
    /// sound.
    fn preset_applied(self, current: &Params, preset: &Params) -> Params {
        let mut next = preset.clone();
        next.power = current.power;
        next.solo_band = current.solo_band;
        next
    }

    fn knob(self, _id: &str) -> Option<KnobSpec> {
        None
    }

    fn panel(e: &KitEditor<Self>, cx: &mut Cx<Self>) -> AnyElement {
        let s = plate_scale_for(e.viewport, DESIGN);
        let body = div()
            .flex()
            .flex_col()
            .size_full()
            .px(px(12.0 * s))
            .pb(px(12.0 * s))
            .child(header(e, cx, s))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .h(px(402.0 * s))
                    .flex_shrink_0()
                    .gap(px(8.0 * s))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .w(px(310.0 * s))
                            .flex_shrink_0()
                            .gap(px(8.0 * s))
                            .child(scope_panel(e, s))
                            .child(image_panel(e, cx, s)),
                    )
                    .child(band_display(e, cx, s)),
            )
            .child(strip(e, cx, s));
        plate_frame_sized(
            s,
            DESIGN,
            GROUND,
            plate::mona_sans(FontWeight::NORMAL),
            vec![body.into_any_element()],
        )
    }

    fn live_view(e: &KitEditor<Self>) -> ImagerView {
        ImagerView {
            params: e.params.clone(),
            selected: e.ui.selected,
            crossover: match e.ui.drag {
                Some(Drag::Crossover(index)) => Some(index),
                _ => None,
            },
        }
    }

    fn paint_live(
        view: &ImagerView,
        live: &mut Live,
        bounds: &LiveBounds,
        window: &mut Window,
        cx: &mut App,
    ) {
        let viewport = window.viewport_size();
        let s = plate_scale_for(
            (f32::from(viewport.width), f32::from(viewport.height)),
            DESIGN,
        );
        if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_BANDS)) {
            paint_bands(window, cx, b, view, live, s);
        }
        if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_SCOPE)) {
            paint_scope(window, cx, b, live, s);
        }
        let audible = live.frame.is_some_and(|f| f.out_rms > 1.0e-4);
        let correlation = live
            .image()
            .filter(|_| audible)
            .map(|image| image.correlation);
        if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_CORRELATION)) {
            paint_correlation(window, b, correlation, s);
        }
        if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_CORRELATION_VALUE)) {
            let text = correlation.map_or_else(|| "\u{2014}".to_string(), correlation_text);
            paint_reading(window, cx, b, &text, s);
        }
        let balance = live.image().filter(|_| audible).and_then(balance_of);
        if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_BALANCE)) {
            paint_balance(window, b, balance, s);
        }
        if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_BALANCE_VALUE)) {
            let text = balance.map_or_else(|| "\u{2014}".to_string(), balance_text);
            paint_reading(window, cx, b, &text, s);
        }
    }

    fn window_size(self) -> (f32, f32) {
        WINDOW_SIZE
    }

    fn min_size(self) -> (f32, f32) {
        MIN_WINDOW_SIZE
    }

    /// A soloed band is let go: a closed editor never leaves a track playing
    /// one band.
    fn on_close(self, params: &Params) -> Option<Params> {
        (params.solo_band >= 0).then(|| {
            let mut next = params.clone();
            next.solo_band = imager::SOLO_NONE;
            next
        })
    }

    fn key_down(e: &mut KitEditor<Self>, key: &str, cx: &mut Cx<Self>) -> bool {
        if key == "escape" && e.params.solo_band >= 0 {
            e.set_value("soloBand", imager::SOLO_NONE as f32, cx);
            return true;
        }
        false
    }

    fn mouse_move(e: &mut KitEditor<Self>, event: &MouseMoveEvent, cx: &mut Cx<Self>) {
        drag_to(e, event, cx);
    }

    fn mouse_up(e: &mut KitEditor<Self>, cx: &mut Cx<Self>) {
        if e.ui.drag.take().is_some() {
            cx.notify();
        }
    }
}

// ── Reading the params ─────────────────────────────────────────────────────

/// The band edges, low to high: the crossovers sorted, as the DSP sorts
/// them.
fn edges(p: &Params) -> [f32; BANDS + 1] {
    let mut sorted = p.crossover_hz;
    sorted.sort_by(f32::total_cmp);
    [20.0, sorted[0], sorted[1], sorted[2], 20_000.0]
}

/// The bands the display draws: all four, or one for the whole signal.
fn shown(p: &Params) -> usize {
    if p.multiband {
        BANDS
    } else {
        1
    }
}

/// Where band `band` runs on the display.
fn span(p: &Params, band: usize) -> (f32, f32) {
    if p.multiband {
        let edges = edges(p);
        (edges[band], edges[band + 1])
    } else {
        (20.0, 20_000.0)
    }
}

/// A band's own width as the design reads it: −100 folds it to mono, 0
/// leaves it as it came, +100 doubles its side.
fn relative_width(p: &Params, band: usize) -> f32 {
    p.width[band] - imager::DEFAULT_WIDTH
}

/// The width a band plays at, on the same scale: its own plus the global
/// offset, or 0 while bypassed.
fn played_width(p: &Params, band: usize) -> f32 {
    if p.band_bypass[band] {
        0.0
    } else {
        (relative_width(p, band) + p.global_width).clamp(-100.0, 100.0)
    }
}

fn soloed(p: &Params) -> Option<usize> {
    (p.multiband && p.solo_band >= 0).then_some(p.solo_band as usize)
}

/// `+35%`, `−60%`, `0%`.
fn percent(v: f32) -> String {
    let v = v.round();
    if v > 0.0 {
        format!("+{v:.0}%")
    } else if v < 0.0 {
        format!("\u{2212}{:.0}%", -v)
    } else {
        "0%".to_string()
    }
}

/// `120`, `1k`, `1.5k`, `12k`: a frequency for a tick.
fn hz_short(hz: f32) -> String {
    if hz < 1_000.0 {
        format!("{:.0}", hz.round())
    } else {
        let k = (hz / 10.0).round() / 100.0;
        let text = format!("{k:.2}");
        let text = text.trim_end_matches('0').trim_end_matches('.');
        format!("{text}k")
    }
}

/// `120 Hz`, `1 kHz`, `1.5 kHz`.
fn hz_label(hz: f32) -> String {
    let short = hz_short(hz);
    match short.strip_suffix('k') {
        Some(k) => format!("{k} kHz"),
        None => format!("{short} Hz"),
    }
}

/// A band's range, as its card heads it: `120 Hz – 1 kHz`.
fn range_label(p: &Params, band: usize) -> String {
    let (low, high) = span(p, band);
    format!("{} \u{2013} {}", hz_label(low), hz_label(high))
}

fn correlation_text(value: f32) -> String {
    let sign = if value < -0.005 { "\u{2212}" } else { "+" };
    format!("{sign}{:.2}", value.abs())
}

/// Left (−1) to right (+1) from the newest scope frame's levels; `None`
/// while it is silent.
fn balance_of(image: &SpherePluginHost::audio_bridge::StereoImageFrame) -> Option<f32> {
    let (mut left, mut right) = (0.0f32, 0.0f32);
    for pair in image.scope.chunks_exact(2) {
        left += pair[0] * pair[0];
        right += pair[1] * pair[1];
    }
    let (left, right) = (left.sqrt(), right.sqrt());
    let total = left + right;
    (total > 1.0e-4).then(|| ((right - left) / total).clamp(-1.0, 1.0))
}

fn balance_text(value: f32) -> String {
    let amount = (value.abs() * 100.0).round();
    if amount < 2.0 {
        "C".to_string()
    } else if value < 0.0 {
        format!("L {amount:.0}")
    } else {
        format!("R {amount:.0}")
    }
}

// ── Gestures ───────────────────────────────────────────────────────────────

fn scale(e: &KitEditor<ImagerPlate>) -> f32 {
    plate_scale_for(e.viewport, DESIGN)
}

/// What a slider sets, its range, and its value now.
fn slider_value(e: &KitEditor<ImagerPlate>, slider: Slider) -> (f32, f32, f32) {
    let p = &e.params;
    match (slider, e.ui.page) {
        (Slider::Band(band), Page::Width) => (-100.0, 100.0, relative_width(p, band)),
        (Slider::Band(band), Page::Stereoize) => (0.0, imager::MAX_STEREOIZE, p.stereoize[band]),
        (Slider::Global, _) => (
            -imager::MAX_GLOBAL_WIDTH,
            imager::MAX_GLOBAL_WIDTH,
            p.global_width,
        ),
    }
}

fn set_slider(
    e: &mut KitEditor<ImagerPlate>,
    slider: Slider,
    value: f32,
    cx: &mut Cx<ImagerPlate>,
) {
    match (slider, e.ui.page) {
        (Slider::Band(band), Page::Width) => {
            e.set_value(width_id(band), value + imager::DEFAULT_WIDTH, cx)
        }
        (Slider::Band(band), Page::Stereoize) => e.set_value(stereoize_id(band), value, cx),
        (Slider::Global, _) => e.set_value("globalWidth", value, cx),
    }
}

fn slider_slot(slider: Slider) -> LiveSlot {
    LiveSlot::Plate(match slider {
        Slider::Band(band) => SLOT_SLIDER + band,
        Slider::Global => SLOT_GLOBAL_SLIDER,
    })
}

/// The slider's value under window x `x`.
fn slider_at(e: &KitEditor<ImagerPlate>, slider: Slider, x: f32) -> Option<f32> {
    let bounds = e.live_bounds.get(slider_slot(slider))?;
    let (x0, _) = plate::xy(bounds.origin);
    let w = f32::from(bounds.size.width);
    let r = THUMB_R * scale(e);
    let fraction = ((x - x0 - r) / (w - 2.0 * r).max(1.0)).clamp(0.0, 1.0);
    let (min, max, _) = slider_value(e, slider);
    Some((min + fraction * (max - min)).round())
}

fn press_slider(
    e: &mut KitEditor<ImagerPlate>,
    slider: Slider,
    (x, _): (f32, f32),
    event: &MouseDownEvent,
    cx: &mut Cx<ImagerPlate>,
) {
    if let Slider::Band(band) = slider {
        e.ui.selected = band;
    }
    if event.click_count >= 2 {
        // A double-click puts it back: every slider rests at 0.
        set_slider(e, slider, 0.0, cx);
        cx.notify();
        return;
    }
    e.ui.drag = Some(Drag::Slider(slider));
    if let Some(value) = slider_at(e, slider, x) {
        set_slider(e, slider, value, cx);
    }
    cx.notify();
}

fn wheel_slider(
    e: &mut KitEditor<ImagerPlate>,
    slider: Slider,
    dy: f32,
    fine: bool,
    cx: &mut Cx<ImagerPlate>,
) {
    let (min, max, value) = slider_value(e, slider);
    let step = if fine {
        1.0
    } else {
        (dy / 20.0).round().abs().max(1.0)
    };
    let next = (value + step * dy.signum()).clamp(min, max).round();
    set_slider(e, slider, next, cx);
}

/// The display's plot in window space: `(x0, y0, width)` and the scale.
fn plot_of(e: &KitEditor<ImagerPlate>) -> Option<(f32, f32, f32, f32)> {
    let bounds = e.live_bounds.get(LiveSlot::Plate(SLOT_BANDS))?;
    let (x0, y0) = plate::xy(bounds.origin);
    Some((x0, y0, f32::from(bounds.size.width), scale(e)))
}

fn press_bands(
    e: &mut KitEditor<ImagerPlate>,
    (x, y): (f32, f32),
    event: &MouseDownEvent,
    cx: &mut Cx<ImagerPlate>,
) {
    let Some((x0, _, w, _)) = plot_of(e) else {
        return;
    };
    let p = e.params.clone();
    let x_of = |hz: f32| x0 + freq_fraction(hz) * w;
    let crossover = (0..CROSSOVER_IDS.len())
        .filter(|_| p.multiband)
        .map(|i| (i, (x_of(p.crossover_hz[i]) - x).abs()))
        .filter(|(_, distance)| *distance <= CROSSOVER_HIT)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(i, _)| i);
    let band = (0..shown(&p))
        .find(|b| {
            let (low, high) = span(&p, *b);
            x >= x_of(low) && x < x_of(high)
        })
        .unwrap_or(0);
    if event.click_count >= 2 {
        match crossover {
            Some(i) => e.set_value(CROSSOVER_IDS[i], imager::DEFAULT_CROSSOVERS_HZ[i], cx),
            None => e.set_value(width_id(band), imager::DEFAULT_WIDTH, cx),
        }
        return;
    }
    e.ui.drag = Some(match crossover {
        Some(i) => Drag::Crossover(i),
        None => {
            e.ui.selected = band;
            Drag::Width {
                band,
                origin_y: y,
                start: p.width[band],
            }
        }
    });
    cx.notify();
}

fn wheel_bands(e: &mut KitEditor<ImagerPlate>, dy: f32, fine: bool, cx: &mut Cx<ImagerPlate>) {
    let band = e.ui.selected.min(shown(&e.params) - 1);
    let step = if fine { 1.0 } else { 5.0 };
    let next = (e.params.width[band] + step * dy.signum()).clamp(0.0, imager::MAX_WIDTH);
    e.set_value(width_id(band), next, cx);
}

fn drag_to(e: &mut KitEditor<ImagerPlate>, event: &MouseMoveEvent, cx: &mut Cx<ImagerPlate>) {
    let Some(drag) = e.ui.drag else {
        return;
    };
    if event.pressed_button.is_none() {
        e.ui.drag = None;
        cx.notify();
        return;
    }
    let (x, y) = (f32::from(event.position.x), f32::from(event.position.y));
    let s = scale(e);
    let fine = if event.modifiers.shift { FINE } else { 1.0 };
    match drag {
        Drag::Crossover(i) => {
            let Some((x0, _, w, _)) = plot_of(e) else {
                return;
            };
            let (low, high) = crossover_bounds(&e.params.crossover_hz, i);
            let hz = freq_at_fraction((x - x0) / w).clamp(low, high).round();
            e.set_value(CROSSOVER_IDS[i], hz, cx);
        }
        Drag::Width {
            band,
            origin_y,
            start,
        } => {
            let delta = (origin_y - y) / (SWING * s) * fine;
            let width = (start + delta).clamp(0.0, imager::MAX_WIDTH).round();
            e.set_value(width_id(band), width, cx);
        }
        Drag::Slider(slider) => {
            if let Some(value) = slider_at(e, slider, x) {
                set_slider(e, slider, value, cx);
            }
        }
        Drag::MonoHz { origin_y, start } => {
            let octaves = (origin_y - y) / (DRAG_SPAN * s) * fine;
            let hz = (start * 2f32.powf(octaves))
                .clamp(imager::MIN_MONO_BASS_HZ, imager::MAX_MONO_BASS_HZ)
                .round();
            e.set_value("monoBassHz", hz, cx);
        }
        Drag::Output { origin_y, start } => {
            let db = start + (origin_y - y) / (DRAG_SPAN * s) * 12.0 * fine;
            let db = (db.clamp(imager::MIN_OUTPUT_DB, imager::MAX_OUTPUT_DB) * 10.0).round() / 10.0;
            e.set_value("outputDb", db, cx);
        }
    }
}

fn width_id(band: usize) -> &'static str {
    ["width1", "width2", "width3", "width4"][band.min(BANDS - 1)]
}

fn stereoize_id(band: usize) -> &'static str {
    ["stereoize1", "stereoize2", "stereoize3", "stereoize4"][band.min(BANDS - 1)]
}

fn bypass_id(band: usize) -> &'static str {
    ["bypass1", "bypass2", "bypass3", "bypass4"][band.min(BANDS - 1)]
}

/// The bottom strip's Reset: every band back to as it came — widths,
/// stereoize, bypass and solo — and the global offset to none. The split,
/// Mono Bass and the output stay.
fn reset_bands(e: &mut KitEditor<ImagerPlate>, cx: &mut Cx<ImagerPlate>) {
    let mut next = e.params.clone();
    next.width = [imager::DEFAULT_WIDTH; BANDS];
    next.stereoize = [0.0; BANDS];
    next.band_bypass = [false; BANDS];
    next.solo_band = imager::SOLO_NONE;
    next.global_width = 0.0;
    e.set_params(next, cx);
}

// ── Pieces ─────────────────────────────────────────────────────────────────

/// A line of Mona Sans at the design size `size`.
fn text(content: impl Into<gpui::SharedString>, size: f32, hex: u32, s: f32) -> gpui::Div {
    div()
        .flex_shrink_0()
        .whitespace_nowrap()
        .text_size(type_px(size, s))
        .line_height(px(size * 1.25 * s))
        .text_color(ink(hex))
        .child(content.into())
}

/// The design's tracking for captions (0.12 em), as near as hair spaces
/// come in Mona Sans: one between letters.
const TRACKING: f32 = 0.08;

/// The design's section caption: 11 px medium, tracked, upper case.
fn caption(content: &str, s: f32) -> gpui::Div {
    text(tracked(content, TRACKING), 11.0, LABEL, s).font_weight(FontWeight::MEDIUM)
}

/// A section: the panel ground, its edge and padding.
fn section(s: f32) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .rounded(px(3.0 * s))
        .border(px(1.0))
        .border_color(ink(EDGE))
        .bg(ink(PANEL))
        .px(px(12.0 * s))
        .py(px(10.0 * s))
}

/// A section's head row: its caption, and what sits at the right.
fn head_row(title: &str, right: impl IntoElement, s: f32) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .h(px(20.0 * s))
        .flex_shrink_0()
        .child(caption(title, s))
        .child(right)
}

/// The design's push button: lit fills it with `lit` and darkens its text.
#[allow(clippy::too_many_arguments)]
fn button(
    id: impl Into<gpui::ElementId>,
    label: impl Into<gpui::SharedString>,
    on: bool,
    lit: u32,
    (h, pad, size): (f32, f32, f32),
    grow: bool,
    s: f32,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let (bg, border, fg) = if on {
        (lit, lit, ON_LIGHT)
    } else {
        (BUTTON, BUTTON_EDGE, INK)
    };
    let mut base = div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .justify_center()
        .h(px(h * s))
        .px(px(pad * s))
        .rounded(px(3.0 * s))
        .border(px(1.0))
        .border_color(ink(border))
        .bg(ink(bg))
        .text_color(ink(fg))
        .text_size(type_px(size, s))
        .whitespace_nowrap()
        .cursor(gpui::CursorStyle::PointingHand)
        .child(label.into());
    if grow {
        base = base.flex_1().min_w(px(0.0));
    }
    if !on {
        base = base.hover(|style| style.bg(ink(HOVER)));
    }
    base.on_click(on_click)
}

/// A header button: 34 px tall on the header's darker ground.
fn header_button(
    id: impl Into<gpui::ElementId>,
    label: impl Into<gpui::SharedString>,
    on: bool,
    lit: u32,
    s: f32,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    button(id, label, on, lit, (34.0, 14.0, 13.0), false, s, on_click).border_color(ink(if on {
        lit
    } else {
        EDGE
    }))
}

/// A canvas that records `slot` for the overlay and the gestures.
fn slot(e: &KitEditor<ImagerPlate>, slot: usize) -> gpui::Canvas<()> {
    plate::live_slot(&e.live_bounds, slot)
}

// ── Header ─────────────────────────────────────────────────────────────────

fn header(e: &KitEditor<ImagerPlate>, cx: &mut Cx<ImagerPlate>, s: f32) -> AnyElement {
    let p = &e.params;
    let title = div()
        .flex_shrink_0()
        .whitespace_nowrap()
        .font(plate::mona_sans_expanded())
        .text_size(type_px(24.0, s))
        .line_height(px(28.0 * s))
        .text_color(ink(WHITE))
        .child(plate::spaced("IMAGER", 0.02));

    let multiband = header_button(
        "imager-multiband",
        "Multiband",
        p.multiband,
        ACCENT,
        s,
        e.click_cb(cx, |this, cx| this.toggle("multiband", cx)),
    );
    let recover = header_button(
        "imager-recover",
        "Recover Sides",
        p.recover_sides,
        ACCENT,
        s,
        e.click_cb(cx, |this, cx| this.toggle("recoverSides", cx)),
    );
    let output_db = p.output_db;
    let output = div()
        .id("imager-output")
        .flex()
        .flex_row()
        .items_center()
        .gap(px(10.0 * s))
        .h(px(34.0 * s))
        .px(px(12.0 * s))
        .rounded(px(3.0 * s))
        .border(px(1.0))
        .border_color(ink(EDGE))
        .bg(ink(BUTTON))
        .text_size(type_px(13.0, s))
        .cursor(gpui::CursorStyle::ResizeUpDown)
        .hover(|style| style.bg(ink(HOVER)))
        .child(text("Output", 13.0, QUIET, s))
        .child(text(plate::fmt_db(output_db), 13.0, INK, s))
        .on_mouse_down(
            MouseButton::Left,
            e.press_cb(cx, move |this, (_, y), event, cx| {
                if event.click_count >= 2 {
                    this.set_value("outputDb", 0.0, cx);
                    return;
                }
                this.ui.drag = Some(Drag::Output {
                    origin_y: y,
                    start: output_db,
                });
                cx.notify();
            }),
        )
        .on_scroll_wheel(e.wheel_cb(cx, move |this, dy, fine, cx| {
            let step = if fine { 0.1 } else { 0.5 };
            let db = (this.params.output_db + step * dy.signum())
                .clamp(imager::MIN_OUTPUT_DB, imager::MAX_OUTPUT_DB);
            this.set_value("outputDb", (db * 10.0).round() / 10.0, cx);
        }));

    let preset = div()
        .relative()
        .flex_shrink_0()
        .child(
            div()
                .id("imager-preset")
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.0 * s))
                .h(px(34.0 * s))
                .px(px(12.0 * s))
                .rounded(px(3.0 * s))
                .border(px(1.0))
                .border_color(ink(if e.preset_open { ACCENT } else { EDGE }))
                .bg(ink(BUTTON))
                .text_size(type_px(13.0, s))
                .cursor(gpui::CursorStyle::PointingHand)
                .hover(|style| style.bg(ink(HOVER)))
                .child(text("Preset", 13.0, QUIET, s))
                .child(text(e.preset_label(), 13.0, INK, s))
                .child(icon(
                    CHEVRON_DOWN,
                    (10.0, 10.0),
                    (12.0, 12.0),
                    1.4,
                    ink(QUIET),
                    s,
                ))
                .on_click(e.click_cb(cx, |this, cx| this.toggle_preset_menu(cx))),
        )
        .children(preset_menu(e, cx, s, ink(ACCENT)));

    let on_b = e.compare.on_b;
    let letter = |label: &'static str, lit: bool, index: usize, cx: &mut Cx<ImagerPlate>| {
        div()
            .id(("imager-ab", index))
            .flex()
            .items_center()
            .justify_center()
            .h_full()
            .px(px(14.0 * s))
            .bg(ink(if lit { INK } else { BUTTON }))
            .text_color(ink(if lit { ON_LIGHT } else { INK }))
            .text_size(type_px(13.0, s))
            .cursor(gpui::CursorStyle::PointingHand)
            .child(label)
            .on_click(e.click_cb(cx, move |this, cx| {
                if (index == 1) != this.compare.on_b {
                    this.swap_compare(cx);
                }
            }))
    };
    let compare = div()
        .flex()
        .flex_row()
        .flex_shrink_0()
        .h(px(34.0 * s))
        .rounded(px(3.0 * s))
        .border(px(1.0))
        .border_color(ink(EDGE))
        .overflow_hidden()
        .child(letter("A", !on_b, 0, cx))
        .child(letter("B", on_b, 1, cx));

    let history = |id: &'static str, path: &'static str, enabled: bool| {
        let base = div()
            .id(id)
            .flex()
            .items_center()
            .justify_center()
            .flex_shrink_0()
            .size(px(34.0 * s))
            .rounded(px(3.0 * s))
            .border(px(1.0))
            .border_color(ink(EDGE))
            .bg(ink(BUTTON))
            .child(icon(path, (14.0, 14.0), (14.0, 14.0), 1.4, ink(INK), s));
        if enabled {
            base.cursor(gpui::CursorStyle::PointingHand)
                .hover(|style| style.bg(ink(HOVER)))
        } else {
            base.opacity(0.35)
        }
    };
    let undo = history("imager-undo", UNDO, e.can_undo())
        .on_click(e.click_cb(cx, |this, cx| this.undo(cx)));
    let redo = history("imager-redo", REDO, e.can_redo())
        .on_click(e.click_cb(cx, |this, cx| this.redo(cx)));

    let bypassed = !e.power();
    let bypass = header_button(
        "imager-bypass",
        "Bypass",
        bypassed,
        WARM,
        s,
        e.click_cb(cx, |this, cx| this.toggle("power", cx)),
    );

    let notice = e
        .notice()
        .map(|notice| text(notice.to_string(), 12.0, LABEL, s).into_any_element());

    div()
        .flex()
        .flex_row()
        .items_center()
        .h(px(54.0 * s))
        .flex_shrink_0()
        .gap(px(8.0 * s))
        .child(title)
        .child(div().w(px(16.0 * s)).flex_shrink_0())
        .child(multiband)
        .child(recover)
        .child(output)
        .child(div().flex_1())
        .children(notice)
        .child(preset)
        .child(compare)
        .child(undo)
        .child(redo)
        .child(bypass)
        .into_any_element()
}

// ── Left column ────────────────────────────────────────────────────────────

fn scope_panel(e: &KitEditor<ImagerPlate>, s: f32) -> AnyElement {
    section(s)
        .h(px(220.0 * s))
        .flex_shrink_0()
        .child(head_row(
            "Vectorscope",
            text("Lissajous", 11.0, LABEL, s),
            s,
        ))
        .child(
            div()
                .w_full()
                .h(px(172.0 * s))
                .child(slot(e, SLOT_SCOPE).size_full()),
        )
        .into_any_element()
}

/// A meter row's head: its caption, and the reading the overlay prints.
fn meter_head(e: &KitEditor<ImagerPlate>, title: &str, value_slot: usize, s: f32) -> gpui::Div {
    head_row(
        title,
        div()
            .w(px(80.0 * s))
            .h(px(19.0 * s))
            .child(slot(e, value_slot).size_full()),
        s,
    )
    .h(px(19.0 * s))
}

fn scale_row(labels: &[&'static str], s: f32) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .justify_between()
        .children(labels.iter().map(|label| text(*label, 10.0, DIM, s)))
}

fn image_panel(e: &KitEditor<ImagerPlate>, cx: &mut Cx<ImagerPlate>, s: f32) -> AnyElement {
    let p = &e.params;
    let bar = |slot_index: usize| {
        div()
            .w_full()
            .h(px(8.0 * s))
            .flex_shrink_0()
            .child(slot(e, slot_index).size_full())
    };
    let correlation = div()
        .flex()
        .flex_col()
        .gap(px(6.0 * s))
        .child(meter_head(e, "Correlation", SLOT_CORRELATION_VALUE, s))
        .child(bar(SLOT_CORRELATION))
        .child(scale_row(&["\u{2212}1", "0", "+1"], s));
    let balance = div()
        .flex()
        .flex_col()
        .gap(px(6.0 * s))
        .child(meter_head(e, "Balance", SLOT_BALANCE_VALUE, s))
        .child(bar(SLOT_BALANCE))
        .child(scale_row(&["L", "R"], s));

    let hz = p.mono_bass_hz;
    let mono_hz = div()
        .id("imager-mono-hz")
        .cursor(gpui::CursorStyle::ResizeUpDown)
        .child(text(hz_label(hz), 12.0, WHITE, s))
        .on_mouse_down(
            MouseButton::Left,
            e.press_cb(cx, move |this, (_, y), event, cx| {
                if event.click_count >= 2 {
                    this.set_value("monoBassHz", imager::DEFAULT_MONO_BASS_HZ, cx);
                    return;
                }
                this.ui.drag = Some(Drag::MonoHz {
                    origin_y: y,
                    start: hz,
                });
                cx.notify();
            }),
        )
        .on_scroll_wheel(e.wheel_cb(cx, move |this, dy, fine, cx| {
            let octaves = dy.signum() * if fine { 1.0 / 24.0 } else { 1.0 / 6.0 };
            let next = (this.params.mono_bass_hz * 2f32.powf(octaves))
                .clamp(imager::MIN_MONO_BASS_HZ, imager::MAX_MONO_BASS_HZ)
                .round();
            this.set_value("monoBassHz", next, cx);
        }));
    let mono = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .child(
            div()
                .flex()
                .flex_row()
                .items_baseline()
                .gap(px(4.0 * s))
                .child(text("Mono below", 12.0, LABEL, s))
                .child(mono_hz),
        )
        .child(button(
            "imager-mono",
            if p.mono_bass { "On" } else { "Off" },
            p.mono_bass,
            ACCENT,
            (30.0, 12.0, 12.0),
            false,
            s,
            e.click_cb(cx, |this, cx| this.toggle("monoBass", cx)),
        ));

    section(s)
        .h(px(174.0 * s))
        .flex_shrink_0()
        .justify_between()
        .child(correlation)
        .child(balance)
        .child(mono)
        .into_any_element()
}

// ── Band display ───────────────────────────────────────────────────────────

fn legend_line(dashed: bool, hex: u32, s: f32) -> gpui::Canvas<()> {
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let (x, y) = plate::xy(bounds.center());
            let half = f32::from(bounds.size.width) * 0.5;
            paint_polyline(
                window,
                &[(x - half, y), (x + half, y)],
                1.5 * s,
                ink(hex),
                dashed.then_some((3.0 * s, 2.0 * s)),
            );
        },
    )
    .w(px(16.0 * s))
    .h(px(2.0 * s))
    .flex_shrink_0()
}

fn band_display(e: &KitEditor<ImagerPlate>, cx: &mut Cx<ImagerPlate>, s: f32) -> AnyElement {
    let item = |swatch: AnyElement, label: &'static str| {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0 * s))
            .child(swatch)
            .child(text(label, 11.0, LABEL, s))
    };
    let legend = div()
        .flex()
        .flex_row()
        .gap(px(16.0 * s))
        .child(item(
            legend_line(false, MID_LINE, s).into_any_element(),
            "Mid",
        ))
        .child(item(legend_line(true, DIM, s).into_any_element(), "Side"))
        .child(item(
            div()
                .size(px(10.0 * s))
                .bg(color_a(WHITE, 0.35))
                .into_any_element(),
            "Width per band",
        ));
    let plot = div()
        .id("imager-bands")
        .relative()
        .w_full()
        .h(px(DISPLAY_H * s))
        .flex_shrink_0()
        .cursor(gpui::CursorStyle::ResizeUpDown)
        .child(slot(e, SLOT_BANDS).absolute().size_full())
        .on_mouse_down(MouseButton::Left, e.press_cb(cx, press_bands))
        .on_scroll_wheel(e.wheel_cb(cx, wheel_bands));
    section(s)
        .flex_1()
        .min_w(px(0.0))
        .gap(px(8.0 * s))
        .child(head_row("Multiband width", legend, s))
        .child(plot)
        .into_any_element()
}

// ── Strip ──────────────────────────────────────────────────────────────────

/// A slider at `value` in `min..max`, in `tone`: filled from the centre when
/// `bipolar`, from the left otherwise.
fn slider_canvas(
    value: f32,
    (min, max): (f32, f32),
    bipolar: bool,
    tone: u32,
    s: f32,
) -> gpui::Canvas<()> {
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let (x0, y0) = plate::xy(bounds.origin);
            let w = f32::from(bounds.size.width);
            let h = f32::from(bounds.size.height);
            let r = THUMB_R * s;
            let track = TRACK_H * s;
            let cy = y0 + h * 0.5;
            let left = x0 + r;
            let span = (w - 2.0 * r).max(1.0);
            let x_of = |v: f32| left + (v - min) / (max - min) * span;
            window.paint_quad(
                fill(rect(left, cy - track * 0.5, span, track), ink(EDGE))
                    .corner_radii(px(track * 0.5)),
            );
            let from = if bipolar { x_of(0.0) } else { left };
            let at = x_of(value.clamp(min, max));
            let (a, b) = if at < from { (at, from) } else { (from, at) };
            window.paint_quad(
                fill(rect(a, cy - track * 0.5, b - a, track), ink(tone))
                    .corner_radii(px(track * 0.5)),
            );
            if bipolar {
                paint_rect(
                    window,
                    (from - 0.5, cy - track * 1.5, 1.0, track * 3.0),
                    color_a(INK, 0.45),
                );
            }
            paint_dot(window, at, cy, r, ink(tone));
        },
    )
    .size_full()
}

fn slider(
    e: &KitEditor<ImagerPlate>,
    cx: &mut Cx<ImagerPlate>,
    which: Slider,
    tone: u32,
    enabled: bool,
    s: f32,
) -> AnyElement {
    let (min, max, value) = slider_value(e, which);
    let bipolar = min < 0.0;
    let index = match which {
        Slider::Band(band) => band,
        Slider::Global => BANDS,
    };
    let mut el = div()
        .id(("imager-slider", index))
        .relative()
        .w_full()
        .h(px(24.0 * s))
        .flex_shrink_0()
        .child(slider_canvas(value, (min, max), bipolar, tone, s))
        .child(
            e.live_bounds
                .slot(slider_slot(which))
                .absolute()
                .size_full(),
        );
    if enabled {
        el = el
            .cursor(gpui::CursorStyle::ResizeLeftRight)
            .on_mouse_down(
                MouseButton::Left,
                e.press_cb(cx, move |this, at, event, cx| {
                    press_slider(this, which, at, event, cx)
                }),
            )
            .on_scroll_wheel(e.wheel_cb(cx, move |this, dy, fine, cx| {
                wheel_slider(this, which, dy, fine, cx)
            }));
    }
    el.into_any_element()
}

/// A card's value row: what the slider sets, and the big readout.
fn value_row(label: &str, value: String, s: f32) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_baseline()
        .justify_between()
        .child(text(label.to_string(), 11.0, LABEL, s))
        .child(
            div()
                .flex_shrink_0()
                .whitespace_nowrap()
                .font(plate::mona_sans_semi_expanded_light())
                .text_size(type_px(26.0, s))
                .line_height(px(30.0 * s))
                .text_color(ink(WHITE))
                .child(value),
        )
}

fn card(s: f32, edge: u32) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w(px(0.0))
        .h_full()
        .gap(px(8.0 * s))
        .px(px(12.0 * s))
        .py(px(10.0 * s))
        .rounded(px(3.0 * s))
        .border(px(1.0))
        .border_color(ink(edge))
        .bg(ink(CARD))
}

fn band_card(
    e: &KitEditor<ImagerPlate>,
    cx: &mut Cx<ImagerPlate>,
    band: usize,
    s: f32,
) -> AnyElement {
    let p = &e.params;
    let tone = BAND_COLORS[band];
    let live = p.multiband || band == 0;
    let selected = live && e.ui.selected == band;
    let bypassed = p.band_bypass[band];
    let solo = soloed(p);
    let is_solo = solo == Some(band);
    let name = if p.multiband || band > 0 {
        BAND_NAMES[band]
    } else {
        "Whole signal"
    };
    let range = if live {
        range_label(p, band)
    } else {
        "Multiband off".to_string()
    };
    let title = div()
        .id(("imager-band", band))
        .flex()
        .flex_col()
        .gap(px(2.0 * s))
        .min_h(px(36.0 * s))
        .flex_shrink_0()
        .child(
            text(tracked(name, TRACKING), 12.0, tone, s)
                .font_weight(FontWeight::SEMIBOLD)
                .truncate(),
        )
        .child(text(range, 11.0, LABEL, s).truncate());
    let title = if live {
        title
            .cursor(gpui::CursorStyle::PointingHand)
            .on_click(e.click_cb(cx, move |this, cx| {
                this.ui.selected = band;
                cx.notify();
            }))
    } else {
        title
    };
    let (label, value) = match e.ui.page {
        Page::Width => ("Width", percent(relative_width(p, band))),
        Page::Stereoize => ("Stereoize", format!("{:.0}%", p.stereoize[band].round())),
    };
    let buttons = div()
        .flex()
        .flex_row()
        .gap(px(6.0 * s))
        .children(p.multiband.then(|| {
            button(
                ("imager-solo", band),
                "Solo",
                is_solo,
                tone,
                (30.0, 12.0, 12.0),
                true,
                s,
                e.click_cb(cx, move |this, cx| {
                    let next = if is_solo {
                        imager::SOLO_NONE as f32
                    } else {
                        band as f32
                    };
                    this.set_value("soloBand", next, cx);
                }),
            )
        }))
        .child(button(
            ("imager-band-bypass", band),
            "Bypass",
            bypassed,
            INK,
            (30.0, 12.0, 12.0),
            true,
            s,
            e.click_cb(cx, move |this, cx| this.toggle(bypass_id(band), cx)),
        ));
    let body = card(s, if selected { tone } else { EDGE })
        .child(title)
        .child(value_row(label, value, s))
        .child(slider(e, cx, Slider::Band(band), tone, live, s))
        .child(div().flex_1())
        .child(buttons);
    let opacity = if !live {
        0.35
    } else if bypassed {
        0.55
    } else if solo.is_some() && !is_solo {
        0.7
    } else {
        1.0
    };
    if live {
        body.opacity(opacity).into_any_element()
    } else {
        // With the split off the other bands stand aside, inert.
        div()
            .flex()
            .flex_1()
            .min_w(px(0.0))
            .h_full()
            .opacity(opacity)
            .child(body)
            .into_any_element()
    }
}

fn global_card(e: &KitEditor<ImagerPlate>, cx: &mut Cx<ImagerPlate>, s: f32) -> AnyElement {
    let p = &e.params;
    let stereoize = e.ui.page == Page::Stereoize;
    let head = div()
        .flex()
        .flex_col()
        .gap(px(2.0 * s))
        .min_h(px(36.0 * s))
        .flex_shrink_0()
        .child(text(tracked("Global", TRACKING), 12.0, WHITE, s).font_weight(FontWeight::SEMIBOLD))
        .child(text(
            if stereoize {
                "How every band makes side"
            } else {
                "Offsets every band"
            },
            11.0,
            LABEL,
            s,
        ));
    let control: AnyElement = if stereoize {
        // The stereoize character, shared by the bands.
        let mode = p.stereoize_mode;
        let choice =
            |label: &'static str, which: imager::StereoizeMode, cx: &mut Cx<ImagerPlate>| {
                let lit = mode == which;
                div()
                    .id(("imager-stereoize-mode", which as usize))
                    .flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .h_full()
                    .bg(ink(if lit { ACCENT } else { BUTTON }))
                    .text_color(ink(if lit { ON_LIGHT } else { INK }))
                    .text_size(type_px(12.0, s))
                    .cursor(gpui::CursorStyle::PointingHand)
                    .child(label)
                    .on_click(e.click_cb(cx, move |this, cx| {
                        this.set_value("stereoizeMode", which.to_wire(), cx)
                    }))
            };
        div()
            .flex()
            .flex_row()
            .w_full()
            .h(px(24.0 * s))
            .flex_shrink_0()
            .rounded(px(3.0 * s))
            .border(px(1.0))
            .border_color(ink(BUTTON_EDGE))
            .overflow_hidden()
            .child(choice("I  Haas", imager::StereoizeMode::One, cx))
            .child(choice("II  Smooth", imager::StereoizeMode::Two, cx))
            .into_any_element()
    } else {
        slider(e, cx, Slider::Global, ACCENT, true, s)
    };
    let (label, value) = if stereoize {
        ("Character", p.stereoize_mode.label().to_string())
    } else {
        ("Width", percent(p.global_width))
    };
    let buttons = div()
        .flex()
        .flex_row()
        .gap(px(6.0 * s))
        .child(button(
            "imager-stereoize",
            "Stereoize",
            stereoize,
            ACCENT,
            (30.0, 12.0, 12.0),
            true,
            s,
            e.click_cb(cx, |this, cx| {
                this.ui.page = match this.ui.page {
                    Page::Width => Page::Stereoize,
                    Page::Stereoize => Page::Width,
                };
                cx.notify();
            }),
        ))
        .child(
            button(
                "imager-reset",
                "Reset",
                false,
                INK,
                (30.0, 12.0, 12.0),
                true,
                s,
                e.click_cb(cx, reset_bands),
            )
            .bg(color_a(0, 0.0)),
        );
    card(s, EDGE)
        .child(head)
        .child(value_row(label, value, s))
        .child(control)
        .child(div().flex_1())
        .child(buttons)
        .into_any_element()
}

fn strip(e: &KitEditor<ImagerPlate>, cx: &mut Cx<ImagerPlate>, s: f32) -> AnyElement {
    let mut cards: Vec<AnyElement> = (0..BANDS).map(|band| band_card(e, cx, band, s)).collect();
    cards.push(global_card(e, cx, s));
    div()
        .flex()
        .flex_row()
        .mt(px(8.0 * s))
        .h(px(211.0 * s))
        .flex_shrink_0()
        .gap(px(8.0 * s))
        .p(px(12.0 * s))
        .rounded(px(3.0 * s))
        .border(px(1.0))
        .border_color(ink(EDGE))
        .bg(ink(PANEL))
        .children(cards)
        .into_any_element()
}

// ── Live painting ──────────────────────────────────────────────────────────

fn label_font(weight: FontWeight) -> gpui::Font {
    plate::mona_sans(weight)
}

/// A line of text by its baseline, the way the design's SVG places it.
#[allow(clippy::too_many_arguments)]
fn paint_at_baseline(
    window: &mut Window,
    cx: &mut App,
    content: &str,
    size: f32,
    hex: u32,
    (x, baseline): (f32, f32),
    align: Align,
    weight: FontWeight,
) {
    paint_text(
        window,
        cx,
        content,
        label_font(weight),
        size,
        ink(hex),
        (x, baseline - size * 1.05),
        align,
    );
}

/// A meter's reading, right-aligned in its slot: 15 px, white.
fn paint_reading(window: &mut Window, cx: &mut App, bounds: Bounds<Pixels>, content: &str, s: f32) {
    let (x, y) = plate::xy(bounds.origin);
    let w = f32::from(bounds.size.width);
    paint_text(
        window,
        cx,
        content,
        label_font(FontWeight::NORMAL),
        15.0 * s,
        ink(WHITE),
        (x + w, y),
        Align::Right,
    );
}

/// A meter bar's well: the dark track, its edge and the centre mark.
fn paint_bar_well(window: &mut Window, bounds: Bounds<Pixels>, s: f32, centre: bool) {
    let (x, y) = plate::xy(bounds.origin);
    let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
    window.paint_quad(
        fill(rect(x, y, w, h), ink(WELL))
            .corner_radii(px(2.0 * s))
            .border_widths(px(1.0))
            .border_color(ink(EDGE)),
    );
    if centre {
        paint_rect(
            window,
            (x + w * 0.5 - 0.5, y - 1.0, 1.0, h + 2.0),
            ink(0x3a3a3a),
        );
    }
}

fn paint_correlation(window: &mut Window, bounds: Bounds<Pixels>, value: Option<f32>, s: f32) {
    paint_bar_well(window, bounds, s, true);
    let Some(value) = value else {
        return;
    };
    let (x, y) = plate::xy(bounds.origin);
    let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
    let at = x + (value.clamp(-1.0, 1.0) + 1.0) * 0.5 * w;
    let centre = x + w * 0.5;
    let (a, b) = if at < centre {
        (at, centre)
    } else {
        (centre, at)
    };
    let tone = if value < 0.0 { WARM } else { ACCENT };
    paint_rect(window, (a, y + 1.0, (b - a).max(1.0), h - 2.0), ink(tone));
}

fn paint_balance(window: &mut Window, bounds: Bounds<Pixels>, value: Option<f32>, s: f32) {
    paint_bar_well(window, bounds, s, false);
    let Some(value) = value else {
        return;
    };
    let (x, y) = plate::xy(bounds.origin);
    let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
    let at = x + (value.clamp(-1.0, 1.0) + 1.0) * 0.5 * w;
    window.paint_quad(
        fill(rect(at - 2.0 * s, y - 1.0, 4.0 * s, h + 2.0), ink(INK)).corner_radii(px(1.0 * s)),
    );
}

/// The vectorscope as the design draws it: a diamond with mid up, left and
/// right on its upper edges and side across, the newest frames brightest.
fn paint_scope(window: &mut Window, cx: &mut App, bounds: Bounds<Pixels>, live: &Live, s: f32) {
    let (x0, y0) = plate::xy(bounds.origin);
    let w = f32::from(bounds.size.width);
    // The design's view box is 284 wide; centre it in whatever the slot got.
    let ox = x0 + (w - 284.0 * s) * 0.5;
    let at = |x: f32, y: f32| (ox + x * s, y0 + y * s);
    let grid = ink(0x2c2c2c);
    let diamond = [
        at(142.0, 10.0),
        at(218.0, 86.0),
        at(142.0, 162.0),
        at(66.0, 86.0),
    ];
    let mut outline = diamond.to_vec();
    outline.push(diamond[0]);
    paint_polyline(window, &outline, 1.0, ink(0x333333), None);
    paint_polyline(window, &[at(142.0, 6.0), at(142.0, 166.0)], 1.0, grid, None);
    paint_polyline(window, &[at(62.0, 86.0), at(222.0, 86.0)], 1.0, grid, None);
    let dash = Some((2.0 * s, 3.0 * s));
    paint_polyline(
        window,
        &[at(104.0, 48.0), at(180.0, 124.0)],
        1.0,
        grid,
        dash,
    );
    paint_polyline(
        window,
        &[at(180.0, 48.0), at(104.0, 124.0)],
        1.0,
        grid,
        dash,
    );
    let tag = |window: &mut Window, cx: &mut App, t: &str, (x, y): (f32, f32), align: Align| {
        paint_at_baseline(
            window,
            cx,
            t,
            9.0 * s,
            DIM,
            at(x, y),
            align,
            FontWeight::NORMAL,
        );
    };
    paint_text(
        window,
        cx,
        "M",
        label_font(FontWeight::NORMAL),
        9.0 * s,
        ink(DIM),
        at(142.0, 3.0),
        Align::Center,
    );
    tag(window, cx, "L", (96.0, 40.0), Align::Right);
    tag(window, cx, "R", (188.0, 40.0), Align::Left);
    tag(window, cx, "+S", (58.0, 89.0), Align::Right);
    tag(window, cx, "\u{2212}S", (226.0, 89.0), Align::Left);

    let frames = live.image_frames().len();
    if frames == 0 {
        return;
    }
    let gain = live.scope_gain();
    let (cx0, cy0) = at(142.0, 86.0);
    let radius = 76.0 * s;
    let dot = (1.2 * s).max(0.8);
    for (age, frame) in live.image_frames().enumerate() {
        let alpha = 0.2 + 0.75 * (age + 1) as f32 / frames as f32;
        let tone = color_a(ACCENT, alpha);
        for pair in frame.scope.chunks_exact(2) {
            let (l, r) = (pair[0] * gain, pair[1] * gain);
            let side = ((l - r) * std::f32::consts::FRAC_1_SQRT_2).clamp(-1.0, 1.0);
            let mid = ((l + r) * std::f32::consts::FRAC_1_SQRT_2).clamp(-1.0, 1.0);
            // Inside the diamond: |x| + |y| ≤ radius.
            let (mut dx, mut dy) = (-side, mid);
            let reach = dx.abs() + dy.abs();
            if reach > 1.0 {
                dx /= reach;
                dy /= reach;
            }
            paint_dot(window, cx0 + dx * radius, cy0 - dy * radius, dot, tone);
        }
    }
}

/// The band display, whole: the picked band's region, the grid, the mid and
/// side spectra, each band's width bar, the crossovers and the axes.
fn paint_bands(
    window: &mut Window,
    cx: &mut App,
    bounds: Bounds<Pixels>,
    view: &ImagerView,
    live: &Live,
    s: f32,
) {
    let p = &view.params;
    let (x0, y0) = plate::xy(bounds.origin);
    let w = f32::from(bounds.size.width);
    let x_of = |hz: f32| x0 + freq_fraction(hz) * w;
    let y_of = |design_y: f32| y0 + design_y * s;
    let plot_bottom = y_of(PLOT_H);
    let bands = shown(p);
    let selected = view.selected.min(bands - 1);

    // The picked band's region.
    let (low, high) = span(p, selected);
    paint_rect(
        window,
        (x_of(low), y0, x_of(high) - x_of(low), PLOT_H * s),
        color_a(BAND_COLORS[selected], 0.06),
    );

    // The grid.
    for (design_y, hex) in [
        (30.0, 0x222222),
        (95.0, 0x222222),
        (CENTRE_Y, 0x3a3a3a),
        (225.0, 0x222222),
        (290.0, 0x222222),
    ] {
        paint_rect(window, (x0, y_of(design_y) - 0.5, w, 1.0), ink(hex));
    }
    for (hz, _) in FREQ_TICKS {
        paint_rect(window, (x_of(hz) - 0.5, y0, 1.0, PLOT_H * s), ink(0x1c1c1c));
    }

    // The spectra: mid filled under its line, side dashed.
    let db_y = |db: f32| {
        let t = (db / SPECTRUM_FLOOR_DB).clamp(0.0, 1.0);
        y_of(30.0 + t * (PLOT_H - 30.0))
    };
    if let Some(bins) = live.spectrum_bins() {
        let count = bins.len() as f32;
        let line: Vec<(f32, f32)> = bins
            .iter()
            .enumerate()
            .map(|(i, db)| (x0 + (i as f32 + 0.5) / count * w, db_y(*db)))
            .collect();
        let mut area = line.clone();
        area.push((x0 + w, plot_bottom));
        area.push((x0, plot_bottom));
        area.insert(0, (x0, line[0].1));
        paint_polygon(window, &area, color_a(WHITE, 0.05));
        paint_polyline(window, &line, 1.5 * s, ink(MID_LINE), None);
    }
    if let Some(block) = live.pads() {
        let side = &block[..imager::telemetry::SLOTS];
        let count = side.len() as f32;
        let line: Vec<(f32, f32)> = side
            .iter()
            .enumerate()
            .map(|(i, db)| (x0 + (i as f32 + 0.5) / count * w, db_y(*db)))
            .collect();
        paint_polyline(window, &line, 1.5 * s, ink(DIM), Some((4.0 * s, 3.0 * s)));
    }

    // Each band's width, from the centre line.
    let solo = soloed(p);
    for band in 0..bands {
        let (low, high) = span(p, band);
        let tone = BAND_COLORS[band];
        let left = x_of(low) + BAR_INSET * s;
        let right = x_of(high) - BAR_INSET * s;
        if right - left < 2.0 {
            continue;
        }
        let width = played_width(p, band);
        let reach = width.abs() * SWING;
        let h = reach.max(2.0);
        let top = if width >= 0.0 { CENTRE_Y - h } else { CENTRE_Y };
        let mut alpha = if p.band_bypass[band] {
            0.05
        } else if band == selected {
            0.4
        } else {
            0.18
        };
        if solo.is_some_and(|b| b != band) {
            alpha *= 0.4;
        }
        paint_rect(
            window,
            (left, y_of(top), right - left, h * s),
            color_a(tone, alpha),
        );
        let cap = if width >= 0.0 {
            CENTRE_Y - reach
        } else {
            CENTRE_Y + reach
        };
        paint_rect(
            window,
            (left, y_of(cap - 1.5), right - left, 3.0 * s),
            if p.band_bypass[band] {
                color_a(tone, 0.4)
            } else {
                ink(tone)
            },
        );
        let name = if p.multiband {
            BAND_NAMES[band].to_uppercase()
        } else {
            "WHOLE SIGNAL".to_string()
        };
        paint_at_baseline(
            window,
            cx,
            &plate::spaced(&name, TRACKING),
            11.0 * s,
            tone,
            (left, y_of(22.0)),
            Align::Left,
            FontWeight::MEDIUM,
        );
        let mut notes = Vec::new();
        if p.band_bypass[band] {
            notes.push("Bypassed".to_string());
        }
        if solo == Some(band) {
            notes.push("Solo".to_string());
        }
        let stereoized = p.stereoize[band] >= 0.5 && !p.band_bypass[band];
        if stereoized {
            notes.push(format!(
                "Stereoize {} {:.0}%",
                p.stereoize_mode.label(),
                p.stereoize[band]
            ));
        }
        // Room up to the band's right edge, clear of the width axis's
        // labels over the last band; the long form gives way first.
        let room = right - left - if band + 1 == bands { 30.0 * s } else { 0.0 };
        let fits = |line: &str| line.chars().count() as f32 * 5.4 * s <= room;
        let mut line = notes.join(" \u{00B7} ");
        if !fits(&line) && stereoized {
            if let Some(last) = notes.last_mut() {
                *last = format!("{} {:.0}%", p.stereoize_mode.label(), p.stereoize[band]);
            }
            line = notes.join(" \u{00B7} ");
        }
        if !line.is_empty() && fits(&line) {
            paint_at_baseline(
                window,
                cx,
                &line,
                10.0 * s,
                DIM,
                (left, y_of(38.0)),
                Align::Left,
                FontWeight::NORMAL,
            );
        }
    }

    // Mono Bass: the side below it is gone, so shade it.
    if p.mono_bass && p.power {
        let edge = x_of(p.mono_bass_hz);
        paint_rect(
            window,
            (
                x0,
                y_of(CENTRE_Y + 2.0),
                edge - x0,
                (PLOT_H - CENTRE_Y - 2.0) * s,
            ),
            color_a(ACCENT, 0.05),
        );
        paint_polyline(
            window,
            &[(edge, y_of(CENTRE_Y)), (edge, plot_bottom)],
            1.0,
            color_a(ACCENT, 0.5),
            Some((3.0 * s, 3.0 * s)),
        );
    }

    // The crossovers and their handles.
    let mut crossover_x = Vec::new();
    if p.multiband {
        for (index, hz) in p.crossover_hz.iter().enumerate() {
            let x = x_of(*hz);
            crossover_x.push((x, *hz));
            paint_rect(window, (x - 0.5, y0, 1.0, PLOT_H * s), color_a(INK, 0.55));
            let held = view.crossover == Some(index);
            window.paint_quad(
                fill(
                    rect(x - 6.0 * s, y_of(306.0), 12.0 * s, 12.0 * s),
                    ink(if held { INK } else { PANEL }),
                )
                .corner_radii(px(2.0 * s))
                .border_widths(px(1.0))
                .border_color(ink(INK)),
            );
        }
    }

    // The width axis on the right, the frequencies along the bottom.
    let right = x0 + w - 5.0 * s;
    for (design_y, label) in [(34.0, "+100"), (164.0, "0"), (294.0, "\u{2212}100")] {
        paint_at_baseline(
            window,
            cx,
            label,
            9.0 * s,
            DIM,
            (right, y_of(design_y)),
            Align::Right,
            FontWeight::NORMAL,
        );
    }
    let baseline = y_of(338.0);
    paint_at_baseline(
        window,
        cx,
        "Hz",
        10.0 * s,
        DIM,
        (x0 + 4.0 * s, baseline),
        Align::Left,
        FontWeight::NORMAL,
    );
    for (hz, label) in FREQ_TICKS {
        let x = x_of(hz);
        // A crossover's own label wins the room around it.
        if crossover_x
            .iter()
            .any(|(cx_, _)| x > *cx_ - 14.0 * s && x < *cx_ + 34.0 * s)
        {
            continue;
        }
        paint_at_baseline(
            window,
            cx,
            label,
            10.0 * s,
            DIM,
            (x, baseline),
            Align::Center,
            FontWeight::NORMAL,
        );
    }
    for (x, hz) in crossover_x {
        paint_at_baseline(
            window,
            cx,
            &hz_short(hz),
            10.0 * s,
            INK,
            (x + 6.0 * s, baseline),
            Align::Left,
            FontWeight::NORMAL,
        );
    }
}

/// Opens with a track's Imager insert.
pub fn open_imager_editor(
    owner_bounds: Option<gpui::Bounds<gpui::Pixels>>,
    key: PluginInstanceKey,
    identity: ShellIdentity,
    host_ops: BuiltinEditorHostOps,
    on_close: Arc<dyn Fn(&mut Window, &mut App)>,
    cx: &mut App,
) -> Result<gpui::WindowHandle<ImagerWindow>, String> {
    plate::ensure_fonts(cx);
    open_kit_editor(
        ImagerPlate,
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
    fn frequencies_read_the_way_the_design_prints_them() {
        assert_eq!(hz_label(20.0), "20 Hz");
        assert_eq!(hz_label(120.0), "120 Hz");
        assert_eq!(hz_label(1_000.0), "1 kHz");
        assert_eq!(hz_label(1_500.0), "1.5 kHz");
        assert_eq!(hz_label(6_000.0), "6 kHz");
        assert_eq!(hz_label(20_000.0), "20 kHz");
        assert_eq!(hz_short(1_250.0), "1.25k");
        assert_eq!(percent(35.0), "+35%");
        assert_eq!(percent(-60.0), "\u{2212}60%");
        assert_eq!(percent(0.2), "0%");
    }

    #[test]
    fn the_played_width_takes_the_offset_and_the_bypass() {
        let mut p = imager::default_params();
        p.width = [40.0, 100.0, 135.0, 200.0];
        p.global_width = 20.0;
        p.band_bypass[1] = true;
        assert_eq!(played_width(&p, 0), -40.0);
        assert_eq!(played_width(&p, 1), 0.0);
        assert_eq!(played_width(&p, 2), 55.0);
        assert_eq!(played_width(&p, 3), 100.0);
    }

    #[test]
    fn balance_reads_left_centre_and_right() {
        let mut frame = SpherePluginHost::audio_bridge::StereoImageFrame {
            correlation: 1.0,
            band_correlation: [1.0; 4],
            band_level: [0.1; 4],
            scope: [0.0; SpherePluginHost::audio_bridge::IMAGE_SCOPE_POINTS * 2],
        };
        assert_eq!(balance_of(&frame), None);
        for pair in frame.scope.chunks_exact_mut(2) {
            pair[0] = 0.5;
            pair[1] = 0.5;
        }
        assert_eq!(balance_text(balance_of(&frame).unwrap()), "C");
        for pair in frame.scope.chunks_exact_mut(2) {
            pair[1] = 0.0;
        }
        assert_eq!(balance_text(balance_of(&frame).unwrap()), "L 100");
    }

    /// A preset keeps what the editor is listening to.
    #[test]
    fn a_preset_keeps_power_and_the_solo() {
        let mut current = imager::default_params();
        current.power = false;
        current.solo_band = 2;
        for preset in ImagerPlate.presets().iter() {
            let applied = ImagerPlate.preset_applied(&current, &preset.params);
            assert!(!applied.power);
            assert_eq!(applied.solo_band, 2);
        }
    }
}

//! The dynamics built-ins' editors on the plate kit — Z-Comp, 67Clipper and
//! Transient — as one [`KitModel`] family, each in its own accent.
//!
//! Each well has the plug-in's level history (input, output and what it
//! takes off, ten seconds of the host's meter frames) beside its own display:
//!
//! * Z-Comp: a moving-coil VU on the circuit's face, reading gain reduction
//!   or output (a switch in the editor, not a param), and the static curve
//!   from `zcomp::transfer_db` with the live operating point on it;
//! * 67Clipper: the static curve from `clipper67::transfer_db`, its clip
//!   level and ceiling, with the live operating point;
//! * Transient: a sketch of a hit before and after the shaping the knobs
//!   set — a picture of the settings, not a measurement.
//!
//! The decks hold the plug-ins' params, each dial over its DSP descriptor's
//! own range, and In / reduction / Out meters. Displays drawn from params are
//! painted when the panel renders; the history, the meters, the VU needle and
//! the operating point ride the window's live overlay.

use std::sync::{Arc, OnceLock};

use gpui::{
    div, px, AnyElement, App, Bounds, FontWeight, InteractiveElement, IntoElement, ParentElement,
    Pixels, Rgba, StatefulInteractiveElement, Styled, Window,
};

use crate::components::builtin_plugin_editor_window::{BuiltinEditorHostOps, PluginInstanceKey};
use crate::components::dyn_model::{preset_applied, presets, DynKind, DynParams};
use crate::components::fx_model::KnobSpec;
use crate::components::native_plugin_shell::ShellIdentity;
use crate::components::plate::{
    self, caption, color, color_a, icon, knob_reading, legend, live_slot, numbers, paint_dot,
    paint_polygon, paint_polyline, paint_rect, paint_text, pill, plate_frame, plate_scale, switch,
    type_px, words, Align, Dial, DialUnit, KnobCell, KnobLook, PillLook, FAINT, GROUND, INK, SOFT,
    WELL_EDGE,
};
use crate::components::plugin_kit::{
    open_kit_editor, Cx, KitEditor, KitModel, KitPreset, KitWindow, LiveBounds, LiveSlot,
};
use crate::components::plugin_live::{Live, HISTORY};
use crate::components::time_plate::{
    paint_meter, range_dial, AXIS_INK, BOX_EDGE, BOX_GROUND, BOX_H, GRID, PLOT_BOTTOM, PLOT_TOP,
};

mod clipper_plate;
mod transient_plate;
mod zcomp_plate;

/// Each plug-in's accent.
pub const ZCOMP_ACCENT: u32 = 0xa3e635;
pub const CLIPPER_ACCENT: u32 = 0xff4d6d;
pub const TRANSIENT_ACCENT: u32 = 0xe879f9;
/// The output line and a ceiling marker.
const OUT_INK: u32 = 0xe6e6e6;
/// The input's line.
const IN_INK: u32 = 0x9a9a9a;

/// The live displays, numbered for [`LiveSlot::Plate`].
const SLOT_HISTORY: usize = 0;
const SLOT_READOUT: usize = 1;
const SLOT_TRANSFER: usize = 2;
const SLOT_VU: usize = 3;
/// The In, reduction and Out meter rows: three slots from here.
const SLOT_METER: usize = 4;

/// The display boxes across the well, in design units: the wide one at the
/// left, the narrow one at the right, and Z-Comp's three.
const WIDE_LEFT: f32 = 16.0;
const WIDE_W: f32 = 600.0;
const NARROW_LEFT: f32 = 630.0;
const NARROW_W: f32 = 364.0;
const VU_W: f32 = 296.0;
const ZCOMP_TRANSFER_LEFT: f32 = 326.0;
const ZCOMP_TRANSFER_W: f32 = 290.0;
/// The reduction meter's full span.
const REDUCTION_SPAN_DB: f32 = 24.0;

const HEADPHONES: &str = "M2 9 V6 A5 5 0 0 1 12 6 V9 M2 9 H4 V12 H2 Z M10 9 H12 V12 H10 Z";

fn accent_of(kind: DynKind) -> u32 {
    match kind {
        DynKind::Zcomp => ZCOMP_ACCENT,
        DynKind::Clipper => CLIPPER_ACCENT,
        DynKind::Transient => TRANSIENT_ACCENT,
    }
}

/// What the reduction reads as, and the sign its readout takes: Transient
/// reports the size of its gain change, boost or cut.
fn reduction_name(kind: DynKind) -> (&'static str, &'static str) {
    match kind {
        DynKind::Zcomp => ("GR", "\u{2212}"),
        DynKind::Clipper => ("Clip", "\u{2212}"),
        DynKind::Transient => ("Shape", ""),
    }
}

/// The level the history draws down to: a clipper works in the top few dB.
fn history_floor(kind: DynKind) -> f32 {
    match kind {
        DynKind::Clipper => -24.0,
        _ => -48.0,
    }
}

/// The input range a transfer display spans, to 0 dBFS.
fn transfer_range(kind: DynKind) -> f32 {
    match kind {
        DynKind::Zcomp => -60.0,
        _ => -24.0,
    }
}

// ── Dials ──────────────────────────────────────────────────────────────────

/// Every knob of every kind: the DSP descriptor's range and default, a label
/// and a readout.
fn dial_table() -> &'static [(DynKind, Dial)] {
    static TABLE: OnceLock<Vec<(DynKind, Dial)>> = OnceLock::new();
    TABLE.get_or_init(|| {
        use DynKind::*;
        let row = |kind: DynKind, id: &'static str, label: &'static str, unit: DialUnit| {
            let row = kind
                .descriptor_row(id)
                .unwrap_or_else(|| panic!("{} has no param {id}", kind.title()));
            (kind, range_dial(row, label, unit))
        };
        let log = |(kind, dial): (DynKind, Dial)| (kind, dial.log());
        let bipolar = |(kind, dial): (DynKind, Dial)| (kind, dial.bipolar());
        vec![
            row(Zcomp, "thresholdDb", "THRESHOLD", DialUnit::Db),
            log(row(Zcomp, "ratio", "RATIO", DialUnit::Ratio)),
            log(row(Zcomp, "attackMs", "ATTACK", DialUnit::Time)),
            log(row(Zcomp, "releaseMs", "RELEASE", DialUnit::Time)),
            row(Zcomp, "kneeDb", "KNEE", DialUnit::Db),
            bipolar(row(Zcomp, "makeupDb", "MAKEUP", DialUnit::Db)),
            log(row(Zcomp, "sidechainHpfHz", "SC HPF", DialUnit::HzRound)),
            row(Zcomp, "stereoLink", "LINK", DialUnit::Percent),
            row(Zcomp, "color", "COLOR", DialUnit::Percent),
            row(Zcomp, "mix", "MIX", DialUnit::Percent),
            row(Clipper, "inputDb", "INPUT", DialUnit::Db),
            row(Clipper, "thresholdDb", "THRESHOLD", DialUnit::Db),
            row(Clipper, "shape", "SHAPE", DialUnit::Percent),
            row(Clipper, "ceilingDb", "CEILING", DialUnit::Db),
            row(Clipper, "mix", "MIX", DialUnit::Percent),
            bipolar(row(Transient, "attack", "ATTACK", DialUnit::Percent)),
            bipolar(row(Transient, "sustain", "SUSTAIN", DialUnit::Percent)),
            row(Transient, "speed", "SPEED", DialUnit::Percent),
            row(Transient, "mix", "MIX", DialUnit::Percent),
        ]
    })
}

/// `kind`'s dial for param `id`.
fn dial(kind: DynKind, id: &str) -> Dial {
    dial_table()
        .iter()
        .find(|(k, dial)| *k == kind && dial.id == id)
        .map(|(_, dial)| *dial)
        .unwrap_or_else(|| panic!("{} has no dial {id}", kind.title()))
}

// ── The model ──────────────────────────────────────────────────────────────

/// The editor-only state: which scale Z-Comp's VU reads.
#[derive(Debug, Default)]
pub struct DynUi {
    pub vu_output: bool,
}

/// What the overlay paints from.
pub struct DynLiveView {
    params: DynParams,
    vu_output: bool,
}

/// A Z-Comp, 67Clipper or Transient insert's editor window.
pub type DynPlateWindow = KitWindow<DynKind>;

impl KitModel for DynKind {
    type Params = DynParams;
    type Ui = DynUi;
    type LiveView = DynLiveView;

    fn title(self) -> &'static str {
        DynKind::title(self)
    }

    fn subtitle(self, params: &DynParams) -> String {
        match params {
            DynParams::Zcomp(p) => format!("Multi-circuit dynamics · {}", p.model.display_name()),
            DynParams::Clipper(_) => "Oversampled clipper and peak limiter".into(),
            DynParams::Transient(_) => "Attack and sustain shaper".into(),
        }
    }

    fn key(self) -> &'static str {
        DynKind::key(self)
    }

    fn defaults(self) -> DynParams {
        DynParams::defaults(self)
    }

    fn from_mirror(self, insert_id: &str) -> Option<DynParams> {
        use crate::components::builtin_plugin_editor as mirror;
        Some(match self {
            DynKind::Zcomp => DynParams::Zcomp(mirror::builtin_zcomp_params(insert_id)?),
            DynKind::Clipper => DynParams::Clipper(mirror::builtin_clipper67_params(insert_id)?),
            DynKind::Transient => {
                DynParams::Transient(mirror::builtin_transient_params(insert_id)?)
            }
        })
    }

    fn wire_values(self, params: &DynParams) -> Vec<(u32, f32)> {
        params.wire_values()
    }

    fn with(self, params: &DynParams, id: &str, value: f32) -> DynParams {
        params.with(id, value)
    }

    fn value(self, params: &DynParams, id: &str) -> f32 {
        params.value(id)
    }

    fn presets(self) -> Arc<Vec<KitPreset<DynParams>>> {
        presets(self)
    }

    fn preset_applied(self, current: &DynParams, preset: &DynParams) -> DynParams {
        preset_applied(current, preset)
    }

    fn knob(self, _id: &str) -> Option<KnobSpec> {
        None
    }

    /// Z-Comp's SC Listen is let go as the window closes, so a closed editor
    /// never leaves a track playing only its sidechain.
    fn on_close(self, params: &DynParams) -> Option<DynParams> {
        match params {
            DynParams::Zcomp(p) if p.sc_listen => Some(params.with("scListen", 0.0)),
            _ => None,
        }
    }

    fn panel(e: &KitEditor<Self>, cx: &mut Cx<Self>) -> AnyElement {
        let s = plate_scale(e.viewport);
        let (display, controls) = match e.model {
            DynKind::Zcomp => (
                zcomp_plate::display(e, cx, s),
                zcomp_plate::controls(e, cx, s),
            ),
            DynKind::Clipper => (
                clipper_plate::display(e, cx, s),
                clipper_plate::controls(e, cx, s),
            ),
            DynKind::Transient => (
                transient_plate::display(e, cx, s),
                transient_plate::controls(e, cx, s),
            ),
        };
        plate_frame(s, vec![display, controls])
    }

    fn live_view(e: &KitEditor<Self>) -> DynLiveView {
        DynLiveView {
            params: e.params.clone(),
            vu_output: e.ui.vu_output,
        }
    }

    fn paint_live(
        view: &DynLiveView,
        live: &mut Live,
        bounds: &LiveBounds,
        window: &mut Window,
        cx: &mut App,
    ) {
        paint_dyn_live(view, live, bounds, window, cx);
    }

    fn window_size(self) -> (f32, f32) {
        plate::WINDOW_SIZE
    }

    fn min_size(self) -> (f32, f32) {
        plate::MIN_WINDOW_SIZE
    }
}

// ── Pieces the three panels share ──────────────────────────────────────────

/// A knob cell for `kind`'s param `id` at `value`.
#[allow(clippy::too_many_arguments)]
fn knob_cell(
    cx: &mut Cx<DynKind>,
    kind: DynKind,
    id: &'static str,
    value: f32,
    readout: Option<String>,
    look: KnobLook,
    cell: KnobCell,
    ink: Rgba,
    enabled: bool,
    s: f32,
) -> AnyElement {
    let dial = dial(kind, id);
    knob_reading(
        cx,
        format!("{}-{id}", kind.key()),
        dial,
        value,
        readout,
        look,
        cell,
        ink,
        enabled,
        s,
        move |this: &mut KitEditor<DynKind>, value, cx| this.set_value(id, value, cx),
    )
}

/// The lead knobs' drawing: a thin track and a large face.
fn hero_look(size: f32) -> KnobLook {
    KnobLook::small(size).styled(3.6, 17.5, plate::FACE, plate::FACE_EDGE, 0.8, 7.0, 1.8)
}

/// An output card's knob drawing.
fn output_look(size: f32) -> KnobLook {
    KnobLook::small(size).styled(4.5, 16.0, plate::FACE, plate::FACE_EDGE, 1.2, 7.0, 2.4)
}

/// A caption over a group of controls.
fn section(label: &str, s: f32) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap(px(6.0 * s))
        .child(caption(label, 10.5, 0.1, SOFT, s))
}

/// A line or two of explanation.
fn note(text: impl Into<gpui::SharedString>, s: f32) -> gpui::Div {
    div()
        .text_size(type_px(11.0, s))
        .line_height(px(14.0 * s))
        .text_color(color(SOFT))
        .child(text.into())
}

/// A row of choice pills setting `param` to their index, `selected` lit.
#[allow(clippy::too_many_arguments)]
fn choice_pills(
    e: &KitEditor<DynKind>,
    cx: &mut Cx<DynKind>,
    key: &'static str,
    param: &'static str,
    labels: &[&'static str],
    selected: usize,
    look: PillLook,
    s: f32,
) -> Vec<AnyElement> {
    let accent = color(accent_of(e.model));
    labels
        .iter()
        .enumerate()
        .map(|(index, label)| {
            pill(
                (key, index),
                *label,
                index == selected,
                accent,
                look,
                true,
                s,
                e.click_cb(cx, move |this, cx| this.set_value(param, index as f32, cx)),
            )
            .into_any_element()
        })
        .collect()
}

/// A labelled on/off switch for flag `param`.
fn switch_row(
    e: &KitEditor<DynKind>,
    cx: &mut Cx<DynKind>,
    param: &'static str,
    label: &'static str,
    s: f32,
) -> gpui::Div {
    let id = format!("{}-{param}", e.model.key());
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .child(words(label, 12.5, INK, s))
        .child(switch(
            gpui::ElementId::Name(id.into()),
            e.flag(param),
            color(accent_of(e.model)),
            true,
            s,
            e.click_cb(cx, move |this, cx| this.toggle(param, cx)),
        ))
}

/// A header button that auditions what the plug-in listens to or takes off:
/// lit in the accent while on.
fn listen_button(
    e: &KitEditor<DynKind>,
    cx: &mut Cx<DynKind>,
    param: &'static str,
    label: &'static str,
    s: f32,
) -> AnyElement {
    let on = e.flag(param);
    let accent = color(accent_of(e.model));
    let (bg, border, fg) = if on {
        (accent, accent, color(0x111111))
    } else {
        (color(GROUND), color(WELL_EDGE), color(INK))
    };
    div()
        .id(gpui::ElementId::Name(
            format!("{}-{param}", e.model.key()).into(),
        ))
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
        .child(label)
        .on_click(e.click_cb(cx, move |this, cx| this.toggle(param, cx)))
        .into_any_element()
}

/// A display box with nothing painted at render: the live overlay draws it.
fn live_box(s: f32, left: f32, width: f32) -> gpui::Div {
    div()
        .absolute()
        .left(px(left * s))
        .top(px(2.0 * s))
        .w(px(width * s))
        .h(px(BOX_H * s))
        .bg(color(BOX_GROUND))
        .border(px(1.0))
        .border_color(color(BOX_EDGE))
        .overflow_hidden()
}

/// `child` at `(left, top)` in a box, painted over the live overlay.
fn over(s: f32, left: f32, top: f32, child: impl IntoElement) -> gpui::Div {
    div()
        .absolute()
        .left(px(left * s))
        .top(px(top * s))
        .child(plate::over_live(child))
}

/// A legend swatch: a short line in `ink`.
fn swatch(ink: Rgba, s: f32) -> gpui::Div {
    div().w(px(12.0 * s)).h(px(2.0 * s)).bg(ink)
}

/// The level history box at `left`, `width` wide: its caption, the live
/// reduction readout at its top right and the legend at its foot.
fn history_box(e: &KitEditor<DynKind>, s: f32, left: f32, width: f32) -> gpui::Div {
    let (name, _) = reduction_name(e.model);
    let accent = color(accent_of(e.model));
    live_box(s, left, width)
        .child(
            live_slot(&e.live_bounds, SLOT_HISTORY)
                .absolute()
                .size_full(),
        )
        .child(over(
            s,
            10.0,
            8.0,
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0 * s))
                .child(caption("Level history", 10.5, 0.1, FAINT, s))
                .child(numbers("10 s", 10.0, plate::GHOST, s)),
        ))
        .child(
            div()
                .absolute()
                .right(px(10.0 * s))
                .top(px(4.0 * s))
                .w(px(110.0 * s))
                .h(px(26.0 * s))
                .child(live_slot(&e.live_bounds, SLOT_READOUT).size_full()),
        )
        .child(
            div()
                .absolute()
                .left(px(10.0 * s))
                .bottom(px(6.0 * s))
                .child(plate::over_live(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(14.0 * s))
                        .child(legend(swatch(color(IN_INK), s), "In", s))
                        .child(legend(swatch(color(OUT_INK), s), "Out", s))
                        .child(legend(swatch(accent, s), name, s)),
                )),
        )
}

/// The In, reduction and Out meter rows, live slots [`SLOT_METER`] on.
fn meter_rows(kind: DynKind, bounds: &LiveBounds, s: f32) -> gpui::Div {
    let (name, _) = reduction_name(kind);
    let meter = |index: usize, label: &str| {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0 * s))
            .child(
                div()
                    .w(px(40.0 * s))
                    .child(caption(label, 10.0, 0.08, SOFT, s)),
            )
            .child(
                div()
                    .flex_1()
                    .h(px(14.0 * s))
                    .child(live_slot(bounds, SLOT_METER + index).size_full()),
            )
    };
    div()
        .flex()
        .flex_col()
        .gap(px(6.0 * s))
        .child(meter(0, "In"))
        .child(meter(1, name))
        .child(meter(2, "Out"))
}

// ── The transfer curve ─────────────────────────────────────────────────────

/// The transfer plot inside a box `width` wide: `(x, y, w, h)` in design
/// units, room for the level labels at its left and the foot's below.
fn transfer_plot(width: f32) -> (f32, f32, f32, f32) {
    let left = 40.0;
    (left, PLOT_TOP, width - left - 14.0, PLOT_BOTTOM - PLOT_TOP)
}

/// What the transfer box draws, computed when the panel renders.
struct TransferCurve {
    /// `(input dB, output dB)` across the range.
    points: Vec<(f32, f32)>,
    range: f32,
    /// A vertical line where the action starts on the input axis.
    threshold_in: Option<f32>,
    /// Horizontal lines on the output axis, with their inks.
    levels: Vec<(f32, u32, f32)>,
    accent: u32,
    dim: f32,
}

impl TransferCurve {
    fn new(params: &DynParams) -> Self {
        let kind = params.kind();
        let range = transfer_range(kind);
        let steps = 160;
        let points = (0..=steps)
            .map(|i| {
                let input = range - range * i as f32 / steps as f32;
                (input, params.transfer_db(input).unwrap_or(input))
            })
            .collect();
        let (threshold_in, levels) = match params {
            DynParams::Zcomp(p) => (Some(zcomp::model_coeffs(p).threshold_db), Vec::new()),
            DynParams::Clipper(p) => {
                let mut levels = vec![(p.ceiling_db, OUT_INK, 0.55)];
                if p.threshold_db < p.ceiling_db {
                    levels.push((p.threshold_db, CLIPPER_ACCENT, 0.75));
                }
                (None, levels)
            }
            DynParams::Transient(_) => (None, Vec::new()),
        };
        Self {
            points,
            range,
            threshold_in,
            levels,
            accent: accent_of(kind),
            dim: if params.power() { 1.0 } else { 0.4 },
        }
    }
}

/// The level labels a transfer or history axis carries: every `step` dB
/// from `range` up, short of the ends.
fn level_ticks(range: f32, step: f32) -> Vec<f32> {
    let mut ticks = Vec::new();
    let mut db = range + step;
    while db < -0.01 {
        ticks.push(db);
        db += step;
    }
    ticks
}

fn paint_transfer(
    window: &mut Window,
    cx: &mut App,
    b: Bounds<Pixels>,
    k: f32,
    width: f32,
    curve: &TransferCurve,
) {
    let (x0, y0) = plate::xy(b.origin);
    let at = |x: f32, y: f32| (x0 + x * k, y0 + y * k);
    let (px0, py0, pw, ph) = transfer_plot(width);
    let range = curve.range;
    let x_at = |db: f32| px0 + (db.clamp(range, 0.0) - range) / -range * pw;
    let y_at = |db: f32| py0 + ph - (db.clamp(range, 0.0) - range) / -range * ph;
    let step = if range < -30.0 { 12.0 } else { 6.0 };
    let font = plate::mono(FontWeight::NORMAL);
    let mut foot = Vec::new();
    for db in level_ticks(range, step) {
        paint_polyline(
            window,
            &[at(x_at(db), py0), at(x_at(db), py0 + ph)],
            1.0,
            color(GRID),
            None,
        );
        paint_polyline(
            window,
            &[at(px0, y_at(db)), at(px0 + pw, y_at(db))],
            1.0,
            color(GRID),
            None,
        );
        let text = plate::signed(db).replace(".0", "");
        paint_text(
            window,
            cx,
            &text,
            font.clone(),
            10.0 * k,
            color(AXIS_INK),
            at(px0 - 6.0, y_at(db) - 6.5),
            Align::Right,
        );
        foot.push((x_at(db), text));
    }
    crate::components::time_plate::paint_axis(window, cx, b, k, &foot);
    // The plot's frame: 0 dBFS out along the top, in at the right.
    paint_polyline(
        window,
        &[at(px0, py0), at(px0 + pw, py0), at(px0 + pw, py0 + ph)],
        1.0,
        color(0x3a3a3a),
        None,
    );
    // Unity.
    paint_polyline(
        window,
        &[at(px0, py0 + ph), at(px0 + pw, py0)],
        1.0,
        color_a(0xffffff, 0.2),
        Some((3.0 * k, 3.0 * k)),
    );
    if let Some(db) = curve.threshold_in.filter(|db| *db > range) {
        let x = x_at(db);
        paint_polyline(
            window,
            &[at(x, py0), at(x, py0 + ph)],
            1.0,
            color_a(curve.accent, 0.6 * curve.dim),
            Some((4.0 * k, 3.0 * k)),
        );
    }
    for &(db, ink, alpha) in &curve.levels {
        let y = y_at(db);
        paint_polyline(
            window,
            &[at(px0, y), at(px0 + pw, y)],
            1.0,
            color_a(ink, alpha * curve.dim),
            Some((4.0 * k, 3.0 * k)),
        );
    }
    let line: Vec<(f32, f32)> = curve
        .points
        .iter()
        .map(|&(input, output)| at(x_at(input), y_at(output)))
        .collect();
    let mut area = line.clone();
    area.push(at(px0 + pw, py0 + ph));
    area.push(at(px0, py0 + ph));
    paint_polygon(window, &area, color_a(curve.accent, 0.12 * curve.dim));
    paint_polyline(
        window,
        &line,
        2.0 * k,
        color_a(curve.accent, curve.dim),
        None,
    );
}

/// The live operating point on a transfer box: the input peak against the
/// output the static curve gives it, traced to both axes.
fn paint_operating_point(
    window: &mut Window,
    b: Bounds<Pixels>,
    width: f32,
    live: &Live,
    params: &DynParams,
) {
    let Some(frame) = live.frame else {
        return;
    };
    if !params.power() || frame.in_peak <= 1.0e-5 {
        return;
    }
    let k = f32::from(b.size.width) / width;
    let (x0, y0) = plate::xy(b.origin);
    let at = |x: f32, y: f32| (x0 + x * k, y0 + y * k);
    let range = transfer_range(params.kind());
    let (px0, py0, pw, ph) = transfer_plot(width);
    let in_db = (20.0 * frame.in_peak.log10()).clamp(range, 0.0);
    let out_db = params.transfer_db(in_db).unwrap_or(in_db).clamp(range, 0.0);
    let x = px0 + (in_db - range) / -range * pw;
    let y = py0 + ph - (out_db - range) / -range * ph;
    let ink = color(accent_of(params.kind()));
    let trace = color_a(0xffffff, 0.45);
    paint_polyline(
        window,
        &[at(x, py0 + ph), at(x, y)],
        1.0,
        trace,
        Some((3.0 * k, 3.0 * k)),
    );
    paint_polyline(
        window,
        &[at(px0, y), at(x, y)],
        1.0,
        trace,
        Some((3.0 * k, 3.0 * k)),
    );
    let (dx, dy) = at(x, y);
    paint_dot(window, dx, dy, 5.5 * k, color(0x111111));
    paint_dot(window, dx, dy, 4.0 * k, ink);
}

// ── Live ───────────────────────────────────────────────────────────────────

/// Levels the history marks: where each plug-in holds or starts to act.
fn markers(params: &DynParams) -> Vec<(f32, String, u32)> {
    match params {
        DynParams::Zcomp(p) => {
            let db = zcomp::model_coeffs(p).threshold_db;
            vec![(db, format!("thr {}", plate::signed(db)), ZCOMP_ACCENT)]
        }
        DynParams::Clipper(p) => {
            let mut marks = vec![(
                p.ceiling_db,
                format!("ceil {}", plate::signed(p.ceiling_db)),
                OUT_INK,
            )];
            // The threshold marks a level only under the ceiling.
            if p.threshold_db < p.ceiling_db {
                marks.push((
                    p.threshold_db,
                    format!("thr {}", plate::signed(p.threshold_db)),
                    CLIPPER_ACCENT,
                ));
            }
            marks
        }
        DynParams::Transient(_) => Vec::new(),
    }
}

/// The last ten seconds, newest at the right: the input peak as a shaded
/// area, what the plug-in takes off hanging from the top, the output peak as
/// a line, and the levels it works at.
fn paint_history(
    window: &mut Window,
    cx: &mut App,
    b: Bounds<Pixels>,
    live: &Live,
    params: &DynParams,
    width: f32,
) {
    let kind = params.kind();
    let k = (f32::from(b.size.width) / width).max(0.01);
    let (x0, y0) = plate::xy(b.origin);
    let at = |x: f32, y: f32| (x0 + x * k, y0 + y * k);
    let floor = history_floor(kind);
    let y_of = |db: f32| PLOT_TOP + (db / floor).clamp(0.0, 1.0) * (PLOT_BOTTOM - PLOT_TOP);
    let font = plate::mono(FontWeight::NORMAL);
    let accent = accent_of(kind);
    let alpha = if params.power() { 1.0 } else { 0.45 };

    let ticks = level_ticks(floor, -floor / 4.0);
    for &db in &ticks {
        let y = y_of(db);
        paint_polyline(window, &[at(0.0, y), at(width, y)], 1.0, color(GRID), None);
    }
    paint_polyline(
        window,
        &[at(0.0, PLOT_TOP), at(width, PLOT_TOP)],
        1.0,
        color(0x3a3a3a),
        None,
    );

    let count = live.history_len().min(HISTORY);
    let step = width / (HISTORY - 1) as f32;
    let points: Vec<(f32, f32, f32, f32)> = (0..count)
        .rev()
        .map(|age| {
            let point = live.history_at(age);
            (
                width - age as f32 * step,
                point.in_db,
                point.out_db,
                point.reduction_db,
            )
        })
        .collect();
    if points.len() >= 2 {
        let first = points[0].0;
        let input: Vec<(f32, f32)> = points
            .iter()
            .map(|&(x, db, _, _)| at(x, y_of(db)))
            .collect();
        let mut area = input.clone();
        area.push(at(width, PLOT_BOTTOM));
        area.push(at(first, PLOT_BOTTOM));
        paint_polygon(window, &area, color_a(0xffffff, 0.07 * alpha));
        paint_polyline(window, &input, 1.0 * k, color_a(IN_INK, alpha), None);

        if points.iter().any(|p| p.3 > 0.05) {
            let taken: Vec<(f32, f32)> = points
                .iter()
                .map(|&(x, _, _, gr)| at(x, y_of(-gr)))
                .collect();
            let mut area = taken.clone();
            area.push(at(width, PLOT_TOP));
            area.push(at(first, PLOT_TOP));
            paint_polygon(window, &area, color_a(accent, 0.3 * alpha));
            paint_polyline(window, &taken, 1.6 * k, color_a(accent, alpha), None);
        }
        let output: Vec<(f32, f32)> = points
            .iter()
            .map(|&(x, _, db, _)| at(x, y_of(db)))
            .collect();
        paint_polyline(window, &output, 1.2 * k, color_a(OUT_INK, alpha), None);
    }

    // The labels go over the traces, each on a patch of the box's ground.
    let tag =
        |window: &mut Window, cx: &mut App, text: &str, ink: Rgba, x: f32, y: f32, align: Align| {
            // IBM Plex Mono advances 0.6 em.
            let w = text.chars().count() as f32 * 6.0 + 6.0;
            let left = match align {
                Align::Left => x - 3.0,
                Align::Center => x - w * 0.5,
                Align::Right => x - w + 3.0,
            };
            let (bx, by) = at(left, y);
            paint_rect(window, (bx, by, w * k, 13.0 * k), color_a(BOX_GROUND, 0.82));
            paint_text(
                window,
                cx,
                text,
                font.clone(),
                10.0 * k,
                ink,
                at(x, y),
                align,
            );
        };
    for &db in &ticks {
        let text = format!("{db:.0}").replace('-', "\u{2212}");
        tag(
            window,
            cx,
            &text,
            color(AXIS_INK),
            6.0,
            y_of(db) - 13.0,
            Align::Left,
        );
    }
    for (db, label, ink) in markers(params) {
        if db <= floor {
            continue;
        }
        let y = y_of(db);
        paint_polyline(
            window,
            &[at(0.0, y), at(width, y)],
            1.0,
            color_a(ink, 0.8),
            Some((5.0 * k, 3.0 * k)),
        );
        // A tag near the top goes under its line, clear of the readout.
        let tag_y = if y - 15.0 < PLOT_TOP + 2.0 {
            y + 3.0
        } else {
            y - 15.0
        };
        tag(
            window,
            cx,
            &label,
            color(ink),
            width - 8.0,
            tag_y,
            Align::Right,
        );
    }
}

/// The reduction readout at the history's top right.
fn paint_readout(
    window: &mut Window,
    cx: &mut App,
    b: Bounds<Pixels>,
    live: &Live,
    kind: DynKind,
    s: f32,
) {
    let Some(frame) = live.frame else {
        return;
    };
    let (_, sign) = reduction_name(kind);
    let (x, y) = plate::xy(b.origin);
    let w = f32::from(b.size.width);
    let db = frame.gain_reduction_db.max(0.0);
    let (text, ink) = if db > 0.05 {
        (format!("{sign}{db:.1}"), color(accent_of(kind)))
    } else {
        ("0.0".to_string(), color(FAINT))
    };
    paint_text(
        window,
        cx,
        &text,
        plate::mono(FontWeight::NORMAL),
        20.0 * s,
        ink,
        (x + w, y),
        Align::Right,
    );
}

/// A reduction meter row's bar, filling from its right end over 24 dB, and
/// its reading.
fn paint_reduction_row(
    window: &mut Window,
    cx: &mut App,
    b: Bounds<Pixels>,
    db: f32,
    kind: DynKind,
    s: f32,
) {
    let (_, sign) = reduction_name(kind);
    let (x, y) = plate::xy(b.origin);
    let (w, h) = (f32::from(b.size.width), f32::from(b.size.height));
    let bar_w = (w - 44.0 * s).max(0.0);
    let bar_y = y + (h - 7.0 * s) * 0.5;
    window.paint_quad(
        gpui::fill(plate::rect(x, bar_y, bar_w, 7.0 * s), color(0x111111))
            .corner_radii(px(2.0 * s)),
    );
    let fill = (db / REDUCTION_SPAN_DB).clamp(0.0, 1.0) * bar_w;
    paint_rect(
        window,
        (x + bar_w - fill, bar_y, fill, 7.0 * s),
        color(accent_of(kind)),
    );
    let text = if db > 0.05 {
        format!("{sign}{db:.1}")
    } else {
        "0.0".to_string()
    };
    paint_text(
        window,
        cx,
        &text,
        plate::mono(FontWeight::NORMAL),
        10.5 * s,
        color(INK),
        (x + w, y + (h - 13.0 * s) * 0.5),
        Align::Right,
    );
}

fn paint_dyn_live(
    view: &DynLiveView,
    live: &mut Live,
    bounds: &LiveBounds,
    window: &mut Window,
    cx: &mut App,
) {
    let viewport = window.viewport_size();
    let s = plate_scale((f32::from(viewport.width), f32::from(viewport.height)));
    let params = &view.params;
    let kind = params.kind();
    let history_w = match kind {
        DynKind::Zcomp => NARROW_W,
        _ => WIDE_W,
    };
    if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_HISTORY)) {
        paint_history(window, cx, b, live, params, history_w);
    }
    if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_READOUT)) {
        paint_readout(window, cx, b, live, kind, s);
    }
    if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_TRANSFER)) {
        let width = match kind {
            DynKind::Zcomp => ZCOMP_TRANSFER_W,
            _ => NARROW_W,
        };
        paint_operating_point(window, b, width, live, params);
    }
    if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_VU)) {
        zcomp_plate::paint_needle(window, cx, b, live, params, view.vu_output);
    }
    if let Some(frame) = live.frame {
        let accent = color(accent_of(kind));
        let level = |peak: f32| 20.0 * peak.max(1.0e-9).log10();
        if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_METER)) {
            paint_meter(window, cx, b, level(frame.in_peak), accent, s);
        }
        if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_METER + 1)) {
            paint_reduction_row(window, cx, b, frame.gain_reduction_db.max(0.0), kind, s);
        }
        if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_METER + 2)) {
            paint_meter(window, cx, b, level(frame.out_peak), accent, s);
        }
    }
}

// ── Opening ────────────────────────────────────────────────────────────────

macro_rules! opener {
    ($name:ident, $kind:expr, $doc:literal) => {
        #[doc = $doc]
        pub fn $name(
            owner_bounds: Option<Bounds<Pixels>>,
            key: PluginInstanceKey,
            identity: ShellIdentity,
            host_ops: BuiltinEditorHostOps,
            on_close: Arc<dyn Fn(&mut Window, &mut App)>,
            cx: &mut App,
        ) -> Result<gpui::WindowHandle<DynPlateWindow>, String> {
            plate::ensure_fonts(cx);
            open_kit_editor($kind, owner_bounds, key, identity, host_ops, on_close, cx)
        }
    };
}

opener!(
    open_zcomp_editor,
    DynKind::Zcomp,
    "Opens with a track's Z-Comp insert."
);
opener!(
    open_clipper67_editor,
    DynKind::Clipper,
    "Opens with a track's 67Clipper insert."
);
opener!(
    open_transient_editor,
    DynKind::Transient,
    "Opens with a track's Transient insert."
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_dial_spans_its_descriptor_row_and_round_trips_its_default() {
        for (kind, dial) in dial_table() {
            let (_, min, max, default) = kind.descriptor_row(dial.id).unwrap();
            assert_eq!((dial.min, dial.max), (min, max), "{kind:?} {}", dial.id);
            assert_eq!(dial.default, default, "{kind:?} {}", dial.id);
            let back = dial.at(dial.norm(default));
            assert!(
                (back - default).abs() <= 0.011 * default.abs().max(1.0),
                "{kind:?} {}: {default} came back {back}",
                dial.id
            );
        }
    }

    #[test]
    fn every_dial_edits_a_param_the_dsp_takes() {
        for (kind, dial) in dial_table() {
            let params = DynParams::defaults(*kind);
            let value = dial.at(0.75);
            let edited = params.with(dial.id, value);
            assert!(
                (edited.value(dial.id) - value).abs() <= 0.011 * value.abs().max(1.0),
                "{kind:?} {}",
                dial.id
            );
        }
    }

    #[test]
    fn bipolar_dials_rest_at_their_middle() {
        for (kind, dial) in dial_table().iter().filter(|(_, d)| d.bipolar) {
            assert!(
                (dial.norm(0.0) - 0.5).abs() < 1.0e-6,
                "{kind:?} {}",
                dial.id
            );
        }
    }

    #[test]
    fn the_attack_reads_under_a_millisecond() {
        let attack = dial(DynKind::Zcomp, "attackMs");
        assert_eq!(attack.text(0.2), "0.20 ms");
        assert_eq!(attack.text(2_500.0), "2.50 s");
        assert!(attack.at(attack.norm(0.05)) < 0.06);
    }

    #[test]
    fn closing_lets_go_of_sc_listen_only() {
        let listening = DynParams::defaults(DynKind::Zcomp).with("scListen", 1.0);
        let closed = DynKind::Zcomp.on_close(&listening).unwrap();
        assert_eq!(closed.value("scListen"), 0.0);
        assert!(DynKind::Zcomp
            .on_close(&DynParams::defaults(DynKind::Zcomp))
            .is_none());
        let delta = DynParams::defaults(DynKind::Clipper).with("delta", 1.0);
        assert!(DynKind::Clipper.on_close(&delta).is_none());
    }

    #[test]
    fn the_transfer_curve_follows_the_dsp() {
        let params = DynParams::defaults(DynKind::Clipper);
        let curve = TransferCurve::new(&params);
        assert_eq!(curve.points.first().unwrap().0, -24.0);
        assert!((curve.points.last().unwrap().0).abs() < 1.0e-4);
        for &(input, output) in &curve.points {
            assert_eq!(output, params.transfer_db(input).unwrap());
        }
    }
}

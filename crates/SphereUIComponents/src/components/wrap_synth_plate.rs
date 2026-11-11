//! WrapSynth's editor, on the plate.
//!
//! The standard [`plate::PLATE_W`] × [`plate::PLATE_H`] artboard: the well
//! along the top — the header with the page tabs, the preset and the power,
//! over four display boxes — the deck of cards under it, and the keyboard
//! strip along the foot. Three pages, each laid out in the same four
//! columns, a display box over the card it draws:
//!
//! * **Synth** — the two wavetable oscillators, each drawn as the cycle the
//!   DSP plays ([`wrapsynth::osc_cycle`]); the filter, its response drawn by
//!   the DSP's own prototype ([`wrapsynth::filter::response_db`]); the amp
//!   envelope, drawn with the curves it really follows; the output.
//! * **Modulation** — the filter and modulation envelopes and both LFOs,
//!   then the eight-slot matrix.
//! * **Voice & FX** — voicing (poly/mono/legato, glide, bend, velocity,
//!   analog drift), then drive, chorus, delay and reverb, each drawn from its
//!   controls: the drive's transfer curve, the chorus sweep, the delay's
//!   repeats and the reverb's decay.
//!
//! Every display is painted from the params when the panel renders, never
//! per frame. This file only renders: the state is the insert's params, held
//! by [`crate::components::wrap_synth_window::WrapSynthEditorWindow`], and
//! every edit is one complete [`Params`] handed to its callback.

use std::rc::Rc;
use std::sync::OnceLock;

use gpui::{
    canvas, div, px, AnyElement, App, AppContext, Bounds, FontWeight, InteractiveElement,
    IntoElement, ParentElement, Pixels, Rgba, ScrollWheelEvent, SharedString,
    StatefulInteractiveElement, Styled, Window,
};

use crate::components::fader::{drag_to, is_reset_click, FaderDrag};
use crate::components::instrument_plate::{
    self as kit, Click, Keyboard, Note, Step, DECK_BOTTOM, HEADER_H, KEYBOARD_HIGHEST_ROOT,
    WELL_TOP,
};
use crate::components::plate::{
    self, caption, card, card_title, color, color_a, icon, numbers, paint_polygon, paint_polyline,
    paint_rect, paint_text, plate_frame, square, type_px, Align, Dial, DialUnit, KnobCell,
    KnobLook, KnobSet, PillLook, CHEVRON_LEFT, CHEVRON_RIGHT, FAINT, GROUND, ICON, INK, SOFT,
    WELL_EDGE, WHITE,
};
use crate::components::plate_parts::{note, section};
use crate::components::time_plate::{
    at, at_right, fmt_time, nice_ceiling, nice_step, x_of_hz, AXIS_INK, BOX_EDGE, BOX_GROUND,
    FREQ_GRID, GRID,
};
use wrapsynth::envelope::{segment_progress, ATTACK_RATIO, DECAY_RATIO};
use wrapsynth::{
    FactoryPreset, FilterModel, FilterType, LfoShape, ModDest, ModSource, Params, VoiceMode,
    Wavetable, MOD_SLOTS,
};

/// WrapSynth's accent.
pub const ACCENT: u32 = 0x4cc9f0;
/// The second ink a display draws in: the right side, the other tap.
const SECOND: u32 = 0xd9d9d9;

/// Four octaves from C2.
const KEYBOARD_DEFAULT_ROOT: u8 = 36;
const CUTOFF_MIN_HZ: f32 = wrapsynth::CUTOFF_MIN_HZ;
const CUTOFF_MAX_HZ: f32 = wrapsynth::CUTOFF_MAX_HZ;

// ── Layout, in design units ────────────────────────────────────────────────

/// The display boxes, under the header, and the well around them.
const BOX_TOP: f32 = 2.0;
const BOX_H: f32 = 140.0;
const WELL_H: f32 = HEADER_H + BOX_TOP + BOX_H + 14.0;
/// The deck, from under the well to [`DECK_BOTTOM`].
const DECK_TOP: f32 = WELL_TOP + WELL_H + 7.0;
const DECK_H: f32 = DECK_BOTTOM - DECK_TOP;
const DECK_PAD_Y: f32 = 12.0;
/// The four columns the boxes and the cards share: the boxes sit 16 in from
/// the well's edge, inside its 1-unit rim; the deck has no rim.
const BOX_INSET: f32 = 16.0;
const DECK_PAD_X: f32 = BOX_INSET + 1.0;
const COL_GAP: f32 = 14.0;
const COL_W: f32 = (plate::PLATE_W - 13.0 - 2.0 - 2.0 * BOX_INSET - 3.0 * COL_GAP) / 4.0;
/// The plot inside a box: the caption row above, axis labels (when it has
/// them) below.
const PLOT_TOP: f32 = 30.0;
const PLOT_BOTTOM: f32 = BOX_H - 10.0;
const AXIS_BOTTOM: f32 = BOX_H - 24.0;
/// The Modulation page's top row of cards, fixed so the matrix under it
/// does not move when an LFO's rate turns into a division.
const MOD_ROW_H: f32 = 204.0;
/// The Voice & FX page's voice card.
const VOICE_ROW_H: f32 = 132.0;

/// `n` columns and the gaps between them.
fn span(n: usize) -> f32 {
    COL_W * n as f32 + COL_GAP * (n.saturating_sub(1)) as f32
}

fn accent() -> Rgba {
    color(ACCENT)
}

// ── State ──────────────────────────────────────────────────────────────────

/// The page the panel shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SynthPage {
    #[default]
    Synth,
    Modulation,
    VoiceFx,
}

impl SynthPage {
    const ALL: [SynthPage; 3] = [Self::Synth, Self::Modulation, Self::VoiceFx];

    fn label(self) -> &'static str {
        match self {
            Self::Synth => "Synth",
            Self::Modulation => "Modulation",
            Self::VoiceFx => "Voice & FX",
        }
    }
}

/// What the panel draws: the insert's params, the page and the keyboard's
/// state.
#[derive(Clone)]
pub struct WrapSynthPanelState {
    pub params: Params,
    pub page: SynthPage,
    pub keyboard_root: u8,
    pub active_notes: Vec<u8>,
}

impl Default for WrapSynthPanelState {
    fn default() -> Self {
        Self {
            params: wrapsynth::default_params(),
            page: SynthPage::default(),
            keyboard_root: KEYBOARD_DEFAULT_ROOT,
            active_notes: Vec::new(),
        }
    }
}

impl WrapSynthPanelState {
    pub fn shift_keyboard_octave(&mut self, delta: i32) {
        let root = self.keyboard_root as i32 + delta * 12;
        self.keyboard_root = root.clamp(0, KEYBOARD_HIGHEST_ROOT as i32) as u8;
    }
}

/// Takes one complete params value, so a knob drag cannot land a partial
/// edit.
pub type SetParams = Rc<dyn Fn(Params, &mut App)>;

#[derive(Clone)]
pub struct WrapSynthCallbacks {
    pub on_set_params: SetParams,
    pub on_page: Rc<dyn Fn(SynthPage, &mut App)>,
    pub on_note_on: Note,
    pub on_note_off: Note,
    /// `-1` / `+1` octave.
    pub on_shift_octave: Step,
}

/// The factory bank, built once.
fn presets() -> &'static [FactoryPreset] {
    static BANK: OnceLock<Vec<FactoryPreset>> = OnceLock::new();
    BANK.get_or_init(wrapsynth::factory_presets)
}

/// Which preset `p` is, if it is one unedited.
fn preset_index(p: &Params) -> Option<usize> {
    presets().iter().position(|preset| {
        let mut ours = *p;
        ours.power = preset.params.power;
        ours == preset.params
    })
}

/// The factory preset `delta` steps from `p`, keeping its power. An edited
/// patch steps to the first preset either side.
fn stepped_preset(p: &Params, delta: i32) -> Params {
    let bank = presets();
    let len = bank.len() as i32;
    let index = match preset_index(p) {
        Some(index) => (index as i32 + delta).rem_euclid(len),
        None if delta > 0 => 0,
        None => len - 1,
    };
    Params {
        power: p.power,
        ..bank[index as usize].params
    }
}

// ── The whole plate ────────────────────────────────────────────────────────

/// What every control on a page is built from: the patch as it is now, where
/// edits go, and the plate's scale.
#[derive(Clone)]
struct Ctx {
    p: Params,
    set: SetParams,
    s: f32,
}

impl Ctx {
    /// A click that sets `next`.
    fn click(&self, next: Params) -> Click {
        let set = self.set.clone();
        Rc::new(move |_window: &mut Window, app: &mut App| set(next, app))
    }

    /// A step (`-1` / `+1`, or an index) that sets what `next` makes of the
    /// patch.
    fn step(&self, next: impl Fn(Params, i32) -> Params + 'static) -> Step {
        let (p, set) = (self.p, self.set.clone());
        Rc::new(move |delta: i32, _window: &mut Window, app: &mut App| set(next(p, delta), app))
    }
}

/// WrapSynth's plate at scale `s`.
pub fn wrap_synth_plate(panel: &WrapSynthPanelState, cb: WrapSynthCallbacks, s: f32) -> AnyElement {
    let c = Ctx {
        p: panel.params,
        set: cb.on_set_params.clone(),
        s,
    };
    // Off, the synth is silent: everything stays editable, but reads as
    // asleep.
    let dim = if c.p.power { 1.0 } else { 0.4 };
    let (boxes, cards) = match panel.page {
        SynthPage::Synth => (synth_boxes(&c, dim), synth_deck(&c)),
        SynthPage::Modulation => (modulation_boxes(&c, dim), modulation_deck(&c)),
        SynthPage::VoiceFx => (voice_fx_boxes(&c, dim), voice_fx_deck(&c)),
    };
    let well = plate::well(s, WELL_TOP, WELL_H)
        .child(header(panel, &cb, &c))
        .child(div().relative().flex_1().children(boxes))
        .into_any_element();
    let deck = plate::deck(s, DECK_TOP, DECK_H)
        .flex_col()
        .px(px(DECK_PAD_X * s))
        .py(px(DECK_PAD_Y * s))
        .gap(px(COL_GAP * s))
        .when_off(!c.p.power)
        .children(cards)
        .into_any_element();
    plate_frame(s, vec![well, deck, keyboard(panel, &cb, s)])
}

/// Dims a region while the synth is off.
trait WhenOff {
    fn when_off(self, off: bool) -> Self;
}

impl WhenOff for gpui::Div {
    fn when_off(self, off: bool) -> Self {
        if off {
            self.opacity(0.55)
        } else {
            self
        }
    }
}

fn header(panel: &WrapSynthPanelState, cb: &WrapSynthCallbacks, c: &Ctx) -> AnyElement {
    let s = c.s;
    let selected = SynthPage::ALL
        .iter()
        .position(|page| *page == panel.page)
        .unwrap_or(0);
    let labels: Vec<&'static str> = SynthPage::ALL.iter().map(|page| page.label()).collect();
    let on_page = cb.on_page.clone();
    let tabs = kit::tabs(
        "wrap-synth-page",
        &labels,
        selected,
        accent(),
        s,
        Rc::new(move |index: i32, _window: &mut Window, app: &mut App| {
            if let Some(page) = SynthPage::ALL.get(index as usize) {
                on_page(*page, app)
            }
        }),
    );
    let p = c.p;
    kit::header(
        "wrap-synth-power",
        s,
        "WrapSynth",
        "Wavetable synth",
        accent(),
        vec![tabs],
        vec![preset_bar(c)],
        Some((p.power, c.click(with(p, |p| p.power = !p.power)))),
    )
}

/// `‹  Preset  ›`: steps through the factory bank. An edited patch reads as
/// "Custom".
fn preset_bar(c: &Ctx) -> AnyElement {
    let s = c.s;
    let label = preset_index(&c.p).map_or("Custom", |index| presets()[index].name);
    let arrow = |index: usize, path: &'static str, delta: i32| {
        let on_step = c.step(move |p, _| stepped_preset(&p, delta));
        square(
            ("wrap-synth-preset", index),
            28.0,
            icon(path, (12.0, 12.0), (12.0, 12.0), 1.5, color(ICON), s),
            true,
            s,
            move |_, window, app| on_step(delta, window, app),
        )
    };
    let chip = div()
        .flex()
        .items_center()
        .w(px(150.0 * s))
        .h(px(28.0 * s))
        .px(px(10.0 * s))
        .rounded(px(3.0 * s))
        .border(px(1.0))
        .border_color(color(WELL_EDGE))
        .bg(color(GROUND))
        .text_size(type_px(12.5, s))
        .text_color(color(INK))
        .child(div().flex_1().min_w(px(0.0)).truncate().child(label));
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(4.0 * s))
        .child(arrow(0, CHEVRON_LEFT, -1))
        .child(chip)
        .child(arrow(1, CHEVRON_RIGHT, 1))
        .into_any_element()
}

fn keyboard(panel: &WrapSynthPanelState, cb: &WrapSynthCallbacks, s: f32) -> AnyElement {
    let hint = if panel.params.power {
        "Hold a key to hear the patch; the track's MIDI plays it too"
    } else {
        "Switch the synth on to play it"
    };
    kit::keyboard(
        Keyboard {
            id: "wrap-synth-keys",
            root: panel.keyboard_root,
            active: &panel.active_notes,
            playable: panel.params.power,
            mark: &|_| None,
            mapped: None,
            hint: hint.to_string(),
            accent: accent(),
            note_on: cb.on_note_on.clone(),
            note_off: cb.on_note_off.clone(),
            on_shift: cb.on_shift_octave.clone(),
        },
        s,
    )
}

// ── Controls ───────────────────────────────────────────────────────────────

fn with(p: Params, apply: impl FnOnce(&mut Params)) -> Params {
    let mut next = p;
    apply(&mut next);
    wrapsynth::ipc::sanitize_params(&mut next);
    next
}

/// A knob's travel: its position `0..1` (or `-1..1`) of some mapping of a
/// field, at a resolution of 1 / [`TRAVEL`]. The mapping — logarithmic,
/// squared — is the knob's own; the dial only carries where it sits.
const TRAVEL: f32 = 1_000.0;

/// A dial over a knob position `0..1`, reset to `default`.
fn travel(label: &'static str, default: f32) -> Dial {
    Dial::new(
        "wrap-synth",
        label,
        0.0,
        TRAVEL,
        default * TRAVEL,
        DialUnit::Plain,
    )
}

/// A centred dial over `-1..1`, reset to `default`.
fn signed_travel(label: &'static str, default: f32) -> Dial {
    Dial::new(
        "wrap-synth",
        label,
        -TRAVEL,
        TRAVEL,
        default * TRAVEL,
        DialUnit::Plain,
    )
    .bipolar()
}

/// A knob's drawing and its cell.
type KnobSize = (KnobLook, KnobCell);
/// Four knobs across a column.
const SMALL4: KnobSize = (KnobLook::small(42.0), KnobCell::new(50.0, 9.5, 11.0));
/// Three, or two, across a column.
const SMALL3: KnobSize = (KnobLook::small(42.0), KnobCell::new(64.0, 9.5, 11.0));
/// A card's lead knobs, up to three across a column.
const LEAD: KnobSize = (
    KnobLook::small(54.0).styled(3.8, 17.0, plate::FACE, plate::FACE_EDGE, 0.9, 7.0, 1.9),
    KnobCell::new(64.0, 10.0, 12.0),
);

/// A knob cell over `dial` at `position` (its travel), printing `readout`;
/// `apply` takes the new position.
fn knob(
    c: &Ctx,
    key: impl Into<SharedString>,
    dial: Dial,
    position: f32,
    readout: String,
    (look, cell): KnobSize,
    apply: impl Fn(&mut Params, f32) + 'static,
) -> AnyElement {
    let (p, set) = (c.p, c.set.clone());
    let on_set: KnobSet = Rc::new(move |value: f32, app: &mut App| {
        set(with(p, |p| apply(p, value / TRAVEL)), app);
    });
    plate::knob_with(
        key,
        dial,
        position * TRAVEL,
        Some(readout),
        look,
        cell,
        accent(),
        true,
        c.s,
        on_set,
    )
}

/// Knob cells spread across a card.
fn knobs(cells: Vec<AnyElement>) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_around()
        .children(cells)
}

/// A row of equal pills, `selected` lit; `on_select` takes the index.
fn segmented(
    id: &'static str,
    labels: &[&str],
    selected: Option<usize>,
    s: f32,
    on_select: Step,
) -> gpui::Div {
    plate::columns(
        4.0,
        s,
        labels
            .iter()
            .enumerate()
            .map(|(index, label)| {
                let on_select = on_select.clone();
                plate::pill(
                    (id, index),
                    label.to_string(),
                    selected == Some(index),
                    accent(),
                    PillLook::words(24.0, 2.0, 11.5),
                    true,
                    s,
                    move |_, window, app| on_select(index as i32, window, app),
                )
                .w_full()
                .into_any_element()
            })
            .collect(),
    )
}

/// A card's title with its on/off switch at the right.
fn switched_title(id: &'static str, title: &str, on: bool, c: &Ctx, next: Params) -> gpui::Div {
    let click = c.click(next);
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .child(card_title(title, c.s))
        .child(plate::switch(
            id,
            on,
            accent(),
            true,
            c.s,
            move |_, window, app| click(window, app),
        ))
}

/// A card's head with a short note under it.
fn noted(head: gpui::Div, text: &'static str, s: f32) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap(px(10.0 * s))
        .child(head)
        .child(note(text, s))
}

/// A card on the deck, `width` design units wide (`None`: it fills).
fn deck_card(s: f32, width: Option<f32>) -> gpui::Div {
    let card = card(s).gap(px(10.0 * s)).py(px(12.0 * s));
    match width {
        Some(width) => card.w(px(width * s)).flex_shrink_0(),
        None => card.flex_1(),
    }
}

/// `‹  value  ›` without a label: a matrix slot's source or destination.
fn picker(
    id: &'static str,
    index: usize,
    value: &str,
    width: f32,
    s: f32,
    on_step: Step,
) -> gpui::Div {
    let arrow = |which: usize, path: &'static str, delta: i32| {
        let on_step = on_step.clone();
        square(
            (id, index * 2 + which),
            24.0,
            icon(path, (12.0, 12.0), (12.0, 12.0), 1.5, color(ICON), s),
            true,
            s,
            move |_, window, app| on_step(delta, window, app),
        )
    };
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(3.0 * s))
        .child(arrow(0, CHEVRON_LEFT, -1))
        .child(
            div()
                .w(px(width * s))
                .h(px(24.0 * s))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(3.0 * s))
                .border(px(1.0))
                .border_color(color(WELL_EDGE))
                .bg(color(GROUND))
                .text_size(type_px(12.0, s))
                .text_color(color(WHITE))
                .child(div().truncate().child(value.to_string())),
        )
        .child(arrow(1, CHEVRON_RIGHT, 1))
}

fn percent(value: f32) -> String {
    format!("{:.0}%", value * 100.0)
}

fn signed_percent(value: f32) -> String {
    format!("{:+.0}%", value * 100.0)
}

fn format_rate(hz: f32) -> String {
    if hz < 1.0 {
        format!("{hz:.2} Hz")
    } else {
        format!("{hz:.1} Hz")
    }
}

fn division_label(division: u8) -> String {
    echospace::DIVISION_LABELS
        .get(usize::from(division))
        .copied()
        .unwrap_or("1/8")
        .to_string()
}

fn cycle<T: Copy + PartialEq>(all: &[T], current: T, delta: i32) -> T {
    let at = all.iter().position(|item| *item == current).unwrap_or(0) as i32;
    all[(at + delta).rem_euclid(all.len() as i32) as usize]
}

/// Cutoff knob position (0..1) ↔ Hz, logarithmic: each tenth of the sweep is
/// the same musical interval.
pub(crate) fn cutoff_from_position(position: f32) -> f32 {
    CUTOFF_MIN_HZ * (CUTOFF_MAX_HZ / CUTOFF_MIN_HZ).powf(position.clamp(0.0, 1.0))
}

pub(crate) fn cutoff_position(hz: f32) -> f32 {
    let hz = hz.clamp(CUTOFF_MIN_HZ, CUTOFF_MAX_HZ);
    ((hz / CUTOFF_MIN_HZ).ln() / (CUTOFF_MAX_HZ / CUTOFF_MIN_HZ).ln()).clamp(0.0, 1.0)
}

/// A time knob's position (0..1) ↔ ms over `min..max`, squared so the short
/// times a pluck lives on get most of the sweep.
pub(crate) fn time_from_position(position: f32, (min, max): (f32, f32)) -> f32 {
    let t = position.clamp(0.0, 1.0);
    min + (max - min) * t * t
}

pub(crate) fn time_position(value: f32, (min, max): (f32, f32)) -> f32 {
    ((value.clamp(min, max) - min) / (max - min)).sqrt()
}

/// A rate knob's position ↔ Hz, logarithmic over `min..max`.
fn rate_from_position(position: f32, (min, max): (f32, f32)) -> f32 {
    min * (max / min).powf(position.clamp(0.0, 1.0))
}

fn rate_position(hz: f32, (min, max): (f32, f32)) -> f32 {
    ((hz.clamp(min, max) / min).ln() / (max / min).ln()).clamp(0.0, 1.0)
}

const ATTACK_RANGE: (f32, f32) = (0.5, 5_000.0);
const DECAY_RANGE: (f32, f32) = (1.0, 5_000.0);
const RELEASE_RANGE: (f32, f32) = (5.0, 8_000.0);
const LONG_DECAY_RANGE: (f32, f32) = (1.0, 10_000.0);
const LONG_RELEASE_RANGE: (f32, f32) = (5.0, 10_000.0);
const MASTER_RANGE: (f32, f32) = (-24.0, 3.0);
const LFO_RANGE: (f32, f32) = (wrapsynth::lfo::MIN_RATE_HZ, wrapsynth::lfo::MAX_RATE_HZ);
const CHORUS_RATE_RANGE: (f32, f32) = (0.05, 5.0);
const GLIDE_RANGE: (f32, f32) = (0.0, 2_000.0);
const DELAY_TIME_RANGE: (f32, f32) = (1.0, 2_000.0);
const REVERB_DECAY_RANGE: (f32, f32) = (0.1, 20.0);
/// Resonance and delay feedback stop short of 1.
const RESONANCE_MAX: f32 = 0.95;
const FEEDBACK_MAX: f32 = 0.95;
/// Osc B's fine tune, either way.
const FINE_CENTS: f32 = 50.0;
/// Unison detune's top.
const UNISON_DETUNE_MAX: f32 = 50.0;

fn master_position(db: f32) -> f32 {
    (db - MASTER_RANGE.0) / (MASTER_RANGE.1 - MASTER_RANGE.0)
}

fn master_from_position(position: f32) -> f32 {
    // Tenths of a dB: a readout that does not flicker through hundredths.
    let db = MASTER_RANGE.0 + position * (MASTER_RANGE.1 - MASTER_RANGE.0);
    (db * 10.0).round() / 10.0
}

fn defaults() -> &'static Params {
    static DEFAULTS: OnceLock<Params> = OnceLock::new();
    DEFAULTS.get_or_init(wrapsynth::default_params)
}

// ── Display boxes ──────────────────────────────────────────────────────────

/// A display box over `columns` columns from `column`, captioned `label` with
/// `reading` at its right, its plot painted by `paint` from the box's bounds,
/// its design width and the design-to-window scale.
fn screen(
    s: f32,
    (column, columns): (usize, usize),
    label: &str,
    reading: Option<String>,
    paint: impl Fn(Bounds<Pixels>, f32, f32, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let left = BOX_INSET + column as f32 * (COL_W + COL_GAP);
    let width = span(columns);
    div()
        .absolute()
        .left(px(left * s))
        .top(px(BOX_TOP * s))
        .w(px(width * s))
        .h(px(BOX_H * s))
        .bg(color(BOX_GROUND))
        .border(px(1.0))
        .border_color(color(BOX_EDGE))
        .overflow_hidden()
        .child(
            canvas(
                |_, _, _| (),
                move |bounds, _, window, cx| {
                    let k = f32::from(bounds.size.width) / width;
                    paint(bounds, width, k, window, cx);
                },
            )
            .absolute()
            .size_full(),
        )
        .child(at(s, 10.0, 8.0, caption(label, 10.5, 0.1, FAINT, s)))
        .children(reading.map(|reading| at_right(s, 10.0, 8.0, numbers(reading, 11.0, SOFT, s))))
        .into_any_element()
}

/// Maps design units in a box to window space.
fn mapper(b: Bounds<Pixels>, k: f32) -> impl Fn(f32, f32) -> (f32, f32) {
    let (x0, y0) = plate::xy(b.origin);
    move |x, y| (x0 + x * k, y0 + y * k)
}

/// The plot's grid: `columns` × `rows` cells between `top` and `bottom`.
fn paint_plot_grid(
    window: &mut Window,
    b: Bounds<Pixels>,
    k: f32,
    width: f32,
    (top, bottom): (f32, f32),
    (columns, rows): (usize, usize),
) {
    let at = mapper(b, k);
    for i in 1..columns {
        let x = width * i as f32 / columns as f32;
        paint_polyline(window, &[at(x, top), at(x, bottom)], 1.0, color(GRID), None);
    }
    for i in 1..rows {
        let y = top + (bottom - top) * i as f32 / rows as f32;
        paint_polyline(window, &[at(0.0, y), at(width, y)], 1.0, color(GRID), None);
    }
}

/// A curve (design units) shaded down to `floor`, in `ink` at `alpha`.
fn paint_shaded(
    window: &mut Window,
    b: Bounds<Pixels>,
    k: f32,
    points: &[(f32, f32)],
    floor: f32,
    ink: u32,
    alpha: f32,
) {
    if points.len() < 2 {
        return;
    }
    let at = mapper(b, k);
    let line: Vec<(f32, f32)> = points.iter().map(|&(x, y)| at(x, y)).collect();
    let mut area = line.clone();
    area.push(at(points[points.len() - 1].0, floor));
    area.push(at(points[0].0, floor));
    paint_polygon(window, &area, color_a(ink, 0.16 * alpha));
    paint_polyline(window, &line, 1.8 * k, color_a(ink, alpha), None);
}

/// Axis labels along a box's foot: `(x, text)` in design units.
fn paint_labels(
    window: &mut Window,
    cx: &mut App,
    b: Bounds<Pixels>,
    k: f32,
    width: f32,
    labels: &[(f32, String)],
) {
    let at = mapper(b, k);
    let font = plate::mono(FontWeight::NORMAL);
    for (x, text) in labels {
        let align = if *x < 16.0 {
            Align::Left
        } else if *x > width - 24.0 {
            Align::Right
        } else {
            Align::Center
        };
        paint_text(
            window,
            cx,
            text,
            font.clone(),
            10.0 * k,
            color(AXIS_INK),
            at(*x, AXIS_BOTTOM + 6.0),
            align,
        );
    }
}

/// The horizontal line through a box's plot middle.
fn paint_centre(window: &mut Window, b: Bounds<Pixels>, k: f32, width: f32, y: f32) {
    let at = mapper(b, k);
    paint_polyline(
        window,
        &[at(0.0, y), at(width, y)],
        1.0,
        color(0x2e2e2e),
        None,
    );
}

/// Points across one wave cycle, `samples` long, between `top` and
/// `bottom`.
fn wave_points(samples: &[f32], width: f32, (top, bottom): (f32, f32)) -> Vec<(f32, f32)> {
    let mid = (top + bottom) * 0.5;
    let half = (bottom - top) * 0.5;
    let last = samples.len().saturating_sub(1).max(1) as f32;
    samples
        .iter()
        .enumerate()
        .map(|(i, v)| (i as f32 / last * width, mid - v.clamp(-1.0, 1.0) * half))
        .collect()
}

/// Columns a box's curve is drawn at.
const CURVE_POINTS: usize = 240;

/// One cycle of an oscillator, through the DSP's own wavetable.
fn osc_box(
    s: f32,
    column: usize,
    label: &str,
    wave: Wavetable,
    position: f32,
    level: f32,
    dim: f32,
) -> AnyElement {
    let samples: Vec<f32> = (0..CURVE_POINTS)
        .map(|i| wrapsynth::osc_cycle(wave, position, i as f32 / (CURVE_POINTS - 1) as f32))
        .collect();
    let alpha = if level > 0.0 { dim } else { 0.35 * dim };
    let reading = wave.morphs().then(|| format!("Pos {}", percent(position)));
    screen(
        s,
        (column, 1),
        label,
        reading,
        move |b, width, k, window, _| {
            let plot = (PLOT_TOP + 4.0, PLOT_BOTTOM - 4.0);
            paint_plot_grid(window, b, k, width, (PLOT_TOP, PLOT_BOTTOM), (4, 2));
            let mid = (plot.0 + plot.1) * 0.5;
            paint_centre(window, b, k, width, mid);
            let points = wave_points(&samples, width, plot);
            paint_shaded(window, b, k, &points, mid, ACCENT, alpha);
        },
    )
}

/// The frequencies the filter box labels: a decade apart, so they never
/// crowd in a one-column box.
const FILTER_TICKS: [(f32, &str); 3] = [(100.0, "100"), (1_000.0, "1k"), (10_000.0, "10k")];

/// Magnitude in dB of `kind` at `hz`: the DSP's own linear prototype.
pub(crate) fn filter_response_db(kind: FilterType, cutoff: f32, resonance: f32, hz: f32) -> f32 {
    wrapsynth::filter::response_db(kind, cutoff, resonance, hz)
}

/// The filter's response, 20 Hz – 20 kHz, +18 dB at the top and −36 dB at
/// the foot, the cutoff marked.
fn filter_box(c: &Ctx, column: usize, dim: f32) -> AnyElement {
    let p = c.p;
    let response: Vec<f32> = (0..CURVE_POINTS)
        .map(|i| {
            let t = i as f32 / (CURVE_POINTS - 1) as f32;
            filter_response_db(
                p.filter_type,
                p.cutoff_hz,
                p.resonance,
                20.0 * 1_000f32.powf(t),
            )
        })
        .collect();
    let reading = format!(
        "{} \u{00B7} {}",
        p.filter_type.label(),
        plate::fmt_hz(p.cutoff_hz)
    );
    screen(
        c.s,
        (column, 1),
        "Filter",
        Some(reading),
        move |b, width, k, window, cx| {
            let at = mapper(b, k);
            let y_for = |db: f32| {
                PLOT_TOP + ((18.0 - db.clamp(-36.0, 18.0)) / 54.0) * (AXIS_BOTTOM - PLOT_TOP)
            };
            for hz in FREQ_GRID {
                let x = x_of_hz(hz, width);
                paint_polyline(
                    window,
                    &[at(x, PLOT_TOP), at(x, AXIS_BOTTOM)],
                    1.0,
                    color(GRID),
                    None,
                );
            }
            for db in [-12.0, -24.0] {
                let y = y_for(db);
                paint_polyline(window, &[at(0.0, y), at(width, y)], 1.0, color(GRID), None);
            }
            paint_centre(window, b, k, width, y_for(0.0));
            let labels: Vec<(f32, String)> = FILTER_TICKS
                .iter()
                .map(|(hz, text)| (x_of_hz(*hz, width), (*text).to_string()))
                .collect();
            paint_labels(window, cx, b, k, width, &labels);
            let last = (CURVE_POINTS - 1) as f32;
            let points: Vec<(f32, f32)> = response
                .iter()
                .enumerate()
                .map(|(i, db)| (i as f32 / last * width, y_for(*db)))
                .collect();
            paint_shaded(window, b, k, &points, AXIS_BOTTOM, ACCENT, dim);
            let x = x_of_hz(p.cutoff_hz, width);
            paint_polyline(
                window,
                &[at(x, PLOT_TOP), at(x, AXIS_BOTTOM)],
                1.0,
                color_a(WHITE, 0.45 * dim),
                Some((3.0 * k, 3.0 * k)),
            );
        },
    )
}

/// An envelope's four times and level, as the editor draws it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct EnvShape {
    pub attack_ms: f32,
    pub decay_ms: f32,
    pub sustain: f32,
    pub release_ms: f32,
}

impl EnvShape {
    fn of(env: Env, p: &Params) -> Self {
        match env {
            Env::Amp => Self {
                attack_ms: p.attack_ms,
                decay_ms: p.decay_ms,
                sustain: p.sustain,
                release_ms: p.release_ms,
            },
            Env::Filter => Self {
                attack_ms: p.filter_attack_ms,
                decay_ms: p.filter_decay_ms,
                sustain: p.filter_sustain,
                release_ms: p.filter_release_ms,
            },
            Env::Mod => Self {
                attack_ms: p.mod_attack_ms,
                decay_ms: p.mod_decay_ms,
                sustain: p.mod_sustain,
                release_ms: p.mod_release_ms,
            },
        }
    }
}

/// The envelope's corner points across `width`: attack, decay and release
/// each take a share of the width that grows with their time (square-root,
/// so a 10 ms attack shows beside an 8 s release), and the sustain holds a
/// fixed stretch so it is always visible.
pub(crate) fn envelope_points(env: &EnvShape, width: f32, height: f32) -> [(f32, f32); 5] {
    let span = |ms: f32, max: f32| (ms.max(0.0) / max).sqrt().min(1.0);
    let sustain_w = 0.22;
    let (a, d, r) = (
        span(env.attack_ms, ATTACK_RANGE.1),
        span(env.decay_ms, DECAY_RANGE.1),
        span(env.release_ms, RELEASE_RANGE.1),
    );
    let total = (a + d + r).max(1.0e-6);
    let share = (1.0 - sustain_w) * (a + d + r).min(2.4) / 2.4;
    let unit = share / total;
    let xa = a * unit;
    let xd = xa + d * unit;
    let xs = xd + sustain_w;
    let xr = xs + r * unit;
    let level = |value: f32| height * (1.0 - value.clamp(0.0, 1.0));
    [
        (0.0, height),
        (xa * width, level(1.0)),
        (xd * width, level(env.sustain)),
        (xs * width, level(env.sustain)),
        (xr * width, height),
    ]
}

/// The envelope traced with the curves the DSP follows: a convex attack and
/// exponential decay and release between the corners.
pub(crate) fn envelope_curve(env: &EnvShape, width: f32, height: f32) -> Vec<(f32, f32)> {
    let corners = envelope_points(env, width, height);
    let mut curve = Vec::with_capacity(4 * 24 + 1);
    curve.push(corners[0]);
    let ratios = [ATTACK_RATIO, DECAY_RATIO, 1.0, DECAY_RATIO];
    for (segment, ratio) in ratios.iter().enumerate() {
        let (from, to) = (corners[segment], corners[segment + 1]);
        for step in 1..=24 {
            let t = step as f32 / 24.0;
            let progress = if segment == 2 {
                t
            } else {
                segment_progress(t, *ratio)
            };
            curve.push((
                from.0 + (to.0 - from.0) * t,
                from.1 + (to.1 - from.1) * progress,
            ));
        }
    }
    curve
}

fn envelope_box(
    s: f32,
    column: usize,
    label: &str,
    reading: String,
    env: EnvShape,
    dim: f32,
) -> AnyElement {
    screen(
        s,
        (column, 1),
        label,
        Some(reading),
        move |b, width, k, window, _| {
            let inset = 8.0;
            let (top, bottom) = (PLOT_TOP + 4.0, PLOT_BOTTOM);
            paint_plot_grid(window, b, k, width, (PLOT_TOP, PLOT_BOTTOM), (4, 2));
            let (w, h) = (width - inset * 2.0, bottom - top);
            let points: Vec<(f32, f32)> = envelope_curve(&env, w, h)
                .iter()
                .map(|&(x, y)| (inset + x, top + y))
                .collect();
            paint_shaded(window, b, k, &points, bottom, ACCENT, dim);
            let at = mapper(b, k);
            let corners = envelope_points(&env, w, h);
            // The note-off: where the release starts.
            let (release_x, _) = corners[3];
            paint_polyline(
                window,
                &[
                    at(inset + release_x, PLOT_TOP),
                    at(inset + release_x, bottom),
                ],
                1.0,
                color_a(WHITE, 0.3 * dim),
                Some((3.0 * k, 3.0 * k)),
            );
            for &(x, y) in &corners[1..4] {
                let (x, y) = at(inset + x, top + y);
                plate::paint_dot(window, x, y, 2.6 * k, color_a(WHITE, dim));
            }
        },
    )
}

/// Two cycles of an LFO; a sample-and-hold drawn as a few steady steps.
fn lfo_box(s: f32, column: usize, label: &str, fields: &LfoFields, dim: f32) -> AnyElement {
    let shape = fields.shape;
    let held = [0.6, -0.3, 0.9, -0.8];
    let samples: Vec<f32> = (0..CURVE_POINTS)
        .map(|i| {
            let t = i as f32 / (CURVE_POINTS - 1) as f32 * 2.0;
            let cycle = (t as usize).min(1);
            let phase = (t - cycle as f32).min(0.9999);
            let step = held[(cycle * 2 + usize::from(phase >= 0.5)) % held.len()];
            shape.at(phase, step)
        })
        .collect();
    let reading = if fields.sync {
        division_label(fields.division)
    } else {
        format_rate(fields.rate_hz)
    };
    screen(
        s,
        (column, 1),
        label,
        Some(reading),
        move |b, width, k, window, _| {
            let plot = (PLOT_TOP + 4.0, PLOT_BOTTOM - 4.0);
            paint_plot_grid(window, b, k, width, (PLOT_TOP, PLOT_BOTTOM), (4, 2));
            let mid = (plot.0 + plot.1) * 0.5;
            paint_centre(window, b, k, width, mid);
            let points = wave_points(&samples, width, plot);
            paint_shaded(window, b, k, &points, mid, ACCENT, dim);
        },
    )
}

// ── Synth page ─────────────────────────────────────────────────────────────

fn synth_boxes(c: &Ctx, dim: f32) -> Vec<AnyElement> {
    let p = c.p;
    vec![
        osc_box(
            c.s,
            0,
            "Osc A",
            p.osc_a_wave,
            p.osc_a_position,
            p.osc_a_level,
            dim,
        ),
        osc_box(
            c.s,
            1,
            "Osc B",
            p.osc_b_wave,
            p.osc_b_position,
            p.osc_b_level,
            dim,
        ),
        filter_box(c, 2, dim),
        envelope_box(
            c.s,
            3,
            "Amp env",
            format!("Sustain {}", percent(p.sustain)),
            EnvShape::of(Env::Amp, &p),
            dim,
        ),
    ]
}

fn synth_deck(c: &Ctx) -> Vec<AnyElement> {
    let s = c.s;
    let column_four = div()
        .flex()
        .flex_col()
        .gap(px(COL_GAP * s))
        .w(px(COL_W * s))
        .flex_shrink_0()
        .child(envelope_card(c, Env::Amp).flex_1())
        .child(output_card(c).flex_1());
    vec![div()
        .flex()
        .flex_row()
        .flex_1()
        .gap(px(COL_GAP * s))
        .child(oscillator_card(c, Osc::A))
        .child(oscillator_card(c, Osc::B))
        .child(filter_card(c))
        .child(column_four)
        .into_any_element()]
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Osc {
    A,
    B,
}

/// `‹ Classic Saw ›` over every table, in the picker's order.
fn table_picker(c: &Ctx, id: &'static str, current: Wavetable, which: Osc) -> AnyElement {
    let on_step = c.step(move |p, delta| {
        let next = cycle(&Wavetable::MENU, current, delta);
        with(p, |p| match which {
            Osc::A => p.osc_a_wave = next,
            Osc::B => p.osc_b_wave = next,
        })
    });
    kit::chooser(id, "Wavetable", current.label(), 140.0, true, c.s, on_step)
}

fn oscillator_card(c: &Ctx, which: Osc) -> gpui::Div {
    let (p, s, d) = (c.p, c.s, defaults());
    let (title, wave, position, level) = match which {
        Osc::A => (
            "Oscillator A",
            p.osc_a_wave,
            p.osc_a_position,
            p.osc_a_level,
        ),
        Osc::B => (
            "Oscillator B",
            p.osc_b_wave,
            p.osc_b_position,
            p.osc_b_level,
        ),
    };
    let (default_position, default_level) = match which {
        Osc::A => (d.osc_a_position, d.osc_a_level),
        Osc::B => (d.osc_b_position, d.osc_b_level),
    };
    let tag = match which {
        Osc::A => "a",
        Osc::B => "b",
    };
    let picker = table_picker(
        c,
        match which {
            Osc::A => "wrap-synth-osc-a-wave",
            Osc::B => "wrap-synth-osc-b-wave",
        },
        wave,
        which,
    );
    let shape = knobs(vec![
        knob(
            c,
            format!("wrap-synth-osc-{tag}-position"),
            travel(
                if wave.morphs() { "WT Pos" } else { "Position" },
                default_position,
            ),
            position,
            percent(position),
            LEAD,
            move |p, v| match which {
                Osc::A => p.osc_a_position = v,
                Osc::B => p.osc_b_position = v,
            },
        ),
        knob(
            c,
            format!("wrap-synth-osc-{tag}-level"),
            travel("Level", default_level),
            level,
            percent(level),
            LEAD,
            move |p, v| match which {
                Osc::A => p.osc_a_level = v,
                Osc::B => p.osc_b_level = v,
            },
        ),
    ]);
    let tail = match which {
        Osc::A => section("Unison", s).child(knobs(vec![
            kit::stepper(
                "wrap-synth-unison",
                "Voices",
                format!("{}\u{00D7}", p.unison),
                true,
                s,
                c.step(|p, delta| {
                    with(p, |p| {
                        p.unison = if delta < 0 {
                            p.unison.saturating_sub(1)
                        } else {
                            p.unison.saturating_add(1)
                        }
                    })
                }),
            ),
            knob(
                c,
                "wrap-synth-unison-detune",
                travel("Detune", d.unison_detune_cents / UNISON_DETUNE_MAX),
                p.unison_detune_cents / UNISON_DETUNE_MAX,
                format!("{:.0} ct", p.unison_detune_cents),
                SMALL4,
                |p, v| p.unison_detune_cents = (v * UNISON_DETUNE_MAX).round(),
            ),
            knob(
                c,
                "wrap-synth-unison-blend",
                travel("Blend", d.unison_blend),
                p.unison_blend,
                percent(p.unison_blend),
                SMALL4,
                |p, v| p.unison_blend = v,
            ),
        ])),
        Osc::B => section("Pitch", s).child(knobs(vec![
            kit::stepper(
                "wrap-synth-osc-b-semitones",
                "Semitones",
                format!("{:+}", p.osc_b_semitones.round() as i32),
                true,
                s,
                c.step(|p, delta| {
                    with(p, |p| {
                        p.osc_b_semitones = p.osc_b_semitones.round() + delta as f32
                    })
                }),
            ),
            knob(
                c,
                "wrap-synth-osc-b-fine",
                signed_travel("Fine", d.osc_b_detune_cents / FINE_CENTS),
                p.osc_b_detune_cents / FINE_CENTS,
                format!("{:+.0} ct", p.osc_b_detune_cents),
                SMALL3,
                |p, v| p.osc_b_detune_cents = (v * FINE_CENTS).round(),
            ),
        ])),
    };
    deck_card(s, Some(COL_W))
        .child(card_title(title, s))
        .child(picker)
        .child(shape)
        .child(tail)
}

fn filter_card(c: &Ctx) -> gpui::Div {
    let (p, s, d) = (c.p, c.s, defaults());
    let model = p.filter_type.model();
    let models: Vec<&str> = FilterModel::ALL.iter().map(|model| model.label()).collect();
    let types: Vec<&str> = model.types().iter().map(|kind| kind.label()).collect();
    let model_at = FilterModel::ALL.iter().position(|m| *m == model);
    let type_at = model.types().iter().position(|kind| *kind == p.filter_type);
    let on_model = c.step(|p, index| {
        let model = FilterModel::ALL[(index.max(0) as usize).min(FilterModel::ALL.len() - 1)];
        with(p, |p| p.filter_type = model.types()[0])
    });
    let on_type = c.step(move |p, index| {
        let kinds = model.types();
        with(p, |p| {
            p.filter_type = kinds[(index.max(0) as usize).min(kinds.len() - 1)]
        })
    });
    deck_card(s, Some(COL_W))
        .child(card_title("Filter", s))
        .child(section("Model", s).child(segmented(
            "wrap-synth-filter-model",
            &models,
            model_at,
            s,
            on_model,
        )))
        .child(section("Type", s).child(segmented(
            "wrap-synth-filter-type",
            &types,
            type_at,
            s,
            on_type,
        )))
        .child(knobs(vec![
            knob(
                c,
                "wrap-synth-cutoff",
                travel("Cutoff", cutoff_position(d.cutoff_hz)),
                cutoff_position(p.cutoff_hz),
                plate::fmt_hz(p.cutoff_hz),
                LEAD,
                |p, v| p.cutoff_hz = cutoff_from_position(v),
            ),
            knob(
                c,
                "wrap-synth-resonance",
                travel("Resonance", d.resonance / RESONANCE_MAX),
                p.resonance / RESONANCE_MAX,
                percent(p.resonance / RESONANCE_MAX),
                LEAD,
                |p, v| p.resonance = v * RESONANCE_MAX,
            ),
            knob(
                c,
                "wrap-synth-drive",
                travel("Drive", d.filter_drive),
                p.filter_drive,
                percent(p.filter_drive),
                LEAD,
                |p, v| p.filter_drive = v,
            ),
        ]))
        .child(knobs(vec![
            knob(
                c,
                "wrap-synth-filter-env-amount",
                signed_travel("Env Amt", d.filter_env_amount),
                p.filter_env_amount,
                signed_percent(p.filter_env_amount),
                SMALL3,
                |p, v| p.filter_env_amount = v,
            ),
            knob(
                c,
                "wrap-synth-key-track",
                travel("Key Track", d.filter_key_track),
                p.filter_key_track,
                percent(p.filter_key_track),
                SMALL3,
                |p, v| p.filter_key_track = v,
            ),
        ]))
}

/// Which envelope a card edits.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Env {
    Amp,
    Filter,
    Mod,
}

fn envelope_card(c: &Ctx, env: Env) -> gpui::Div {
    let (p, s) = (c.p, c.s);
    let (title, id) = match env {
        Env::Amp => ("Amp Envelope", "amp"),
        Env::Filter => ("Filter Envelope", "filter"),
        Env::Mod => ("Mod Envelope", "mod"),
    };
    let shape = EnvShape::of(env, &p);
    let reset = EnvShape::of(env, defaults());
    let (decay_range, release_range) = match env {
        Env::Amp => (DECAY_RANGE, RELEASE_RANGE),
        _ => (LONG_DECAY_RANGE, LONG_RELEASE_RANGE),
    };
    let set_attack = move |p: &mut Params, v: f32| {
        let ms = time_from_position(v, ATTACK_RANGE);
        match env {
            Env::Amp => p.attack_ms = ms,
            Env::Filter => p.filter_attack_ms = ms,
            Env::Mod => p.mod_attack_ms = ms,
        }
    };
    let set_decay = move |p: &mut Params, v: f32| {
        let ms = time_from_position(v, decay_range);
        match env {
            Env::Amp => p.decay_ms = ms,
            Env::Filter => p.filter_decay_ms = ms,
            Env::Mod => p.mod_decay_ms = ms,
        }
    };
    let set_sustain = move |p: &mut Params, v: f32| match env {
        Env::Amp => p.sustain = v,
        Env::Filter => p.filter_sustain = v,
        Env::Mod => p.mod_sustain = v,
    };
    let set_release = move |p: &mut Params, v: f32| {
        let ms = time_from_position(v, release_range);
        match env {
            Env::Amp => p.release_ms = ms,
            Env::Filter => p.filter_release_ms = ms,
            Env::Mod => p.mod_release_ms = ms,
        }
    };
    deck_card(s, None)
        .child(card_title(title, s))
        .child(knobs(vec![
            knob(
                c,
                format!("wrap-synth-{id}-attack"),
                travel("Attack", time_position(reset.attack_ms, ATTACK_RANGE)),
                time_position(shape.attack_ms, ATTACK_RANGE),
                fmt_time(shape.attack_ms),
                SMALL4,
                set_attack,
            ),
            knob(
                c,
                format!("wrap-synth-{id}-decay"),
                travel("Decay", time_position(reset.decay_ms, decay_range)),
                time_position(shape.decay_ms, decay_range),
                fmt_time(shape.decay_ms),
                SMALL4,
                set_decay,
            ),
            knob(
                c,
                format!("wrap-synth-{id}-sustain"),
                travel("Sustain", reset.sustain),
                shape.sustain,
                percent(shape.sustain),
                SMALL4,
                set_sustain,
            ),
            knob(
                c,
                format!("wrap-synth-{id}-release"),
                travel("Release", time_position(reset.release_ms, release_range)),
                time_position(shape.release_ms, release_range),
                fmt_time(shape.release_ms),
                SMALL4,
                set_release,
            ),
        ]))
}

fn output_card(c: &Ctx) -> gpui::Div {
    let (p, s, d) = (c.p, c.s, defaults());
    deck_card(s, None)
        .child(card_title("Output", s))
        .child(knobs(vec![
            knob(
                c,
                "wrap-synth-sub",
                travel("Sub", d.sub_level),
                p.sub_level,
                percent(p.sub_level),
                SMALL4,
                |p, v| p.sub_level = v,
            ),
            knob(
                c,
                "wrap-synth-noise",
                travel("Noise", d.noise_level),
                p.noise_level,
                percent(p.noise_level),
                SMALL4,
                |p, v| p.noise_level = v,
            ),
            knob(
                c,
                "wrap-synth-width",
                travel("Width", d.stereo_width),
                p.stereo_width,
                percent(p.stereo_width),
                SMALL4,
                |p, v| p.stereo_width = v,
            ),
            knob(
                c,
                "wrap-synth-master",
                travel("Master", master_position(d.master_db)),
                master_position(p.master_db),
                plate::fmt_db(p.master_db),
                SMALL4,
                |p, v| p.master_db = master_from_position(v),
            ),
        ]))
}

// ── Modulation page ────────────────────────────────────────────────────────

fn modulation_boxes(c: &Ctx, dim: f32) -> Vec<AnyElement> {
    let p = c.p;
    vec![
        envelope_box(
            c.s,
            0,
            "Filter env",
            format!("Amt {}", signed_percent(p.filter_env_amount)),
            EnvShape::of(Env::Filter, &p),
            dim,
        ),
        envelope_box(
            c.s,
            1,
            "Mod env",
            format!("Sustain {}", percent(p.mod_sustain)),
            EnvShape::of(Env::Mod, &p),
            dim,
        ),
        lfo_box(c.s, 2, "LFO 1", &LfoFields::of(&p, 0), dim),
        lfo_box(c.s, 3, "LFO 2", &LfoFields::of(&p, 1), dim),
    ]
}

fn modulation_deck(c: &Ctx) -> Vec<AnyElement> {
    let s = c.s;
    vec![
        div()
            .flex()
            .flex_row()
            .flex_shrink_0()
            .h(px(MOD_ROW_H * s))
            .gap(px(COL_GAP * s))
            .child(envelope_card(c, Env::Filter))
            .child(envelope_card(c, Env::Mod))
            .child(lfo_card(c, 0))
            .child(lfo_card(c, 1))
            .into_any_element(),
        matrix_card(c).into_any_element(),
    ]
}

/// One LFO's fields, read out of and written back into a patch.
struct LfoFields {
    shape: LfoShape,
    rate_hz: f32,
    sync: bool,
    division: u8,
    retrigger: bool,
}

impl LfoFields {
    fn of(p: &Params, which: usize) -> Self {
        if which == 0 {
            Self {
                shape: p.lfo1_shape,
                rate_hz: p.lfo1_rate_hz,
                sync: p.lfo1_sync,
                division: p.lfo1_division,
                retrigger: p.lfo1_retrigger,
            }
        } else {
            Self {
                shape: p.lfo2_shape,
                rate_hz: p.lfo2_rate_hz,
                sync: p.lfo2_sync,
                division: p.lfo2_division,
                retrigger: p.lfo2_retrigger,
            }
        }
    }

    fn store(&self, p: &mut Params, which: usize) {
        if which == 0 {
            p.lfo1_shape = self.shape;
            p.lfo1_rate_hz = self.rate_hz;
            p.lfo1_sync = self.sync;
            p.lfo1_division = self.division;
            p.lfo1_retrigger = self.retrigger;
        } else {
            p.lfo2_shape = self.shape;
            p.lfo2_rate_hz = self.rate_hz;
            p.lfo2_sync = self.sync;
            p.lfo2_division = self.division;
            p.lfo2_retrigger = self.retrigger;
        }
    }
}

/// `p` with LFO `which` edited by `apply`.
fn edit_lfo(p: Params, which: usize, apply: impl FnOnce(&mut LfoFields)) -> Params {
    with(p, |p| {
        let mut fields = LfoFields::of(p, which);
        apply(&mut fields);
        fields.store(p, which);
    })
}

fn lfo_card(c: &Ctx, which: usize) -> gpui::Div {
    let (p, s) = (c.p, c.s);
    let fields = LfoFields::of(&p, which);
    let reset = LfoFields::of(defaults(), which);
    let ids = |first: &'static str, second: &'static str| if which == 0 { first } else { second };
    let shapes: Vec<&str> = LfoShape::ALL.iter().map(|shape| shape.label()).collect();
    let shape_at = LfoShape::ALL
        .iter()
        .position(|shape| *shape == fields.shape);
    let on_shape = c.step(move |p, index| {
        let shape = LfoShape::ALL[(index.max(0) as usize).min(LfoShape::ALL.len() - 1)];
        edit_lfo(p, which, |f| f.shape = shape)
    });
    let rows: Vec<AnyElement> = shapes
        .chunks(3)
        .enumerate()
        .map(|(row, labels)| {
            let on_shape = on_shape.clone();
            segmented(
                if row == 0 {
                    ids("wrap-synth-lfo0-shape", "wrap-synth-lfo1-shape")
                } else {
                    ids("wrap-synth-lfo0-shape-b", "wrap-synth-lfo1-shape-b")
                },
                labels,
                shape_at
                    .and_then(|at| at.checked_sub(row * 3))
                    .filter(|at| *at < 3),
                s,
                Rc::new(move |index: i32, window: &mut Window, app: &mut App| {
                    on_shape(index + row as i32 * 3, window, app)
                }),
            )
            .into_any_element()
        })
        .collect();
    let rate = if fields.sync {
        let max = echospace::MAX_DIVISION_WIRE as u8;
        kit::chooser(
            ids("wrap-synth-lfo0-division", "wrap-synth-lfo1-division"),
            "Rate",
            division_label(fields.division),
            48.0,
            true,
            s,
            c.step(move |p, delta| {
                edit_lfo(p, which, |f| {
                    f.division = if delta < 0 {
                        f.division.saturating_sub(1)
                    } else {
                        (f.division + 1).min(max)
                    }
                })
            }),
        )
    } else {
        knob(
            c,
            ids("wrap-synth-lfo0-rate", "wrap-synth-lfo1-rate"),
            travel("Rate", rate_position(reset.rate_hz, LFO_RANGE)),
            rate_position(fields.rate_hz, LFO_RANGE),
            format_rate(fields.rate_hz),
            SMALL3,
            move |p, v| {
                let mut fields = LfoFields::of(p, which);
                fields.rate_hz = rate_from_position(v, LFO_RANGE);
                fields.store(p, which);
            },
        )
    };
    let toggles = div()
        .flex()
        .flex_col()
        .gap(px(6.0 * s))
        .w(px(84.0 * s))
        .child(
            kit::toggle(
                ids("wrap-synth-lfo0-sync", "wrap-synth-lfo1-sync"),
                "Sync",
                fields.sync,
                accent(),
                true,
                s,
                c.click(edit_lfo(p, which, |f| f.sync = !f.sync)),
            )
            .w_full(),
        )
        .child(
            kit::toggle(
                ids("wrap-synth-lfo0-retrigger", "wrap-synth-lfo1-retrigger"),
                "Retrigger",
                fields.retrigger,
                accent(),
                true,
                s,
                c.click(edit_lfo(p, which, |f| f.retrigger = !f.retrigger)),
            )
            .w_full(),
        );
    deck_card(s, Some(COL_W))
        .child(card_title(if which == 0 { "LFO 1" } else { "LFO 2" }, s))
        .child(div().flex().flex_col().gap(px(4.0 * s)).children(rows))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_around()
                .h(px(76.0 * s))
                .child(rate)
                .child(toggles),
        )
}

/// A slot's amount: a bar out from the centre, `-1..1`. Drag across it
/// (Shift for fine), turn the wheel, double-click for none.
fn amount_bar(slot: usize, amount: f32, s: f32, set: KnobSet) -> AnyElement {
    let n = (amount.clamp(-1.0, 1.0) + 1.0) * 0.5;
    let face = canvas(
        |_, _, _| (),
        move |b, _, window, _| {
            let (x, y) = plate::xy(b.origin);
            let (w, h) = (f32::from(b.size.width), f32::from(b.size.height));
            let bar_h = 6.0 * s;
            let by = y + (h - bar_h) * 0.5;
            window.paint_quad(
                gpui::fill(plate::rect(x, by, w, bar_h), color(plate::TRACK))
                    .corner_radii(px(2.0 * s)),
            );
            let mid = x + w * 0.5;
            let to = x + w * n;
            if (to - mid).abs() > 0.5 {
                paint_rect(window, (mid.min(to), by, (to - mid).abs(), bar_h), accent());
            }
            paint_rect(
                window,
                (mid - 0.5, y + (h - 14.0 * s) * 0.5, 1.0, 14.0 * s),
                color(0x6a6a6a),
            );
        },
    )
    .size_full();
    let drag_id = format!("wrap-synth-mod{slot}-amount");
    let (drag_set, wheel_set) = (set.clone(), set.clone());
    let round = |n: f32| ((n.clamp(0.0, 1.0) * 2.0 - 1.0) * 100.0).round() / 100.0;
    div()
        .id(("wrap-synth-mod-amount", slot))
        .flex_1()
        .min_w(px(0.0))
        .h(px(24.0 * s))
        .cursor(gpui::CursorStyle::ResizeLeftRight)
        .child(face)
        .on_drag(FaderDrag::new(drag_id.clone()), |drag, _, _, cx| {
            cx.new(|_| FaderDrag::new(drag.id.clone()))
        })
        .on_drag_move::<FaderDrag>(move |event, _window, app| {
            let drag = event.drag(app);
            if drag.id != drag_id {
                return;
            }
            let x = f32::from(event.event.position.x);
            let next = drag_to(drag, n, x, event.event.modifiers.shift, 160.0 * s, |dx| dx);
            drag_set(round(next), app);
        })
        .on_scroll_wheel(move |event: &ScrollWheelEvent, _window, app| {
            let dy = f32::from(event.delta.pixel_delta(px(20.0)).y);
            if dy == 0.0 {
                return;
            }
            let span = if event.modifiers.shift {
                2_000.0
            } else {
                600.0
            };
            wheel_set(round(n + dy / span), app);
        })
        .on_click(move |event, _window, app| {
            if is_reset_click(event) {
                set(0.0, app);
            }
        })
        .into_any_element()
}

/// The source and destination pickers' widths.
const SOURCE_W: f32 = 88.0;
const DEST_W: f32 = 104.0;
const SLOT_NUMBER_W: f32 = 16.0;
const AMOUNT_READOUT_W: f32 = 42.0;

fn matrix_slot(c: &Ctx, slot: usize) -> AnyElement {
    let (p, s) = (c.p, c.s);
    let current = p.mod_slots[slot];
    let active = current.is_active();
    let on_source = c.step(move |p, delta| {
        with(p, |p| {
            p.mod_slots[slot].source = cycle(&ModSource::ALL, current.source, delta)
        })
    });
    let on_dest = c.step(move |p, delta| {
        with(p, |p| {
            p.mod_slots[slot].dest = cycle(&ModDest::ALL, current.dest, delta)
        })
    });
    let set = c.set.clone();
    let on_amount: KnobSet = Rc::new(move |amount: f32, app: &mut App| {
        set(with(p, |p| p.mod_slots[slot].amount = amount), app)
    });
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0 * s))
        .child(
            div()
                .w(px(SLOT_NUMBER_W * s))
                .flex_shrink_0()
                .child(numbers(
                    format!("{}", slot + 1),
                    12.0,
                    if active { ACCENT } else { plate::GHOST },
                    s,
                )),
        )
        .child(picker(
            "wrap-synth-mod-source",
            slot,
            current.source.label(),
            SOURCE_W,
            s,
            on_source,
        ))
        .child(picker(
            "wrap-synth-mod-dest",
            slot,
            current.dest.label(),
            DEST_W,
            s,
            on_dest,
        ))
        .child(amount_bar(slot, current.amount, s, on_amount))
        .child(
            div()
                .w(px(AMOUNT_READOUT_W * s))
                .flex_shrink_0()
                .flex()
                .justify_end()
                .child(numbers(
                    signed_percent(current.amount),
                    11.0,
                    if active { WHITE } else { FAINT },
                    s,
                )),
        )
        .into_any_element()
}

fn matrix_card(c: &Ctx) -> gpui::Div {
    let s = c.s;
    // The column captions, over the pickers.
    let heading = || {
        let label = |text: &str, width: f32| {
            div()
                .w(px(width * s))
                .flex_shrink_0()
                .flex()
                .justify_center()
                .child(caption(text, 9.5, 0.1, plate::GHOST, s))
        };
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0 * s))
            .child(div().w(px(SLOT_NUMBER_W * s)).flex_shrink_0())
            .child(label("Source", SOURCE_W + 54.0))
            .child(label("Destination", DEST_W + 54.0))
            .child(div().flex_1().flex().justify_center().child(caption(
                "Amount",
                9.5,
                0.1,
                plate::GHOST,
                s,
            )))
    };
    let half = |slots: std::ops::Range<usize>| {
        div()
            .flex()
            .flex_col()
            .flex_basis(px(0.0))
            .flex_grow_1()
            .min_w(px(0.0))
            .gap(px(8.0 * s))
            .child(heading())
            .children(slots.map(|slot| matrix_slot(c, slot)))
    };
    let routes = c.p.mod_slots.iter().filter(|slot| slot.is_active()).count();
    deck_card(s, None)
        .justify_start()
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .child(card_title("Modulation Matrix", s))
                .child(numbers(
                    format!("{routes} of {MOD_SLOTS} routed"),
                    11.0,
                    SOFT,
                    s,
                )),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .gap(px(32.0 * s))
                .child(half(0..MOD_SLOTS / 2))
                .child(half(MOD_SLOTS / 2..MOD_SLOTS)),
        )
}

// ── Voice & FX page ────────────────────────────────────────────────────────

/// The distortion's curve: what `fx.rs` does to a sample `x` — a soft clip
/// driven by `1 + 24 · drive`, made up to full scale, mixed with the dry.
pub(crate) fn drive_transfer(p: &Params, x: f32) -> f32 {
    let gain = 1.0 + p.dist_drive * 24.0;
    let makeup = (1.0 + gain) / gain;
    let driven = x * gain;
    let shaped = driven / (1.0 + driven.abs()) * makeup;
    x + (shaped - x) * p.dist_mix
}

/// The delay's repeats, by level relative to the first: each pass is
/// `feedback` of the one before, down to −48 dB.
pub(crate) fn delay_repeats(feedback: f32) -> Vec<f32> {
    let mut levels = vec![0.0];
    let step = 20.0 * feedback.max(1.0e-6).log10();
    while levels.len() < 24 {
        let next = levels[levels.len() - 1] + step;
        if next < -48.0 {
            break;
        }
        levels.push(next);
    }
    levels
}

fn voice_fx_boxes(c: &Ctx, dim: f32) -> Vec<AnyElement> {
    let (p, s) = (c.p, c.s);
    let fade = |on: bool| if on { dim } else { 0.3 * dim };
    let off = || Some("Off".to_string());

    // Drive: input across, output up, the straight line dashed.
    let transfer: Vec<f32> = (0..CURVE_POINTS)
        .map(|i| {
            let x = i as f32 / (CURVE_POINTS - 1) as f32 * 2.0 - 1.0;
            if p.dist_on {
                drive_transfer(&p, x)
            } else {
                x
            }
        })
        .collect();
    let drive_alpha = fade(p.dist_on);
    let drive = screen(
        s,
        (0, 1),
        "Drive",
        if p.dist_on {
            Some(percent(p.dist_drive))
        } else {
            off()
        },
        move |b, width, k, window, _| {
            let plot = (PLOT_TOP + 4.0, PLOT_BOTTOM - 4.0);
            paint_plot_grid(window, b, k, width, (PLOT_TOP, PLOT_BOTTOM), (4, 2));
            let mid = (plot.0 + plot.1) * 0.5;
            paint_centre(window, b, k, width, mid);
            let at = mapper(b, k);
            paint_polyline(
                window,
                &[at(0.0, plot.1), at(width, plot.0)],
                1.0,
                color_a(WHITE, 0.22),
                Some((3.0 * k, 3.0 * k)),
            );
            let points = wave_points(&transfer, width, plot);
            paint_shaded(window, b, k, &points, mid, ACCENT, drive_alpha);
        },
    );

    // Chorus: the two taps' sweep, the right a quarter cycle behind, over
    // two cycles.
    let depth = p.chorus_depth;
    let chorus_alpha = fade(p.chorus_on);
    let cycle_ms = 1_000.0 / p.chorus_rate_hz.max(1.0e-3);
    let chorus = screen(
        s,
        (1, 1),
        "Chorus",
        if p.chorus_on {
            Some(format_rate(p.chorus_rate_hz))
        } else {
            off()
        },
        move |b, width, k, window, cx| {
            let plot = (PLOT_TOP + 6.0, AXIS_BOTTOM - 6.0);
            paint_plot_grid(window, b, k, width, (PLOT_TOP, AXIS_BOTTOM), (4, 2));
            let mid = (plot.0 + plot.1) * 0.5;
            paint_centre(window, b, k, width, mid);
            let side = |offset: f32| -> Vec<f32> {
                (0..CURVE_POINTS)
                    .map(|i| {
                        let t = i as f32 / (CURVE_POINTS - 1) as f32 * 2.0;
                        depth * (std::f32::consts::TAU * (t + offset)).sin()
                    })
                    .collect()
            };
            let at = mapper(b, k);
            let right: Vec<(f32, f32)> = wave_points(&side(0.25), width, plot)
                .into_iter()
                .map(|(x, y)| at(x, y))
                .collect();
            paint_polyline(
                window,
                &right,
                1.4 * k,
                color_a(SECOND, 0.7 * chorus_alpha),
                None,
            );
            let left = wave_points(&side(0.0), width, plot);
            paint_shaded(window, b, k, &left, mid, ACCENT, chorus_alpha);
            paint_labels(
                window,
                cx,
                b,
                k,
                width,
                &[
                    (0.0, "0".to_string()),
                    (width * 0.5, fmt_time(cycle_ms)),
                    (width, fmt_time(cycle_ms * 2.0)),
                ],
            );
        },
    );

    // Delay: the dry hit, then each repeat as tall as it is loud; ping-pong
    // repeats alternate inks.
    let repeats = delay_repeats(p.delay_feedback);
    let ping_pong = p.delay_ping_pong;
    let delay_alpha = fade(p.delay_on);
    let time = if p.delay_sync {
        division_label(p.delay_division)
    } else {
        fmt_time(p.delay_time_ms)
    };
    let delay = screen(
        s,
        (2, 1),
        "Delay",
        if p.delay_on {
            Some(format!("{time} \u{00B7} {}", percent(p.delay_feedback)))
        } else {
            off()
        },
        move |b, width, k, window, cx| {
            let (top, bottom) = (PLOT_TOP + 4.0, AXIS_BOTTOM);
            paint_plot_grid(window, b, k, width, (PLOT_TOP, AXIS_BOTTOM), (1, 4));
            let at = mapper(b, k);
            let lead = 12.0;
            let pitch = (width - lead - 14.0) / repeats.len().max(8) as f32;
            let (dx, dy) = at(lead - 1.0, top);
            paint_rect(
                window,
                (dx, dy, 2.0 * k, (bottom - top) * k),
                color_a(WHITE, 0.5 * delay_alpha),
            );
            let mut labels = vec![(lead, "0".to_string())];
            for (i, db) in repeats.iter().enumerate() {
                let x = lead + pitch * (i + 1) as f32;
                let h = (1.0 + db / 48.0).clamp(0.0, 1.0) * (bottom - top);
                let ink = if ping_pong && i % 2 == 1 {
                    SECOND
                } else {
                    ACCENT
                };
                let (bx, by) = at(x - 2.0, bottom - h);
                paint_rect(
                    window,
                    (bx, by, 4.0 * k, h * k),
                    color_a(ink, 0.9 * delay_alpha),
                );
                if i < 4 {
                    labels.push((x, format!("{}\u{00D7}", i + 1)));
                }
            }
            paint_labels(window, cx, b, k, width, &labels);
        },
    );

    // Reverb: the level falling 60 dB in the decay time.
    let decay_sec = p.reverb_decay_sec;
    let reverb_alpha = fade(p.reverb_on);
    let span_ms = nice_ceiling(decay_sec * 1_000.0 * 1.2);
    let reverb = screen(
        s,
        (3, 1),
        "Reverb",
        if p.reverb_on {
            Some(format!("RT60 {}", fmt_time(decay_sec * 1_000.0)))
        } else {
            off()
        },
        move |b, width, k, window, cx| {
            let (top, bottom) = (PLOT_TOP + 4.0, AXIS_BOTTOM);
            paint_plot_grid(window, b, k, width, (PLOT_TOP, AXIS_BOTTOM), (1, 4));
            let at = mapper(b, k);
            let x_t = |ms: f32| (ms / span_ms).clamp(0.0, 1.0) * width;
            let step = nice_step(span_ms, 3);
            let mut labels = vec![(0.0, "0".to_string())];
            let mut t = step;
            while t < span_ms * 0.999 {
                let x = x_t(t);
                paint_polyline(
                    window,
                    &[at(x, PLOT_TOP), at(x, bottom)],
                    1.0,
                    color(GRID),
                    None,
                );
                labels.push((x, fmt_time(t)));
                t += step;
            }
            paint_labels(window, cx, b, k, width, &labels);
            let end = x_t(decay_sec * 1_000.0);
            let points = [(0.0, top), (end, bottom)];
            paint_shaded(window, b, k, &points, bottom, ACCENT, reverb_alpha);
        },
    );
    vec![drive, chorus, delay, reverb]
}

fn voice_fx_deck(c: &Ctx) -> Vec<AnyElement> {
    let s = c.s;
    vec![
        voice_card(c)
            .h(px(VOICE_ROW_H * s))
            .flex_none()
            .into_any_element(),
        div()
            .flex()
            .flex_row()
            .flex_1()
            .gap(px(COL_GAP * s))
            .child(drive_card(c))
            .child(chorus_card(c))
            .child(delay_card(c))
            .child(reverb_card(c))
            .into_any_element(),
    ]
}

fn voice_card(c: &Ctx) -> gpui::Div {
    let (p, s, d) = (c.p, c.s, defaults());
    let modes: Vec<&str> = VoiceMode::ALL.iter().map(|mode| mode.label()).collect();
    let mode_at = VoiceMode::ALL.iter().position(|mode| *mode == p.voice_mode);
    let on_mode = c.step(|p, index| {
        let mode = VoiceMode::ALL[(index.max(0) as usize).min(VoiceMode::ALL.len() - 1)];
        with(p, |p| p.voice_mode = mode)
    });
    let mode = section("Mode", s)
        .w(px((COL_W - 32.0) * s))
        .flex_shrink_0()
        .child(segmented(
            "wrap-synth-voice-mode",
            &modes,
            mode_at,
            s,
            on_mode,
        ));
    let controls = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .flex_1()
        .pl(px(24.0 * s))
        .child(knob(
            c,
            "wrap-synth-glide",
            travel("Glide", time_position(d.glide_ms, GLIDE_RANGE)),
            time_position(p.glide_ms, GLIDE_RANGE),
            fmt_time(p.glide_ms),
            SMALL3,
            |p, v| p.glide_ms = time_from_position(v, GLIDE_RANGE).round(),
        ))
        .child(kit::stepper(
            "wrap-synth-bend-range",
            "Bend",
            format!("\u{00B1}{}", p.pitch_bend_range.round() as i32),
            true,
            s,
            c.step(|p, delta| {
                with(p, |p| {
                    p.pitch_bend_range = (p.pitch_bend_range.round() + delta as f32).max(0.0)
                })
            }),
        ))
        .child(knob(
            c,
            "wrap-synth-velocity",
            travel("Velocity", d.velocity_amount),
            p.velocity_amount,
            percent(p.velocity_amount),
            SMALL3,
            |p, v| p.velocity_amount = v,
        ))
        .child(knob(
            c,
            "wrap-synth-analog",
            travel("Analog", d.analog),
            p.analog,
            percent(p.analog),
            SMALL3,
            |p, v| p.analog = v,
        ))
        .child(knob(
            c,
            "wrap-synth-phase-random",
            travel("Phase Rand", d.phase_random),
            p.phase_random,
            percent(p.phase_random),
            SMALL3,
            |p, v| p.phase_random = v,
        ));
    deck_card(s, None).child(card_title("Voice", s)).child(
        div()
            .flex()
            .flex_row()
            .items_center()
            .child(mode)
            .child(controls),
    )
}

fn drive_card(c: &Ctx) -> gpui::Div {
    let (p, s, d) = (c.p, c.s, defaults());
    deck_card(s, Some(COL_W))
        .child(noted(
            switched_title(
                "wrap-synth-dist-on",
                "Drive",
                p.dist_on,
                c,
                with(p, |p| p.dist_on = !p.dist_on),
            ),
            "A soft clip after the voices, made up to full scale: more drive, more harmonics, the same peak.",
            s,
        ))
        .child(knobs(vec![
            knob(
                c,
                "wrap-synth-dist-drive",
                travel("Drive", d.dist_drive),
                p.dist_drive,
                percent(p.dist_drive),
                LEAD,
                |p, v| p.dist_drive = v,
            ),
            knob(
                c,
                "wrap-synth-dist-mix",
                travel("Mix", d.dist_mix),
                p.dist_mix,
                percent(p.dist_mix),
                LEAD,
                |p, v| p.dist_mix = v,
            ),
        ]))
}

fn chorus_card(c: &Ctx) -> gpui::Div {
    let (p, s, d) = (c.p, c.s, defaults());
    deck_card(s, Some(COL_W))
        .child(noted(
            switched_title(
                "wrap-synth-chorus-on",
                "Chorus",
                p.chorus_on,
                c,
                with(p, |p| p.chorus_on = !p.chorus_on),
            ),
            "Two short delays swept by one sine, the right a quarter cycle behind the left.",
            s,
        ))
        .child(knobs(vec![
            knob(
                c,
                "wrap-synth-chorus-rate",
                travel("Rate", rate_position(d.chorus_rate_hz, CHORUS_RATE_RANGE)),
                rate_position(p.chorus_rate_hz, CHORUS_RATE_RANGE),
                format_rate(p.chorus_rate_hz),
                LEAD,
                |p, v| p.chorus_rate_hz = rate_from_position(v, CHORUS_RATE_RANGE),
            ),
            knob(
                c,
                "wrap-synth-chorus-depth",
                travel("Depth", d.chorus_depth),
                p.chorus_depth,
                percent(p.chorus_depth),
                LEAD,
                |p, v| p.chorus_depth = v,
            ),
            knob(
                c,
                "wrap-synth-chorus-mix",
                travel("Mix", d.chorus_mix),
                p.chorus_mix,
                percent(p.chorus_mix),
                LEAD,
                |p, v| p.chorus_mix = v,
            ),
        ]))
}

fn delay_card(c: &Ctx) -> gpui::Div {
    let (p, s, d) = (c.p, c.s, defaults());
    let max = echospace::MAX_DIVISION_WIRE as u8;
    let time = if p.delay_sync {
        kit::chooser(
            "wrap-synth-delay-division",
            "Time",
            division_label(p.delay_division),
            48.0,
            true,
            s,
            c.step(move |p, delta| {
                with(p, |p| {
                    p.delay_division = if delta < 0 {
                        p.delay_division.saturating_sub(1)
                    } else {
                        (p.delay_division + 1).min(max)
                    }
                })
            }),
        )
    } else {
        knob(
            c,
            "wrap-synth-delay-time",
            travel("Time", time_position(d.delay_time_ms, DELAY_TIME_RANGE)),
            time_position(p.delay_time_ms, DELAY_TIME_RANGE),
            fmt_time(p.delay_time_ms),
            SMALL3,
            |p, v| p.delay_time_ms = time_from_position(v, DELAY_TIME_RANGE).round(),
        )
    };
    let toggles = div()
        .flex()
        .flex_row()
        .gap(px(6.0 * s))
        .child(
            div().flex_1().child(
                kit::toggle(
                    "wrap-synth-delay-sync",
                    "Sync",
                    p.delay_sync,
                    accent(),
                    true,
                    s,
                    c.click(with(p, |p| p.delay_sync = !p.delay_sync)),
                )
                .w_full(),
            ),
        )
        .child(
            div().flex_1().child(
                kit::toggle(
                    "wrap-synth-delay-ping-pong",
                    "Ping-Pong",
                    p.delay_ping_pong,
                    accent(),
                    true,
                    s,
                    c.click(with(p, |p| p.delay_ping_pong = !p.delay_ping_pong)),
                )
                .w_full(),
            ),
        );
    deck_card(s, Some(COL_W))
        .child(switched_title(
            "wrap-synth-delay-on",
            "Delay",
            p.delay_on,
            c,
            with(p, |p| p.delay_on = !p.delay_on),
        ))
        .child(toggles)
        .child(knobs(vec![time]))
        .child(knobs(vec![
            knob(
                c,
                "wrap-synth-delay-feedback",
                travel("Feedback", d.delay_feedback / FEEDBACK_MAX),
                p.delay_feedback / FEEDBACK_MAX,
                percent(p.delay_feedback),
                SMALL3,
                |p, v| p.delay_feedback = v * FEEDBACK_MAX,
            ),
            knob(
                c,
                "wrap-synth-delay-mix",
                travel("Mix", d.delay_mix),
                p.delay_mix,
                percent(p.delay_mix),
                SMALL3,
                |p, v| p.delay_mix = v,
            ),
        ]))
}

fn reverb_card(c: &Ctx) -> gpui::Div {
    let (p, s, d) = (c.p, c.s, defaults());
    deck_card(s, Some(COL_W))
        .child(switched_title(
            "wrap-synth-reverb-on",
            "Reverb",
            p.reverb_on,
            c,
            with(p, |p| p.reverb_on = !p.reverb_on),
        ))
        .child(knobs(vec![
            knob(
                c,
                "wrap-synth-reverb-size",
                travel("Size", d.reverb_size),
                p.reverb_size,
                percent(p.reverb_size),
                SMALL3,
                |p, v| p.reverb_size = v,
            ),
            knob(
                c,
                "wrap-synth-reverb-decay",
                travel(
                    "Decay",
                    time_position(d.reverb_decay_sec, REVERB_DECAY_RANGE),
                ),
                time_position(p.reverb_decay_sec, REVERB_DECAY_RANGE),
                format!("{:.1} s", p.reverb_decay_sec),
                SMALL3,
                |p, v| {
                    p.reverb_decay_sec =
                        (time_from_position(v, REVERB_DECAY_RANGE) * 10.0).round() / 10.0
                },
            ),
        ]))
        .child(knobs(vec![
            knob(
                c,
                "wrap-synth-reverb-damping",
                travel("Damping", d.reverb_damping),
                p.reverb_damping,
                percent(p.reverb_damping),
                SMALL3,
                |p, v| p.reverb_damping = v,
            ),
            knob(
                c,
                "wrap-synth-reverb-mix",
                travel("Mix", d.reverb_mix),
                p.reverb_mix,
                percent(p.reverb_mix),
                SMALL3,
                |p, v| p.reverb_mix = v,
            ),
        ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn knob_mappings_round_trip() {
        for hz in [40.0, 220.0, 5_200.0, 20_000.0] {
            let back = cutoff_from_position(cutoff_position(hz));
            assert!((back - hz).abs() / hz < 1.0e-3, "{hz} → {back}");
        }
        for ms in [0.5, 8.0, 420.0, 5_000.0] {
            let back = time_from_position(time_position(ms, ATTACK_RANGE), ATTACK_RANGE);
            assert!((back - ms).abs() < 1.0e-2, "{ms} → {back}");
        }
        for hz in [0.01, 0.3, 4.0, 30.0] {
            let back = rate_from_position(rate_position(hz, LFO_RANGE), LFO_RANGE);
            assert!((back - hz).abs() / hz < 1.0e-3, "{hz} → {back}");
        }
        assert_eq!(master_from_position(master_position(-6.0)), -6.0);
        // The knob's ends are the param's ends.
        assert_eq!(time_from_position(1.0, RELEASE_RANGE), RELEASE_RANGE.1);
        assert_eq!(master_from_position(0.0), MASTER_RANGE.0);
    }

    #[test]
    fn a_knob_travel_is_fine_enough_for_a_log_sweep() {
        // A dial rounds to its readout; the travel dial's steps are a
        // thousandth of a tenth of the sweep, so a cutoff drag never jumps.
        let dial = travel("Cutoff", 0.5);
        let a = cutoff_from_position(dial.at(0.5) / TRAVEL);
        let b = cutoff_from_position(dial.at(0.5 + 1.0e-4) / TRAVEL);
        assert!((b / a - 1.0).abs() < 2.0e-3, "{a} → {b}");
        // Its reset lands on the default it was given.
        assert!((dial.default / TRAVEL - 0.5).abs() < 1.0e-6);
        let signed = signed_travel("Fine", -0.2);
        assert!((signed.norm(0.0) - 0.5).abs() < 1.0e-6, "centred at zero");
        assert!((signed.at(signed.norm(-0.2 * TRAVEL)) / TRAVEL + 0.2).abs() < 1.0e-3);
    }

    #[test]
    fn the_filter_curve_passes_below_and_cuts_above() {
        let kind = FilterType::LowPass12;
        assert!(filter_response_db(kind, 1_000.0, 0.0, 50.0).abs() < 0.5);
        assert!(filter_response_db(kind, 1_000.0, 0.0, 10_000.0) < -30.0);
        // Resonance peaks at the cutoff.
        assert!(filter_response_db(kind, 1_000.0, 0.9, 1_000.0) > 6.0);
        // A high-pass is the other way round.
        assert!(filter_response_db(FilterType::HighPass12, 1_000.0, 0.0, 50.0) < -30.0);
    }

    #[test]
    fn the_envelope_starts_and_ends_silent_and_holds_its_sustain() {
        let env = EnvShape::of(Env::Amp, &wrapsynth::default_params());
        let points = envelope_points(&env, 200.0, 100.0);
        assert_eq!(points[0], (0.0, 100.0));
        assert_eq!(points[4].1, 100.0);
        assert_eq!(points[1].1, 0.0);
        assert!((points[2].1 - 100.0 * (1.0 - env.sustain)).abs() < 1.0e-3);
        assert_eq!(points[2].1, points[3].1);
        assert!(points.windows(2).all(|pair| pair[0].0 <= pair[1].0));
        assert!(points[4].0 <= 200.0);
        // The traced curve runs through every corner, left to right.
        let curve = envelope_curve(&env, 200.0, 100.0);
        assert_eq!(curve[0], points[0]);
        assert!((curve.last().unwrap().1 - points[4].1).abs() < 1.0e-3);
        assert!(curve.windows(2).all(|pair| pair[0].0 <= pair[1].0 + 1.0e-4));
    }

    #[test]
    fn the_octave_stays_on_the_keyboard() {
        let mut panel = WrapSynthPanelState::default();
        panel.shift_keyboard_octave(-10);
        assert_eq!(panel.keyboard_root, 0);
        panel.shift_keyboard_octave(20);
        assert_eq!(panel.keyboard_root, KEYBOARD_HIGHEST_ROOT);
    }

    #[test]
    fn a_preset_is_recognised_until_it_is_edited() {
        let bank = presets();
        assert_eq!(preset_index(&bank[3].params), Some(3));
        let mut off = bank[3].params;
        off.power = false;
        assert_eq!(preset_index(&off), Some(3), "power is not part of a preset");
        let mut edited = bank[3].params;
        edited.cutoff_hz += 1.0;
        assert_eq!(preset_index(&edited), None);
    }

    #[test]
    fn stepping_presets_wraps_and_keeps_the_power() {
        let bank = presets();
        let mut first = bank[0].params;
        first.power = false;
        let back = stepped_preset(&first, -1);
        assert_eq!(preset_index(&back), Some(bank.len() - 1));
        assert!(!back.power);
        let mut edited = bank[3].params;
        edited.cutoff_hz += 1.0;
        assert_eq!(preset_index(&stepped_preset(&edited, 1)), Some(0));
    }

    #[test]
    fn cycling_wraps_both_ways() {
        assert_eq!(
            cycle(&ModSource::ALL, ModSource::None, -1),
            ModSource::Random
        );
        assert_eq!(cycle(&ModDest::ALL, ModDest::Pan, 1), ModDest::None);
    }

    #[test]
    #[allow(
        clippy::assertions_on_constants,
        reason = "documents the plate's layout constants as a named test"
    )]
    fn the_columns_fill_the_well_and_the_deck() {
        // Four boxes across the well, 16 in from each side of its rim.
        let boxes = BOX_INSET * 2.0 + span(4);
        assert!((boxes - (1011.0 - 2.0)).abs() < 1.0e-3, "{boxes}");
        // The deck's columns sit under them.
        assert!((DECK_PAD_X * 2.0 + span(4) - 1011.0).abs() < 1.0e-3);
        assert!(WELL_TOP + WELL_H < DECK_TOP);
        assert!(DECK_TOP + DECK_H <= DECK_BOTTOM + 1.0e-3);
        assert!(MOD_ROW_H + VOICE_ROW_H < DECK_H);
    }

    #[test]
    fn the_drive_curve_keeps_full_scale_and_mixes_with_the_dry() {
        let mut p = wrapsynth::default_params();
        p.dist_drive = 0.8;
        p.dist_mix = 1.0;
        assert!((drive_transfer(&p, 1.0) - 1.0).abs() < 1.0e-5);
        assert!((drive_transfer(&p, -1.0) + 1.0).abs() < 1.0e-5);
        assert!(
            drive_transfer(&p, 0.2) > 0.2,
            "driven, a quiet sample comes up"
        );
        p.dist_mix = 0.0;
        assert_eq!(drive_transfer(&p, 0.2), 0.2);
    }

    #[test]
    fn the_repeats_fall_by_the_feedback() {
        let repeats = delay_repeats(0.5);
        assert_eq!(repeats[0], 0.0);
        assert!((repeats[1] + 6.02).abs() < 0.01);
        assert!(repeats.iter().all(|db| *db >= -48.0));
        assert_eq!(delay_repeats(0.0).len(), 1);
        assert_eq!(delay_repeats(0.95).len(), 24);
    }
}

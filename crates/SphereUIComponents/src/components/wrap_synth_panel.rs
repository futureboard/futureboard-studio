//! The built-in WrapSynth's panel.
//!
//! The header carries the preset, the page and the power switch. Three pages,
//! each laid out along the signal path:
//!
//! * **Synth** — two wavetable **oscillators**, each drawn as the cycle the
//!   DSP plays ([`wrapsynth::osc_cycle`]); the **filter**, its response drawn
//!   by the DSP's own prototype ([`wrapsynth::filter::response_db`]); the
//!   **amp envelope**, drawn with the curves it really follows; the
//!   **output**.
//! * **Modulation** — the filter and modulation envelopes, both LFOs, and
//!   the eight-slot matrix.
//! * **Voice & FX** — voicing (poly/mono/legato, glide, bend, velocity,
//!   analog drift), then drive, chorus, delay and reverb.
//!
//! A keyboard across the full width plays through the track on every page.
//!
//! This file only renders: the state is the insert's params, held by
//! [`crate::components::wrap_synth_window::WrapSynthEditorWindow`]. The
//! module cards, graphs and keyboard are the samplers' own, so the built-in
//! instruments read as one family.

use std::sync::{Arc, OnceLock};

use gpui::{
    AnyElement, App, InteractiveElement, IntoElement, ParentElement, StatefulInteractiveElement,
    Styled, Window, canvas, div, fill, px, svg,
};

use crate::assets;
use crate::components::controls::{
    FbSegment, fb_checkbox, fb_segment, fb_segmented_track, fb_stepper_button,
};
use crate::components::knob::{knob_bipolar, knob_with_default};
use crate::components::quick_sampler_panel::{
    GRAPH_H, GraphColors, I32Cb, KNOB_SIZE, NoteCb, controls_row, format_hz, format_ms, knob_cell,
    module, paint_curve, paint_graph_frame, percent, piano_keyboard, rect, sampler_keyboard_footer,
};
use crate::theme::{Colors, radius, size as ui_size, space, typography};
use wrapsynth::envelope::{ATTACK_RATIO, DECAY_RATIO, segment_progress};
use wrapsynth::{
    FactoryPreset, FilterModel, FilterType, LfoShape, MAX_UNISON, MAX_VOICES, MOD_SLOTS, ModDest,
    ModSource, Params, VoiceMode, Wavetable,
};

/// Four octaves from C2.
const KEYBOARD_DEFAULT_ROOT: u8 = 36;
/// The oscillator drawings: taller than the samplers' graphs, the shape is
/// the point of them.
const WAVE_H: f32 = 76.0;
const CUTOFF_MIN_HZ: f32 = wrapsynth::CUTOFF_MIN_HZ;
const CUTOFF_MAX_HZ: f32 = wrapsynth::CUTOFF_MAX_HZ;

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
        let highest = crate::components::quick_sampler_panel::KEYBOARD_HIGHEST_ROOT;
        let root = self.keyboard_root as i32 + delta * 12;
        self.keyboard_root = root.clamp(0, highest as i32) as u8;
    }
}

pub(crate) type SynthParamsCb = Arc<dyn Fn(&Params, &mut Window, &mut App) + 'static>;
pub(crate) type SynthPageCb = Arc<dyn Fn(&SynthPage, &mut Window, &mut App) + 'static>;

#[derive(Clone)]
pub struct WrapSynthCallbacks {
    /// One complete params value, so a knob drag cannot land a partial edit.
    pub on_set_params: SynthParamsCb,
    pub on_page: SynthPageCb,
    pub on_note_on: NoteCb,
    pub on_note_off: NoteCb,
    pub on_shift_octave: I32Cb,
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

/// The whole panel.
pub fn wrap_synth_panel(panel: &WrapSynthPanelState, cb: WrapSynthCallbacks) -> AnyElement {
    let p = panel.params;
    let set = &cb.on_set_params;
    // Off, the synth is silent: everything stays editable, but reads as
    // asleep.
    let dim = if p.power { 1.0 } else { 0.48 };
    let page = match panel.page {
        SynthPage::Synth => synth_page(p, set),
        SynthPage::Modulation => modulation_page(p, set),
        SynthPage::VoiceFx => voice_fx_page(p, set),
    };
    div()
        .flex()
        .flex_col()
        .size_full()
        .bg(Colors::surface_window())
        .child(header(panel, &cb))
        .child(
            div()
                .id("wrap-synth-body")
                .flex()
                .flex_col()
                .flex_1()
                .min_h(px(0.0))
                .overflow_y_scroll()
                .px(px(space::SECTION))
                .pt(px(space::LOOSE))
                .pb(px(space::SECTION))
                .gap(px(space::BASE))
                .opacity(dim)
                .child(page),
        )
        .child(keyboard_footer(panel, &cb))
        .into_any_element()
}

fn header(panel: &WrapSynthPanelState, cb: &WrapSynthCallbacks) -> AnyElement {
    let p = panel.params;
    let on_power = cb.on_set_params.clone();
    let subtitle = if p.power {
        format!(
            "Wavetable synthesizer · {MAX_VOICES} voices · 2 oscillators · up to {MAX_UNISON}× unison"
        )
    } else {
        "Off — the synth is silent until it is switched on".to_string()
    };
    let mut pages = fb_segmented_track();
    let last = SynthPage::ALL.len() - 1;
    for (index, page) in SynthPage::ALL.into_iter().enumerate() {
        let on_page = cb.on_page.clone();
        pages = pages.child(fb_segment(
            ("wrap-synth-page", index),
            page.label(),
            panel.page == page,
            segment_position(index, last),
            move |_, w, cx| on_page(&page, w, cx),
        ));
    }
    div()
        .flex()
        .flex_row()
        .items_center()
        .flex_shrink_0()
        .gap(px(space::LOOSE))
        .px(px(space::SECTION))
        .py(px(space::BASE))
        .border_b(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_base())
        .child(
            div()
                .flex()
                .items_center()
                .justify_center()
                .flex_shrink_0()
                .size(px(ui_size::PROMINENT))
                .rounded(px(radius::CONTROL))
                .bg(Colors::surface_card())
                .child(
                    svg()
                        .path(assets::ICON_MUSIC_PATH)
                        .size(px(16.0))
                        .text_color(if p.power {
                            Colors::accent_primary()
                        } else {
                            Colors::text_muted()
                        }),
                ),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(0.0))
                .gap(px(space::HAIR))
                .child(
                    div()
                        .truncate()
                        .text_size(px(typography::UI_MD))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(Colors::text_primary())
                        .child("WrapSynth"),
                )
                .child(
                    div()
                        .truncate()
                        .text_size(px(typography::UI_XS))
                        .text_color(Colors::text_muted())
                        .child(subtitle),
                ),
        )
        .child(preset_picker(p, &cb.on_set_params))
        .child(pages)
        .child(fb_checkbox(
            "wrap-synth-power",
            "Power",
            p.power,
            true,
            move |_, w, cx| on_power(&with(p, |p| p.power = !p.power), w, cx),
        ))
        .into_any_element()
}

/// `‹ Preset ›`: steps through the factory bank. An edited patch reads as
/// "Custom", and stepping from it starts at the first preset either side.
fn preset_picker(p: Params, on_set: &SynthParamsCb) -> AnyElement {
    let bank = presets();
    let current = preset_index(&p);
    let label = current.map_or("Custom", |index| bank[index].name);
    let step = |delta: isize| -> Params {
        let len = bank.len() as isize;
        let index = match current {
            Some(index) => (index as isize + delta).rem_euclid(len),
            None if delta > 0 => 0,
            None => len - 1,
        };
        Params {
            power: p.power,
            ..bank[index as usize].params
        }
    };
    choice_stepper(
        "wrap-synth-preset",
        "Preset",
        label.to_string(),
        (step(-1), step(1)),
        on_set,
        116.0,
    )
}

fn segment_position(index: usize, last: usize) -> FbSegment {
    match index {
        0 => FbSegment::First,
        i if i == last => FbSegment::Last,
        _ => FbSegment::Middle,
    }
}

// ── Controls ───────────────────────────────────────────────────────────────

fn with(p: Params, apply: impl FnOnce(&mut Params)) -> Params {
    let mut next = p;
    apply(&mut next);
    wrapsynth::ipc::sanitize_params(&mut next);
    next
}

/// A knob over `0..1` of some mapping of one field: `position` is where the
/// field sits now, `default` where a double-click puts it.
#[allow(clippy::too_many_arguments)]
fn knob(
    id: impl Into<gpui::SharedString>,
    caption: &'static str,
    readout: String,
    p: Params,
    (position, default): (f32, f32),
    on_set: &SynthParamsCb,
    apply: impl Fn(&mut Params, f32) + 'static,
) -> AnyElement {
    let on_change = on_set.clone();
    knob_cell(
        caption,
        readout,
        knob_with_default(
            id,
            position,
            0.0,
            1.0,
            KNOB_SIZE,
            Colors::accent_primary(),
            default,
            move |value, w, cx| on_change(&with(p, |p| apply(p, *value)), w, cx),
        ),
    )
}

/// A centred knob for a signed field, over its own range.
fn signed_knob(
    id: impl Into<gpui::SharedString>,
    caption: &'static str,
    readout: String,
    p: Params,
    (value, min, max, default): (f32, f32, f32, f32),
    on_set: &SynthParamsCb,
    apply: impl Fn(&mut Params, f32) + 'static,
) -> AnyElement {
    let on_change = on_set.clone();
    knob_cell(
        caption,
        readout,
        knob_bipolar(
            id,
            value,
            min,
            max,
            KNOB_SIZE,
            Colors::accent_primary(),
            None,
            default,
            move |value, w, cx| on_change(&with(p, |p| apply(p, *value)), w, cx),
        ),
    )
}

/// A `− value +` stepper for a whole-number field.
fn stepper(
    id: &'static str,
    caption: &'static str,
    readout: String,
    (dec, inc): (Params, Params),
    on_set: &SynthParamsCb,
) -> AnyElement {
    stepper_with(id, caption, readout, (dec, inc), on_set, ("−", "+"), 36.0)
}

/// `‹ label ›` for a choice from a long list.
fn choice_stepper(
    id: impl Into<gpui::SharedString>,
    caption: &'static str,
    label: String,
    (prev, next): (Params, Params),
    on_set: &SynthParamsCb,
    width: f32,
) -> AnyElement {
    stepper_with(id, caption, label, (prev, next), on_set, ("‹", "›"), width)
}

fn stepper_with(
    id: impl Into<gpui::SharedString>,
    caption: &'static str,
    readout: String,
    (dec, inc): (Params, Params),
    on_set: &SynthParamsCb,
    (dec_label, inc_label): (&'static str, &'static str),
    width: f32,
) -> AnyElement {
    let id: gpui::SharedString = id.into();
    let on_dec = on_set.clone();
    let on_inc = on_set.clone();
    div()
        .flex()
        .flex_col()
        .items_center()
        .gap(px(space::HAIR))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::HAIR))
                .child(fb_stepper_button(
                    (id.clone(), 0usize),
                    dec_label,
                    move |_, w, cx| on_dec(&dec, w, cx),
                ))
                .child(
                    div()
                        .w(px(width))
                        .flex()
                        .justify_center()
                        .truncate()
                        .text_size(px(typography::UI_SM))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(Colors::text_primary())
                        .child(readout),
                )
                .child(fb_stepper_button(
                    (id, 1usize),
                    inc_label,
                    move |_, w, cx| on_inc(&inc, w, cx),
                )),
        )
        .child(
            div()
                .text_size(px(typography::DENSE_CAPTION))
                .text_color(Colors::text_muted())
                .child(caption),
        )
        .into_any_element()
}

/// One segmented row over a list of choices.
fn segments<T: Copy + PartialEq + 'static>(
    id: &'static str,
    choices: &[(T, &'static str)],
    current: T,
    set: impl Fn(T) -> Params,
    on_set: &SynthParamsCb,
) -> AnyElement {
    let mut track = fb_segmented_track();
    let last = choices.len().saturating_sub(1);
    for (index, &(value, label)) in choices.iter().enumerate() {
        let on_change = on_set.clone();
        let next = set(value);
        track = track.child(fb_segment(
            (id, index),
            label,
            current == value,
            segment_position(index, last),
            move |_, w, cx| on_change(&next, w, cx),
        ));
    }
    track.into_any_element()
}

fn toggle(
    id: &'static str,
    label: &'static str,
    value: bool,
    next: Params,
    on_set: &SynthParamsCb,
) -> AnyElement {
    let on_change = on_set.clone();
    fb_checkbox(id, label, value, true, move |_, w, cx| {
        on_change(&next, w, cx)
    })
    .into_any_element()
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

fn master_position(db: f32) -> f32 {
    (db - MASTER_RANGE.0) / (MASTER_RANGE.1 - MASTER_RANGE.0)
}

fn master_from_position(position: f32) -> f32 {
    // Tenths of a dB: a readout that does not flicker through hundredths.
    let db = MASTER_RANGE.0 + position * (MASTER_RANGE.1 - MASTER_RANGE.0);
    (db * 10.0).round() / 10.0
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

/// A card that grows to share its row, like [`module`], but wider.
fn wide(card: AnyElement) -> AnyElement {
    div()
        .flex()
        .flex_1()
        .min_w(px(300.0))
        .child(card)
        .into_any_element()
}

fn row(children: impl IntoIterator<Item = AnyElement>) -> AnyElement {
    div()
        .flex()
        .flex_row()
        .flex_wrap()
        .gap(px(space::BASE))
        .children(children)
        .into_any_element()
}

// ── Synth page ─────────────────────────────────────────────────────────────

fn synth_page(p: Params, set: &SynthParamsCb) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(space::BASE))
        .child(row([oscillator_a(p, set), oscillator_b(p, set)]))
        .child(row([
            wide(filter_module(p, set)),
            amp_module(p, set),
            output_module(p, set),
        ]))
        .into_any_element()
}

/// One cycle of the oscillator, through the DSP's own wavetable.
fn wave_graph(wave: Wavetable, position: f32, level: f32) -> AnyElement {
    let colors = GraphColors::resolve(level > 0.0);
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            paint_graph_frame(window, bounds, &colors);
            let x0 = f32::from(bounds.origin.x);
            let y0 = f32::from(bounds.origin.y);
            let w = f32::from(bounds.size.width).max(1.0);
            let h = f32::from(bounds.size.height).max(1.0);
            let mid = y0 + h * 0.5;
            let half = h * 0.5 - 6.0;
            window.paint_quad(fill(rect(x0, mid, w, 1.0), colors.guide));
            let columns = (w as usize).clamp(2, 512);
            let curve: Vec<(f32, f32)> = (0..columns)
                .map(|i| {
                    let phase = i as f32 / (columns - 1) as f32;
                    let sample = wrapsynth::osc_cycle(wave, position, phase);
                    (x0 + phase * w, mid - sample.clamp(-1.0, 1.0) * half)
                })
                .collect();
            paint_curve(window, &curve, mid, &colors);
        },
    )
    .w_full()
    .h(px(WAVE_H))
    .into_any_element()
}

/// `‹ Classic Saw ›` over every table, in the picker's order.
fn table_picker(
    id: &'static str,
    current: Wavetable,
    set: impl Fn(Wavetable) -> Params,
    on_set: &SynthParamsCb,
) -> AnyElement {
    let menu = Wavetable::MENU;
    let at = menu.iter().position(|table| *table == current).unwrap_or(0);
    let prev = menu[(at + menu.len() - 1) % menu.len()];
    let next = menu[(at + 1) % menu.len()];
    choice_stepper(
        id,
        "Wavetable",
        current.label().to_string(),
        (set(prev), set(next)),
        on_set,
        120.0,
    )
}

fn oscillator_a(p: Params, on_set: &SynthParamsCb) -> AnyElement {
    let morphs = p.osc_a_wave.morphs();
    wide(module(
        "OSCILLATOR A",
        Some(wave_graph(p.osc_a_wave, p.osc_a_position, p.osc_a_level)),
        controls_row()
            .items_center()
            .child(table_picker(
                "wrap-synth-osc-a-wave",
                p.osc_a_wave,
                |wave| with(p, |p| p.osc_a_wave = wave),
                on_set,
            ))
            .child(knob(
                "wrap-synth-osc-a-position",
                if morphs { "WT Pos" } else { "Position" },
                percent(p.osc_a_position),
                p,
                (p.osc_a_position, 0.0),
                on_set,
                |p, v| p.osc_a_position = v,
            ))
            .child(knob(
                "wrap-synth-osc-a-level",
                "Level",
                percent(p.osc_a_level),
                p,
                (p.osc_a_level, 0.8),
                on_set,
                |p, v| p.osc_a_level = v,
            ))
            .child(stepper(
                "wrap-synth-unison",
                "Unison",
                format!("{}×", p.unison),
                (
                    with(p, |p| p.unison = p.unison.saturating_sub(1)),
                    with(p, |p| p.unison = p.unison.saturating_add(1)),
                ),
                on_set,
            ))
            .child(knob(
                "wrap-synth-unison-detune",
                "Detune",
                format!("{:.0} ct", p.unison_detune_cents),
                p,
                (p.unison_detune_cents / 50.0, 12.0 / 50.0),
                on_set,
                |p, v| p.unison_detune_cents = (v * 50.0).round(),
            ))
            .child(knob(
                "wrap-synth-unison-blend",
                "Blend",
                percent(p.unison_blend),
                p,
                (p.unison_blend, 0.5),
                on_set,
                |p, v| p.unison_blend = v,
            )),
    ))
}

fn oscillator_b(p: Params, on_set: &SynthParamsCb) -> AnyElement {
    let morphs = p.osc_b_wave.morphs();
    wide(module(
        "OSCILLATOR B",
        Some(wave_graph(p.osc_b_wave, p.osc_b_position, p.osc_b_level)),
        controls_row()
            .items_center()
            .child(table_picker(
                "wrap-synth-osc-b-wave",
                p.osc_b_wave,
                |wave| with(p, |p| p.osc_b_wave = wave),
                on_set,
            ))
            .child(knob(
                "wrap-synth-osc-b-position",
                if morphs { "WT Pos" } else { "Position" },
                percent(p.osc_b_position),
                p,
                (p.osc_b_position, 0.0),
                on_set,
                |p, v| p.osc_b_position = v,
            ))
            .child(knob(
                "wrap-synth-osc-b-level",
                "Level",
                percent(p.osc_b_level),
                p,
                (p.osc_b_level, 0.45),
                on_set,
                |p, v| p.osc_b_level = v,
            ))
            .child(stepper(
                "wrap-synth-osc-b-semitones",
                "Semitones",
                format!("{:+}", p.osc_b_semitones.round() as i32),
                (
                    with(p, |p| p.osc_b_semitones = p.osc_b_semitones.round() - 1.0),
                    with(p, |p| p.osc_b_semitones = p.osc_b_semitones.round() + 1.0),
                ),
                on_set,
            ))
            .child(signed_knob(
                "wrap-synth-osc-b-fine",
                "Fine",
                format!("{:+.0} ct", p.osc_b_detune_cents),
                p,
                (p.osc_b_detune_cents, -50.0, 50.0, 9.0),
                on_set,
                |p, v| p.osc_b_detune_cents = v.round(),
            )),
    ))
}

// ── Filter ─────────────────────────────────────────────────────────────────

/// Magnitude in dB of `kind` at `hz`: the DSP's own linear prototype.
pub(crate) fn filter_response_db(kind: FilterType, cutoff: f32, resonance: f32, hz: f32) -> f32 {
    wrapsynth::filter::response_db(kind, cutoff, resonance, hz)
}

fn filter_graph(p: Params) -> AnyElement {
    let colors = GraphColors::resolve(true);
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            paint_graph_frame(window, bounds, &colors);
            let x0 = f32::from(bounds.origin.x);
            let y0 = f32::from(bounds.origin.y);
            let w = f32::from(bounds.size.width).max(1.0);
            let h = f32::from(bounds.size.height).max(1.0);
            // -36 dB at the bottom, +18 dB at the top; 0 dB a third down.
            let y_for = |db: f32| y0 + ((18.0 - db.clamp(-36.0, 18.0)) / 54.0) * h;
            let columns = (w as usize).clamp(2, 512);
            let curve: Vec<(f32, f32)> = (0..columns)
                .map(|i| {
                    let t = i as f32 / (columns - 1) as f32;
                    let hz = 20.0 * 1_000.0_f32.powf(t);
                    (
                        x0 + t * w,
                        y_for(filter_response_db(
                            p.filter_type,
                            p.cutoff_hz,
                            p.resonance,
                            hz,
                        )),
                    )
                })
                .collect();
            paint_curve(window, &curve, y0 + h, &colors);
            // The cutoff, marked, on the graph's own 20 Hz – 20 kHz axis.
            let x = x0 + ((p.cutoff_hz / 20.0).ln() / 1_000.0_f32.ln()).clamp(0.0, 1.0) * w;
            window.paint_quad(fill(rect(x, y0, 1.0, h), colors.guide));
        },
    )
    .w_full()
    .h(px(GRAPH_H))
    .into_any_element()
}

fn filter_module(p: Params, on_set: &SynthParamsCb) -> AnyElement {
    let model = p.filter_type.model();
    let models: Vec<(FilterModel, &'static str)> = FilterModel::ALL
        .iter()
        .map(|model| (*model, model.label()))
        .collect();
    let types: Vec<(FilterType, &'static str)> = model
        .types()
        .iter()
        .map(|kind| (*kind, kind.label()))
        .collect();
    module(
        "FILTER",
        Some(filter_graph(p)),
        div()
            .flex()
            .flex_col()
            .gap(px(space::BASE))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap(px(space::BASE))
                    .child(segments(
                        "wrap-synth-filter-model",
                        &models,
                        model,
                        |model| with(p, |p| p.filter_type = model.types()[0]),
                        on_set,
                    ))
                    .child(segments(
                        "wrap-synth-filter-type",
                        &types,
                        p.filter_type,
                        |kind| with(p, |p| p.filter_type = kind),
                        on_set,
                    )),
            )
            .child(
                controls_row()
                    .child(knob(
                        "wrap-synth-cutoff",
                        "Cutoff",
                        format_hz(p.cutoff_hz),
                        p,
                        (cutoff_position(p.cutoff_hz), cutoff_position(5_200.0)),
                        on_set,
                        |p, v| p.cutoff_hz = cutoff_from_position(v),
                    ))
                    .child(knob(
                        "wrap-synth-resonance",
                        "Resonance",
                        percent(p.resonance / 0.95),
                        p,
                        (p.resonance / 0.95, 0.15 / 0.95),
                        on_set,
                        |p, v| p.resonance = v * 0.95,
                    ))
                    .child(knob(
                        "wrap-synth-drive",
                        "Drive",
                        percent(p.filter_drive),
                        p,
                        (p.filter_drive, 0.0),
                        on_set,
                        |p, v| p.filter_drive = v,
                    ))
                    .child(signed_knob(
                        "wrap-synth-filter-env-amount",
                        "Env Amt",
                        format!("{:+.0}%", p.filter_env_amount * 100.0),
                        p,
                        (p.filter_env_amount, -1.0, 1.0, 0.18),
                        on_set,
                        |p, v| p.filter_env_amount = v,
                    ))
                    .child(knob(
                        "wrap-synth-key-track",
                        "Key Track",
                        percent(p.filter_key_track),
                        p,
                        (p.filter_key_track, 0.5),
                        on_set,
                        |p, v| p.filter_key_track = v,
                    )),
            ),
    )
}

// ── Envelopes ──────────────────────────────────────────────────────────────

/// An envelope's four times and level, as the editor draws it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct EnvShape {
    pub attack_ms: f32,
    pub decay_ms: f32,
    pub sustain: f32,
    pub release_ms: f32,
}

impl EnvShape {
    fn amp(p: &Params) -> Self {
        Self {
            attack_ms: p.attack_ms,
            decay_ms: p.decay_ms,
            sustain: p.sustain,
            release_ms: p.release_ms,
        }
    }

    fn filter(p: &Params) -> Self {
        Self {
            attack_ms: p.filter_attack_ms,
            decay_ms: p.filter_decay_ms,
            sustain: p.filter_sustain,
            release_ms: p.filter_release_ms,
        }
    }

    fn modulation(p: &Params) -> Self {
        Self {
            attack_ms: p.mod_attack_ms,
            decay_ms: p.mod_decay_ms,
            sustain: p.mod_sustain,
            release_ms: p.mod_release_ms,
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

fn envelope_graph(env: EnvShape) -> AnyElement {
    let colors = GraphColors::resolve(true);
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            paint_graph_frame(window, bounds, &colors);
            let x0 = f32::from(bounds.origin.x);
            let y0 = f32::from(bounds.origin.y);
            let w = f32::from(bounds.size.width).max(1.0);
            let h = f32::from(bounds.size.height).max(1.0);
            let inset = 4.0;
            let curve: Vec<(f32, f32)> = envelope_curve(&env, w - inset * 2.0, h - inset * 2.0)
                .iter()
                .map(|&(x, y)| (x0 + inset + x, y0 + inset + y))
                .collect();
            paint_curve(window, &curve, y0 + h, &colors);
            for &(x, y) in &envelope_points(&env, w - inset * 2.0, h - inset * 2.0)[1..4] {
                let (x, y) = (x0 + inset + x, y0 + inset + y);
                window.paint_quad(
                    fill(rect(x - 2.5, y - 2.5, 5.0, 5.0), colors.line).corner_radii(px(2.5)),
                );
            }
        },
    )
    .w_full()
    .h(px(GRAPH_H))
    .into_any_element()
}

/// Which envelope a card edits.
#[derive(Clone, Copy)]
enum Env {
    Amp,
    Filter,
    Mod,
}

fn envelope_module(env: Env, p: Params, on_set: &SynthParamsCb) -> AnyElement {
    let (title, shape, id) = match env {
        Env::Amp => ("AMP ENVELOPE", EnvShape::amp(&p), "amp"),
        Env::Filter => ("FILTER ENVELOPE", EnvShape::filter(&p), "filter"),
        Env::Mod => ("MOD ENVELOPE", EnvShape::modulation(&p), "mod"),
    };
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
    let defaults = EnvShape::amp(&wrapsynth::default_params());
    module(
        title,
        Some(envelope_graph(shape)),
        controls_row()
            .child(knob(
                format!("wrap-synth-{id}-attack"),
                "Attack",
                format_ms(shape.attack_ms),
                p,
                (
                    time_position(shape.attack_ms, ATTACK_RANGE),
                    time_position(defaults.attack_ms, ATTACK_RANGE),
                ),
                on_set,
                set_attack,
            ))
            .child(knob(
                format!("wrap-synth-{id}-decay"),
                "Decay",
                format_ms(shape.decay_ms),
                p,
                (
                    time_position(shape.decay_ms, decay_range),
                    time_position(defaults.decay_ms, decay_range),
                ),
                on_set,
                set_decay,
            ))
            .child(knob(
                format!("wrap-synth-{id}-sustain"),
                "Sustain",
                percent(shape.sustain),
                p,
                (shape.sustain, defaults.sustain),
                on_set,
                set_sustain,
            ))
            .child(knob(
                format!("wrap-synth-{id}-release"),
                "Release",
                format_ms(shape.release_ms),
                p,
                (
                    time_position(shape.release_ms, release_range),
                    time_position(defaults.release_ms, release_range),
                ),
                on_set,
                set_release,
            )),
    )
}

fn amp_module(p: Params, on_set: &SynthParamsCb) -> AnyElement {
    envelope_module(Env::Amp, p, on_set)
}

// ── Output ─────────────────────────────────────────────────────────────────

fn output_module(p: Params, on_set: &SynthParamsCb) -> AnyElement {
    module(
        "OUTPUT",
        None,
        controls_row()
            .child(knob(
                "wrap-synth-sub",
                "Sub",
                percent(p.sub_level),
                p,
                (p.sub_level, 0.0),
                on_set,
                |p, v| p.sub_level = v,
            ))
            .child(knob(
                "wrap-synth-noise",
                "Noise",
                percent(p.noise_level),
                p,
                (p.noise_level, 0.0),
                on_set,
                |p, v| p.noise_level = v,
            ))
            .child(knob(
                "wrap-synth-width",
                "Width",
                percent(p.stereo_width),
                p,
                (p.stereo_width, 0.75),
                on_set,
                |p, v| p.stereo_width = v,
            ))
            .child(knob(
                "wrap-synth-master",
                "Master",
                format!("{:+.1} dB", p.master_db),
                p,
                (master_position(p.master_db), master_position(-6.0)),
                on_set,
                |p, v| p.master_db = master_from_position(v),
            )),
    )
}

// ── Modulation page ────────────────────────────────────────────────────────

fn modulation_page(p: Params, set: &SynthParamsCb) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(space::BASE))
        .child(row([
            envelope_module(Env::Filter, p, set),
            envelope_module(Env::Mod, p, set),
        ]))
        .child(row([lfo_module(0, p, set), lfo_module(1, p, set)]))
        .child(matrix_module(p, set))
        .into_any_element()
}

fn lfo_graph(shape: LfoShape) -> AnyElement {
    let colors = GraphColors::resolve(true);
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            paint_graph_frame(window, bounds, &colors);
            let x0 = f32::from(bounds.origin.x);
            let y0 = f32::from(bounds.origin.y);
            let w = f32::from(bounds.size.width).max(1.0);
            let h = f32::from(bounds.size.height).max(1.0);
            let mid = y0 + h * 0.5;
            let half = h * 0.5 - 6.0;
            window.paint_quad(fill(rect(x0, mid, w, 1.0), colors.guide));
            // Two cycles; a sample-and-hold drawn as a few steady steps.
            let held = [0.6, -0.3, 0.9, -0.8];
            let columns = (w as usize).clamp(2, 512);
            let curve: Vec<(f32, f32)> = (0..columns)
                .map(|i| {
                    let t = i as f32 / (columns - 1) as f32 * 2.0;
                    let cycle = (t as usize).min(1);
                    let phase = (t - cycle as f32).min(0.9999);
                    let step = held[(cycle * 2 + usize::from(phase >= 0.5)) % held.len()];
                    let value = shape.at(phase, step);
                    (x0 + t * 0.5 * w, mid - value * half)
                })
                .collect();
            paint_curve(window, &curve, mid, &colors);
        },
    )
    .w_full()
    .h(px(GRAPH_H))
    .into_any_element()
}

fn lfo_module(which: usize, p: Params, on_set: &SynthParamsCb) -> AnyElement {
    let (title, shape, rate, sync, division, retrigger) = if which == 0 {
        (
            "LFO 1",
            p.lfo1_shape,
            p.lfo1_rate_hz,
            p.lfo1_sync,
            p.lfo1_division,
            p.lfo1_retrigger,
        )
    } else {
        (
            "LFO 2",
            p.lfo2_shape,
            p.lfo2_rate_hz,
            p.lfo2_sync,
            p.lfo2_division,
            p.lfo2_retrigger,
        )
    };
    let edit = move |p: Params, apply: &dyn Fn(&mut LfoFields)| -> Params {
        with(p, |p| {
            let mut fields = LfoFields::of(p, which);
            apply(&mut fields);
            fields.store(p, which);
        })
    };
    let shapes: Vec<(LfoShape, &'static str)> = LfoShape::ALL
        .iter()
        .map(|shape| (*shape, shape.label()))
        .collect();
    let rate_control = if sync {
        let max = echospace::MAX_DIVISION_WIRE as u8;
        choice_stepper(
            format!("wrap-synth-lfo{which}-division"),
            "Rate",
            division_label(division),
            (
                edit(p, &|f| f.division = f.division.saturating_sub(1)),
                edit(p, &|f| f.division = (f.division + 1).min(max)),
            ),
            on_set,
            44.0,
        )
    } else {
        knob(
            format!("wrap-synth-lfo{which}-rate"),
            "Rate",
            format_rate(rate),
            p,
            (
                rate_position(rate, LFO_RANGE),
                rate_position(2.0, LFO_RANGE),
            ),
            on_set,
            move |p, v| {
                let mut fields = LfoFields::of(p, which);
                fields.rate_hz = rate_from_position(v, LFO_RANGE);
                fields.store(p, which);
            },
        )
    };
    module(
        title,
        Some(lfo_graph(shape)),
        div()
            .flex()
            .flex_col()
            .gap(px(space::BASE))
            .child(segments(
                if which == 0 {
                    "wrap-synth-lfo0-shape"
                } else {
                    "wrap-synth-lfo1-shape"
                },
                &shapes,
                shape,
                |shape| edit(p, &|f| f.shape = shape),
                on_set,
            ))
            .child(
                controls_row()
                    .items_center()
                    .child(rate_control)
                    .child(toggle(
                        if which == 0 {
                            "wrap-synth-lfo0-sync"
                        } else {
                            "wrap-synth-lfo1-sync"
                        },
                        "Sync",
                        sync,
                        edit(p, &|f| f.sync = !f.sync),
                        on_set,
                    ))
                    .child(toggle(
                        if which == 0 {
                            "wrap-synth-lfo0-retrigger"
                        } else {
                            "wrap-synth-lfo1-retrigger"
                        },
                        "Retrigger",
                        retrigger,
                        edit(p, &|f| f.retrigger = !f.retrigger),
                        on_set,
                    )),
            ),
    )
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

fn cycle<T: Copy + PartialEq>(all: &[T], current: T, delta: isize) -> T {
    let at = all.iter().position(|item| *item == current).unwrap_or(0) as isize;
    all[(at + delta).rem_euclid(all.len() as isize) as usize]
}

fn matrix_module(p: Params, on_set: &SynthParamsCb) -> AnyElement {
    // Two columns of four slots: a slot is a short row, and the card is
    // as wide as the window.
    let mut rows = div()
        .flex()
        .flex_row()
        .flex_wrap()
        .gap_x(px(space::SECTION))
        .gap_y(px(space::TIGHT));
    for slot in 0..MOD_SLOTS {
        let current = p.mod_slots[slot];
        let active = current.is_active();
        let source = |delta: isize| {
            with(p, |p| {
                p.mod_slots[slot].source = cycle(&ModSource::ALL, current.source, delta)
            })
        };
        let dest = |delta: isize| {
            with(p, |p| {
                p.mod_slots[slot].dest = cycle(&ModDest::ALL, current.dest, delta)
            })
        };
        rows = rows.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .flex_1()
                .min_w(px(520.0))
                .gap(px(space::LOOSE))
                .child(
                    div()
                        .w(px(18.0))
                        .text_size(px(typography::UI_SM))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(if active {
                            Colors::accent_primary()
                        } else {
                            Colors::text_muted()
                        })
                        .child(format!("{}", slot + 1)),
                )
                .child(choice_stepper(
                    format!("wrap-synth-mod{slot}-source"),
                    "Source",
                    current.source.label().to_string(),
                    (source(-1), source(1)),
                    on_set,
                    84.0,
                ))
                .child(choice_stepper(
                    format!("wrap-synth-mod{slot}-dest"),
                    "Destination",
                    current.dest.label().to_string(),
                    (dest(-1), dest(1)),
                    on_set,
                    100.0,
                ))
                .child(signed_knob(
                    format!("wrap-synth-mod{slot}-amount"),
                    "Amount",
                    format!("{:+.0}%", current.amount * 100.0),
                    p,
                    (current.amount, -1.0, 1.0, 0.0),
                    on_set,
                    move |p, v| p.mod_slots[slot].amount = (v * 100.0).round() / 100.0,
                )),
        );
    }
    module("MODULATION MATRIX", None, rows)
}

// ── Voice & FX page ────────────────────────────────────────────────────────

fn voice_fx_page(p: Params, set: &SynthParamsCb) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(space::BASE))
        .child(row([voice_module(p, set)]))
        .child(row([drive_module(p, set), chorus_module(p, set)]))
        .child(row([delay_module(p, set), reverb_module(p, set)]))
        .into_any_element()
}

fn voice_module(p: Params, on_set: &SynthParamsCb) -> AnyElement {
    let modes: Vec<(VoiceMode, &'static str)> = VoiceMode::ALL
        .iter()
        .map(|mode| (*mode, mode.label()))
        .collect();
    wide(module(
        "VOICE",
        None,
        controls_row()
            .items_center()
            .child(segments(
                "wrap-synth-voice-mode",
                &modes,
                p.voice_mode,
                |mode| with(p, |p| p.voice_mode = mode),
                on_set,
            ))
            .child(knob(
                "wrap-synth-glide",
                "Glide",
                format_ms(p.glide_ms),
                p,
                (time_position(p.glide_ms, GLIDE_RANGE), 0.0),
                on_set,
                |p, v| p.glide_ms = time_from_position(v, GLIDE_RANGE).round(),
            ))
            .child(stepper(
                "wrap-synth-bend-range",
                "Bend",
                format!("±{}", p.pitch_bend_range.round() as i32),
                (
                    with(p, |p| {
                        p.pitch_bend_range = (p.pitch_bend_range - 1.0).max(0.0)
                    }),
                    with(p, |p| p.pitch_bend_range += 1.0),
                ),
                on_set,
            ))
            .child(knob(
                "wrap-synth-velocity",
                "Velocity",
                percent(p.velocity_amount),
                p,
                (p.velocity_amount, 0.7),
                on_set,
                |p, v| p.velocity_amount = v,
            ))
            .child(knob(
                "wrap-synth-analog",
                "Analog",
                percent(p.analog),
                p,
                (p.analog, 0.12),
                on_set,
                |p, v| p.analog = v,
            ))
            .child(knob(
                "wrap-synth-phase-random",
                "Phase Rand",
                percent(p.phase_random),
                p,
                (p.phase_random, 1.0),
                on_set,
                |p, v| p.phase_random = v,
            )),
    ))
}

fn drive_module(p: Params, on_set: &SynthParamsCb) -> AnyElement {
    module(
        "DRIVE",
        None,
        controls_row()
            .items_center()
            .child(toggle(
                "wrap-synth-dist-on",
                "On",
                p.dist_on,
                with(p, |p| p.dist_on = !p.dist_on),
                on_set,
            ))
            .child(knob(
                "wrap-synth-dist-drive",
                "Drive",
                percent(p.dist_drive),
                p,
                (p.dist_drive, 0.3),
                on_set,
                |p, v| p.dist_drive = v,
            ))
            .child(knob(
                "wrap-synth-dist-mix",
                "Mix",
                percent(p.dist_mix),
                p,
                (p.dist_mix, 1.0),
                on_set,
                |p, v| p.dist_mix = v,
            )),
    )
}

fn chorus_module(p: Params, on_set: &SynthParamsCb) -> AnyElement {
    module(
        "CHORUS",
        None,
        controls_row()
            .items_center()
            .child(toggle(
                "wrap-synth-chorus-on",
                "On",
                p.chorus_on,
                with(p, |p| p.chorus_on = !p.chorus_on),
                on_set,
            ))
            .child(knob(
                "wrap-synth-chorus-rate",
                "Rate",
                format_rate(p.chorus_rate_hz),
                p,
                (
                    rate_position(p.chorus_rate_hz, CHORUS_RATE_RANGE),
                    rate_position(0.6, CHORUS_RATE_RANGE),
                ),
                on_set,
                |p, v| p.chorus_rate_hz = rate_from_position(v, CHORUS_RATE_RANGE),
            ))
            .child(knob(
                "wrap-synth-chorus-depth",
                "Depth",
                percent(p.chorus_depth),
                p,
                (p.chorus_depth, 0.5),
                on_set,
                |p, v| p.chorus_depth = v,
            ))
            .child(knob(
                "wrap-synth-chorus-mix",
                "Mix",
                percent(p.chorus_mix),
                p,
                (p.chorus_mix, 0.4),
                on_set,
                |p, v| p.chorus_mix = v,
            )),
    )
}

fn delay_module(p: Params, on_set: &SynthParamsCb) -> AnyElement {
    let max = echospace::MAX_DIVISION_WIRE as u8;
    let time = if p.delay_sync {
        choice_stepper(
            "wrap-synth-delay-division",
            "Time",
            division_label(p.delay_division),
            (
                with(p, |p| p.delay_division = p.delay_division.saturating_sub(1)),
                with(p, |p| p.delay_division = (p.delay_division + 1).min(max)),
            ),
            on_set,
            44.0,
        )
    } else {
        knob(
            "wrap-synth-delay-time",
            "Time",
            format_ms(p.delay_time_ms),
            p,
            (
                time_position(p.delay_time_ms, DELAY_TIME_RANGE),
                time_position(320.0, DELAY_TIME_RANGE),
            ),
            on_set,
            |p, v| p.delay_time_ms = time_from_position(v, DELAY_TIME_RANGE).round(),
        )
    };
    module(
        "DELAY",
        None,
        controls_row()
            .items_center()
            .child(toggle(
                "wrap-synth-delay-on",
                "On",
                p.delay_on,
                with(p, |p| p.delay_on = !p.delay_on),
                on_set,
            ))
            .child(toggle(
                "wrap-synth-delay-sync",
                "Sync",
                p.delay_sync,
                with(p, |p| p.delay_sync = !p.delay_sync),
                on_set,
            ))
            .child(time)
            .child(knob(
                "wrap-synth-delay-feedback",
                "Feedback",
                percent(p.delay_feedback),
                p,
                (p.delay_feedback / 0.95, 0.35 / 0.95),
                on_set,
                |p, v| p.delay_feedback = v * 0.95,
            ))
            .child(knob(
                "wrap-synth-delay-mix",
                "Mix",
                percent(p.delay_mix),
                p,
                (p.delay_mix, 0.25),
                on_set,
                |p, v| p.delay_mix = v,
            ))
            .child(toggle(
                "wrap-synth-delay-ping-pong",
                "Ping-Pong",
                p.delay_ping_pong,
                with(p, |p| p.delay_ping_pong = !p.delay_ping_pong),
                on_set,
            )),
    )
}

fn reverb_module(p: Params, on_set: &SynthParamsCb) -> AnyElement {
    module(
        "REVERB",
        None,
        controls_row()
            .items_center()
            .child(toggle(
                "wrap-synth-reverb-on",
                "On",
                p.reverb_on,
                with(p, |p| p.reverb_on = !p.reverb_on),
                on_set,
            ))
            .child(knob(
                "wrap-synth-reverb-size",
                "Size",
                percent(p.reverb_size),
                p,
                (p.reverb_size, 0.6),
                on_set,
                |p, v| p.reverb_size = v,
            ))
            .child(knob(
                "wrap-synth-reverb-decay",
                "Decay",
                format!("{:.1} s", p.reverb_decay_sec),
                p,
                (
                    time_position(p.reverb_decay_sec, REVERB_DECAY_RANGE),
                    time_position(2.4, REVERB_DECAY_RANGE),
                ),
                on_set,
                |p, v| {
                    p.reverb_decay_sec =
                        (time_from_position(v, REVERB_DECAY_RANGE) * 10.0).round() / 10.0
                },
            ))
            .child(knob(
                "wrap-synth-reverb-damping",
                "Damping",
                percent(p.reverb_damping),
                p,
                (p.reverb_damping, 0.45),
                on_set,
                |p, v| p.reverb_damping = v,
            ))
            .child(knob(
                "wrap-synth-reverb-mix",
                "Mix",
                percent(p.reverb_mix),
                p,
                (p.reverb_mix, 0.22),
                on_set,
                |p, v| p.reverb_mix = v,
            )),
    )
}

// ── Keyboard ───────────────────────────────────────────────────────────────

fn keyboard_footer(panel: &WrapSynthPanelState, cb: &WrapSynthCallbacks) -> AnyElement {
    let board = piano_keyboard(
        "wrap-synth",
        panel.keyboard_root,
        &panel.active_notes,
        panel.params.power,
        &|_| None,
        cb.on_note_on.clone(),
        cb.on_note_off.clone(),
    );
    sampler_keyboard_footer(
        "wrap-synth-octave",
        panel.keyboard_root,
        &panel.active_notes,
        if panel.params.power {
            "Hold a key to hear the patch; the track's MIDI plays it too".to_string()
        } else {
            "Switch the synth on to play it".to_string()
        },
        cb.on_shift_octave.clone(),
        board,
    )
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
        let env = EnvShape::amp(&wrapsynth::default_params());
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
        assert_eq!(
            panel.keyboard_root,
            crate::components::quick_sampler_panel::KEYBOARD_HIGHEST_ROOT
        );
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
    fn cycling_wraps_both_ways() {
        assert_eq!(
            cycle(&ModSource::ALL, ModSource::None, -1),
            ModSource::Random
        );
        assert_eq!(cycle(&ModDest::ALL, ModDest::Pan, 1), ModDest::None);
    }
}

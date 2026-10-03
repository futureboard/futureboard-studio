//! The built-in WrapSynth's panel.
//!
//! Laid out along the signal path, left to right and top to bottom:
//!
//! * two **oscillators** — each drawn as the very cycle the DSP plays
//!   ([`wrapsynth::wavetable`]), with its shape, wavetable position and
//!   level; A carries the unison stack, B its tuning against A;
//! * the **filter** — the low-pass's response curve, cutoff, resonance and
//!   the drive in front of it;
//! * the **amp envelope**, drawn as its shape;
//! * the **output** — sub, noise, stereo width and master level;
//! * a keyboard across the full width that plays through the track.
//!
//! This file only renders: the state is the insert's params, held by
//! [`crate::components::wrap_synth_window::WrapSynthEditorWindow`]. The
//! module cards, graphs and keyboard are the samplers' own, so the built-in
//! instruments read as one family.

use std::sync::Arc;

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
use wrapsynth::{MAX_UNISON, MAX_VOICES, Params, Waveform};

/// Four octaves from C2.
const KEYBOARD_DEFAULT_ROOT: u8 = 36;
/// The oscillator drawings: taller than the samplers' graphs, the shape is
/// the point of them.
const WAVE_H: f32 = 76.0;
const CUTOFF_MIN_HZ: f32 = 40.0;
const CUTOFF_MAX_HZ: f32 = 20_000.0;

/// What the panel draws: the insert's params and the keyboard's state.
#[derive(Clone)]
pub struct WrapSynthPanelState {
    pub params: Params,
    pub keyboard_root: u8,
    pub active_notes: Vec<u8>,
}

impl Default for WrapSynthPanelState {
    fn default() -> Self {
        Self {
            params: wrapsynth::default_params(),
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

#[derive(Clone)]
pub struct WrapSynthCallbacks {
    /// One complete params value, so a knob drag cannot land a partial edit.
    pub on_set_params: SynthParamsCb,
    pub on_note_on: NoteCb,
    pub on_note_off: NoteCb,
    pub on_shift_octave: I32Cb,
}

/// The whole panel.
pub fn wrap_synth_panel(panel: &WrapSynthPanelState, cb: WrapSynthCallbacks) -> AnyElement {
    let p = panel.params;
    let set = &cb.on_set_params;
    // Off, the synth is silent: everything stays editable, but reads as
    // asleep.
    let dim = if p.power { 1.0 } else { 0.48 };
    div()
        .flex()
        .flex_col()
        .size_full()
        .bg(Colors::surface_window())
        .child(header(p, set))
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
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap(px(space::BASE))
                        .child(oscillator_a(p, set))
                        .child(oscillator_b(p, set)),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap(px(space::BASE))
                        .child(filter_module(p, set))
                        .child(amp_module(p, set))
                        .child(output_module(p, set)),
                ),
        )
        .child(keyboard_footer(panel, &cb))
        .into_any_element()
}

fn header(p: Params, on_set: &SynthParamsCb) -> AnyElement {
    let on_power = on_set.clone();
    let subtitle = if p.power {
        format!(
            "Wavetable instrument · {MAX_VOICES} voices · 2 oscillators · up to {MAX_UNISON}× unison"
        )
    } else {
        "Off — the synth is silent until it is switched on".to_string()
    };
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
        .child(fb_checkbox(
            "wrap-synth-power",
            "Power",
            p.power,
            true,
            move |_, w, cx| on_power(&with(p, |p| p.power = !p.power), w, cx),
        ))
        .into_any_element()
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
    id: &'static str,
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
    id: &'static str,
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
                .child(fb_stepper_button((id, 0usize), "−", move |_, w, cx| {
                    on_dec(&dec, w, cx)
                }))
                .child(
                    div()
                        .w(px(36.0))
                        .flex()
                        .justify_center()
                        .text_size(px(typography::UI_SM))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(Colors::text_primary())
                        .child(readout),
                )
                .child(fb_stepper_button((id, 1usize), "+", move |_, w, cx| {
                    on_inc(&inc, w, cx)
                })),
        )
        .child(
            div()
                .text_size(px(typography::DENSE_CAPTION))
                .text_color(Colors::text_muted())
                .child(caption),
        )
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

pub(crate) fn time_position(ms: f32, (min, max): (f32, f32)) -> f32 {
    ((ms.clamp(min, max) - min) / (max - min)).sqrt()
}

const ATTACK_RANGE: (f32, f32) = (0.5, 5_000.0);
const DECAY_RANGE: (f32, f32) = (1.0, 5_000.0);
const RELEASE_RANGE: (f32, f32) = (5.0, 8_000.0);
const MASTER_RANGE: (f32, f32) = (-24.0, 3.0);

fn master_position(db: f32) -> f32 {
    (db - MASTER_RANGE.0) / (MASTER_RANGE.1 - MASTER_RANGE.0)
}

fn master_from_position(position: f32) -> f32 {
    // Tenths of a dB: a readout that does not flicker through hundredths.
    let db = MASTER_RANGE.0 + position * (MASTER_RANGE.1 - MASTER_RANGE.0);
    (db * 10.0).round() / 10.0
}

// ── Oscillators ────────────────────────────────────────────────────────────

fn wave_label(wave: Waveform) -> &'static str {
    match wave {
        Waveform::Saw => "Saw",
        Waveform::Square => "Square",
        Waveform::Triangle => "Triangle",
        Waveform::Sine => "Sine",
    }
}

/// One cycle of the oscillator, through the DSP's own wavetable.
fn wave_graph(wave: Waveform, position: f32, level: f32) -> AnyElement {
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
                    let sample = wrapsynth::wavetable(phase.min(0.9999), wave, position);
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

fn wave_tabs(
    id: &'static str,
    current: Waveform,
    set: impl Fn(Waveform) -> Params,
    on_set: &SynthParamsCb,
) -> AnyElement {
    let mut track = fb_segmented_track();
    let last = Waveform::ALL.len() - 1;
    for (index, wave) in Waveform::ALL.into_iter().enumerate() {
        let on_change = on_set.clone();
        let next = set(wave);
        track = track.child(fb_segment(
            (id, index),
            wave_label(wave),
            current == wave,
            match index {
                0 => FbSegment::First,
                i if i == last => FbSegment::Last,
                _ => FbSegment::Middle,
            },
            move |_, w, cx| on_change(&next, w, cx),
        ));
    }
    track.w_full().into_any_element()
}

/// A card that grows to share its row, like [`module`], but wider: two
/// oscillators fill a row on their own.
fn wide(card: AnyElement) -> AnyElement {
    div()
        .flex()
        .flex_1()
        .min_w(px(300.0))
        .child(card)
        .into_any_element()
}

fn oscillator_a(p: Params, on_set: &SynthParamsCb) -> AnyElement {
    wide(module(
        "OSCILLATOR A",
        Some(wave_graph(p.osc_a_wave, p.osc_a_position, p.osc_a_level)),
        div()
            .flex()
            .flex_col()
            .gap(px(space::BASE))
            .child(wave_tabs(
                "wrap-synth-osc-a-wave",
                p.osc_a_wave,
                |wave| with(p, |p| p.osc_a_wave = wave),
                on_set,
            ))
            .child(
                controls_row()
                    .items_center()
                    .child(knob(
                        "wrap-synth-osc-a-position",
                        "Position",
                        percent(p.osc_a_position),
                        p,
                        (p.osc_a_position, 0.18),
                        on_set,
                        |p, v| p.osc_a_position = v,
                    ))
                    .child(knob(
                        "wrap-synth-osc-a-level",
                        "Level",
                        percent(p.osc_a_level),
                        p,
                        (p.osc_a_level, 0.78),
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
                        (p.unison_detune_cents / 50.0, 14.0 / 50.0),
                        on_set,
                        |p, v| p.unison_detune_cents = (v * 50.0).round(),
                    )),
            ),
    ))
}

fn oscillator_b(p: Params, on_set: &SynthParamsCb) -> AnyElement {
    wide(module(
        "OSCILLATOR B",
        Some(wave_graph(p.osc_b_wave, p.osc_b_position, p.osc_b_level)),
        div()
            .flex()
            .flex_col()
            .gap(px(space::BASE))
            .child(wave_tabs(
                "wrap-synth-osc-b-wave",
                p.osc_b_wave,
                |wave| with(p, |p| p.osc_b_wave = wave),
                on_set,
            ))
            .child(
                controls_row()
                    .items_center()
                    .child(knob(
                        "wrap-synth-osc-b-position",
                        "Position",
                        percent(p.osc_b_position),
                        p,
                        (p.osc_b_position, 0.42),
                        on_set,
                        |p, v| p.osc_b_position = v,
                    ))
                    .child(knob(
                        "wrap-synth-osc-b-level",
                        "Level",
                        percent(p.osc_b_level),
                        p,
                        (p.osc_b_level, 0.38),
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
                        (p.osc_b_detune_cents, -50.0, 50.0, 7.0),
                        on_set,
                        |p, v| p.osc_b_detune_cents = v.round(),
                    )),
            ),
    ))
}

// ── Filter ─────────────────────────────────────────────────────────────────

/// Magnitude in dB of the synth's low-pass at `hz`: the analog prototype of
/// its state-variable filter, with the DSP's own damping
/// (`2 - 1.9 · resonance`, floored at 0.08).
pub(crate) fn filter_response_db(cutoff: f32, resonance: f32, hz: f32) -> f32 {
    let k = (2.0 - 1.9 * resonance).max(0.08);
    let x = hz / cutoff.max(1.0);
    let denominator = ((1.0 - x * x).powi(2) + (k * x).powi(2)).sqrt().max(1.0e-9);
    20.0 * (1.0 / denominator).max(1.0e-6).log10()
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
                        y_for(filter_response_db(p.cutoff_hz, p.resonance, hz)),
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
    module(
        "FILTER · LOW-PASS",
        Some(filter_graph(p)),
        controls_row()
            .justify_around()
            .child(knob(
                "wrap-synth-cutoff",
                "Cutoff",
                format_hz(p.cutoff_hz),
                p,
                (cutoff_position(p.cutoff_hz), cutoff_position(6_400.0)),
                on_set,
                |p, v| p.cutoff_hz = cutoff_from_position(v),
            ))
            .child(knob(
                "wrap-synth-resonance",
                "Resonance",
                percent(p.resonance),
                p,
                (p.resonance / 0.95, 0.18 / 0.95),
                on_set,
                |p, v| p.resonance = v * 0.95,
            ))
            .child(knob(
                "wrap-synth-drive",
                "Drive",
                percent(p.filter_drive),
                p,
                (p.filter_drive, 0.12),
                on_set,
                |p, v| p.filter_drive = v,
            )),
    )
}

// ── Amp envelope ───────────────────────────────────────────────────────────

/// The envelope's corner points across `width`: attack, decay and release
/// each take a share of the width that grows with their time (square-root,
/// so a 10 ms attack shows beside an 8 s release), and the sustain holds a
/// fixed stretch so it is always visible.
pub(crate) fn envelope_points(p: &Params, width: f32, height: f32) -> [(f32, f32); 5] {
    let span = |ms: f32, max: f32| (ms.max(0.0) / max).sqrt().min(1.0);
    let sustain_w = 0.22;
    let (a, d, r) = (
        span(p.attack_ms, ATTACK_RANGE.1),
        span(p.decay_ms, DECAY_RANGE.1),
        span(p.release_ms, RELEASE_RANGE.1),
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
        (xd * width, level(p.sustain)),
        (xs * width, level(p.sustain)),
        (xr * width, height),
    ]
}

fn amp_graph(p: Params) -> AnyElement {
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
            let points = envelope_points(&p, w - inset * 2.0, h - inset * 2.0);
            let curve: Vec<(f32, f32)> = points
                .iter()
                .map(|&(x, y)| (x0 + inset + x, y0 + inset + y))
                .collect();
            paint_curve(window, &curve, y0 + h, &colors);
            for &(x, y) in &curve[1..4] {
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

fn amp_module(p: Params, on_set: &SynthParamsCb) -> AnyElement {
    module(
        "AMP ENVELOPE",
        Some(amp_graph(p)),
        controls_row()
            .child(knob(
                "wrap-synth-attack",
                "Attack",
                format_ms(p.attack_ms),
                p,
                (
                    time_position(p.attack_ms, ATTACK_RANGE),
                    time_position(8.0, ATTACK_RANGE),
                ),
                on_set,
                |p, v| p.attack_ms = time_from_position(v, ATTACK_RANGE),
            ))
            .child(knob(
                "wrap-synth-decay",
                "Decay",
                format_ms(p.decay_ms),
                p,
                (
                    time_position(p.decay_ms, DECAY_RANGE),
                    time_position(220.0, DECAY_RANGE),
                ),
                on_set,
                |p, v| p.decay_ms = time_from_position(v, DECAY_RANGE),
            ))
            .child(knob(
                "wrap-synth-sustain",
                "Sustain",
                percent(p.sustain),
                p,
                (p.sustain, 0.72),
                on_set,
                |p, v| p.sustain = v,
            ))
            .child(knob(
                "wrap-synth-release",
                "Release",
                format_ms(p.release_ms),
                p,
                (
                    time_position(p.release_ms, RELEASE_RANGE),
                    time_position(420.0, RELEASE_RANGE),
                ),
                on_set,
                |p, v| p.release_ms = time_from_position(v, RELEASE_RANGE),
            )),
    )
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
                (p.sub_level, 0.16),
                on_set,
                |p, v| p.sub_level = v,
            ))
            .child(knob(
                "wrap-synth-noise",
                "Noise",
                percent(p.noise_level),
                p,
                (p.noise_level, 0.025),
                on_set,
                |p, v| p.noise_level = v,
            ))
            .child(knob(
                "wrap-synth-width",
                "Width",
                percent(p.stereo_width),
                p,
                (p.stereo_width, 0.72),
                on_set,
                |p, v| p.stereo_width = v,
            ))
            .child(knob(
                "wrap-synth-master",
                "Master",
                format!("{:+.1} dB", p.master_db),
                p,
                (master_position(p.master_db), master_position(-8.0)),
                on_set,
                |p, v| p.master_db = master_from_position(v),
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
        for hz in [40.0, 220.0, 6_400.0, 20_000.0] {
            let back = cutoff_from_position(cutoff_position(hz));
            assert!((back - hz).abs() / hz < 1.0e-3, "{hz} → {back}");
        }
        for ms in [0.5, 8.0, 420.0, 5_000.0] {
            let back = time_from_position(time_position(ms, ATTACK_RANGE), ATTACK_RANGE);
            assert!((back - ms).abs() < 1.0e-2, "{ms} → {back}");
        }
        assert_eq!(master_from_position(master_position(-8.0)), -8.0);
        // The knob's ends are the param's ends.
        assert_eq!(time_from_position(1.0, RELEASE_RANGE), RELEASE_RANGE.1);
        assert_eq!(master_from_position(0.0), MASTER_RANGE.0);
    }

    #[test]
    fn the_filter_curve_passes_below_and_cuts_above() {
        assert!(filter_response_db(1_000.0, 0.0, 50.0).abs() < 0.5);
        assert!(filter_response_db(1_000.0, 0.0, 10_000.0) < -30.0);
        // Resonance peaks at the cutoff.
        assert!(filter_response_db(1_000.0, 0.9, 1_000.0) > 6.0);
    }

    #[test]
    fn the_envelope_starts_and_ends_silent_and_holds_its_sustain() {
        let p = wrapsynth::default_params();
        let points = envelope_points(&p, 200.0, 100.0);
        assert_eq!(points[0], (0.0, 100.0));
        assert_eq!(points[4].1, 100.0);
        assert_eq!(points[1].1, 0.0);
        assert!((points[2].1 - 100.0 * (1.0 - p.sustain)).abs() < 1.0e-3);
        assert_eq!(points[2].1, points[3].1);
        assert!(points.windows(2).all(|pair| pair[0].0 <= pair[1].0));
        assert!(points[4].0 <= 200.0);
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
}

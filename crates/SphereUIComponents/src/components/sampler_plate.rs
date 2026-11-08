//! What Quick Sampler's and Slicer's plate editors share: the plate around
//! them with its drop target, the well's header and sample display box, the
//! strip under the display, and the voice cards on the deck — Filter (with
//! its response curve), Amp envelope (with its shape) and Output — that a
//! Quick Sampler and every slice of a Slicer play through.
//!
//! Both instruments keep their state and wire path in their windows; the
//! render here only reads params and hands each edit, whole, to a callback.
//! Sizes are at the plate's design size and scaled by `s`; the sample
//! display paints in window pixels, as the windows' hit-testing expects.

use std::rc::Rc;

use gpui::{
    canvas, div, prelude::FluentBuilder, px, AnyElement, App, FontWeight, InteractiveElement,
    IntoElement, ParentElement, Rgba, SharedString, Styled, Window,
};
use quicksampler::{FilterMode, QuickSamplerParams, CUTOFF_MAX_HZ, CUTOFF_MIN_HZ};

use crate::components::instrument_plate::{self, paint_curve, paint_grid, Click, Step, WELL_TOP};
use crate::components::knob::format_pan_label;
use crate::components::plate::{
    self, caption, card, card_title, color, color_a, icon, numbers, square, words, Dial, DialUnit,
    KnobCell, KnobLook, KnobSet, PillLook, FAINT, GHOST, ICON, PALE, SOFT, WHITE,
};
use crate::components::plate_parts::{knob_row, note};
use crate::components::quick_sampler_panel::{
    cutoff_position, envelope_points, filter_response_db, format_hz, format_ms, format_seconds,
    percent, ruler_labels, with, DisplayFrame, SampleSummary, ENVELOPE_KNOB_MAX_MS, FLAG_H,
    RULER_H, SAMPLE_DROP_GROUP,
};
use crate::components::time_plate::{AXIS_INK, BOX_EDGE, BOX_GROUND};

/// A voice edit: one complete params value.
pub type SetVoice = Rc<dyn Fn(QuickSamplerParams, &mut App)>;

/// The well, from [`WELL_TOP`]: the header, the sample display and the
/// strip under it.
pub const WELL_H: f32 = 300.0;
/// The deck, under the well down to [`instrument_plate::DECK_BOTTOM`].
pub const DECK_TOP: f32 = WELL_TOP + WELL_H + 7.0;
const DECK_PAD: f32 = 12.0;
const DECK_GAP: f32 = 10.0;
/// The deck's cards, left to right; Output takes what is left.
pub const PITCH_W: f32 = 246.0;
pub const FILTER_W: f32 = 236.0;
pub const AMP_W: f32 = 270.0;
/// The strip under the display.
pub const STRIP_H: f32 = 30.0;
/// The well's inset around the display and the strip.
const WELL_PAD: f32 = 12.0;

/// An error line's ink.
const ERROR: u32 = 0xff6b6b;

// ── The plate ──────────────────────────────────────────────────────────────

/// The plate at scale `s` with `children` on it, and over the whole of it a
/// drop target that shows while a file is dragged in (the window's drop
/// handler takes the file wherever it lands).
pub fn plate_root(
    s: f32,
    accent: Rgba,
    drop_message: &'static str,
    children: Vec<AnyElement>,
) -> AnyElement {
    div()
        .relative()
        .size_full()
        .group(SAMPLE_DROP_GROUP)
        .child(plate::plate_frame(s, children))
        .child(drop_overlay(drop_message, accent, s))
        .into_any_element()
}

/// Invisible, and never hit-tested (it has no handlers), until a file from
/// outside is dragged over the plate.
fn drop_overlay(message: &'static str, accent: Rgba, s: f32) -> AnyElement {
    div()
        .absolute()
        .inset_0()
        .p(px(18.0 * s))
        .opacity(0.0)
        .font(plate::outfit(FontWeight::NORMAL))
        .group_drag_over::<gpui::ExternalPaths>(SAMPLE_DROP_GROUP, |style| style.opacity(1.0))
        .child(
            div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(8.0 * s))
                .rounded(px(6.0 * s))
                .border_2()
                .border_dashed()
                .border_color(accent)
                .bg(color_a(plate::GROUND, 0.9))
                .child(
                    words(message, 18.0, WHITE, s)
                        .font_weight(FontWeight::SEMIBOLD)
                        .line_height(px(24.0 * s)),
                )
                .child(caption(
                    "WAV, FLAC, MP3, OGG, AIFF or M4A",
                    10.5,
                    0.12,
                    SOFT,
                    s,
                )),
        )
        .into_any_element()
}

// ── The well ───────────────────────────────────────────────────────────────

/// `name`, shortened in the middle to `max` characters.
fn shortened(name: &str, max: usize) -> String {
    let count = name.chars().count();
    if count <= max {
        return name.to_string();
    }
    let head: String = name.chars().take(max * 2 / 3).collect();
    let tail: String = name.chars().skip(count - (max - max * 2 / 3 - 1)).collect();
    format!("{head}\u{2026}{tail}")
}

/// What the loaded sample is: `44.1 kHz · stereo · 3.69 s`.
pub fn sample_facts(sample: &SampleSummary) -> String {
    format!(
        "{:.1} kHz \u{00B7} {} \u{00B7} {}",
        sample.sample_rate as f32 / 1_000.0,
        if sample.channels == 1 {
            "mono"
        } else {
            "stereo"
        },
        format_seconds(sample.seconds)
    )
}

/// The well's header: the instrument, the sample's name (or that there is
/// none), what the sample is, and the button that loads another.
pub fn header(
    title: &str,
    sample: Option<&SampleSummary>,
    loading: bool,
    accent: Rgba,
    on_browse: Click,
    s: f32,
) -> AnyElement {
    let subtitle = if loading {
        "Loading\u{2026}".to_string()
    } else {
        sample
            .map(|sample| shortened(&sample.file_name, 34))
            .unwrap_or_else(|| "No sample".to_string())
    };
    let facts = sample
        .filter(|_| !loading)
        .map(|sample| numbers(sample_facts(sample), 11.0, FAINT, s));
    let browse = plate::pill(
        "sampler-browse",
        "Load Sample\u{2026}",
        false,
        accent,
        PillLook::words(28.0, 12.0, 12.0),
        !loading,
        s,
        move |_, window, cx| on_browse(window, cx),
    );
    let mut right: Vec<AnyElement> = Vec::new();
    if let Some(facts) = facts {
        right.push(facts.mr(px(4.0 * s)).into_any_element());
    }
    right.push(browse.into_any_element());
    instrument_plate::header(
        "sampler-header",
        s,
        title,
        &subtitle,
        accent,
        vec![],
        right,
        None,
    )
}

/// The well's body under the header: the display, then `rows` under it.
pub fn well_body(display: impl IntoElement, rows: Vec<AnyElement>, s: f32) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h(px(0.0))
        .px(px(WELL_PAD * s))
        .pb(px(10.0 * s))
        .gap(px(8.0 * s))
        .child(display)
        .children(rows)
}

/// The sample display's box: it takes the well's height left over.
pub fn display_box(id: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .relative()
        .w_full()
        .flex_1()
        .min_h(px(0.0))
        .overflow_hidden()
        .bg(color(BOX_GROUND))
        .border(px(1.0))
        .border_color(color(BOX_EDGE))
}

/// What an empty display shows: the way in.
pub fn empty_drop_zone(loading: bool, hint: &'static str, s: f32) -> AnyElement {
    div()
        .absolute()
        .inset_0()
        .p(px(12.0 * s))
        .child(
            div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(6.0 * s))
                .rounded(px(4.0 * s))
                .border_1()
                .border_dashed()
                .border_color(color(0x3a3a3a))
                .child(
                    words(
                        if loading {
                            "Decoding\u{2026}"
                        } else {
                            "Drop an audio file here"
                        },
                        15.0,
                        PALE,
                        s,
                    )
                    .font_weight(FontWeight::MEDIUM),
                )
                .when(!loading, |zone| zone.child(words(hint, 11.5, FAINT, s))),
        )
        .into_any_element()
}

/// An error over the top of the display.
pub fn status_banner(message: String, s: f32) -> AnyElement {
    div()
        .absolute()
        .left(px(8.0 * s))
        .right(px(8.0 * s))
        .bottom(px(RULER_H + 6.0))
        .px(px(10.0 * s))
        .py(px(5.0 * s))
        .rounded(px(3.0 * s))
        .border(px(1.0))
        .border_color(color_a(ERROR, 0.7))
        .bg(color_a(0x2a1414, 0.94))
        .text_size(plate::type_px(11.5, s))
        .line_height(px(15.0 * s))
        .text_color(color(ERROR))
        .child(message)
        .into_any_element()
}

/// The time ruler's labels along the display's foot.
pub(crate) fn paint_ruler_labels(
    window: &mut Window,
    cx: &mut App,
    frame: &DisplayFrame,
    seconds: f64,
) {
    let y = frame.y0 + frame.wave_h + 3.0;
    for (fraction, label) in ruler_labels(seconds) {
        plate::paint_text(
            window,
            cx,
            &label,
            plate::mono(FontWeight::NORMAL),
            9.5,
            color(AXIS_INK),
            (frame.at(fraction) + 3.0, y),
            plate::Align::Left,
        );
    }
}

/// A flag's letter or number, centred on a flag `w` wide at `(x, y)`.
pub fn paint_flag_text(
    window: &mut Window,
    cx: &mut App,
    text: &str,
    (x, y): (f32, f32),
    w: f32,
    ink: Rgba,
) {
    plate::paint_text(
        window,
        cx,
        text,
        plate::mono(FontWeight::BOLD),
        9.0,
        ink,
        (x + w * 0.5, y + (FLAG_H - 11.7) * 0.5),
        plate::Align::Center,
    );
}

/// The strip under the display.
pub fn strip(s: f32) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .flex_shrink_0()
        .h(px(STRIP_H * s))
        .gap(px(10.0 * s))
}

/// A strip's caption.
pub fn strip_caption(text: &str, s: f32) -> gpui::Div {
    caption(text, 10.0, 0.12, SOFT, s)
}

/// `LABEL value`: a reading in the strip.
pub fn reading(label: &str, value: impl Into<SharedString>, ink: u32, s: f32) -> AnyElement {
    div()
        .flex()
        .flex_row()
        .items_baseline()
        .gap(px(5.0 * s))
        .child(caption(label, 9.5, 0.12, ink, s))
        .child(numbers(value, 11.5, WHITE, s))
        .into_any_element()
}

fn step_button(
    id: impl Into<gpui::ElementId>,
    path: &'static str,
    enabled: bool,
    s: f32,
    step: i32,
    on_step: &Step,
) -> gpui::Stateful<gpui::Div> {
    let on_step = on_step.clone();
    square(
        id,
        24.0,
        icon(path, (12.0, 12.0), (12.0, 12.0), 1.5, color(ICON), s),
        enabled,
        s,
        move |_, window, cx| on_step(step, window, cx),
    )
}

/// `− value +` on one line: a setting in the strip.
pub fn inline_stepper(
    id: &'static str,
    value: impl Into<SharedString>,
    width: f32,
    s: f32,
    on_step: Step,
) -> AnyElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(4.0 * s))
        .child(step_button((id, 0usize), "M2 6 H10", true, s, -1, &on_step))
        .child(
            div()
                .w(px(width * s))
                .flex()
                .justify_center()
                .child(numbers(value, 12.5, WHITE, s)),
        )
        .child(step_button((id, 1usize), plate::PLUS, true, s, 1, &on_step))
        .into_any_element()
}

/// A small pill that does one thing.
pub fn action(
    id: &'static str,
    label: &'static str,
    enabled: bool,
    accent: Rgba,
    s: f32,
    click: Click,
) -> AnyElement {
    plate::pill(
        id,
        label,
        false,
        accent,
        PillLook::words(26.0, 10.0, 11.5),
        enabled,
        s,
        move |_, window, cx| click(window, cx),
    )
    .into_any_element()
}

// ── Knobs and steppers ─────────────────────────────────────────────────────

const KNOB: KnobLook = KnobLook::small(44.0);

/// A voice knob: `dial` at `value`, `apply` writing the new value into the
/// params `set` takes.
#[allow(clippy::too_many_arguments)]
fn voice_knob(
    key: &'static str,
    dial: Dial,
    value: f32,
    readout: String,
    width: f32,
    accent: Rgba,
    s: f32,
    p: QuickSamplerParams,
    set: &SetVoice,
    apply: impl Fn(&mut QuickSamplerParams, f32) + 'static,
) -> AnyElement {
    let set = set.clone();
    let on_set: KnobSet = Rc::new(move |value: f32, cx: &mut App| {
        set(with(p, |p| apply(p, value)), cx);
    });
    plate::knob_with(
        key,
        dial,
        value,
        Some(readout),
        KNOB,
        KnobCell::new(width, 9.5, 11.5),
        accent,
        true,
        s,
        on_set,
    )
}

/// A whole-number stepper over the voice params.
pub fn voice_stepper(
    id: &'static str,
    label: &str,
    value: impl Into<SharedString>,
    s: f32,
    p: QuickSamplerParams,
    set: &SetVoice,
    apply: impl Fn(&mut QuickSamplerParams, i32) + 'static,
) -> AnyElement {
    let set = set.clone();
    instrument_plate::stepper(
        id,
        label,
        value,
        true,
        s,
        Rc::new(move |delta: i32, _: &mut Window, cx: &mut App| {
            set(with(p, |p| apply(p, delta)), cx)
        }),
    )
}

/// Semitones added to every note.
pub fn transpose_stepper(p: QuickSamplerParams, set: &SetVoice, s: f32) -> AnyElement {
    voice_stepper(
        "sampler-transpose",
        "Transpose",
        format!("{:+}", p.transpose),
        s,
        p,
        set,
        |p, delta| p.transpose = (p.transpose as i32 + delta).clamp(-128, 127) as i8,
    )
}

/// The pitch wheel's reach, semitones either way.
pub fn bend_stepper(p: QuickSamplerParams, set: &SetVoice, s: f32) -> AnyElement {
    voice_stepper(
        "sampler-bend",
        "Bend range",
        format!("\u{00B1}{}", p.pitch_bend_range),
        s,
        p,
        set,
        |p, delta| p.pitch_bend_range = (p.pitch_bend_range as i32 + delta).clamp(0, 255) as u8,
    )
}

/// Cents added to every note.
pub fn fine_knob(p: QuickSamplerParams, set: &SetVoice, accent: Rgba, s: f32) -> AnyElement {
    voice_knob(
        "sampler-fine",
        Dial::new("fine", "FINE", -100.0, 100.0, 0.0, DialUnit::Plain).bipolar(),
        p.fine_cents,
        format!("{:+.0} ct", p.fine_cents),
        64.0,
        accent,
        s,
        p,
        set,
        |p, value| p.fine_cents = value.round(),
    )
}

/// Two cells on one line, one at each end.
pub fn pair(left: AnyElement, right: AnyElement) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .child(left)
        .child(right)
}

// ── The deck ───────────────────────────────────────────────────────────────

/// The deck: the instrument's own first card (`pitch`), then the voice's
/// Filter, Amp envelope and Output.
pub fn deck(
    pitch: gpui::Div,
    p: QuickSamplerParams,
    set: &SetVoice,
    accent: Rgba,
    s: f32,
) -> AnyElement {
    plate::deck(s, DECK_TOP, instrument_plate::DECK_BOTTOM - DECK_TOP)
        .p(px(DECK_PAD * s))
        .gap(px(DECK_GAP * s))
        .child(pitch.w(px(PITCH_W * s)).flex_shrink_0())
        .child(
            filter_card(p, set, accent, s)
                .w(px(FILTER_W * s))
                .flex_shrink_0(),
        )
        .child(amp_card(p, set, accent, s).w(px(AMP_W * s)).flex_shrink_0())
        .child(output_card(p, set, s).flex_1())
        .into_any_element()
}

/// A card's graph: a box like the well's displays, `height` tall.
fn graph_box(height: f32, s: f32) -> gpui::Div {
    div()
        .relative()
        .w_full()
        .h(px(height * s))
        .flex_shrink_0()
        .bg(color(BOX_GROUND))
        .border(px(1.0))
        .border_color(color(BOX_EDGE))
        .overflow_hidden()
}

const GRAPH_H: f32 = 92.0;

/// The filter's response from 20 Hz to 20 kHz, −36 to +18 dB, the cutoff
/// marked; grey while the filter is off.
fn filter_graph(p: QuickSamplerParams, accent: Rgba, s: f32) -> gpui::Div {
    let active = p.filter != FilterMode::Off;
    let ink = if active { accent } else { color(GHOST) };
    graph_box(GRAPH_H, s).child(
        canvas(
            |_, _, _| (),
            move |bounds, _, window, _| {
                paint_grid(window, bounds, 3, 3);
                let (x0, y0) = plate::xy(bounds.origin);
                let w = f32::from(bounds.size.width).max(1.0);
                let h = f32::from(bounds.size.height).max(1.0);
                let pad = 4.0 * s;
                let y_for =
                    |db: f32| y0 + pad + ((18.0 - db.clamp(-36.0, 18.0)) / 54.0) * (h - pad);
                let columns = (w as usize).max(2);
                let curve: Vec<(f32, f32)> = (0..columns)
                    .map(|i| {
                        let t = i as f32 / (columns - 1) as f32;
                        let hz = 20.0 * 1_000.0_f32.powf(t);
                        (
                            x0 + t * w,
                            y_for(filter_response_db(p.filter, p.cutoff_hz, p.resonance, hz)),
                        )
                    })
                    .collect();
                paint_curve(window, &curve, y0 + h, ink, s);
                if active {
                    let x = x0 + cutoff_position(p.cutoff_hz) * w;
                    plate::paint_polyline(
                        window,
                        &[(x, y0), (x, y0 + h)],
                        1.0,
                        Rgba { a: 0.55, ..accent },
                        Some((3.0 * s, 3.0 * s)),
                    );
                }
            },
        )
        .absolute()
        .size_full(),
    )
}

/// Filter: its type, its response, cutoff and resonance.
pub fn filter_card(p: QuickSamplerParams, set: &SetVoice, accent: Rgba, s: f32) -> gpui::Div {
    let labels: Vec<&str> = FilterMode::ALL.iter().map(|mode| mode.label()).collect();
    let selected = FilterMode::ALL.iter().position(|mode| *mode == p.filter);
    let on_type = {
        let set = set.clone();
        Rc::new(move |index: i32, _: &mut Window, cx: &mut App| {
            if let Some(mode) = FilterMode::ALL.get(index as usize) {
                set(with(p, |p| p.filter = *mode), cx);
            }
        })
    };
    let active = p.filter != FilterMode::Off;
    let width = (FILTER_W - 32.0 - 3.0 * 4.0) / FilterMode::ALL.len() as f32;
    card(s)
        .child(card_title("Filter", s))
        .child(instrument_plate::choices(
            "sampler-filter",
            &labels,
            selected,
            accent,
            Some(width),
            true,
            s,
            on_type,
        ))
        .child(filter_graph(p, accent, s))
        .child(
            knob_row(vec![
                voice_knob(
                    "sampler-cutoff",
                    Dial::new(
                        "cutoff",
                        "CUTOFF",
                        CUTOFF_MIN_HZ,
                        CUTOFF_MAX_HZ,
                        CUTOFF_MAX_HZ,
                        DialUnit::Hz,
                    )
                    .log(),
                    p.cutoff_hz,
                    format_hz(p.cutoff_hz),
                    76.0,
                    accent,
                    s,
                    p,
                    set,
                    |p, value| p.cutoff_hz = value,
                ),
                voice_knob(
                    "sampler-resonance",
                    Dial::new("resonance", "RESONANCE", 0.0, 1.0, 0.0, DialUnit::Q),
                    p.resonance,
                    percent(p.resonance),
                    76.0,
                    accent,
                    s,
                    p,
                    set,
                    |p, value| p.resonance = value,
                ),
            ])
            .justify_around()
            // Off, the knobs still set what the filter will do when it is on.
            .opacity(if active { 1.0 } else { 0.48 }),
        )
}

/// The envelope's shape: attack, decay, the sustain held, the release.
fn amp_graph(p: QuickSamplerParams, accent: Rgba, s: f32) -> gpui::Div {
    graph_box(GRAPH_H, s).child(
        canvas(
            |_, _, _| (),
            move |bounds, _, window, _| {
                paint_grid(window, bounds, 4, 3);
                let (x0, y0) = plate::xy(bounds.origin);
                let w = f32::from(bounds.size.width).max(1.0);
                let h = f32::from(bounds.size.height).max(1.0);
                let inset = 6.0 * s;
                let points = envelope_points(&p, w - inset * 2.0, h - inset * 2.0);
                let curve: Vec<(f32, f32)> = points
                    .iter()
                    .map(|&(x, y)| (x0 + inset + x, y0 + inset + y))
                    .collect();
                paint_curve(window, &curve, y0 + h, accent, s);
                for &(x, y) in &curve[1..4] {
                    plate::paint_dot(window, x, y, 3.0 * s, color(WHITE));
                }
            },
        )
        .absolute()
        .size_full(),
    )
}

/// Amp envelope: its shape, and attack, decay, sustain and release.
pub fn amp_card(p: QuickSamplerParams, set: &SetVoice, accent: Rgba, s: f32) -> gpui::Div {
    let time = |id: &'static str, label: &'static str, default: f32| {
        Dial::new(
            id,
            label,
            0.0,
            ENVELOPE_KNOB_MAX_MS,
            default,
            DialUnit::Time,
        )
    };
    const CELL: f32 = 58.0;
    card(s)
        .child(card_title("Amp envelope", s))
        .child(amp_graph(p, accent, s))
        .child(knob_row(vec![
            voice_knob(
                "sampler-attack",
                time("attack", "ATTACK", 0.0),
                p.attack_ms,
                format_ms(p.attack_ms),
                CELL,
                accent,
                s,
                p,
                set,
                |p, value| p.attack_ms = value,
            ),
            voice_knob(
                "sampler-decay",
                time("decay", "DECAY", 0.0),
                p.decay_ms,
                format_ms(p.decay_ms),
                CELL,
                accent,
                s,
                p,
                set,
                |p, value| p.decay_ms = value,
            ),
            voice_knob(
                "sampler-sustain",
                Dial::new("sustain", "SUSTAIN", 0.0, 1.0, 1.0, DialUnit::Q),
                p.sustain,
                percent(p.sustain),
                CELL,
                accent,
                s,
                p,
                set,
                |p, value| p.sustain = value,
            ),
            voice_knob(
                "sampler-release",
                time("release", "RELEASE", 30.0),
                p.release_ms,
                format_ms(p.release_ms),
                CELL,
                accent,
                s,
                p,
                set,
                |p, value| p.release_ms = value,
            ),
        ]))
}

/// What the Voices setting means.
fn voices_note(polyphony: u8) -> String {
    if polyphony <= 1 {
        "Mono: a new note takes over from the one sounding.".to_string()
    } else {
        format!("Up to {polyphony} notes sound at once.")
    }
}

/// Output: level, pan, how much velocity counts, and how many voices.
pub fn output_card(p: QuickSamplerParams, set: &SetVoice, s: f32) -> gpui::Div {
    const CELL: f32 = 54.0;
    let ink = color(plate::INK);
    card(s)
        .child(card_title("Output", s))
        .child(knob_row(vec![
            voice_knob(
                "sampler-volume",
                Dial::new("volume", "VOLUME", 0.0, 1.0, 0.8, DialUnit::Q),
                p.volume,
                percent(p.volume),
                CELL,
                ink,
                s,
                p,
                set,
                |p, value| p.volume = value,
            ),
            voice_knob(
                "sampler-pan",
                Dial::new("pan", "PAN", -1.0, 1.0, 0.0, DialUnit::Q).bipolar(),
                p.pan,
                format_pan_label(p.pan),
                CELL,
                ink,
                s,
                p,
                set,
                |p, value| p.pan = value,
            ),
            voice_knob(
                "sampler-velocity",
                Dial::new("velocity", "VELOCITY", 0.0, 1.0, 1.0, DialUnit::Q),
                p.velocity,
                percent(p.velocity),
                CELL,
                ink,
                s,
                p,
                set,
                |p, value| p.velocity = value,
            ),
        ]))
        .child(
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(8.0 * s))
                .child(voice_stepper(
                    "sampler-voices",
                    "Voices",
                    if p.polyphony == 1 {
                        "Mono".to_string()
                    } else {
                        p.polyphony.to_string()
                    },
                    s,
                    p,
                    set,
                    |p, delta| p.polyphony = (p.polyphony as i32 + delta).clamp(0, 255) as u8,
                ))
                .child(note(voices_note(p.polyphony), s).text_center()),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_deck_fits_between_the_well_and_the_keyboard() {
        assert!(DECK_TOP < instrument_plate::DECK_BOTTOM);
        // The deck is as wide as the well.
        let output = 1_011.0 - 2.0 * DECK_PAD - 3.0 * DECK_GAP - PITCH_W - FILTER_W - AMP_W;
        // Three output knobs and the card's padding.
        assert!(output >= 3.0 * 54.0 + 32.0, "{output}");
    }

    #[test]
    fn long_names_are_cut_in_the_middle() {
        assert_eq!(shortened("kick.wav", 34), "kick.wav");
        let long = "a_very_long_sample_name_from_a_pack_v2_final.wav";
        let short = shortened(long, 34);
        assert_eq!(short.chars().count(), 34);
        assert!(short.starts_with("a_very") && short.ends_with("final.wav"));
    }

    #[test]
    fn the_voice_note_reads_the_setting() {
        assert!(voices_note(1).starts_with("Mono"));
        assert_eq!(voices_note(16), "Up to 16 notes sound at once.");
    }
}

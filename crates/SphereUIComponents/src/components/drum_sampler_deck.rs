//! The Drum Sampler plate's deck, right of the pads: the selected pad's
//! Voice, Envelope, Filter, Trigger and Output cards — and, on the Mixer
//! page, a strip per pad of the bank across the whole deck.
//!
//! Every control edits the pad through the panel's callbacks, so each edit
//! is a wire param the window sends; ranges and reset values are the DSP's
//! (`drumsampler::ipc::sanitize_pad` clamps to the same).

use std::rc::Rc;

use drumsampler::{
    FilterMode, Pad, BANK_PADS, MAX_CUTOFF_HZ, MAX_DECAY_MS, MAX_HOLD_MS, MIN_CUTOFF_HZ, OUTPUTS,
    PADS,
};
use gpui::prelude::FluentBuilder;
use gpui::{
    canvas, div, px, AnyElement, FontWeight, InteractiveElement, IntoElement, ParentElement, Rgba,
    StatefulInteractiveElement, Styled,
};

use crate::components::drum_sampler_menu::DrumMenuTarget;
use crate::components::drum_sampler_panel::{
    accent, choke_members, fader_position, filter_db, format_semis, hit_flash, level_unit,
    one_output_per_pad, output_label, pad_number, pad_outputs, time_from_position, time_position,
    DrumSamplerCallbacks, DrumSamplerPanelState, FaderPress, ACCENT, ATTACK_MAX_MS, DECK_GAP,
    DECK_H, DECK_PAD, DECK_TOP, ERROR, FADER_H, SOLO,
};
use crate::components::instrument_plate::{self, Step};
use crate::components::knob::format_pan_label;
use crate::components::plate::{
    self, caption, card, card_title, color, color_a, numbers, paint_rect, words, Dial, DialUnit,
    KnobCell, KnobLook, CARD, CARD_EDGE, FAINT, GHOST, PALE, SOFT, WHITE,
};
use crate::components::plate_parts::knob_row;
use crate::components::soundfont_player_mdi::note_label;
use crate::components::time_plate::{x_of_hz, BOX_EDGE, BOX_GROUND, FREQ_GRID, GRID};

/// The cards' knobs.
const LOOK: KnobLook = KnobLook::small(44.0);
const CELL: KnobCell = KnobCell::new(64.0, 9.5, 11.5);
/// The envelope's time knobs turn a square-root position, read off a dial
/// this many steps long so a drag keeps the resolution a time needs.
const TIME_STEPS: f32 = 1_000.0;
/// The filter graph's span, in dB.
const GRAPH_TOP_DB: f32 = 18.0;
const GRAPH_FLOOR_DB: f32 = -36.0;

fn knob_id(name: &str, pad: usize) -> String {
    format!("drum-{name}-{pad}")
}

/// A plate knob over one field of the selected pad.
fn pad_knob(
    panel: &DrumSamplerPanelState,
    cb: &DrumSamplerCallbacks,
    dial: Dial,
    value: f32,
    readout: Option<String>,
    s: f32,
    apply: impl Fn(&mut Pad, f32) + 'static,
) -> AnyElement {
    let index = panel.selected;
    plate::knob_with(
        knob_id(dial.id, index),
        dial,
        value,
        readout,
        LOOK,
        CELL,
        accent(),
        true,
        s,
        cb.pad_knob(panel, index, apply),
    )
}

/// A card's title row: the title, and `right` at its end.
fn title_row(title: &str, right: Option<AnyElement>, s: f32) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(10.0 * s))
        .h(px(26.0 * s))
        .child(card_title(title, s))
        .children(right)
}

/// The pad's cards, in three rows right of the pads.
pub fn pad_cards(panel: &DrumSamplerPanelState, cb: &DrumSamplerCallbacks, s: f32) -> AnyElement {
    let row = |cards: Vec<AnyElement>| {
        div()
            .flex()
            .flex_row()
            .flex_1()
            .min_h(px(0.0))
            .gap(px(DECK_GAP * s))
            .children(cards)
    };
    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w(px(0.0))
        .gap(px(DECK_GAP * s))
        .child(row(vec![
            voice_card(panel, cb, s).flex_1().into_any_element(),
            envelope_card(panel, cb, s)
                .w(px(250.0 * s))
                .flex_shrink_0()
                .into_any_element(),
        ]))
        .child(row(vec![filter_card(panel, cb, s)
            .flex_1()
            .into_any_element()]))
        .child(row(vec![
            trigger_card(panel, cb, s).flex_1().into_any_element(),
            output_card(panel, cb, s)
                .w(px(266.0 * s))
                .flex_shrink_0()
                .into_any_element(),
        ]))
        .into_any_element()
}

// ── Voice, Envelope ────────────────────────────────────────────────────────

fn voice_card(panel: &DrumSamplerPanelState, cb: &DrumSamplerCallbacks, s: f32) -> gpui::Div {
    let pad = panel.pad();
    card(s)
        .child(title_row("Voice", None, s))
        .child(knob_row(vec![
            pad_knob(
                panel,
                cb,
                Dial::new("tune", "Tune", -24.0, 24.0, 0.0, DialUnit::Plain).bipolar(),
                pad.tune_semitones,
                Some(format_semis(pad.tune_semitones)),
                s,
                |pad, v| pad.tune_semitones = v.round(),
            ),
            pad_knob(
                panel,
                cb,
                Dial::new("gain", "Gain", -60.0, 12.0, 0.0, DialUnit::Db),
                pad.gain_db,
                None,
                s,
                |pad, v| pad.gain_db = (v * 10.0).round() / 10.0,
            ),
            pad_knob(
                panel,
                cb,
                Dial::new("pan", "Pan", -1.0, 1.0, 0.0, DialUnit::Q).bipolar(),
                pad.pan,
                Some(format_pan_label(pad.pan)),
                s,
                |pad, v| pad.pan = v,
            ),
            pad_knob(
                panel,
                cb,
                Dial::new("velocity", "Velocity", 0.0, 100.0, 100.0, DialUnit::Percent),
                pad.velocity_sensitivity,
                None,
                s,
                |pad, v| pad.velocity_sensitivity = v.round(),
            ),
        ]))
}

/// A dial over a time's square-root position, `max` at its end.
fn time_dial(id: &'static str, label: &'static str, max: f32, default_ms: f32) -> Dial {
    Dial::new(
        id,
        label,
        0.0,
        TIME_STEPS,
        time_position(default_ms, max) * TIME_STEPS,
        DialUnit::Plain,
    )
}

fn envelope_card(panel: &DrumSamplerPanelState, cb: &DrumSamplerCallbacks, s: f32) -> gpui::Div {
    let pad = panel.pad();
    let decays = pad.decay_ms > 0.0;
    let time = |dial: Dial, ms: f32, max: f32, readout: String| {
        pad_knob(
            panel,
            cb,
            dial,
            time_position(ms, max) * TIME_STEPS,
            Some(readout),
            s,
            move |pad, v| {
                let ms = time_from_position(v / TIME_STEPS, max);
                match dial.id {
                    "attack" => pad.attack_ms = ms,
                    "hold" => pad.hold_ms = ms,
                    _ => pad.decay_ms = ms,
                }
            },
        )
    };
    let attack = time(
        time_dial("attack", "Attack", ATTACK_MAX_MS, 1.0),
        pad.attack_ms,
        ATTACK_MAX_MS,
        plate::fmt_ms(pad.attack_ms),
    );
    // Hold only matters once there is a decay to hold off.
    let hold = div()
        .opacity(if decays { 1.0 } else { 0.45 })
        .child(time(
            time_dial("hold", "Hold", MAX_HOLD_MS, 0.0),
            pad.hold_ms,
            MAX_HOLD_MS,
            plate::fmt_ms(pad.hold_ms),
        ))
        .into_any_element();
    let decay = time(
        time_dial("decay", "Decay", MAX_DECAY_MS, 0.0),
        pad.decay_ms,
        MAX_DECAY_MS,
        if decays {
            plate::fmt_ms(pad.decay_ms)
        } else {
            "Off".into()
        },
    );
    // With no decay the region plays out at full level: say so, since the
    // greyed Hold would not.
    let mode =
        (!decays).then(|| caption("Plays to the end", 9.5, 0.08, FAINT, s).into_any_element());
    card(s)
        .child(title_row("Envelope", mode, s))
        .child(knob_row(vec![attack, hold, decay]))
}

// ── Filter ─────────────────────────────────────────────────────────────────

fn filter_label(mode: FilterMode) -> &'static str {
    match mode {
        FilterMode::Off => "Off",
        FilterMode::LowPass => "LP",
        FilterMode::HighPass => "HP",
        FilterMode::BandPass => "BP",
    }
}

fn filter_card(panel: &DrumSamplerPanelState, cb: &DrumSamplerCallbacks, s: f32) -> gpui::Div {
    let index = panel.selected;
    let pad = panel.pad().clone();
    let active = pad.filter_mode != FilterMode::Off;
    let labels: Vec<&str> = FilterMode::ALL.into_iter().map(filter_label).collect();
    let set = cb.on_set_pad.clone();
    let mode_pad = pad.clone();
    let modes = instrument_plate::choices(
        "drum-filter",
        &labels,
        FilterMode::ALL
            .iter()
            .position(|mode| *mode == pad.filter_mode),
        accent(),
        Some(40.0),
        true,
        s,
        Rc::new(move |choice, _window, cx| {
            let mut next = mode_pad.clone();
            next.filter_mode = FilterMode::ALL[(choice.max(0) as usize).min(3)];
            set((index, next), cx)
        }),
    );
    let knobs = div()
        .flex()
        .flex_row()
        .flex_shrink_0()
        .opacity(if active { 1.0 } else { 0.45 })
        .child(pad_knob(
            panel,
            cb,
            Dial::new(
                "cutoff",
                "Cutoff",
                MIN_CUTOFF_HZ,
                MAX_CUTOFF_HZ,
                MAX_CUTOFF_HZ,
                DialUnit::Hz,
            )
            .log(),
            pad.cutoff_hz,
            None,
            s,
            |pad, v| pad.cutoff_hz = v.round(),
        ))
        .child(pad_knob(
            panel,
            cb,
            Dial::new("resonance", "Resonance", 0.0, 100.0, 0.0, DialUnit::Percent),
            pad.resonance,
            None,
            s,
            |pad, v| pad.resonance = v.round(),
        ));
    card(s)
        .child(title_row("Filter", Some(modes.into_any_element()), s))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(16.0 * s))
                .child(filter_graph(pad, s))
                .child(knobs),
        )
}

/// The pad filter's response across 20 Hz – 20 kHz, its cutoff marked.
fn filter_graph(pad: Pad, s: f32) -> AnyElement {
    let active = pad.filter_mode != FilterMode::Off;
    let ink = if active { accent() } else { color(GHOST) };
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            let (x0, y0) = plate::xy(bounds.origin);
            let w = f32::from(bounds.size.width).max(1.0);
            let h = f32::from(bounds.size.height).max(1.0);
            let y_for = |db: f32| {
                y0 + (GRAPH_TOP_DB - db.clamp(GRAPH_FLOOR_DB, GRAPH_TOP_DB))
                    / (GRAPH_TOP_DB - GRAPH_FLOOR_DB)
                    * h
            };
            for hz in FREQ_GRID {
                paint_rect(window, (x0 + x_of_hz(hz, w), y0, 1.0, h), color(GRID));
            }
            for db in [12.0, -12.0, -24.0] {
                paint_rect(window, (x0, y_for(db), w, 1.0), color(GRID));
            }
            paint_rect(window, (x0, y_for(0.0), w, 1.0), color(0x2e2e2e));
            let columns = (w as usize).max(2);
            let curve: Vec<(f32, f32)> = (0..columns)
                .map(|i| {
                    let t = i as f32 / (columns - 1) as f32;
                    let hz = 20.0 * 1_000.0_f32.powf(t);
                    (x0 + t * w, y_for(filter_db(&pad, hz)))
                })
                .collect();
            instrument_plate::paint_curve(window, &curve, y0 + h, ink, s);
            if active {
                let x = x0 + x_of_hz(pad.cutoff_hz, w);
                plate::paint_polyline(
                    window,
                    &[(x, y0), (x, y0 + h)],
                    1.0,
                    color_a(0xffffff, 0.35),
                    Some((3.0 * s, 3.0 * s)),
                );
            }
        },
    )
    .flex_1()
    .min_w(px(0.0))
    .h(px(84.0 * s))
    .bg(color(BOX_GROUND))
    .border(px(1.0))
    .border_color(color(BOX_EDGE))
    .into_any_element()
}

// ── Trigger ────────────────────────────────────────────────────────────────

fn trigger_card(panel: &DrumSamplerPanelState, cb: &DrumSamplerCallbacks, s: f32) -> gpui::Div {
    let index = panel.selected;
    let pad = panel.pad().clone();
    let note_pad = pad.clone();
    let set_note = cb.on_set_pad.clone();
    let note: Step = Rc::new(move |step, _window, cx| {
        let mut next = note_pad.clone();
        next.note = (next.note as i32 + step).clamp(0, 127) as u8;
        set_note((index, next), cx)
    });
    let toggle =
        |id: &'static str, label: &'static str, on: bool, ink: Rgba, apply: fn(&mut Pad)| {
            let mut next = pad.clone();
            apply(&mut next);
            plate::pill(
                (id, index),
                label,
                on,
                ink,
                plate::PillLook::words(26.0, 9.0, 11.5),
                true,
                s,
                {
                    let click = cb.send_pad(index, next);
                    move |_, window, cx| click(window, cx)
                },
            )
            .into_any_element()
        };
    let flags = div()
        .flex()
        .flex_row()
        .gap(px(4.0 * s))
        .child(toggle(
            "drum-reverse",
            "Reverse",
            pad.reverse,
            accent(),
            |p| p.reverse = !p.reverse,
        ))
        .child(toggle("drum-mute", "Mute", pad.muted, color(ERROR), |p| {
            p.muted = !p.muted
        }))
        .child(toggle("drum-solo", "Solo", pad.solo, color(SOLO), |p| {
            p.solo = !p.solo
        }));
    card(s)
        .child(title_row("Trigger", None, s))
        .child(
            div()
                .flex()
                .flex_row()
                .items_start()
                .justify_between()
                .child(instrument_plate::stepper(
                    "drum-note",
                    "Note",
                    note_label(pad.note),
                    true,
                    s,
                    note,
                ))
                .child(flags),
        )
        .child(choke(panel, cb, s))
}

/// The selected pad's choke group — Off, then 1–8 — each group showing how
/// many pads are in it, and which pads share the selected one's.
fn choke(panel: &DrumSamplerPanelState, cb: &DrumSamplerCallbacks, s: f32) -> AnyElement {
    let index = panel.selected;
    let pad = panel.pad().clone();
    let mut chips = div().flex().flex_row().w_full().gap(px(3.0 * s));
    for group in 0..=8_u8 {
        let on = pad.choke_group == group;
        let count = choke_members(&panel.params, group).len();
        let mut next = pad.clone();
        next.choke_group = group;
        let click = cb.send_pad(index, next);
        let (bg, border, ink) = plate::pill_colors(on, accent());
        chips = chips.child(
            div()
                .id(("drum-choke-group", group as usize))
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .flex_1()
                .min_w(px(0.0))
                .h(px(30.0 * s))
                .rounded(px(3.0 * s))
                .border(px(1.0))
                .border_color(border)
                .bg(bg)
                .cursor(gpui::CursorStyle::PointingHand)
                .hover(move |style| style.bg(color(if on { 0x222222 } else { 0x2e2e2e })))
                .on_click(move |_, window, cx| click(window, cx))
                .child(
                    words(
                        if group == 0 {
                            "Off".to_string()
                        } else {
                            group.to_string()
                        },
                        11.5,
                        0,
                        s,
                    )
                    .text_color(ink)
                    .line_height(px(13.0 * s)),
                )
                .when(group > 0 && count > 0, |chip| {
                    chip.child(
                        words(
                            format!("{count} pad{}", if count == 1 { "" } else { "s" }),
                            8.0,
                            FAINT,
                            s,
                        )
                        .line_height(px(10.0 * s)),
                    )
                }),
        );
    }
    let members = choke_members(&panel.params, pad.choke_group);
    let summary = if pad.choke_group == 0 {
        "Off \u{2014} this pad neither cuts nor is cut".to_string()
    } else if members.len() <= 1 {
        format!(
            "Group {} \u{2014} put another pad in it to cut each other",
            pad.choke_group
        )
    } else {
        let names: Vec<String> = members.iter().map(|m| pad_number(*m)).collect();
        format!(
            "Group {} \u{2014} pads {} cut each other",
            pad.choke_group,
            names.join(", ")
        )
    };
    div()
        .flex()
        .flex_col()
        .gap(px(5.0 * s))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0 * s))
                .child(caption("Choke group", 9.5, 0.1, SOFT, s))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .overflow_hidden()
                        .child(words(summary, 10.5, FAINT, s).w_full().truncate()),
                ),
        )
        .child(chips)
        .into_any_element()
}

// ── Output ─────────────────────────────────────────────────────────────────

/// Where the selected pad plays: Out 1, on the instrument's own channel, or
/// one of fifteen more outputs, each heard on a mixer channel of its own that
/// Studio adds as soon as a pad uses it (and drops when none does) — nothing
/// to set up first. Two shortcuts send the whole kit at once.
fn output_card(panel: &DrumSamplerPanelState, cb: &DrumSamplerCallbacks, s: f32) -> gpui::Div {
    let index = panel.selected;
    let pad = panel.pad().clone();
    let mut grid = div().flex().flex_col().gap(px(3.0 * s));
    for row in 0..2u8 {
        let mut line = div().flex().flex_row().gap(px(3.0 * s));
        for output in row * 8..row * 8 + 8 {
            let on = pad.output == output;
            let used = panel.params.pads.iter().any(|p| p.output == output);
            let mut next = pad.clone();
            next.output = output;
            let click = cb.send_pad(index, next);
            let (bg, border, ink) = if on {
                (color(0x1d1d1d), accent(), color(WHITE))
            } else if used {
                (color(0x262626), color(0x484848), color(PALE))
            } else {
                (color(0x202020), color(0x383838), color(GHOST))
            };
            line = line.child(
                div()
                    .id(("drum-output", output as usize))
                    .flex()
                    .items_center()
                    .justify_center()
                    .flex_1()
                    .min_w(px(0.0))
                    .h(px(22.0 * s))
                    .rounded(px(3.0 * s))
                    .border(px(1.0))
                    .border_color(border)
                    .bg(bg)
                    .cursor(gpui::CursorStyle::PointingHand)
                    .hover(|style| style.bg(color(0x2e2e2e)))
                    .on_click(move |_, window, cx| click(window, cx))
                    .child(numbers((output + 1).to_string(), 11.0, WHITE, s).text_color(ink)),
            );
        }
        grid = grid.child(line);
    }
    let mut extra: Vec<u8> = panel
        .params
        .pads
        .iter()
        .map(|p| p.output)
        .filter(|output| *output > 0)
        .collect();
    extra.sort_unstable();
    extra.dedup();
    let summary = if pad.output == 0 {
        "Out 1 \u{2014} this instrument's own channel".to_string()
    } else {
        format!(
            "{} \u{2014} its own mixer channel, \u{201C}Out Ch {}\u{201D}",
            output_label(pad.output),
            pad.output + 1
        )
    };
    let channels = if extra.is_empty() {
        "Every pad on Out 1".to_string()
    } else {
        let names: Vec<String> = extra.iter().map(|o| (o + 1).to_string()).collect();
        format!("Own: {}", names.join(" "))
    };
    let spread = cb.on_set_outputs.clone();
    let gather = cb.on_set_outputs.clone();
    let outputs = pad_outputs(&panel.params);
    let bank = panel.bank;
    let look = plate::PillLook::words(24.0, 8.0, 11.0);
    card(s)
        .child(title_row(
            "Output",
            Some(
                div()
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .child(numbers(channels, 10.0, FAINT, s).truncate())
                    .into_any_element(),
            ),
            s,
        ))
        .child(grid)
        .child(
            div()
                .w_full()
                .overflow_hidden()
                .child(words(summary, 10.5, SOFT, s).w_full().truncate()),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .gap(px(4.0 * s))
                .child(plate::pill(
                    "drum-outputs-spread",
                    "One per pad (this bank)",
                    false,
                    accent(),
                    look,
                    true,
                    s,
                    move |_, _, cx| spread(one_output_per_pad(outputs, bank), cx),
                ))
                .child(plate::pill(
                    "drum-outputs-gather",
                    "All to Out 1",
                    false,
                    accent(),
                    look,
                    !extra.is_empty(),
                    s,
                    move |_, _, cx| gather([0; PADS], cx),
                )),
        )
}

// ── Mixer ──────────────────────────────────────────────────────────────────

/// A strip per pad of the bank: what each pad plays, where it goes, and how
/// loud — the kit's own mixer, before the outputs reach Studio's.
pub fn mixer_deck(panel: &DrumSamplerPanelState, cb: &DrumSamplerCallbacks, s: f32) -> AnyElement {
    let first = panel.bank * BANK_PADS;
    let mut strips = div()
        .flex()
        .flex_row()
        .flex_1()
        .min_h(px(0.0))
        .w_full()
        .gap(px(6.0 * s));
    for index in first..first + BANK_PADS {
        strips = strips.child(mixer_strip(panel, index, cb, s));
    }
    plate::deck(s, DECK_TOP, DECK_H)
        .flex_col()
        .p(px(DECK_PAD * s))
        .gap(px(8.0 * s))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(12.0 * s))
                .child(card_title("Mixer", s))
                .child(words(
                    "Drag a fader for the pad's level \u{2014} double-click it for 0 dB \
                     \u{00B7} Out picks the mixer channel the pad plays on",
                    11.0,
                    SOFT,
                    s,
                )),
        )
        .child(strips)
        .into_any_element()
}

fn small_square(
    id: (&'static str, usize),
    path: &'static str,
    enabled: bool,
    s: f32,
    click: instrument_plate::Click,
) -> AnyElement {
    plate::square(
        id,
        18.0,
        plate::icon(path, (12.0, 12.0), (10.0, 10.0), 1.5, color(plate::ICON), s),
        enabled,
        s,
        move |_, window, cx| click(window, cx),
    )
    .into_any_element()
}

fn mixer_strip(
    panel: &DrumSamplerPanelState,
    index: usize,
    cb: &DrumSamplerCallbacks,
    s: f32,
) -> AnyElement {
    const MINUS: &str = "M2 6 H10";
    let pad = panel.params.pads[index].clone();
    let selected = panel.selected == index;
    let empty = pad.sample_name.is_none();
    let meter = level_unit(panel.levels[index]);
    let position = fader_position(pad.gain_db);
    let select = cb.on_select.clone();
    let press = cb.on_fader_press.clone();
    let context = cb.on_context.clone();
    let with = |apply: &dyn Fn(&mut Pad)| {
        let mut next = pad.clone();
        apply(&mut next);
        cb.send_pad(index, next)
    };
    let out_dec = with(&|p| p.output = p.output.saturating_sub(1));
    let out_inc = with(&|p| p.output = (p.output + 1).min(OUTPUTS as u8 - 1));
    let toggle = |id: &'static str, label: &'static str, on: bool, ink: Rgba, click| {
        let click: instrument_plate::Click = click;
        plate::pill(
            (id, index),
            label,
            on,
            ink,
            plate::PillLook::words(20.0, 0.0, 10.5),
            true,
            s,
            move |_, window, cx| click(window, cx),
        )
        .flex_1()
        .into_any_element()
    };

    let fader_h = FADER_H * s;
    let thumb_h = 10.0 * s;
    let fader = div()
        .id(("drum-fader", index))
        .relative()
        .w(px(22.0 * s))
        .h(px(fader_h))
        .cursor(gpui::CursorStyle::ResizeUpDown)
        .on_mouse_down(gpui::MouseButton::Left, move |event, _, cx| {
            press(
                FaderPress {
                    pad: index,
                    y: f32::from(event.position.y),
                    clicks: event.click_count,
                },
                cx,
            )
        })
        .child(
            div()
                .absolute()
                .left(px(9.0 * s))
                .w(px(4.0 * s))
                .top_0()
                .bottom_0()
                .rounded(px(2.0 * s))
                .bg(color(0x161616)),
        )
        .child(
            div()
                .absolute()
                .left(px(9.0 * s))
                .w(px(4.0 * s))
                .bottom_0()
                .h(px(fader_h * position))
                .rounded(px(2.0 * s))
                .bg(color_a(ACCENT, 0.55)),
        )
        // 0 dB, marked.
        .child(
            div()
                .absolute()
                .left(px(2.0 * s))
                .w(px(18.0 * s))
                .h(px(1.0))
                .bottom(px(fader_h * fader_position(0.0)))
                .bg(color(GHOST)),
        )
        .child(
            div()
                .absolute()
                .left_0()
                .w(px(22.0 * s))
                .h(px(thumb_h))
                .bottom(px((fader_h - thumb_h) * position))
                .rounded(px(2.0 * s))
                .border(px(1.0))
                .border_color(color(0x5a5a5a))
                .bg(if selected { accent() } else { color(0xc4c4c4) }),
        );
    let level = div()
        .relative()
        .w(px(5.0 * s))
        .h(px(fader_h))
        .rounded(px(2.0 * s))
        .bg(color(0x161616))
        .child(
            div()
                .absolute()
                .left_0()
                .right_0()
                .bottom_0()
                .h(px(fader_h * meter))
                .rounded(px(2.0 * s))
                .bg(accent()),
        );
    let pan = plate::knob_with(
        knob_id("strip-pan", index),
        Dial::new("pan", "Pan", -1.0, 1.0, 0.0, DialUnit::Q).bipolar(),
        pad.pan,
        Some(format_pan_label(pad.pan)),
        KnobLook::small(30.0),
        KnobCell::new(46.0, 8.5, 10.0),
        accent(),
        true,
        s,
        cb.pad_knob(panel, index, |pad, v| pad.pan = v),
    );
    let border = if selected { accent() } else { color(CARD_EDGE) };
    let bg = if meter > 0.02 {
        Rgba {
            a: 0.08 + 0.2 * meter,
            ..accent()
        }
    } else {
        color(CARD)
    };

    div()
        .id(("drum-strip", index))
        .relative()
        .flex()
        .flex_col()
        .items_center()
        .flex_1()
        .min_w(px(0.0))
        .overflow_hidden()
        .gap(px(8.0 * s))
        .px(px(4.0 * s))
        .py(px(8.0 * s))
        .rounded(px(4.0 * s))
        .border(px(1.0))
        .border_color(border)
        .bg(color(CARD))
        .when(meter > 0.02, |strip| strip.bg(bg))
        .opacity(if empty { 0.6 } else { 1.0 })
        .on_mouse_down(gpui::MouseButton::Right, move |event, _, cx| {
            context(
                (
                    DrumMenuTarget::Pad(index),
                    f32::from(event.position.x),
                    f32::from(event.position.y),
                ),
                cx,
            )
        })
        .child(
            div()
                .id(("drum-strip-head", index))
                .flex()
                .flex_col()
                .items_center()
                .w_full()
                .cursor(gpui::CursorStyle::PointingHand)
                .on_click(move |_, _, cx| select(index, cx))
                .child(
                    numbers(
                        pad_number(index),
                        12.0,
                        if selected { ACCENT } else { WHITE },
                        s,
                    )
                    .font_weight(FontWeight::SEMIBOLD),
                )
                .child(
                    div().w_full().overflow_hidden().child(
                        words(
                            pad.sample_name.clone().unwrap_or_else(|| "Empty".into()),
                            9.5,
                            if empty { GHOST } else { PALE },
                            s,
                        )
                        .w_full()
                        .text_center()
                        .truncate(),
                    ),
                )
                .child(numbers(note_label(pad.note), 9.5, FAINT, s)),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(2.0 * s))
                .child(caption("Out", 8.5, 0.1, SOFT, s))
                .child(numbers(
                    (pad.output + 1).to_string(),
                    13.0,
                    if pad.output > 0 { ACCENT } else { WHITE },
                    s,
                ))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(2.0 * s))
                        .child(small_square(
                            ("drum-strip-out", index * 2),
                            MINUS,
                            pad.output > 0,
                            s,
                            out_dec,
                        ))
                        .child(small_square(
                            ("drum-strip-out", index * 2 + 1),
                            plate::PLUS,
                            (pad.output as usize) < OUTPUTS - 1,
                            s,
                            out_inc,
                        )),
                ),
        )
        .child(pan)
        .child(
            div()
                .flex()
                .flex_row()
                .items_end()
                .gap(px(4.0 * s))
                .child(fader)
                .child(level),
        )
        .child(numbers(plate::signed(pad.gain_db), 10.5, PALE, s))
        .child(
            div()
                .flex()
                .flex_row()
                .w_full()
                .gap(px(2.0 * s))
                .child(toggle(
                    "drum-strip-mute",
                    "M",
                    pad.muted,
                    color(ERROR),
                    with(&|p| p.muted = !p.muted),
                ))
                .child(toggle(
                    "drum-strip-solo",
                    "S",
                    pad.solo,
                    color(SOLO),
                    with(&|p| p.solo = !p.solo),
                )),
        )
        .when(panel.hits[index] > 0, |strip| {
            strip.child(hit_flash(index, panel.hits[index], s))
        })
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_time_dials_reset_to_the_dsp_defaults() {
        let attack = time_dial("attack", "Attack", ATTACK_MAX_MS, 1.0);
        let at_default = attack.at(attack.norm(attack.default));
        assert_eq!(
            time_from_position(at_default / TIME_STEPS, ATTACK_MAX_MS),
            drumsampler::default_pad(0).attack_ms
        );
        let decay = time_dial("decay", "Decay", MAX_DECAY_MS, 0.0);
        assert_eq!(decay.default, 0.0);
        // A step of the dial is fine enough near the top of a long decay.
        let near_top = time_from_position(0.999, MAX_DECAY_MS);
        let top = time_from_position(1.0, MAX_DECAY_MS);
        assert!(top - near_top <= 20.0, "{near_top} → {top}");
    }

    #[test]
    fn the_filter_modes_are_labelled_in_order() {
        let labels: Vec<&str> = FilterMode::ALL.into_iter().map(filter_label).collect();
        assert_eq!(labels, ["Off", "LP", "HP", "BP"]);
    }
}

//! Rodhareist native editor — Drive (A/B) and Amp (tonestack) cards.
//!
//! Pure presentation: see `rodhareist_panel_dynamics` for the contract every
//! `rodhareist_panel_*` module follows.
//!
//! Filled in by a delegated task; see the Rodhareist native-UI rewrite.

use gpui::{
    div, px, App, AnyElement, ClickEvent, InteractiveElement, IntoElement, ParentElement,
    StatefulInteractiveElement, Styled, Window,
};

use crate::components::controls::fb_stepper_button;
use crate::components::knob::knob_with_default;
use crate::theme::{radius, space, typography, Colors};

/// Width of one knob cell, matching the quick sampler panel's style.
const KNOB_CELL_W: f32 = 64.0;
/// Diameter of a knob drawn inside a card.
const KNOB_SIZE: f32 = 34.0;

// ── Shared card chrome ───────────────────────────────────────────────────

/// Titled card shell: header row, then whatever body the caller supplies.
fn card(title: &'static str, header_extra: Option<AnyElement>, body: AnyElement) -> AnyElement {
    let mut header = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(space::SNUG))
        .child(
            div()
                .text_size(px(typography::DENSE_CAPTION))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(Colors::text_faint())
                .child(title),
        );
    if let Some(extra) = header_extra {
        header = header.child(extra);
    }

    div()
        .flex()
        .flex_col()
        .gap(px(space::SNUG))
        .p(px(space::LOOSE))
        .rounded(px(radius::SURFACE))
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_panel())
        .child(header)
        .child(body)
        .into_any_element()
}

/// One knob with its caption and readout, stacked ~64px wide.
fn knob_cell(caption: &'static str, readout: String, control: impl IntoElement) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .items_center()
        .w(px(KNOB_CELL_W))
        .gap(px(space::HAIR))
        .child(control)
        .child(
            div()
                .text_size(px(typography::DENSE_CAPTION))
                .text_color(Colors::text_muted())
                .child(caption),
        )
        .child(
            div()
                .text_size(px(typography::DENSE_CAPTION))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(Colors::text_secondary())
                .child(readout),
        )
        .into_any_element()
}

/// A wrapped row of [`knob_cell`]s.
fn knobs_row() -> gpui::Div {
    div().flex().flex_row().flex_wrap().gap(px(space::LOOSE))
}

/// `‹ Label ›` model stepper for the card header — previous/next buttons
/// flanking the current model's display name.
fn model_stepper(
    id_prefix: String,
    label: &'static str,
    on_prev: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_next: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::TIGHT))
        .child(fb_stepper_button(
            gpui::ElementId::Name(format!("{id_prefix}-prev").into()),
            "-",
            on_prev,
        ))
        .child(
            div()
                .min_w(px(96.0))
                .text_size(px(typography::DENSE_CAPTION))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(Colors::text_secondary())
                .child(label),
        )
        .child(fb_stepper_button(
            gpui::ElementId::Name(format!("{id_prefix}-next").into()),
            "+",
            on_next,
        ))
        .into_any_element()
}

/// Format a `0..10` tonestack value for a knob readout.
fn tone_value(value: f32) -> String {
    format!("{value:.1}")
}

// ── Drive model labels ───────────────────────────────────────────────────

fn drive_model_label(m: rodharerist::DriveModel) -> &'static str {
    use rodharerist::DriveModel::*;
    match m {
        Screamer => "Green Screamer",
        Minotaur => "Minotaur Boost",
        Rat => "Rats Nest",
        Breaker => "Breaker Blues",
        Fuzz => "Face Fuzz",
        Centurion => "Centurion",
        DsOne => "DS Classic",
        SuperDrive => "Super Drive",
        MetalCore => "Metal Core",
        TightRift => "Tight Rift",
        AmberCrunch => "Amber Crunch",
        CopperFuzz => "Copper Fuzz",
    }
}

fn amp_model_label(m: rodharerist::AmpModel) -> &'static str {
    use rodharerist::AmpModel::*;
    match m {
        Mandarin => "Mandarin 80",
        Plexi => "Brit Plexi 100",
        Twin => "Twin Clean",
        TopBoost => "Top Boost",
        Recto => "Recto Modern",
        Jcm => "JCM Crunch",
        Slate => "Lead Slate",
        Bassman => "Bassman",
        Boutique => "Overdrive Special",
        Invader => "Invader 5150",
        TweedCombo => "Tweed Deluxe",
    }
}

fn tone_engine_label(k: rodharerist::ToneEngineKind) -> &'static str {
    use rodharerist::ToneEngineKind::*;
    match k {
        Classic => "Amp",
        NamCapture => "NAM Capture",
        Bypass => "Bypass",
    }
}

// ── Drive card ───────────────────────────────────────────────────────────

/// Drive card: on/off, model stepper, gain/tone/level. Caller instantiates
/// this once for "Drive A" and once for "Drive B".
#[allow(clippy::too_many_arguments)]
pub(crate) fn drive_card(
    title: &'static str,
    id_prefix: &'static str,
    on: bool,
    model: rodharerist::DriveModel,
    gain: f32,
    tone: f32,
    level: f32,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_prev_model: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_next_model: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_gain: impl Fn(&f32, &mut Window, &mut App) + 'static,
    on_tone: impl Fn(&f32, &mut Window, &mut App) + 'static,
    on_level: impl Fn(&f32, &mut Window, &mut App) + 'static,
) -> gpui::AnyElement {
    let accent = if on {
        Colors::accent_primary()
    } else {
        Colors::text_muted()
    };

    let header_extra = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::SNUG))
        .child(model_stepper(
            format!("{id_prefix}-model"),
            drive_model_label(model),
            on_prev_model,
            on_next_model,
        ))
        .child(power_toggle(format!("{id_prefix}-power"), on, on_toggle))
        .into_any_element();

    let body = knobs_row()
        .child(knob_cell(
            "Gain",
            tone_value(gain),
            knob_with_default(
                format!("{id_prefix}-gain"),
                gain,
                0.0,
                10.0,
                KNOB_SIZE,
                accent,
                5.0,
                on_gain,
            ),
        ))
        .child(knob_cell(
            "Tone",
            tone_value(tone),
            knob_with_default(
                format!("{id_prefix}-tone"),
                tone,
                0.0,
                10.0,
                KNOB_SIZE,
                accent,
                5.0,
                on_tone,
            ),
        ))
        .child(knob_cell(
            "Level",
            tone_value(level),
            knob_with_default(
                format!("{id_prefix}-level"),
                level,
                0.0,
                10.0,
                KNOB_SIZE,
                accent,
                5.0,
                on_level,
            ),
        ))
        .into_any_element();

    card(title, Some(header_extra), body)
}

// ── Amp card ─────────────────────────────────────────────────────────────

/// Amp (tonestack) card: on/off, amp-model stepper, tone-engine stepper, and
/// the six tonestack knobs.
#[allow(clippy::too_many_arguments)]
pub(crate) fn amp_card(
    on: bool,
    model: rodharerist::AmpModel,
    tone_engine: rodharerist::ToneEngineKind,
    gain: f32,
    bass: f32,
    middle: f32,
    treble: f32,
    presence: f32,
    master: f32,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_prev_model: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_next_model: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_prev_engine: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_next_engine: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_gain: impl Fn(&f32, &mut Window, &mut App) + 'static,
    on_bass: impl Fn(&f32, &mut Window, &mut App) + 'static,
    on_middle: impl Fn(&f32, &mut Window, &mut App) + 'static,
    on_treble: impl Fn(&f32, &mut Window, &mut App) + 'static,
    on_presence: impl Fn(&f32, &mut Window, &mut App) + 'static,
    on_master: impl Fn(&f32, &mut Window, &mut App) + 'static,
) -> gpui::AnyElement {
    let accent = if on {
        Colors::accent_primary()
    } else {
        Colors::text_muted()
    };

    let header_extra = div()
        .flex()
        .flex_col()
        .items_end()
        .gap(px(space::HAIR))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::SNUG))
                .child(model_stepper(
                    "amp-model".to_string(),
                    amp_model_label(model),
                    on_prev_model,
                    on_next_model,
                ))
                .child(power_toggle("amp-power", on, on_toggle)),
        )
        .child(model_stepper(
            "amp-engine".to_string(),
            tone_engine_label(tone_engine),
            on_prev_engine,
            on_next_engine,
        ))
        .into_any_element();

    let body = knobs_row()
        .child(knob_cell(
            "Gain",
            tone_value(gain),
            knob_with_default(
                "amp-gain",
                gain,
                0.0,
                10.0,
                KNOB_SIZE,
                accent,
                5.0,
                on_gain,
            ),
        ))
        .child(knob_cell(
            "Bass",
            tone_value(bass),
            knob_with_default(
                "amp-bass",
                bass,
                0.0,
                10.0,
                KNOB_SIZE,
                accent,
                5.0,
                on_bass,
            ),
        ))
        .child(knob_cell(
            "Middle",
            tone_value(middle),
            knob_with_default(
                "amp-middle",
                middle,
                0.0,
                10.0,
                KNOB_SIZE,
                accent,
                5.0,
                on_middle,
            ),
        ))
        .child(knob_cell(
            "Treble",
            tone_value(treble),
            knob_with_default(
                "amp-treble",
                treble,
                0.0,
                10.0,
                KNOB_SIZE,
                accent,
                5.0,
                on_treble,
            ),
        ))
        .child(knob_cell(
            "Presence",
            tone_value(presence),
            knob_with_default(
                "amp-presence",
                presence,
                0.0,
                10.0,
                KNOB_SIZE,
                accent,
                5.0,
                on_presence,
            ),
        ))
        .child(knob_cell(
            "Master",
            tone_value(master),
            knob_with_default(
                "amp-master",
                master,
                0.0,
                10.0,
                KNOB_SIZE,
                accent,
                5.0,
                on_master,
            ),
        ))
        .into_any_element();

    card("Amp", Some(header_extra), body)
}

/// Small power toggle used in a card header.
fn power_toggle(
    id: impl Into<gpui::ElementId>,
    on: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let rest = if on {
        Colors::accent_primary()
    } else {
        Colors::button_bg()
    };
    let text = if on {
        Colors::on_accent()
    } else {
        Colors::text_muted()
    };
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .w(px(22.0))
        .h(px(18.0))
        .rounded(px(radius::CONTROL_SM))
        .border(px(1.0))
        .border_color(if on {
            Colors::accent_primary()
        } else {
            Colors::border_subtle()
        })
        .bg(rest)
        .cursor(gpui::CursorStyle::PointingHand)
        .text_size(px(typography::DENSE_CAPTION))
        .font_weight(gpui::FontWeight::BOLD)
        .text_color(text)
        .on_click(on_click)
        .child("\u{23FB}")
        .into_any_element()
}

//! Rodhareist native editor — Delay (A/B) and Reverb cards.
//!
//! Pure presentation: every function here takes plain values and closures
//! over plain values, never `rodharerist::Params` itself. `rodhareist_panel`
//! (the shell) owns the `Params`, computes the next value for whichever
//! control moved, diffs it against the wire table and forwards it to the
//! host — these functions only draw and report "the user changed X to Y".
//!
//! Two cards:
//!
//! * [`delay_card`] — one generic Delay instance; the caller renders it twice
//!   (Delay A, Delay B) with different `id_prefix`es.
//! * [`reverb_card`] — the single Reverb instance.
//!
//! Both follow the same shape as the other `rodhareist_panel_*` siblings: a
//! titled [`card`] with a model stepper in its header, and a wrapped row of
//! [`knob_cell`]s underneath.

use gpui::{div, px, AnyElement, App, IntoElement, ParentElement, Styled, Window};

use crate::components::controls::{fb_checkbox, fb_stepper_button};
use crate::components::knob::knob_with_default;
use crate::theme::{radius, space, typography, Colors};

const KNOB_SIZE: f32 = 32.0;
const KNOB_CELL_W: f32 = 56.0;

// ---------------------------------------------------------------------------
// Shared small helpers — re-derived locally; see quick_sampler_panel for the
// sibling versions. No cross-dependency between the parallel panel files.
// ---------------------------------------------------------------------------

/// Titled card shell: header row, then the caller's body below it.
fn card(title: &'static str, header_trailing: impl IntoElement, body: impl IntoElement) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(space::SNUG))
        .p(px(space::LOOSE))
        .rounded(px(radius::SURFACE))
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_panel())
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(px(space::BASE))
                .child(
                    div()
                        .text_size(px(typography::DENSE_CAPTION))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(Colors::text_faint())
                        .child(title),
                )
                .child(header_trailing),
        )
        .child(body)
        .into_any_element()
}

/// Wrapped row of knob cells.
fn knobs_row() -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .flex_wrap()
        .items_start()
        .gap(px(space::TIGHT))
}

/// One knob with its caption and readout underneath.
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

/// `‹ Model Name ›` stepper used in a card's header to cycle its voicing.
fn model_stepper(
    id_prefix: &'static str,
    label: &'static str,
    on_prev: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    on_next: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::TIGHT))
        .child(fb_stepper_button(
            format!("{id_prefix}-model-prev"),
            "−",
            on_prev,
        ))
        .child(
            div()
                .min_w(px(96.0))
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(typography::UI_XS))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(Colors::text_secondary())
                .child(label),
        )
        .child(fb_stepper_button(
            format!("{id_prefix}-model-next"),
            "+",
            on_next,
        ))
        .into_any_element()
}

fn percent(value: f32) -> String {
    format!("{:.0}%", value)
}

fn format_ms(ms: f32) -> String {
    if ms < 1_000.0 {
        format!("{:.0} ms", ms)
    } else {
        format!("{:.2} s", ms / 1_000.0)
    }
}

fn format_seconds(s: f32) -> String {
    format!("{:.2} s", s)
}

fn delay_model_label(model: rodharerist::DelayModel) -> &'static str {
    match model {
        rodharerist::DelayModel::Tape => "Tape Echo",
        rodharerist::DelayModel::Digital => "Digital Delay",
        rodharerist::DelayModel::Analog => "Analog Delay",
        rodharerist::DelayModel::PingPong => "Ping-Pong",
        rodharerist::DelayModel::Dual => "Dual Delay",
    }
}

fn reverb_model_label(model: rodharerist::ReverbModel) -> &'static str {
    match model {
        rodharerist::ReverbModel::Plate => "Studio Plate",
        rodharerist::ReverbModel::Room => "Tracking Room",
        rodharerist::ReverbModel::Hall => "Concert Hall",
        rodharerist::ReverbModel::Shimmer => "Shimmer",
    }
}

// ---------------------------------------------------------------------------
// Delay
// ---------------------------------------------------------------------------

/// One Delay card: on/off, a model stepper and the Time/Feedback/Mix/Tone
/// knobs every model shares. The caller renders this twice, for Delay A and
/// Delay B, with distinct `title`/`id_prefix` pairs.
#[allow(clippy::too_many_arguments)]
pub(crate) fn delay_card(
    title: &'static str,
    id_prefix: &'static str,
    on: bool,
    model: rodharerist::DelayModel,
    time_ms: f32,
    fb: f32,
    mix: f32,
    tone: f32,
    on_toggle: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    on_prev_model: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    on_next_model: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    on_time: impl Fn(&f32, &mut Window, &mut App) + 'static,
    on_fb: impl Fn(&f32, &mut Window, &mut App) + 'static,
    on_mix: impl Fn(&f32, &mut Window, &mut App) + 'static,
    on_tone: impl Fn(&f32, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let header_trailing = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::BASE))
        .child(model_stepper(
            id_prefix,
            delay_model_label(model),
            on_prev_model,
            on_next_model,
        ))
        .child(fb_checkbox(
            format!("{id_prefix}-on"),
            "On",
            on,
            true,
            on_toggle,
        ));

    let mut body = knobs_row();
    body = body
        .child(knob_cell(
            "Time",
            format_ms(time_ms),
            knob_with_default(
                format!("{id_prefix}-time"),
                time_ms,
                40.0,
                1200.0,
                KNOB_SIZE,
                Colors::accent_primary(),
                350.0,
                on_time,
            ),
        ))
        .child(knob_cell(
            "Feedback",
            percent(fb),
            knob_with_default(
                format!("{id_prefix}-feedback"),
                fb,
                0.0,
                100.0,
                KNOB_SIZE,
                Colors::accent_primary(),
                35.0,
                on_fb,
            ),
        ))
        .child(knob_cell(
            "Mix",
            percent(mix),
            knob_with_default(
                format!("{id_prefix}-mix"),
                mix,
                0.0,
                100.0,
                KNOB_SIZE,
                Colors::accent_primary(),
                30.0,
                on_mix,
            ),
        ))
        .child(knob_cell(
            "Tone",
            format!("{:.1}", tone),
            knob_with_default(
                format!("{id_prefix}-tone"),
                tone,
                0.0,
                10.0,
                KNOB_SIZE,
                Colors::accent_primary(),
                5.0,
                on_tone,
            ),
        ));

    let mut element = card(title, header_trailing, body);
    if !on {
        element = div().opacity(0.6).child(element).into_any_element();
    }
    element
}

// ---------------------------------------------------------------------------
// Reverb
// ---------------------------------------------------------------------------

/// The single Reverb card: on/off, a model stepper, Decay/Mix, and a Shimmer
/// knob that only matters for the Shimmer model (dimmed otherwise, but always
/// rendered so its layout slot is stable).
#[allow(clippy::too_many_arguments)]
pub(crate) fn reverb_card(
    on: bool,
    model: rodharerist::ReverbModel,
    decay_s: f32,
    mix: f32,
    shimmer: f32,
    on_toggle: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    on_prev_model: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    on_next_model: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    on_decay: impl Fn(&f32, &mut Window, &mut App) + 'static,
    on_mix: impl Fn(&f32, &mut Window, &mut App) + 'static,
    on_shimmer: impl Fn(&f32, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let header_trailing = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::BASE))
        .child(model_stepper(
            "reverb",
            reverb_model_label(model),
            on_prev_model,
            on_next_model,
        ))
        .child(fb_checkbox("reverb-on", "On", on, true, on_toggle));

    let shimmer_cell = knob_cell(
        "Shimmer",
        percent(shimmer),
        knob_with_default(
            "reverb-shimmer",
            shimmer,
            0.0,
            100.0,
            KNOB_SIZE,
            Colors::accent_primary(),
            0.0,
            on_shimmer,
        ),
    );
    let shimmer_cell: AnyElement = if model != rodharerist::ReverbModel::Shimmer {
        div().opacity(0.4).child(shimmer_cell).into_any_element()
    } else {
        shimmer_cell
    };

    let body = knobs_row()
        .child(knob_cell(
            "Decay",
            format_seconds(decay_s),
            knob_with_default(
                "reverb-decay",
                decay_s,
                0.5,
                15.0,
                KNOB_SIZE,
                Colors::accent_primary(),
                2.5,
                on_decay,
            ),
        ))
        .child(knob_cell(
            "Mix",
            percent(mix),
            knob_with_default(
                "reverb-mix",
                mix,
                0.0,
                100.0,
                KNOB_SIZE,
                Colors::accent_primary(),
                30.0,
                on_mix,
            ),
        ))
        .child(shimmer_cell);

    let mut element = card("Reverb", header_trailing, body);
    if !on {
        element = div().opacity(0.6).child(element).into_any_element();
    }
    element
}

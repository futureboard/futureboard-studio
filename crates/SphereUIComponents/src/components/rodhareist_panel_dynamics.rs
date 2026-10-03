//! Rodhareist native editor — Gate, Compressor (A/B) and Studio EQ (A/B) cards.
//!
//! Pure presentation: every function here takes plain values and closures
//! over plain values, never `rodharerist::Params` itself. `rodhareist_panel`
//! (the shell) owns the `Params`, computes the next value for whichever knob
//! moved, diffs it against the wire table and forwards it to the host — these
//! functions only draw and report "the user changed X to Y".
//!
//! Filled in by a delegated task; see the Rodhareist native-UI rewrite.

use gpui::{div, px, AnyElement, App, ClickEvent, IntoElement, ParentElement, Styled, Window};
use rodharerist::EqModel;

use crate::components::controls::{fb_checkbox, fb_stepper_button};
use crate::components::knob::{knob_bipolar, knob_with_default};
use crate::theme::{radius, space, typography, Colors};

// ---------------------------------------------------------------------------
// Local layout helpers (mirrors quick_sampler_panel.rs's module/knob_cell).
// ---------------------------------------------------------------------------

fn card(title: &'static str, header_extra: Option<AnyElement>, controls: impl IntoElement) -> AnyElement {
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
                .children(header_extra),
        )
        .child(controls)
        .into_any_element()
}

fn knob_cell(caption: &'static str, readout: String, control: impl IntoElement) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .items_center()
        .w(px(64.0))
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

fn knobs_row() -> gpui::Div {
    div().flex().flex_row().flex_wrap().items_start().gap(px(space::LOOSE))
}

fn model_stepper(
    id_prefix: &'static str,
    label: &'static str,
    on_prev: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_next: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::TIGHT))
        .child(fb_stepper_button(format!("{id_prefix}-model-prev"), "‹", on_prev))
        .child(
            div()
                .min_w(px(90.0))
                .text_size(px(typography::DENSE_CAPTION))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(Colors::text_primary())
                .child(label),
        )
        .child(fb_stepper_button(format!("{id_prefix}-model-next"), "›", on_next))
        .into_any_element()
}

fn eq_model_label(model: EqModel) -> &'static str {
    match model {
        EqModel::Studio => "Studio EQ",
        EqModel::Vintage => "Vintage EQ",
        EqModel::Modern => "Modern EQ",
    }
}

// ---------------------------------------------------------------------------
// Gate
// ---------------------------------------------------------------------------

const GATE_THRESH_DEFAULT: f32 = -45.0;

pub(crate) fn gate_card(
    on: bool,
    thresh_db: f32,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_thresh: impl Fn(&f32, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let header = fb_checkbox("gate-on", "On", on, true, on_toggle).into_any_element();

    let controls = knobs_row().child(knob_cell(
        "Threshold",
        format!("{:.1} dB", thresh_db),
        knob_with_default(
            "gate-thresh",
            thresh_db,
            -80.0,
            0.0,
            36.0,
            Colors::accent_primary(),
            GATE_THRESH_DEFAULT,
            on_thresh,
        ),
    ));

    card("Gate", Some(header), controls)
}

// ---------------------------------------------------------------------------
// Compressor (shared by A and B)
// ---------------------------------------------------------------------------

const COMP_THRESH_DEFAULT: f32 = -18.0;
const COMP_RATIO_DEFAULT: f32 = 2.5;
const COMP_ATTACK_DEFAULT: f32 = 8.0;
const COMP_RELEASE_DEFAULT: f32 = 120.0;
const COMP_MAKEUP_DEFAULT: f32 = 0.0;

#[allow(clippy::too_many_arguments)]
pub(crate) fn comp_card(
    title: &'static str,
    id_prefix: &'static str,
    on: bool,
    thresh_db: f32,
    ratio: f32,
    attack_ms: f32,
    release_ms: f32,
    makeup_db: f32,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_thresh: impl Fn(&f32, &mut Window, &mut App) + 'static,
    on_ratio: impl Fn(&f32, &mut Window, &mut App) + 'static,
    on_attack: impl Fn(&f32, &mut Window, &mut App) + 'static,
    on_release: impl Fn(&f32, &mut Window, &mut App) + 'static,
    on_makeup: impl Fn(&f32, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let header = fb_checkbox(format!("{id_prefix}-on"), "On", on, true, on_toggle).into_any_element();

    let controls = knobs_row()
        .child(knob_cell(
            "Threshold",
            format!("{:.1} dB", thresh_db),
            knob_with_default(
                format!("{id_prefix}-thresh"),
                thresh_db,
                -60.0,
                0.0,
                36.0,
                Colors::accent_primary(),
                COMP_THRESH_DEFAULT,
                on_thresh,
            ),
        ))
        .child(knob_cell(
            "Ratio",
            format!("{:.1}:1", ratio),
            knob_with_default(
                format!("{id_prefix}-ratio"),
                ratio,
                1.0,
                20.0,
                36.0,
                Colors::accent_primary(),
                COMP_RATIO_DEFAULT,
                on_ratio,
            ),
        ))
        .child(knob_cell(
            "Attack",
            format!("{:.1} ms", attack_ms),
            knob_with_default(
                format!("{id_prefix}-attack"),
                attack_ms,
                0.1,
                100.0,
                36.0,
                Colors::accent_primary(),
                COMP_ATTACK_DEFAULT,
                on_attack,
            ),
        ))
        .child(knob_cell(
            "Release",
            format!("{:.0} ms", release_ms),
            knob_with_default(
                format!("{id_prefix}-release"),
                release_ms,
                10.0,
                1000.0,
                36.0,
                Colors::accent_primary(),
                COMP_RELEASE_DEFAULT,
                on_release,
            ),
        ))
        .child(knob_cell(
            "Makeup",
            format!("{:.1} dB", makeup_db),
            knob_with_default(
                format!("{id_prefix}-makeup"),
                makeup_db,
                0.0,
                24.0,
                36.0,
                Colors::accent_primary(),
                COMP_MAKEUP_DEFAULT,
                on_makeup,
            ),
        ));

    card(title, Some(header), controls)
}

// ---------------------------------------------------------------------------
// Studio EQ (shared by A and B)
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub(crate) fn eq_card(
    title: &'static str,
    id_prefix: &'static str,
    on: bool,
    model: EqModel,
    low_gain_db: f32,
    mid1_freq_hz: f32,
    mid1_gain_db: f32,
    mid2_freq_hz: f32,
    mid2_gain_db: f32,
    high_gain_db: f32,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_prev_model: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_next_model: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_low: impl Fn(&f32, &mut Window, &mut App) + 'static,
    on_mid1_freq: impl Fn(&f32, &mut Window, &mut App) + 'static,
    on_mid1_gain: impl Fn(&f32, &mut Window, &mut App) + 'static,
    on_mid2_freq: impl Fn(&f32, &mut Window, &mut App) + 'static,
    on_mid2_gain: impl Fn(&f32, &mut Window, &mut App) + 'static,
    on_high: impl Fn(&f32, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let header = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::BASE))
        .child(model_stepper(
            id_prefix,
            eq_model_label(model),
            on_prev_model,
            on_next_model,
        ))
        .child(fb_checkbox(format!("{id_prefix}-on"), "On", on, true, on_toggle))
        .into_any_element();

    let controls = knobs_row()
        .child(knob_cell(
            "Low",
            format!("{:.1} dB", low_gain_db),
            knob_bipolar(
                format!("{id_prefix}-low"),
                low_gain_db,
                -15.0,
                15.0,
                36.0,
                Colors::accent_primary(),
                None,
                0.0,
                on_low,
            ),
        ))
        .child(knob_cell(
            "Mid1 Freq",
            format!("{:.0} Hz", mid1_freq_hz),
            knob_with_default(
                format!("{id_prefix}-mid1-freq"),
                mid1_freq_hz,
                100.0,
                1000.0,
                36.0,
                Colors::accent_primary(),
                400.0,
                on_mid1_freq,
            ),
        ))
        .child(knob_cell(
            "Mid1",
            format!("{:.1} dB", mid1_gain_db),
            knob_bipolar(
                format!("{id_prefix}-mid1-gain"),
                mid1_gain_db,
                -15.0,
                15.0,
                36.0,
                Colors::accent_primary(),
                None,
                0.0,
                on_mid1_gain,
            ),
        ))
        .child(knob_cell(
            "Mid2 Freq",
            format!("{:.0} Hz", mid2_freq_hz),
            knob_with_default(
                format!("{id_prefix}-mid2-freq"),
                mid2_freq_hz,
                600.0,
                6000.0,
                36.0,
                Colors::accent_primary(),
                2000.0,
                on_mid2_freq,
            ),
        ))
        .child(knob_cell(
            "Mid2",
            format!("{:.1} dB", mid2_gain_db),
            knob_bipolar(
                format!("{id_prefix}-mid2-gain"),
                mid2_gain_db,
                -15.0,
                15.0,
                36.0,
                Colors::accent_primary(),
                None,
                0.0,
                on_mid2_gain,
            ),
        ))
        .child(knob_cell(
            "High",
            format!("{:.1} dB", high_gain_db),
            knob_bipolar(
                format!("{id_prefix}-high"),
                high_gain_db,
                -15.0,
                15.0,
                36.0,
                Colors::accent_primary(),
                None,
                0.0,
                on_high,
            ),
        ));

    card(title, Some(header), controls)
}

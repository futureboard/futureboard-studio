//! Rodhareist native editor — Mod (A/B) and Wah cards.
//!
//! Pure presentation: see `rodhareist_panel_dynamics` for the contract every
//! `rodhareist_panel_*` module follows.
//!
//! Filled in by a delegated task; see the Rodhareist native-UI rewrite.

use gpui::{div, px, App, AnyElement, IntoElement, ParentElement, Styled, Window};

use crate::components::controls::{fb_checkbox, fb_stepper_button};
use crate::components::knob::knob_with_default;
use crate::theme::{radius, space, typography, Colors};

const KNOB_SIZE: f32 = 32.0;
const KNOB_CELL_W: f32 = 56.0;

/// Titled panel shell shared by every card in this file.
fn card() -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap(px(space::SNUG))
        .p(px(space::LOOSE))
        .rounded(px(radius::SURFACE))
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_panel())
}

/// One knob with its caption underneath, at this file's shared knob size.
fn knob_cell(caption: &'static str, control: impl IntoElement) -> AnyElement {
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
        .into_any_element()
}

/// Wrapped row of [`knob_cell`]s.
fn knobs_row() -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .flex_wrap()
        .items_start()
        .gap(px(space::TIGHT))
}

/// `‹ Model Name ›` stepper pair that cycles a model enum, plus the on/off
/// checkbox, laid out as the card's header row.
fn card_header(
    id_prefix: &'static str,
    title: &'static str,
    model_label: &'static str,
    on: bool,
    on_toggle: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    on_prev_model: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    on_next_model: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Div {
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
        .child(model_stepper(
            id_prefix,
            model_label,
            on_prev_model,
            on_next_model,
        ))
        .child(fb_checkbox(
            gpui::ElementId::Name(format!("{id_prefix}-on").into()),
            "On",
            on,
            true,
            on_toggle,
        ))
}

/// `‹  Model Name  ›` model stepper: a pair of small buttons flanking the
/// current model's display name.
fn model_stepper(
    id_prefix: &'static str,
    model_label: &'static str,
    on_prev: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    on_next: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::AnyElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::TIGHT))
        .child(fb_stepper_button(
            gpui::ElementId::Name(format!("{id_prefix}-model-prev").into()),
            "−",
            on_prev,
        ))
        .child(
            div()
                .min_w(px(120.0))
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(typography::UI_XS))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(Colors::text_secondary())
                .child(model_label),
        )
        .child(fb_stepper_button(
            gpui::ElementId::Name(format!("{id_prefix}-model-next").into()),
            "+",
            on_next,
        ))
        .into_any_element()
}

fn mod_model_label(model: rodharerist::ModModel) -> &'static str {
    match model {
        rodharerist::ModModel::Chorus => "70s Analog Chorus",
        rodharerist::ModModel::Phaser => "Vibe Phase 90",
        rodharerist::ModModel::Flanger => "Jet Flanger",
        rodharerist::ModModel::Tremolo => "Opto Tremolo",
        rodharerist::ModModel::MolamSwirl => "Molam Swirl",
        rodharerist::ModModel::PhinVibe => "Phin Vibe",
        rodharerist::ModModel::KhaenSwirl => "Khaen Swirl",
        rodharerist::ModModel::BiLam => "Bi-Lam",
        rodharerist::ModModel::IsanJet => "Isan Jet",
        rodharerist::ModModel::SoftPhase => "Soft Phase",
        rodharerist::ModModel::WideVibe => "Wide Vibe",
    }
}

fn wah_model_label(model: rodharerist::WahModel) -> &'static str {
    match model {
        rodharerist::WahModel::CryWah => "Cry Wah",
        rodharerist::WahModel::TouchWah => "Touch Wah",
    }
}

/// Mod card — one of the two identical Mod slots (A/B). All 11 algorithms
/// share the same Rate/Depth/Mix knobs, so the model stepper only changes the
/// voicing underneath; the knobs never change shape.
#[allow(clippy::too_many_arguments)]
pub(crate) fn mod_card(
    title: &'static str,
    id_prefix: &'static str,
    on: bool,
    model: rodharerist::ModModel,
    rate: f32,
    depth: f32,
    mix: f32,
    on_toggle: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    on_prev_model: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    on_next_model: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    on_rate: impl Fn(&f32, &mut Window, &mut App) + 'static,
    on_depth: impl Fn(&f32, &mut Window, &mut App) + 'static,
    on_mix: impl Fn(&f32, &mut Window, &mut App) + 'static,
) -> gpui::AnyElement {
    let rate_id: gpui::SharedString = format!("{id_prefix}-rate").into();
    let depth_id: gpui::SharedString = format!("{id_prefix}-depth").into();
    let mix_id: gpui::SharedString = format!("{id_prefix}-mix").into();

    card()
        .child(card_header(
            id_prefix,
            title,
            mod_model_label(model),
            on,
            on_toggle,
            on_prev_model,
            on_next_model,
        ))
        .child(
            knobs_row()
                .child(knob_cell(
                    "Rate",
                    knob_with_default(
                        rate_id,
                        rate,
                        0.0,
                        10.0,
                        KNOB_SIZE,
                        Colors::accent_primary(),
                        5.0,
                        on_rate,
                    ),
                ))
                .child(knob_cell(
                    "Depth",
                    knob_with_default(
                        depth_id,
                        depth,
                        0.0,
                        10.0,
                        KNOB_SIZE,
                        Colors::accent_primary(),
                        5.0,
                        on_depth,
                    ),
                ))
                .child(knob_cell(
                    "Mix",
                    knob_with_default(
                        mix_id,
                        mix,
                        0.0,
                        100.0,
                        KNOB_SIZE,
                        Colors::accent_primary(),
                        50.0,
                        on_mix,
                    ),
                )),
        )
        .into_any_element()
}

/// Wah card — the single Wah slot. `sens` (envelope sensitivity) only matters
/// for Touch Wah, but it is always rendered, dimmed for Cry Wah so the layout
/// never jumps as the model is stepped.
#[allow(clippy::too_many_arguments)]
pub(crate) fn wah_card(
    on: bool,
    model: rodharerist::WahModel,
    pos: f32,
    res: f32,
    sens: f32,
    on_toggle: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    on_prev_model: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    on_next_model: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    on_pos: impl Fn(&f32, &mut Window, &mut App) + 'static,
    on_res: impl Fn(&f32, &mut Window, &mut App) + 'static,
    on_sens: impl Fn(&f32, &mut Window, &mut App) + 'static,
) -> gpui::AnyElement {
    const ID_PREFIX: &str = "wah";
    let sens_dim = model != rodharerist::WahModel::TouchWah;

    let sens_cell = {
        let cell = knob_cell(
            "Sens",
            knob_with_default(
                "wah-sens",
                sens,
                0.0,
                10.0,
                KNOB_SIZE,
                Colors::accent_primary(),
                5.0,
                on_sens,
            ),
        );
        if sens_dim {
            div().opacity(0.4).child(cell).into_any_element()
        } else {
            cell
        }
    };

    card()
        .child(card_header(
            ID_PREFIX,
            "Wah",
            wah_model_label(model),
            on,
            on_toggle,
            on_prev_model,
            on_next_model,
        ))
        .child(
            knobs_row()
                .child(knob_cell(
                    "Position",
                    knob_with_default(
                        "wah-pos",
                        pos,
                        0.0,
                        10.0,
                        KNOB_SIZE,
                        Colors::accent_primary(),
                        5.0,
                        on_pos,
                    ),
                ))
                .child(knob_cell(
                    "Resonance",
                    knob_with_default(
                        "wah-res",
                        res,
                        0.0,
                        10.0,
                        KNOB_SIZE,
                        Colors::accent_primary(),
                        5.0,
                        on_res,
                    ),
                ))
                .child(sens_cell),
        )
        .into_any_element()
}

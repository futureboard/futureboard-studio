//! Rodhareist native editor — Cabinet/IR and NAM Capture cards.
//!
//! Pure presentation: see `rodhareist_panel_dynamics` for the contract every
//! `rodhareist_panel_*` module follows. Every exported function here takes
//! plain values and closures over plain values — never `rodharerist::Params`
//! — and the caller owns any file-dialog / host plumbing; these functions
//! only render a "Load…" button and call the closure they are given.

use gpui::{div, px, App, AnyElement, IntoElement, ParentElement, Styled, Window};

use crate::components::controls::{fb_button, fb_checkbox, fb_stepper_button, fb_progress, FbButtonKind};
use crate::components::knob::{knob, knob_bipolar};
use crate::theme::{radius, space, typography, Colors};

const KNOB_SIZE: f32 = 34.0;
const KNOB_CELL_W: f32 = 64.0;

// ── Shared card chrome ──────────────────────────────────────────────────────

/// Titled panel shell shared by both cards in this file.
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

/// Header row: title on the left, optional trailing controls (model stepper)
/// on the right.
fn card_header(title: &'static str, trailing: Option<AnyElement>) -> AnyElement {
    div()
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
        )
        .children(trailing)
        .into_any_element()
}

/// A `‹ Model Name ›` stepper used for the cabinet/mic model pickers.
fn model_stepper(
    id_prefix: &'static str,
    label: String,
    on_prev: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    on_next: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
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
                .min_w(px(140.0))
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

/// Row of knob cells, wrapped.
fn knobs_row() -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .flex_wrap()
        .items_start()
        .gap(px(space::TIGHT))
}

/// One knob with its caption and numeric readout underneath, ~64px wide.
fn knob_cell(caption: &'static str, readout: String, control: impl IntoElement) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .items_center()
        .w(px(KNOB_CELL_W))
        .gap(px(space::TIGHT))
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

fn percent(value: f32) -> String {
    format!("{:.0}%", value.clamp(0.0, 100.0))
}

fn db(value: f32) -> String {
    format!("{value:+.1} dB")
}

/// Load-button / loading-spinner / error / loaded-file block shared by the IR
/// loader and the NAM capture loader.
fn load_block(
    id: &'static str,
    button_label: &'static str,
    loading: bool,
    error: Option<String>,
    loaded_label: Option<String>,
    on_load: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let mut col = div().flex().flex_col().gap(px(space::TIGHT));

    if loading {
        col = col.child(
            div()
                .flex()
                .flex_col()
                .gap(px(space::TIGHT))
                .w(px(160.0))
                .child(
                    div()
                        .text_size(px(typography::DENSE_CAPTION))
                        .text_color(Colors::text_muted())
                        .child("Loading…"),
                )
                .child(fb_progress(0.5)),
        );
    } else {
        col = col.child(fb_button(
            gpui::ElementId::Name(id.into()),
            button_label,
            FbButtonKind::Default,
            true,
            on_load,
        ));
    }

    if let Some(name) = loaded_label {
        col = col.child(
            div()
                .text_size(px(typography::DENSE_CAPTION))
                .text_color(Colors::text_secondary())
                .child(name),
        );
    }

    if let Some(err) = error {
        col = col.child(
            div()
                .text_size(px(typography::DENSE_CAPTION))
                .text_color(Colors::accent_danger())
                .child(err),
        );
    }

    col.into_any_element()
}

// ── Cabinet / IR card ───────────────────────────────────────────────────────

/// Cabinet/IR card: on/off, cabinet model stepper, mic model stepper, mic
/// position and distance knobs, and the IR load block (shown whenever the
/// model is [`rodharerist::CabModel::Ir`]).
#[allow(clippy::too_many_arguments)]
pub(crate) fn cab_card(
    on: bool,
    model: rodharerist::CabModel,
    mic_model: rodharerist::MicModel,
    mic: f32,
    dist: f32,
    ir_loaded: Option<(String, f32)>,
    ir_loading: bool,
    ir_error: Option<String>,
    on_toggle: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    on_prev_model: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    on_next_model: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    on_prev_mic: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    on_next_mic: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    on_mic: impl Fn(&f32, &mut Window, &mut App) + 'static,
    on_dist: impl Fn(&f32, &mut Window, &mut App) + 'static,
    on_load_ir: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::AnyElement {
    let is_ir = model == rodharerist::CabModel::Ir;

    let model_label = cab_model_label(model);
    let mic_label = mic_model_label(mic_model);

    let header_trailing = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::LOOSE))
        .child(model_stepper(
            "cab-model",
            model_label.to_string(),
            on_prev_model,
            on_next_model,
        ))
        .into_any_element();

    let mic_cell = knob_cell(
        "Mic Pos",
        percent(mic),
        knob(
            "cab-mic",
            mic,
            0.0,
            100.0,
            Colors::accent_primary(),
            None,
            on_mic,
        ),
    );
    let dist_cell = knob_cell(
        "Distance",
        percent(dist),
        knob(
            "cab-dist",
            dist,
            0.0,
            100.0,
            Colors::accent_primary(),
            None,
            on_dist,
        ),
    );

    let mic_cell = if is_ir {
        div().opacity(0.4).child(mic_cell).into_any_element()
    } else {
        mic_cell
    };
    let dist_cell = if is_ir {
        div().opacity(0.4).child(dist_cell).into_any_element()
    } else {
        dist_cell
    };

    let mut body = card()
        .child(card_header("CABINET / IR", Some(header_trailing)))
        .child(fb_checkbox("cab-on", "Enabled", on, true, on_toggle))
        .child(
            knobs_row()
                .child(mic_cell)
                .child(dist_cell)
                .child(knob_cell(
                    "Mic Type",
                    mic_label.to_string(),
                    model_stepper("cab-mic-model", String::new(), on_prev_mic, on_next_mic),
                )),
        );

    if is_ir {
        let loaded_label = ir_loaded.map(|(name, secs)| format!("{name} ({secs:.2}s)"));
        body = body.child(load_block(
            "cab-load-ir",
            "Load IR…",
            ir_loading,
            ir_error,
            loaded_label,
            on_load_ir,
        ));
    }

    body.into_any_element()
}

fn cab_model_label(model: rodharerist::CabModel) -> &'static str {
    match model {
        rodharerist::CabModel::Vintage4x12 => "1960v Vintage 4x12",
        rodharerist::CabModel::American2x12 => "American 2x12",
        rodharerist::CabModel::Tweed1x12 => "Tweed 1x12",
        rodharerist::CabModel::Modern4x12 => "Modern 4x12",
        rodharerist::CabModel::OpenBack => "Open Back",
        rodharerist::CabModel::Vintage2x12 => "Vintage 2x12",
        rodharerist::CabModel::Oversized4x12 => "Oversized 4x12",
        rodharerist::CabModel::BassCabinet => "Bass Cabinet",
        rodharerist::CabModel::Brit4x12 => "British Stack 4x12",
        rodharerist::CabModel::Uber4x12 => "Uberkab 4x12",
        rodharerist::CabModel::Slo4x12 => "SLO Custom 4x12",
        rodharerist::CabModel::Ir => "Impulse Response (loaded file)",
        rodharerist::CabModel::Modern2x12 => "Modern 2x12",
        rodharerist::CabModel::American1x12 => "American 1x12 Combo",
    }
}

fn mic_model_label(model: rodharerist::MicModel) -> &'static str {
    match model {
        rodharerist::MicModel::Dynamic => "Dynamic",
        rodharerist::MicModel::Ribbon => "Ribbon",
        rodharerist::MicModel::Condenser => "Condenser",
    }
}

// ── NAM Capture card ─────────────────────────────────────────────────────────

/// NAM Capture card: input/output trim (bipolar, centred on 0 dB), mix,
/// loudness normalization toggle, quality ("slim size") knob, and the
/// capture load block. Rendered unconditionally — the caller decides whether
/// to show or gray it out based on the Amp card's tone engine selection.
#[allow(clippy::too_many_arguments)]
pub(crate) fn nam_card(
    input_trim_db: f32,
    output_trim_db: f32,
    mix: f32,
    loudness_norm: bool,
    slim_size: f32,
    captured: Option<String>,
    loading: bool,
    error: Option<String>,
    on_input_trim: impl Fn(&f32, &mut Window, &mut App) + 'static,
    on_output_trim: impl Fn(&f32, &mut Window, &mut App) + 'static,
    on_mix: impl Fn(&f32, &mut Window, &mut App) + 'static,
    on_toggle_loudness: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    on_slim_size: impl Fn(&f32, &mut Window, &mut App) + 'static,
    on_load: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::AnyElement {
    card()
        .child(card_header("NAM CAPTURE", None))
        .child(
            knobs_row()
                .child(knob_cell(
                    "In Trim",
                    db(input_trim_db),
                    knob_bipolar(
                        "nam-input-trim",
                        input_trim_db,
                        -24.0,
                        24.0,
                        KNOB_SIZE,
                        Colors::accent_primary(),
                        None,
                        0.0,
                        on_input_trim,
                    ),
                ))
                .child(knob_cell(
                    "Out Trim",
                    db(output_trim_db),
                    knob_bipolar(
                        "nam-output-trim",
                        output_trim_db,
                        -24.0,
                        24.0,
                        KNOB_SIZE,
                        Colors::accent_primary(),
                        None,
                        0.0,
                        on_output_trim,
                    ),
                ))
                .child(knob_cell(
                    "Mix",
                    percent(mix),
                    knob(
                        "nam-mix",
                        mix,
                        0.0,
                        100.0,
                        Colors::accent_primary(),
                        None,
                        on_mix,
                    ),
                ))
                .child(knob_cell(
                    "Quality",
                    percent(slim_size),
                    knob(
                        "nam-slim-size",
                        slim_size,
                        0.0,
                        100.0,
                        Colors::accent_primary(),
                        None,
                        on_slim_size,
                    ),
                )),
        )
        .child(fb_checkbox(
            "nam-loudness-norm",
            "Loudness Norm",
            loudness_norm,
            true,
            on_toggle_loudness,
        ))
        .child(load_block(
            "nam-load",
            "Load Capture…",
            loading,
            error,
            captured,
            on_load,
        ))
        .into_any_element()
}

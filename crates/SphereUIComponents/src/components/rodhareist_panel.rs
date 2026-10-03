//! Rodhareist's native editor — the shell that composes every card.
//!
//! Owns no state of its own: [`rodhareist_panel`] takes the editor window
//! (for its params and the `knob_cb`/`click_cb` closure builders) and lays
//! out, top to bottom:
//!
//! 1. the header — power, global input/output trim, a clear-clip button;
//! 2. the signal chain rack — the order stages actually run in
//!    (`Params::stage_order`), with up/down/remove per slot and a row of
//!    "add" buttons for stages not yet in the chain;
//! 3. every stage's card, built by the `rodhareist_panel_*` modules.
//!
//! The rack and the cards are deliberately independent: a stage can sit in
//! the chain bypassed (its own `*_on` flag off) or have its knobs edited
//! while not in the chain at all — exactly like unplugging a pedal rather
//! than stepping on it.

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, AnyElement, App, Context, InteractiveElement, IntoElement, ParentElement,
    StatefulInteractiveElement, Styled, Window,
};
use rodharerist::{AmpModel, CabModel, DelayModel, DriveModel, ModModel, Params, StageKind, WahModel};

use crate::components::controls::{fb_badge, fb_button, fb_stepper_button, FbButtonKind};
use crate::components::knob::knob_bipolar;
use crate::components::rodhareist_panel_cab_capture::{cab_card, nam_card};
use crate::components::rodhareist_panel_drive_amp::{amp_card, drive_card};
use crate::components::rodhareist_panel_dynamics::{comp_card, eq_card, gate_card};
use crate::components::rodhareist_panel_modulation::{mod_card, wah_card};
use crate::components::rodhareist_panel_timefx::{delay_card, reverb_card};
use crate::components::rodhareist_window::RodhareistEditorWindow;
use crate::theme::{radius, space, typography, Colors};

const KNOB_SIZE: f32 = 34.0;

fn stage_label(kind: StageKind) -> &'static str {
    match kind {
        StageKind::Gate => "Gate",
        StageKind::Drive => "Drive A",
        StageKind::Amp => "Amp",
        StageKind::Mod => "Mod A",
        StageKind::Delay => "Delay A",
        StageKind::Reverb => "Reverb",
        StageKind::Cab => "Cabinet",
        StageKind::Comp => "Comp A",
        StageKind::Eq => "EQ A",
        StageKind::Wah => "Wah",
        StageKind::Drive2 => "Drive B",
        StageKind::Mod2 => "Mod B",
        StageKind::Delay2 => "Delay B",
        StageKind::Eq2 => "EQ B",
        StageKind::Comp2 => "Comp B",
    }
}

/// Every stage not currently in `order`, in `StageKind::ALL` order — what
/// the "add" row offers.
fn stages_not_in_chain(order: &[Option<StageKind>]) -> Vec<StageKind> {
    StageKind::ALL
        .iter()
        .copied()
        .filter(|kind| !order.iter().any(|slot| *slot == Some(*kind)))
        .collect()
}

fn header(window: &RodhareistEditorWindow, params: &Params, cx: &Context<RodhareistEditorWindow>) -> AnyElement {
    let power_on = params.power;
    div()
        .flex()
        .flex_row()
        .items_center()
        .flex_wrap()
        .gap(px(space::LOOSE))
        .p(px(space::LOOSE))
        .border_b(px(1.0))
        .border_color(Colors::border_subtle())
        .child(fb_button(
            "rodhareist-power",
            if power_on { "On" } else { "Bypassed" },
            if power_on {
                FbButtonKind::Primary
            } else {
                FbButtonKind::Default
            },
            true,
            window.click_cb(cx, |p| p.power = !p.power),
        ))
        .child(trim_knob(
            "rodhareist-input-trim",
            "Input",
            params.input_trim_db,
            window.knob_cb(cx, |p, v| p.input_trim_db = v),
        ))
        .child(trim_knob(
            "rodhareist-output-trim",
            "Output",
            params.output_trim_db,
            window.knob_cb(cx, |p, v| p.output_trim_db = v),
        ))
        .child(div().flex_1())
        .child(fb_button(
            "rodhareist-clear-clip",
            "Clear Clip",
            FbButtonKind::Ghost,
            true,
            {
                let entity = cx.entity().clone();
                move |_event, _window, app: &mut App| {
                    let _ = entity.update(app, |this, cx| {
                        if let (Some(forward), Some(index)) = (
                            this_host_forward(this),
                            rodharerist::ui_param_index("clear_clip"),
                        ) {
                            forward(this.key(), index, 1.0, cx);
                        }
                    });
                }
            },
        ))
        .into_any_element()
}

/// `forward_param`, read through the window without borrowing it mutably —
/// a tiny indirection so the clear-clip click above can call it from inside
/// an `entity.update` closure that already holds `&mut this`.
fn this_host_forward(
    this: &RodhareistEditorWindow,
) -> Option<crate::components::builtin_plugin_editor_window::BuiltinParamForwarder> {
    this.host_ops_forward_param()
}

fn trim_knob(
    id: &'static str,
    label: &'static str,
    value: f32,
    on_change: impl Fn(&f32, &mut Window, &mut App) + 'static,
) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .items_center()
        .gap(px(space::HAIR))
        .child(knob_bipolar(
            id,
            value,
            -24.0,
            24.0,
            KNOB_SIZE,
            Colors::accent_primary(),
            None,
            0.0,
            on_change,
        ))
        .child(
            div()
                .text_size(px(typography::DENSE_CAPTION))
                .text_color(Colors::text_muted())
                .child(format!("{label} {value:+.1} dB")),
        )
        .into_any_element()
}

fn rack(window: &RodhareistEditorWindow, params: &Params, cx: &Context<RodhareistEditorWindow>) -> AnyElement {
    let order = params.stage_order;
    let slots: Vec<(usize, StageKind)> = order
        .iter()
        .enumerate()
        .filter_map(|(index, slot)| slot.map(|kind| (index, kind)))
        .collect();

    let mut row = div()
        .flex()
        .flex_row()
        .flex_wrap()
        .items_center()
        .gap(px(space::SNUG));
    for (position, (slot_index, kind)) in slots.iter().enumerate() {
        let slot_index = *slot_index;
        let can_move_left = position > 0;
        let can_move_right = position + 1 < slots.len();
        let left_target = slots.get(position.wrapping_sub(1)).map(|(i, _)| *i);
        let right_target = slots.get(position + 1).map(|(i, _)| *i);
        row = row.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::HAIR))
                .px(px(space::SNUG))
                .py(px(space::HAIR))
                .rounded(px(radius::CONTROL))
                .border(px(1.0))
                .border_color(Colors::border_subtle())
                .bg(Colors::surface_card())
                .child(fb_badge((position + 1).to_string(), Colors::text_faint()))
                .child(
                    div()
                        .text_size(px(typography::DENSE_CAPTION))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(Colors::text_primary())
                        .child(stage_label(*kind)),
                )
                .when_some(left_target.filter(|_| can_move_left), |el, other| {
                    el.child(fb_stepper_button(
                        ("rodhareist-rack-left", slot_index),
                        "\u{2039}",
                        window.click_cb(cx, move |p| {
                            p.stage_order.swap(slot_index, other);
                        }),
                    ))
                })
                .when_some(right_target.filter(|_| can_move_right), |el, other| {
                    el.child(fb_stepper_button(
                        ("rodhareist-rack-right", slot_index),
                        "\u{203a}",
                        window.click_cb(cx, move |p| {
                            p.stage_order.swap(slot_index, other);
                        }),
                    ))
                })
                .child(fb_stepper_button(
                    ("rodhareist-rack-remove", slot_index),
                    "\u{d7}",
                    window.click_cb(cx, move |p| {
                        p.stage_order[slot_index] = None;
                    }),
                )),
        );
    }

    let missing = stages_not_in_chain(&order);
    let mut add_row = div()
        .flex()
        .flex_row()
        .flex_wrap()
        .items_center()
        .gap(px(space::TIGHT));
    for kind in missing {
        add_row = add_row.child(fb_button(
            ("rodhareist-rack-add", kind as usize),
            format!("+ {}", stage_label(kind)),
            FbButtonKind::Ghost,
            true,
            window.click_cb(cx, move |p| {
                if let Some(slot) = p.stage_order.iter_mut().find(|slot| slot.is_none()) {
                    *slot = Some(kind);
                }
            }),
        ));
    }

    div()
        .flex()
        .flex_col()
        .gap(px(space::SNUG))
        .p(px(space::LOOSE))
        .border_b(px(1.0))
        .border_color(Colors::border_subtle())
        .child(
            div()
                .text_size(px(typography::DENSE_CAPTION))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(Colors::text_faint())
                .child("SIGNAL CHAIN"),
        )
        .child(row)
        .child(add_row)
        .into_any_element()
}

/// Every stage's card, in a wrapped grid below the rack. Always shown
/// regardless of chain membership, so a stage's knobs stay editable while it
/// is unplugged.
fn cards(window: &RodhareistEditorWindow, p: &Params, cx: &Context<RodhareistEditorWindow>) -> AnyElement {
    let b = &p.stage_b;
    div()
        .flex()
        .flex_row()
        .flex_wrap()
        .items_start()
        .gap(px(space::LOOSE))
        .p(px(space::LOOSE))
        .child(gate_card(
            p.gate_on,
            p.gate_thresh_db,
            window.click_cb(cx, |p| p.gate_on = !p.gate_on),
            window.knob_cb(cx, |p, v| p.gate_thresh_db = v),
        ))
        .child(comp_card(
            "Compressor A",
            "comp-a",
            p.comp_on,
            p.comp_thresh_db,
            p.comp_ratio,
            p.comp_attack_ms,
            p.comp_release_ms,
            p.comp_makeup_db,
            window.click_cb(cx, |p| p.comp_on = !p.comp_on),
            window.knob_cb(cx, |p, v| p.comp_thresh_db = v),
            window.knob_cb(cx, |p, v| p.comp_ratio = v),
            window.knob_cb(cx, |p, v| p.comp_attack_ms = v),
            window.knob_cb(cx, |p, v| p.comp_release_ms = v),
            window.knob_cb(cx, |p, v| p.comp_makeup_db = v),
        ))
        .child(comp_card(
            "Compressor B",
            "comp-b",
            b.comp_on,
            b.comp_thresh_db,
            b.comp_ratio,
            b.comp_attack_ms,
            b.comp_release_ms,
            b.comp_makeup_db,
            window.click_cb(cx, |p| p.stage_b.comp_on = !p.stage_b.comp_on),
            window.knob_cb(cx, |p, v| p.stage_b.comp_thresh_db = v),
            window.knob_cb(cx, |p, v| p.stage_b.comp_ratio = v),
            window.knob_cb(cx, |p, v| p.stage_b.comp_attack_ms = v),
            window.knob_cb(cx, |p, v| p.stage_b.comp_release_ms = v),
            window.knob_cb(cx, |p, v| p.stage_b.comp_makeup_db = v),
        ))
        .child(drive_card(
            "Drive A",
            "drive-a",
            p.drive_on,
            p.drive_model,
            p.drive_gain,
            p.drive_tone,
            p.drive_level,
            window.click_cb(cx, |p| p.drive_on = !p.drive_on),
            window.click_cb(cx, |p| p.drive_model = step_drive_model(p.drive_model, -1)),
            window.click_cb(cx, |p| p.drive_model = step_drive_model(p.drive_model, 1)),
            window.knob_cb(cx, |p, v| p.drive_gain = v),
            window.knob_cb(cx, |p, v| p.drive_tone = v),
            window.knob_cb(cx, |p, v| p.drive_level = v),
        ))
        .child(drive_card(
            "Drive B",
            "drive-b",
            b.drive_on,
            b.drive_model,
            b.drive_gain,
            b.drive_tone,
            b.drive_level,
            window.click_cb(cx, |p| p.stage_b.drive_on = !p.stage_b.drive_on),
            window.click_cb(cx, |p| {
                p.stage_b.drive_model = step_drive_model(p.stage_b.drive_model, -1)
            }),
            window.click_cb(cx, |p| {
                p.stage_b.drive_model = step_drive_model(p.stage_b.drive_model, 1)
            }),
            window.knob_cb(cx, |p, v| p.stage_b.drive_gain = v),
            window.knob_cb(cx, |p, v| p.stage_b.drive_tone = v),
            window.knob_cb(cx, |p, v| p.stage_b.drive_level = v),
        ))
        .child(amp_card(
            p.amp_on,
            p.amp_model,
            p.tone_engine,
            p.amp_gain,
            p.amp_bass,
            p.amp_middle,
            p.amp_treble,
            p.amp_presence,
            p.amp_master,
            window.click_cb(cx, |p| p.amp_on = !p.amp_on),
            window.click_cb(cx, |p| p.amp_model = step_amp_model(p.amp_model, -1)),
            window.click_cb(cx, |p| p.amp_model = step_amp_model(p.amp_model, 1)),
            window.click_cb(cx, |p| p.tone_engine = step_tone_engine(p.tone_engine, -1)),
            window.click_cb(cx, |p| p.tone_engine = step_tone_engine(p.tone_engine, 1)),
            window.knob_cb(cx, |p, v| p.amp_gain = v),
            window.knob_cb(cx, |p, v| p.amp_bass = v),
            window.knob_cb(cx, |p, v| p.amp_middle = v),
            window.knob_cb(cx, |p, v| p.amp_treble = v),
            window.knob_cb(cx, |p, v| p.amp_presence = v),
            window.knob_cb(cx, |p, v| p.amp_master = v),
        ))
        .child(cab_card(
            p.cab_on,
            p.cab_model,
            p.mic_model,
            p.cab_mic,
            p.cab_dist,
            window.ir_loaded_info(),
            window.ir_loading(),
            window.ir_error(),
            window.click_cb(cx, |p| p.cab_on = !p.cab_on),
            window.click_cb(cx, |p| p.cab_model = step_cab_model(p.cab_model, -1)),
            window.click_cb(cx, |p| p.cab_model = step_cab_model(p.cab_model, 1)),
            window.click_cb(cx, |p| p.mic_model = step_mic_model(p.mic_model, -1)),
            window.click_cb(cx, |p| p.mic_model = step_mic_model(p.mic_model, 1)),
            window.knob_cb(cx, |p, v| p.cab_mic = v),
            window.knob_cb(cx, |p, v| p.cab_dist = v),
            window.load_ir_cb(cx),
        ))
        .child(nam_card(
            p.nam_input_trim_db,
            p.nam_output_trim_db,
            p.nam_mix,
            p.nam_loudness_norm,
            p.nam_slim_size,
            window.nam_loaded_info(),
            window.nam_loading(),
            window.nam_error(),
            window.knob_cb(cx, |p, v| p.nam_input_trim_db = v),
            window.knob_cb(cx, |p, v| p.nam_output_trim_db = v),
            window.knob_cb(cx, |p, v| p.nam_mix = v),
            window.click_cb(cx, |p| p.nam_loudness_norm = !p.nam_loudness_norm),
            window.knob_cb(cx, |p, v| p.nam_slim_size = v),
            window.load_nam_cb(cx),
        ))
        .child(eq_card(
            "EQ A",
            "eq-a",
            p.eq_on,
            p.eq_model,
            p.eq_low_gain_db,
            p.eq_mid1_freq_hz,
            p.eq_mid1_gain_db,
            p.eq_mid2_freq_hz,
            p.eq_mid2_gain_db,
            p.eq_high_gain_db,
            window.click_cb(cx, |p| p.eq_on = !p.eq_on),
            window.click_cb(cx, |p| p.eq_model = step_eq_model(p.eq_model, -1)),
            window.click_cb(cx, |p| p.eq_model = step_eq_model(p.eq_model, 1)),
            window.knob_cb(cx, |p, v| p.eq_low_gain_db = v),
            window.knob_cb(cx, |p, v| p.eq_mid1_freq_hz = v),
            window.knob_cb(cx, |p, v| p.eq_mid1_gain_db = v),
            window.knob_cb(cx, |p, v| p.eq_mid2_freq_hz = v),
            window.knob_cb(cx, |p, v| p.eq_mid2_gain_db = v),
            window.knob_cb(cx, |p, v| p.eq_high_gain_db = v),
        ))
        .child(eq_card(
            "EQ B",
            "eq-b",
            b.eq_on,
            b.eq_model,
            b.eq_low_gain_db,
            b.eq_mid1_freq_hz,
            b.eq_mid1_gain_db,
            b.eq_mid2_freq_hz,
            b.eq_mid2_gain_db,
            b.eq_high_gain_db,
            window.click_cb(cx, |p| p.stage_b.eq_on = !p.stage_b.eq_on),
            window.click_cb(cx, |p| p.stage_b.eq_model = step_eq_model(p.stage_b.eq_model, -1)),
            window.click_cb(cx, |p| p.stage_b.eq_model = step_eq_model(p.stage_b.eq_model, 1)),
            window.knob_cb(cx, |p, v| p.stage_b.eq_low_gain_db = v),
            window.knob_cb(cx, |p, v| p.stage_b.eq_mid1_freq_hz = v),
            window.knob_cb(cx, |p, v| p.stage_b.eq_mid1_gain_db = v),
            window.knob_cb(cx, |p, v| p.stage_b.eq_mid2_freq_hz = v),
            window.knob_cb(cx, |p, v| p.stage_b.eq_mid2_gain_db = v),
            window.knob_cb(cx, |p, v| p.stage_b.eq_high_gain_db = v),
        ))
        .child(mod_card(
            "Mod A",
            "mod-a",
            p.mod_on,
            p.mod_model,
            p.chorus_rate,
            p.chorus_depth,
            p.chorus_mix,
            window.click_cb(cx, |p| p.mod_on = !p.mod_on),
            window.click_cb(cx, |p| p.mod_model = step_mod_model(p.mod_model, -1)),
            window.click_cb(cx, |p| p.mod_model = step_mod_model(p.mod_model, 1)),
            window.knob_cb(cx, |p, v| p.chorus_rate = v),
            window.knob_cb(cx, |p, v| p.chorus_depth = v),
            window.knob_cb(cx, |p, v| p.chorus_mix = v),
        ))
        .child(mod_card(
            "Mod B",
            "mod-b",
            b.mod_on,
            b.mod_model,
            b.chorus_rate,
            b.chorus_depth,
            b.chorus_mix,
            window.click_cb(cx, |p| p.stage_b.mod_on = !p.stage_b.mod_on),
            window.click_cb(cx, |p| p.stage_b.mod_model = step_mod_model(p.stage_b.mod_model, -1)),
            window.click_cb(cx, |p| p.stage_b.mod_model = step_mod_model(p.stage_b.mod_model, 1)),
            window.knob_cb(cx, |p, v| p.stage_b.chorus_rate = v),
            window.knob_cb(cx, |p, v| p.stage_b.chorus_depth = v),
            window.knob_cb(cx, |p, v| p.stage_b.chorus_mix = v),
        ))
        .child(wah_card(
            p.wah_on,
            p.wah_model,
            p.wah_pos,
            p.wah_res,
            p.wah_sens,
            window.click_cb(cx, |p| p.wah_on = !p.wah_on),
            window.click_cb(cx, |p| p.wah_model = step_wah_model(p.wah_model, -1)),
            window.click_cb(cx, |p| p.wah_model = step_wah_model(p.wah_model, 1)),
            window.knob_cb(cx, |p, v| p.wah_pos = v),
            window.knob_cb(cx, |p, v| p.wah_res = v),
            window.knob_cb(cx, |p, v| p.wah_sens = v),
        ))
        .child(delay_card(
            "Delay A",
            "delay-a",
            p.delay_on,
            p.delay_model,
            p.delay_time_ms,
            p.delay_fb,
            p.delay_mix,
            p.delay_tone,
            window.click_cb(cx, |p| p.delay_on = !p.delay_on),
            window.click_cb(cx, |p| p.delay_model = step_delay_model(p.delay_model, -1)),
            window.click_cb(cx, |p| p.delay_model = step_delay_model(p.delay_model, 1)),
            window.knob_cb(cx, |p, v| p.delay_time_ms = v),
            window.knob_cb(cx, |p, v| p.delay_fb = v),
            window.knob_cb(cx, |p, v| p.delay_mix = v),
            window.knob_cb(cx, |p, v| p.delay_tone = v),
        ))
        .child(delay_card(
            "Delay B",
            "delay-b",
            b.delay_on,
            b.delay_model,
            b.delay_time_ms,
            b.delay_fb,
            b.delay_mix,
            b.delay_tone,
            window.click_cb(cx, |p| p.stage_b.delay_on = !p.stage_b.delay_on),
            window.click_cb(cx, |p| {
                p.stage_b.delay_model = step_delay_model(p.stage_b.delay_model, -1)
            }),
            window.click_cb(cx, |p| {
                p.stage_b.delay_model = step_delay_model(p.stage_b.delay_model, 1)
            }),
            window.knob_cb(cx, |p, v| p.stage_b.delay_time_ms = v),
            window.knob_cb(cx, |p, v| p.stage_b.delay_fb = v),
            window.knob_cb(cx, |p, v| p.stage_b.delay_mix = v),
            window.knob_cb(cx, |p, v| p.stage_b.delay_tone = v),
        ))
        .child(reverb_card(
            p.reverb_on,
            p.reverb_model,
            p.reverb_decay_s,
            p.reverb_mix,
            p.reverb_shimmer,
            window.click_cb(cx, |p| p.reverb_on = !p.reverb_on),
            window.click_cb(cx, |p| p.reverb_model = step_reverb_model(p.reverb_model, -1)),
            window.click_cb(cx, |p| p.reverb_model = step_reverb_model(p.reverb_model, 1)),
            window.knob_cb(cx, |p, v| p.reverb_decay_s = v),
            window.knob_cb(cx, |p, v| p.reverb_mix = v),
            window.knob_cb(cx, |p, v| p.reverb_shimmer = v),
        ))
        .into_any_element()
}

fn step_drive_model(current: DriveModel, delta: i32) -> DriveModel {
    step_enum(DriveModel::ALL, current, delta)
}
fn step_amp_model(current: AmpModel, delta: i32) -> AmpModel {
    step_enum(AmpModel::ALL, current, delta)
}
fn step_cab_model(current: CabModel, delta: i32) -> CabModel {
    step_enum(CabModel::ALL, current, delta)
}
fn step_mic_model(current: rodharerist::MicModel, delta: i32) -> rodharerist::MicModel {
    step_enum(rodharerist::MicModel::ALL, current, delta)
}
fn step_mod_model(current: ModModel, delta: i32) -> ModModel {
    step_enum(ModModel::ALL, current, delta)
}
fn step_wah_model(current: WahModel, delta: i32) -> WahModel {
    step_enum(WahModel::ALL, current, delta)
}
fn step_eq_model(current: rodharerist::EqModel, delta: i32) -> rodharerist::EqModel {
    step_enum(rodharerist::EqModel::ALL, current, delta)
}
fn step_reverb_model(current: rodharerist::ReverbModel, delta: i32) -> rodharerist::ReverbModel {
    step_enum(rodharerist::ReverbModel::ALL, current, delta)
}
fn step_delay_model(current: DelayModel, delta: i32) -> DelayModel {
    step_enum(DelayModel::ALL, current, delta)
}
fn step_tone_engine(
    current: rodharerist::ToneEngineKind,
    delta: i32,
) -> rodharerist::ToneEngineKind {
    const ALL: [rodharerist::ToneEngineKind; 3] = [
        rodharerist::ToneEngineKind::Classic,
        rodharerist::ToneEngineKind::NamCapture,
        rodharerist::ToneEngineKind::Bypass,
    ];
    step_enum(&ALL, current, delta)
}

fn step_enum<T: Copy + PartialEq>(all: &[T], current: T, delta: i32) -> T {
    let len = all.len() as i32;
    if len == 0 {
        return current;
    }
    let index = all.iter().position(|v| *v == current).unwrap_or(0) as i32;
    let next = ((index + delta) % len + len) % len;
    all[next as usize]
}

/// The whole editor body: header, rack, cards — scrollable, since the full
/// rig does not fit a modest window at once.
pub(crate) fn rodhareist_panel(
    window: &RodhareistEditorWindow,
    cx: &Context<RodhareistEditorWindow>,
) -> AnyElement {
    let params = window.params_snapshot();
    div()
        .id("rodhareist-panel-scroll")
        .flex()
        .flex_col()
        .size_full()
        .overflow_y_scroll()
        .child(header(window, &params, cx))
        .child(rack(window, &params, cx))
        .child(cards(window, &params, cx))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stages_not_in_chain_excludes_every_placed_kind() {
        let mut order = [None; rodharerist::PATH_SLOTS];
        order[0] = Some(StageKind::Gate);
        order[1] = Some(StageKind::Amp);
        let missing = stages_not_in_chain(&order);
        assert!(!missing.contains(&StageKind::Gate));
        assert!(!missing.contains(&StageKind::Amp));
        assert!(missing.contains(&StageKind::Wah));
        assert_eq!(missing.len(), StageKind::COUNT - 2);
    }

    #[test]
    fn step_enum_wraps_both_directions() {
        assert_eq!(step_drive_model(DriveModel::Screamer, -1), DriveModel::CopperFuzz);
        assert_eq!(step_drive_model(DriveModel::CopperFuzz, 1), DriveModel::Screamer);
    }
}

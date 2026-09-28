//! Effect Editor bottom-tab content backed by the selected track's real insert chain.

use gpui::{
    canvas, div, px, App, Context, Entity, InteractiveElement, IntoElement, ParentElement, Render,
    StatefulInteractiveElement, Styled, Window,
};

use crate::components::slider::slider;
use crate::components::timeline::timeline::Timeline;
use crate::components::timeline::timeline_state::{
    InsertLoadStatus, InsertPluginFormat, InsertSlotState, TrackState,
};
use crate::layout::StudioLayout;
use crate::theme::Colors;

pub struct EffectEditorTabView {
    owner: Entity<StudioLayout>,
    timeline: Entity<Timeline>,
    selected_insert_id: Option<String>,
}

impl EffectEditorTabView {
    pub fn new(owner: Entity<StudioLayout>, timeline: Entity<Timeline>) -> Self {
        Self {
            owner,
            timeline,
            selected_insert_id: None,
        }
    }
}

impl Render for EffectEditorTabView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _scope = crate::perf::PerfScope::enter("BottomPanelEffectEditor");

        let owner_entity = self.owner.clone();
        let callbacks = self
            .owner
            .read(cx)
            .build_mixer_callbacks(owner_entity.clone(), cx);
        let (selected_track_id, selected_track) = {
            let timeline = self.timeline.read(cx);
            let selected_id = timeline.state.selection.selected_track_id.clone();
            let track = selected_id
                .as_ref()
                .and_then(|id| timeline.state.tracks.iter().find(|track| &track.id == id))
                .cloned();
            (selected_id, track)
        };

        if let Some(track) = selected_track.as_ref() {
            if self
                .selected_insert_id
                .as_ref()
                .is_some_and(|id| !track.inserts.iter().any(|slot| &slot.id == id))
            {
                self.selected_insert_id = None;
            }
            if self.selected_insert_id.is_none() {
                self.selected_insert_id = track
                    .inserts
                    .iter()
                    .find(|slot| !slot.is_empty())
                    .map(|slot| slot.id.clone());
            }
        } else {
            self.selected_insert_id = None;
        }

        let selected_insert_id = self.selected_insert_id.clone();
        let selected_insert = selected_track.as_ref().and_then(|track| {
            selected_insert_id
                .as_ref()
                .and_then(|id| track.inserts.iter().find(|slot| &slot.id == id))
                .cloned()
        });

        div()
            .flex()
            .flex_row()
            .items_start()
            .size_full()
            .relative()
            .child(effect_editor_background())
            .child(match (selected_track_id, selected_track) {
                (Some(track_id), Some(track)) => effect_editor_content(
                    owner_entity,
                    track_id,
                    track,
                    selected_insert_id,
                    selected_insert,
                    callbacks,
                    cx.entity(),
                )
                .into_any_element(),
                _ => no_track_state().into_any_element(),
            })
    }
}

fn effect_editor_background() -> impl IntoElement {
    canvas(
        |_bounds, _window, _cx| {},
        |bounds, (), window, _cx| {
            window.paint_quad(gpui::fill(bounds, Colors::surface_base()));
        },
    )
    .absolute()
    .inset_0()
}

fn effect_editor_content(
    owner: Entity<StudioLayout>,
    track_id: String,
    track: TrackState,
    selected_insert_id: Option<String>,
    selected_insert: Option<InsertSlotState>,
    callbacks: crate::components::mixer_panel::MixerCallbacks,
    view: Entity<EffectEditorTabView>,
) -> impl IntoElement {
    let add_track_id = track_id.clone();
    let add_cb = callbacks.on_add_insert.clone();
    let mut rack = div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .overflow_hidden()
        .min_w(px(0.0))
        .flex_1();

    if track.inserts.iter().filter(|slot| !slot.is_empty()).count() == 0 {
        rack = rack.child(empty_chain_state(add_track_id.clone(), add_cb.clone()));
    } else {
        for (index, slot) in track.inserts.iter().enumerate() {
            if slot.is_empty() {
                continue;
            }
            rack = rack.child(device_card(
                track_id.clone(),
                index,
                slot.clone(),
                selected_insert_id.as_deref() == Some(slot.id.as_str()),
                callbacks.clone(),
                view.clone(),
            ));
        }
        rack = rack.child(add_device_card(add_track_id, add_cb));
    }

    div()
        .flex()
        .flex_row()
        .size_full()
        .px(px(12.0))
        .py(px(10.0))
        .gap_3()
        .child(rack)
        .child(parameter_panel(owner, track_id, selected_insert, callbacks))
}

fn no_track_state() -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .justify_center()
        .size_full()
        .text_size(px(11.0))
        .text_color(Colors::text_muted())
        .child("Select a track to edit inserts")
}

fn empty_chain_state(
    track_id: String,
    add_cb: std::sync::Arc<dyn Fn(&String, &mut Window, &mut App) + 'static>,
) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_2()
        .w_full()
        .h_full()
        .text_size(px(11.0))
        .text_color(Colors::text_muted())
        .child("No insert plugins on selected track")
        .child(add_device_button(track_id, add_cb))
}

fn add_device_card(
    track_id: String,
    add_cb: std::sync::Arc<dyn Fn(&String, &mut Window, &mut App) + 'static>,
) -> impl IntoElement {
    div()
        .w(px(132.0))
        .h(px(82.0))
        .rounded(px(crate::theme::radius::CONTROL))
        .border(px(1.0))
        .border_color(Colors::panel_border())
        .border_dashed()
        .flex()
        .items_center()
        .justify_center()
        .text_color(Colors::text_muted())
        .text_size(px(10.0))
        .id("effect-add-device-card")
        .on_click(move |_, window, cx| {
            eprintln!("[plugin-picker] opened from effect editor track={track_id}");
            add_cb(&track_id, window, cx);
        })
        .child("+ Add Device")
}

fn add_device_button(
    track_id: String,
    add_cb: std::sync::Arc<dyn Fn(&String, &mut Window, &mut App) + 'static>,
) -> impl IntoElement {
    div()
        .px(px(10.0))
        .py(px(5.0))
        .rounded(px(crate::theme::radius::CONTROL))
        .bg(Colors::surface_hover())
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .text_color(Colors::text_primary())
        .id("effect-add-device-button")
        .on_click(move |_, window, cx| {
            eprintln!("[plugin-picker] opened from effect editor track={track_id}");
            add_cb(&track_id, window, cx);
        })
        .child("Add Device")
}

fn device_card(
    track_id: String,
    slot_index: usize,
    slot: InsertSlotState,
    selected: bool,
    callbacks: crate::components::mixer_panel::MixerCallbacks,
    view: Entity<EffectEditorTabView>,
) -> impl IntoElement {
    let select_id = slot.id.clone();
    let bypass_pair = (track_id.clone(), slot.id.clone());
    let open_target = (track_id, slot_index, slot.id.clone());
    let bypass = callbacks.on_toggle_insert_bypass.clone();
    let open = callbacks.on_open_insert_editor.clone();
    let status = plugin_state_label(&slot);

    div()
        .w(px(148.0))
        .h(px(96.0))
        .rounded(px(crate::theme::radius::CONTROL))
        .bg(if selected {
            Colors::surface_selected()
        } else {
            Colors::surface_panel()
        })
        .border(px(1.0))
        .border_color(if selected {
            Colors::accent_primary()
        } else {
            Colors::border_subtle()
        })
        .px(px(8.0))
        .py(px(6.0))
        .flex_col()
        .justify_between()
        .id(gpui::SharedString::from(format!(
            "effect-device-{}",
            slot.id
        )))
        .on_click(move |_, _window, cx| {
            let selected = select_id.clone();
            let _ = view.update(cx, |view, cx| {
                view.selected_insert_id = Some(selected);
                cx.notify();
            });
        })
        .child(
            div()
                .flex()
                .flex_row()
                .items_start()
                .justify_between()
                .gap_1()
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .flex_col()
                        .gap(px(2.0))
                        .child(
                            div()
                                .truncate()
                                .text_color(Colors::text_primary())
                                .text_xs()
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .child(slot.display_name.clone()),
                        )
                        .child(
                            div()
                                .truncate()
                                .text_color(Colors::text_muted())
                                .text_size(px(9.0))
                                .child(
                                    slot.vendor
                                        .clone()
                                        .unwrap_or_else(|| "Unknown vendor".to_string()),
                                ),
                        ),
                )
                .child(format_chip(plugin_format_label(&slot))),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap_1()
                .child(
                    div()
                        .text_color(Colors::text_muted())
                        .text_size(px(9.0))
                        .truncate()
                        .child(format!("{} · {} params", status, slot.parameters.len())),
                )
                .child(
                    div()
                        .w(px(9.0))
                        .h(px(9.0))
                        .rounded(px(crate::theme::radius::CONTROL))
                        .bg(if slot.bypassed {
                            Colors::text_faint()
                        } else {
                            Colors::accent_primary()
                        })
                        .id(gpui::SharedString::from(format!(
                            "effect-bypass-{}",
                            bypass_pair.1
                        )))
                        .on_click(move |_, window, cx| {
                            eprintln!(
                                "[PluginBypass] bypass changed requested track={} insert={}",
                                bypass_pair.0, bypass_pair.1
                            );
                            bypass(&bypass_pair, window, cx);
                        }),
                ),
        )
        .child(
            div()
                .px(px(6.0))
                .py(px(3.0))
                .rounded(px(crate::theme::radius::CONTROL))
                .bg(Colors::surface_input())
                .border(px(1.0))
                .border_color(Colors::border_subtle())
                .text_size(px(9.0))
                .text_color(Colors::text_secondary())
                .id(gpui::SharedString::from(format!(
                    "effect-open-{}",
                    open_target.2
                )))
                .on_click(move |_, window, cx| {
                    eprintln!(
                        "[PluginEditor] editor open requested track={} insert={}",
                        open_target.0, open_target.2
                    );
                    open(&open_target, window, cx);
                })
                .child("Open Editor"),
        )
}

fn parameter_panel(
    owner: Entity<StudioLayout>,
    track_id: String,
    slot: Option<InsertSlotState>,
    callbacks: crate::components::mixer_panel::MixerCallbacks,
) -> impl IntoElement {
    let Some(slot) = slot else {
        return div()
            .w(px(300.0))
            .h_full()
            .rounded(px(crate::theme::radius::CONTROL))
            .border(px(1.0))
            .border_color(Colors::border_subtle())
            .bg(Colors::surface_panel())
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(11.0))
            .text_color(Colors::text_muted())
            .child("Select a device")
            .into_any_element();
    };

    let open = callbacks.on_open_insert_editor.clone();
    let slot_index = 0usize;
    let open_target = (track_id.clone(), slot_index, slot.id.clone());
    let mut params = div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .overflow_hidden()
        .min_h_0()
        .flex_1();

    let visible_params: Vec<_> = slot
        .parameters
        .iter()
        .filter(|param| !param.hidden)
        .take(24)
        .cloned()
        .collect();

    if visible_params.is_empty() {
        params = params.child(
            div()
                .text_size(px(10.0))
                .text_color(Colors::text_muted())
                .child("No generic parameters reported by PluginHost yet"),
        );
    } else {
        for param in visible_params {
            params = params.child(parameter_row(
                owner.clone(),
                track_id.clone(),
                slot.id.clone(),
                param,
            ));
        }
    }

    div()
        .w(px(320.0))
        .h_full()
        .rounded(px(crate::theme::radius::CONTROL))
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_panel())
        .p(px(10.0))
        .flex_col()
        .gap_2()
        .child(
            div()
                .flex()
                .flex_row()
                .items_start()
                .justify_between()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .flex_col()
                        .child(
                            div()
                                .truncate()
                                .text_size(px(13.0))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(Colors::text_primary())
                                .child(slot.display_name.clone()),
                        )
                        .child(
                            div()
                                .truncate()
                                .text_size(px(10.0))
                                .text_color(Colors::text_muted())
                                .child(
                                    slot.vendor
                                        .clone()
                                        .unwrap_or_else(|| "Unknown vendor".to_string()),
                                ),
                        ),
                )
                .child(format_chip(plugin_format_label(&slot))),
        )
        .child(status_line(&slot))
        .child(
            div()
                .px(px(8.0))
                .py(px(4.0))
                .rounded(px(crate::theme::radius::CONTROL))
                .bg(Colors::surface_input())
                .border(px(1.0))
                .border_color(Colors::border_subtle())
                .text_size(px(10.0))
                .text_color(Colors::text_primary())
                .id("effect-panel-open-editor")
                .on_click(move |_, window, cx| {
                    eprintln!(
                        "[PluginEditor] editor open requested track={} insert={}",
                        open_target.0, open_target.2
                    );
                    open(&open_target, window, cx);
                })
                .child("Open Editor"),
        )
        .child(
            div()
                .text_size(px(10.0))
                .font_weight(gpui::FontWeight::BOLD)
                .text_color(Colors::text_secondary())
                .child("GENERIC PARAMETERS"),
        )
        .child(params)
        .into_any_element()
}

fn parameter_row(
    owner: Entity<StudioLayout>,
    track_id: String,
    insert_id: String,
    param: crate::components::timeline::timeline_state::PluginParameterState,
) -> impl IntoElement {
    let param_id = param.id;
    let disabled = param.read_only;
    let label = if param.unit.trim().is_empty() {
        format!("{:.1}%", param.value_normalized * 100.0)
    } else {
        format!("{:.1}% {}", param.value_normalized * 100.0, param.unit)
    };

    div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap_2()
                .child(
                    div()
                        .truncate()
                        .text_size(px(10.0))
                        .text_color(if disabled {
                            Colors::text_faint()
                        } else {
                            Colors::text_secondary()
                        })
                        .child(param.name),
                )
                .child(
                    div()
                        .text_size(px(9.0))
                        .text_color(Colors::text_muted())
                        .child(label),
                ),
        )
        .child(slider(
            gpui::SharedString::from(format!("effect-param-{}-{}", insert_id, param_id)),
            param.value_normalized,
            if disabled {
                Colors::text_faint()
            } else {
                Colors::accent_primary()
            },
            move |value, _window, cx| {
                if disabled {
                    return;
                }
                let track_id = track_id.clone();
                let insert_id = insert_id.clone();
                let value = *value;
                let owner = owner.clone();
                crate::layout::StudioLayout::defer_update(&owner, cx, move |layout, cx| {
                    layout.set_insert_parameter_from_ui(track_id, insert_id, param_id, value, cx);
                });
            },
        ))
}

fn status_line(slot: &InsertSlotState) -> impl IntoElement {
    div()
        .text_size(px(10.0))
        .text_color(Colors::text_muted())
        .child(format!(
            "{} · editor {} · {} parameters",
            plugin_state_label(slot),
            if editor_available(slot) {
                "available"
            } else {
                "not reported"
            },
            slot.parameters.len()
        ))
}

fn format_chip(label: &'static str) -> impl IntoElement {
    div()
        .flex_shrink_0()
        .px(px(6.0))
        .py(px(2.0))
        .rounded(px(crate::theme::radius::CONTROL))
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_input())
        .text_size(px(9.0))
        .font_weight(gpui::FontWeight::BOLD)
        .text_color(Colors::text_secondary())
        .child(label)
}

fn plugin_format_label(slot: &InsertSlotState) -> &'static str {
    match slot.plugin_format.unwrap_or(InsertPluginFormat::Unknown) {
        InsertPluginFormat::Vst3 => "VST3",
        InsertPluginFormat::Vst2 => "VST2",
        InsertPluginFormat::Clap => "CLAP",
        InsertPluginFormat::Au => "AU",
        InsertPluginFormat::Lv2 => "LV2",
        InsertPluginFormat::Unknown => "?",
    }
}

fn plugin_state_label(slot: &InsertSlotState) -> String {
    match &slot.load_status {
        InsertLoadStatus::Empty => "Empty".to_string(),
        InsertLoadStatus::Loading => "Loading".to_string(),
        InsertLoadStatus::Ready if slot.bypassed => "Bypassed".to_string(),
        InsertLoadStatus::Ready if !slot.enabled => "Disabled".to_string(),
        InsertLoadStatus::Ready => "Ready".to_string(),
        InsertLoadStatus::Missing(message) => format!("Missing: {message}"),
        InsertLoadStatus::Failed(message) => format!("Failed: {message}"),
        InsertLoadStatus::Disabled => "Disabled".to_string(),
    }
}

fn editor_available(slot: &InsertSlotState) -> bool {
    // Every module format has a native editor the host can embed.
    matches!(
        slot.plugin_format,
        Some(InsertPluginFormat::Vst3 | InsertPluginFormat::Vst2 | InsertPluginFormat::Clap)
    ) && !slot.is_empty()
}

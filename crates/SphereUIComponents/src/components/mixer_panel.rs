//! MixerPanel — the bottom-panel mixer view.
//!
//! Layout structure:
//!
//! ```text
//! ┌─ mixer_sub_header ───────────────────────────────────────────────────┐
//! ├──────────────────────────────────────────────────────────────────────┤
//! │                                                                      │
//! │  channel_scroll_area (flex_1, overflow-x scroll)        │  master    │
//! │  ┌───────┐┌───────┐┌───────┐ ...                        │  block     │
//! │  │ strip ││ strip ││ strip │                            │ (fixed)    │
//! │  └───────┘└───────┘└───────┘                            │            │
//! │                                                          │            │
//! └──────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! * Channel strips are a horizontal flex row inside the scroll area; they
//!   never share width with the master block.
//! * The master block is pinned to the right edge and has its own bordered
//!   gutter so the empty middle (when track count is small) reads as
//!   intentional, not as floating dead space.
//! * Strip internals are a vertical stack in fixed console order — type row,
//!   inserts, sends, I/O, pan, fader bay, toggles, name plate — so a row of
//!   strips reads across as well as down. Only the two racks resize; every
//!   other row is the same height on every strip, which is what keeps all the
//!   faders in one bay and all the plates on one baseline.
//! * The track colour appears once per strip, as the name plate's fill. See
//!   [`console`] for the rest of the visual rules.

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, svg, AppContext, ClickEvent, DragMoveEvent, Entity, InteractiveElement, IntoElement,
    MouseDownEvent, ParentElement, StatefulInteractiveElement, Styled,
};
use std::collections::HashSet;

use crate::assets;
use crate::components::fader::fader_with_drag_callbacks;
use crate::components::knob::knob_bipolar;
use crate::components::mixer_render::{MixerRenderSnapshot, MixerRenderViewport, MixerStripGeom};
use crate::components::mixer_surface::{mixer_gpu_primitives_active, render_mixer_primitives};
use crate::components::mixer_tree_sidebar_view::MixerTreeSidebar;
use crate::components::panel::FxSlotDrag;
use crate::components::reorder::drop_over_highlight;
use crate::components::sidebar::BrowserDragItem;
use crate::components::timeline::timeline_state::{
    is_vsti_output_child_track_id, volume, vsti_output_bus_flat_range,
    vsti_output_bus_strip_indices, vsti_output_child_channels_for_bus_layout,
    vsti_output_child_insert_id, InsertLoadStatus, InsertSlotState, ListenMode, MasterBusState,
    MonitorBusState, SendSlotState, TrackOutputRouting, TrackState, TrackType, MASTER_TRACK_ID,
};
use crate::components::timeline::vu_meter::meter_surface_db;
use crate::i18n::I18n;
use crate::theme::{typography, Colors};

mod callbacks;
mod console;
mod drag;
mod split;
pub use callbacks::*;
use console::{type_scale, RackPlus, SlotTone, ToggleState};
use drag::{MixerScrollDrag, SendSlotDrag};
pub use split::*;

/// True when a mixer strip should paint as selected (multi-select aware).
#[inline]
fn mixer_strip_is_selected(track_id: &str, primary: Option<&str>, selected_ids: &[String]) -> bool {
    if !selected_ids.is_empty() {
        selected_ids.iter().any(|id| id == track_id)
    } else {
        primary == Some(track_id)
    }
}

/// Maximum insert slots per track. Once reached, the trailing empty "+ Add
/// Insert" slot and the INSERTS header "+" are hidden/disabled.
const MAX_INSERT_SLOTS: usize = 8;

// ─── Mixer sub-header ("Mixer  N ch") ────────────────────────────────────────

/// Height of the "Mixer  N ch" strip above the console.
pub const MIXER_SUB_HEADER_H: f32 = 30.0;

pub fn mixer_sub_header(track_count: usize, i18n: I18n) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .h(px(MIXER_SUB_HEADER_H))
        .px(px(10.0))
        .border_b(px(1.0))
        .border_color(Colors::border_default())
        .child(
            svg()
                .path(assets::ICON_SLIDERS_HORIZONTAL_PATH)
                .w(px(14.0))
                .h(px(14.0))
                .text_color(Colors::text_muted()),
        )
        .child(
            div()
                .text_size(px(typography::DENSE_LABEL))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(Colors::text_primary())
                .child(i18n.tr("mixer.title")),
        )
        .child(
            div()
                .flex()
                .items_center()
                .px(px(5.0))
                .py(px(1.0))
                .rounded(px(crate::theme::radius::CONTROL))
                .bg(Colors::button_bg())
                .border(px(1.0))
                .border_color(Colors::border_default())
                .text_size(px(typography::DENSE_CAPTION))
                .text_color(Colors::text_secondary())
                .child(format!("{} ch", track_count)),
        )
}

fn mixer_track_type_label(track_type: TrackType, i18n: I18n) -> String {
    match track_type {
        TrackType::Audio => i18n.tr("mixer.track-type.audio"),
        TrackType::Midi => i18n.tr("mixer.track-type.midi"),
        TrackType::Instrument => i18n.tr("mixer.track-type.instrument"),
        TrackType::Bus => i18n.tr("mixer.track-type.bus"),
        TrackType::Return => i18n.tr("mixer.track-type.return"),
        TrackType::Group => "GRP".to_string(),
        TrackType::Master => i18n.tr("mixer.track-type.master"),
        TrackType::Video => i18n.tr("mixer.track-type.video"),
    }
}

/// The channel toggles: M/S/R/I over PFL/AFL.
///
/// Colours are the console's, not the app's: blue mute, amber solo, red record,
/// green input. Those four are close to universal across desks and DAWs, and a
/// player glancing down a row of strips reads the colour well before the
/// letter. The app accent stays out of it — cyan already means selection here.
///
/// `solo_implied` is set only for a VSTi multi-out channel whose parent
/// instrument is soloed: the engine sounds the channel, so its S reads as
/// engaged-by-parent even though `track.solo` is clear.
fn button_row(
    track: &TrackState,
    callbacks: &MixerCallbacks,
    id_num: usize,
    solo_implied: bool,
    base: gpui::Rgba,
) -> impl IntoElement {
    let track_id = track.id.clone();
    let solo_state = if track.solo {
        ToggleState::On
    } else if solo_implied {
        ToggleState::Implied
    } else {
        ToggleState::Off
    };

    let on_mute = {
        let id = track_id.clone();
        let cb = callbacks.on_toggle_mute.clone();
        move |_: &gpui::MouseDownEvent, w: &mut gpui::Window, cx: &mut gpui::App| cb(&id, w, cx)
    };
    let on_solo = {
        let id = track_id.clone();
        let cb = callbacks.on_toggle_solo.clone();
        move |_: &gpui::MouseDownEvent, w: &mut gpui::Window, cx: &mut gpui::App| cb(&id, w, cx)
    };
    let on_arm = {
        let id = track_id.clone();
        let cb = callbacks.on_toggle_arm.clone();
        move |_: &gpui::MouseDownEvent, w: &mut gpui::Window, cx: &mut gpui::App| cb(&id, w, cx)
    };
    let on_input = {
        let id = track_id.clone();
        let cb = callbacks.on_toggle_input.clone();
        move |_: &gpui::MouseDownEvent, w: &mut gpui::Window, cx: &mut gpui::App| cb(&id, w, cx)
    };
    // Listen taps feed the Control Room only; they never change what this
    // channel sends to master, so they are safe to engage mid-take.
    let on_pfl = {
        let id = track_id.clone();
        let cb = callbacks.on_toggle_listen.clone();
        move |_: &gpui::MouseDownEvent, w: &mut gpui::Window, cx: &mut gpui::App| {
            cb(&(id.clone(), ListenMode::Pfl), w, cx)
        }
    };
    let on_afl = {
        let id = track_id.clone();
        let cb = callbacks.on_toggle_listen.clone();
        move |_: &gpui::MouseDownEvent, w: &mut gpui::Window, cx: &mut gpui::App| {
            cb(&(id.clone(), ListenMode::Afl), w, cx)
        }
    };

    let state_row = div()
        .flex()
        .flex_row()
        .w_full()
        .gap(px(2.0))
        .child(console::toggle(
            ("mix-m-btn", id_num).into(),
            "M",
            track.muted.into(),
            Colors::state_mute(),
            base,
            on_mute,
        ))
        .child(console::toggle(
            ("mix-s-btn", id_num).into(),
            "S",
            solo_state,
            Colors::state_solo(),
            base,
            on_solo,
        ))
        .child(console::toggle(
            ("mix-r-btn", id_num).into(),
            "R",
            track.armed.into(),
            Colors::state_arm(),
            base,
            on_arm,
        ))
        .child(console::toggle(
            ("mix-i-btn", id_num).into(),
            "I",
            track.input_monitor.is_active(track.armed).into(),
            Colors::state_monitor(),
            base,
            on_input,
        ));
    let listen_row = div()
        .flex()
        .flex_row()
        .w_full()
        .gap(px(2.0))
        .child(console::toggle(
            ("mix-pfl-btn", id_num).into(),
            "PFL",
            (track.listen == ListenMode::Pfl).into(),
            Colors::accent_primary(),
            base,
            on_pfl,
        ))
        .child(console::toggle(
            ("mix-afl-btn", id_num).into(),
            "AFL",
            (track.listen == ListenMode::Afl).into(),
            Colors::accent_primary(),
            base,
            on_afl,
        ));

    div()
        .flex()
        .flex_col()
        .flex_none()
        .w_full()
        .h(px(console::BUTTONS_H))
        .gap(px(2.0))
        .px(px(4.0))
        .justify_center()
        .child(state_row)
        .child(listen_row)
}

// ─── Meter ──────────────────────────────────────────────────────────────────

// ─── Strip sections ─────────────────────────────────────────────────────────

fn vsti_output_group_key(track_id: &str, insert_id: &str) -> String {
    format!("{track_id}:{insert_id}")
}

/// The row above the racks: what kind of channel this is, and the expander for
/// an instrument's multi-output group.
///
/// Logic spends this slot on the channel-strip Setting menu. This spends it on
/// what the model actually has, and on nothing else — the name and number moved
/// to the plate at the foot of the strip, where the colour is, so the eye has
/// one place to look to answer "which channel is this".
fn strip_top_row(
    track: &TrackState,
    vsti_output_group: Option<(&str, bool, usize, &MixerCallbacks)>,
    i18n: I18n,
) -> impl IntoElement {
    let type_label = mixer_track_type_label(track.track_type, i18n);

    div()
        .flex()
        .flex_row()
        .items_center()
        .flex_none()
        .gap(px(3.0))
        .h(px(console::TOP_ROW_H))
        .pl(px(5.0))
        .pr(px(3.0))
        .border_b(px(1.0))
        .border_color(console::rule())
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .truncate()
                .text_size(px(type_scale::CAPTION))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(Colors::text_faint())
                .child(type_label),
        )
        .children(
            vsti_output_group.map(|(group_key, expanded, count, callbacks)| {
                let group_key = group_key.to_string();
                let toggle = callbacks.on_toggle_vsti_output_group.clone();
                div()
                    .id(gpui::SharedString::from(format!(
                        "vsti-output-group-toggle-{group_key}"
                    )))
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .gap(px(2.0))
                    .h(px(13.0))
                    .px(px(3.0))
                    .rounded(px(crate::theme::radius::MICRO))
                    .bg(if count > 0 {
                        Colors::state_hover()
                    } else {
                        gpui::transparent_black().into()
                    })
                    .cursor(gpui::CursorStyle::PointingHand)
                    .hover(|s| s.bg(Colors::state_pressed()))
                    .when(count > 0, |this| {
                        this.child(
                            div()
                                .text_size(px(type_scale::CAPTION))
                                .font_weight(gpui::FontWeight::BOLD)
                                .text_color(Colors::text_muted())
                                .child(format!("{count}")),
                        )
                    })
                    .child(
                        svg()
                            .path(if expanded {
                                assets::ICON_CHEVRON_DOWN_PATH
                            } else {
                                assets::ICON_CHEVRON_RIGHT_PATH
                            })
                            .w(px(9.0))
                            .h(px(9.0))
                            .text_color(Colors::text_muted()),
                    )
                    .on_mouse_down(gpui::MouseButton::Left, move |_e, w, cx| {
                        toggle(&group_key, w, cx);
                    })
                    .occlude()
            }),
        )
}

/// Output bus indices that get their own mixer sub-strip for this instrument
/// insert, derived from the plugin's REAL per-bus output layout (never by
/// blindly pairing channels two-by-two). One strip per real output bus:
/// a mono bus is one strip, a stereo bus is one strip, a multi-channel bus is
/// one strip. Mirrors `vsti_output_bus_strip_indices` / `ensure_vsti_output_child_tracks`
/// so the visible sub-strips line up 1:1 with the model child tracks and the
/// engine routes.
fn vsti_output_bus_strips(slot: &InsertSlotState) -> Vec<u8> {
    vsti_output_bus_strip_indices(&slot.output_bus_channel_counts)
}

/// Human-readable label for a VSTi output bus strip, reflecting the real bus
/// layout: `"Out 1 (Mono / Ch 1)"`, `"Out 2 (Mono / Ch 2)"`,
/// `"Out 3 (Stereo / Ch 3/4)"`, or `"Out N (Multi / Ch X-Y)"` for buses with
/// more than two channels. Falls back to a plain channel-pair label when the
/// host has not reported a layout yet.
pub fn vsti_output_bus_label(bus_counts: &[u8], bus_index: u8) -> String {
    let n = (bus_index as u16).saturating_add(1);
    // A single multichannel bus is split into flat stereo pairs; describe each
    // pair from its mapped flat channels rather than the (single) bus range.
    if bus_counts.len() == 1 && bus_counts[0] > 2 {
        if let Some((l, r)) = vsti_output_child_channels_for_bus_layout(bus_counts, bus_index) {
            return if l == r {
                format!("Out {n} (Mono / Ch {l})")
            } else {
                format!("Out {n} (Stereo / Ch {l}/{r})")
            };
        }
    }
    if let Some((start, count)) = vsti_output_bus_flat_range(bus_counts, bus_index as usize) {
        return match count {
            0 | 1 => format!("Out {n} (Mono / Ch {start})"),
            2 => format!("Out {n} (Stereo / Ch {start}/{})", start.saturating_add(1)),
            c => {
                let end = (start as u16).saturating_add(c as u16).saturating_sub(1);
                format!("Out {n} (Multi / Ch {start}-{end})")
            }
        };
    }
    // Unknown layout (host hasn't reported): legacy consecutive pair label.
    if let Some((l, r)) = vsti_output_child_channels_for_bus_layout(bus_counts, bus_index) {
        return format!("Out {n} (Ch {l}/{r})");
    }
    format!("Out {n}")
}

fn log_vsti_child_meter_subscribe_once(track: &TrackState) {
    if !crate::forensic_trace::forensic_trace_enabled() || !is_vsti_output_child_track_id(&track.id)
    {
        return;
    }
    static LOGGED: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    let logged = LOGGED.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()));
    let Ok(mut logged) = logged.lock() else {
        return;
    };
    if !logged.insert(track.id.clone()) {
        return;
    }
    let bus_index = track
        .id
        .rsplit_once(":bus:")
        .and_then(|(_, bus)| bus.parse::<u8>().ok())
        .unwrap_or(0);
    eprintln!(
        "[METER SUBSCRIBE]\nstrip_view_id={}\nsubscription_key={}\nmixer_channel_id={}\nbus_index={}\ninitial_peak_l={:.6}\ninitial_peak_r={:.6}",
        track.id, track.id, track.id, bus_index, track.meter_level_l, track.meter_level_r
    );
}

fn log_vsti_child_strip_state(track: &TrackState) {
    if !crate::forensic_trace::forensic_trace_enabled() || !is_vsti_output_child_track_id(&track.id)
    {
        return;
    }
    let bus_index = track
        .id
        .rsplit_once(":bus:")
        .and_then(|(_, bus)| bus.parse::<u8>().ok())
        .unwrap_or(0);
    let plugin_instance_id = track
        .id
        .strip_prefix("vsti-out:")
        .and_then(|rest| rest.split_once(":bus:").map(|(plugin, _)| plugin))
        .unwrap_or("");
    eprintln!(
        "[STRIP STATE]\nstrip_view_id={}\nplugin_instance_id={}\nbus_index={}\nmixer_channel_id={}\nvu_peak_l={:.6}\nvu_peak_r={:.6}\nmute_state={}\nsolo_state={}",
        track.id,
        plugin_instance_id,
        bus_index,
        track.id,
        track.meter_level_l,
        track.meter_level_r,
        track.muted,
        track.solo
    );
}

fn insert_chip(
    track_id: &str,
    insert_index: usize,
    slot: &InsertSlotState,
    callbacks: &MixerCallbacks,
    base: gpui::Rgba,
) -> impl IntoElement {
    let track_id_owned = track_id.to_string();
    let slot_id = slot.id.clone();
    let display = slot.display_name.clone();
    let display_for_log = display.clone();
    let bypassed = slot.bypassed;
    let on_open = callbacks.on_open_insert_editor.clone();
    let on_bypass = callbacks.on_toggle_insert_bypass.clone();
    let on_remove = callbacks.on_remove_insert.clone();

    // A loaded plug-in is the lightest thing in the rack and a bypassed one
    // sinks back into the well, so the chain reads as a signal path at a glance
    // rather than as eight identically-outlined chips.
    let tone = match &slot.load_status {
        InsertLoadStatus::Ready if !bypassed => SlotTone::Active,
        InsertLoadStatus::Ready | InsertLoadStatus::Disabled => SlotTone::Bypassed,
        InsertLoadStatus::Loading | InsertLoadStatus::Empty => SlotTone::Pending,
        InsertLoadStatus::Missing(_) | InsertLoadStatus::Failed(_) => SlotTone::Failed,
    };
    let (bg, text) = tone.colors(base);
    let hover_bg = Colors::composite(bg, Colors::state_hover());

    let id_owned = slot_id.clone();
    let bypass_pair = (track_id_owned.clone(), slot_id.clone());
    let remove_pair = (track_id_owned.clone(), slot_id.clone());

    // Drag payload carries the stable plugin_instance_id, so reorder identity
    // follows the instance rather than the visual index.
    //
    // The slot itself is the handle. A separate grip column cost six of the
    // eighty-eight pixels on every row to say what the pointer already says by
    // picking the row up — and it made the plug-in's name, the thing the user
    // is actually aiming at, the one part of the row that did not drag.
    let chip_drag_payload = FxSlotDrag {
        track_id: track_id_owned.clone(),
        insert_id: slot_id.clone(),
        display_name: slot.display_name.clone(),
    };

    // Drop target: dropping a compatible drag onto this chip moves it into the
    // gap *above* this slot (`insertion_index == insert_index`, the slot's full
    // insert-chain index). `can_drop` restricts drops to the same track and
    // `drag_over` paints the shared accent drop-position line.
    let drop_track = track_id_owned.clone();
    let can_drop_track = track_id_owned.clone();
    let reorder = callbacks.on_reorder_insert.clone();
    let drop_plugin_preset = callbacks.on_drop_plugin_preset.clone();
    let drop_gap = insert_index;
    let preset_track = track_id_owned.clone();
    let preset_slot = insert_index;

    let open_target = (track_id_owned, insert_index, slot_id);

    div()
        .id(gpui::SharedString::from(format!(
            "insert-chip-{}",
            id_owned
        )))
        .can_drop(move |dragged, _window, _cx| {
            if dragged
                .downcast_ref::<FxSlotDrag>()
                .is_some_and(|d| d.track_id == can_drop_track)
            {
                return true;
            }
            dragged
                .downcast_ref::<BrowserDragItem>()
                .is_some_and(|item| is_plugin_preset_path(&item.path))
        })
        .drag_over::<FxSlotDrag>(|style, _drag, _window, _cx| drop_over_highlight(style))
        .drag_over::<BrowserDragItem>(|style, _drag, _window, _cx| drop_over_highlight(style))
        .on_drop::<FxSlotDrag>(move |drag, window, cx| {
            if drag.track_id == drop_track {
                reorder(
                    &(drop_track.clone(), drag.insert_id.clone(), drop_gap),
                    window,
                    cx,
                );
            }
        })
        .on_drop::<BrowserDragItem>(move |item, window, cx| {
            if is_plugin_preset_path(&item.path) {
                drop_plugin_preset(
                    &(item.path.clone(), preset_track.clone(), preset_slot),
                    window,
                    cx,
                );
            }
        })
        .flex()
        .flex_none()
        .flex_row()
        .items_center()
        .gap(px(3.0))
        .px(px(4.0))
        .h(px(console::SLOT_H))
        .rounded(px(crate::theme::radius::MICRO))
        .bg(bg)
        .hover(move |style| style.bg(hover_bg))
        .text_size(px(type_scale::LABEL))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(text)
        .cursor(gpui::CursorStyle::PointingHand)
        .on_drag(chip_drag_payload, |drag, _offset, _window, cx| {
            cx.new(|_| drag.clone())
        })
        .on_mouse_down(gpui::MouseButton::Left, move |_e, w, cx| {
            eprintln!(
                "[mixer] insert row clicked track_id={} insert_index={} plugin={} plugin_instance_id={}",
                open_target.0, open_target.1, display_for_log, open_target.2
            );
            on_open(&open_target, w, cx);
        })
        .occlude()
        .child(div().flex_1().min_w(px(0.0)).truncate().child(display))
        // In-circuit pip. A slot is either in the signal path or it is not, and
        // that is the one thing about it readable without stopping to read.
        .child(
            div()
                .id(gpui::SharedString::from(format!(
                    "insert-bypass-{}",
                    bypass_pair.1
                )))
                .w(px(5.0))
                .h(px(9.0))
                .rounded(px(crate::theme::radius::MICRO))
                .bg(if bypassed {
                    Colors::text_disabled()
                } else {
                    Colors::state_monitor()
                })
                .on_mouse_down(gpui::MouseButton::Left, move |_e, w, cx| {
                    on_bypass(&bypass_pair, w, cx);
                })
                .occlude(),
        )
        // Remove ×.
        .child(
            div()
                .id(gpui::SharedString::from(format!(
                    "insert-remove-{}",
                    remove_pair.1
                )))
                .text_size(px(type_scale::LABEL))
                .text_color(Colors::text_faint())
                .px(px(2.0))
                .cursor(gpui::CursorStyle::PointingHand)
                .child("×")
                .on_mouse_down(gpui::MouseButton::Left, move |_e, w, cx| {
                    on_remove(&remove_pair, w, cx);
                })
                .occlude(),
        )
}

/// Trailing drop zone rendered below the last insert chip so a dragged slot can
/// land at the very end of the chain (`gap == inserts.len()`); the per-chip drop
/// targets only cover the gaps *above* each existing slot. Same-track guarded and
/// shows the shared accent drop-position line while a compatible drag hovers.
fn insert_drop_end(track_id: &str, gap: usize, callbacks: &MixerCallbacks) -> impl IntoElement {
    let track_id_owned = track_id.to_string();
    let can_drop_track = track_id_owned.clone();
    let reorder = callbacks.on_reorder_insert.clone();
    div()
        .id(gpui::SharedString::from(format!(
            "mixer-fx-drop-end-{track_id_owned}"
        )))
        .flex_none()
        .h(px(6.0))
        .mx(px(2.0))
        .can_drop(move |dragged, _window, _cx| {
            dragged
                .downcast_ref::<FxSlotDrag>()
                .is_some_and(|d| d.track_id == can_drop_track)
        })
        .drag_over::<FxSlotDrag>(|style, _drag, _window, _cx| drop_over_highlight(style))
        .on_drop::<FxSlotDrag>(move |drag, window, cx| {
            if drag.track_id == track_id_owned {
                reorder(
                    &(track_id_owned.clone(), drag.insert_id.clone(), gap),
                    window,
                    cx,
                );
            }
        })
}

/// Trailing empty insert slot. Clicking it opens the plugin picker for the
/// next available slot (`next_slot`) on this track. `next_slot` is used for
/// debug logging only — the picker appends to the track's insert chain.
fn add_insert_button(
    track_id: &str,
    next_slot: usize,
    callbacks: &MixerCallbacks,
) -> impl IntoElement {
    let track_id_owned = track_id.to_string();
    let on_add = callbacks.on_add_insert.clone();
    let drop_plugin_preset = callbacks.on_drop_plugin_preset.clone();
    let drop_track = track_id_owned.clone();
    div()
        .id(gpui::SharedString::from(format!(
            "insert-add-{}",
            track_id_owned
        )))
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .gap(px(3.0))
        .px(px(4.0))
        .h(px(console::SLOT_H))
        .rounded(px(crate::theme::radius::MICRO))
        .border(px(1.0))
        .border_dashed()
        .border_color(Colors::border_subtle())
        .text_size(px(type_scale::LABEL))
        .text_color(Colors::text_faint())
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(|s| {
            s.bg(Colors::state_hover())
                .border_color(Colors::border_strong())
                .text_color(Colors::text_secondary())
        })
        .can_drop(|dragged, _window, _cx| {
            dragged
                .downcast_ref::<BrowserDragItem>()
                .is_some_and(|item| is_plugin_preset_path(&item.path))
        })
        .drag_over::<BrowserDragItem>(|style, _drag, _window, _cx| drop_over_highlight(style))
        .on_drop::<BrowserDragItem>(move |item, window, cx| {
            if is_plugin_preset_path(&item.path) {
                drop_plugin_preset(
                    &(item.path.clone(), drop_track.clone(), next_slot),
                    window,
                    cx,
                );
            }
        })
        .child(
            svg()
                .path(assets::ICON_PLUS_PATH)
                .w(px(8.0))
                .h(px(8.0))
                .text_color(Colors::text_faint()),
        )
        .child("Insert")
        .on_mouse_down(gpui::MouseButton::Left, move |_e, w, cx| {
            eprintln!(
                "[mixer] empty insert slot + clicked track={track_id_owned} slot={next_slot}"
            );
            on_add(&track_id_owned, w, cx);
        })
        .occlude()
}

fn is_plugin_preset_path(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("pst"))
}

fn inserts_section(
    track: &TrackState,
    callbacks: &MixerCallbacks,
    height_px: f32,
    base: gpui::Rgba,
    i18n: I18n,
) -> impl IntoElement {
    let effect_start = if track.track_type == TrackType::Instrument {
        1
    } else {
        0
    };
    let used = track.inserts.len();
    let at_max = used >= MAX_INSERT_SLOTS;

    let mut chips = div().flex().flex_col().flex_none().gap(px(1.0)).px(px(3.0));
    let effects = track.effect_inserts();
    for (offset, slot) in effects.iter().enumerate() {
        let insert_index = effect_start + offset;
        chips = chips.child(insert_chip(&track.id, insert_index, slot, callbacks, base));
    }
    // Drop-at-end zone below the last chip (gap == full insert-chain length, so
    // the instrument slot at index 0 is counted). Only meaningful once a slot
    // exists to drag.
    if !effects.is_empty() {
        chips = chips.child(insert_drop_end(&track.id, track.inserts.len(), callbacks));
    }
    // Requirement: always render one trailing empty slot after the last insert,
    // until MAX_INSERT_SLOTS is reached.
    if !at_max {
        chips = chips.child(add_insert_button(&track.id, used, callbacks));
    }

    // Header "+" adds to the next available slot for *this* track; gone once
    // the rack is full, rather than sitting there greyed and unpressable.
    let header_plus = if at_max {
        None
    } else {
        let track_id = track.id.clone();
        let on_add = callbacks.on_add_insert.clone();
        Some(RackPlus {
            id: gpui::SharedString::from(format!("insert-header-add-{}", track.id)),
            on_click: std::sync::Arc::new(move |w, cx| {
                eprintln!("[mixer] INSERTS header + clicked track={track_id} slot={used}");
                on_add(&track_id, w, cx);
            }),
        })
    };

    rack(
        gpui::SharedString::from(format!("insert-slot-scroll-{}", track.id)),
        i18n.tr("mixer.section.inserts"),
        header_plus,
        height_px,
        base,
        chips,
    )
}

/// A rack: caption, then a recessed well the slots scroll inside.
///
/// The well is the strip's only sunken surface. It is what tells the eye where
/// the plug-in chain starts and ends without drawing a box around each slot in
/// it — which is the difference between a console and a list of buttons.
fn rack(
    scroll_id: gpui::SharedString,
    label: String,
    plus: Option<RackPlus>,
    height_px: f32,
    base: gpui::Rgba,
    body: gpui::Div,
) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .flex_none()
        .h(px(height_px))
        .overflow_hidden()
        .border_b(px(1.0))
        .border_color(console::rule())
        .child(console::rack_label(label, plus))
        .child(
            div()
                .id(scroll_id)
                .flex_1()
                .min_h_0()
                .py(px(2.0))
                .bg(console::well(base))
                .overflow_y_scroll()
                .child(body),
        )
}

fn master_inserts_section(
    master: &MasterBusState,
    callbacks: &MixerCallbacks,
    height_px: f32,
    base: gpui::Rgba,
    i18n: I18n,
) -> impl IntoElement {
    let used = master.inserts.len();
    let at_max = used >= MAX_INSERT_SLOTS;

    let mut chips = div().flex().flex_col().flex_none().gap(px(1.0)).px(px(3.0));
    for (insert_index, slot) in master.inserts.iter().enumerate() {
        chips = chips.child(insert_chip(
            MASTER_TRACK_ID,
            insert_index,
            slot,
            callbacks,
            base,
        ));
    }
    if !master.inserts.is_empty() {
        chips = chips.child(insert_drop_end(
            MASTER_TRACK_ID,
            master.inserts.len(),
            callbacks,
        ));
    }
    if !at_max {
        chips = chips.child(add_insert_button(MASTER_TRACK_ID, used, callbacks));
    }

    let header_plus = if at_max {
        None
    } else {
        let on_add = callbacks.on_add_insert.clone();
        Some(RackPlus {
            id: gpui::SharedString::from("insert-header-add-master"),
            on_click: std::sync::Arc::new(move |w, cx| {
                eprintln!("[mixer] INSERTS header + clicked track=master slot={used}");
                on_add(&MASTER_TRACK_ID.to_string(), w, cx);
            }),
        })
    };

    rack(
        gpui::SharedString::from("insert-slot-scroll-master"),
        i18n.tr("mixer.section.inserts"),
        header_plus,
        height_px,
        base,
        chips,
    )
}

/// One send: where it goes, and how much of this channel goes there.
///
/// # Why there is no dB printed on it
///
/// The old row carried "+0.0 dB" beside every target name. Four sends on a
/// channel meant the same four characters repeated down an 88 px column, and at
/// unity — which is where a send sits until someone moves it — the number was
/// saying nothing at all. What the eye needs from a rack of sends is *which
/// ones are up*, and a level bar answers that in one glance where four decimal
/// numbers do not. The exact value is a hover away, in the tooltip, for the
/// moment it actually matters.
fn send_chip(
    track_id: &str,
    send_index: usize,
    send: &SendSlotState,
    target_name: &str,
    callbacks: &MixerCallbacks,
    base: gpui::Rgba,
) -> impl IntoElement {
    let remove_pair = (track_id.to_string(), send.id.clone());
    let on_remove = callbacks.on_remove_send.clone();
    let (bg, text) = if send.enabled {
        SlotTone::Active.colors(base)
    } else {
        SlotTone::Bypassed.colors(base)
    };
    let hover_bg = Colors::composite(bg, Colors::state_hover());
    // The whole row is the drag handle, as with an insert slot.
    let chip_drag_payload = SendSlotDrag {
        track_id: track_id.to_string(),
        send_id: send.id.clone(),
        target_name: target_name.to_string(),
    };
    let can_drop_track = track_id.to_string();
    let drop_track = track_id.to_string();
    let reorder = callbacks.on_reorder_send.clone();
    let gain_pair = (track_id.to_string(), send.id.clone());
    let gain_change = callbacks.on_send_gain_change.clone();
    let gain_reset_pair = gain_pair.clone();
    let gain_reset = callbacks.on_send_gain_change.clone();
    let gain_norm = ((send.gain_db.clamp(-60.0, 6.0) + 60.0) / 66.0).clamp(0.0, 1.0);
    let gain_label = if send.gain_db <= -59.95 {
        "-∞".to_string()
    } else {
        format!("{:+.1} dB", send.gain_db)
    };
    let tooltip = format!("{target_name} · {gain_label}");
    div()
        .id(gpui::SharedString::from(format!("send-chip-{}", send.id)))
        .can_drop(move |dragged, _window, _cx| {
            dragged
                .downcast_ref::<SendSlotDrag>()
                .is_some_and(|d| d.track_id == can_drop_track)
        })
        .drag_over::<SendSlotDrag>(|style, _drag, _window, _cx| drop_over_highlight(style))
        .on_drop::<SendSlotDrag>(move |drag, window, cx| {
            if drag.track_id == drop_track {
                reorder(
                    &(drop_track.clone(), drag.send_id.clone(), send_index),
                    window,
                    cx,
                );
            }
        })
        .flex()
        .flex_none()
        .flex_col()
        .justify_center()
        .gap(px(1.0))
        .px(px(4.0))
        .py(px(2.0))
        .h(px(console::SEND_SLOT_H))
        .rounded(px(crate::theme::radius::MICRO))
        .bg(bg)
        .hover(move |style| style.bg(hover_bg))
        .cursor(gpui::CursorStyle::PointingHand)
        .tooltip(strip_tooltip(tooltip))
        .on_drag(chip_drag_payload, |drag, _offset, _window, cx| {
            cx.new(|_| drag.clone())
        })
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(2.0))
                .w_full()
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .truncate()
                        .text_size(px(type_scale::LABEL))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(text)
                        .child(target_name.to_string()),
                )
                .child(
                    div()
                        .id(gpui::SharedString::from(format!("send-remove-{}", send.id)))
                        .flex_none()
                        .text_size(px(type_scale::LABEL))
                        .text_color(Colors::text_faint())
                        .cursor(gpui::CursorStyle::PointingHand)
                        .child("×")
                        .on_mouse_down(gpui::MouseButton::Left, move |_e, w, cx| {
                            on_remove(&remove_pair, w, cx);
                        })
                        .occlude(),
                ),
        )
        // The level itself. It is the row's second line rather than a control
        // squeezed in beside the name, so a rack of sends reads as a small
        // bar chart of how much of this channel is going where.
        .child(crate::components::slider::compact_slider_with_reset(
            format!("mixer-send-gain-{}-{}", track_id, send.id),
            gain_norm,
            if send.enabled {
                Colors::accent_primary()
            } else {
                Colors::text_disabled()
            },
            move |value, window, cx| {
                let gain_db = (*value * 66.0 - 60.0).clamp(-60.0, 6.0);
                gain_change(
                    &(gain_pair.0.clone(), gain_pair.1.clone(), gain_db),
                    window,
                    cx,
                );
            },
            Some(move |window: &mut gpui::Window, cx: &mut gpui::App| {
                gain_reset(
                    &(gain_reset_pair.0.clone(), gain_reset_pair.1.clone(), 0.0),
                    window,
                    cx,
                );
            }),
        ))
}

fn send_drop_end(track_id: &str, gap: usize, callbacks: &MixerCallbacks) -> impl IntoElement {
    let track_id_owned = track_id.to_string();
    let can_drop_track = track_id_owned.clone();
    let reorder = callbacks.on_reorder_send.clone();
    div()
        .id(gpui::SharedString::from(format!(
            "mixer-send-drop-end-{track_id_owned}"
        )))
        .flex_none()
        .h(px(6.0))
        .mx(px(2.0))
        .can_drop(move |dragged, _window, _cx| {
            dragged
                .downcast_ref::<SendSlotDrag>()
                .is_some_and(|d| d.track_id == can_drop_track)
        })
        .drag_over::<SendSlotDrag>(|style, _drag, _window, _cx| drop_over_highlight(style))
        .on_drop::<SendSlotDrag>(move |drag, window, cx| {
            if drag.track_id == track_id_owned {
                reorder(
                    &(track_id_owned.clone(), drag.send_id.clone(), gap),
                    window,
                    cx,
                );
            }
        })
}

fn add_send_button(track_id: &str, callbacks: &MixerCallbacks) -> impl IntoElement {
    let track_id_owned = track_id.to_string();
    let on_add = callbacks.on_add_send.clone();
    div()
        .id(gpui::SharedString::from(format!(
            "send-add-{}",
            track_id_owned
        )))
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .gap(px(3.0))
        .px(px(4.0))
        .h(px(console::SLOT_H))
        .rounded(px(crate::theme::radius::MICRO))
        .border(px(1.0))
        .border_dashed()
        .border_color(Colors::border_subtle())
        .text_size(px(type_scale::LABEL))
        .text_color(Colors::text_faint())
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(|s| {
            s.bg(Colors::state_hover())
                .border_color(Colors::border_strong())
                .text_color(Colors::text_secondary())
        })
        .child(
            svg()
                .path(assets::ICON_PLUS_PATH)
                .w(px(8.0))
                .h(px(8.0))
                .text_color(Colors::text_faint()),
        )
        .child("Send")
        .on_mouse_down(gpui::MouseButton::Left, move |event, w, cx| {
            let x: f32 = event.position.x.into();
            let y: f32 = event.position.y.into();
            on_add(&(track_id_owned.clone(), x, y), w, cx);
        })
        .occlude()
}

fn sends_section(
    track: &TrackState,
    all_tracks: &[TrackState],
    callbacks: &MixerCallbacks,
    height_px: f32,
    base: gpui::Rgba,
    i18n: I18n,
) -> impl IntoElement {
    // Bus/return strips carry an aux-send rack so chained send/return paths
    // (bus → return, return → bus) are available from the mixer.
    let mut chips = div().flex().flex_col().flex_none().gap(px(1.0)).px(px(3.0));
    for (send_index, send) in track.sends.iter().enumerate() {
        // Resolve the live target name (handles renames) with the stored
        // label as a fallback.
        let target_name = all_tracks
            .iter()
            .find(|t| t.id == send.target_track_id)
            .map(|t| t.name.clone())
            .unwrap_or_else(|| send.target_name.clone());
        chips = chips.child(send_chip(
            &track.id,
            send_index,
            send,
            &target_name,
            callbacks,
            base,
        ));
    }
    if !track.sends.is_empty() {
        chips = chips.child(send_drop_end(&track.id, track.sends.len(), callbacks));
    }
    chips = chips.child(add_send_button(&track.id, callbacks));

    rack(
        gpui::SharedString::from(format!("send-slot-scroll-{}", track.id)),
        i18n.tr("mixer.section.sends"),
        None,
        height_px,
        base,
        chips,
    )
}

/// Pan, with its value printed under the knob.
///
/// The old row spent its second line on a static "L … R" legend, which says
/// nothing a bipolar knob has not already said with its centre tick. The
/// position does need saying — "is that dead centre or two degrees off?" is not
/// answerable from a 26 px disc — so the number takes the line instead.
fn pan_section(track: &TrackState, callbacks: &MixerCallbacks) -> impl IntoElement {
    let track_id = track.id.clone();
    let pan_cb = callbacks.on_pan_change.clone();
    let on_pan_change = move |new_pan: &f32, w: &mut gpui::Window, cx: &mut gpui::App| {
        pan_cb(&(track_id.clone(), *new_pan), w, cx);
    };

    div()
        .flex()
        .flex_col()
        .flex_none()
        .items_center()
        .justify_center()
        .gap(px(2.0))
        .h(px(console::PAN_H))
        .py(px(4.0))
        .border_b(px(1.0))
        .border_color(console::rule())
        .child(knob_bipolar(
            format!("mix-pan-{}", track.id),
            track.pan,
            -1.0,
            1.0,
            // Neutral, not the app accent: there is a pan knob on every strip,
            // and twenty cyan arcs across the panel is the same wall of colour
            // the coloured strip borders were, moved twenty pixels down.
            Colors::text_secondary(),
            None,
            0.0,
            on_pan_change,
        ))
        .child(
            div()
                .text_size(px(type_scale::CAPTION))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(Colors::text_muted())
                .child(format_pan(track.pan)),
        )
}

/// `C`, `L34`, `R100` — the console shorthand, not a signed float. A pan is a
/// side and an amount, and reading "-0.34" forces the eye to decode which side
/// negative means.
fn format_pan(pan: f32) -> String {
    let amount = (pan.abs() * 100.0).round() as i32;
    if amount == 0 {
        "C".to_string()
    } else if pan < 0.0 {
        format!("L{amount}")
    } else {
        format!("R{amount}")
    }
}

/// The fader bay: cap, meter, and the channel's gain printed under both.
///
/// The meter sits hard against the fader, the way it does on a desk, so the
/// hand that moves the cap and the eye that reads the level are working in one
/// place. The dB value goes *below* the pair rather than above it: it is the
/// result of the gesture, and putting the result at the end of the travel is
/// what makes the bay read top-to-bottom as one control.
fn fader_area(
    track: &TrackState,
    callbacks: &MixerCallbacks,
    is_selected: bool,
) -> impl IntoElement {
    let display_vol = track.display_volume();
    let db_str = volume::format_db(display_vol);
    let has_volume_automation = track.has_active_volume_automation();
    let automation_reading = has_volume_automation && track.volume_automation_read;
    let track_id = track.id.clone();
    let start_cb = callbacks.on_volume_drag_start.clone();
    let start_id = track_id.clone();
    let on_vol_start = move |new_norm: &f32, w: &mut gpui::Window, cx: &mut gpui::App| {
        start_cb(&(start_id.clone(), *new_norm), w, cx);
    };
    let preview_cb = callbacks.on_volume_drag_preview.clone();
    let preview_id = track_id.clone();
    let on_vol_preview = move |new_norm: &f32, w: &mut gpui::Window, cx: &mut gpui::App| {
        preview_cb(&(preview_id.clone(), *new_norm), w, cx);
    };
    let commit_cb = callbacks.on_volume_drag_commit.clone();
    let commit_id = track_id.clone();
    let on_vol_commit = move |w: &mut gpui::Window, cx: &mut gpui::App| {
        commit_cb(&commit_id, w, cx);
    };
    let reset_cb = callbacks.on_volume_change.clone();
    let reset_id = track_id;
    let on_vol_reset = move |w: &mut gpui::Window, cx: &mut gpui::App| {
        reset_cb(&(reset_id.clone(), volume::db_to_norm(0.0)), w, cx);
    };

    fader_bay(
        fader_with_drag_callbacks(
            format!("mix-fader-{}", track.id),
            display_vol,
            Some(on_vol_start),
            Some(on_vol_preview),
            Some(on_vol_commit),
            Some(on_vol_reset),
        )
        .into_any_element(),
        meter_surface_db(
            track.meter_level_l,
            track.meter_level_r,
            track.meter_peak_hold_l,
            track.meter_peak_hold_r,
            track.meter_clip,
        )
        .into_any_element(),
        db_str,
        is_selected || automation_reading,
        has_volume_automation.then_some(automation_reading),
    )
}

/// The shared bay every strip's fader sits in — channel, VSTi sub-strip, Master
/// and Control Room alike, so the four never drift apart.
///
/// `automation` is `Some(reading)` only when the channel has volume automation:
/// `true` while the fader is following it, `false` while the user's hand owns
/// the value. A channel with no automation shows no badge at all rather than a
/// permanently dark one.
fn fader_bay(
    fader: gpui::AnyElement,
    meter: gpui::AnyElement,
    db_text: String,
    highlight: bool,
    automation: Option<bool>,
) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h(px(console::FADER_MIN_H))
        .items_center()
        .w_full()
        .px(px(3.0))
        .pt(px(5.0))
        .pb(px(3.0))
        .gap(px(3.0))
        .child(
            div()
                .flex()
                .flex_row()
                .items_stretch()
                .justify_center()
                .gap(px(3.0))
                .flex_1()
                .min_h_0()
                .w_full()
                .child(fader)
                .child(console::meter_scale())
                .child(meter),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_center()
                .gap(px(3.0))
                .w_full()
                .h(px(13.0))
                .child(
                    div()
                        .text_size(px(type_scale::VALUE))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(if highlight {
                            Colors::text_primary()
                        } else {
                            Colors::text_secondary()
                        })
                        .child(db_text),
                )
                .children(automation.map(|reading| {
                    div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .h(px(11.0))
                        .px(px(3.0))
                        .rounded(px(crate::theme::radius::MICRO))
                        .bg(if reading {
                            Colors::state_automation()
                        } else {
                            Colors::state_hover()
                        })
                        .text_size(px(type_scale::CAPTION))
                        .font_weight(gpui::FontWeight::BOLD)
                        .text_color(if reading {
                            Colors::on_color(Colors::state_automation())
                        } else {
                            Colors::text_muted()
                        })
                        .child("A")
                })),
        )
}

/// Human-readable output-routing label for a strip: `Main`, the resolved
/// Bus/Return name, or the routing's own fallback label.
fn output_routing_label(track: &TrackState, all_tracks: &[TrackState]) -> String {
    match &track.routing.output {
        TrackOutputRouting::Main => "Main".to_string(),
        TrackOutputRouting::Bus { bus_id } => all_tracks
            .iter()
            .find(|t| &t.id == bus_id)
            .map(|t| t.name.clone())
            .unwrap_or_else(|| bus_id.clone()),
        other => other.label(),
    }
}

/// Vertical drop applied to the output picker's anchor so the menu opens
/// just under the OUT pill rather than on top of it. Half the pill height
/// (16px) plus a small gap clears the pill for a centred click.
const OUTPUT_BUTTON_MENU_DROP: f32 = 12.0;

/// Output-routing dropdown button. Shows the current target and opens the
/// output picker (Main / Bus / Return) on click, wired through
/// `on_open_output_picker` to the real `set_track_output_routing`.
fn output_button(
    track_id: &str,
    label: String,
    id_num: usize,
    base: gpui::Rgba,
    on_open: std::sync::Arc<
        dyn Fn(&(String, f32, f32), &mut gpui::Window, &mut gpui::App) + 'static,
    >,
) -> impl IntoElement {
    let track_id = track_id.to_string();
    console::io_button(
        ("mix-output-btn", id_num).into(),
        None,
        label,
        base,
        move |event: &gpui::MouseDownEvent, w, cx| {
            let x: f32 = event.position.x.into();
            // Drop the menu just below the button instead of at the click
            // point, so it doesn't cover the button itself. The overlay
            // positioner still flips upward if it can't fit below.
            let y: f32 = f32::from(event.position.y) + OUTPUT_BUTTON_MENU_DROP;
            on_open(&(track_id.clone(), x, y), w, cx);
            cx.stop_propagation();
        },
    )
}

// ─── Vertical split handle ───────────────────────────────────────────────────

/// Compact horizontal splitter between mixer vertical sections. 6px hitbox
/// with a short centered grip line; hover/active use theme tokens (no web-style
/// chunky handle). `id_num` namespaces the GPUI element id per strip so
/// drag/click state never bleeds between strips. The drag uses the same
/// `on_drag` + ancestor `on_drag_move` capture pattern as the bottom panel.
fn vertical_split_handle(
    id_num: usize,
    target: MixerSplitTarget,
    split: &MixerSplit,
) -> impl IntoElement {
    let on_down = split.on_action.clone();
    let on_dbl = split.on_action.clone();
    let is_resizing = split.active_target == Some(target);

    let grip = if is_resizing {
        Colors::accent_primary()
    } else {
        Colors::border_subtle()
    };

    let mut handle = div()
        .id((
            match target {
                MixerSplitTarget::InsertSend => "mix-split-insert-send",
                MixerSplitTarget::SendFader => "mix-split-send-fader",
            },
            id_num,
        ))
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .w_full()
        .h(px(SEC_SPLITTER_H))
        .border_b(px(1.0))
        .border_color(Colors::border_default())
        .cursor(gpui::CursorStyle::ResizeUpDown)
        .child(
            div()
                .w(px(20.0))
                .h(px(1.0))
                .rounded(px(crate::theme::radius::PILL))
                .bg(grip),
        )
        .on_mouse_down(gpui::MouseButton::Left, move |e: &MouseDownEvent, w, cx| {
            let y: f32 = e.position.y.into();
            on_down(MixerSplitAction::ResizeStart(target, y), w, cx);
        })
        .on_drag(MixerSplitDrag, |_drag, _offset, _window, cx| {
            cx.new(|_| MixerSplitDrag)
        })
        .on_click(move |e: &ClickEvent, w, cx| {
            if e.click_count() >= 2 {
                on_dbl(MixerSplitAction::Reset(target), w, cx);
            }
        })
        .occlude();

    if is_resizing {
        handle = handle.bg(Colors::accent_soft());
    } else {
        handle = handle.hover(|s| s.bg(Colors::surface_control_hover()));
    }
    handle
}

// ─── Channel strip ──────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn channel_strip(
    track: &TrackState,
    all_tracks: &[TrackState],
    index: usize,
    is_selected: bool,
    callbacks: &MixerCallbacks,
    split: &MixerSplit,
    strip_available_px: f32,
    vsti_group_expanded: Option<bool>,
    // When true the GPU primitive layer owns the strip background, the name
    // plate fill, and the right separator — the strip container omits them so
    // the batched canvas behind shows through. Inner sections keep their own
    // styling.
    gpu_decor: bool,
    i18n: I18n,
) -> impl IntoElement {
    log_vsti_child_meter_subscribe_once(track);
    log_vsti_child_strip_state(track);
    let id_num = {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        track.id.hash(&mut hasher);
        hasher.finish() as usize
    };

    // Every surface inside the strip is resolved against this one fill, so a
    // selected strip lifts as a whole rather than growing a coloured outline.
    let base = console::strip_surface(is_selected);

    let select_id = track.id.clone();
    let select_cb = callbacks.on_select_track.clone();
    let on_select_strip =
        move |event: &gpui::MouseDownEvent, w: &mut gpui::Window, cx: &mut gpui::App| {
            if event.button == gpui::MouseButton::Left {
                let additive = event.modifiers.control || event.modifiers.platform;
                let range = event.modifiers.shift;
                select_cb(&select_id, additive, range, w, cx);
            }
        };
    let context_id = track.id.clone();
    let on_context = callbacks.on_context_menu.clone();
    let (insert_h, send_h) = clamp_mixer_section_heights_for_strip(
        split.insert_px,
        split.send_px,
        strip_available_px.max(STRIP_MIN_HEIGHT),
    );
    let vsti_group = track
        .instrument_insert()
        .filter(|slot| !slot.is_empty())
        .map(|slot| {
            let group_key = vsti_output_group_key(&track.id, &slot.id);
            let expanded = vsti_group_expanded.unwrap_or(true);
            let count = vsti_output_bus_strips(slot).len();
            (group_key, expanded, count)
        });

    div()
        .flex()
        .flex_col()
        .flex_none()
        .w(px(STRIP_WIDTH))
        .min_h(px(STRIP_MIN_HEIGHT))
        .h_full()
        .overflow_hidden()
        .when(!gpu_decor, |s| {
            let hover = Colors::composite(base, Colors::state_hover());
            s.bg(base)
                .border_r(px(1.0))
                .border_color(console::rule())
                .hover(move |h| h.bg(hover))
        })
        .id(("mix-strip", id_num))
        // Select during capture so occluding child controls (racks, pan and
        // fader) cannot delay or swallow channel selection. The child still
        // receives the same event and performs its own action normally.
        .capture_any_mouse_down(on_select_strip)
        .when_some(on_context, |this, cb| {
            this.on_mouse_down(
                gpui::MouseButton::Right,
                move |event: &gpui::MouseDownEvent, window, cx| {
                    let x: f32 = event.position.x.into();
                    let y: f32 = event.position.y.into();
                    cb(&(context_id.clone(), x, y), window, cx);
                },
            )
        })
        .child(console::channel_header(
            track.color,
            Some(index + 1),
            track.name.clone(),
            is_selected,
        ))
        .child(strip_top_row(
            track,
            vsti_group.as_ref().map(|(group_key, expanded, count)| {
                (group_key.as_str(), *expanded, *count, callbacks)
            }),
            i18n,
        ))
        .child(inserts_section(track, callbacks, insert_h, base, i18n))
        .child(vertical_split_handle(
            id_num,
            MixerSplitTarget::InsertSend,
            split,
        ))
        .child(sends_section(
            track, all_tracks, callbacks, send_h, base, i18n,
        ))
        .child(vertical_split_handle(
            id_num,
            MixerSplitTarget::SendFader,
            split,
        ))
        // ── Lower console — I/O, pan, fader bay, toggles. Takes the remaining
        // height; the fader bay is the flex_1 child so it absorbs growth and
        // gives way first when space is tight.
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h(px(LOWER_CONTROL_MIN_H))
                .overflow_hidden()
                .w_full()
                .child(output_button(
                    &track.id,
                    output_routing_label(track, all_tracks),
                    id_num,
                    base,
                    callbacks.on_open_output_picker.clone(),
                ))
                .child(pan_section(track, callbacks))
                .child(fader_area(track, callbacks, is_selected))
                .child(button_row(track, callbacks, id_num, false, base)),
        )
        .child(console::name_plate(
            track.color,
            Some(index + 1),
            track.name.clone(),
            is_selected,
            gpu_decor,
        ))
}

#[allow(clippy::too_many_arguments)]
fn vsti_output_sub_strip(
    parent_track: &TrackState,
    child_track: &TrackState,
    all_tracks: &[TrackState],
    insert_id: &str,
    bus_index: u8,
    bus_counts: &[u8],
    selected_track_id: Option<&str>,
    focus_highlight: bool,
    vsti_output_meters: &std::collections::HashMap<String, VstiOutputMeterState>,
    callbacks: &MixerCallbacks,
    split: &MixerSplit,
    strip_available_px: f32,
    gpu_decor: bool,
    i18n: I18n,
) -> impl IntoElement {
    let id_num = {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        child_track.id.hash(&mut hasher);
        hasher.finish() as usize
    };
    let bus_label = vsti_output_bus_label(bus_counts, bus_index);
    // Render the backing per-bus child track so its mute/solo/fader/pan and VU
    // are the real, engine-routed per-output-bus state — never the parent's.
    let mut sub_track = child_track.clone();
    sub_track.name = bus_label.clone();
    sub_track.inserts.clear();
    sub_track.sends.clear();
    // Per-output-bus VU: the child track already carries engine pass-2 L/R
    // peaks. Fall back to the dedicated per-channel meter map for this bus's
    // flat channels when the child track meter has not been populated yet.
    if sub_track.meter_level_l <= 0.0 && sub_track.meter_level_r <= 0.0 {
        if let Some((channel_l, channel_r)) =
            vsti_output_child_channels_for_bus_layout(bus_counts, bus_index)
        {
            let meter_l = vsti_output_meters.get(&vsti_output_meter_key(
                &parent_track.id,
                insert_id,
                channel_l,
            ));
            let meter_r = vsti_output_meters.get(&vsti_output_meter_key(
                &parent_track.id,
                insert_id,
                channel_r,
            ));
            if let Some(meter) = meter_l {
                sub_track.meter_level_l = meter.level;
                sub_track.meter_peak_hold_l = meter.peak_hold;
                sub_track.meter_clip |= meter.clip;
            }
            if let Some(meter) = meter_r {
                sub_track.meter_level_r = meter.level;
                sub_track.meter_peak_hold_r = meter.peak_hold;
                sub_track.meter_clip |= meter.clip;
            }
        }
    }
    let is_selected = focus_highlight || selected_track_id == Some(child_track.id.as_str());
    // A sunken body is what marks these as children of the instrument above
    // them. The parent's colour used to bracket the group with tinted side
    // borders; it now reaches them the way it reaches every other strip —
    // through the plate at the foot, which carries the parent's colour because
    // the bus belongs to the parent's instrument.
    let base = console::sub_strip_surface(is_selected);
    let select_id = child_track.id.clone();
    let select_cb = callbacks.on_select_track.clone();
    let on_select_strip =
        move |event: &gpui::MouseDownEvent, w: &mut gpui::Window, cx: &mut gpui::App| {
            if event.button == gpui::MouseButton::Left {
                let additive = event.modifiers.control || event.modifiers.platform;
                let range = event.modifiers.shift;
                select_cb(&select_id, additive, range, w, cx);
            }
        };
    let context_id = child_track.id.clone();
    let on_context = callbacks.on_context_menu.clone();
    let (insert_h, send_h) = clamp_mixer_section_heights_for_strip(
        split.insert_px,
        split.send_px,
        strip_available_px.max(STRIP_MIN_HEIGHT),
    );

    div()
        .flex()
        .flex_col()
        .flex_none()
        .w(px(STRIP_WIDTH))
        .min_h(px(STRIP_MIN_HEIGHT))
        .h_full()
        .overflow_hidden()
        .when(!gpu_decor, |s| {
            let hover = Colors::composite(base, Colors::state_hover());
            s.bg(base)
                .border_r(px(1.0))
                .border_color(console::rule())
                .hover(move |h| h.bg(hover))
        })
        .id(("mix-vsti-sub-strip", id_num))
        .capture_any_mouse_down(on_select_strip)
        .when_some(on_context, |this, cb| {
            this.on_mouse_down(
                gpui::MouseButton::Right,
                move |event: &gpui::MouseDownEvent, window, cx| {
                    let x: f32 = event.position.x.into();
                    let y: f32 = event.position.y.into();
                    cb(&(context_id.clone(), x, y), window, cx);
                },
            )
        })
        // Real callbacks: mute / solo / volume / pan all target the child track
        // id (via button_row / pan_section / fader_area below), so S/M and the
        // fader operate per output bus.
        .child(console::channel_header(
            parent_track.color,
            None,
            bus_label.clone(),
            is_selected,
        ))
        .child(strip_top_row(&sub_track, None, i18n))
        // Real per-bus insert rack: the backing child track is a genuine Bus
        // model track, so its FX chain is added/bypassed/reordered by child
        // track id and processed by the engine's pass-2 routing chain for
        // this output bus only (Add Insert opens the Effects picker).
        .child(inserts_section(
            child_track,
            callbacks,
            insert_h,
            base,
            i18n,
        ))
        .child(vertical_split_handle(
            id_num,
            MixerSplitTarget::InsertSend,
            split,
        ))
        .child(sends_section(
            child_track,
            all_tracks,
            callbacks,
            send_h,
            base,
            i18n,
        ))
        .child(vertical_split_handle(
            id_num,
            MixerSplitTarget::SendFader,
            split,
        ))
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h(px(LOWER_CONTROL_MIN_H))
                .overflow_hidden()
                .w_full()
                .child(output_button(
                    &child_track.id,
                    output_routing_label(child_track, all_tracks),
                    id_num,
                    base,
                    callbacks.on_open_output_picker.clone(),
                ))
                .child(pan_section(&sub_track, callbacks))
                .child(fader_area(&sub_track, callbacks, is_selected))
                // Solo on the parent instrument sounds every one of its output
                // channels (engine: `has_soloed_vsti_output_parent`), so this
                // channel's S shows the inherited state rather than sitting
                // dark while the channel is plainly audible.
                .child(button_row(
                    &sub_track,
                    callbacks,
                    id_num,
                    parent_track.solo,
                    base,
                )),
        )
        .child(console::name_plate(
            parent_track.color,
            None,
            bus_label,
            is_selected,
            gpu_decor,
        ))
}

// ─── Master block ───────────────────────────────────────────────────────────

pub(crate) fn master_strip(
    master: &MasterBusState,
    on_master_vol_change: std::sync::Arc<dyn Fn(&f32, &mut gpui::Window, &mut gpui::App) + 'static>,
    callbacks: &MixerCallbacks,
    split: &MixerSplit,
    strip_available_px: f32,
    i18n: I18n,
) -> impl IntoElement {
    let db_str = volume::format_db(master.volume);
    let base = console::pinned_surface();
    let on_start_cb = callbacks.on_master_volume_drag_start.clone();
    let on_master_start = move |v: &f32, w: &mut gpui::Window, cx: &mut gpui::App| {
        on_start_cb(v, w, cx);
    };
    let on_preview_cb = callbacks.on_master_volume_drag_preview.clone();
    let on_master_preview = move |v: &f32, w: &mut gpui::Window, cx: &mut gpui::App| {
        on_preview_cb(v, w, cx);
    };
    let on_commit_cb = callbacks.on_master_volume_drag_commit.clone();
    let on_master_commit = move |w: &mut gpui::Window, cx: &mut gpui::App| {
        on_commit_cb(w, cx);
    };
    let on_reset_cb = on_master_vol_change;
    let on_master_reset = move |w: &mut gpui::Window, cx: &mut gpui::App| {
        on_reset_cb(&volume::db_to_norm(0.0), w, cx);
    };
    let (insert_h, send_h) = clamp_mixer_section_heights_for_strip(
        split.insert_px,
        split.send_px,
        strip_available_px.max(STRIP_MIN_HEIGHT),
    );

    div()
        .flex()
        .flex_col()
        .flex_none()
        .w(px(STRIP_WIDTH))
        .min_h(px(STRIP_MIN_HEIGHT))
        .h_full()
        .overflow_hidden()
        .bg(base)
        .border_l(px(1.0))
        .border_color(Colors::master_strip_border())
        .child(console::channel_header(
            console::master_plate_fill(),
            None,
            i18n.tr("mixer.master.label"),
            false,
        ))
        .child(pinned_top_row(i18n.tr("mixer.master.bus-label")))
        .child(master_inserts_section(
            master, callbacks, insert_h, base, i18n,
        ))
        // The master takes no sends, but it keeps their slot: every fixed row
        // below this point then lands on the same baseline as the channels
        // beside it, which is the whole reason the mixer reads as one console
        // and not as a row of strips plus two odd ones on the end.
        .child(pinned_rack_spacer(send_h + SEC_SPLITTER_H * 2.0))
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h(px(LOWER_CONTROL_MIN_H))
                .overflow_hidden()
                .w_full()
                // Output destination — a logical Output Audio Connection, never
                // a raw hardware pair. Same slot as a channel strip's picker.
                .child(control_room_output_button(
                    "master-output",
                    i18n.tr("mixer.master.output"),
                    master.output_label.clone(),
                    base,
                    callbacks.on_master_output_picker.clone(),
                ))
                // The master has no pan. This row states the bus format only —
                // a caption, not a control: it used to be an accent-bordered
                // chip that read as an armed toggle nobody could press.
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_none()
                        .items_center()
                        .justify_center()
                        .h(px(console::PAN_H))
                        .px(px(5.0))
                        .border_b(px(1.0))
                        .border_color(console::rule())
                        .child(console::caption(i18n.tr("mixer.stereo"))),
                )
                .child(fader_bay(
                    fader_with_drag_callbacks(
                        "mix-fader-master",
                        master.volume,
                        Some(on_master_start),
                        Some(on_master_preview),
                        Some(on_master_commit),
                        Some(on_master_reset),
                    )
                    .into_any_element(),
                    meter_surface_db(
                        master.meter_level_l,
                        master.meter_level_r,
                        master.meter_peak_hold_l,
                        master.meter_peak_hold_r,
                        master.meter_clip,
                    )
                    .into_any_element(),
                    db_str,
                    true,
                    None,
                ))
                // No M/S/R/I on the master; the row is kept so the plate below
                // stays on the channel strips' baseline.
                .child(div().h(px(console::BUTTONS_H)).w_full().flex_none()),
        )
        .child(console::name_plate(
            console::master_plate_fill(),
            None,
            i18n.tr("mixer.master.label"),
            false,
            false,
        ))
}

/// The pinned strips' top row: what bus this is. Same height and rule as a
/// channel's type row, so the two kinds of strip start on the same line.
fn pinned_top_row(bus_label: String) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .flex_none()
        .h(px(console::TOP_ROW_H))
        .px(px(5.0))
        .border_b(px(1.0))
        .border_color(console::rule())
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .truncate()
                .text_size(px(type_scale::CAPTION))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(Colors::text_faint())
                .child(bus_label),
        )
}

/// Holds a rack's worth of height on a pinned strip that has no such rack, so
/// the rows below it stay level with the channel strips.
fn pinned_rack_spacer(height_px: f32) -> impl IntoElement {
    div()
        .flex_none()
        .w_full()
        .h(px(height_px))
        .border_b(px(1.0))
        .border_color(console::rule())
}

/// Tooltip body for a pinned-strip control. The strips are 88 px wide, so every
/// routing value truncates sooner or later — the hover text is the only place
/// the full name can be read.
struct StripTooltip(String);

impl gpui::Render for StripTooltip {
    fn render(
        &mut self,
        _window: &mut gpui::Window,
        _cx: &mut gpui::Context<Self>,
    ) -> impl IntoElement {
        div()
            .px(px(8.0))
            .py(px(4.0))
            .rounded(px(crate::theme::radius::CONTROL))
            .bg(Colors::surface_raised())
            .border(px(1.0))
            .border_color(Colors::border_subtle())
            .text_size(px(typography::DENSE_LABEL))
            .text_color(Colors::text_secondary())
            .child(self.0.clone())
    }
}

fn strip_tooltip(
    text: String,
) -> impl Fn(&mut gpui::Window, &mut gpui::App) -> gpui::AnyView + 'static {
    move |_window, cx| cx.new(|_| StripTooltip(text.clone())).into()
}

/// The Monitor Source row: a caption and the routing it names.
///
/// `accent_value` marks a source the user is hearing *because a Listen tap
/// overrode their choice* — the one case where the value on screen is not the
/// one they picked, and the only reason this row is ever coloured.
fn control_room_selector(
    id: &str,
    label: String,
    value: String,
    accent_value: bool,
    base: gpui::Rgba,
    on_click: Option<std::sync::Arc<dyn Fn(&(f32, f32), &mut gpui::Window, &mut gpui::App)>>,
) -> impl IntoElement {
    let tooltip_value = value.clone();
    let text_color = if accent_value {
        Colors::accent_primary()
    } else {
        Colors::text_secondary()
    };
    div()
        .flex()
        .flex_col()
        .w_full()
        .min_w(px(0.0))
        .gap(px(2.0))
        .child(
            div()
                .w_full()
                .min_w(px(0.0))
                .truncate()
                .text_size(px(type_scale::CAPTION))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(Colors::text_faint())
                .child(label.to_uppercase()),
        )
        .children(on_click.map(|cb| {
            let rest = console::well(base);
            let hover = Colors::composite(rest, Colors::state_hover());
            div()
                .id(gpui::ElementId::Name(id.to_string().into()))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(3.0))
                .w_full()
                .min_w(px(0.0))
                .h(px(16.0))
                .px(px(4.0))
                .rounded(px(crate::theme::radius::MICRO))
                .bg(rest)
                .cursor(gpui::CursorStyle::PointingHand)
                .hover(move |s| s.bg(hover))
                .tooltip(strip_tooltip(tooltip_value))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .truncate()
                        .text_size(px(type_scale::LABEL))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(text_color)
                        .child(value),
                )
                .child(
                    svg()
                        .path(assets::ICON_CHEVRON_DOWN_PATH)
                        .w(px(8.0))
                        .h(px(8.0))
                        .flex_shrink_0()
                        .text_color(Colors::text_faint()),
                )
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    move |event: &gpui::MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        let x: f32 = event.position.x.into();
                        let y: f32 = event.position.y.into();
                        cb(&(x, y), window, cx);
                    },
                )
                .occlude()
        }))
}

/// Output-routing row for a pinned strip, in the same slot and shape as a
/// channel strip's [`output_button`] so all three read as one control.
///
/// Master and Monitor previously repeated their output name as a static chip
/// here while the only working picker sat elsewhere in the strip — this is that
/// row turned into the real control, with the duplicate removed.
fn control_room_output_button(
    id: &str,
    label: String,
    value: String,
    base: gpui::Rgba,
    on_open: Option<std::sync::Arc<dyn Fn(&(f32, f32), &mut gpui::Window, &mut gpui::App)>>,
) -> gpui::AnyElement {
    let Some(cb) = on_open else {
        // No picker wired: state the destination rather than offering a button
        // that does nothing when pressed.
        return div()
            .flex()
            .flex_none()
            .items_center()
            .h(px(console::IO_ROW_H))
            .px(px(6.0))
            .child(console::caption(value))
            .into_any_element();
    };
    let tooltip_value = format!("{label}: {value}");
    div()
        .id(gpui::ElementId::Name(format!("{id}-row").into()))
        .tooltip(strip_tooltip(tooltip_value))
        .child(console::io_button(
            gpui::ElementId::Name(id.to_string().into()),
            None,
            value,
            base,
            move |event: &gpui::MouseDownEvent, window, cx| {
                cx.stop_propagation();
                let x: f32 = event.position.x.into();
                // Same drop as a channel strip's output picker: the menu opens
                // under the button, not over it.
                let y: f32 = f32::from(event.position.y) + OUTPUT_BUTTON_MENU_DROP;
                cb(&(x, y), window, cx);
            },
        ))
        .into_any_element()
}

/// A Control Room toggle (Mute / Dim / Mono), in the channel toggles' shape so
/// the pinned strips read as part of the same desk.
fn control_room_toggle(
    id: &str,
    label: &str,
    active: bool,
    semantic: gpui::Rgba,
    base: gpui::Rgba,
    on_toggle: std::sync::Arc<dyn Fn(&(), &mut gpui::Window, &mut gpui::App)>,
) -> impl IntoElement {
    let rest = console::well(base);
    let hover = Colors::composite(rest, Colors::state_hover());
    let on_hover = Colors::composite(semantic, Colors::state_hover());
    div()
        .id(gpui::ElementId::Name(id.to_string().into()))
        .flex()
        .flex_1()
        .min_w(px(0.0))
        .items_center()
        .justify_center()
        .h(px(15.0))
        .rounded(px(crate::theme::radius::MICRO))
        .cursor(gpui::CursorStyle::PointingHand)
        .bg(if active { semantic } else { rest })
        .hover(move |s| s.bg(if active { on_hover } else { hover }))
        .text_size(px(type_scale::CAPTION))
        .font_weight(gpui::FontWeight::BOLD)
        .text_color(if active {
            Colors::on_color(semantic)
        } else {
            Colors::text_muted()
        })
        .child(label.to_string())
        .on_mouse_down(
            gpui::MouseButton::Left,
            move |_event: &gpui::MouseDownEvent, window, cx| {
                cx.stop_propagation();
                on_toggle(&(), window, cx);
            },
        )
}

/// Pinned Control Room strip — the monitoring bus fed from the master bus,
/// shown beside Master as its own channel instrument.
///
/// Signal flow, all of it owned by the engine:
///
/// ```txt
/// Master Bus -> Monitor Source Router -> Listen (PFL/AFL) override
///     -> Monitor Inserts -> Gain/Dim/Mono/Mute -> selected hardware output
/// ```
///
/// Everything on this strip affects **playback monitoring only**. The engine
/// applies the Control Room inside the device callback, which offline export,
/// stem export, and recording never enter — so nothing here can change the
/// master mix or any rendered file.
///
/// The meter shows the post-monitor-processing signal actually leaving for the
/// monitoring output, so it reflects monitor gain, dim, mono, and monitor
/// inserts — not the master bus level.
pub(crate) fn monitor_strip(
    monitor: &MonitorBusState,
    callbacks: &MixerCallbacks,
    split: &MixerSplit,
    strip_available_px: f32,
    i18n: I18n,
) -> impl IntoElement {
    let db_str = volume::format_db(monitor.volume);
    let base = console::pinned_surface();
    let on_volume = callbacks.on_monitor_volume_change.clone();
    let on_preview = on_volume.clone();
    let on_monitor_preview = move |v: &f32, w: &mut gpui::Window, cx: &mut gpui::App| {
        on_preview(v, w, cx);
    };
    let on_start = on_volume.clone();
    let on_monitor_start = move |v: &f32, w: &mut gpui::Window, cx: &mut gpui::App| {
        on_start(v, w, cx);
    };
    // Every pointer sample is already the committed value (session state, no
    // undo entry to coalesce), so release has nothing left to do.
    let on_monitor_commit = move |_w: &mut gpui::Window, _cx: &mut gpui::App| {};
    let on_reset = on_volume;
    let on_monitor_reset = move |w: &mut gpui::Window, cx: &mut gpui::App| {
        on_reset(&volume::db_to_norm(0.0), w, cx);
    };

    // Same section split as `master_strip`, so both pinned strips align with
    // the channels and with each other.
    let (source_h, send_h) = clamp_mixer_section_heights_for_strip(
        split.insert_px,
        split.send_px,
        strip_available_px.max(STRIP_MIN_HEIGHT),
    );

    // A Listen tap overrides the selected source; say so rather than showing a
    // Source the user is not actually hearing.
    let source_value = if monitor.listen_active {
        i18n.tr("mixer.monitor.listening")
    } else {
        monitor.source_label()
    };

    div()
        .flex()
        .flex_col()
        .flex_none()
        .w(px(STRIP_WIDTH))
        .min_h(px(STRIP_MIN_HEIGHT))
        .h_full()
        .overflow_hidden()
        .bg(base)
        .border_l(px(1.0))
        .border_color(console::rule())
        .child(console::channel_header(
            console::monitor_plate_fill(),
            None,
            i18n.tr("mixer.monitor.label"),
            false,
        ))
        .child(pinned_top_row(i18n.tr("mixer.monitor.bus-label")))
        // Routing — Source only. Occupies the channels' insert rack so the two
        // pinned strips stay on matching baselines with them.
        .child(
            div()
                .flex()
                .flex_col()
                .flex_none()
                .h(px(source_h))
                .overflow_hidden()
                .px(px(5.0))
                .py(px(4.0))
                .border_b(px(1.0))
                .border_color(console::rule())
                .child(control_room_selector(
                    "monitor-source",
                    i18n.tr("mixer.monitor.source"),
                    source_value,
                    monitor.listen_active,
                    base,
                    callbacks.on_monitor_source_picker.clone(),
                )),
        )
        .child(pinned_rack_spacer(send_h + SEC_SPLITTER_H * 2.0))
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h(px(LOWER_CONTROL_MIN_H))
                .overflow_hidden()
                .w_full()
                .child(control_room_output_button(
                    "monitor-output",
                    i18n.tr("mixer.monitor.output"),
                    monitor.output_name.clone(),
                    base,
                    callbacks.on_monitor_output_picker.clone(),
                ))
                // The Control Room has no pan. What it has instead is the room
                // itself: how loud, how quiet, and in how many speakers.
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_none()
                        .items_center()
                        .justify_center()
                        .h(px(console::PAN_H))
                        .px(px(4.0))
                        .border_b(px(1.0))
                        .border_color(console::rule())
                        .child(console::caption(i18n.tr("mixer.monitor.label"))),
                )
                .child(fader_bay(
                    fader_with_drag_callbacks(
                        "mix-fader-monitor",
                        monitor.volume,
                        Some(on_monitor_start),
                        Some(on_monitor_preview),
                        Some(on_monitor_commit),
                        Some(on_monitor_reset),
                    )
                    .into_any_element(),
                    meter_surface_db(
                        monitor.meter_level_l,
                        monitor.meter_level_r,
                        monitor.meter_peak_hold_l,
                        monitor.meter_peak_hold_r,
                        monitor.meter_clip,
                    )
                    .into_any_element(),
                    db_str,
                    monitor.dim || monitor.mute,
                    None,
                ))
                // Mute / Dim / Mono take the channels' toggle row: same slot,
                // same shape, and the three controls a monitor path actually
                // has instead of the four a channel has.
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_none()
                        .items_center()
                        .w_full()
                        .h(px(console::BUTTONS_H))
                        .px(px(4.0))
                        .gap(px(2.0))
                        .child(control_room_toggle(
                            "monitor-mute",
                            &i18n.tr("mixer.monitor.mute"),
                            monitor.mute,
                            Colors::state_arm(),
                            base,
                            callbacks.on_monitor_toggle_mute.clone(),
                        ))
                        .child(control_room_toggle(
                            "monitor-dim",
                            &i18n.tr("mixer.monitor.dim"),
                            monitor.dim,
                            Colors::state_solo(),
                            base,
                            callbacks.on_monitor_toggle_dim.clone(),
                        ))
                        .child(control_room_toggle(
                            "monitor-mono",
                            &i18n.tr("mixer.monitor.mono"),
                            monitor.mono,
                            Colors::state_mute(),
                            base,
                            callbacks.on_monitor_toggle_mono.clone(),
                        )),
                ),
        )
        .child(console::name_plate(
            console::monitor_plate_fill(),
            None,
            i18n.tr("mixer.monitor.label"),
            false,
            false,
        ))
}

// ─── Public: Mixer Panel ─────────────────────────────────────────────────────

/// Strip columns above/below the visible viewport that are kept rendered to
/// prevent pop-in during horizontal mixer scrolling.
const MIXER_OVERSCAN: usize = 1;

/// Mixer channel-scroller scrollbar metrics. Same values the arrangement
/// scrollbars use, so both surfaces read as one control language.
const MIXER_SCROLLBAR_THICKNESS: f32 = 8.0;
const MIXER_SCROLLBAR_MIN_THUMB: f32 = 24.0;

/// The single coordinate transform behind the channel scrollbar: it sizes and
/// places the thumb, and it maps a pointer x back to a scroll offset. Drawing
/// and hit-testing both go through it, so a track click can never land the thumb
/// somewhere a drag would not.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct MixerScrollbarGeometry {
    pub track_w: f32,
    pub thumb_w: f32,
    pub thumb_left: f32,
    max_scroll_x: f32,
}

impl MixerScrollbarGeometry {
    /// `None` when the channel strips already fit, i.e. there is nothing to
    /// scroll and no bar should be drawn.
    pub fn new(scroll_x: f32, content_w: f32, view_w: f32, max_scroll_x: f32) -> Option<Self> {
        if max_scroll_x <= 0.5 || view_w <= 0.0 || content_w <= 0.0 {
            return None;
        }
        let track_w = view_w;
        let thumb_w = ((view_w / content_w) * track_w)
            .max(MIXER_SCROLLBAR_MIN_THUMB)
            .min(track_w);
        let progress = (scroll_x / max_scroll_x).clamp(0.0, 1.0);
        Some(Self {
            track_w,
            thumb_w,
            thumb_left: progress * (track_w - thumb_w).max(0.0),
            max_scroll_x,
        })
    }

    /// Scroll offset that centers the thumb on a pointer at `local_x` (pointer x
    /// relative to the scrollbar track's left edge).
    pub fn scroll_x_for_local_pointer(&self, local_x: f32) -> f32 {
        let track_range = (self.track_w - self.thumb_w).max(1.0);
        let local = (local_x - self.thumb_w * 0.5).clamp(0.0, track_range);
        ((local / track_range) * self.max_scroll_x).clamp(0.0, self.max_scroll_x)
    }
}

/// Horizontal scrollbar for the channel-strip scroller.
///
/// The scroller is virtualized against an owner-held `scroll_x` rather than a
/// GPUI `ScrollHandle`, so it gets no scrollbar for free: before this the only
/// way to move through the channels was the wheel, with nothing showing scroll
/// position or extent. Returns `None` when every strip already fits, so the bar
/// never covers the empty bay.
///
/// Geometry, drawing, and hit-testing share one transform: `track` is the full
/// scroller width, the thumb is sized by `view_w / content_w`, and both the
/// track click and the thumb drag center the thumb on the pointer. The track's
/// window bounds are captured in prepaint so pointer x maps to an offset
/// without estimating the panel origin.
fn mixer_horizontal_scrollbar(
    scroll_x: f32,
    content_w: f32,
    view_w: f32,
    max_scroll_x: f32,
    on_scroll: std::sync::Arc<dyn Fn(f32, &mut gpui::Window, &mut gpui::App) + 'static>,
) -> Option<gpui::AnyElement> {
    use gpui::canvas;
    use std::cell::Cell;
    use std::rc::Rc;

    let geometry = MixerScrollbarGeometry::new(scroll_x, content_w, view_w, max_scroll_x)?;

    // Written during prepaint, read by this frame's pointer handlers.
    let track_origin_x: Rc<Cell<Option<f32>>> = Rc::new(Cell::new(None));
    let measure = {
        let track_origin_x = track_origin_x.clone();
        canvas(
            move |bounds: gpui::Bounds<gpui::Pixels>, _w, _cx| {
                track_origin_x.set(Some(bounds.origin.x.into()));
            },
            |_b, _r, _w, _cx| {},
        )
        .absolute()
        .inset_0()
    };

    // Window pointer x -> scroll offset. Shared by the track click and the thumb
    // drag so a click never jumps somewhere a drag would not.
    let scroll_for_pointer = {
        let track_origin_x = track_origin_x.clone();
        move |pointer_x: f32| -> Option<f32> {
            let origin_x = track_origin_x.get()?;
            Some(geometry.scroll_x_for_local_pointer(pointer_x - origin_x))
        }
    };

    let on_track_down = {
        let on_scroll = on_scroll.clone();
        let scroll_for_pointer = scroll_for_pointer.clone();
        move |event: &MouseDownEvent, window: &mut gpui::Window, cx: &mut gpui::App| {
            if let Some(new_x) = scroll_for_pointer(event.position.x.into()) {
                on_scroll(new_x, window, cx);
            }
            cx.stop_propagation();
        }
    };

    let on_thumb_drag = {
        let on_scroll = on_scroll.clone();
        move |event: &DragMoveEvent<MixerScrollDrag>,
              window: &mut gpui::Window,
              cx: &mut gpui::App| {
            if let Some(new_x) = scroll_for_pointer(event.event.position.x.into()) {
                on_scroll(new_x, window, cx);
            }
        }
    };

    Some(
        div()
            .absolute()
            .left_0()
            .right_0()
            .bottom_0()
            .h(px(MIXER_SCROLLBAR_THICKNESS))
            .id("mixer-hscroll")
            .child(measure)
            .on_mouse_down(gpui::MouseButton::Left, on_track_down)
            .on_drag(MixerScrollDrag, |drag, _offset, _window, cx| {
                cx.new(|_| *drag)
            })
            .on_drag_move::<MixerScrollDrag>(on_thumb_drag)
            .child(
                div()
                    .absolute()
                    .left(px(geometry.thumb_left))
                    .top(px(2.0))
                    .bottom(px(2.0))
                    .w(px(geometry.thumb_w))
                    .rounded(px(crate::theme::radius::PILL))
                    .bg(Colors::with_alpha(Colors::text_primary(), 0.2)),
            )
            .into_any_element(),
    )
}

pub(crate) fn mixer_visible_item_range(
    strip_count: usize,
    scroll_x: f32,
    viewport_width: f32,
) -> std::ops::Range<usize> {
    let max_scroll_x = (strip_count as f32 * STRIP_WIDTH - viewport_width.max(0.0)).max(0.0);
    let scroll_x = scroll_x.clamp(0.0, max_scroll_x);
    let first_visible = (scroll_x / STRIP_WIDTH).floor() as usize;
    let visible_start = first_visible.saturating_sub(MIXER_OVERSCAN);
    let last_visible = ((scroll_x + viewport_width.max(0.0)) / STRIP_WIDTH).ceil() as usize;
    let visible_end = (last_visible + MIXER_OVERSCAN).min(strip_count);
    visible_start.min(visible_end)..visible_end
}

#[derive(Clone, Debug, Default)]
pub struct VstiOutputMeterState {
    pub level: f32,
    pub peak_hold: f32,
    pub clip: bool,
    /// Meter tick this entry was last published in. Bookkeeping for the
    /// meter path's prune pass: an entry still carrying the current
    /// generation is live, an older one is decaying toward removal. Kept
    /// here so liveness needs no parallel set of owned key strings — see
    /// `apply_engine_meters`.
    pub last_seen: u64,
}

pub fn vsti_output_meter_key(track_id: &str, insert_id: &str, channel: u8) -> String {
    let mut key = String::new();
    write_vsti_output_meter_key(&mut key, track_id, insert_id, channel);
    key
}

/// Format a meter key into a reusable buffer.
///
/// The meter path resolves one key per plugin output channel on every tick, so
/// on a project with thousands of VSTi output channels the allocating form
/// above is hundreds of microseconds of pure `String` churn per tick. Callers
/// on that path keep one buffer and rewrite it in place.
pub fn write_vsti_output_meter_key(
    buffer: &mut String,
    track_id: &str,
    insert_id: &str,
    channel: u8,
) {
    use std::fmt::Write;
    buffer.clear();
    let _ = write!(buffer, "{track_id}:{insert_id}:{channel}");
}

#[derive(Clone, Copy)]
pub(crate) enum MixerRenderItem {
    Track {
        track_index: usize,
    },
    /// One mixer sub-strip per REAL plugin output bus. `child_index` points at
    /// the backing `vsti-out:{insert}:bus:{bus_index}` model track that owns the
    /// per-bus mute/solo/fader/meter state; `bus_index` is the plugin output bus.
    VstiOutput {
        parent_index: usize,
        insert_index: usize,
        bus_index: u8,
        child_index: usize,
    },
}

pub fn mixer_render_item_count(
    tracks: &[TrackState],
    collapsed_vsti_output_groups: &HashSet<String>,
    hidden_channels: &HashSet<String>,
) -> usize {
    collect_mixer_render_items(tracks, collapsed_vsti_output_groups, hidden_channels).len()
}

fn channel_id_for_render_item(tracks: &[TrackState], item: &MixerRenderItem) -> String {
    match item {
        MixerRenderItem::Track { track_index } => tracks[*track_index].id.clone(),
        MixerRenderItem::VstiOutput { child_index, .. } => tracks[*child_index].id.clone(),
    }
}

pub fn mixer_strip_index_for_channel(
    tracks: &[TrackState],
    collapsed_vsti_output_groups: &HashSet<String>,
    hidden_channels: &HashSet<String>,
    channel_id: &str,
) -> Option<usize> {
    let items = collect_mixer_render_items(tracks, collapsed_vsti_output_groups, hidden_channels);
    items
        .iter()
        .position(|item| channel_id_for_render_item(tracks, item) == channel_id)
}

pub fn mixer_scroll_x_for_strip_index(
    strip_index: usize,
    viewport_width: f32,
    strip_count: usize,
) -> f32 {
    let total_content_w = strip_count as f32 * STRIP_WIDTH;
    let max_scroll_x = (total_content_w - viewport_width).max(0.0);
    let strip_x = strip_index as f32 * STRIP_WIDTH;
    let target = strip_x - (viewport_width - STRIP_WIDTH) * 0.5;
    target.clamp(0.0, max_scroll_x.max(0.0))
}

pub(crate) fn collect_mixer_render_items(
    tracks: &[TrackState],
    collapsed_vsti_output_groups: &HashSet<String>,
    hidden_channels: &HashSet<String>,
) -> Vec<MixerRenderItem> {
    // Index VSTi output children once. The old parent-by-parent full track scan
    // was O(n²), which became visible as multi-second mixer stalls at 1k tracks.
    let mut children_by_insert: std::collections::HashMap<&str, Vec<(u8, usize)>> =
        std::collections::HashMap::new();
    for (child_index, child) in tracks.iter().enumerate() {
        let Some(insert_id) = vsti_output_child_insert_id(&child.id) else {
            continue;
        };
        let Some(bus_index) = child
            .id
            .rsplit_once(":bus:")
            .and_then(|(_, bus)| bus.parse::<u8>().ok())
        else {
            continue;
        };
        children_by_insert
            .entry(insert_id)
            .or_default()
            .push((bus_index, child_index));
    }
    for children in children_by_insert.values_mut() {
        children.sort_unstable_by_key(|(bus_index, _)| *bus_index);
    }

    let mut items = Vec::with_capacity(tracks.len());
    for (track_index, track) in tracks.iter().enumerate() {
        // VSTi multi-out child tracks are model/engine route nodes. The visible
        // mixer sub-strips are injected from the parent insert below, so these
        // backing tracks should never render as ordinary BUS channel strips.
        if vsti_output_child_insert_id(&track.id).is_some() {
            continue;
        }
        if hidden_channels.contains(&track.id) {
            continue;
        }
        items.push(MixerRenderItem::Track { track_index });

        let Some(slot) = track.instrument_insert().filter(|slot| !slot.is_empty()) else {
            continue;
        };
        let group_key = vsti_output_group_key(&track.id, &slot.id);
        if collapsed_vsti_output_groups.contains(&group_key) {
            continue;
        }
        // One sub-strip per backing child track (real output bus). Iterating the
        // model child tracks — rather than re-deriving channel pairs here — keeps
        // the visible strips, the engine routes, and per-bus mute/solo/meters in
        // perfect 1:1 lockstep with `ensure_vsti_output_child_tracks`.
        for &(bus_index, child_index) in children_by_insert
            .get(slot.id.as_str())
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            let child_id = &tracks[child_index].id;
            if hidden_channels.contains(child_id) {
                continue;
            }
            items.push(MixerRenderItem::VstiOutput {
                parent_index: track_index,
                insert_index: 0,
                bus_index,
                child_index,
            });
        }
    }
    items
}

/// Cached center background for the empty mixer bay. Grid geometry is rebuilt
/// only when width/height changes — never during paint from routing/session data.
#[derive(Clone, Copy, Debug, Default)]
struct MixerCenterGridCache {
    width_q: i64,
    height_q: i64,
    rebuild_count: u64,
}

thread_local! {
    static MIXER_CENTER_GRID: std::cell::RefCell<MixerCenterGridCache> =
        const { std::cell::RefCell::new(MixerCenterGridCache {
            width_q: 0,
            height_q: 0,
            rebuild_count: 0,
        }) };
}

/// Lightweight empty mixer center: solid background + optional batched vertical
/// grid lines. No strip layout nodes, no per-frame Vec allocation.
pub fn mixer_center_lightweight(width: f32, height: f32) -> impl IntoElement {
    use gpui::canvas;

    let width = width.max(0.0);
    let height = height.max(STRIP_MIN_HEIGHT);
    let width_q = (width * 4.0).round() as i64;
    let height_q = (height * 4.0).round() as i64;

    MIXER_CENTER_GRID.with(|cell| {
        let mut cache = cell.borrow_mut();
        if cache.width_q != width_q || cache.height_q != height_q {
            cache.width_q = width_q;
            cache.height_q = height_q;
            cache.rebuild_count = cache.rebuild_count.saturating_add(1);
            crate::perf::count("mixer_grid_rebuild_count", cache.rebuild_count);
        }
    });

    div()
        .flex_1()
        .min_w(px(0.0))
        .h_full()
        .relative()
        .bg(Colors::surface_window())
        .child(
            canvas(
                move |bounds, _window, _cx| {
                    let _ = bounds;
                },
                move |bounds, (), window, _cx| {
                    let _scope = crate::perf::PerfScope::enter("MixerCenter");
                    crate::perf::count("mixer_center_paint_count", 1);
                    paint_mixer_center_grid(bounds, width, window);
                },
            )
            .absolute()
            .inset_0(),
        )
}

fn paint_mixer_center_grid(
    bounds: gpui::Bounds<gpui::Pixels>,
    width: f32,
    window: &mut gpui::Window,
) {
    use gpui::{fill, point, px, size, Bounds};

    let origin_x = f32::from(bounds.origin.x);
    let origin_y = f32::from(bounds.origin.y);
    let h = f32::from(bounds.size.height).max(0.0);
    if h <= 0.0 || width <= 0.0 {
        return;
    }
    let stripe_count = (width / STRIP_WIDTH).ceil().clamp(0.0, 64.0) as usize;
    let color = Colors::border_subtle();
    for i in 0..stripe_count {
        let x = i as f32 * STRIP_WIDTH;
        if x > width {
            break;
        }
        let rect = Bounds::new(point(px(origin_x + x), px(origin_y)), size(px(1.0), px(h)));
        window.paint_quad(fill(rect, color));
    }
}

fn mixer_empty_bay(spare_w: f32, height: f32) -> impl IntoElement {
    mixer_center_lightweight(spare_w, height)
}

/// Build the draw-only mixer primitive snapshot for the GPU layer. Mirrors the
/// virtualized strip order in [`mixer_strip_scroller`] (strip *render position* ×
/// `STRIP_WIDTH`), and reproduces each strip's background / accent / separator so
/// the batched canvas paints exactly what the legacy `div` strips would. Reads
/// only cloned UI state — never the audio engine, project, or routing.
pub(crate) fn build_mixer_render_snapshot(
    tracks: &[TrackState],
    collapsed_vsti_output_groups: &HashSet<String>,
    hidden_mixer_channels: &HashSet<String>,
    selected_track_id: Option<&str>,
    selected_track_ids: &[String],
    scroll_x: f32,
    channel_area_width: f32,
    strip_height: f32,
) -> MixerRenderSnapshot {
    let _s = crate::perf::PerfScope::enter("MixerSnapshotBuild");
    let render_items =
        collect_mixer_render_items(tracks, collapsed_vsti_output_groups, hidden_mixer_channels);
    let mut strips = Vec::with_capacity(render_items.len());
    for (pos, item) in render_items.iter().enumerate() {
        let x = pos as f32 * STRIP_WIDTH;
        let geom = match *item {
            MixerRenderItem::Track { track_index } => {
                let track = &tracks[track_index];
                let selected =
                    mixer_strip_is_selected(&track.id, selected_track_id, selected_track_ids);
                MixerStripGeom {
                    x,
                    width: STRIP_WIDTH,
                    height: strip_height,
                    bg: console::strip_surface(selected),
                    plate: track.color,
                    separator: console::rule(),
                    selected,
                    is_master: false,
                    meter_l: track.meter_level_l,
                    meter_r: track.meter_level_r,
                    hovered: false,
                }
            }
            MixerRenderItem::VstiOutput {
                parent_index,
                child_index,
                ..
            } => {
                let parent = &tracks[parent_index];
                let child = &tracks[child_index];
                let selected =
                    mixer_strip_is_selected(&child.id, selected_track_id, selected_track_ids);
                MixerStripGeom {
                    x,
                    width: STRIP_WIDTH,
                    height: strip_height,
                    bg: console::sub_strip_surface(selected),
                    plate: parent.color,
                    separator: console::rule(),
                    selected,
                    is_master: false,
                    meter_l: child.meter_level_l,
                    meter_r: child.meter_level_r,
                    hovered: false,
                }
            }
        };
        strips.push(geom);
    }
    let viewport = MixerRenderViewport {
        channel_area_width,
        height: strip_height,
        scroll_x,
        master_x: None,
    };
    // The batched layer owns the strip's two flat fills: the body, and the name
    // plate at its foot. Both are painted at the same heights the div layer
    // would have used, so turning the GPU path on or off changes nothing on
    // screen. See `console::name_plate`.
    MixerRenderSnapshot::new(viewport, strips, None, console::PLATE_H, 1.0)
}

#[allow(clippy::too_many_arguments)]
pub fn mixer_panel(
    tracks: &[TrackState],
    master: &MasterBusState,
    monitor: &MonitorBusState,
    selected_track_id: Option<&str>,
    selected_track_ids: &[String],
    callbacks: MixerCallbacks,
    collapsed_vsti_output_groups: &HashSet<String>,
    hidden_mixer_channels: &HashSet<String>,
    vsti_output_meters: &std::collections::HashMap<String, VstiOutputMeterState>,
    scroll_x: f32,
    viewport_width: f32,
    viewport_height: f32,
    on_scroll: std::sync::Arc<dyn Fn(f32, &mut gpui::Window, &mut gpui::App) + 'static>,
    split: MixerSplit,
    tree_sidebar: Option<Entity<MixerTreeSidebar>>,
    tree_sidebar_enabled: bool,
    i18n: I18n,
) -> impl IntoElement {
    let _shell = crate::perf::PerfScope::enter("MixerShell");
    crate::perf::count("mixer_shell_layout_count", 1);

    let track_count = tracks.len();
    let on_master = callbacks.on_master_volume_change.clone();
    // What the panel actually has, and what a strip needs. They are not the
    // same number, and pretending they were is what broke the mixer at small
    // heights: the strips were laid out at their 320 px minimum inside a
    // shorter panel and simply overflowed it, so the plate, the toggles and
    // the foot of the fader were drawn outside the panel and clipped away —
    // with the GPU decoration painting at the taller height too, which is what
    // made the colours bleed past the bottom edge.
    //
    // Below the minimum the answer is to scroll, not to lie about the height.
    let body_height = (viewport_height - MIXER_SUB_HEADER_H).max(0.0);
    let strip_available_px = body_height.max(STRIP_MIN_HEIGHT);
    // A panel shorter than one strip scrolls vertically; a taller one must not
    // grow a scrollbar it never uses.
    let body_scrolls = body_height + 0.5 < STRIP_MIN_HEIGHT;

    // Optional GPU primitive layer: when active, the channel strips drop their
    // background / accent bar / separator and a single batched `canvas` paints
    // them behind the strip row. The master stays a native pinned strip (its
    // exact pinned x is not known until layout). Opt-in / reversible.
    let gpu_active = mixer_gpu_primitives_active();

    let strip_count =
        mixer_render_item_count(tracks, collapsed_vsti_output_groups, hidden_mixer_channels);

    // Empty mixer fast path — shell + tree (external) + cheap center + isolated master.
    if strip_count == 0 {
        crate::perf::count("mixer_shell_layout_count", 1);
        let channel_row = div()
            .flex()
            .flex_row()
            .flex_1()
            .min_h_0()
            .when(body_scrolls, |row| row.h(px(strip_available_px)))
            .child(mixer_center_lightweight(viewport_width, strip_available_px))
            .child(div().w(px(1.0)).h_full().bg(console::rule()))
            .child(mixer_master_strip_pinned(
                master,
                monitor,
                on_master,
                &callbacks,
                &split,
                strip_available_px,
                i18n,
            ));

        let content_row = if tree_sidebar_enabled {
            let mut row = div().flex().flex_row().flex_1().min_h_0();
            if let Some(sidebar) = tree_sidebar {
                row = row.child(sidebar);
            }
            row.child(channel_row)
        } else {
            channel_row
        };

        let content_row = mixer_body_scroller(content_row, body_scrolls);
        let split_for_move = split.clone();
        let split_for_end = split.clone();
        return div()
            .flex()
            .flex_col()
            .size_full()
            .overflow_hidden()
            .bg(Colors::surface_window())
            .on_drag_move::<MixerSplitDrag>(move |event: &DragMoveEvent<MixerSplitDrag>, w, cx| {
                let y: f32 = event.event.position.y.into();
                (split_for_move.on_action)(MixerSplitAction::ResizeMove(y), w, cx);
            })
            .on_mouse_up(gpui::MouseButton::Left, move |_e, w, cx| {
                (split_for_end.on_action)(MixerSplitAction::ResizeEnd, w, cx);
            })
            .child(mixer_sub_header(track_count, i18n))
            .child(content_row);
    }

    let strip_row = mixer_strip_scroller(
        tracks,
        selected_track_id,
        selected_track_ids,
        callbacks.clone(),
        collapsed_vsti_output_groups,
        hidden_mixer_channels,
        vsti_output_meters,
        scroll_x,
        viewport_width,
        strip_available_px,
        &split,
        on_scroll,
        gpu_active,
        i18n,
    );

    let master_block = mixer_master_strip_pinned(
        master,
        monitor,
        on_master,
        &callbacks,
        &split,
        strip_available_px,
        i18n,
    );

    let mut channel_row = div()
        .flex()
        .flex_row()
        .flex_1()
        .min_h_0()
        .when(body_scrolls, |row| row.h(px(strip_available_px)))
        .child(strip_row)
        .child(div().w(px(1.0)).h_full().bg(console::rule()))
        .child(master_block);

    if gpu_active {
        let snapshot = build_mixer_render_snapshot(
            tracks,
            collapsed_vsti_output_groups,
            hidden_mixer_channels,
            selected_track_id,
            selected_track_ids,
            scroll_x,
            viewport_width,
            strip_available_px,
        );
        let primitives = render_mixer_primitives(&snapshot);
        // Wrap so the batched canvas paints behind the strip/gutter/master row.
        channel_row = div()
            .relative()
            .flex_1()
            .min_h_0()
            .child(primitives)
            .child(channel_row.size_full());
    }

    let content_row = if tree_sidebar_enabled {
        let mut row = div().flex().flex_row().flex_1().min_h_0();
        if let Some(sidebar) = tree_sidebar {
            row = row.child(sidebar);
        }
        row.child(channel_row)
    } else {
        channel_row
    };
    let content_row = mixer_body_scroller(content_row, body_scrolls);

    let split_for_move = split.clone();
    let split_for_end = split.clone();

    div()
        .flex()
        .flex_col()
        .size_full()
        .overflow_hidden()
        .bg(Colors::surface_window())
        .on_drag_move::<MixerSplitDrag>(move |event: &DragMoveEvent<MixerSplitDrag>, w, cx| {
            let y: f32 = event.event.position.y.into();
            (split_for_move.on_action)(MixerSplitAction::ResizeMove(y), w, cx);
        })
        .on_mouse_up(gpui::MouseButton::Left, move |_e, w, cx| {
            (split_for_end.on_action)(MixerSplitAction::ResizeEnd, w, cx);
        })
        .child(mixer_sub_header(track_count, i18n))
        .child(content_row)
}

/// Wraps the mixer body so a panel shorter than one channel strip scrolls
/// instead of spilling its strips out of the bottom.
///
/// Only when it has to. An always-scrollable body would take the wheel away
/// from the horizontal strip scroller at every ordinary height, and the mixer
/// is a thing you scroll sideways.
fn mixer_body_scroller(body: gpui::Div, scrolls: bool) -> gpui::AnyElement {
    if !scrolls {
        return body.into_any_element();
    }
    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .id("mixer-body-scroll")
        .overflow_y_scroll()
        .child(body)
        .into_any_element()
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn mixer_strip_scroller(
    tracks: &[TrackState],
    selected_track_id: Option<&str>,
    selected_track_ids: &[String],
    callbacks: MixerCallbacks,
    collapsed_vsti_output_groups: &HashSet<String>,
    hidden_mixer_channels: &HashSet<String>,
    vsti_output_meters: &std::collections::HashMap<String, VstiOutputMeterState>,
    scroll_x: f32,
    viewport_width: f32,
    strip_available_px: f32,
    split: &MixerSplit,
    on_scroll: std::sync::Arc<dyn Fn(f32, &mut gpui::Window, &mut gpui::App) + 'static>,
    gpu_decor: bool,
    i18n: I18n,
) -> impl IntoElement {
    let _scope = crate::perf::PerfScope::enter("MixerStripScroller");
    crate::perf::count("mixer_strip_layout_count", 1);
    crate::perf::count("mixer_strip_paint_count", 1);

    let render_items =
        collect_mixer_render_items(tracks, collapsed_vsti_output_groups, hidden_mixer_channels);
    let strip_count = render_items.len();
    crate::perf::count("mixer_strips", tracks.len() as u64);

    let total_content_w = strip_count as f32 * STRIP_WIDTH;
    let max_scroll_x = (total_content_w - viewport_width).max(0.0);
    let scroll_x = scroll_x.clamp(0.0, max_scroll_x.max(0.0));
    let spare_channel_w = (viewport_width - total_content_w).max(0.0);

    let visible_range = mixer_visible_item_range(strip_count, scroll_x, viewport_width);
    let visible_start = visible_range.start;
    let visible_end = visible_range.end;

    let left_spacer_w = visible_start as f32 * STRIP_WIDTH;
    let right_spacer_w = strip_count.saturating_sub(visible_end) as f32 * STRIP_WIDTH;

    crate::perf::count(
        "visible_mixer_strips",
        visible_end.saturating_sub(visible_start) as u64,
    );

    let visible_strips: Vec<gpui::AnyElement> = render_items[visible_start..visible_end]
        .iter()
        .map(|item| match *item {
            MixerRenderItem::Track { track_index } => {
                let track = &tracks[track_index];
                let is_sel =
                    mixer_strip_is_selected(&track.id, selected_track_id, selected_track_ids);
                let vsti_group_expanded = track
                    .instrument_insert()
                    .filter(|slot| !slot.is_empty())
                    .map(|slot| {
                        !collapsed_vsti_output_groups
                            .contains(&vsti_output_group_key(&track.id, &slot.id))
                    });
                channel_strip(
                    track,
                    tracks,
                    track_index,
                    is_sel,
                    &callbacks,
                    split,
                    strip_available_px,
                    vsti_group_expanded,
                    gpu_decor,
                    i18n,
                )
                .into_any_element()
            }
            MixerRenderItem::VstiOutput {
                parent_index,
                insert_index,
                bus_index,
                child_index,
            } => {
                let parent = &tracks[parent_index];
                let bus_counts = parent.inserts[insert_index]
                    .output_bus_channel_counts
                    .clone();
                let child_selected = mixer_strip_is_selected(
                    &tracks[child_index].id,
                    selected_track_id,
                    selected_track_ids,
                );
                vsti_output_sub_strip(
                    parent,
                    &tracks[child_index],
                    tracks,
                    &parent.inserts[insert_index].id,
                    bus_index,
                    &bus_counts,
                    selected_track_id,
                    child_selected,
                    vsti_output_meters,
                    &callbacks,
                    split,
                    strip_available_px,
                    gpu_decor,
                    i18n,
                )
                .into_any_element()
            }
        })
        .collect();

    let on_scroll_wheel = {
        let on_scroll = on_scroll.clone();
        move |event: &gpui::ScrollWheelEvent, window: &mut gpui::Window, cx: &mut gpui::App| {
            let (dx, dy) = match &event.delta {
                gpui::ScrollDelta::Pixels(p) => (f32::from(p.x), f32::from(p.y)),
                gpui::ScrollDelta::Lines(l) => (l.x * STRIP_WIDTH, l.y * STRIP_WIDTH * 0.5),
            };
            let delta = if event.modifiers.shift {
                if dx.abs() > f32::EPSILON {
                    dx
                } else {
                    dy
                }
            } else if dx.abs() >= dy.abs() {
                dx
            } else {
                dy
            };
            if delta.abs() <= f32::EPSILON || max_scroll_x <= 0.0 {
                return;
            }
            let new_x = (scroll_x + delta).clamp(0.0, max_scroll_x.max(0.0));
            on_scroll(new_x, window, cx);
            window.prevent_default();
            cx.stop_propagation();
        }
    };

    // Drawn last so the bar sits above the strip row; hidden entirely when every
    // channel already fits, which is also when the empty bay is showing.
    let scrollbar = mixer_horizontal_scrollbar(
        scroll_x,
        total_content_w,
        viewport_width,
        max_scroll_x,
        on_scroll.clone(),
    );

    div()
        .flex_1()
        .min_w(px(0.0))
        .h_full()
        .relative()
        .overflow_hidden()
        .on_scroll_wheel(on_scroll_wheel)
        .when(spare_channel_w > 0.0, |d| {
            // Keep the empty bay confined to the unused right-hand region.
            // A normal flex child expands across the entire scroller and, in
            // GPU-decoration mode, paints over every strip when the window is
            // wide enough to show all channels.
            d.child(
                div()
                    .absolute()
                    .right_0()
                    .top_0()
                    .bottom_0()
                    .w(px(spare_channel_w))
                    .overflow_hidden()
                    .child(mixer_empty_bay(spare_channel_w, strip_available_px)),
            )
        })
        .child(
            div()
                .absolute()
                .left(px(-scroll_x))
                .top_0()
                .bottom_0()
                .flex()
                .flex_row()
                .h_full()
                .min_h(px(STRIP_MIN_HEIGHT))
                .when(left_spacer_w > 0.0, |d| {
                    d.child(
                        div()
                            .w(px(left_spacer_w))
                            .h_full()
                            .flex_none()
                            .bg(Colors::surface_window()),
                    )
                })
                .children(visible_strips)
                .when(right_spacer_w > 0.0, |d| {
                    d.child(
                        div()
                            .w(px(right_spacer_w))
                            .h_full()
                            .flex_none()
                            .bg(Colors::surface_window()),
                    )
                }),
        )
        .children(scrollbar)
}

/// The pinned right-hand pair: Master then Monitor, sharing one divider.
///
/// Single composer for both mixer surfaces — the docked panel's
/// `MixerMasterStripView` entity and the detached Mixer Window both route
/// here, so the two strips can never drift apart between them.
pub(crate) fn mixer_master_strip_pinned(
    master: &MasterBusState,
    monitor: &MonitorBusState,
    on_master: std::sync::Arc<dyn Fn(&f32, &mut gpui::Window, &mut gpui::App) + 'static>,
    callbacks: &MixerCallbacks,
    split: &MixerSplit,
    strip_available_px: f32,
    i18n: I18n,
) -> impl IntoElement {
    let _scope = crate::perf::PerfScope::enter("MixerMasterStrip");
    div()
        .flex()
        .flex_row()
        .flex_none()
        .h_full()
        .child(master_strip(
            master,
            on_master,
            callbacks,
            split,
            strip_available_px,
            i18n,
        ))
        .child(monitor_strip(
            monitor,
            callbacks,
            split,
            strip_available_px,
            i18n,
        ))
}

#[cfg(test)]
mod vsti_output_label_tests {
    use super::vsti_output_bus_label;

    #[test]
    fn mixed_mono_and_stereo_buses_get_real_layout_labels() {
        // bus0 mono (ch1), bus1 mono (ch2), bus2 stereo (ch3/4).
        let counts = [1u8, 1, 2];
        assert_eq!(vsti_output_bus_label(&counts, 0), "Out 1 (Mono / Ch 1)");
        assert_eq!(vsti_output_bus_label(&counts, 1), "Out 2 (Mono / Ch 2)");
        assert_eq!(vsti_output_bus_label(&counts, 2), "Out 3 (Stereo / Ch 3/4)");
    }

    #[test]
    fn multichannel_bus_label_shows_full_channel_range() {
        // bus0 stereo (ch1/2), bus1 quad (ch3-6).
        let counts = [2u8, 4];
        assert_eq!(vsti_output_bus_label(&counts, 1), "Out 2 (Multi / Ch 3-6)");
    }

    #[test]
    fn single_multichannel_bus_labels_each_flat_pair() {
        assert_eq!(vsti_output_bus_label(&[8u8], 0), "Out 1 (Stereo / Ch 1/2)");
        assert_eq!(vsti_output_bus_label(&[8u8], 1), "Out 2 (Stereo / Ch 3/4)");
    }
}

#[cfg(test)]
mod collapse_filter_tests {
    use super::{collect_mixer_render_items, vsti_output_group_key, MixerRenderItem};
    use crate::components::timeline::timeline_state::{
        CreateTrackOptions, InputMonitorMode, InsertPluginFormat, TimelineState, TrackType,
    };
    use std::collections::HashSet;

    fn drum_scenario(output_bus_layout: &[u32]) -> (TimelineState, String, String) {
        let mut state = TimelineState::default();
        let track_id = state.create_track(CreateTrackOptions {
            name: "Drums".to_string(),
            track_type: TrackType::Instrument,
            color: crate::theme::Colors::track_color_for_index(0),
            volume: 1.0,
            pan: 0.0,
            armed: false,
            input_monitor: InputMonitorMode::Off,
        });
        let slot = state.ensure_insert_slot_at(&track_id, 0).expect("slot");
        state.set_insert_plugin(
            &track_id,
            &slot,
            "drums".to_string(),
            Some(std::path::PathBuf::from("C:/p/drums.vst3")),
            InsertPluginFormat::Vst3,
            None,
            "drums".to_string(),
        );
        state.set_insert_output_bus_layout(&track_id, &slot, output_bus_layout);
        let output_channels = output_bus_layout.iter().copied().sum::<u32>().max(2);
        state.auto_enable_detected_insert_outputs(&track_id, &slot, output_channels);
        (state, track_id, slot)
    }

    #[test]
    fn expanded_includes_children_collapsed_hides_only_children() {
        let (state, track_id, slot) = drum_scenario(&[2, 2, 2]);

        // Expanded (empty collapsed set): parent + one sub-strip per REAL output
        // bus (bus 0/1/2), never split per channel into 4 strips.
        let expanded = collect_mixer_render_items(&state.tracks, &HashSet::new(), &HashSet::new());
        assert_eq!(expanded.len(), 4);
        assert_eq!(
            expanded
                .iter()
                .filter(|item| matches!(item, MixerRenderItem::VstiOutput { .. }))
                .count(),
            3
        );

        // Collapsed: child strips filtered out, only the parent strip remains.
        let mut collapsed = HashSet::new();
        collapsed.insert(vsti_output_group_key(&track_id, &slot));
        let collapsed_items =
            collect_mixer_render_items(&state.tracks, &collapsed, &HashSet::new());
        assert_eq!(
            collapsed_items.len(),
            1,
            "collapse hides only the child strips, never the parent"
        );

        // The model still holds every track — collapse changed the VIEW only.
        assert_eq!(state.tracks.len(), 4);
    }

    #[test]
    fn mixed_mono_and_stereo_buses_show_one_strip_per_bus() {
        // Acceptance: outputs 1: mono, 2: mono, 3/4: stereo → THREE output
        // strips (one per bus), never two blindly-paired stereo strips.
        let (state, _track_id, _slot) = drum_scenario(&[1, 1, 2]);
        let expanded = collect_mixer_render_items(&state.tracks, &HashSet::new(), &HashSet::new());
        assert_eq!(
            expanded
                .iter()
                .filter(|item| matches!(item, MixerRenderItem::VstiOutput { .. }))
                .count(),
            3
        );
    }

    #[test]
    fn expanded_single_multichannel_bus_includes_flat_pair_children() {
        let (state, track_id, slot) = drum_scenario(&[8]);

        // Expanded: one parent strip + one sub-strip per flat stereo pair of the
        // single 8-channel bus (Ch 1/2, 3/4, 5/6, 7/8).
        let expanded = collect_mixer_render_items(&state.tracks, &HashSet::new(), &HashSet::new());
        assert_eq!(expanded.len(), 5);
        assert_eq!(
            expanded
                .iter()
                .filter(|item| matches!(item, MixerRenderItem::VstiOutput { .. }))
                .count(),
            4
        );

        // Collapsed: only the view list hides the children; the state still keeps
        // every child track for routing and meters.
        let mut collapsed = HashSet::new();
        collapsed.insert(vsti_output_group_key(&track_id, &slot));
        let collapsed_items =
            collect_mixer_render_items(&state.tracks, &collapsed, &HashSet::new());
        assert_eq!(collapsed_items.len(), 1);
        assert_eq!(state.tracks.len(), 5);
    }
}

#[cfg(test)]
mod mixer_virtualization_tests {
    use super::mixer_visible_item_range;

    #[test]
    fn thousand_strip_detail_window_stays_bounded() {
        let first = mixer_visible_item_range(1_000, 0.0, 880.0);
        let middle = mixer_visible_item_range(1_000, 44_000.0, 880.0);
        let end = mixer_visible_item_range(1_000, f32::MAX, 880.0);
        assert!(first.len() <= 12);
        assert!(middle.len() <= 12);
        assert!(end.len() <= 12);
        assert_eq!(end.end, 1_000);
    }
}

#[cfg(test)]
mod vsti_meter_key_tests {
    use super::{vsti_output_meter_key, write_vsti_output_meter_key, VstiOutputMeterState};

    /// The buffered form is what the meter path uses every tick; it has to
    /// produce byte-identical keys to the allocating form the mixer render
    /// path still calls, or lookups would silently miss.
    #[test]
    fn buffered_and_allocating_keys_are_identical() {
        let cases = [
            ("track-1", "insert-7", 1u8),
            ("vsti-out:abc:bus:3", "slot-0", 32),
            ("", "", 0),
        ];
        let mut buffer = String::new();
        for (track_id, insert_id, channel) in cases {
            write_vsti_output_meter_key(&mut buffer, track_id, insert_id, channel);
            assert_eq!(
                buffer,
                vsti_output_meter_key(track_id, insert_id, channel),
                "key mismatch for {track_id}/{insert_id}/{channel}"
            );
        }
    }

    /// Reusing one buffer must never leak the previous key's tail.
    #[test]
    fn reused_buffer_never_keeps_a_longer_previous_key() {
        let mut buffer = String::new();
        write_vsti_output_meter_key(&mut buffer, "a-very-long-track-id", "insert-12", 31);
        write_vsti_output_meter_key(&mut buffer, "t", "i", 1);
        assert_eq!(buffer, "t:i:1");
    }

    /// Liveness is now a generation stamp rather than a set of live keys.
    /// A meter published this tick is live; one that stopped publishing must
    /// fall through to the decay branch exactly as before.
    #[test]
    fn generation_stamp_separates_live_from_stale_entries() {
        let generation = 41u64;
        let live = VstiOutputMeterState {
            level: 0.5,
            peak_hold: 0.5,
            clip: false,
            last_seen: generation,
        };
        let stale = VstiOutputMeterState {
            level: 0.5,
            peak_hold: 0.5,
            clip: false,
            last_seen: generation - 1,
        };
        assert!(live.last_seen == generation, "published this tick");
        assert!(stale.last_seen != generation, "not published this tick");
        // A fresh entry starts stale, so it only survives once the meter loop
        // stamps it — matching `or_default()` + live-set insertion before.
        assert_eq!(VstiOutputMeterState::default().last_seen, 0);
    }
}

#[cfg(test)]
mod mixer_scrollbar_tests {
    use super::{MixerScrollbarGeometry, MIXER_SCROLLBAR_MIN_THUMB, STRIP_WIDTH};

    /// The 2k-channel session from the bug report: 88px strips in an ~880px bay.
    fn big_session() -> (f32, f32, f32) {
        let content_w = 2_000.0 * STRIP_WIDTH;
        let view_w = 880.0;
        (content_w, view_w, content_w - view_w)
    }

    #[test]
    fn no_bar_when_every_channel_already_fits() {
        // Content narrower than the bay -> nothing to scroll, nothing to draw,
        // so the bar never covers the empty channel bay.
        assert!(MixerScrollbarGeometry::new(0.0, 400.0, 880.0, 0.0).is_none());
    }

    #[test]
    fn thumb_stays_grabbable_and_inside_the_track() {
        let (content_w, view_w, max_scroll_x) = big_session();
        for scroll_x in [0.0, max_scroll_x * 0.5, max_scroll_x] {
            let geometry =
                MixerScrollbarGeometry::new(scroll_x, content_w, view_w, max_scroll_x).unwrap();
            assert!(
                geometry.thumb_w >= MIXER_SCROLLBAR_MIN_THUMB,
                "a 2000-channel thumb must stay grabbable, got {}",
                geometry.thumb_w
            );
            assert!(geometry.thumb_left >= 0.0);
            assert!(
                geometry.thumb_left + geometry.thumb_w <= geometry.track_w + 0.01,
                "thumb must not overhang the track"
            );
        }
    }

    #[test]
    fn thumb_position_tracks_scroll_offset() {
        let (content_w, view_w, max_scroll_x) = big_session();
        let start = MixerScrollbarGeometry::new(0.0, content_w, view_w, max_scroll_x).unwrap();
        let end =
            MixerScrollbarGeometry::new(max_scroll_x, content_w, view_w, max_scroll_x).unwrap();
        assert_eq!(start.thumb_left, 0.0);
        assert!((end.thumb_left + end.thumb_w - end.track_w).abs() < 0.01);
    }

    #[test]
    fn pointer_maps_back_to_the_offset_that_drew_the_thumb() {
        // Drawing and hit-testing share one transform: clicking the center of a
        // drawn thumb must resolve to the scroll offset it was drawn for.
        let (content_w, view_w, max_scroll_x) = big_session();
        for scroll_x in [0.0, max_scroll_x * 0.25, max_scroll_x * 0.5, max_scroll_x] {
            let geometry =
                MixerScrollbarGeometry::new(scroll_x, content_w, view_w, max_scroll_x).unwrap();
            let thumb_center = geometry.thumb_left + geometry.thumb_w * 0.5;
            let resolved = geometry.scroll_x_for_local_pointer(thumb_center);
            assert!(
                (resolved - scroll_x).abs() < 1.0,
                "round trip drifted: drew at {scroll_x}, pointer resolved {resolved}"
            );
        }
    }

    #[test]
    fn pointer_outside_the_track_clamps_to_the_scroll_range() {
        let (content_w, view_w, max_scroll_x) = big_session();
        let geometry = MixerScrollbarGeometry::new(0.0, content_w, view_w, max_scroll_x).unwrap();
        assert_eq!(geometry.scroll_x_for_local_pointer(-500.0), 0.0);
        assert_eq!(
            geometry.scroll_x_for_local_pointer(geometry.track_w + 500.0),
            max_scroll_x
        );
    }
}

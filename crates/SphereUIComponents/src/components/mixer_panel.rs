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
//! * Strip internals are a vertical stack in fixed console order — colour bar,
//!   type row, I/O, inserts, sends, pan, fader bay, toggles, name plate — so a
//!   row of strips reads across as well as down. Only the two racks resize;
//!   every other row is the same height on every strip, which is what keeps
//!   all the faders in one bay and all the plates on one baseline.
//! * The header's SHOW and STRIPS buttons ([`MixerView`]) hide I/O, Inserts or
//!   Sends and switch Narrow / Wide strips, for every strip at once. See
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
use crate::components::mixer_meter_layer::{
    clip_region, meter_slot, peak_readout_text, peak_slot, ClipRegion, MeterSource,
    SharedMeterLayout, VstiMeterFallback, PEAK_TEXT_SIZE,
};
use crate::components::mixer_render::{MixerRenderSnapshot, MixerRenderViewport, MixerStripGeom};
use crate::components::mixer_surface::{mixer_gpu_primitives_active, render_mixer_primitives};
use crate::components::mixer_tree_sidebar_view::MixerTreeSidebar;
use crate::components::panel::FxSlotDrag;
use crate::components::reorder::{
    drop_over_highlight, insert_drop_forwarder, insert_drop_target, insert_drop_target_also,
    same_list_anchor, slot_drop_target, DragRefusal, DropIndicator, DropSlot, InsertDropTarget,
};
use crate::components::sidebar::BrowserDragItem;
use crate::components::timeline::timeline_state::{
    is_vsti_output_child_track_id, volume, vsti_output_bus_flat_range,
    vsti_output_child_channels_for_bus_layout, vsti_output_child_insert_id, GroupTree,
    InsertLoadStatus, InsertSlotState, ListenMode, MasterBusState, MonitorBusState, SendSlotState,
    TrackOutputRouting, TrackState, TrackType, MASTER_TRACK_ID,
};
use crate::components::timeline::vu_meter::meter_surface;
use crate::i18n::I18n;
use crate::theme::{typography, Colors};

mod callbacks;
mod console;
pub(crate) use console::describe_room_position;
mod drag;
mod split;
pub use callbacks::*;
use console::{type_scale, RackPlus, SlotTone, ToggleState};
use drag::SendSlotDrag;
pub use split::*;

/// True when a mixer strip should paint as selected (multi-select aware).
///
/// The same rule as `TimelineSelection::is_track_selected`, so a strip and
/// its arrangement header always agree: the selected tracks, or the primary
/// alone when the multi-selection does not hold it (a track picked by an
/// import or a new-track action, which set only the primary).
#[inline]
fn mixer_strip_is_selected(track_id: &str, primary: Option<&str>, selected_ids: &[String]) -> bool {
    match primary {
        Some(primary)
            if selected_ids.is_empty() || !selected_ids.iter().any(|id| id == primary) =>
        {
            primary == track_id
        }
        Some(_) => selected_ids.iter().any(|id| id == track_id),
        None => false,
    }
}

/// Maximum insert slots per track. Once reached, the trailing empty "+ Add
/// Insert" slot and the INSERTS header "+" are hidden/disabled. The same limit
/// a plug-in dragged in from another channel is held to.
use crate::components::timeline::timeline_state::MAX_INSERT_SLOTS;

// ─── Mixer sub-header ("Mixer  N ch") ────────────────────────────────────────

/// Height of the mixer header above the console: title, SHOW, STRIPS.
pub const MIXER_SUB_HEADER_H: f32 = 36.0;

/// Clears every solo, from the mixer header. `None` where the surface has no
/// timeline to clear it on (the detached window's snapshot).
pub type ClearSolosCb = std::sync::Arc<dyn Fn(&mut gpui::Window, &mut gpui::App) + 'static>;

/// The inserts rack's channel bypass for `owner_id`, from the bypass state of
/// its loaded effect slots (instrument slots are not effects). `None` when
/// the channel has no effect to bypass or the surface cannot toggle it.
fn channel_bypass(
    owner_id: &str,
    mut effects_bypassed: impl Iterator<Item = bool>,
    callbacks: &MixerCallbacks,
    i18n: I18n,
) -> Option<console::RackBypass> {
    let on_toggle = callbacks.on_toggle_channel_bypass.clone()?;
    let first = effects_bypassed.next()?;
    let bypassed = first && effects_bypassed.all(|bypassed| bypassed);
    let owner_id = owner_id.to_string();
    Some(console::RackBypass {
        id: gpui::SharedString::from(format!("insert-channel-bypass-{owner_id}")),
        bypassed,
        tooltip: if bypassed {
            i18n.tr_or(
                "mixer.enable-channel-plugins",
                "Enable this channel's plug-ins",
            )
        } else {
            i18n.tr_or(
                "mixer.bypass-channel-plugins",
                "Bypass this channel's plug-ins",
            )
        },
        on_click: std::sync::Arc::new(move |window, cx| on_toggle(&owner_id, window, cx)),
    })
}

/// What the mixer header shows and offers.
pub struct MixerHeader<'a> {
    /// Channel strips on screen once hidden channels and folded groups are
    /// left out, and channels in the project.
    pub visible_channels: usize,
    pub total_channels: usize,
    pub soloed: usize,
    /// `None` where the surface cannot clear solos.
    pub on_clear_solos: Option<ClearSolosCb>,
    /// Opens Add Track. `None` where the surface cannot.
    pub on_add_track: Option<ClearSolosCb>,
    /// The strips' sections and width, and where the SHOW / STRIPS buttons
    /// send their changes.
    pub split: &'a MixerSplit,
}

/// A header toggle: one of the SHOW sections, or Narrow / Wide.
fn header_pill(
    id: &'static str,
    label: String,
    on: bool,
    on_click: impl Fn(&mut gpui::Window, &mut gpui::App) + 'static,
) -> impl IntoElement {
    let rest = if on {
        Colors::surface_raised()
    } else {
        Colors::surface_panel_alt()
    };
    let hover = Colors::composite(rest, Colors::state_hover());
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .h(px(22.0))
        .px(px(9.0))
        .rounded(px(crate::theme::radius::CONTROL_SM))
        .bg(rest)
        .border(px(1.0))
        .border_color(if on {
            Colors::accent_primary()
        } else {
            Colors::border_default()
        })
        .text_size(px(typography::DENSE_LABEL))
        .text_color(if on {
            Colors::text_primary()
        } else {
            Colors::text_secondary()
        })
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(move |s| s.bg(hover))
        .on_click(move |_, window, cx| on_click(window, cx))
        .child(label)
}

/// A group of header toggles with its caption.
fn header_group(caption: String, pills: Vec<gpui::AnyElement>) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .flex_none()
        .items_center()
        .gap(px(3.0))
        .child(
            div()
                .mr(px(4.0))
                .text_size(px(typography::DENSE_CAPTION))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(Colors::text_faint())
                .child(caption),
        )
        .children(pills)
}

pub fn mixer_sub_header(header: MixerHeader<'_>, i18n: I18n) -> impl IntoElement {
    let view = header.split.view;
    let section_pill = |id: &'static str, label: String, section: MixerSection| {
        let on_action = header.split.on_action.clone();
        header_pill(id, label, view.shows(section), move |window, cx| {
            on_action(MixerSplitAction::ToggleSection(section), window, cx)
        })
        .into_any_element()
    };
    let width_pill = |id: &'static str, label: String, wide: bool| {
        let on_action = header.split.on_action.clone();
        header_pill(id, label, view.wide == wide, move |window, cx| {
            on_action(MixerSplitAction::SetWide(wide), window, cx)
        })
        .into_any_element()
    };

    // Any solo silences every channel that is not soloed — including one on a
    // strip scrolled out of view, or on a channel in a collapsed folder. The
    // header says so, and clears it, from wherever the mixer is scrolled.
    let solo_button = (header.soloed > 0).then(|| {
        let solo = Colors::state_solo();
        let label = format!(
            "{} ({})",
            i18n.tr_or("mixer.header.clear-solo", "Clear solo"),
            header.soloed
        );
        let mut button = div()
            .id("mixer-clear-solos")
            .flex()
            .flex_none()
            .items_center()
            .h(px(22.0))
            .px(px(9.0))
            .rounded(px(crate::theme::radius::CONTROL_SM))
            .bg(solo)
            .border(px(1.0))
            .border_color(solo)
            .text_size(px(typography::DENSE_LABEL))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(Colors::on_color(solo))
            .child(label);
        if let Some(on_clear) = header.on_clear_solos {
            let hover = Colors::composite(solo, Colors::state_hover());
            button = button
                .cursor(gpui::CursorStyle::PointingHand)
                .hover(move |s| s.bg(hover))
                .tooltip(crate::components::controls::fb_tooltip(
                    "Every channel that is not soloed is silent. Click to clear all solos.",
                ))
                .on_click(move |_, window, cx| on_clear(window, cx));
        }
        button
    });

    let add_track = header.on_add_track.map(|on_add| {
        let rest = Colors::surface_raised();
        let hover = Colors::composite(rest, Colors::state_hover());
        div()
            .id("mixer-add-track")
            .flex()
            .flex_row()
            .flex_none()
            .items_center()
            .gap(px(6.0))
            .h(px(22.0))
            .px(px(9.0))
            .rounded(px(crate::theme::radius::CONTROL_SM))
            .bg(rest)
            .border(px(1.0))
            .border_color(Colors::border_default())
            .text_size(px(typography::DENSE_LABEL))
            .text_color(Colors::text_primary())
            .cursor(gpui::CursorStyle::PointingHand)
            .hover(move |s| s.bg(hover))
            .on_click(move |_, window, cx| on_add(window, cx))
            .child(
                svg()
                    .path(assets::ICON_PLUS_PATH)
                    .w(px(10.0))
                    .h(px(10.0))
                    .text_color(Colors::text_secondary()),
            )
            .child(i18n.tr_or("mixer.header.add-track", "Add track"))
    });

    div()
        .flex()
        .flex_row()
        .flex_none()
        .items_center()
        .gap(px(16.0))
        .h(px(MIXER_SUB_HEADER_H))
        .px(px(10.0))
        .overflow_hidden()
        .bg(Colors::surface_panel_alt())
        .border_b(px(1.0))
        .border_color(Colors::border_subtle())
        .child(
            div()
                .flex()
                .flex_row()
                .flex_none()
                .items_center()
                .gap(px(8.0))
                .child(
                    svg()
                        .path(assets::ICON_SLIDERS_HORIZONTAL_PATH)
                        .w(px(14.0))
                        .h(px(14.0))
                        .text_color(Colors::text_secondary()),
                )
                .child(
                    div()
                        .text_size(px(typography::DENSE_LABEL + 1.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(Colors::text_primary())
                        .child(i18n.tr("mixer.title")),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .px(px(6.0))
                        .py(px(1.0))
                        .rounded(px(crate::theme::radius::MICRO))
                        .bg(Colors::surface_raised())
                        .text_size(px(typography::DENSE_CAPTION))
                        .text_color(Colors::text_secondary())
                        .child(if header.visible_channels == header.total_channels {
                            format!("{} ch", header.total_channels)
                        } else {
                            format!(
                                "{} of {} ch",
                                header.visible_channels, header.total_channels
                            )
                        }),
                ),
        )
        .child(header_group(
            i18n.tr_or("mixer.header.show", "SHOW"),
            vec![
                section_pill("mixer-show-io", "I/O".to_string(), MixerSection::Io),
                section_pill(
                    "mixer-show-inserts",
                    i18n.tr_or("mixer.header.inserts", "Inserts"),
                    MixerSection::Inserts,
                ),
                section_pill(
                    "mixer-show-sends",
                    i18n.tr_or("mixer.header.sends", "Sends"),
                    MixerSection::Sends,
                ),
            ],
        ))
        .child(header_group(
            i18n.tr_or("mixer.header.strips", "STRIPS"),
            vec![
                width_pill(
                    "mixer-strips-narrow",
                    i18n.tr_or("mixer.header.narrow", "Narrow"),
                    false,
                ),
                width_pill(
                    "mixer-strips-wide",
                    i18n.tr_or("mixer.header.wide", "Wide"),
                    true,
                ),
            ],
        ))
        .child(div().flex_1())
        .children(solo_button)
        .children(add_track)
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
            console::TOGGLE_H,
            on_mute,
        ))
        .child(console::toggle(
            ("mix-s-btn", id_num).into(),
            "S",
            solo_state,
            Colors::state_solo(),
            base,
            console::TOGGLE_H,
            on_solo,
        ))
        .child(console::toggle(
            ("mix-r-btn", id_num).into(),
            "R",
            track.armed.into(),
            Colors::state_arm(),
            base,
            console::TOGGLE_H,
            on_arm,
        ))
        .child(console::toggle(
            ("mix-i-btn", id_num).into(),
            "I",
            track.input_monitor.is_active(track.armed).into(),
            Colors::state_monitor(),
            base,
            console::TOGGLE_H,
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
            console::LISTEN_TOGGLE_H,
            on_pfl,
        ))
        .child(console::toggle(
            ("mix-afl-btn", id_num).into(),
            "AFL",
            (track.listen == ListenMode::Afl).into(),
            Colors::accent_primary(),
            base,
            console::LISTEN_TOGGLE_H,
            on_afl,
        ));

    div()
        .flex()
        .flex_col()
        .flex_none()
        .w_full()
        .h(px(console::BUTTONS_H))
        .gap(px(2.0))
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
    folder: Option<(usize, &MixerCallbacks)>,
    i18n: I18n,
) -> impl IntoElement {
    let type_label = mixer_track_type_label(track.track_type, i18n);
    let folder_chip = folder.map(|(members, callbacks)| {
        let toggle = callbacks.on_toggle_folder.clone();
        let track_id = track.id.clone();
        let collapsed = track.group_collapsed;
        div()
            .id(gpui::SharedString::from(format!(
                "mixer-folder-toggle-{}",
                track.id
            )))
            .flex()
            .flex_none()
            .items_center()
            .gap(px(2.0))
            .h(px(12.0))
            .px(px(3.0))
            .rounded(px(crate::theme::radius::MICRO))
            .cursor(gpui::CursorStyle::PointingHand)
            .hover(|s| s.bg(Colors::state_hover()))
            .tooltip(crate::components::controls::fb_tooltip(if collapsed {
                "Show the tracks in this folder"
            } else {
                "Hide the tracks in this folder"
            }))
            .child(
                svg()
                    .path(if collapsed {
                        assets::ICON_FOLDER_PATH
                    } else {
                        assets::ICON_FOLDER_OPEN_PATH
                    })
                    .w(px(9.0))
                    .h(px(9.0))
                    .text_color(track.color),
            )
            .child(
                div()
                    .text_size(px(type_scale::CAPTION))
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(Colors::text_muted())
                    .child(format!("{members}")),
            )
            .child(
                svg()
                    .path(if collapsed {
                        assets::ICON_CHEVRON_RIGHT_PATH
                    } else {
                        assets::ICON_CHEVRON_DOWN_PATH
                    })
                    .w(px(9.0))
                    .h(px(9.0))
                    .text_color(Colors::text_muted()),
            )
            .on_mouse_down(gpui::MouseButton::Left, move |_e, w, cx| {
                cx.stop_propagation();
                toggle(&track_id, w, cx);
            })
            .occlude()
    });

    div()
        .flex()
        .flex_row()
        .items_center()
        .flex_none()
        .gap(px(3.0))
        .h(px(console::TOP_ROW_H))
        .pl(px(1.0))
        .child(console::section_caption(type_label))
        .children(folder_chip)
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
                    .h(px(12.0))
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
    slot.output_strip_indices()
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

/// Spacing between two slots in a rack. Each slot owns the spacing below it
/// (as bottom padding) instead of the stack having a flex gap, so there is no
/// point between two slots that is not a drop target.
const SLOT_GAP: f32 = 2.0;

/// One insert slot in a rack.
///
/// `dnd` is the chip's drag payload and drop target; `None` makes it a
/// click-only instrument chip (a MIDI track's VSTi), which neither drags nor
/// takes drops — which slot is the instrument is decided by position.
///
/// Every action is a click, never a press: GPUI drops a click once a drag
/// starts, so picking a slot up never opens its editor, toggles its bypass or
/// removes it. The chip and its glyphs block clicks from reaching the strip
/// behind them but not the wheel, so the rack and the strip row still scroll
/// under the pointer — during a drag as well.
fn insert_chip(
    track_id: &str,
    insert_index: usize,
    slot: &InsertSlotState,
    dnd: Option<(FxSlotDrag, InsertDropTarget)>,
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

    // Dropping a `.pst` preset from the browser onto a chip loads it into
    // this slot (`insert_index` is the full insert-chain index).
    let drop_plugin_preset = callbacks.on_drop_plugin_preset.clone();
    let preset_track = track_id_owned.clone();
    let preset_slot = insert_index;

    let open_target = (track_id_owned, insert_index, slot_id);

    // The glyphs forward slot drops to the chip, so a drop released over one
    // still lands where the chip says it would. The chip alone tracks the
    // refusal cursor: its bounds contain the glyphs.
    let on_drop_insert = callbacks.on_drop_insert.clone();
    let forward_drops = |element: gpui::Stateful<gpui::Div>| match &dnd {
        Some((_, target)) => insert_drop_forwarder(element, target.clone(), on_drop_insert.clone()),
        None => element,
    };

    // The chip's look. The interactive element around it is transparent and
    // also covers the spacing below, where the drop line is drawn.
    let body = div()
        .flex()
        .flex_none()
        .flex_row()
        .items_center()
        .h(px(console::SLOT_H))
        .rounded(px(crate::theme::radius::MICRO))
        .bg(bg)
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .hover(move |style| style.bg(hover_bg))
        .text_size(px(type_scale::LABEL))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(text)
        .cursor(gpui::CursorStyle::PointingHand)
        // In-circuit lamp, and the bypass switch. A slot is either in the
        // signal path or it is not, and that is the one thing about it
        // readable without stopping to read.
        .child(forward_drops(
            div()
                .id(gpui::SharedString::from(format!(
                    "insert-bypass-{}",
                    bypass_pair.1
                )))
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .w(px(15.0))
                .h_full()
                .tooltip(crate::components::controls::fb_tooltip(if bypassed {
                    "Bypassed — click to put it back in the signal path"
                } else {
                    "In the signal path — click to bypass"
                }))
                .child(
                    div()
                        .size(px(6.0))
                        .rounded(px(crate::theme::radius::PILL))
                        .bg(if bypassed {
                            Colors::text_disabled()
                        } else {
                            Colors::state_monitor()
                        }),
                )
                .on_click(move |_e, w, cx| {
                    on_bypass(&bypass_pair, w, cx);
                })
                .block_mouse_except_scroll(),
        ))
        .child(div().flex_1().min_w(px(0.0)).truncate().child(display))
        // Remove ×.
        .child(forward_drops(
            div()
                .id(gpui::SharedString::from(format!(
                    "insert-remove-{}",
                    remove_pair.1
                )))
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .w(px(13.0))
                .h_full()
                .text_size(px(type_scale::LABEL))
                .text_color(Colors::text_faint())
                .cursor(gpui::CursorStyle::PointingHand)
                .hover(|s| s.text_color(Colors::text_primary()))
                .child("×")
                .on_click(move |_e, w, cx| {
                    on_remove(&remove_pair, w, cx);
                })
                .block_mouse_except_scroll(),
        ));

    // The slot itself is the handle. A separate grip column cost six of the
    // eighty-eight pixels on every row to say what the pointer already says by
    // picking the row up — and it made the plug-in's name, the thing the user
    // is actually aiming at, the one part of the row that did not drag.
    let chip = div()
        .id(gpui::SharedString::from(format!(
            "insert-chip-{}",
            id_owned
        )))
        .flex_none()
        .pb(px(SLOT_GAP))
        .cursor(gpui::CursorStyle::PointingHand)
        .drag_over::<BrowserDragItem>(|style, _drag, _window, _cx| drop_over_highlight(style))
        .on_drop::<BrowserDragItem>(move |item, window, cx| {
            if is_plugin_preset_path(&item.path) {
                drop_plugin_preset(
                    &(item.path.clone(), preset_track.clone(), preset_slot),
                    window,
                    cx,
                );
            }
        })
        .on_click(move |_e, w, cx| {
            eprintln!(
                "[mixer] insert row clicked track_id={} insert_index={} plugin={} plugin_instance_id={}",
                open_target.0, open_target.1, display_for_log, open_target.2
            );
            on_open(&open_target, w, cx);
        })
        .block_mouse_except_scroll()
        .child(body);
    match dnd {
        Some((payload, target)) => insert_drop_target_also(
            chip.on_drag(payload, |drag, _offset, _window, cx| {
                drag.refusal.reset();
                cx.new(|_| drag.clone())
            }),
            target,
            DropIndicator::Row { gap: SLOT_GAP },
            callbacks.on_drop_insert.clone(),
            accepts_plugin_preset,
        ),
        None => chip.can_drop(|dragged, _window, _cx| accepts_plugin_preset(dragged)),
    }
}

/// `can_drop` answer for a browser `.pst` preset.
fn accepts_plugin_preset(dragged: &dyn std::any::Any) -> bool {
    dragged
        .downcast_ref::<BrowserDragItem>()
        .is_some_and(|item| is_plugin_preset_path(&item.path))
}

/// Trailing drop zone for the end of a chain, rendered for an empty chain too
/// so it can take a plug-in dragged from another channel. The chips cover the
/// places in front of each slot; this covers the one after the last. Its
/// height plus the last chip's spacing is the gap the rack always had above
/// "+ Insert".
fn insert_drop_end(target: InsertDropTarget, callbacks: &MixerCallbacks) -> impl IntoElement {
    let id = gpui::SharedString::from(format!("mixer-fx-drop-end-{}", target.track_id));
    insert_drop_target(
        div().id(id).flex_none().h(px(7.0)).mx(px(2.0)),
        target,
        DropIndicator::Row { gap: 0.0 },
        callbacks.on_drop_insert.clone(),
    )
}

/// Trailing empty insert slot. Clicking it opens the plugin picker for the
/// next available slot (`next_slot`) on this track. `next_slot` is used for
/// debug logging only — the picker appends to the track's insert chain. It is
/// also the end of the chain for a dragged slot (`end`), and takes a browser
/// `.pst` preset.
fn add_insert_button(
    track_id: &str,
    next_slot: usize,
    end: InsertDropTarget,
    callbacks: &MixerCallbacks,
) -> impl IntoElement {
    let track_id_owned = track_id.to_string();
    let on_add = callbacks.on_add_insert.clone();
    let drop_plugin_preset = callbacks.on_drop_plugin_preset.clone();
    let drop_track = track_id_owned.clone();
    let button = div()
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
        .block_mouse_except_scroll();
    insert_drop_target_also(
        button,
        end.with_key_suffix("add"),
        DropIndicator::Outline,
        callbacks.on_drop_insert.clone(),
        accepts_plugin_preset,
    )
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
    // An Instrument track shows its instrument elsewhere; a MIDI track's
    // instrument sits in the rack as a click-only chip. Either way nothing
    // drags below the chain's floor, and nothing lands there.
    let first_shown = usize::from(track.track_type == TrackType::Instrument);
    let floor = track.fx_chain_floor();
    let used = track.inserts.len();
    let at_max = used >= MAX_INSERT_SLOTS;

    let mut chips = div().flex().flex_col().flex_none();
    for (insert_index, slot) in track.inserts.iter().enumerate().skip(first_shown) {
        let dnd = (insert_index >= floor).then(|| {
            (
                FxSlotDrag::for_slot(&track.id, slot, insert_index, &track.automation_lanes),
                InsertDropTarget::for_track(
                    track,
                    DropSlot::Row {
                        id: slot.id.clone(),
                        index: insert_index,
                    },
                    "mixer",
                ),
            )
        });
        chips = chips.child(insert_chip(
            &track.id,
            insert_index,
            slot,
            dnd,
            callbacks,
            base,
        ));
    }
    let end = InsertDropTarget::for_track(
        track,
        DropSlot::End {
            last_id: track.inserts.last().map(|slot| slot.id.clone()),
        },
        "mixer",
    );
    let chips = chain_end(
        chips,
        &track.id,
        used,
        used > first_shown,
        at_max,
        end,
        callbacks,
    );

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

    let bypass = channel_bypass(
        &track.id,
        track
            .effect_inserts()
            .iter()
            .filter(|slot| !slot.is_empty())
            .map(|slot| slot.bypassed),
        callbacks,
        i18n,
    );

    rack(
        gpui::SharedString::from(format!("insert-slot-scroll-{}", track.id)),
        i18n.tr("mixer.section.inserts"),
        bypass,
        header_plus,
        height_px,
        chips,
    )
}

/// The end of a rack's chain: the drop strip after the last slot, and
/// "+ Insert" (one trailing empty slot until `MAX_INSERT_SLOTS` is reached).
/// With no slots, "+ Insert" keeps the top of the rack and the strip goes
/// under it, so an empty rack looks as it always did.
fn chain_end(
    mut chips: gpui::Div,
    track_id: &str,
    used: usize,
    has_slots: bool,
    at_max: bool,
    end: InsertDropTarget,
    callbacks: &MixerCallbacks,
) -> gpui::Div {
    let add = (!at_max).then(|| add_insert_button(track_id, used, end.clone(), callbacks));
    let strip = insert_drop_end(end, callbacks);
    if has_slots {
        chips = chips.child(strip).children(add);
    } else {
        chips = chips.children(add).child(strip);
    }
    chips
}

/// A rack: caption, then the slots, scrolling inside the rack's height.
fn rack(
    scroll_id: gpui::SharedString,
    label: String,
    bypass: Option<console::RackBypass>,
    plus: Option<RackPlus>,
    height_px: f32,
    body: gpui::Div,
) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .flex_none()
        .h(px(height_px))
        .overflow_hidden()
        .child(console::rack_label(label, bypass, plus))
        .child(
            div()
                .id(scroll_id)
                .flex_1()
                .min_h_0()
                .pt(px(1.0))
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

    let mut chips = div().flex().flex_col().flex_none();
    for (insert_index, slot) in master.inserts.iter().enumerate() {
        // The master keeps no automation lanes of its own.
        let dnd = (
            FxSlotDrag::for_slot(MASTER_TRACK_ID, slot, insert_index, &[]),
            InsertDropTarget::for_master(
                master,
                DropSlot::Row {
                    id: slot.id.clone(),
                    index: insert_index,
                },
                "mixer",
            ),
        );
        chips = chips.child(insert_chip(
            MASTER_TRACK_ID,
            insert_index,
            slot,
            Some(dnd),
            callbacks,
            base,
        ));
    }
    let end = InsertDropTarget::for_master(
        master,
        DropSlot::End {
            last_id: master.inserts.last().map(|slot| slot.id.clone()),
        },
        "mixer",
    );
    let chips = chain_end(
        chips,
        MASTER_TRACK_ID,
        used,
        used > 0,
        at_max,
        end,
        callbacks,
    );

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

    let bypass = channel_bypass(
        MASTER_TRACK_ID,
        master
            .inserts
            .iter()
            .filter(|slot| !slot.is_empty())
            .map(|slot| slot.bypassed),
        callbacks,
        i18n,
    );

    rack(
        gpui::SharedString::from("insert-slot-scroll-master"),
        i18n.tr("mixer.section.inserts"),
        bypass,
        header_plus,
        height_px,
        chips,
    )
}

/// The send level's travel: −60 dB at the left end, +6 dB at the right.
const SEND_FLOOR_DB: f32 = -60.0;
const SEND_CEIL_DB: f32 = 6.0;

/// Pointer travel per dB of send level, at the least. The bar is a strip's
/// width — some 70 px for 66 dB — and moving the level by the bar's own
/// width made every pixel about a dB, so a hand's ordinary 2–4 px per mouse
/// event stepped the level in whole dB. Shift is ten times finer still.
const SEND_DRAG_PX_PER_DB: f32 = 4.0;

/// A send level as a fraction of the bar.
fn send_norm(gain_db: f32) -> f32 {
    ((gain_db.clamp(SEND_FLOOR_DB, SEND_CEIL_DB) - SEND_FLOOR_DB) / (SEND_CEIL_DB - SEND_FLOOR_DB))
        .clamp(0.0, 1.0)
}

/// The travel, in pixels, that sweeps a send bar `width` wide end to end.
fn send_drag_span(width: f32) -> f32 {
    width.max((SEND_CEIL_DB - SEND_FLOOR_DB) * SEND_DRAG_PX_PER_DB)
}

/// The level a send-bar drag reaches at window x `pointer`, the drag having
/// started from level fraction `start`.
fn send_drag_db(
    drag: &crate::components::fader::FaderDrag,
    start: f32,
    pointer: f32,
    fine: bool,
    width: f32,
) -> f32 {
    let next = crate::components::fader::drag_to(
        drag,
        start,
        pointer,
        fine,
        send_drag_span(width),
        |dx| dx,
    );
    SEND_FLOOR_DB + next * (SEND_CEIL_DB - SEND_FLOOR_DB)
}

/// A send level as the bar prints it: `−∞` at the floor, a signed dB
/// otherwise.
fn send_level_text(gain_db: f32) -> String {
    if gain_db <= SEND_FLOOR_DB + 0.05 {
        "\u{2212}\u{221e}".to_string()
    } else if gain_db.abs() < 0.05 {
        "0.0".to_string()
    } else if gain_db < 0.0 {
        format!("\u{2212}{:.1}", -gain_db)
    } else {
        format!("+{gain_db:.1}")
    }
}

/// One send: where it goes, and how much of this channel goes there.
///
/// The row is a bar that fills with the level, so a rack of sends reads as a
/// small bar chart of how much of this channel is going where, with the exact
/// value printed at its end. Dragging the bar sideways sets the level — Shift
/// for fine steps — and a double-click puts it back to unity. The target's
/// name is the handle that moves the send up or down the list.
fn send_chip(
    track_id: &str,
    send_index: usize,
    send: &SendSlotState,
    target_name: &str,
    callbacks: &MixerCallbacks,
    base: gpui::Rgba,
) -> impl IntoElement {
    use crate::components::fader::{is_reset_click, FaderDrag};

    let remove_pair = (track_id.to_string(), send.id.clone());
    let on_remove = callbacks.on_remove_send.clone();
    let text = if send.enabled {
        Colors::text_primary()
    } else {
        Colors::text_muted()
    };
    let fill = if send.enabled {
        Colors::with_alpha(Colors::accent_primary(), 0.35)
    } else {
        Colors::with_alpha(Colors::text_disabled(), 0.35)
    };
    let rest = console::well(base);
    let hover = Colors::composite(rest, Colors::state_hover());
    let chip_drag_payload = SendSlotDrag {
        track_id: track_id.to_string(),
        send_id: send.id.clone(),
        target_name: target_name.to_string(),
        source_index: send_index,
        refusal: DragRefusal::default(),
    };
    let norm = send_norm(send.gain_db);
    let level_text = send_level_text(send.gain_db);
    let tooltip = format!("{target_name} · {level_text} dB");
    let gain_drag_id = format!("mixer-send-gain-{}-{}", track_id, send.id);
    let gain_pair = (track_id.to_string(), send.id.clone());
    let gain_change = callbacks.on_send_gain_change.clone();
    let gain_reset_pair = gain_pair.clone();
    let gain_reset = callbacks.on_send_gain_change.clone();

    // The level bar. Its own drag sets the level; it covers the row, under
    // the name and the remove button, which take their own presses.
    let bar = div()
        .id(gpui::SharedString::from(gain_drag_id.clone()))
        .absolute()
        .inset_0()
        .cursor(gpui::CursorStyle::ResizeLeftRight)
        .child(
            div()
                .absolute()
                .left_0()
                .top_0()
                .bottom_0()
                .w(gpui::relative(norm))
                .bg(fill),
        )
        .on_drag(
            FaderDrag::new(gain_drag_id.clone()),
            |drag, _offset, _window, cx| cx.new(|_| FaderDrag::new(drag.id.clone())),
        )
        .on_drag_move::<FaderDrag>(move |event: &DragMoveEvent<FaderDrag>, window, cx| {
            let drag = event.drag(cx);
            if drag.id != gain_drag_id {
                return;
            }
            let x: f32 = event.event.position.x.into();
            let width = f32::from(event.bounds.size.width);
            let gain_db = send_drag_db(drag, norm, x, event.event.modifiers.shift, width);
            gain_change(
                &(gain_pair.0.clone(), gain_pair.1.clone(), gain_db),
                window,
                cx,
            );
        })
        .on_click(move |event, window, cx| {
            if is_reset_click(event) {
                gain_reset(
                    &(gain_reset_pair.0.clone(), gain_reset_pair.1.clone(), 0.0),
                    window,
                    cx,
                );
            }
        });

    // The row's look; the interactive element around it is transparent and
    // owns the spacing below, as an insert chip's does.
    let body = div()
        .relative()
        .flex()
        .flex_none()
        .flex_row()
        .items_center()
        .h(px(console::SEND_SLOT_H))
        .rounded(px(crate::theme::radius::MICRO))
        .overflow_hidden()
        .bg(rest)
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .hover(move |style| style.bg(hover))
        .child(bar)
        // The target's name: the handle the send is dragged up or down by.
        .child(
            div()
                .id(gpui::SharedString::from(format!("send-chip-{}", send.id)))
                .relative()
                .flex_none()
                .max_w(gpui::relative(0.62))
                .pl(px(5.0))
                .pr(px(2.0))
                .truncate()
                .text_size(px(type_scale::LABEL))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(text)
                .cursor(gpui::CursorStyle::PointingHand)
                .child(target_name.to_string())
                .on_drag(chip_drag_payload, |drag, _offset, _window, cx| {
                    drag.refusal.reset();
                    cx.new(|_| drag.clone())
                }),
        )
        .child(div().flex_1())
        .child(
            div()
                .relative()
                .flex_none()
                .text_size(px(type_scale::CAPTION + 0.5))
                .text_color(Colors::text_secondary())
                .child(level_text),
        )
        .child(
            div()
                .id(gpui::SharedString::from(format!("send-remove-{}", send.id)))
                .relative()
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .w(px(13.0))
                .h_full()
                .text_size(px(type_scale::LABEL))
                .text_color(Colors::text_faint())
                .cursor(gpui::CursorStyle::PointingHand)
                .hover(|s| s.text_color(Colors::text_primary()))
                .child("×")
                .on_mouse_down(gpui::MouseButton::Left, move |_e, w, cx| {
                    on_remove(&remove_pair, w, cx);
                })
                .occlude(),
        );
    let chip = div()
        .id(gpui::SharedString::from(format!("send-row-{}", send.id)))
        .flex_none()
        .pb(px(SLOT_GAP))
        .tooltip(strip_tooltip(tooltip))
        .child(body);
    send_drop_target(
        chip,
        track_id,
        DropSlot::Row {
            id: send.id.clone(),
            index: send_index,
        },
        DropIndicator::Row { gap: SLOT_GAP },
        callbacks,
    )
}

/// Make `element` a drop target on `track_id`'s send list. A send belongs to
/// its own channel's routing, so the list takes only its own sends, with the
/// same take-its-place rule and no-op refusals as an insert chain.
fn send_drop_target(
    element: gpui::Stateful<gpui::Div>,
    track_id: &str,
    slot: DropSlot,
    indicator: DropIndicator,
    callbacks: &MixerCallbacks,
) -> gpui::Stateful<gpui::Div> {
    let place = match &slot {
        DropSlot::Row { id, .. } => id.clone(),
        DropSlot::End { .. } => "<end>".to_string(),
    };
    let key = gpui::SharedString::from(format!("mixer-send/{track_id}/{place}"));
    let list_track = track_id.to_string();
    let drop_track = track_id.to_string();
    let reorder = callbacks.on_reorder_send.clone();
    slot_drop_target::<SendSlotDrag>(
        element,
        key,
        indicator,
        // Sends have no copy gesture.
        move |drag, _copy| {
            if drag.track_id != list_track {
                return None;
            }
            same_list_anchor(&drag.send_id, drag.source_index, &slot)
        },
        move |drag, anchor, window, cx| {
            reorder(
                &(drop_track.clone(), drag.send_id.clone(), anchor),
                window,
                cx,
            );
        },
        |_| false,
    )
}

/// Trailing drop zone for the end of a send list. Its height plus the last
/// send's spacing is the gap the rack always had above "+ Send".
fn send_drop_end(
    track_id: &str,
    last_id: Option<String>,
    callbacks: &MixerCallbacks,
) -> impl IntoElement {
    send_drop_target(
        div()
            .id(gpui::SharedString::from(format!(
                "mixer-send-drop-end-{track_id}"
            )))
            .flex_none()
            .h(px(7.0))
            .mx(px(2.0)),
        track_id,
        DropSlot::End { last_id },
        DropIndicator::Row { gap: 0.0 },
        callbacks,
    )
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
    let mut chips = div().flex().flex_col().flex_none();
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
        let last_id = track.sends.last().map(|send| send.id.clone());
        chips = chips.child(send_drop_end(&track.id, last_id, callbacks));
    }
    chips = chips.child(add_send_button(&track.id, callbacks));

    rack(
        gpui::SharedString::from(format!("send-slot-scroll-{}", track.id)),
        i18n.tr("mixer.section.sends"),
        None,
        None,
        height_px,
        chips,
    )
}

/// Pan, with its value printed under the knob.
///
/// The old row spent its second line on a static "L … R" legend, which says
/// nothing a bipolar knob has not already said with its centre tick. The
/// position does need saying — "is that dead centre or two degrees off?" is not
/// answerable from a 26 px disc — so the number takes the line instead.
fn pan_section(
    track: &TrackState,
    callbacks: &MixerCallbacks,
    base: gpui::Rgba,
) -> gpui::AnyElement {
    // A spatial mix places the channel in the room instead of panning it.
    if callbacks.spatial_format.is_spatial() {
        let track_id = track.id.clone();
        let on_spatial = callbacks.on_spatial_change.clone();
        return console::room_panner(
            gpui::SharedString::from(format!("mix-room-{}", track.id)),
            track.spatial,
            callbacks.spatial_format,
            base,
            move |params, w, cx| on_spatial(&(track_id.clone(), params), w, cx),
        )
        .into_any_element();
    }
    let track_id = track.id.clone();
    let pan_cb = callbacks.on_pan_change.clone();
    let centred = track.pan.abs() < 0.005;
    div()
        .flex()
        .flex_col()
        .flex_none()
        .items_center()
        .justify_start()
        .gap(px(2.0))
        .h(px(console::PAN_H))
        .child(knob_bipolar(
            format!("mix-pan-{}", track.id),
            track.pan,
            -1.0,
            1.0,
            console::PAN_KNOB_SIZE,
            // The track's colour: the arc ties the knob to the bar across the
            // top of its strip.
            track.color,
            None,
            0.0,
            move |new_pan: &f32, w: &mut gpui::Window, cx: &mut gpui::App| {
                pan_cb(&(track_id.clone(), *new_pan), w, cx)
            },
        ))
        .child(
            div()
                .flex_none()
                .h(px(11.0))
                .text_size(px(type_scale::CAPTION + 0.5))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(if centred {
                    Colors::text_muted()
                } else {
                    Colors::text_primary()
                })
                .child(console::format_pan(track.pan)),
        )
        .into_any_element()
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
    meter: gpui::AnyElement,
    peak: gpui::AnyElement,
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
        meter,
        peak,
        db_str,
        is_selected || automation_reading,
        has_volume_automation.then_some(automation_reading),
    )
}

/// A channel strip's meter: drawn in place, or — in the docked mixer, where
/// meters move without rebuilding the strips — an empty slot the meter layer
/// paints (see [`crate::components::mixer_meter_layer`]).
fn strip_meter(
    track: &TrackState,
    track_index: usize,
    meter_layer: Option<&SharedMeterLayout>,
    vsti: Option<VstiMeterFallback>,
) -> gpui::AnyElement {
    match meter_layer {
        Some(layout) => meter_slot(
            layout,
            MeterSource::Track {
                id: track.id.clone(),
                index: track_index,
                vsti,
            },
        )
        .into_any_element(),
        None => meter_surface(
            track.meter_level_l,
            track.meter_level_r,
            track.meter_peak_hold_l,
            track.meter_peak_hold_r,
            track.meter_clip,
        )
        .into_any_element(),
    }
}

/// The peak readout's content: a slot the meter layer prints into, or — in
/// the detached window — the number itself.
fn peak_readout(
    meter_layer: Option<&SharedMeterLayout>,
    source: MeterSource,
    hold_l: f32,
    hold_r: f32,
    clip: bool,
) -> gpui::AnyElement {
    match meter_layer {
        Some(layout) => peak_slot(layout, source).into_any_element(),
        None => div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(crate::theme::radius::MICRO))
            .when(clip, |s| s.bg(Colors::status_error()))
            .text_size(px(PEAK_TEXT_SIZE))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(if clip {
                Colors::on_color(Colors::status_error())
            } else {
                Colors::text_secondary()
            })
            .child(peak_readout_text(hold_l, hold_r, clip))
            .into_any_element(),
    }
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
    peak: gpui::AnyElement,
    db_text: String,
    highlight: bool,
    automation: Option<bool>,
) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h(px(console::FADER_MIN_H
            + console::READOUT_GAP
            + console::READOUT_H))
        .items_center()
        .w_full()
        .gap(px(console::READOUT_GAP))
        // Fader, meter, scale: the hand on the left, the level beside it, and
        // the numbers the level is read against on the outside.
        .child(
            div()
                .flex()
                .flex_row()
                .items_stretch()
                .justify_center()
                .gap(px(4.0))
                .flex_1()
                .min_h_0()
                .w_full()
                .child(fader)
                .child(console::fader_aligned(meter))
                .child(console::fader_aligned(
                    console::meter_scale().into_any_element(),
                )),
        )
        // Two readouts, like a console's: the fader's gain on the left, the
        // channel's held peak on the right — what it is set to, and what it
        // is doing.
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(px(3.0))
                .w_full()
                .h(px(console::READOUT_H))
                .child(
                    div()
                        .flex()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .items_center()
                        .gap(px(2.0))
                        .child(
                            div()
                                .truncate()
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
                .child(div().flex_none().w(px(32.0)).h_full().child(peak)),
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

    // The handle is the gap between two sections, drawn as a fold across the
    // strip with a grip in its middle: the rule says where one section ends,
    // the grip that the edge can be pulled. Both light up under the pointer,
    // and stay lit in the accent while it is dragged.
    let (rule, grip) = if is_resizing {
        (Colors::accent_primary(), Colors::accent_primary())
    } else {
        (Colors::border_default(), Colors::border_strong())
    };

    let mut handle = div()
        .id((
            match target {
                MixerSplitTarget::InsertSend => "mix-split-insert-send",
                MixerSplitTarget::SendFader => "mix-split-send-fader",
            },
            id_num,
        ))
        .group("mix-split")
        .relative()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .w_full()
        .h(px(SEC_SPLITTER_H))
        .cursor(gpui::CursorStyle::ResizeUpDown)
        .child(
            div()
                .absolute()
                .left_0()
                .right_0()
                .top(px((SEC_SPLITTER_H - 1.0) / 2.0))
                .h(px(1.0))
                .bg(rule)
                .group_hover("mix-split", |s| s.bg(Colors::border_strong())),
        )
        .child(
            div()
                .w(px(18.0))
                .h(px(3.0))
                .rounded(px(crate::theme::radius::PILL))
                .bg(grip)
                .group_hover("mix-split", |s| s.bg(Colors::text_muted())),
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
    }
    handle
}

/// The I/O block: where the channel's signal comes from, over where it goes.
/// The output row is the routing picker; the input row states the input, whose
/// picker is the Inspector's.
fn io_block(
    track: &TrackState,
    all_tracks: &[TrackState],
    input_label: Option<&str>,
    id_num: usize,
    base: gpui::Rgba,
    callbacks: &MixerCallbacks,
) -> impl IntoElement {
    let input = input_label
        .map(str::to_string)
        .unwrap_or_else(|| mixer_input_fallback_label(track));
    div()
        .flex()
        .flex_col()
        .flex_none()
        .w_full()
        .h(px(console::IO_BLOCK_H))
        .gap(px(console::IO_ROW_GAP))
        .child(
            div()
                .id(("mix-input-row", id_num))
                .w_full()
                .tooltip(strip_tooltip(format!("Input: {input}")))
                .child(console::io_label(None, input, base)),
        )
        .child(output_button(
            &track.id,
            format!("\u{2192} {}", output_routing_label(track, all_tracks)),
            id_num,
            base,
            callbacks.on_open_output_picker.clone(),
        ))
}

/// The input row for a channel the input labels have no entry for: what kind
/// of input it takes.
fn mixer_input_fallback_label(track: &TrackState) -> String {
    match track.track_type {
        TrackType::Bus => "Bus".to_string(),
        TrackType::Return => "Return".to_string(),
        TrackType::Group => "Folder".to_string(),
        TrackType::Master => "Master".to_string(),
        TrackType::Video => "Video".to_string(),
        TrackType::Audio => "No Input".to_string(),
        TrackType::Midi | TrackType::Instrument => "MIDI".to_string(),
    }
}

/// Each channel's input as its strip's I/O block names it: an audio track's
/// input connection, resolved from the Audio Connections registry so a rename
/// shows at once; a MIDI or instrument track's MIDI input. Channels without an
/// input of their own (buses, returns, folders) have no entry.
pub(crate) fn mixer_input_labels(
    tracks: &[TrackState],
    connections: &crate::audio_connections::AudioConnectionRegistry,
) -> std::collections::HashMap<String, String> {
    use crate::components::timeline::timeline_state::TrackMidiInputRouting;
    let mut labels = std::collections::HashMap::with_capacity(tracks.len());
    for track in tracks {
        let label = match track.track_type {
            TrackType::Audio => Some(match track.routing.audio_input_connection_id.as_ref() {
                None => "No Input".to_string(),
                Some(id) => match connections.get(id) {
                    Some(connection) if connection.status.is_usable() => connection.name.clone(),
                    Some(connection) => format!("{} (unavailable)", connection.name),
                    None => "Missing input".to_string(),
                },
            }),
            TrackType::Midi | TrackType::Instrument => Some(match &track.routing.midi_input {
                TrackMidiInputRouting::AllInputs => "All MIDI".to_string(),
                TrackMidiInputRouting::None => "No MIDI Input".to_string(),
                TrackMidiInputRouting::MidiDevice { device_id } => device_id.clone(),
                TrackMidiInputRouting::PluginOutput { track_id } => tracks
                    .iter()
                    .find(|other| &other.id == track_id)
                    .map(|other| format!("MIDI from {}", other.name))
                    .unwrap_or_else(|| "Missing MIDI source".to_string()),
            }),
            _ => None,
        };
        if let Some(label) = label {
            labels.insert(track.id.clone(), label);
        }
    }
    labels
}

// ─── Channel strip ──────────────────────────────────────────────────────────

/// The space between two sections.
fn section_gap() -> gpui::AnyElement {
    div()
        .flex_none()
        .h(px(console::SECTION_GAP))
        .into_any_element()
}

/// What goes in each of a strip's sections. Every kind of strip — channel,
/// VSTi output, Master, Control Room — fills the same slots, so a row of them
/// keeps its pans, faders and plates on one line.
struct StripSections {
    top: gpui::AnyElement,
    io: gpui::AnyElement,
    inserts: gpui::AnyElement,
    sends: gpui::AnyElement,
    pan: gpui::AnyElement,
    fader: gpui::AnyElement,
    buttons: gpui::AnyElement,
}

/// The strip between its colour bar and its plate: the sections the view
/// shows, top to bottom, with the gaps between them. With `handles` the two
/// racks are each followed by their splitter handle — dragging any strip's
/// resizes every strip — and without, by a plain gap of the same height.
fn strip_body(
    sections: StripSections,
    view: MixerView,
    handles: Option<(usize, &MixerSplit)>,
) -> gpui::Div {
    let StripSections {
        top,
        io,
        inserts,
        sends,
        pan,
        fader,
        buttons,
    } = sections;
    let mut body = div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .w_full()
        .overflow_hidden()
        .px(px(console::STRIP_GUTTER))
        .pt(px(console::BODY_PAD_T))
        .pb(px(console::BODY_PAD_B))
        .child(top);
    if view.show_io {
        body = body.child(section_gap()).child(io);
    }
    body = body.child(section_gap());
    let after = |target: MixerSplitTarget| match handles {
        Some((id_num, split)) => vertical_split_handle(id_num, target, split).into_any_element(),
        None => section_gap(),
    };
    if view.show_inserts {
        body = body
            .child(inserts)
            .child(after(MixerSplitTarget::InsertSend));
    }
    if view.show_sends {
        body = body.child(sends).child(after(MixerSplitTarget::SendFader));
    }
    body.child(
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(LOWER_CONTROL_MIN_H))
            .overflow_hidden()
            .w_full()
            .child(pan)
            .child(section_gap())
            .child(fader)
            .child(section_gap())
            .child(buttons),
    )
}

/// Height of one folder band across a strip's top edge.
const MIXER_FOLDER_BAND_H: f32 = 2.0;
/// Folder levels a strip shows bands for: the innermost ones. Two fit above
/// the type label without touching it.
const MIXER_FOLDER_BANDS: usize = 2;

/// Where a strip sits among folders. Adjacent strips of one folder carry the
/// same band, so the bands read as one bar across the folder and its members,
/// starting at the folder's own strip.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct MixerFolderPlacement {
    /// Band colours, outermost first: the enclosing folders and, for a folder,
    /// itself; the innermost [`MIXER_FOLDER_BANDS`] of them.
    pub bands: Vec<gpui::Rgba>,
    /// For a folder: how many tracks sit directly inside it.
    pub members: Option<usize>,
}

impl MixerFolderPlacement {
    pub(crate) fn of(tracks: &[TrackState], tree: &GroupTree, index: usize) -> Self {
        let is_folder = tracks[index].track_type == TrackType::Group;
        let mut bands: Vec<gpui::Rgba> = tree
            .ancestors(index)
            .into_iter()
            .rev()
            .map(|folder| tracks[folder].color)
            .collect();
        if is_folder {
            bands.push(tracks[index].color);
        }
        let skip = bands.len().saturating_sub(MIXER_FOLDER_BANDS);
        Self {
            bands: bands.split_off(skip),
            members: is_folder.then(|| tree.member_count(index)),
        }
    }
}

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
    folder: &MixerFolderPlacement,
    // The channel's input, as its I/O block names it (`mixer_input_labels`).
    input_label: Option<&str>,
    // When true the GPU primitive layer owns the strip background, the name
    // plate fill, and the right separator — the strip container omits them so
    // the batched canvas behind shows through. Inner sections keep their own
    // styling.
    gpu_decor: bool,
    meter_layer: Option<&SharedMeterLayout>,
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

    let sections = StripSections {
        top: strip_top_row(
            track,
            vsti_group.as_ref().map(|(group_key, expanded, count)| {
                (group_key.as_str(), *expanded, *count, callbacks)
            }),
            folder.members.map(|members| (members, callbacks)),
            i18n,
        )
        .into_any_element(),
        io: io_block(track, all_tracks, input_label, id_num, base, callbacks).into_any_element(),
        inserts: inserts_section(track, callbacks, insert_h, base, i18n).into_any_element(),
        sends: sends_section(track, all_tracks, callbacks, send_h, base, i18n).into_any_element(),
        pan: pan_section(track, callbacks, base),
        fader: fader_area(
            track,
            callbacks,
            is_selected,
            strip_meter(track, index, meter_layer, None),
            peak_readout(
                meter_layer,
                MeterSource::Track {
                    id: track.id.clone(),
                    index,
                    vsti: None,
                },
                track.meter_peak_hold_l,
                track.meter_peak_hold_r,
                track.meter_clip,
            ),
        )
        .into_any_element(),
        buttons: button_row(track, callbacks, id_num, false, base).into_any_element(),
    };

    strip_shell(base, gpu_decor)
        .relative()
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
        .child(console::color_bar(track.color))
        // The folder bands, under the colour bar: GPUI children, so they
        // paint on both the element and the batched decor paths.
        .children(folder.bands.iter().enumerate().map(|(level, color)| {
            div()
                .absolute()
                .left_0()
                .right_0()
                .top(px(console::COLOR_BAR_H + level as f32 * MIXER_FOLDER_BAND_H))
                .h(px(MIXER_FOLDER_BAND_H))
                .bg(*color)
        }))
        .child(strip_body(sections, split.view, Some((id_num, split))))
        .child(console::name_plate(
            console::plate_fill(is_selected),
            Some(index + 1),
            track.name.clone(),
            gpu_decor,
        ))
}

/// A channel strip's outer column: its width, its surface and the gap at its
/// right edge. In GPU-decoration mode the batched layer paints the surface
/// and the gap, and the column paints neither — it keeps the gap's width as a
/// clear border, so its contents lay out the same on both paths.
fn strip_shell(base: gpui::Rgba, gpu_decor: bool) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .flex_none()
        .w(px(strip_width()))
        .min_h(px(STRIP_MIN_HEIGHT))
        .h_full()
        .overflow_hidden()
        .border_r(px(console::STRIP_GAP_W))
        .border_color(if gpu_decor {
            gpui::transparent_black().into()
        } else {
            console::strip_gap()
        })
        .when(!gpu_decor, |s| {
            let hover = Colors::composite(base, Colors::state_hover());
            s.bg(base).hover(move |h| h.bg(hover))
        })
}

#[allow(clippy::too_many_arguments)]
fn vsti_output_sub_strip(
    parent_track: &TrackState,
    child_track: &TrackState,
    child_index: usize,
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
    meter_layer: Option<&SharedMeterLayout>,
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
    let vsti_fallback = vsti_output_child_channels_for_bus_layout(bus_counts, bus_index).map(
        |(channel_l, channel_r)| VstiMeterFallback {
            parent_track_id: parent_track.id.clone(),
            insert_id: insert_id.to_string(),
            channel_l,
            channel_r,
        },
    );
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

    // Real callbacks: mute / solo / volume / pan all target the child track
    // id (via button_row / pan_section / fader_area below), so S/M and the
    // fader operate per output bus.
    let sections = StripSections {
        top: strip_top_row(&sub_track, None, None, i18n).into_any_element(),
        io: io_block(
            child_track,
            all_tracks,
            Some(&format!("From {}", parent_track.name)),
            id_num,
            base,
            callbacks,
        )
        .into_any_element(),
        // Real per-bus insert rack: the backing child track is a genuine Bus
        // model track, so its FX chain is added/bypassed/reordered by child
        // track id and processed by the engine's pass-2 routing chain for
        // this output bus only (Add Insert opens the Effects picker).
        inserts: inserts_section(child_track, callbacks, insert_h, base, i18n).into_any_element(),
        sends: sends_section(child_track, all_tracks, callbacks, send_h, base, i18n)
            .into_any_element(),
        pan: pan_section(&sub_track, callbacks, base),
        fader: fader_area(
            &sub_track,
            callbacks,
            is_selected,
            // `sub_track` is the child with the plug-in meter fallback
            // already applied, for the inline meter; the meter layer
            // applies the same fallback itself at paint time.
            strip_meter(&sub_track, child_index, meter_layer, vsti_fallback.clone()),
            peak_readout(
                meter_layer,
                MeterSource::Track {
                    id: sub_track.id.clone(),
                    index: child_index,
                    vsti: vsti_fallback,
                },
                sub_track.meter_peak_hold_l,
                sub_track.meter_peak_hold_r,
                sub_track.meter_clip,
            ),
        )
        .into_any_element(),
        // Solo on the parent instrument sounds every one of its output
        // channels (engine: `has_soloed_vsti_output_parent`), so this
        // channel's S shows the inherited state rather than sitting
        // dark while the channel is plainly audible.
        buttons: button_row(&sub_track, callbacks, id_num, parent_track.solo, base)
            .into_any_element(),
    };

    strip_shell(base, gpu_decor)
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
        // The bus belongs to the parent's instrument, so it carries the
        // parent's colour.
        .child(console::color_bar(parent_track.color))
        .child(strip_body(sections, split.view, Some((id_num, split))))
        .child(console::name_plate(
            console::plate_fill(is_selected),
            None,
            bus_label,
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
    meter_layer: Option<&SharedMeterLayout>,
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

    let sections = StripSections {
        top: pinned_top_row(i18n.tr("mixer.master.bus-label")).into_any_element(),
        // What the bus carries, over where it goes: the format in the
        // channels' input row, and the output destination — a logical Output
        // Audio Connection, never a raw hardware pair — in their output row.
        io: div()
            .flex()
            .flex_col()
            .flex_none()
            .w_full()
            .h(px(console::IO_BLOCK_H))
            .gap(px(console::IO_ROW_GAP))
            .child(console::io_label(None, i18n.tr("mixer.stereo"), base))
            .child(control_room_output_button(
                "master-output",
                i18n.tr("mixer.master.output"),
                master.output_label.clone(),
                base,
                callbacks.on_master_output_picker.clone(),
            ))
            .into_any_element(),
        inserts: master_inserts_section(master, callbacks, insert_h, base, i18n).into_any_element(),
        // The master takes no sends, but it keeps their slot: every fixed row
        // below this point then lands on the same baseline as the channels
        // beside it, which is the whole reason the mixer reads as one console
        // and not as a row of strips plus two odd ones on the end.
        sends: pinned_rack_spacer(send_h).into_any_element(),
        pan: pinned_rack_spacer(console::PAN_H).into_any_element(),
        fader: fader_bay(
            fader_with_drag_callbacks(
                "mix-fader-master",
                master.volume,
                Some(on_master_start),
                Some(on_master_preview),
                Some(on_master_commit),
                Some(on_master_reset),
            )
            .into_any_element(),
            pinned_meter(
                meter_layer,
                MeterSource::Master,
                master.meter_level_l,
                master.meter_level_r,
                master.meter_peak_hold_l,
                master.meter_peak_hold_r,
                master.meter_clip,
            ),
            peak_readout(
                meter_layer,
                MeterSource::Master,
                master.meter_peak_hold_l,
                master.meter_peak_hold_r,
                master.meter_clip,
            ),
            db_str,
            true,
            None,
        )
        .into_any_element(),
        // No M/S/R/I on the master; the row is kept so the plate below stays
        // on the channel strips' baseline.
        buttons: pinned_rack_spacer(console::BUTTONS_H).into_any_element(),
    };

    pinned_shell(base)
        .child(console::color_bar(Colors::accent_primary()))
        .child(strip_body(
            sections,
            split.view,
            Some((PINNED_MASTER_ID, split)),
        ))
        .child(console::name_plate(
            console::master_plate_fill(),
            None,
            i18n.tr("mixer.master.label"),
            false,
        ))
}

/// Element-id numbers for the pinned strips' splitter handles; the channel
/// strips number theirs by a hash of the track id.
const PINNED_MASTER_ID: usize = 1;
const PINNED_MONITOR_ID: usize = 2;

/// A pinned strip's outer column, in the channel strips' width and with the
/// same gap at its right edge.
fn pinned_shell(base: gpui::Rgba) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .flex_none()
        .w(px(strip_width()))
        .min_h(px(STRIP_MIN_HEIGHT))
        .h_full()
        .overflow_hidden()
        .bg(base)
        .border_r(px(console::STRIP_GAP_W))
        .border_color(console::strip_gap())
}

/// The pinned strips' top row: what bus this is. Same height as a channel's
/// type row, so the two kinds of strip start on the same line.
fn pinned_top_row(bus_label: String) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .flex_none()
        .h(px(console::TOP_ROW_H))
        .pl(px(1.0))
        .child(console::section_caption(bus_label))
}

/// Holds a section's worth of height on a pinned strip that has no such
/// section, so the rows below it stay level with the channel strips.
fn pinned_rack_spacer(height_px: f32) -> impl IntoElement {
    div().flex_none().w_full().h(px(height_px))
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
    let tooltip_value = format!("{label}: {value}");
    let text_color = if accent_value {
        Colors::accent_primary()
    } else {
        Colors::text_primary()
    };
    let rest = console::well(base);
    let hover = Colors::composite(rest, Colors::state_hover());
    let row = div()
        .id(gpui::ElementId::Name(id.to_string().into()))
        .flex()
        .flex_row()
        .flex_none()
        .items_center()
        .gap(px(3.0))
        .w_full()
        .min_w(px(0.0))
        .h(px(console::IO_ROW_H))
        .px(px(5.0))
        .rounded(px(crate::theme::radius::MICRO))
        .bg(rest)
        .border(px(1.0))
        .border_color(Colors::border_subtle())
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
        );
    match on_click {
        Some(cb) => row
            .cursor(gpui::CursorStyle::PointingHand)
            .hover(move |s| s.bg(hover))
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
            .occlude(),
        None => row,
    }
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
    height: f32,
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
        .h(px(height))
        .rounded(px(crate::theme::radius::MICRO))
        .border(px(1.0))
        .border_color(if active {
            semantic
        } else {
            Colors::border_default()
        })
        .cursor(gpui::CursorStyle::PointingHand)
        .bg(if active { semantic } else { rest })
        .hover(move |s| s.bg(if active { on_hover } else { hover }))
        .text_size(px(type_scale::CAPTION))
        .font_weight(gpui::FontWeight::BOLD)
        .text_color(if active {
            Colors::on_color(semantic)
        } else {
            Colors::text_secondary()
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
    meter_layer: Option<&SharedMeterLayout>,
    i18n: I18n,
) -> impl IntoElement {
    let base = console::pinned_surface();

    // Same section split as `master_strip`, so both pinned strips align with
    // the channels and with each other.
    let (insert_h, send_h) = clamp_mixer_section_heights_for_strip(
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

    let level_text = volume::format_db(monitor.volume);
    let on_level = callbacks.on_monitor_volume_change.clone();
    let sections = StripSections {
        top: pinned_top_row(i18n.tr("mixer.monitor.bus-label")).into_any_element(),
        // What it listens to, over where it plays: the Control Room's routing
        // in the channels' I/O slot.
        io: div()
            .flex()
            .flex_col()
            .flex_none()
            .w_full()
            .h(px(console::IO_BLOCK_H))
            .gap(px(console::IO_ROW_GAP))
            .child(control_room_selector(
                "monitor-source",
                i18n.tr("mixer.monitor.source"),
                source_value,
                monitor.listen_active,
                base,
                callbacks.on_monitor_source_picker.clone(),
            ))
            .child(control_room_output_button(
                "monitor-output",
                i18n.tr("mixer.monitor.output"),
                monitor.output_name.clone(),
                base,
                callbacks.on_monitor_output_picker.clone(),
            ))
            .into_any_element(),
        inserts: pinned_rack_spacer(insert_h).into_any_element(),
        sends: pinned_rack_spacer(send_h).into_any_element(),
        // The monitor level, as a knob in the pan slot: the Control Room has
        // no pan, and a level knob leaves the bay to the meter.
        pan: div()
            .flex()
            .flex_col()
            .flex_none()
            .items_center()
            .gap(px(2.0))
            .h(px(console::PAN_H))
            .child(crate::components::knob::knob_with_default(
                "mix-monitor-level",
                monitor.volume,
                0.0,
                1.0,
                console::PAN_KNOB_SIZE,
                Colors::text_secondary(),
                volume::db_to_norm(0.0),
                move |value: &f32, w: &mut gpui::Window, cx: &mut gpui::App| on_level(value, w, cx),
            ))
            .child(
                div()
                    .flex_none()
                    .h(px(11.0))
                    .text_size(px(type_scale::CAPTION + 0.5))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(Colors::text_primary())
                    .child(format!(
                        "{} {}",
                        i18n.tr_or("mixer.monitor.level", "Level"),
                        level_text
                    )),
            )
            .into_any_element(),
        fader: fader_bay(
            div().flex_none().into_any_element(),
            pinned_meter(
                meter_layer,
                MeterSource::Monitor,
                monitor.meter_level_l,
                monitor.meter_level_r,
                monitor.meter_peak_hold_l,
                monitor.meter_peak_hold_r,
                monitor.meter_clip,
            ),
            peak_readout(
                meter_layer,
                MeterSource::Monitor,
                monitor.meter_peak_hold_l,
                monitor.meter_peak_hold_r,
                monitor.meter_clip,
            ),
            level_text,
            monitor.dim || monitor.mute,
            None,
        )
        .into_any_element(),
        // Mute / Dim / Mono take the channels' toggle row: same slot, same
        // shape, and the three controls a monitor path actually has instead
        // of the four a channel has.
        // Mute and Dim over Mono, in the rows a channel spends on M S R I and
        // the Listen taps: three words do not fit one Narrow row.
        buttons: div()
            .flex()
            .flex_col()
            .flex_none()
            .w_full()
            .h(px(console::BUTTONS_H))
            .gap(px(2.0))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .w_full()
                    .gap(px(2.0))
                    .child(control_room_toggle(
                        "monitor-mute",
                        &i18n.tr("mixer.monitor.mute"),
                        monitor.mute,
                        Colors::state_arm(),
                        base,
                        console::TOGGLE_H,
                        callbacks.on_monitor_toggle_mute.clone(),
                    ))
                    .child(control_room_toggle(
                        "monitor-dim",
                        &i18n.tr("mixer.monitor.dim"),
                        monitor.dim,
                        Colors::state_solo(),
                        base,
                        console::TOGGLE_H,
                        callbacks.on_monitor_toggle_dim.clone(),
                    )),
            )
            .child(div().flex().flex_row().w_full().child(control_room_toggle(
                "monitor-mono",
                &i18n.tr("mixer.monitor.mono"),
                monitor.mono,
                Colors::state_mute(),
                base,
                console::LISTEN_TOGGLE_H,
                callbacks.on_monitor_toggle_mono.clone(),
            )))
            .into_any_element(),
    };

    pinned_shell(base)
        .child(console::color_bar(Colors::text_faint()))
        .child(strip_body(
            sections,
            split.view,
            Some((PINNED_MONITOR_ID, split)),
        ))
        .child(console::name_plate(
            console::monitor_plate_fill(),
            None,
            i18n.tr("mixer.monitor.label"),
            false,
        ))
}

// ─── Public: Mixer Panel ─────────────────────────────────────────────────────

/// Strip columns above/below the visible viewport that are kept rendered to
/// prevent pop-in during horizontal mixer scrolling.
const MIXER_OVERSCAN: usize = 1;

/// Height of the row under the channel strips that holds their scrollbar.
pub(crate) const MIXER_SCROLLBAR_ROW: f32 = crate::components::scrollbar::THICKNESS;

/// Width of the pinned Master and Control Room pair and the rule before it.
pub(crate) fn mixer_pinned_width() -> f32 {
    2.0 * strip_width() + 1.0
}

/// Whether the channel strips overflow the scroller and so get a scrollbar
/// row. The row is reserved out of the strip height — not drawn over the
/// strips, where it used to sit on the coloured name plates and could hardly
/// be seen — so every strip, the pinned master pair and the GPU decoration all
/// lay out to the same, shorter height.
pub(crate) fn mixer_reserves_scrollbar(strip_count: usize, viewport_width: f32) -> bool {
    strip_count as f32 * strip_width() > viewport_width.max(0.0) + 0.5
}

/// Strip height and vertical-scroll flag once the scrollbar row is taken out of
/// the body.
pub(crate) fn mixer_strip_height_with_scrollbar(
    strip_available_px: f32,
    body_scrolls: bool,
    reserve: bool,
) -> (f32, bool) {
    if !reserve {
        return (strip_available_px, body_scrolls);
    }
    let h = strip_available_px - MIXER_SCROLLBAR_ROW;
    if h < STRIP_MIN_HEIGHT {
        (STRIP_MIN_HEIGHT, true)
    } else {
        (h, body_scrolls)
    }
}

/// The scrollbar row under the channel strips: the bar under the scroller and
/// an empty cell under the pinned Master/Control Room pair, so the bar ends
/// where the channels do. `None` when every strip fits.
pub(crate) fn mixer_scrollbar_row(
    strip_count: usize,
    scroll_x: f32,
    viewport_width: f32,
    on_scroll: std::sync::Arc<dyn Fn(f32, &mut gpui::Window, &mut gpui::App) + 'static>,
) -> Option<gpui::AnyElement> {
    let total_content_w = strip_count as f32 * strip_width();
    let max_scroll_x = (total_content_w - viewport_width).max(0.0);
    let bar = crate::components::scrollbar::scrollbar(
        "mixer-hscroll",
        crate::components::scrollbar::Axis::Horizontal,
        scroll_x.clamp(0.0, max_scroll_x),
        total_content_w,
        viewport_width,
        max_scroll_x,
        viewport_width,
        on_scroll,
    )?;
    // The pinned pair and the rule before it (see `mixer_panel_viewport_metrics`).
    let pinned_w = mixer_pinned_width();
    Some(
        div()
            .flex()
            .flex_row()
            .flex_none()
            .h(px(MIXER_SCROLLBAR_ROW))
            .child(div().flex_1().min_w(px(0.0)).h_full().child(bar))
            .child(div().w(px(pinned_w)).flex_none().h_full())
            .into_any_element(),
    )
}

pub(crate) fn mixer_visible_item_range(
    strip_count: usize,
    scroll_x: f32,
    viewport_width: f32,
) -> std::ops::Range<usize> {
    let max_scroll_x = (strip_count as f32 * strip_width() - viewport_width.max(0.0)).max(0.0);
    let scroll_x = scroll_x.clamp(0.0, max_scroll_x);
    let first_visible = (scroll_x / strip_width()).floor() as usize;
    let visible_start = first_visible.saturating_sub(MIXER_OVERSCAN);
    let last_visible = ((scroll_x + viewport_width.max(0.0)) / strip_width()).ceil() as usize;
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
    let total_content_w = strip_count as f32 * strip_width();
    let max_scroll_x = (total_content_w - viewport_width).max(0.0);
    let strip_x = strip_index as f32 * strip_width();
    let target = strip_x - (viewport_width - strip_width()) * 0.5;
    target.clamp(0.0, max_scroll_x.max(0.0))
}

/// The scroll that brings a strip into view, or `None` when it is already
/// wholly on screen. Revealing the selection should not move a mixer the
/// user is looking at; an off-screen strip is centred like a tree focus.
pub fn mixer_scroll_x_to_reveal(
    strip_index: usize,
    scroll_x: f32,
    viewport_width: f32,
    strip_count: usize,
) -> Option<f32> {
    let left = strip_index as f32 * strip_width();
    let right = left + strip_width();
    if left >= scroll_x - 0.5 && right <= scroll_x + viewport_width + 0.5 {
        return None;
    }
    Some(mixer_scroll_x_for_strip_index(
        strip_index,
        viewport_width,
        strip_count,
    ))
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

    // A collapsed folder folds its members' strips away, as it folds their
    // rows in the arrangement.
    let folders = GroupTree::of(tracks);
    let mut items = Vec::with_capacity(tracks.len());
    for (track_index, track) in tracks.iter().enumerate() {
        if folders.hidden[track_index] {
            continue;
        }
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
    let stripe_count = (width / strip_width()).ceil().clamp(0.0, 64.0) as usize;
    let color = Colors::border_subtle();
    for i in 0..stripe_count {
        let x = i as f32 * strip_width();
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
/// `strip_width()`), and reproduces each strip's background / accent / separator so
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
        let x = pos as f32 * strip_width();
        let geom = match *item {
            MixerRenderItem::Track { track_index } => {
                let track = &tracks[track_index];
                let selected =
                    mixer_strip_is_selected(&track.id, selected_track_id, selected_track_ids);
                MixerStripGeom {
                    x,
                    width: strip_width(),
                    height: strip_height,
                    bg: console::strip_surface(selected),
                    plate: console::plate_fill(selected),
                    separator: console::strip_gap(),
                    selected,
                    is_master: false,
                    meter_l: track.meter_level_l,
                    meter_r: track.meter_level_r,
                    hovered: false,
                }
            }
            MixerRenderItem::VstiOutput { child_index, .. } => {
                let child = &tracks[child_index];
                let selected =
                    mixer_strip_is_selected(&child.id, selected_track_id, selected_track_ids);
                MixerStripGeom {
                    x,
                    width: strip_width(),
                    height: strip_height,
                    bg: console::sub_strip_surface(selected),
                    plate: console::plate_fill(selected),
                    separator: console::strip_gap(),
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
    // The batched layer owns the strip's flat fills: the body, the name plate at
    // its foot, and the gap at its right edge. They are painted at the sizes the
    // div layer would have used, so turning the GPU path on or off changes
    // nothing on screen. See `console::name_plate` and `strip_shell`.
    MixerRenderSnapshot::new(
        viewport,
        strips,
        None,
        console::PLATE_H,
        console::STRIP_GAP_W,
    )
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
    input_labels: &std::collections::HashMap<String, String>,
    scroll_x: f32,
    viewport_width: f32,
    viewport_height: f32,
    on_scroll: std::sync::Arc<dyn Fn(f32, &mut gpui::Window, &mut gpui::App) + 'static>,
    split: MixerSplit,
    tree_sidebar: Option<Entity<MixerTreeSidebar>>,
    tree_sidebar_enabled: bool,
    meter_layer: Option<&SharedMeterLayout>,
    i18n: I18n,
) -> impl IntoElement {
    let _shell = crate::perf::PerfScope::enter("MixerShell");
    crate::perf::count("mixer_shell_layout_count", 1);

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
    let strip_count =
        mixer_render_item_count(tracks, collapsed_vsti_output_groups, hidden_mixer_channels);
    let reserve_scrollbar = mixer_reserves_scrollbar(strip_count, viewport_width);
    let (strip_available_px, body_scrolls) =
        mixer_strip_height_with_scrollbar(strip_available_px, body_scrolls, reserve_scrollbar);

    // Optional GPU primitive layer: when active, the channel strips drop their
    // background / accent bar / separator and a single batched `canvas` paints
    // them behind the strip row. The master stays a native pinned strip (its
    // exact pinned x is not known until layout). Opt-in / reversible.
    let gpu_active = mixer_gpu_primitives_active();

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
                meter_layer,
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

        let content_row =
            mixer_meter_body(mixer_body_scroller(content_row, body_scrolls), meter_layer);
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
            .child(mixer_sub_header(
                mixer_header(tracks, strip_count, &callbacks, &split),
                i18n,
            ))
            .child(content_row);
    }

    let scrollbar_row = if reserve_scrollbar {
        mixer_scrollbar_row(strip_count, scroll_x, viewport_width, on_scroll.clone())
    } else {
        None
    };
    let strip_row = mixer_strip_scroller(
        tracks,
        selected_track_id,
        selected_track_ids,
        callbacks.clone(),
        collapsed_vsti_output_groups,
        hidden_mixer_channels,
        vsti_output_meters,
        input_labels,
        scroll_x,
        viewport_width,
        strip_available_px,
        body_scrolls,
        &split,
        on_scroll,
        gpu_active,
        meter_layer,
        i18n,
    );

    let master_block = mixer_master_strip_pinned(
        master,
        monitor,
        on_master,
        &callbacks,
        &split,
        strip_available_px,
        meter_layer,
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

    // The strips, then their scrollbar in its own row underneath.
    let channel_row = match scrollbar_row {
        Some(bar) => div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .when(body_scrolls, |col| {
                col.flex_none()
                    .h(px(strip_available_px + MIXER_SCROLLBAR_ROW))
            })
            .child(channel_row)
            .child(bar),
        None => channel_row,
    };

    let content_row = if tree_sidebar_enabled {
        let mut row = div().flex().flex_row().flex_1().min_h_0();
        if let Some(sidebar) = tree_sidebar {
            row = row.child(sidebar);
        }
        row.child(channel_row)
    } else {
        channel_row
    };
    let content_row = mixer_meter_body(mixer_body_scroller(content_row, body_scrolls), meter_layer);

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
        .child(mixer_sub_header(
            mixer_header(tracks, strip_count, &callbacks, &split),
            i18n,
        ))
        .child(content_row)
}

/// The header's counts and actions for a mixer showing `strip_count` strips
/// of `tracks`.
pub(crate) fn mixer_header<'a>(
    tracks: &[TrackState],
    strip_count: usize,
    callbacks: &MixerCallbacks,
    split: &'a MixerSplit,
) -> MixerHeader<'a> {
    MixerHeader {
        visible_channels: strip_count,
        total_channels: tracks.len(),
        soloed: tracks.iter().filter(|track| track.solo).count(),
        on_clear_solos: callbacks.on_clear_solos.clone(),
        on_add_track: callbacks.on_add_track.clone(),
        split,
    }
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

/// The region a meter layer may paint in: the body as far as it is on screen.
/// It sits beside the vertical scroller, not in it, so a scrolled body never
/// lets a meter paint over the header.
fn mixer_meter_body(
    body: gpui::AnyElement,
    meter_layer: Option<&SharedMeterLayout>,
) -> gpui::AnyElement {
    let Some(layer) = meter_layer else {
        return body;
    };
    div()
        .relative()
        .flex()
        .flex_col()
        .flex_1()
        .min_w_0()
        .min_h_0()
        .child(body)
        .child(clip_region(layer, ClipRegion::Body))
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
    input_labels: &std::collections::HashMap<String, String>,
    scroll_x: f32,
    viewport_width: f32,
    strip_available_px: f32,
    body_scrolls: bool,
    split: &MixerSplit,
    on_scroll: std::sync::Arc<dyn Fn(f32, &mut gpui::Window, &mut gpui::App) + 'static>,
    gpu_decor: bool,
    meter_layer: Option<&SharedMeterLayout>,
    i18n: I18n,
) -> impl IntoElement {
    let _scope = crate::perf::PerfScope::enter("MixerStripScroller");
    crate::perf::count("mixer_strip_layout_count", 1);
    crate::perf::count("mixer_strip_paint_count", 1);

    let render_items =
        collect_mixer_render_items(tracks, collapsed_vsti_output_groups, hidden_mixer_channels);
    let strip_count = render_items.len();
    crate::perf::count("mixer_strips", tracks.len() as u64);

    let total_content_w = strip_count as f32 * strip_width();
    let max_scroll_x = (total_content_w - viewport_width).max(0.0);
    let scroll_x = scroll_x.clamp(0.0, max_scroll_x.max(0.0));
    let spare_channel_w = (viewport_width - total_content_w).max(0.0);

    let visible_range = mixer_visible_item_range(strip_count, scroll_x, viewport_width);
    let visible_start = visible_range.start;
    let visible_end = visible_range.end;

    let left_spacer_w = visible_start as f32 * strip_width();
    let right_spacer_w = strip_count.saturating_sub(visible_end) as f32 * strip_width();

    crate::perf::count(
        "visible_mixer_strips",
        visible_end.saturating_sub(visible_start) as u64,
    );

    let folders = GroupTree::of(tracks);
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
                    &MixerFolderPlacement::of(tracks, &folders, track_index),
                    input_labels.get(&track.id).map(String::as_str),
                    gpu_decor,
                    meter_layer,
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
                    child_index,
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
                    meter_layer,
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
                gpui::ScrollDelta::Lines(l) => (l.x * strip_width(), l.y * strip_width() * 0.5),
            };
            // A panel too short for its strips scrolls them vertically: a
            // plain vertical wheel is that scroll's, not a sideways one.
            if body_scrolls && !event.modifiers.shift && dy.abs() > dx.abs() {
                return;
            }
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
            //
            // Anchored where the strips end rather than sized from the right:
            // the viewport width is an estimate, and when it ran wide the bay
            // slid under the last strips and showed through them.
            d.child(
                div()
                    .absolute()
                    .left(px(total_content_w - scroll_x))
                    .right_0()
                    .top_0()
                    .bottom_0()
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
        .children(meter_layer.map(|layout| clip_region(layout, ClipRegion::Strips)))
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
    meter_layer: Option<&SharedMeterLayout>,
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
            meter_layer,
            i18n,
        ))
        .child(monitor_strip(
            monitor,
            callbacks,
            split,
            strip_available_px,
            meter_layer,
            i18n,
        ))
}

/// A pinned strip's meter: a slot for the meter layer in the docked mixer,
/// drawn in place in the detached Mixer window.
#[allow(clippy::too_many_arguments)]
fn pinned_meter(
    meter_layer: Option<&SharedMeterLayout>,
    source: MeterSource,
    level_l: f32,
    level_r: f32,
    hold_l: f32,
    hold_r: f32,
    clip: bool,
) -> gpui::AnyElement {
    match meter_layer {
        Some(layout) => meter_slot(layout, source).into_any_element(),
        None => meter_surface(level_l, level_r, hold_l, hold_r, clip).into_any_element(),
    }
}

#[cfg(test)]
mod send_drag_tests {
    use super::{send_drag_db, send_norm};
    use crate::components::fader::FaderDrag;

    /// A drag on a strip-width bar from `start_db`, `travel` px to the right.
    fn dragged(start_db: f32, travel: f32, fine: bool) -> f32 {
        let drag = FaderDrag::new("send".into());
        let start = send_norm(start_db);
        // The first sample anchors the drag where the press landed.
        let _ = send_drag_db(&drag, start, 500.0, fine, 70.0);
        send_drag_db(&drag, start, 500.0 + travel, fine, 70.0)
    }

    #[test]
    fn a_few_pixels_move_a_send_by_a_fraction_of_a_db() {
        // A mouse event's usual 3 px: under a dB, not three whole dB.
        let moved = dragged(-10.0, 3.0, false) - -10.0;
        assert!((moved - 0.75).abs() < 0.01, "3 px moved {moved} dB");
        let fine = dragged(-10.0, 3.0, true) - -10.0;
        assert!(
            (fine - 0.075).abs() < 0.01,
            "3 px with Shift moved {fine} dB"
        );
    }

    #[test]
    fn a_press_does_not_move_the_level_and_the_ends_still_hold() {
        assert!((dragged(-10.0, 0.0, false) - -10.0).abs() < 1.0e-4);
        assert_eq!(dragged(-10.0, 10_000.0, false), 6.0);
        assert_eq!(dragged(-10.0, -10_000.0, false), -60.0);
    }
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
    use super::{
        mixer_scroll_x_to_reveal, mixer_strip_is_selected, mixer_visible_item_range, strip_width,
    };

    #[test]
    fn thousand_strip_detail_window_stays_bounded() {
        let first = mixer_visible_item_range(1_000, 0.0, 880.0);
        let middle = mixer_visible_item_range(1_000, 44_000.0, 880.0);
        let end = mixer_visible_item_range(1_000, f32::MAX, 880.0);
        // What fits the viewport, a strip cut at each edge, and the overscan.
        let bound = (880.0 / strip_width()).ceil() as usize + 1 + 2 * super::MIXER_OVERSCAN;
        assert!(first.len() <= bound);
        assert!(middle.len() <= bound);
        assert!(end.len() <= bound);
        assert_eq!(end.end, 1_000);
    }

    #[test]
    fn revealing_a_strip_scrolls_only_when_it_is_off_screen() {
        let viewport = strip_width() * 5.0;
        // Strips 0..5 fill the view at scroll 0.
        assert_eq!(mixer_scroll_x_to_reveal(0, 0.0, viewport, 40), None);
        assert_eq!(mixer_scroll_x_to_reveal(4, 0.0, viewport, 40), None);

        let scroll = mixer_scroll_x_to_reveal(20, 0.0, viewport, 40).expect("off screen");
        let left = 20.0 * strip_width();
        assert!(left >= scroll && left + strip_width() <= scroll + viewport);

        // Back to the start from far right; clamped at the first strip.
        assert_eq!(
            mixer_scroll_x_to_reveal(0, 30.0 * strip_width(), viewport, 40),
            Some(0.0)
        );
        // A strip half under the right edge counts as off screen; once the
        // view has moved one strip it is wholly visible and stays put.
        assert!(mixer_scroll_x_to_reveal(5, strip_width() * 0.5, viewport, 40).is_some());
        assert!(mixer_scroll_x_to_reveal(5, 0.0, viewport, 40).is_some());
        assert!(mixer_scroll_x_to_reveal(5, strip_width(), viewport, 40).is_none());
    }

    #[test]
    fn a_strip_is_selected_exactly_when_its_arrangement_header_is() {
        use crate::components::timeline::timeline_state::TimelineState;

        let ids = |list: &[&str]| list.iter().map(|id| id.to_string()).collect::<Vec<_>>();
        let cases: [(Option<&str>, Vec<String>); 5] = [
            (None, ids(&[])),
            (Some("a"), ids(&[])),
            (Some("a"), ids(&["a", "b"])),
            // A primary set alone (an import, a new track) over a stale list.
            (Some("c"), ids(&["a", "b"])),
            (None, ids(&["a"])),
        ];
        for (primary, selected) in cases {
            let mut selection = TimelineState::default().selection;
            selection.selected_track_id = primary.map(str::to_string);
            selection.selected_track_ids = selected.clone();
            for track in ["a", "b", "c"] {
                assert_eq!(
                    mixer_strip_is_selected(track, primary, &selected),
                    selection.is_track_selected(track),
                    "track {track} with primary {primary:?} and list {selected:?}"
                );
            }
        }
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

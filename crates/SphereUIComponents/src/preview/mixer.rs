//! The mixer, laid out as the bottom panel or the Mixer window lays it out.
//!
//! The session is built through the timeline's own mutations — tracks,
//! inserts, sends, routing — so the strips draw exactly what a project with
//! those channels would show. Only the meter levels are invented: a still,
//! music-shaped frame, since no engine runs here.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use gpui::{
    div, AnyWindowHandle, App, AppContext, Context, Entity, IntoElement, ParentElement, Render,
    Result, Styled, Window, WindowOptions,
};

use crate::components::mixer_panel::{mixer_panel, noop_mixer_callbacks, MixerSplit, MixerView};
use crate::components::mixer_tree_sidebar::MixerTreeCallbacks;
use crate::components::mixer_tree_sidebar_view::MixerTreeSidebar;
use crate::components::timeline::timeline_state::{
    volume, CreateTrackOptions, InputMonitorMode, InsertLoadStatus, InsertPluginFormat,
    TimelineState, TrackOutputRouting, TrackType, MASTER_TRACK_ID,
};
use crate::i18n::I18n;
use crate::theme::Colors;

pub(super) fn mixer(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    open(MixerView::default(), options, cx)
}

/// Wide strips.
pub(super) fn mixer_wide(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    open(
        MixerView {
            wide: true,
            ..MixerView::default()
        },
        options,
        cx,
    )
}

/// I/O and Sends hidden: the fader takes their height.
pub(super) fn mixer_compact(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    open(
        MixerView {
            show_io: false,
            show_sends: false,
            ..MixerView::default()
        },
        options,
        cx,
    )
}

fn open(view: MixerView, options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    let state = session();
    let handle = cx.open_window(options, move |_, cx| {
        let tree = {
            let state = state.clone();
            cx.new(|cx| {
                MixerTreeSidebar::for_preview(state, noop_tree_callbacks(), cx.focus_handle())
            })
        };
        cx.new(|_| MixerPreview { state, view, tree })
    })?;
    Ok(handle.into())
}

/// The tree's callbacks, with nothing behind them: the preview only draws.
fn noop_tree_callbacks() -> MixerTreeCallbacks {
    let channel = Arc::new(|_: &String, _: &mut Window, _: &mut App| {});
    let action = Arc::new(|_: &mut Window, _: &mut App| {});
    MixerTreeCallbacks {
        on_select_channel: channel.clone(),
        on_focus_channel: channel.clone(),
        on_toggle_expand: Arc::new(|_: &(String, bool), _: &mut Window, _: &mut App| {}),
        on_toggle_visibility: channel.clone(),
        on_toggle_pin: channel.clone(),
        on_toggle_mute: channel.clone(),
        on_toggle_solo: channel,
        on_collapse_all: action.clone(),
        on_expand_all: action.clone(),
        on_show_only_selected_group: action.clone(),
        on_reset_visibility: action.clone(),
        on_toggle_sidebar: action,
    }
}

struct MixerPreview {
    state: TimelineState,
    view: MixerView,
    tree: Entity<MixerTreeSidebar>,
}

impl Render for MixerPreview {
    fn render(&mut self, window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let width: f32 = window.bounds().size.width.into();
        let height: f32 = window.bounds().size.height.into();
        // The strip width is the mixer's one shared setting; each scene sets
        // its own before it lays out.
        crate::components::mixer_panel::set_mixer_strips_wide(self.view.wide);
        let state = &self.state;
        let pinned = crate::components::mixer_panel::mixer_pinned_width();
        let tree_w = crate::components::mixer_tree_sidebar::clamp_mixer_tree_sidebar_width(
            crate::components::mixer_tree_sidebar::MIXER_TREE_SIDEBAR_DEFAULT_WIDTH,
        );
        div()
            .size_full()
            .flex()
            .flex_col()
            .font(crate::theme::ui_font())
            .bg(Colors::surface_window())
            .child(mixer_panel(
                &state.tracks,
                &state.master,
                &state.monitor,
                state.selection.selected_track_id.as_deref(),
                &state.selection.selected_track_ids,
                noop_mixer_callbacks(),
                &HashSet::new(),
                &HashSet::new(),
                &HashMap::new(),
                &crate::components::mixer_panel::mixer_input_labels(
                    &state.tracks,
                    &state.audio_connections,
                ),
                0.0,
                (width - pinned - tree_w).max(100.0),
                height,
                Arc::new(|_, _, _| {}),
                MixerSplit {
                    view: self.view,
                    ..MixerSplit::inert()
                },
                Some(self.tree.clone()),
                true,
                None,
                I18n::new("en"),
            ))
    }
}

struct Channel {
    name: &'static str,
    kind: TrackType,
    color: u32,
    db: f32,
    pan: f32,
    inserts: &'static [&'static str],
    bypassed: &'static [&'static str],
    sends: &'static [(&'static str, f32)],
    output: Option<&'static str>,
    /// Peak level, dBFS, for the still meter frame.
    level_db: f32,
}

const CHANNELS: &[Channel] = &[
    Channel {
        name: "Kick",
        kind: TrackType::Audio,
        color: 0xF25C54,
        db: -3.0,
        pan: 0.0,
        inserts: &["EQUZ8", "Zapp"],
        bypassed: &[],
        sends: &[("Reverb", -30.0)],
        output: Some("Drums Bus"),
        level_db: -9.0,
    },
    Channel {
        name: "Snare",
        kind: TrackType::Audio,
        color: 0xF2A23A,
        db: -5.0,
        pan: 0.0,
        inserts: &["EQUZ8"],
        bypassed: &[],
        sends: &[("Reverb", -14.0)],
        output: Some("Drums Bus"),
        level_db: -14.0,
    },
    Channel {
        name: "Bass",
        kind: TrackType::Audio,
        color: 0xF6D55C,
        db: -4.5,
        pan: 0.0,
        inserts: &["EQUZX", "Zapp"],
        bypassed: &["Zapp"],
        sends: &[],
        output: None,
        level_db: -12.0,
    },
    Channel {
        name: "Guitar L",
        kind: TrackType::Audio,
        color: 0x9BE15D,
        db: -7.0,
        pan: -0.7,
        inserts: &["Rodhareist"],
        bypassed: &[],
        sends: &[("Delay", -20.0)],
        output: None,
        level_db: -18.0,
    },
    Channel {
        name: "Guitar R",
        kind: TrackType::Audio,
        color: 0x9BE15D,
        db: -7.0,
        pan: 0.7,
        inserts: &["Rodhareist"],
        bypassed: &[],
        sends: &[("Delay", -20.0)],
        output: None,
        level_db: -17.0,
    },
    Channel {
        name: "Keys",
        kind: TrackType::Audio,
        color: 0x4FD1C5,
        db: -9.0,
        pan: -0.2,
        inserts: &["EQUZX"],
        bypassed: &[],
        sends: &[("Reverb", -16.0)],
        output: None,
        level_db: -22.0,
    },
    Channel {
        name: "Lead Vox",
        kind: TrackType::Audio,
        color: 0x5DB7F5,
        db: -2.0,
        pan: 0.0,
        inserts: &["EQUZ8", "Zapp"],
        bypassed: &[],
        sends: &[("Reverb", -10.0), ("Delay", -14.0)],
        output: None,
        level_db: -6.0,
    },
    Channel {
        name: "BG Vox",
        kind: TrackType::Audio,
        color: 0xB9A6FF,
        db: -10.0,
        pan: 0.3,
        inserts: &["EQUZX"],
        bypassed: &[],
        sends: &[("Reverb", -8.0)],
        output: None,
        level_db: -20.0,
    },
    Channel {
        name: "Drums Bus",
        kind: TrackType::Bus,
        color: 0xF25C54,
        db: -1.0,
        pan: 0.0,
        inserts: &["MixStation"],
        bypassed: &[],
        sends: &[],
        output: None,
        level_db: -5.0,
    },
    Channel {
        name: "Reverb",
        kind: TrackType::Return,
        color: 0x8F8F8F,
        db: -8.0,
        pan: 0.0,
        inserts: &["VerbSpace"],
        bypassed: &[],
        sends: &[],
        output: None,
        level_db: -26.0,
    },
    Channel {
        name: "Delay",
        kind: TrackType::Return,
        color: 0x8F8F8F,
        db: -12.0,
        pan: 0.0,
        inserts: &["EchoSpace"],
        bypassed: &[],
        sends: &[],
        output: None,
        level_db: -30.0,
    },
];

fn amplitude(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

fn session() -> TimelineState {
    let mut state = TimelineState::default();
    let mut ids = Vec::new();
    for channel in CHANNELS {
        let id = state.create_track(CreateTrackOptions {
            name: channel.name.to_string(),
            track_type: channel.kind,
            color: gpui::rgb(channel.color),
            volume: volume::db_to_norm(channel.db),
            pan: channel.pan,
            armed: false,
            input_monitor: InputMonitorMode::Off,
        });
        ids.push(id);
    }
    let id_of = |name: &str| {
        CHANNELS
            .iter()
            .position(|channel| channel.name == name)
            .map(|index| ids[index].clone())
    };
    for (channel, id) in CHANNELS.iter().zip(&ids) {
        for (index, plugin) in channel.inserts.iter().enumerate() {
            let Some(slot) = state.ensure_insert_slot_at(id, index) else {
                continue;
            };
            state.set_insert_plugin(
                id,
                &slot,
                plugin.to_lowercase(),
                None,
                InsertPluginFormat::Vst3,
                None,
                plugin.to_string(),
            );
        }
        for (target, gain_db) in channel.sends {
            let Some(target_id) = id_of(target) else {
                continue;
            };
            if let Some(send_id) = state.add_send_to_target(id, &target_id) {
                if let Some(send) = state
                    .tracks
                    .iter_mut()
                    .find(|track| &track.id == id)
                    .and_then(|track| track.sends.iter_mut().find(|send| send.id == send_id))
                {
                    send.gain_db = *gain_db;
                }
            }
        }
        if let Some(bus) = channel.output.and_then(id_of) {
            state.set_track_output_routing(id, TrackOutputRouting::Bus { bus_id: bus });
        }
    }
    for (channel, track) in CHANNELS.iter().zip(state.tracks.iter_mut()) {
        for slot in &mut track.inserts {
            slot.load_status = InsertLoadStatus::Ready;
            slot.bypassed = channel.bypassed.contains(&slot.display_name.as_str());
        }
        let left = amplitude(channel.level_db + 1.5 * channel.pan.min(0.0).abs());
        let right = amplitude(channel.level_db - 1.5 * channel.pan.max(0.0));
        track.meter_level_l = left;
        track.meter_level_r = right;
        track.meter_peak_hold_l = left * 1.25;
        track.meter_peak_hold_r = right * 1.25;
    }
    for (index, plugin) in ["EQUZX", "BurnLimit"].iter().enumerate() {
        if let Some(slot) = state.ensure_insert_slot_at(MASTER_TRACK_ID, index) {
            state.set_insert_plugin(
                MASTER_TRACK_ID,
                &slot,
                plugin.to_lowercase(),
                None,
                InsertPluginFormat::Vst3,
                None,
                plugin.to_string(),
            );
        }
    }
    for slot in &mut state.master.inserts {
        slot.load_status = InsertLoadStatus::Ready;
    }
    if let Some(vox) = id_of("Lead Vox") {
        state.selection.selected_track_ids.clear();
        state.selection.select_track(&vox);
    }
    if let Some(snare) = id_of("Snare") {
        state.toggle_track_solo(&snare);
    }
    if let Some(bass) = id_of("Bass") {
        state.toggle_track_mute(&bass);
    }
    state.master.meter_level_l = amplitude(-4.0);
    state.master.meter_level_r = amplitude(-4.6);
    state.master.meter_peak_hold_l = amplitude(-1.8);
    state.master.meter_peak_hold_r = amplitude(-2.2);
    state.monitor.meter_level_l = amplitude(-16.0);
    state.monitor.meter_level_r = amplitude(-16.5);
    crate::components::mixer_tree_model::ensure_timeline_mixer_tree_defaults(&mut state, 2);
    state
}

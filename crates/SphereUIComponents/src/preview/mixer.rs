//! The mixer, laid out as the bottom panel or the Mixer window lays it out.
//!
//! The session is built through the timeline's own mutations — tracks,
//! inserts, sends, routing — so the strips draw exactly what a project with
//! those channels would show. The built-in EQs' settings go through Studio's
//! state mirror, as a loaded project's would. Invented, since no engine or
//! plug-in host runs here: the meter levels (a still, music-shaped frame) and
//! the parameter values the Pad's Pro-Q 3 reports.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use gpui::{
    div, AnyWindowHandle, App, AppContext, Context, Entity, IntoElement, ParentElement, Render,
    Result, Styled, Window, WindowOptions,
};

use crate::components::builtin_plugin_editor::builtin_state_seed;
use crate::components::mixer_panel::{mixer_panel, noop_mixer_callbacks, MixerSplit, MixerView};
use crate::components::mixer_tree_sidebar::MixerTreeCallbacks;
use crate::components::mixer_tree_sidebar_view::MixerTreeSidebar;
use crate::components::timeline::timeline_state::{
    volume, CreateTrackOptions, InputMonitorMode, InsertLoadStatus, InsertPluginFormat,
    PluginParameterState, TimelineState, TrackOutputRouting, TrackType, MASTER_TRACK_ID,
};
use crate::i18n::I18n;
use crate::theme::Colors;

pub(super) fn mixer(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    open(MixerView::default(), options, cx)
}

/// The same session in the Community Edition: no EQ curve, and the
/// pre-fader send and post-fader limiter shown but not changeable.
pub(super) fn mixer_community(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    open_as(MixerView::default(), false, options, cx)
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
    open_as(view, true, options, cx)
}

/// `professional`: render as the Professional Edition, with its mixing tools.
fn open_as(
    view: MixerView,
    professional: bool,
    options: WindowOptions,
    cx: &mut App,
) -> Result<AnyWindowHandle> {
    let state = session();
    let handle = cx.open_window(options, move |_, cx| {
        let tree = {
            let state = state.clone();
            cx.new(|cx| {
                MixerTreeSidebar::for_preview(state, noop_tree_callbacks(), cx.focus_handle())
            })
        };
        cx.new(|_| MixerPreview {
            state,
            view,
            tree,
            professional,
        })
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
    professional: bool,
}

/// Install a Professional edition provider, or remove it. Global, so each
/// scene sets its own before it lays out, like the strip width.
fn set_edition(professional: bool) {
    if professional {
        crate::edition::set_edition_provider(std::sync::Arc::new(|| {
            crate::edition::EditionInfo {
                edition: "Professional",
                // Not `edition::app_version()`: with no version set it asks
                // this provider, which would ask it back.
                app_version: env!("CARGO_PKG_VERSION").to_string(),
                license: None,
                audio_engine: None,
            }
        }));
    } else {
        crate::edition::clear_edition_provider();
    }
}

impl Render for MixerPreview {
    fn render(&mut self, window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let width: f32 = window.bounds().size.width.into();
        let height: f32 = window.bounds().size.height.into();
        // The strip width is the mixer's one shared setting; each scene sets
        // its own before it lays out.
        crate::components::mixer_panel::set_mixer_strips_wide(self.view.wide);
        set_edition(self.professional);
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
        // A bypassed EQ: its curve stays, flat and dim.
        bypassed: &["EQUZX"],
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
        name: "Pad",
        kind: TrackType::Audio,
        color: 0x7FB2FF,
        db: -11.0,
        pan: 0.0,
        inserts: &[PRO_Q_3],
        bypassed: &[],
        sends: &[("Reverb", -12.0)],
        output: None,
        level_db: -24.0,
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

const PRO_Q_3: &str = "FabFilter Pro-Q 3";

fn amplitude(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// An EQ-Z8 setting for `channel`.
fn equz8_params(channel: &str) -> equz8::Params {
    use equz8::BandType;
    let mut params = equz8::default_params();
    let mut band = |slot: usize, kind: BandType, freq: f32, gain_db: f32, q: f32| {
        let band = &mut params.bands[slot];
        band.active = true;
        band.band_type = kind;
        band.freq = freq;
        band.gain_db = gain_db;
        band.q = q;
    };
    match channel {
        "Kick" => {
            band(0, BandType::HighPass, 30.0, 0.0, 0.7);
            band(1, BandType::LowShelf, 70.0, 4.0, 0.8);
            band(3, BandType::Bell, 380.0, -5.0, 1.8);
            band(5, BandType::Bell, 3_500.0, 3.0, 1.4);
        }
        "Snare" => {
            band(0, BandType::HighPass, 90.0, 0.0, 0.7);
            band(2, BandType::Bell, 200.0, 3.0, 1.2);
            band(4, BandType::Bell, 900.0, -3.5, 2.0);
            band(6, BandType::HighShelf, 7_000.0, 2.5, 0.8);
        }
        _ => {
            band(0, BandType::HighPass, 100.0, 0.0, 0.7);
            band(3, BandType::Bell, 320.0, -2.5, 1.4);
            band(5, BandType::Bell, 3_000.0, 2.0, 1.0);
            band(6, BandType::HighShelf, 10_000.0, 3.5, 0.8);
        }
    }
    params
}

/// An EQ-ZX setting for `channel`: a low-end shape for the bass, the mix-bus
/// curve of the EQ-ZX scene otherwise.
fn equzx_params(channel: &str) -> equzx::Params {
    let mut params = super::equzx_mix_params();
    if channel == "Bass" {
        let band = &mut params.bands[1];
        band.band_type = equzx::BandType::LowShelf;
        band.freq = 90.0;
        band.gain_db = 3.5;
        band.q = 0.8;
    }
    params
}

/// A Pro-Q 3's parameter list as its host would report it, with the value
/// texts the plug-in gives: a low cut, a broad dip, presence and air.
fn pro_q3_parameters() -> Vec<PluginParameterState> {
    let bands: [(&str, &str, &str, &str, &str); 4] = [
        ("40.00 Hz", "0.00 dB", "0.707", "Low Cut", "24 dB/oct"),
        ("450.0 Hz", "-3.50 dB", "0.800", "Bell", "12 dB/oct"),
        ("2500.0 Hz", "2.50 dB", "1.400", "Bell", "12 dB/oct"),
        ("12000 Hz", "4.00 dB", "0.700", "High Shelf", "12 dB/oct"),
    ];
    let mut id = 0;
    let mut param = |name: String, text: &str| {
        id += 1;
        PluginParameterState {
            id,
            name,
            value_normalized: 0.5,
            automatable: true,
            hidden: false,
            read_only: false,
            unit: String::new(),
            value_text: text.to_string(),
        }
    };
    let mut parameters = Vec::new();
    for (index, (freq, gain, q, shape, slope)) in bands.iter().enumerate() {
        let n = index + 1;
        parameters.push(param(format!("Band {n} Used"), "Used"));
        parameters.push(param(format!("Band {n} Enabled"), "Enabled"));
        parameters.push(param(format!("Band {n} Frequency"), freq));
        parameters.push(param(format!("Band {n} Gain"), gain));
        parameters.push(param(format!("Band {n} Q"), q));
        parameters.push(param(format!("Band {n} Shape"), shape));
        parameters.push(param(format!("Band {n} Slope"), slope));
    }
    parameters.push(param("Band 5 Used".to_string(), "Unused"));
    parameters.push(param("Band 5 Frequency".to_string(), "1000.0 Hz"));
    parameters.push(param("Output Level".to_string(), "0.00 dB"));
    parameters.push(param("Gain Scale".to_string(), "100.0%"));
    parameters.push(param("Bypass".to_string(), "Not Bypassed"));
    parameters
}

/// Puts the EQ settings of every EQ insert in `state` where the mixer reads
/// them: the state mirror for the built-ins, the reported parameter list for
/// the Pro-Q.
fn seed_eqs(state: &mut TimelineState) {
    let mut reported = Vec::new();
    let owners = state
        .tracks
        .iter()
        .map(|track| (track.id.clone(), track.name.clone(), &track.inserts))
        .chain(std::iter::once((
            MASTER_TRACK_ID.to_string(),
            "Master".to_string(),
            &state.master.inserts,
        )));
    for (track_id, name, inserts) in owners {
        for slot in inserts {
            // Slots hold the built-ins under their stems, as a picked insert does.
            match slot.plugin_id.as_deref() {
                Some(id @ "equz8") => {
                    if let Ok(json) = equz8::ipc::Equz8State::new(equz8_params(&name)).to_json() {
                        builtin_state_seed(id, &slot.id, json.as_bytes());
                    }
                }
                Some(id @ "equzx") => {
                    if let Ok(json) = equzx::ipc::EquzxState::new(equzx_params(&name)).to_json() {
                        builtin_state_seed(id, &slot.id, json.as_bytes());
                    }
                }
                _ if slot.display_name == PRO_Q_3 => {
                    reported.push((track_id.clone(), slot.id.clone()));
                }
                _ => {}
            }
        }
    }
    for (track_id, insert_id) in reported {
        state.set_insert_parameters(&track_id, &insert_id, pro_q3_parameters());
    }
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
    seed_eqs(&mut state);
    // The master's limiter after its fader, and the vocal's delay send taken
    // before its fader: one of each on screen.
    if let Some(limiter) = state
        .master
        .inserts
        .iter()
        .find(|slot| slot.display_name == "BurnLimit")
        .map(|slot| slot.id.clone())
    {
        state.set_insert_post_fader(MASTER_TRACK_ID, &limiter, true);
    }
    // The drums under one VCA, pulled down 3 dB.
    let drums: Vec<String> = ["Kick", "Snare"]
        .iter()
        .filter_map(|name| id_of(name))
        .collect();
    let vca = state.create_vca_group("Drums", &drums);
    state.set_vca_level(&vca, volume::db_to_norm(-3.0));
    if let Some(vox) = id_of("Lead Vox") {
        let delay_send = state.find_track(&vox).and_then(|track| {
            track
                .sends
                .iter()
                .find(|send| send.target_name == "Delay")
                .map(|send| send.id.clone())
        });
        if let Some(send) = delay_send {
            state.set_send_pre_fader(&vox, &send, true);
        }
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

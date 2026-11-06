//! Chain and track presets, saved as `.pst` — the same FBPST file a one
//! plug-in preset uses, in its bundle form (`SpherePluginHost::preset`).
//!
//! * **FX chain preset**: a track's effects in order, each with its state as
//!   the plug-in has it now. Loads onto any track that can take it, replacing
//!   its effects and keeping its instrument — Paste FX Chain from a file.
//! * **Track preset**: the whole channel — kind, name, colour, level, pan,
//!   input monitor, and every plug-in on it, instrument first. Loads as a new
//!   track.
//!
//! The model builds the slots; this is the studio's half, the same as Copy /
//! Paste FX Chain: capturing the plug-ins' state, tearing down what a load
//! replaces, loading the new instances and syncing the engine. A chain load is
//! one undo step; a track load is two (the track, then its plug-ins), as
//! dropping an instrument on empty space already is.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::Context;
use SpherePluginHost::preset::{BundleKind, BundlePreset, PresetSlot, PresetTrack};

use crate::components::timeline::timeline_state::{
    CreateTrackOptions, InputMonitorMode, InsertPluginFormat, InsertSlotState, TrackEditScope,
    TrackType, MASTER_TRACK_ID,
};
use crate::project::{
    project_insert_to_timeline, PluginFormat, PluginStateBlob, ProjectInsert, ProjectPluginInstance,
};

use super::StudioLayout;

/// The one extension every preset this app writes uses.
const PRESET_EXTENSION: &str = "pst";

/// Where FX chain presets are kept: `Documents/Futureboard Studio/Presets/Chains`.
pub(crate) fn chain_preset_dir() -> PathBuf {
    crate::paths::FutureboardPaths::resolve()
        .presets
        .join("Chains")
}

/// Where track presets are kept: `Documents/Futureboard Studio/Presets/Tracks`.
pub(crate) fn track_preset_dir() -> PathBuf {
    crate::paths::FutureboardPaths::resolve()
        .presets
        .join("Tracks")
}

// ── Slots ───────────────────────────────────────────────────────────────────

fn format_label(format: Option<InsertPluginFormat>) -> &'static str {
    match format {
        Some(InsertPluginFormat::Vst3) => "VST3",
        Some(InsertPluginFormat::Vst2) => "VST2",
        Some(InsertPluginFormat::Clap) => "CLAP",
        Some(InsertPluginFormat::Au) => "AU",
        Some(InsertPluginFormat::Lv2) => "LV2",
        _ => "",
    }
}

fn parse_format(label: &str) -> PluginFormat {
    match label.to_ascii_uppercase().as_str() {
        "VST3" => PluginFormat::Vst3,
        "VST2" => PluginFormat::Vst2,
        "CLAP" => PluginFormat::Clap,
        "AU" => PluginFormat::Au,
        "LV2" => PluginFormat::Lv2,
        _ => PluginFormat::Unknown,
    }
}

/// A loaded insert as a preset slot, with the state captured for it (or the
/// state it already holds when the plug-in returned none).
fn slot_to_preset(slot: &InsertSlotState, state: Option<Arc<Vec<u8>>>) -> PresetSlot {
    let state = state
        .or_else(|| slot.vst3_state.clone())
        .map(|state| state.as_ref().clone())
        .unwrap_or_default();
    PresetSlot {
        plugin_id: slot.plugin_id.clone().unwrap_or_default(),
        plugin_name: slot.display_name.clone(),
        vendor: slot.vendor.clone().unwrap_or_default(),
        format: format_label(slot.plugin_format).to_string(),
        plugin_path: slot
            .plugin_path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_default(),
        version: None,
        is_instrument: slot.plugin_is_instrument,
        display_name: slot.display_name.clone(),
        enabled: slot.enabled,
        bypassed: slot.bypassed,
        output_channels: slot.enabled_audio_output_channels.clone(),
        multiout_collapsed: slot.multiout_collapsed,
        state,
    }
}

/// A preset slot as an insert, built the way a project file's insert is, so
/// a plug-in missing on this machine comes back marked missing rather than
/// pretending to load. The id is a placeholder: the model gives each copy its
/// own.
fn slot_from_preset(slot: &PresetSlot) -> InsertSlotState {
    let format = parse_format(&slot.format);
    project_insert_to_timeline(&ProjectInsert {
        id: "preset".to_string(),
        slot_index: 0,
        bypassed: slot.bypassed,
        enabled: slot.enabled,
        enabled_audio_output_channels: slot.output_channels.clone(),
        plugin_is_instrument: slot.is_instrument,
        multiout_collapsed: slot.multiout_collapsed,
        plugin: Some(ProjectPluginInstance {
            instance_id: "preset".to_string(),
            format,
            plugin_path: (!slot.plugin_path.is_empty()).then(|| PathBuf::from(&slot.plugin_path)),
            plugin_uid: slot.plugin_id.clone(),
            display_name: slot.display_name.clone(),
            state: PluginStateBlob {
                plugin_id: slot.plugin_id.clone(),
                format: Some(format),
                state_bytes: slot.state.clone(),
                vendor: (!slot.vendor.is_empty()).then(|| slot.vendor.clone()),
                name: (!slot.plugin_name.is_empty()).then(|| slot.plugin_name.clone()),
                version: slot.version.clone(),
            },
        }),
    })
}

// ── Track settings ──────────────────────────────────────────────────────────

fn track_type_label(track_type: TrackType) -> &'static str {
    match track_type {
        TrackType::Audio => "audio",
        TrackType::Midi => "midi",
        TrackType::Instrument => "instrument",
        TrackType::Bus => "bus",
        TrackType::Return => "return",
        _ => "audio",
    }
}

fn parse_track_type(label: &str) -> TrackType {
    match label {
        "midi" => TrackType::Midi,
        "instrument" => TrackType::Instrument,
        "bus" => TrackType::Bus,
        "return" => TrackType::Return,
        _ => TrackType::Audio,
    }
}

fn monitor_label(mode: InputMonitorMode) -> &'static str {
    match mode {
        InputMonitorMode::Off => "off",
        InputMonitorMode::WhenRecordArmed => "auto",
        InputMonitorMode::Always => "always",
    }
}

fn parse_monitor(label: &str) -> InputMonitorMode {
    match label {
        "auto" => InputMonitorMode::WhenRecordArmed,
        "always" => InputMonitorMode::Always,
        _ => InputMonitorMode::Off,
    }
}

fn color_hex(color: gpui::Rgba) -> String {
    let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!(
        "#{:02x}{:02x}{:02x}",
        byte(color.r),
        byte(color.g),
        byte(color.b)
    )
}

fn parse_color(hex: &str) -> Option<gpui::Rgba> {
    let hex = hex.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let value = u32::from_str_radix(hex, 16).ok()?;
    Some(gpui::rgb(value))
}

/// A file name for `name`: the characters Windows refuses become spaces.
fn file_stem(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => ' ',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim().trim_end_matches('.').trim();
    if trimmed.is_empty() {
        "Preset".to_string()
    } else {
        trimmed.to_string()
    }
}

fn with_extension(path: PathBuf) -> PathBuf {
    if path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case(PRESET_EXTENSION))
    {
        path
    } else {
        let mut name = path.into_os_string();
        name.push(".");
        name.push(PRESET_EXTENSION);
        PathBuf::from(name)
    }
}

impl StudioLayout {
    // ── Save ────────────────────────────────────────────────────────────────

    /// The slots as preset slots, each with its state as the plug-in has it
    /// now. Captured before any dialog opens: what is saved is the sound the
    /// user heard when they chose Save.
    fn capture_preset_slots(
        &mut self,
        track_id: &str,
        slots: &[InsertSlotState],
        cx: &mut Context<Self>,
    ) -> Vec<PresetSlot> {
        let ids: Vec<String> = slots.iter().map(|slot| slot.id.clone()).collect();
        let mut states = self.capture_insert_states(track_id, &ids, cx);
        slots
            .iter()
            .map(|slot| slot_to_preset(slot, states.remove(&slot.id)))
            .collect()
    }

    /// Save the context (or selected) track's effects as an FX chain preset.
    pub(super) fn save_context_chain_preset(&mut self, cx: &mut Context<Self>) {
        let Some(track_id) = self.context_track_id_or_selected(cx) else {
            return;
        };
        let (track_name, effects) = {
            let state = &self.timeline.read(cx).state;
            let name = if track_id == MASTER_TRACK_ID {
                "Master".to_string()
            } else {
                state
                    .find_track(&track_id)
                    .map(|track| track.name.clone())
                    .unwrap_or_default()
            };
            (name, state.fx_chain_effects(&track_id))
        };
        if effects.is_empty() {
            return;
        }
        let slots = self.capture_preset_slots(&track_id, &effects, cx);
        let name = format!("{track_name} Chain");
        let preset = BundlePreset {
            kind: BundleKind::Chain,
            name: name.clone(),
            slots,
            track: None,
        };
        eprintln!(
            "[ChainPreset] saving chain track={track_id} effects={}",
            preset.slots.len()
        );
        self.save_bundle_through_dialog(
            preset,
            chain_preset_dir(),
            name,
            "Save FX Chain Preset",
            cx,
        );
    }

    /// Save the context (or selected) track — its settings and every plug-in
    /// on it — as a track preset.
    pub(super) fn save_context_track_preset(&mut self, cx: &mut Context<Self>) {
        let Some(track_id) = self.context_track_id_or_selected(cx) else {
            return;
        };
        let captured = {
            let state = &self.timeline.read(cx).state;
            state
                .find_track(&track_id)
                .filter(|track| can_save_as_track_preset(track.track_type))
                .map(|track| {
                    let slots: Vec<InsertSlotState> = track
                        .inserts
                        .iter()
                        .filter(|slot| !slot.is_empty())
                        .cloned()
                        .collect();
                    let settings = PresetTrack {
                        track_type: track_type_label(track.track_type).to_string(),
                        name: track.name.clone(),
                        color: color_hex(track.color),
                        volume: track.volume,
                        pan: track.pan,
                        input_monitor: monitor_label(track.input_monitor).to_string(),
                    };
                    (slots, settings)
                })
        };
        let Some((slots, settings)) = captured else {
            return;
        };
        let slots = self.capture_preset_slots(&track_id, &slots, cx);
        let name = settings.name.clone();
        let preset = BundlePreset {
            kind: BundleKind::Track,
            name: name.clone(),
            slots,
            track: Some(settings),
        };
        eprintln!(
            "[ChainPreset] saving track preset track={track_id} plugins={}",
            preset.slots.len()
        );
        self.save_bundle_through_dialog(preset, track_preset_dir(), name, "Save Track Preset", cx);
    }

    #[cfg(feature = "native-dialogs")]
    fn save_bundle_through_dialog(
        &mut self,
        preset: BundlePreset,
        dir: PathBuf,
        suggested: String,
        title: &'static str,
        cx: &mut Context<Self>,
    ) {
        let _ = std::fs::create_dir_all(&dir);
        let file_name = format!("{}.{PRESET_EXTENSION}", file_stem(&suggested));
        cx.spawn(async move |_this, _cx| {
            let Some(handle) = rfd::AsyncFileDialog::new()
                .set_title(title)
                .set_directory(&dir)
                .set_file_name(file_name)
                .add_filter("Futureboard Preset", &[PRESET_EXTENSION])
                .save_file()
                .await
            else {
                return; // cancelled
            };
            let path = with_extension(handle.path().to_path_buf());
            // The file is named for what the user typed.
            let mut preset = preset;
            if let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) {
                preset.name = stem.to_string();
                if let Some(track) = preset.track.as_mut() {
                    track.name = stem.to_string();
                }
            }
            match SpherePluginHost::preset::write_bundle_preset(&path, &preset) {
                Ok(()) => eprintln!(
                    "[ChainPreset] saved {} plugins={}",
                    path.display(),
                    preset.slots.len()
                ),
                Err(error) => {
                    eprintln!("[ChainPreset] could not write {}: {error}", path.display());
                    // A save the user asked for that silently does nothing is
                    // worse than an extra dialog.
                    rfd::AsyncMessageDialog::new()
                        .set_level(rfd::MessageLevel::Error)
                        .set_title(title)
                        .set_description(format!("Could not save {}:\n{error}", path.display()))
                        .show()
                        .await;
                }
            }
        })
        .detach();
    }

    /// Without native dialogs there is nowhere to ask for a path, so the preset
    /// goes into its folder under the suggested name.
    #[cfg(not(feature = "native-dialogs"))]
    fn save_bundle_through_dialog(
        &mut self,
        preset: BundlePreset,
        dir: PathBuf,
        suggested: String,
        _title: &'static str,
        _cx: &mut Context<Self>,
    ) {
        let path = dir.join(format!("{}.{PRESET_EXTENSION}", file_stem(&suggested)));
        if let Err(error) = SpherePluginHost::preset::write_bundle_preset(&path, &preset) {
            eprintln!("[ChainPreset] could not write {}: {error}", path.display());
        }
    }

    // ── Load ────────────────────────────────────────────────────────────────

    /// Ask for a chain or track preset and load it: a chain onto the context
    /// (or selected) track, a track as a new track.
    #[cfg(feature = "native-dialogs")]
    pub(super) fn open_context_preset(&mut self, cx: &mut Context<Self>) {
        let track_id = self.context_track_id_or_selected(cx);
        let dir = chain_preset_dir();
        let _ = std::fs::create_dir_all(&dir);
        cx.spawn(async move |this, cx| {
            let Some(handle) = rfd::AsyncFileDialog::new()
                .set_title("Load Preset")
                .set_directory(&dir)
                .add_filter("Futureboard Preset", &[PRESET_EXTENSION])
                .pick_file()
                .await
            else {
                return;
            };
            let path = handle.path().to_path_buf();
            let preset = match SpherePluginHost::preset::read_bundle_preset(&path) {
                Ok(preset) => preset,
                Err(error) => {
                    rfd::AsyncMessageDialog::new()
                        .set_level(rfd::MessageLevel::Warning)
                        .set_title("Load Preset")
                        .set_description(format!(
                            "{} is not a chain or track preset:\n{error}",
                            path.display()
                        ))
                        .show()
                        .await;
                    return;
                }
            };
            let _ = this.update(cx, |layout, cx| {
                layout.load_bundle_preset(track_id.as_deref(), &preset, None, cx);
            });
        })
        .detach();
    }

    #[cfg(not(feature = "native-dialogs"))]
    pub(super) fn open_context_preset(&mut self, _cx: &mut Context<Self>) {
        eprintln!("[ChainPreset] native file dialogs are disabled in this build");
    }

    /// A chain or track preset file dropped on the arrangement (`track_id` is
    /// the track under the drop, `None` for empty space) or a mixer strip.
    /// Returns whether the file was a bundle at all; a one-plug-in `.pst` is
    /// the caller's.
    pub(super) fn apply_dropped_bundle_preset(
        &mut self,
        track_id: Option<&str>,
        path: &Path,
        insert_index: Option<usize>,
        cx: &mut Context<Self>,
    ) -> bool {
        use SpherePluginHost::preset::PstKind;
        if !matches!(
            SpherePluginHost::preset::pst_kind(path),
            Some(PstKind::Bundle(_))
        ) {
            return false;
        }
        match SpherePluginHost::preset::read_bundle_preset(path) {
            Ok(preset) => self.load_bundle_preset(track_id, &preset, insert_index, cx),
            Err(error) => eprintln!("[ChainPreset] could not read {}: {error}", path.display()),
        }
        true
    }

    /// Load a chain onto `track_id` (or onto a new audio track when there is
    /// none), or a track preset as a new track.
    pub(super) fn load_bundle_preset(
        &mut self,
        track_id: Option<&str>,
        preset: &BundlePreset,
        insert_index: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        match preset.kind {
            BundleKind::Track => {
                let _ = self.load_track_preset(preset, insert_index, cx);
            }
            BundleKind::Chain => {
                let target = match track_id {
                    Some(track_id) => track_id.to_string(),
                    None => self.create_preset_track(
                        CreateTrackOptions {
                            track_type: TrackType::Audio,
                            name: preset.name.clone(),
                            color: {
                                let state = &self.timeline.read(cx).state;
                                state.track_color_for_index(state.tracks.len())
                            },
                            volume: crate::components::timeline::timeline_state::volume::db_to_norm(
                                0.0,
                            ),
                            pan: 0.0,
                            armed: false,
                            input_monitor: InputMonitorMode::Off,
                        },
                        insert_index,
                        cx,
                    ),
                };
                self.load_chain_preset_onto(&target, preset, cx);
            }
        }
    }

    /// Replace `track_id`'s effects with the chain's, keeping its instrument.
    /// One undo step. Returns whether anything was loaded.
    pub(super) fn load_chain_preset_onto(
        &mut self,
        track_id: &str,
        preset: &BundlePreset,
        cx: &mut Context<Self>,
    ) -> bool {
        // A chain is effects; an instrument in a hand-edited file is left out
        // rather than replacing the track's own.
        let effects: Vec<InsertSlotState> = preset
            .slots
            .iter()
            .filter(|slot| slot.is_instrument != Some(true))
            .map(slot_from_preset)
            .collect();
        let replaced = {
            let state = &self.timeline.read(cx).state;
            if !state.can_take_fx_chain(track_id, effects.len()) {
                eprintln!(
                    "[ChainPreset] refused '{}' on track={track_id}: {} effects do not fit",
                    preset.name,
                    effects.len()
                );
                return false;
            }
            state.fx_chain_ids(track_id)
        };
        // The undo step keeps the replaced effects as they sound now.
        self.store_live_plugin_states(track_id, &replaced, cx);
        let before = self.capture_insert_chains(&[track_id], cx);
        for insert_id in &replaced {
            self.teardown_insert_instance(track_id, insert_id, cx, "load_chain_preset");
        }
        let copies = self.timeline.update(cx, |timeline, cx| {
            let copies = timeline.state.replace_fx_chain(track_id, &effects);
            cx.notify();
            copies
        });
        let Some(copies) = copies else {
            return false;
        };
        eprintln!(
            "[ChainPreset] loaded chain '{}' onto track={track_id} replaced={} effects={}",
            preset.name,
            replaced.len(),
            copies.len()
        );
        self.record_insert_chains("Load FX Chain Preset", before, cx);
        self.load_copied_inserts(track_id, &copies, cx);
        self.after_channel_chain_edit(cx, "load_chain_preset");
        true
    }

    /// A new track from a track preset, selected, with every plug-in loaded.
    pub(super) fn load_track_preset(
        &mut self,
        preset: &BundlePreset,
        insert_index: Option<usize>,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        let settings = preset.track.as_ref()?;
        let track_type = parse_track_type(&settings.track_type);
        let color = parse_color(&settings.color).unwrap_or_else(|| {
            let state = &self.timeline.read(cx).state;
            state.track_color_for_index(state.tracks.len())
        });
        let track_id = self.create_preset_track(
            CreateTrackOptions {
                track_type,
                name: if settings.name.is_empty() {
                    preset.name.clone()
                } else {
                    settings.name.clone()
                },
                color,
                volume: settings.volume,
                pan: settings.pan.clamp(-1.0, 1.0),
                armed: false,
                input_monitor: parse_monitor(&settings.input_monitor),
            },
            insert_index,
            cx,
        );
        let slots: Vec<InsertSlotState> = preset.slots.iter().map(slot_from_preset).collect();
        if slots.is_empty() {
            return Some(track_id);
        }
        let before = self.capture_insert_chains(&[track_id.as_str()], cx);
        let copies = self.timeline.update(cx, |timeline, cx| {
            let copies = timeline.state.replace_insert_chain(&track_id, &slots);
            cx.notify();
            copies
        });
        let Some(copies) = copies else {
            eprintln!(
                "[ChainPreset] track preset '{}' plug-ins do not fit track={track_id}",
                preset.name
            );
            return Some(track_id);
        };
        eprintln!(
            "[ChainPreset] loaded track preset '{}' as track={track_id} plugins={}",
            preset.name,
            copies.len()
        );
        self.record_insert_chains("Load Track Preset", before, cx);
        self.load_copied_inserts(&track_id, &copies, cx);
        self.after_channel_chain_edit(cx, "load_track_preset");
        Some(track_id)
    }

    /// A fresh track for a preset, selected, moved to `insert_index` when one
    /// is given. Its own undo step, before the plug-ins that go on it.
    fn create_preset_track(
        &mut self,
        options: CreateTrackOptions,
        insert_index: Option<usize>,
        cx: &mut Context<Self>,
    ) -> String {
        self.timeline.update(cx, |timeline, cx| {
            let edit = timeline.begin_track_edit(TrackEditScope::track_list());
            let id = timeline.state.create_track(options);
            if let Some(insert_index) = insert_index {
                if insert_index < timeline.state.tracks.len() - 1 {
                    timeline.state.reorder_track(&id, insert_index);
                }
            }
            timeline.state.select_track(&id);
            timeline.commit_track_edit("Add Track", edit, false, cx);
            cx.notify();
            id
        })
    }

    /// Whether the context track's menu should offer Save Track as Preset.
    pub(super) fn can_save_track_preset(&self, track_id: &str, cx: &Context<Self>) -> bool {
        self.timeline
            .read(cx)
            .state
            .find_track(track_id)
            .is_some_and(|track| can_save_as_track_preset(track.track_type))
    }
}

/// Tracks a track preset can stand for: a channel with plug-ins that can be
/// created again. Master, video and group tracks are not.
fn can_save_as_track_preset(track_type: TrackType) -> bool {
    matches!(
        track_type,
        TrackType::Audio
            | TrackType::Midi
            | TrackType::Instrument
            | TrackType::Bus
            | TrackType::Return
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_settings_round_trip_through_their_labels() {
        for track_type in [
            TrackType::Audio,
            TrackType::Midi,
            TrackType::Instrument,
            TrackType::Bus,
            TrackType::Return,
        ] {
            assert_eq!(parse_track_type(track_type_label(track_type)), track_type);
        }
        for mode in [
            InputMonitorMode::Off,
            InputMonitorMode::WhenRecordArmed,
            InputMonitorMode::Always,
        ] {
            assert_eq!(parse_monitor(monitor_label(mode)), mode);
        }
        let color = gpui::rgb(0xff8800);
        assert_eq!(color_hex(color), "#ff8800");
        assert_eq!(parse_color("#ff8800"), Some(color));
        assert_eq!(parse_color("nonsense"), None);
    }

    /// A slot saved and read back is the same insert: plug-in, role, switches,
    /// outputs and state.
    #[test]
    fn a_slot_survives_the_preset() {
        let preset = PresetSlot {
            plugin_id: "builtin:equz8".to_string(),
            plugin_name: "EQ-Z8".to_string(),
            display_name: "Vocal EQ".to_string(),
            enabled: true,
            bypassed: true,
            is_instrument: Some(false),
            output_channels: vec![3, 4],
            multiout_collapsed: true,
            state: b"{\"gain\":3}".to_vec(),
            ..Default::default()
        };
        let insert = slot_from_preset(&preset);
        assert_eq!(insert.plugin_id.as_deref(), Some("builtin:equz8"));
        assert_eq!(insert.display_name, "Vocal EQ");
        assert!(insert.bypassed && insert.enabled);
        assert_eq!(insert.plugin_is_instrument, Some(false));
        assert_eq!(insert.enabled_audio_output_channels, vec![3, 4]);
        assert_eq!(
            insert.vst3_state.as_deref().map(Vec::as_slice),
            Some(&b"{\"gain\":3}"[..])
        );
        let back = slot_to_preset(&insert, None);
        assert_eq!(back.plugin_id, preset.plugin_id);
        assert_eq!(back.state, preset.state);
        assert_eq!(back.output_channels, preset.output_channels);
    }

    #[test]
    fn preset_names_become_safe_file_names() {
        assert_eq!(file_stem("Lead: Vocal/Chain?"), "Lead  Vocal Chain");
        assert_eq!(file_stem("   "), "Preset");
        assert_eq!(
            with_extension(PathBuf::from("C:/x/Vocal")),
            PathBuf::from("C:/x/Vocal.pst")
        );
    }
}

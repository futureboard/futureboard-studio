use gpui::{App, Bounds, Context, Window};

use std::path::PathBuf;
use std::sync::Arc;

use crate::components::add_track_dialog::{
    open_add_track_window, AddTrackDialogState, AddTrackKind, AudioFormat, InstrumentMode,
};
use crate::components::combo_box::dedupe_preserve_order;
use crate::components::keymap_window::{open_keymap_window, KeymapChangedCb};
use crate::components::midi_editor_window::{midi_editor_debug, open_midi_editor_window};
use crate::components::settings_dialog::{
    open_settings_window, AudioDeviceListsProvider, OnSettingUpdate, SettingsAudioDeviceLists,
};
use crate::components::timeline::timeline_state::{
    self, ClipType, CreateTrackOptions, InsertPluginFormat, TrackAudioFormat,
    TrackMidiInputRouting, TrackOutputRouting, TrackType,
};
use crate::components::{external_mixer_debug, open_mixer_window};
use crate::window_position::resolve_owner_bounds_with_preferred;
use SpherePluginHost::{PluginFormat as RegistryPluginFormat, PluginKind};

use super::helpers::{cleaned_track_name, numbered_name_stem};
use super::{ContextMenuTarget, OpenPopover, StudioLayout};

fn add_track_instrument_plugins_from_catalog(
    catalog: &super::plugin_ops::PluginCatalogState,
) -> Vec<SpherePluginHost::RegistryPlugin> {
    catalog
        .available
        .as_ref()
        .map(|plugins| {
            plugins
                .iter()
                .filter(|plugin| {
                    plugin.kind == PluginKind::Instrument
                        && plugin.supports_insert()
                        && plugin.scan_status.is_usable()
                        // Audio Unit hosting currently covers effect inserts.
                        // Listing AU instruments here would offer a track the
                        // instrument path cannot finish creating yet.
                        && plugin.format != RegistryPluginFormat::Au
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

fn dialog_audio_format(format: AudioFormat) -> TrackAudioFormat {
    match format {
        AudioFormat::Mono => TrackAudioFormat::Mono,
        AudioFormat::Stereo => TrackAudioFormat::Stereo,
    }
}

/// Logical Input Audio Connections the Add Track dialog offers, as
/// `(menu label, connection id)`.
///
/// Built from the project registry alone, so the dialog cannot expose a device
/// or create a bus: a disconnected connection is still listed (marked
/// unavailable) rather than dropped, because hiding it would silently change
/// what a re-opened dialog offers.
fn dialog_audio_input_choices(
    state: &crate::components::timeline::timeline_state::TimelineState,
) -> Vec<crate::components::add_track_dialog::AddTrackInputChoice> {
    use crate::components::add_track_dialog::AddTrackInputChoice;

    // Built once for the widest track format; the dialog re-filters by channel
    // count when the user switches Mono/Stereo.
    crate::input_routing::track_input_options(&state.audio_connections, 2, None)
        .into_iter()
        .filter_map(|choice| {
            let label = choice.label();
            let connection_id = choice.connection_id()?.clone();
            let channels = state
                .audio_connections
                .get(&connection_id)?
                .channel_layout
                .channel_count();
            Some(AddTrackInputChoice {
                label,
                connection_id,
                channels,
            })
        })
        .collect()
}

fn dialog_audio_output_routing(
    label: &str,
    bus_targets: &[(String, String)],
) -> TrackOutputRouting {
    match label {
        "None" => TrackOutputRouting::None,
        "Main" | "Stereo Master" | "Mono Master" => TrackOutputRouting::Main,
        other => {
            if let Some(name) = other.strip_prefix("Bus - ") {
                if let Some((id, _)) = bus_targets.iter().find(|(_, n)| n == name) {
                    return TrackOutputRouting::Bus { bus_id: id.clone() };
                }
            }
            if let Some((id, _)) = bus_targets
                .iter()
                .find(|(id, name)| id == other || name == other)
            {
                return TrackOutputRouting::Bus { bus_id: id.clone() };
            }
            TrackOutputRouting::Main
        }
    }
}

fn project_bus_output_targets(
    state: &crate::components::timeline::timeline_state::TimelineState,
) -> Vec<(String, String)> {
    use crate::components::timeline::timeline_state::is_project_routing_track;
    state
        .tracks
        .iter()
        .filter(|track| is_project_routing_track(track))
        .map(|track| (track.id.clone(), track.name.clone()))
        .collect()
}

fn dialog_midi_input_routing(label: &str) -> TrackMidiInputRouting {
    match label {
        "All MIDI Inputs" => TrackMidiInputRouting::AllInputs,
        "None" => TrackMidiInputRouting::None,
        device => TrackMidiInputRouting::MidiDevice {
            device_id: device.to_string(),
        },
    }
}

/// Real, enabled MIDI input device names for the Add Track dialog's
/// Instrument/MIDI routing selects — the same cached registry + Preferences
/// enable-state resolution Settings and the Inspector routing combo use
/// (`device_registry` + `resolve_midi_devices`), not a mocked list.
fn add_track_midi_input_devices(schema: &crate::settings::SettingsSchema) -> Vec<String> {
    let saved = schema.hardware.midi.devices.clone();
    let detected = crate::device_registry::cached_midi_devices();
    let resolved = sphere_midi_service::resolve_midi_devices(&saved, &detected);
    resolved
        .iter()
        .filter(|d| {
            d.enabled
                && matches!(
                    d.direction,
                    crate::settings::MidiDeviceDirection::Input
                        | crate::settings::MidiDeviceDirection::InputOutput
                )
        })
        .map(|d| d.name.clone())
        .collect()
}

/// Studio-window / app-integration hooks — this workspace's own window handle,
/// the last known window bounds (used to position child windows without
/// re-entering the root `WindowHandle`), and the app-level "re-open Welcome"
/// hook. `StudioLayout` decomposition slice (all Option → derived `Default`).
#[derive(Default)]
pub(crate) struct StudioWindowHooks {
    /// Handle to this workspace's own window; `None` until wired by the app layer.
    pub self_window: Option<gpui::WindowHandle<StudioLayout>>,
    /// Last known main workspace bounds, updated during render.
    pub cached_bounds: Option<Bounds<gpui::Pixels>>,
    /// App-level hook that re-opens the Welcome window (invoked by close_project).
    pub on_request_welcome: Option<Arc<dyn Fn(&mut gpui::App) + 'static>>,
    /// App-level hook for in-studio project open/replace — keeps the root studio
    /// window alive and swaps the session in place.
    pub on_request_project_load: Option<
        Arc<dyn Fn(PathBuf, super::project_ops::ProjectOpenOptions, &mut gpui::App) + 'static>,
    >,
    /// App-level hook for visible session shutdown (close project).
    pub on_request_session_shutdown: Option<
        Arc<
            dyn Fn(
                    crate::session_shutdown::SessionShutdownReason,
                    Option<Bounds<gpui::Pixels>>,
                    gpui::WindowHandle<StudioLayout>,
                    &mut gpui::App,
                ) + 'static,
        >,
    >,
}

/// Floating MIDI editor window state — the single editor window handle (switches
/// clip on open) and the owner bounds parked for a deferred open. `StudioLayout`
/// decomposition slice (both Option → derived `Default`).
#[derive(Default)]
pub(crate) struct MidiEditorWindowState {
    /// Global floating MIDI editor window; `None` when closed.
    pub window: Option<gpui::WindowHandle<crate::components::midi_editor_window::MidiEditorWindow>>,
    /// Owner bounds for a deferred editor open.
    pub pending_open: Option<Bounds<gpui::Pixels>>,
}

/// Detached / external window handles owned by the studio (settings, mixer,
/// add-track, plugin-manager, export-arrangement) plus the bounds parked for a
/// deferred external-mixer open. `StudioLayout` decomposition slice (all Option
/// → derived `Default`).
#[derive(Default)]
pub(crate) struct ExternalWindows {
    /// External Settings window; `None` when closed.
    pub settings: Option<gpui::WindowHandle<crate::components::settings_dialog::SettingsWindow>>,
    /// Detached mixer window (multi-monitor layouts).
    pub mixer: Option<gpui::WindowHandle<crate::components::MixerWindow>>,
    /// Bounds for an external-mixer open deferred to after the current update.
    pub pending_mixer_open: Option<Bounds<gpui::Pixels>>,
    /// Add Track dialog window.
    pub add_track: Option<gpui::WindowHandle<crate::components::add_track_dialog::AddTrackWindow>>,
    /// Plugin Manager window.
    pub plugin_manager:
        Option<gpui::WindowHandle<crate::components::plugin_manager::PluginManagerWindow>>,
    /// Export Arrangement window.
    pub export_arrangement: Option<gpui::WindowHandle<crate::export::ExportArrangementWindow>>,
    /// Export MIDI File options dialog.
    pub export_midi:
        Option<gpui::WindowHandle<crate::components::midi_export_dialog::MidiExportDialog>>,
    /// Import MIDI File options dialog, opened by a drop that carries markers /
    /// controller lanes / SysEx.
    pub import_midi:
        Option<gpui::WindowHandle<crate::components::midi_import_dialog::MidiImportDialog>>,
    /// Indeterminate progress while a plug-in loads in the host process.
    pub plugin_loading:
        Option<gpui::WindowHandle<crate::components::progress_dialog::ProgressDialogWindow>>,
    /// Stem Extractor (MDX-NET) dialog window.
    pub stem_extractor:
        Option<gpui::WindowHandle<crate::components::stem_extractor_dialog::StemExtractorWindow>>,
    /// Keymap / keyboard shortcuts editor window.
    pub keymap: Option<gpui::WindowHandle<crate::components::keymap_window::KeymapWindow>>,
    /// About Futureboard Studio window.
    pub about: Option<gpui::WindowHandle<crate::components::about_window::AboutWindow>>,
    /// Audio Jam — the room, its participants, and their streams.
    pub jam: Option<gpui::WindowHandle<crate::components::jam_window::JamWindow>>,
    /// Project Settings — tempo, meter, and sample rate for the open project.
    pub project_settings: Option<
        gpui::WindowHandle<crate::components::project_settings_window::ProjectSettingsWindow>,
    >,
    /// Built-in Soundfont Player MDI window.
    pub soundfont_player: Option<
        gpui::WindowHandle<crate::components::soundfont_player_window::SoundfontPlayerWindow>,
    >,
    /// Extensions registry browser window.
    pub extensions:
        Option<gpui::WindowHandle<crate::components::extensions_window::ExtensionsWindow>>,
    /// Video Player — reference/preview monitor for the Video track.
    pub video_player:
        Option<gpui::WindowHandle<crate::components::video_player_window::VideoPlayerWindow>>,
    /// Audio Connections editor — the logical bus registry.
    pub audio_connections: Option<
        gpui::WindowHandle<crate::components::audio_connections_window::AudioConnectionsWindow>,
    >,
    /// Audio Routing Matrix (track send/return matrix) window.
    pub routing_matrix:
        Option<gpui::WindowHandle<crate::components::routing_matrix_window::RoutingMatrixWindow>>,
    pub chord_display:
        Option<gpui::WindowHandle<crate::components::song_text_panel::SongTextWindow>>,
    pub lyric_display:
        Option<gpui::WindowHandle<crate::components::song_text_panel::SongTextWindow>>,
    pub lyric_editor:
        Option<gpui::WindowHandle<crate::components::song_text_panel::SongTextWindow>>,
    /// Big Clock — the playhead in bars, large enough to read from the room.
    pub big_clock: Option<gpui::WindowHandle<crate::components::clock_window::ClockWindow>>,
    /// Timecode — the same playhead as SMPTE, for a session cut to picture.
    pub timecode: Option<gpui::WindowHandle<crate::components::clock_window::ClockWindow>>,
    /// Performance Monitor — engine latency and PDC, cores, memory and drives.
    pub performance:
        Option<gpui::WindowHandle<crate::components::performance_window::PerformanceWindow>>,
}

impl StudioLayout {
    /// Build the view-model the Audio Connections window renders from.
    pub(crate) fn build_audio_connections_snapshot(
        &self,
        cx: &gpui::App,
    ) -> crate::components::audio_connections_window::AudioConnectionsSnapshot {
        let timeline = self.timeline.read(cx);
        let registry = timeline.state.audio_connections.clone();

        // What references each connection, so a removal can describe its
        // consequences before it happens. Track inputs, the Master output, and
        // the Monitor override all land in the same map: a bus used only by
        // Master must not look unused.
        let mut references: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for track in &timeline.state.tracks {
            if let Some(id) = track.routing.audio_input_connection_id.as_ref() {
                references
                    .entry(id.as_str().to_string())
                    .or_default()
                    .push(track.name.clone());
            }
        }
        if let Some(id) = timeline.state.master_output_connection_id.as_ref() {
            references
                .entry(id.as_str().to_string())
                .or_default()
                .push("Master".to_string());
        }
        if let Some(id) = timeline.state.monitor_output_connection_id.as_ref() {
            references
                .entry(id.as_str().to_string())
                .or_default()
                .push("Monitor".to_string());
        }

        // Device identity stays separate from bus identity: these two strings
        // are context for the header, never a source for bus names.
        let (input_device, output_device) = {
            let settings = self.settings.read(cx);
            (
                settings.current.hardware.audio.device_in.trim().to_string(),
                settings
                    .current
                    .hardware
                    .audio
                    .device_out
                    .trim()
                    .to_string(),
            )
        };

        crate::components::audio_connections_window::AudioConnectionsSnapshot {
            registry,
            // One consistent view of the hardware per refresh, so the table
            // and its dropdowns cannot disagree about what exists.
            ports: crate::audio_connections::current_available_ports(),
            input_device,
            output_device,
            has_project: true,
            references,
        }
    }

    /// Apply one panel edit to the project registry.
    ///
    /// Every mutation goes through the registry's structured API, which reports
    /// whether runtime routing must be recompiled — a rename does not, a
    /// mapping change does.
    pub(crate) fn apply_audio_connection_edit(
        &mut self,
        edit: &crate::components::audio_connections_window::ConnectionEdit,
        cx: &mut Context<Self>,
    ) {
        use crate::components::audio_connections_window::ConnectionEdit;

        // Requests are not mutations: they ask the layout to decide whether a
        // confirmation is needed, because only the layout knows what
        // references a bus.
        match edit {
            ConnectionEdit::OpenAudioDeviceSetup => {
                let owner = self.audio_connections_window_bounds(cx);
                self.open_settings_dialog(owner, cx);
                return;
            }
            ConnectionEdit::RequestRemove { id } => {
                self.confirm_audio_connection_removal(id.clone(), cx);
                return;
            }
            ConnectionEdit::RequestResetDefaults { direction } => {
                self.confirm_audio_connection_reset(*direction, cx);
                return;
            }
            ConnectionEdit::RequestApplyPreset { preset } => {
                self.confirm_audio_connection_preset(*preset, cx);
                return;
            }
            _ => {}
        }

        let ports = crate::audio_connections::current_available_ports();
        let (device_id, output_device_id) = {
            let settings = self.settings.read(cx);
            (
                settings.current.hardware.audio.device_in.trim().to_string(),
                settings
                    .current
                    .hardware
                    .audio
                    .device_out
                    .trim()
                    .to_string(),
            )
        };

        let mutation = self.timeline.update(cx, |timeline, cx| {
            let registry = &mut timeline.state.audio_connections;
            let mutation = match edit {
                ConnectionEdit::Add { direction, layout } => {
                    registry.add_connection(*direction, *layout, &ports).1
                }
                ConnectionEdit::SetEnabled { id, enabled } => {
                    registry.update_enabled(id, *enabled, &ports)
                }
                // A rename never reports needs_routing_rebuild, so this path
                // marks the project dirty without republishing routing.
                ConnectionEdit::Rename { id, name } => registry.update_name(id, name),
                ConnectionEdit::SetLayout { id, layout } => {
                    registry.update_layout(id, *layout, &ports)
                }
                ConnectionEdit::SetDevice { id, device_id } => {
                    registry.update_device(id, device_id.as_deref(), &ports)
                }
                ConnectionEdit::SetPort {
                    id,
                    logical_channel,
                    port,
                } => registry.update_port_binding(id, *logical_channel, port.clone(), &ports),
                ConnectionEdit::Duplicate { id } => registry.duplicate_connection(id, &ports).1,
                ConnectionEdit::Remove { id } => {
                    let affected: Vec<String> = timeline
                        .state
                        .tracks
                        .iter()
                        .filter(|track| {
                            track.routing.audio_input_connection_id.as_ref() == Some(id)
                        })
                        .map(|track| track.id.clone())
                        .collect();
                    let mutation = timeline
                        .state
                        .audio_connections
                        .remove_connection(id, &affected, &ports);
                    // Affected tracks become No Input — never re-pointed at
                    // some other bus. MIDI routing is untouched.
                    timeline.state.unassign_audio_connection(id);
                    // Same rule for the buses: Master loses its output and
                    // Monitor returns to Follow Master Output. Neither picks a
                    // replacement automatically.
                    timeline.state.unassign_output_connection(id);
                    mutation
                }
                ConnectionEdit::ResetDefaults { direction } => {
                    let removed: Vec<crate::audio_connections::AudioConnectionId> = timeline
                        .state
                        .audio_connections
                        .by_direction(*direction)
                        .into_iter()
                        .map(|connection| connection.id.clone())
                        .collect();
                    let affected: Vec<String> = timeline
                        .state
                        .tracks
                        .iter()
                        .filter(|track| {
                            track
                                .routing
                                .audio_input_connection_id
                                .as_ref()
                                .is_some_and(|id| removed.contains(id))
                        })
                        .map(|track| track.id.clone())
                        .collect();
                    let mutation = timeline
                        .state
                        .audio_connections
                        .reset_defaults_for(*direction, &ports, &device_id, &affected);
                    for id in &removed {
                        timeline.state.unassign_audio_connection(id);
                        timeline.state.unassign_output_connection(id);
                    }
                    mutation
                }
                ConnectionEdit::ApplyPreset { preset } => {
                    // A preset replaces both directions, so every bus in the
                    // project is a removal and every referencing track is
                    // affected — the same unassign rules as Remove, applied
                    // wholesale rather than to one id.
                    let removed: Vec<crate::audio_connections::AudioConnectionId> = timeline
                        .state
                        .audio_connections
                        .all()
                        .iter()
                        .map(|connection| connection.id.clone())
                        .collect();
                    let affected: Vec<String> = timeline
                        .state
                        .tracks
                        .iter()
                        .filter(|track| {
                            track
                                .routing
                                .audio_input_connection_id
                                .as_ref()
                                .is_some_and(|id| removed.contains(id))
                        })
                        .map(|track| track.id.clone())
                        .collect();
                    let mutation = timeline.state.audio_connections.apply_preset(
                        *preset,
                        &ports,
                        &device_id,
                        &output_device_id,
                        &affected,
                    );
                    for id in &removed {
                        timeline.state.unassign_audio_connection(id);
                        timeline.state.unassign_output_connection(id);
                    }
                    mutation
                }
                ConnectionEdit::AutoMap { direction } => {
                    let selected = match direction {
                        crate::audio_connections::AudioConnectionDirection::Input => &device_id,
                        crate::audio_connections::AudioConnectionDirection::Output => {
                            &output_device_id
                        }
                    };
                    timeline
                        .state
                        .audio_connections
                        .auto_map(*direction, &ports, selected)
                }
                // Handled above, before this closure runs.
                ConnectionEdit::RequestRemove { .. }
                | ConnectionEdit::RequestResetDefaults { .. }
                | ConnectionEdit::RequestApplyPreset { .. }
                | ConnectionEdit::OpenAudioDeviceSetup => {
                    crate::audio_connections::ConnectionMutation::default()
                }
            };
            // A rename changes only the chips; a removal may also have cleared
            // a Master/Monitor assignment. Either way the labels are re-derived
            // from the registry rather than cached at assignment time.
            timeline.state.refresh_output_labels();
            cx.notify();
            mutation
        });

        if mutation.needs_routing_rebuild {
            self.publish_audio_connection_routing(cx);
        }
        if mutation.did_change() {
            self.mark_dirty_view_only();
        }
        let warnings = mutation.warnings.clone();
        self.refresh_audio_connections_window(cx);
        if let Some(handle) = self.external_windows.audio_connections.clone() {
            let _ = handle.update(cx, |window, _w, cx| {
                window.set_warnings(warnings, cx);
            });
        }
    }

    /// Screen bounds of the Audio Connections window, so a confirmation opens
    /// over it rather than over the main project window.
    fn audio_connections_window_bounds(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<Bounds<gpui::Pixels>> {
        let handle = self.external_windows.audio_connections.clone()?;
        handle.update(cx, |_window, w, _cx| w.bounds()).ok()
    }

    /// Confirm removing a bus, then apply it.
    ///
    /// Only a *referenced* bus asks: removing an unused one is trivially
    /// reversible and a dialog there is noise. The question is a normal
    /// message-box window, so the rest of Studio keeps working while it is up.
    fn confirm_audio_connection_removal(
        &mut self,
        id: crate::audio_connections::AudioConnectionId,
        cx: &mut Context<Self>,
    ) {
        use crate::components::audio_connections_panel::removal_needs_confirmation;
        use crate::components::audio_connections_window::ConnectionEdit;
        use crate::components::message_box_dialog::{MessageBoxKind, MessageBoxOptions};

        let (name, affected, uses_master, uses_monitor) = {
            let timeline = self.timeline.read(cx);
            let name = timeline
                .state
                .audio_connections
                .name_of(&id)
                .unwrap_or_default()
                .to_string();
            let affected: Vec<String> = timeline
                .state
                .tracks
                .iter()
                .filter(|track| track.routing.audio_input_connection_id.as_ref() == Some(&id))
                .map(|track| track.name.clone())
                .collect();
            (
                name,
                affected,
                timeline.state.master_output_connection_id.as_ref() == Some(&id),
                timeline.state.monitor_output_connection_id.as_ref() == Some(&id),
            )
        };

        // A bus used only by Master or Monitor still needs the question — the
        // track list alone would call it unused.
        if !removal_needs_confirmation(&affected) && !uses_master && !uses_monitor {
            self.apply_audio_connection_edit(&ConnectionEdit::Remove { id }, cx);
            return;
        }

        let mut lines = Vec::new();
        if !affected.is_empty() {
            lines.push(format!(
                "{} track(s) use this connection and will be set to No Input: {}.",
                affected.len(),
                affected.join(", ")
            ));
        }
        if uses_master {
            lines.push(format!(
                "\"{name}\" is used by Master. Master will have no output."
            ));
        }
        if uses_monitor {
            lines.push(format!(
                "\"{name}\" is used by Monitor. Monitor will follow the Master output."
            ));
        }
        let detail = lines.join(" ");
        let options = MessageBoxOptions {
            kind: MessageBoxKind::Warning,
            title: "Remove Audio Connection".to_string(),
            message: format!("Remove \"{name}\"?"),
            detail: Some(detail),
            buttons: vec!["Remove".to_string(), "Cancel".to_string()],
            default_id: 1,
            cancel_id: Some(1),
        };
        self.confirm_audio_connection_edit(options, ConnectionEdit::Remove { id }, cx);
    }

    /// Confirm resetting a direction to the application defaults.
    ///
    /// Always asks: this discards buses the user created by hand.
    fn confirm_audio_connection_reset(
        &mut self,
        direction: crate::audio_connections::AudioConnectionDirection,
        cx: &mut Context<Self>,
    ) {
        use crate::components::audio_connections_window::ConnectionEdit;
        use crate::components::message_box_dialog::{MessageBoxKind, MessageBoxOptions};

        let affected = {
            let timeline = self.timeline.read(cx);
            let removed: Vec<_> = timeline
                .state
                .audio_connections
                .by_direction(direction)
                .into_iter()
                .map(|connection| connection.id.clone())
                .collect();
            timeline
                .state
                .tracks
                .iter()
                .filter(|track| {
                    track
                        .routing
                        .audio_input_connection_id
                        .as_ref()
                        .is_some_and(|id| removed.contains(id))
                })
                .count()
        };
        let label = match direction {
            crate::audio_connections::AudioConnectionDirection::Input => "input",
            crate::audio_connections::AudioConnectionDirection::Output => "output",
        };
        let options = MessageBoxOptions {
            kind: MessageBoxKind::Warning,
            title: "Reset Audio Connections".to_string(),
            message: format!("Reset {label} connections to defaults?"),
            detail: Some(format!(
                "Existing {label} buses are replaced. {affected} track reference(s) may be \
                 unassigned."
            )),
            buttons: vec!["Reset".to_string(), "Cancel".to_string()],
            default_id: 1,
            cancel_id: Some(1),
        };
        self.confirm_audio_connection_edit(
            options,
            ConnectionEdit::ResetDefaults { direction },
            cx,
        );
    }

    /// Confirm applying a preset, then apply it.
    ///
    /// Always asks, and for a bigger reason than Reset Defaults does: a preset
    /// replaces the buses in *both* directions, including ones the user built
    /// by hand on the tab they are not looking at.
    fn confirm_audio_connection_preset(
        &mut self,
        preset: crate::audio_connections::ConnectionPreset,
        cx: &mut Context<Self>,
    ) {
        use crate::components::audio_connections_window::ConnectionEdit;
        use crate::components::message_box_dialog::{MessageBoxKind, MessageBoxOptions};

        let (buses, affected) = {
            let timeline = self.timeline.read(cx);
            let buses = timeline.state.audio_connections.len();
            let affected = timeline
                .state
                .tracks
                .iter()
                .filter(|track| track.routing.audio_input_connection_id.is_some())
                .count();
            (buses, affected)
        };
        let label = preset.label();
        let options = MessageBoxOptions {
            kind: MessageBoxKind::Warning,
            title: "Apply Connection Preset".to_string(),
            message: format!("Apply the {label} preset?"),
            detail: Some(format!(
                concat!(
                    "All {} existing input and output bus(es) are replaced. ",
                    "{} track reference(s) may be unassigned."
                ),
                buses, affected
            )),
            buttons: vec!["Apply".to_string(), "Cancel".to_string()],
            default_id: 1,
            cancel_id: Some(1),
        };
        self.confirm_audio_connection_edit(options, ConnectionEdit::ApplyPreset { preset }, cx);
    }

    /// Ask, and apply `edit` only if the first button was chosen.
    fn confirm_audio_connection_edit(
        &mut self,
        options: crate::components::message_box_dialog::MessageBoxOptions,
        edit: crate::components::audio_connections_window::ConnectionEdit,
        cx: &mut Context<Self>,
    ) {
        use crate::components::message_box_dialog::{
            open_message_box_window, MessageBoxResponseCb, MessageBoxResult,
        };

        let owner = self.audio_connections_window_bounds(cx);
        let studio = cx.entity().clone();
        let on_response: MessageBoxResponseCb =
            std::sync::Arc::new(move |result: MessageBoxResult, _window, cx| {
                if result.response != 0 {
                    return;
                }
                let edit = edit.clone();
                StudioLayout::defer_update(&studio, cx, move |this, cx| {
                    this.apply_audio_connection_edit(&edit, cx);
                });
            });
        if let Err(error) = open_message_box_window(owner, options, on_response, cx) {
            self.audio_bridge.last_error =
                Some(format!("Audio Connections confirmation failed: {error}"));
        }
    }

    /// Recompile and publish the runtime routing snapshot.
    ///
    /// Compilation happens here, on the control thread — never in the audio
    /// callback, which only ever reads the published snapshot.
    pub(crate) fn publish_audio_connection_routing(&mut self, cx: &mut Context<Self>) {
        use crate::audio_routing_compile::{compile_routing, RoutingCompileRequest};

        // Publish the track list before the port inventory is read: a track
        // added or renamed in this same edit has to be selectable in the Input
        // menu the compile below validates against, or its first appearance
        // would be one recompile late.
        {
            let timeline = self.timeline.read(cx);
            crate::loopback_ports::set_sources(
                timeline
                    .state
                    .tracks
                    .iter()
                    .filter(|track| {
                        crate::loopback_ports::is_loopback_source_kind(track.track_type)
                    })
                    .map(|track| crate::loopback_ports::LoopbackSource {
                        track_id: track.id.clone(),
                        display_name: track.name.clone(),
                    })
                    .collect(),
            );
        }
        let ports = crate::audio_connections::current_available_ports();
        let request = {
            let timeline = self.timeline.read(cx);
            RoutingCompileRequest {
                track_inputs: timeline
                    .state
                    .tracks
                    .iter()
                    .map(|track| track.routing.audio_input_connection_id.clone())
                    .collect::<Vec<_>>(),
                master_output: timeline.state.master_output_connection_id.clone(),
                // `None` here is Follow Master Output; the compiler resolves it.
                monitor_output_override: timeline.state.monitor_output_connection_id.clone(),
                control_room_active: timeline.state.monitor.control_room_enabled,
            }
        };
        let registry = self.timeline.read(cx).state.audio_connections.clone();
        let publisher = crate::audio_routing_compile::global_routing_publisher();
        let generation = publisher.next_generation();
        let result = compile_routing(&registry, &ports, &request, generation);
        publisher.publish(result.snapshot);
        // What arrives from the jam is part of the same decision. A remote
        // stream is only audible through an input connection, so the registry is
        // what says which performers this project is listening to — and a stream
        // nobody routed should not be sent at all.
        crate::jam::ingress::sync(&registry);
        // And what leaves: an output bus bound to the jam's send ports is a
        // publish, so it is applied from the same registry edit that made it.
        crate::jam::egress::sync(&registry);
        // Hardware ownership is part of the same compile: publishing routing
        // without it would leave the engine writing the previous destination.
        self.push_output_routing_to_engine(cx);
    }

    /// Report routing warnings on the single project surface.
    ///
    /// Takes the registry's prose, classifies it, and aggregates: one clause per
    /// condition, no matter how many connections or tracks it affects. The full
    /// list stays in `audio_bridge.routing_warnings.details` for diagnostics.
    pub(crate) fn push_routing_warnings(&mut self, warnings: Vec<String>, cx: &mut Context<Self>) {
        use crate::layout::routing_warnings::RoutingWarning;

        let classified: Vec<RoutingWarning> = warnings
            .into_iter()
            .map(RoutingWarning::from_registry_text)
            .collect();
        self.report_routing_warnings(classified, cx);
    }

    /// Report already-classified warnings. `Vec::new()` clears the surface, so a
    /// condition that has been resolved stops being advertised.
    pub(crate) fn report_routing_warnings(
        &mut self,
        warnings: Vec<crate::layout::routing_warnings::RoutingWarning>,
        cx: &mut Context<Self>,
    ) {
        let _ = self.audio_bridge.routing_warnings.report(warnings);
        cx.notify();
    }

    /// Re-read the hardware inventory and revalidate every logical connection
    /// against it, then publish **one** routing snapshot.
    ///
    /// Called after any change that can move the device inventory (backend or
    /// device switch, reopen, disconnect/reconnect). Stable ids and port
    /// bindings are preserved throughout: a missing device turns a connection
    /// unavailable and reconnecting the same hardware restores it exactly.
    /// Nothing is ever remapped onto a different device's channel.
    pub(crate) fn refresh_audio_device_inventory(&mut self, cx: &mut Context<Self>) {
        if let Some(engine) = self.audio_bridge.engine.as_ref() {
            crate::device_registry::scan_audio_for_engine(engine);
        } else {
            crate::device_registry::scan_audio();
        }
        let ports = crate::audio_connections::current_available_ports();

        let mutation = self.timeline.update(cx, |timeline, cx| {
            let mutation = timeline.state.audio_connections.validate_all(&ports);
            // Master/Monitor assignments are untouched — only their labels move
            // between available and unavailable.
            timeline.state.refresh_output_labels();
            cx.notify();
            mutation
        });

        // One compile for the whole inventory change, never one per connection.
        self.publish_audio_connection_routing(cx);
        self.refresh_audio_connections_window(cx);
        if !mutation.warnings.is_empty() {
            self.push_routing_warnings(mutation.warnings, cx);
        }
    }

    /// Push fresh project data into the open window, if any.
    pub(crate) fn refresh_audio_connections_window(&mut self, cx: &mut Context<Self>) {
        let Some(handle) = self.external_windows.audio_connections.clone() else {
            return;
        };
        let snapshot = self.build_audio_connections_snapshot(cx);
        let _ = handle.update(cx, |window, _w, cx| {
            window.sync(snapshot, cx);
        });
    }

    pub(super) fn open_audio_connections_window(
        &mut self,
        owner_bounds: Option<Bounds<gpui::Pixels>>,
        cx: &mut Context<Self>,
    ) {
        use crate::components::audio_connections_window::{reopen_action, ReopenAction};

        // Exactly one Audio Connections window: focus the live one, replace a
        // stale handle, never open a second instance.
        let activated = self
            .external_windows
            .audio_connections
            .clone()
            .map(|handle| {
                handle
                    .update(cx, |_window, w, _cx| w.activate_window())
                    .is_ok()
            });
        match reopen_action(activated) {
            ReopenAction::FocusExisting => return,
            ReopenAction::OpenNew => self.external_windows.audio_connections = None,
        }

        let snapshot = self.build_audio_connections_snapshot(cx);
        let studio = cx.entity().clone();
        let on_edit: crate::components::audio_connections_window::ConnectionEditCb = {
            let studio = studio.clone();
            std::sync::Arc::new(move |edit, _w, cx| {
                let edit = edit.clone();
                StudioLayout::defer_update(&studio, cx, move |this, cx| {
                    this.apply_audio_connection_edit(&edit, cx);
                });
            })
        };
        let on_close: std::sync::Arc<dyn Fn(&mut gpui::Window, &mut gpui::App) + 'static> = {
            let studio = studio.clone();
            std::sync::Arc::new(move |_w, cx| {
                StudioLayout::defer_update(&studio, cx, |this, _cx| {
                    this.external_windows.audio_connections = None;
                });
            })
        };

        match crate::components::audio_connections_window::open_audio_connections_window(
            owner_bounds,
            snapshot,
            on_edit,
            on_close,
            cx,
        ) {
            Ok(handle) => {
                self.external_windows.audio_connections = Some(handle);
            }
            Err(error) => {
                self.audio_bridge.last_error =
                    Some(format!("Audio Connections window failed to open: {error}"));
            }
        }
    }

    /// Open a clock window, or bring the one already open to the front.
    ///
    /// Two windows rather than one with a switch: the two readings are watched
    /// by different people at the same time, often on different monitors, and a
    /// clock that has to be re-pointed is a clock somebody has to look away
    /// from. They share one view — see [`crate::components::clock_window`] —
    /// so the readings can never disagree about where the playhead is.
    pub(crate) fn open_clock_window(
        &mut self,
        kind: crate::components::clock_window::ClockKind,
        owner_bounds: Option<Bounds<gpui::Pixels>>,
        cx: &mut Context<Self>,
    ) {
        use crate::components::clock_window::ClockKind;

        let slot = match kind {
            ClockKind::BigClock => &mut self.external_windows.big_clock,
            ClockKind::Timecode => &mut self.external_windows.timecode,
        };
        if let Some(handle) = slot.clone() {
            if handle
                .update(cx, |_view, window, _cx| window.activate_window())
                .is_ok()
            {
                return;
            }
            *slot = None;
        }

        let owner = cx.entity().clone();
        let on_close: Arc<dyn Fn(ClockKind, &mut App) + Send + Sync> =
            Arc::new(move |closed, app| {
                let _ = owner.update(app, |layout, cx| {
                    match closed {
                        ClockKind::BigClock => layout.external_windows.big_clock = None,
                        ClockKind::Timecode => layout.external_windows.timecode = None,
                    }
                    cx.notify();
                });
            });

        match crate::components::clock_window::open_clock_window(
            kind,
            owner_bounds,
            self.timeline.clone(),
            on_close,
            cx,
        ) {
            Ok(handle) => *slot = Some(handle),
            Err(error) => eprintln!("[clock] failed to open {}: {error}", kind.title()),
        }
        cx.notify();
    }

    pub(crate) fn open_song_text_external_window(
        &mut self,
        kind: crate::components::SongTextPanelKind,
        owner_bounds: Option<Bounds<gpui::Pixels>>,
        cx: &mut Context<Self>,
    ) {
        let slot = match kind {
            crate::components::SongTextPanelKind::ChordDisplay => {
                &mut self.external_windows.chord_display
            }
            crate::components::SongTextPanelKind::LyricDisplay => {
                &mut self.external_windows.lyric_display
            }
            crate::components::SongTextPanelKind::LyricEditor => {
                &mut self.external_windows.lyric_editor
            }
        };
        if let Some(handle) = slot.clone() {
            if handle
                .update(cx, |_view, window, _cx| window.activate_window())
                .is_ok()
            {
                return;
            }
            *slot = None;
        }
        let owner = cx.entity().clone();
        let on_close: Arc<dyn Fn(crate::components::SongTextPanelKind, &mut App) + Send + Sync> =
            Arc::new(move |closed_kind, app| {
                let _ = owner.update(app, |layout, cx| {
                    match closed_kind {
                        crate::components::SongTextPanelKind::ChordDisplay => {
                            layout.external_windows.chord_display = None
                        }
                        crate::components::SongTextPanelKind::LyricDisplay => {
                            layout.external_windows.lyric_display = None
                        }
                        crate::components::SongTextPanelKind::LyricEditor => {
                            layout.external_windows.lyric_editor = None
                        }
                    }
                    cx.notify();
                });
            });
        match crate::components::open_song_text_window(
            owner_bounds,
            self.timeline.clone(),
            kind,
            on_close,
            cx,
        ) {
            Ok(handle) => *slot = Some(handle),
            Err(error) => eprintln!("[song-text] failed to open {}: {error}", kind.title()),
        }
        cx.notify();
    }

    pub(super) fn update_add_track_instrument_plugins(&mut self, cx: &mut Context<Self>) {
        let Some(handle) = self.external_windows.add_track.clone() else {
            return;
        };
        let instrument_plugins = add_track_instrument_plugins_from_catalog(&self.plugin_catalog);
        let _ = handle.update(cx, |add_track, _window, cx| {
            add_track.set_instrument_plugins(instrument_plugins);
            cx.notify();
        });
    }

    pub(super) fn open_add_track_external_window(
        &mut self,
        kind: AddTrackKind,
        owner_bounds: Option<Bounds<gpui::Pixels>>,
        cx: &mut Context<Self>,
    ) {
        let mut track_count = 0;
        let mut has_master_track = false;
        let _ = self.timeline.update(cx, |timeline, _cx| {
            track_count = timeline.state.tracks.len();
            has_master_track = timeline
                .state
                .tracks
                .iter()
                .any(|track| track.track_type == TrackType::Master);
        });

        self.open_add_track_external_window_with_context(
            kind,
            track_count,
            has_master_track,
            owner_bounds,
            cx,
        );
    }

    /// Opens/activates the Add Track external window using precomputed
    /// track-count context (so callers from Timeline events do not need a nested
    /// `timeline.update(...)`).
    ///
    /// Callers that originate from a Timeline `cx.listener` must still defer via
    /// [`StudioLayout::defer_update_in_window`]: this path reads Timeline for bus
    /// output targets and will panic on a nested lease.
    pub(super) fn open_add_track_external_window_with_context(
        &mut self,
        kind: AddTrackKind,
        track_count: usize,
        has_master_track: bool,
        owner_bounds: Option<Bounds<gpui::Pixels>>,
        cx: &mut Context<Self>,
    ) {
        // If window is already open, activate and refresh its context.
        let default_monitor_mode = self
            .settings
            .read(cx)
            .current
            .recording
            .default_monitor_mode
            .add_track_value();
        // Real MIDI input devices scanned fresh on every open so a device
        // plugged/unplugged since the last open (or since Preferences was
        // touched) is reflected immediately — cheap, non-destructive scan
        // (see `device_registry::scan_midi`), never done per paint.
        crate::device_registry::scan_midi();
        let midi_input_devices = add_track_midi_input_devices(&self.settings.read(cx).current);

        if let Some(handle) = self.external_windows.add_track.clone() {
            let audio_output_targets = project_bus_output_targets(&self.timeline.read(cx).state);
            let audio_input_choices = dialog_audio_input_choices(&self.timeline.read(cx).state);
            if handle
                .update(cx, |win, window, cx| {
                    win.set_instrument_plugins(add_track_instrument_plugins_from_catalog(
                        &self.plugin_catalog,
                    ));
                    win.set_midi_input_devices(midi_input_devices.clone());
                    win.set_context(kind, track_count, has_master_track, default_monitor_mode);
                    win.set_audio_output_targets(audio_output_targets);
                    win.set_audio_input_choices(audio_input_choices);
                    window.activate_window();
                    cx.notify();
                })
                .is_ok()
            {
                return;
            }
            self.external_windows.add_track = None;
        }

        self.menu_bar.open_menu_id = None;
        self.menu_bar.submenu_path.clear();
        self.overlay.open_popover = None;
        self.overlay.text_context_menu = None;

        let owner_bounds =
            resolve_owner_bounds_with_preferred(owner_bounds, self.studio_window_bounds(cx), cx);

        if self.plugin_catalog.available.is_none()
            || !matches!(
                self.plugin_catalog.status,
                crate::components::plugin_picker::CatalogStatus::Ready
            )
        {
            self.arm_catalog_load(cx);
        }
        let instrument_plugins = add_track_instrument_plugins_from_catalog(&self.plugin_catalog);

        let layout = cx.entity().clone();
        let language = self.settings.read(cx).current.general.language.clone();
        let audio_output_targets = project_bus_output_targets(&self.timeline.read(cx).state);
        let on_confirm_request: Arc<dyn Fn(AddTrackDialogState, String, &mut gpui::App) + 'static> =
            Arc::new(move |dialog, _name, cx| {
                let Some(track_type) = dialog.selected_kind.native_track_type() else {
                    return;
                };
                let _ = layout.update(cx, |this, cx| {
                    this.mark_dirty();
                    let mut bridge_inserts = Vec::new();
                    let _ = this.timeline.update(cx, |timeline, cx| {
                        let route_selected_to_new_bus = dialog.selected_kind == AddTrackKind::Bus;
                        let selected_for_bus: Vec<String> = if route_selected_to_new_bus {
                            let mut ids = timeline.state.selection.selected_track_ids.clone();
                            if ids.is_empty() {
                                if let Some(primary) =
                                    timeline.state.selection.selected_track_id.clone()
                                {
                                    ids.push(primary);
                                }
                            }
                            ids
                        } else {
                            Vec::new()
                        };
                        let count = dialog.count.clamp(1, 128) as usize;
                        let base_name =
                            cleaned_track_name(&dialog.track_name, dialog.selected_kind);
                        let mut selected_track_id = None;
                        let mut created_ids = Vec::new();
                        for i in 0..count {
                            let name = if count == 1 {
                                base_name.clone()
                            } else {
                                format!(
                                    "{} {}",
                                    numbered_name_stem(&base_name),
                                    dialog.next_number + i
                                )
                            };
                            // Auto color → generated palette color per track.
                            // Custom color → the user's chosen color (applied to
                            // every track created in this batch).
                            let color = if dialog.auto_color {
                                timeline
                                    .state
                                    .track_color_for_index(dialog.base_track_count + i)
                            } else if let Some(custom) = dialog.custom_color {
                                custom
                            } else {
                                timeline.state.track_color_for_index(dialog.color_index + i)
                            };
                            let id = timeline.state.create_track(CreateTrackOptions {
                                track_type,
                                name,
                                color,
                                volume: timeline_state::volume::db_to_norm(0.0),
                                pan: 0.0,
                                armed: dialog.selected_kind == AddTrackKind::Audio
                                    && dialog.arm_track,
                                input_monitor: match dialog.monitor_mode {
                                    "input" => timeline_state::InputMonitorMode::Always,
                                    "auto" => timeline_state::InputMonitorMode::WhenRecordArmed,
                                    _ => timeline_state::InputMonitorMode::Off,
                                },
                            });
                            if dialog.selected_kind == AddTrackKind::Instrument
                                || dialog.selected_kind == AddTrackKind::Midi
                            {
                                let midi_input = dialog_midi_input_routing(&dialog.input_label);
                                timeline.state.set_track_midi_input(&id, midi_input);
                            }
                            if matches!(
                                dialog.selected_kind,
                                AddTrackKind::Audio | AddTrackKind::Instrument | AddTrackKind::Midi
                            ) {
                                let output = dialog_audio_output_routing(
                                    &dialog.output_label,
                                    &dialog.audio_output_targets,
                                );
                                timeline.state.set_track_output_routing(&id, output);
                            }
                            if dialog.selected_kind == AddTrackKind::Audio {
                                let audio_format = dialog_audio_format(dialog.audio_format);
                                timeline.state.set_track_audio_format(&id, audio_format);
                                // Only ever a logical connection id. No bus is
                                // created here, and No Input stays No Input.
                                let connection_id =
                                    dialog.audio_input_connection_for_label(&dialog.input_label);
                                timeline
                                    .state
                                    .set_track_audio_input_connection(&id, connection_id);
                            }
                            if dialog.selected_kind == AddTrackKind::Instrument
                                && dialog.instrument_mode == InstrumentMode::Vsti
                            {
                                if let Some(plugin_id) = dialog.instrument_plugin_id.as_deref() {
                                    let instrument_registry =
                                        add_track_instrument_plugins_from_catalog(
                                            &this.plugin_catalog,
                                        );
                                    if let Some(reg) = instrument_registry.iter().find(|p| {
                                        p.id == plugin_id
                                            || p.class_id.as_deref() == Some(plugin_id)
                                            || p.name.eq_ignore_ascii_case(plugin_id)
                                    }) {
                                        if let Some(slot_id) = timeline.state.add_insert(&id) {
                                            let format = match reg.format {
                                                RegistryPluginFormat::Vst3 => {
                                                    InsertPluginFormat::Vst3
                                                }
                                                RegistryPluginFormat::Vst2 => {
                                                    InsertPluginFormat::Vst2
                                                }
                                                RegistryPluginFormat::Clap => {
                                                    InsertPluginFormat::Clap
                                                }
                                                RegistryPluginFormat::Au => InsertPluginFormat::Au,
                                                RegistryPluginFormat::Lv2 => {
                                                    InsertPluginFormat::Lv2
                                                }
                                                RegistryPluginFormat::Unknown => {
                                                    InsertPluginFormat::Unknown
                                                }
                                            };
                                            let plugin_uid = reg
                                                .class_id
                                                .clone()
                                                .unwrap_or_else(|| reg.id.clone());
                                            timeline.state.set_insert_plugin(
                                                &id,
                                                &slot_id,
                                                plugin_uid,
                                                Some(reg.path.clone()),
                                                format,
                                                Some(reg.vendor.clone())
                                                    .filter(|vendor| !vendor.trim().is_empty()),
                                                reg.name.clone(),
                                            );
                                            timeline
                                                .state
                                                .set_insert_plugin_role(&id, &slot_id, true);
                                            if matches!(
                                                format,
                                                InsertPluginFormat::Vst3
                                                    | InsertPluginFormat::Vst2
                                                    | InsertPluginFormat::Clap
                                            ) {
                                                // Auto-open the editor for a freshly
                                                // added instrument: mark the slot
                                                // pending so the host editor shell
                                                // opens (with its "Loading Plugin"
                                                // overlay) as soon as the runtime
                                                // instance is ready — matching the
                                                // insert picker (apply_picked_insert).
                                                // Skip for batch creation (count > 1)
                                                // to avoid opening many windows at once.
                                                if count == 1
                                                    && super::plugin_bridge_runtime::bridge_enabled(
                                                    )
                                                {
                                                    timeline.state.set_insert_pending_editor_open(
                                                        &id, &slot_id, true,
                                                    );
                                                }
                                                bridge_inserts.push((id.clone(), slot_id));
                                            }
                                        }
                                    }
                                }
                            } else if dialog.selected_kind == AddTrackKind::Instrument
                                && dialog.instrument_mode == InstrumentMode::SoundfontPlayer
                            {
                                // Built-in Soundfont Player is not a hosted plugin — it
                                // never goes through the VST3/CLAP/AU/LV2 bridge or
                                // plugin registry, so it gets a plain track marker
                                // instead of an insert. Inspector shows an Open button
                                // that opens the Soundfont Player MDI window for it.
                                timeline.state.set_track_builtin_soundfont_player(&id, true);
                            } else if dialog.selected_kind == AddTrackKind::Instrument
                                && dialog.instrument_mode == InstrumentMode::SolfegeEngine
                            {
                                let model_path = dialog
                                    .solfege_model_path
                                    .clone()
                                    .filter(|path| !path.trim().is_empty());
                                if let Some(path) = model_path.as_deref() {
                                    match crate::solfege::load_model_info(
                                        std::path::Path::new(path),
                                    ) {
                                        Ok(info) => eprintln!(
                                            "[solfege] loaded model='{}' type={} sample_rate={} validated={}",
                                            info.name, info.model_type, info.sample_rate, info.validated
                                        ),
                                        Err(error) => eprintln!(
                                            "[solfege] model load deferred for '{}': {error}",
                                            path
                                        ),
                                    }
                                }
                                timeline.state.set_track_solfege_engine(
                                    &id,
                                    Some(crate::solfege::SolfegeTrackState::violin(model_path)),
                                );
                            }
                            created_ids.push(id.clone());
                            selected_track_id = Some(id);
                        }
                        // Creating a Bus with tracks already selected routes those
                        // tracks' main outs into the new bus (single-bus create only).
                        if route_selected_to_new_bus && created_ids.len() == 1 {
                            let bus_id = &created_ids[0];
                            for track_id in &selected_for_bus {
                                if track_id == bus_id {
                                    continue;
                                }
                                let Some(track) = timeline.state.find_track(track_id) else {
                                    continue;
                                };
                                if track.track_type.is_routing()
                                    || track.track_type == TrackType::Master
                                {
                                    continue;
                                }
                                timeline.state.set_track_output_routing(
                                    track_id,
                                    TrackOutputRouting::Bus {
                                        bus_id: bus_id.clone(),
                                    },
                                );
                            }
                        }
                        if let Some(id) = selected_track_id {
                            timeline.state.select_track(&id);
                        }
                        crate::components::add_track_dialog::add_track_debug(&format!(
                            "created tracks kind={} count={} ids={:?}",
                            dialog.selected_kind.tab_label(),
                            count,
                            created_ids
                        ));
                        cx.notify();
                    });
                    let mut bridge_loaded = false;
                    for (track_id, slot_id) in bridge_inserts {
                        bridge_loaded |= this.load_bridge_insert_for_slot(&track_id, &slot_id, cx);
                    }
                    if !bridge_loaded {
                        this.audio_bridge.project_dirty = true;
                        this.schedule_audio_project_sync(cx, true, "add_track_dialog");
                    }
                    cx.notify();
                });
            });

        match open_add_track_window(
            owner_bounds,
            kind,
            track_count,
            has_master_track,
            default_monitor_mode,
            language,
            instrument_plugins,
            midi_input_devices,
            audio_output_targets,
            dialog_audio_input_choices(&self.timeline.read(cx).state),
            on_confirm_request,
            cx,
        ) {
            Ok(handle) => self.external_windows.add_track = Some(handle),
            Err(err) => eprintln!("[add-track] failed to open window: {err}"),
        }
    }

    pub(super) fn open_settings_dialog(
        &mut self,
        owner_bounds: Option<Bounds<gpui::Pixels>>,
        cx: &mut Context<Self>,
    ) {
        let open_started = std::time::Instant::now();
        // If window is already open, activate it
        if let Some(handle) = self.external_windows.settings.clone() {
            if handle
                .update(cx, |_settings, window, _cx| window.activate_window())
                .is_ok()
            {
                return;
            }
            self.external_windows.settings = None;
        }

        self.menu_bar.open_menu_id = None;
        self.menu_bar.submenu_path.clear();
        self.overlay.open_popover = None;
        self.project_switcher.is_open = false;
        self.overlay.text_context_menu = None;

        let owner_bounds =
            resolve_owner_bounds_with_preferred(owner_bounds, self.studio_window_bounds(cx), cx);
        let settings = self.settings.clone();
        let owner = cx.entity().clone();
        let schema = self.settings.read(cx).current.clone();

        let device_lists_provider: Option<AudioDeviceListsProvider> =
            self.audio_bridge.engine.clone().map(|engine| {
                Arc::new(move |driver_type: &str| {
                    let backend = super::native_audio_backend_from_driver_type(driver_type);
                    let input_devices = engine.list_input_devices_for_backend(backend);
                    let output_devices = engine.list_output_devices_for_backend(backend);
                    SettingsAudioDeviceLists {
                        inputs: dedupe_preserve_order(
                            &input_devices
                                .iter()
                                .map(|d| d.name.clone())
                                .collect::<Vec<_>>(),
                        ),
                        outputs: dedupe_preserve_order(
                            &output_devices
                                .iter()
                                .map(|d| d.name.clone())
                                .collect::<Vec<_>>(),
                        ),
                        input_channels: input_devices
                            .into_iter()
                            .map(|d| (d.name, d.channels))
                            .collect(),
                        output_channels: output_devices
                            .into_iter()
                            .map(|d| (d.name, d.channels))
                            .collect(),
                    }
                }) as AudioDeviceListsProvider
            });
        // Do NOT enumerate/probe synchronously here when a live engine provider
        // exists — that would block opening Settings on hardware enumeration /
        // WDM-KS probing. The window populates backend-scoped lists off the UI
        // thread on its first render. The no-engine path still builds cheap
        // placeholder lists below.
        let initial_device_lists = SettingsAudioDeviceLists::default();

        let mut available_inputs = initial_device_lists.inputs.clone();
        if !available_inputs.contains(&schema.hardware.audio.device_in)
            && !schema.hardware.audio.device_in.is_empty()
            && device_lists_provider.is_none()
        {
            available_inputs.push(schema.hardware.audio.device_in.clone());
        }
        if available_inputs.is_empty() && device_lists_provider.is_none() {
            available_inputs.push("Built-in Microphone".to_string());
        }
        available_inputs = dedupe_preserve_order(&available_inputs);

        let mut available_outputs = initial_device_lists.outputs.clone();
        if !available_outputs.contains(&schema.hardware.audio.device_out)
            && !schema.hardware.audio.device_out.is_empty()
            && device_lists_provider.is_none()
        {
            available_outputs.push(schema.hardware.audio.device_out.clone());
        }
        if available_outputs.is_empty() && device_lists_provider.is_none() {
            available_outputs.push("Speakers (Realtek)".to_string());
        }
        available_outputs = dedupe_preserve_order(&available_outputs);

        // (device name, channel count) for the read-only channel lists (Phase C).
        let available_input_channels = initial_device_lists.input_channels.clone();
        let available_output_channels = initial_device_lists.output_channels.clone();

        // Only list backends that actually exist on this build/edition — a
        // Windows-only option like "WASAPI Shared" must never be selectable
        // on Linux/macOS, and Exclusive-only ASIO must never appear on
        // Community (or an Exclusive build without ASIO entitlement).
        let available_backends = crate::settings::available_audio_driver_types();

        let on_update: OnSettingUpdate = Arc::new(move |updater, cx| {
            let _ = owner.update(cx, |this, cx| {
                // A sample-rate change while the engine is live is intercepted
                // here (confirmation dialog) instead of silently restarting the
                // audio engine — see `handle_setting_update`.
                this.handle_setting_update(updater, cx);
            });
        });

        // "Open Keyboard Shortcuts…" (Settings > Shortcuts) reuses the exact
        // window `help:keyboard-shortcuts` opens — only the studio window
        // holds the live `KeymapManager`, so Settings calls back into it
        // instead of duplicating the editor.
        let keymap_owner = cx.entity().clone();
        let on_open_keyboard_shortcuts: Option<
            crate::components::settings_dialog::OnOpenKeyboardShortcuts,
        > = Some(Arc::new(move |window, cx| {
            let owner_bounds = Some(window.bounds());
            let _ = keymap_owner.update(cx, |this, cx| {
                this.open_keymap_window(owner_bounds, cx);
            });
        }));
        let plugin_manager_owner = cx.entity().clone();
        let on_open_plugin_manager: Option<
            crate::components::settings_dialog::OnOpenPluginManager,
        > = Some(Arc::new(move |window, cx| {
            let owner_bounds = Some(window.bounds());
            let _ = plugin_manager_owner.update(cx, |this, cx| {
                this.open_plugin_manager_external_window(owner_bounds, cx);
            });
        }));

        let engine_for_latency = self.audio_bridge.engine.clone();
        let deferred_rate = self.audio_bridge.sample_rate_deferred_target.clone();
        let latency_provider: crate::components::settings_dialog::AudioLatencySnapshotProvider =
            Arc::new(move || {
                let mut snapshot = engine_for_latency
                    .as_ref()
                    .map(crate::settings::SettingsAudioLatencySnapshot::from_engine)
                    .unwrap_or_else(crate::settings::SettingsAudioLatencySnapshot::unavailable);
                // A deferred ("Later") rate is pending only until the active device
                // rate actually matches it — then it resolves automatically.
                let target = deferred_rate.load(std::sync::atomic::Ordering::Relaxed);
                if target != 0 && target != snapshot.active_sample_rate {
                    snapshot.restart_pending = true;
                    snapshot.deferred_sample_rate = target;
                }
                snapshot
            });
        let input_test_start: Option<crate::components::settings_dialog::InputTestStartFn> =
            self.audio_bridge.engine.clone().map(|engine| {
                Arc::new(move |device_id: Option<String>| {
                    let device_id = device_id.filter(|id| !id.trim().is_empty());
                    engine
                        .start_input_test(device_id.as_deref())
                        .map_err(|error| error.to_string())
                }) as crate::components::settings_dialog::InputTestStartFn
            });
        let input_test_stop: Option<crate::components::settings_dialog::InputTestStopFn> =
            self.audio_bridge.engine.clone().map(|engine| {
                Arc::new(move || {
                    engine.stop_input_test();
                }) as crate::components::settings_dialog::InputTestStopFn
            });
        let input_test_level: Option<crate::components::settings_dialog::InputTestLevelFn> =
            self.audio_bridge.engine.clone().map(|engine| {
                Arc::new(move || engine.input_test_level())
                    as crate::components::settings_dialog::InputTestLevelFn
            });

        match open_settings_window(
            owner_bounds,
            settings,
            available_inputs,
            available_outputs,
            available_backends,
            available_input_channels,
            available_output_channels,
            device_lists_provider,
            latency_provider,
            input_test_start,
            input_test_stop,
            input_test_level,
            on_update,
            on_open_keyboard_shortcuts,
            on_open_plugin_manager,
            cx,
        ) {
            Ok(handle) => self.external_windows.settings = Some(handle),
            Err(err) => eprintln!("[settings] failed to open settings window: {err}"),
        }

        if crate::components::settings_dialog::settings_perf_debug_enabled() {
            eprintln!(
                "[settings-perf] open_settings_dialog took {:.1}ms (synchronous; device probing deferred off-thread)",
                open_started.elapsed().as_secs_f64() * 1000.0
            );
        }
    }

    pub(super) fn close_settings_dialog(&mut self, cx: &mut Context<Self>) {
        if let Some(handle) = self.external_windows.settings.take() {
            let _ = handle.update(cx, |_settings, window, _cx| window.remove_window());
        }
        self.overlay.text_context_menu = None;
        cx.notify();
    }

    pub(super) fn open_keymap_window(
        &mut self,
        owner_bounds: Option<Bounds<gpui::Pixels>>,
        cx: &mut Context<Self>,
    ) {
        if let Some(handle) = self.external_windows.keymap.clone() {
            if handle
                .update(cx, |_keymap, window, _cx| window.activate_window())
                .is_ok()
            {
                return;
            }
            self.external_windows.keymap = None;
        }

        let manager = self.keymap_manager.clone();
        let studio = cx.entity().clone();
        let on_changed: KeymapChangedCb = Arc::new(move |manager, app| {
            let _ = studio.update(app, |layout, cx| {
                let profile_id = manager.active_profile_id().to_string();
                layout.keymap_manager = manager;
                // Persist the active profile so the chosen keymap survives a
                // restart. `update_setting` writes settings.json and notifies;
                // skip the write when the id is unchanged to avoid churn from
                // binding-only edits that keep the same profile.
                let _ = layout.settings.update(cx, |settings, cx| {
                    if settings.current.general.keymap_profile != profile_id {
                        settings.update_setting(
                            |schema| schema.general.keymap_profile = profile_id.clone(),
                            cx,
                        );
                    }
                });
                cx.notify();
            });
        });

        match open_keymap_window(owner_bounds, manager, on_changed, cx) {
            Ok(handle) => self.external_windows.keymap = Some(handle),
            Err(err) => eprintln!("[keymap] failed to open window: {err}"),
        }
    }

    /// Open (or re-focus) the Audio Jam window.
    pub(super) fn open_jam_window(
        &mut self,
        owner_bounds: Option<Bounds<gpui::Pixels>>,
        cx: &mut Context<Self>,
    ) {
        if let Some(handle) = self.external_windows.jam.clone() {
            if handle
                .update(cx, |_jam, window, _cx| window.activate_window())
                .is_ok()
            {
                return;
            }
            self.external_windows.jam = None;
        }

        // The jam controller needs the engine's shared state to reach the audio
        // bus. Installing it here rather than at startup means a Studio that
        // never opens this window never starts a jam subsystem at all.
        if let Some(engine) = self.audio_bridge.engine.as_ref() {
            if let Err(error) = crate::jam::install(engine.jam_bus(), Some(engine.clone())) {
                eprintln!("[jam] {error}");
            }
        } else {
            eprintln!("[jam] the audio engine is not running; Audio Jam needs it for routing");
        }

        let layout = cx.entity().clone();
        let create_layout = layout.clone();
        let multitrack_layout = layout.clone();
        let handlers = crate::components::jam_window::JamWindowHandlers {
            create_track: std::sync::Arc::new(move |request, app| {
                let _ = create_layout.update(app, |this, cx| {
                    this.create_track_from_jam_stream(request, cx);
                });
            }),
            publish_track: std::sync::Arc::new(move |share, app| {
                let _ = layout.update(app, |this, cx| {
                    this.share_selected_track_over_jam(share, cx);
                });
            }),
            multitrack_tracks: std::sync::Arc::new(move |app| {
                multitrack_layout.read_with(app, |this, cx| this.jam_multitrack_tracks(cx))
            }),
        };

        match crate::components::jam_window::open_jam_window(owner_bounds, handlers, cx) {
            Ok(handle) => self.external_windows.jam = Some(handle),
            Err(error) => eprintln!("[jam] failed to open window: {error}"),
        }
    }

    /// Share the selected track over Audio Jam, or stop sharing it.
    ///
    /// Which track is selected is the shell's own state, which is why the panel
    /// asks rather than deciding. Sharing nothing is not an error the user needs
    /// a dialog for — the panel's button is simply about a selection they can
    /// see is empty.
    pub(crate) fn share_selected_track_over_jam(&mut self, share: bool, cx: &mut Context<Self>) {
        let selected = {
            let timeline = self.timeline.read(cx);
            timeline
                .state
                .selection
                .selected_track_id
                .clone()
                .and_then(|id| {
                    timeline
                        .state
                        .tracks
                        .iter()
                        .find(|track| track.id == id)
                        .map(|track| (track.id.clone(), track.name.clone()))
                })
        };
        let Some((track_id, track_name)) = selected else {
            eprintln!("[jam] no track is selected to share");
            return;
        };

        let result = crate::jam::with_controller(|controller| {
            if share {
                controller.publish_track(&track_id, &track_name)
            } else {
                controller.unpublish_track(&track_id)
            }
        });
        if let Err(error) = result {
            eprintln!(
                "[jam] sharing '{track_name}' failed: {}",
                error.user_message()
            );
        }
    }

    /// The tracks a multitrack jam stream would carry, in channel-pair order.
    ///
    /// Every channel that holds or produces audio of its own — audio, MIDI and
    /// instrument tracks — and nothing that is a sum of them. Sending Master,
    /// a bus or a return alongside the tracks feeding it would put the same
    /// signal in the stream twice, which is not a multitrack take; it is a
    /// multitrack take plus a duplicate mix of itself.
    ///
    /// The list is capped rather than refused when a project has more tracks
    /// than the layout holds: sharing the first eight is useful, and a project
    /// with nine tracks should not simply be unable to share.
    pub(crate) fn jam_multitrack_tracks(&self, cx: &App) -> Vec<(String, String)> {
        use crate::components::timeline::state::TrackType;

        self.timeline
            .read(cx)
            .state
            .tracks
            .iter()
            .filter(|track| {
                matches!(
                    track.track_type,
                    TrackType::Audio | TrackType::Midi | TrackType::Instrument
                )
            })
            .take(DirectAudio::jam_bus::MAX_MULTITRACK_PAIRS)
            .map(|track| (track.id.clone(), track.name.clone()))
            .collect()
    }

    /// Share the arrangement as one multitrack jam stream, or stop.
    pub(crate) fn share_multitrack_over_jam(&mut self, share: bool, cx: &mut Context<Self>) {
        let tracks = if share {
            self.jam_multitrack_tracks(cx)
        } else {
            Vec::new()
        };
        if share && tracks.is_empty() {
            eprintln!("[jam] this project has no tracks to share");
            return;
        }
        let result =
            crate::jam::with_controller(|controller| controller.publish_multitrack(&tracks));
        if let Err(error) = result {
            eprintln!("[jam] multitrack share failed: {}", error.user_message());
        }
    }

    /// Turn one remote jam stream into an audio track.
    ///
    /// Three steps, in the order the rest of Studio does them: mint an Audio
    /// Connections input bus bound to the stream's channels, create an audio
    /// track, point the track at that bus. Nothing here is jam-specific except
    /// the device id — which is exactly the point, because it means the track
    /// survives, is saved, and is re-routed by the same code every other input
    /// uses.
    pub(crate) fn create_track_from_jam_stream(
        &mut self,
        request: crate::components::jam_window::CreateTrackFromStream,
        cx: &mut Context<Self>,
    ) {
        use crate::audio_connections::{AudioConnectionDirection, AudioPortId, ChannelLayout};
        use timeline_state::CreateTrackOptions;

        let ports = crate::audio_connections::current_available_ports();
        if !ports.has_device(&request.device_id) {
            // The publisher left between the panel rendering and the click.
            eprintln!(
                "[jam] stream {} is no longer published; no track was created",
                request.stream_id
            );
            return;
        }
        let layout_kind = if request.channels >= 2 {
            ChannelLayout::Stereo
        } else {
            ChannelLayout::Mono
        };

        let mutation = self.timeline.update(cx, |timeline, cx| {
            let (connection_id, mut mutation) = timeline.state.audio_connections.add_connection(
                AudioConnectionDirection::Input,
                layout_kind,
                &ports,
            );
            timeline
                .state
                .audio_connections
                .update_name(&connection_id, &request.track_name);
            timeline
                .state
                .audio_connections
                .set_device(&connection_id, &request.device_id, &ports);
            for channel in 0..layout_kind.channel_count() {
                let port_name = request
                    .channel_labels
                    .get(channel)
                    .cloned()
                    .unwrap_or_else(|| format!("Ch {}", channel + 1));
                let bound = timeline.state.audio_connections.update_port_binding(
                    &connection_id,
                    channel,
                    Some(AudioPortId::new(
                        request.device_id.clone(),
                        port_name,
                        channel as u32,
                    )),
                    &ports,
                );
                // Binding a channel is a routing change like any other; the
                // warnings ride along so a partially bound bus still explains
                // itself in the Audio Connections window.
                mutation.needs_routing_rebuild |= bound.needs_routing_rebuild;
                mutation.warnings.extend(bound.warnings);
            }

            let index = timeline.state.tracks.len();
            let track_id = timeline.state.create_track(CreateTrackOptions {
                track_type: timeline_state::TrackType::Audio,
                name: request.track_name.clone(),
                color: timeline.state.track_color_for_index(index),
                volume: timeline_state::volume::db_to_norm(0.0),
                pan: 0.0,
                // Not armed: a remote performer is being monitored, not
                // recorded, until somebody deliberately arms the track.
                armed: false,
                // Monitoring on, because a track routed to a performer that
                // cannot be heard is indistinguishable from a broken one.
                input_monitor: timeline_state::InputMonitorMode::Always,
            });
            timeline
                .state
                .set_track_audio_input_connection(&track_id, Some(connection_id));
            cx.notify();
            mutation
        });

        self.mark_dirty();
        if mutation.needs_routing_rebuild {
            self.publish_audio_connection_routing(cx);
        }
        self.refresh_audio_connections_window(cx);
    }

    pub(super) fn open_about_window(
        &mut self,
        owner_bounds: Option<Bounds<gpui::Pixels>>,
        cx: &mut Context<Self>,
    ) {
        if let Some(handle) = self.external_windows.about.clone() {
            if handle
                .update(cx, |_about, window, _cx| window.activate_window())
                .is_ok()
            {
                return;
            }
            self.external_windows.about = None;
        }

        match crate::components::about_window::open_about_window(owner_bounds, cx) {
            Ok(handle) => self.external_windows.about = Some(handle),
            Err(err) => eprintln!("[about] failed to open window: {err}"),
        }
    }

    /// What the audio engine currently reports about itself.
    ///
    /// Two calls into the engine: `stats()` for the device and the callback
    /// budget, `latency_info()` for the delay-compensation graph. Both are
    /// control-thread reads of already-published state — no engine command, no
    /// lock held across the UI — so the monitor can ask twice a second.
    pub(crate) fn audio_engine_reading(
        &self,
    ) -> crate::components::performance_window::AudioEngineReading {
        use crate::components::performance_window::AudioEngineReading;

        let Some(engine) = self.audio_bridge.engine.as_ref() else {
            return AudioEngineReading::default();
        };
        let stats = engine.stats();
        let latency = engine.latency_info();
        AudioEngineReading {
            present: true,
            running: stats.running && stats.stream_open,
            device_state: stats.device_state.clone(),
            backend_name: stats.backend_name.clone(),
            output_device: stats.output_device.clone(),
            sample_rate: stats.sample_rate,
            buffer_frames: stats.buffer_size,
            output_latency_ms: stats.estimated_latency_ms,
            input_latency_ms: stats.input_latency_ms,
            round_trip_latency_ms: stats.round_trip_latency_ms,
            pdc_samples: latency.max_path_samples,
            pdc_ms: latency.max_path_ms,
            master_latency_samples: latency.master_samples,
            pdc_enabled: latency.pdc_enabled,
            callback_last_us: stats.callback_last_us,
            callback_max_us: stats.callback_max_us,
            callback_deadline_us: stats.callback_deadline_us,
            glitch_count: stats.glitch_count,
            dropout_count: stats.dropout_count,
            dropout_last_reason: stats.dropout_last_reason.clone(),
            dropout_protection_mode: stats.dropout_protection_mode.clone(),
            active_voices: stats.active_voices,
            last_error: stats.last_error.clone(),
        }
    }

    /// Close and re-open the audio device at the rate it is already running,
    /// then rebuild the project runtime against it.
    ///
    /// The same path a sample-rate change takes — which is the point. A stream
    /// that has stopped coping (a device that dropped, a plug-in that wedged
    /// the graph) is recovered by the teardown and rebuild, not by a reset flag
    /// invented for this button; anything less would be a control that looks
    /// like it did something.
    pub(crate) fn restart_audio_engine(&mut self, cx: &mut Context<Self>) {
        let rate = self.current_audio_sample_rate().max(1);
        eprintln!("[audio-device] restart requested from Performance Monitor rate={rate}");
        if self.is_recording_active(cx) {
            self.stop_native_recording(cx);
        } else {
            self.stop_native_playback(cx);
        }
        self.reopen_audio_with_sample_rate(rate, cx);
    }

    pub(super) fn open_performance_window(
        &mut self,
        owner_bounds: Option<Bounds<gpui::Pixels>>,
        cx: &mut Context<Self>,
    ) {
        if let Some(handle) = self.external_windows.performance.clone() {
            if handle
                .update(cx, |_view, window, _cx| window.activate_window())
                .is_ok()
            {
                return;
            }
            self.external_windows.performance = None;
        }

        // The window owns no engine handle of its own: it asks the Studio for a
        // reading on its own refresh tick, and asks it to restart when the user
        // does. One owner of the engine, whatever is on screen.
        //
        // `update`, not `read`, and never from the window's `render`. This
        // function runs inside a Studio update — the window's first frame can
        // land before that update returns — and touching a leased entity is a
        // double-lease panic, which the release profile turns into an abort.
        // The reader is only ever called from the window's refresh task, which
        // runs outside any lease.
        let reader_owner = cx.entity().downgrade();
        let read_engine: crate::components::performance_window::AudioEngineReader =
            Arc::new(move |app: &mut gpui::App| {
                reader_owner
                    .update(app, |layout, _cx| layout.audio_engine_reading())
                    .unwrap_or_default()
            });
        let restart_owner = cx.entity().clone();
        let on_restart: crate::components::performance_window::RestartEngineCb =
            Arc::new(move |app: &mut gpui::App| {
                StudioLayout::defer_update(&restart_owner, app, |layout, cx| {
                    layout.restart_audio_engine(cx);
                });
            });

        // Taken here, where `self` is already borrowed, so the window has
        // something real to draw on its first frame without reading back into
        // an entity that is still leased.
        let reading = self.audio_engine_reading();

        match crate::components::performance_window::open_performance_window(
            owner_bounds,
            reading,
            read_engine,
            on_restart,
            cx,
        ) {
            Ok(handle) => self.external_windows.performance = Some(handle),
            Err(err) => eprintln!("[performance] failed to open window: {err}"),
        }
    }

    /// Build the Project Settings view-model from live project state. Cheap —
    /// a handful of scalars plus the project name/path — so it is safe to
    /// re-derive whenever the window needs refreshing.
    pub(crate) fn build_project_settings_snapshot(
        &self,
        cx: &gpui::App,
    ) -> crate::components::project_settings_window::ProjectSettingsSnapshot {
        let timeline = self.timeline.read(cx);
        let base_ts = timeline
            .state
            .time_signature_map
            .time_signature_at_beat(0.0);
        let engine_sample_rate = self.audio_bridge.engine.as_ref().and_then(|engine| {
            engine
                .stats()
                .stream_open
                .then(|| self.current_audio_sample_rate())
        });
        crate::components::project_settings_window::ProjectSettingsSnapshot {
            name: self.project_switcher.current_project.name.clone(),
            path: self.project_path.clone(),
            is_dirty: self.project_switcher.current_project.is_dirty,
            bpm: timeline.state.bpm,
            time_signature: (base_ts.numerator as u32, base_ts.denominator as u32),
            has_tempo_markers: !timeline.state.tempo_map.points.is_empty(),
            has_time_signature_markers: timeline.state.time_signature_has_markers(),
            sample_rate: timeline.state.project_sample_rate,
            engine_sample_rate,
            time_display_format: timeline.state.time_display_format,
            timecode_rate: timeline.state.timecode_rate,
            track_count: timeline.state.tracks.len(),
        }
    }

    /// Push fresh project state to the Project Settings window. No-op when the
    /// window is closed, so ordinary edits pay nothing for a window that is not
    /// on screen.
    pub(crate) fn push_project_settings_snapshot_to_window(&mut self, cx: &mut Context<Self>) {
        let Some(handle) = self.external_windows.project_settings.clone() else {
            return;
        };
        let snapshot = self.build_project_settings_snapshot(cx);
        let _ = handle.update(cx, |view, _window, cx| {
            if view.set_snapshot(snapshot) {
                cx.notify();
            }
        });
    }

    pub(super) fn open_project_settings_window(
        &mut self,
        owner_bounds: Option<Bounds<gpui::Pixels>>,
        cx: &mut Context<Self>,
    ) {
        if let Some(handle) = self.external_windows.project_settings.clone() {
            if handle
                .update(cx, |_view, window, _cx| window.activate_window())
                .is_ok()
            {
                self.push_project_settings_snapshot_to_window(cx);
                return;
            }
            self.external_windows.project_settings = None;
        }

        let snapshot = self.build_project_settings_snapshot(cx);
        let owner = cx.entity().clone();
        let callbacks = crate::components::project_settings_window::ProjectSettingsCallbacks {
            on_bpm_drag: {
                let owner = owner.clone();
                Arc::new(move |sample, cx| {
                    StudioLayout::defer_update(&owner, cx, move |this, cx| {
                        // Reuse the transport scrub path so tempo-map targeting,
                        // fine/coarse modifiers, engine sync, and bounds match.
                        this.apply_bpm_drag_sample(sample, cx);
                        this.push_project_settings_snapshot_to_window(cx);
                    });
                })
            },
            on_bpm_drag_end: {
                let owner = owner.clone();
                Arc::new(move |cx| {
                    StudioLayout::defer_update(&owner, cx, move |this, cx| {
                        this.commit_bpm_drag(cx);
                        this.push_project_settings_snapshot_to_window(cx);
                    });
                })
            },
            on_set_time_signature: {
                let owner = owner.clone();
                Arc::new(move |numerator, denominator, cx| {
                    StudioLayout::defer_update(&owner, cx, move |this, cx| {
                        this.set_project_base_time_signature(numerator, denominator, cx);
                        this.push_project_settings_snapshot_to_window(cx);
                    });
                })
            },
            on_set_sample_rate: {
                let owner = owner.clone();
                Arc::new(move |rate, cx| {
                    StudioLayout::defer_update(&owner, cx, move |this, cx| {
                        this.request_project_sample_rate_change(rate, cx);
                        this.push_project_settings_snapshot_to_window(cx);
                    });
                })
            },
            on_set_time_display_format: {
                let owner = owner.clone();
                Arc::new(move |format, cx| {
                    StudioLayout::defer_update(&owner, cx, move |this, cx| {
                        this.set_project_time_display_format(format, cx);
                        this.push_project_settings_snapshot_to_window(cx);
                    });
                })
            },
            on_set_timecode_rate: {
                let owner = owner.clone();
                Arc::new(move |rate, cx| {
                    StudioLayout::defer_update(&owner, cx, move |this, cx| {
                        this.set_project_timecode_rate(rate, cx);
                        this.push_project_settings_snapshot_to_window(cx);
                    });
                })
            },
            on_close: {
                let owner = owner.clone();
                Arc::new(move |window, cx| {
                    let _ = owner.update(cx, |this, cx| {
                        this.external_windows.project_settings = None;
                        cx.notify();
                    });
                    window.remove_window();
                })
            },
        };

        match crate::components::project_settings_window::open_project_settings_window(
            owner_bounds,
            snapshot,
            callbacks,
            cx,
        ) {
            Ok(handle) => self.external_windows.project_settings = Some(handle),
            Err(err) => eprintln!("[project-settings] failed to open window: {err}"),
        }
    }

    /// Set the meter at beat 0 — the project's base time signature — and push
    /// the resulting map to the engine.
    fn set_project_base_time_signature(
        &mut self,
        numerator: u32,
        denominator: u32,
        cx: &mut Context<Self>,
    ) {
        let numerator = numerator.clamp(1, 64) as u16;
        let denominator = denominator.clamp(1, 64) as u16;
        self.edit_time_signature_state(
            "Set Time Signature",
            |timeline| {
                let before = timeline
                    .state
                    .time_signature_map
                    .time_signature_at_beat(0.0);
                if before.numerator == numerator && before.denominator == denominator {
                    return;
                }
                timeline
                    .state
                    .add_time_signature_point(0.0, numerator, denominator);
            },
            cx,
        );
    }

    /// Set the project timebase — the unit the ruler and every position readout
    /// are shown in.
    ///
    /// Display state, so there is nothing to push to the engine and nothing to
    /// undo: no musical position changes, only how it is written. It is project
    /// state rather than a preference, so it marks the project dirty and travels
    /// in the project file.
    fn set_project_time_display_format(
        &mut self,
        format: crate::components::timeline::timeline_state::TimeDisplayFormat,
        cx: &mut Context<Self>,
    ) {
        let changed = self.timeline.update(cx, |timeline, cx| {
            if timeline.state.time_display_format == format {
                return false;
            }
            timeline.state.time_display_format = format;
            cx.notify();
            true
        });
        if changed {
            self.mark_dirty();
            cx.notify();
        }
    }

    /// Set the frame rate Timecode is counted at. Same contract as
    /// [`Self::set_project_time_display_format`].
    fn set_project_timecode_rate(
        &mut self,
        rate: crate::components::timeline::timeline_state::TimecodeRate,
        cx: &mut Context<Self>,
    ) {
        let changed = self.timeline.update(cx, |timeline, cx| {
            if timeline.state.timecode_rate == rate {
                return false;
            }
            timeline.state.timecode_rate = rate;
            cx.notify();
            true
        });
        if changed {
            self.mark_dirty();
            cx.notify();
        }
    }

    /// Builds the routing-matrix view-model from the current timeline tracks.
    fn build_routing_matrix_snapshot(
        &self,
        cx: &gpui::App,
    ) -> crate::components::RoutingMatrixSnapshot {
        crate::components::RoutingMatrixSnapshot {
            tracks: self.timeline.read(cx).state.tracks.clone(),
        }
    }

    /// Opens the Extensions registry browser, or focuses it if already open.
    /// The window owns no studio state — it talks to the public registry and
    /// installs into the user extensions directory — so closing it only clears
    /// the handle.
    pub(super) fn open_extensions_window(
        &mut self,
        owner_bounds: Option<Bounds<gpui::Pixels>>,
        cx: &mut Context<Self>,
    ) {
        if let Some(handle) = self.external_windows.extensions.clone() {
            if handle
                .update(cx, |_window, w, _cx| w.activate_window())
                .is_ok()
            {
                return;
            }
            self.external_windows.extensions = None;
        }

        let studio = cx.entity().clone();
        let on_close: Arc<dyn Fn(&mut Window, &mut App) + Send + Sync> = Arc::new(move |_, app| {
            let _ = studio.update(app, |layout, cx| {
                layout.external_windows.extensions = None;
                cx.notify();
            });
        });

        match crate::components::open_extensions_window(owner_bounds, on_close, cx) {
            Ok(handle) => self.external_windows.extensions = Some(handle),
            Err(err) => eprintln!("[extensions] failed to open window: {err}"),
        }
    }

    /// Pushes a refreshed snapshot to the routing-matrix window if it is open.
    pub(crate) fn push_routing_matrix_snapshot_to_window(&mut self, cx: &mut Context<Self>) {
        let Some(handle) = self.external_windows.routing_matrix.clone() else {
            return;
        };
        let snapshot = self.build_routing_matrix_snapshot(cx);
        let _ = handle.update(cx, |window, _w, cx| {
            window.set_snapshot(snapshot);
            cx.notify();
        });
    }

    /// Opens the Audio Routing Matrix ("Audio Connections") window, or focuses
    /// it if already open. Toggling a send cell routes back through the owner
    /// (`defer_update`) to mutate track state and push a refreshed snapshot.
    pub(super) fn open_routing_matrix_window(
        &mut self,
        owner_bounds: Option<Bounds<gpui::Pixels>>,
        cx: &mut Context<Self>,
    ) {
        if let Some(handle) = self.external_windows.routing_matrix.clone() {
            if handle
                .update(cx, |_window, w, _cx| w.activate_window())
                .is_ok()
            {
                return;
            }
            self.external_windows.routing_matrix = None;
        }

        let snapshot = self.build_routing_matrix_snapshot(cx);

        let studio = cx.entity().clone();
        let on_toggle_send: crate::components::routing_matrix_window::ToggleSendCb = {
            let studio = studio.clone();
            Arc::new(move |source_id: String, dest_id: String, _w, cx| {
                StudioLayout::defer_update(&studio, cx, move |this, cx| {
                    let changed = this.timeline.update(cx, |timeline, _cx| {
                        let already = timeline
                            .state
                            .tracks
                            .iter()
                            .find(|t| t.id == source_id)
                            .and_then(|t| {
                                t.sends
                                    .iter()
                                    .find(|s| s.target_track_id == dest_id)
                                    .cloned()
                            });
                        if let Some(send) = already {
                            timeline.state.remove_send(&source_id, &send.id);
                            true
                        } else {
                            timeline
                                .state
                                .add_send_to_target(&source_id, &dest_id)
                                .is_some()
                        }
                    });
                    if changed {
                        this.mark_dirty();
                        this.audio_bridge.project_dirty = true;
                    }
                    this.push_routing_matrix_snapshot_to_window(cx);
                    cx.notify();
                });
            })
        };

        let on_close: Arc<dyn Fn(&mut Window, &mut App) + Send + Sync> = Arc::new(move |_, app| {
            let _ = studio.update(app, |layout, cx| {
                layout.external_windows.routing_matrix = None;
                cx.notify();
            });
        });

        match crate::components::open_routing_matrix_window(
            owner_bounds,
            snapshot,
            on_toggle_send,
            on_close,
            cx,
        ) {
            Ok(handle) => self.external_windows.routing_matrix = Some(handle),
            Err(err) => eprintln!("[routing-matrix] failed to open window: {err}"),
        }
    }

    /// A track's persisted Soundfont Player settings, so an opening window shows
    /// the `.sf2` and preset the engine is already playing rather than an empty
    /// panel.
    fn soundfont_track_state(
        &self,
        track_id: &str,
        cx: &App,
    ) -> crate::components::soundfont_player_window::SoundfontPlayerTrackState {
        use crate::components::soundfont_player_window::SoundfontPlayerTrackState;
        let timeline = self.timeline.read(cx);
        let Some(track) = timeline.state.find_track(track_id) else {
            return SoundfontPlayerTrackState::default();
        };
        SoundfontPlayerTrackState {
            path: track.soundfont_path.clone(),
            preset: track.soundfont_preset,
            volume: track.soundfont_volume,
            reverb_chorus: track.soundfont_reverb_chorus,
            polyphony: track.soundfont_polyphony,
            envelope: track.soundfont_envelope,
            quality: track.soundfont_quality,
        }
    }

    /// Opens the built-in Soundfont Player MDI window, or focuses it (and its
    /// document) if already open. Called from the Inspector's Open button for
    /// an Instrument track whose `builtin_soundfont_player` marker is set.
    pub(super) fn open_soundfont_player_window(
        &mut self,
        owner_bounds: Option<Bounds<gpui::Pixels>>,
        track_id: String,
        cx: &mut Context<Self>,
    ) {
        let initial = self.soundfont_track_state(&track_id, cx);
        if let Some(handle) = self.external_windows.soundfont_player.clone() {
            let activated = handle
                .update(cx, |window, w, cx| {
                    window.focus_soundfont_player(track_id.clone(), initial.clone(), cx);
                    w.activate_window();
                    cx.notify();
                })
                .is_ok();
            if activated {
                return;
            }
            self.external_windows.soundfont_player = None;
        }

        let studio = cx.entity().clone();
        let on_close: Arc<dyn Fn(&mut Window, &mut App) + Send + Sync> = Arc::new({
            let studio = studio.clone();
            move |_, app| {
                let _ = studio.update(app, |layout, cx| {
                    layout.external_windows.soundfont_player = None;
                    cx.notify();
                });
            }
        });
        let on_update_track: Arc<
            dyn Fn(crate::components::soundfont_player_window::SoundfontPlayerTrackUpdate, &mut App)
                + Send
                + Sync,
        > = Arc::new(move |update, app| {
            let _ = studio.update(app, |layout, cx| {
                let changed = layout.timeline.update(cx, |timeline, cx| {
                    let changed = timeline.state.set_track_soundfont_player_state(
                        &update.track_id,
                        update.settings.clone(),
                    );
                    if changed {
                        cx.notify();
                    }
                    changed
                });
                if changed {
                    layout.mark_dirty();
                    layout.audio_bridge.project_dirty = true;
                    layout.schedule_audio_project_sync(cx, false, "soundfont_player_update");
                }
                cx.notify();
            });
        });

        // The window's own player is control-side only. Auditioning routes
        // through the engine's MIDI preview so the sound comes from the track's
        // instrument — the same signal path playback uses.
        let studio = cx.entity().clone();
        let on_preview: Arc<
            dyn Fn(
                    &str,
                    crate::components::soundfont_player_window::SoundfontPlayerPreview,
                    &mut App,
                ) + Send
                + Sync,
        > = Arc::new(move |track_id, event, app| {
            let _ = studio.update(app, |layout, cx| {
                layout.dispatch_soundfont_preview(track_id, event, cx);
            });
        });

        match crate::components::soundfont_player_window::open_soundfont_player_window(
            owner_bounds,
            track_id,
            initial,
            on_close,
            on_update_track,
            on_preview,
            cx,
        ) {
            Ok(handle) => self.external_windows.soundfont_player = Some(handle),
            Err(err) => eprintln!("[soundfont-player] failed to open window: {err}"),
        }
    }

    pub(crate) fn open_mixer_external_window(
        &mut self,
        owner_bounds: Option<Bounds<gpui::Pixels>>,
        cx: &mut Context<Self>,
    ) {
        external_mixer_debug("external mixer open requested");
        let owner_bounds =
            resolve_owner_bounds_with_preferred(owner_bounds, self.studio_window_bounds(cx), cx);
        self.external_windows.pending_mixer_open = owner_bounds;
        self.schedule_pending_mixer_external_open(cx);
        cx.notify();
    }

    pub(super) fn schedule_pending_mixer_external_open(&mut self, cx: &mut Context<Self>) {
        if self.external_windows.pending_mixer_open.is_none() {
            return;
        }
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(0))
                .await;
            let _ = this.update(cx, |layout, cx| {
                layout.flush_pending_mixer_external_open(cx)
            });
        })
        .detach();
    }

    pub(super) fn flush_pending_mixer_external_open(&mut self, cx: &mut Context<Self>) {
        let owner_bounds = resolve_owner_bounds_with_preferred(
            self.external_windows.pending_mixer_open.take(),
            self.studio_window_bounds(cx),
            cx,
        );
        let Some(owner_bounds) = owner_bounds else {
            return;
        };

        self.prune_mixer_window(cx);
        if let Some(handle) = self.external_windows.mixer.clone() {
            if handle
                .update(cx, |_mixer, window, _cx| window.activate_window())
                .is_ok()
            {
                self.panels.bottom_docked = false;
                self.sync_timeline_chrome_metrics(cx);
                self.push_mixer_snapshot_to_window(cx);
                cx.notify();
                return;
            }
            self.external_windows.mixer = None;
        }

        self.menu_bar.open_menu_id = None;
        self.menu_bar.submenu_path.clear();
        self.overlay.open_popover = None;
        self.panels.bottom_docked = false;

        let snapshot = self.build_mixer_snapshot(cx);
        let callbacks = self.build_mixer_callbacks(cx.entity().clone());
        let owner = cx.entity().clone();
        let on_close: std::sync::Arc<dyn Fn(&mut Window, &mut gpui::App) + Send + Sync> =
            std::sync::Arc::new(move |_window, cx| {
                let _ = owner.update(cx, |layout, cx| layout.note_mixer_window_closed(cx));
            });
        let scroll_owner = cx.entity().clone();
        let on_mixer_scroll: std::sync::Arc<
            dyn Fn(f32, &mut Window, &mut gpui::App) + Send + Sync,
        > = std::sync::Arc::new(move |new_x: f32, _w, cx| {
            let _ = scroll_owner.update(cx, |layout, cx| {
                if layout.set_mixer_scroll_x(new_x, cx) {
                    layout.push_mixer_snapshot_to_window(cx);
                }
            });
        });
        let split_owner = cx.entity().clone();
        let on_mixer_split: std::sync::Arc<
            dyn Fn(crate::components::mixer_panel::MixerSplitAction, &mut Window, &mut gpui::App)
                + Send
                + Sync,
        > = std::sync::Arc::new(move |action, _w, cx| {
            let _ =
                split_owner.update(cx, |layout, cx| layout.apply_mixer_split_action(action, cx));
        });
        let dispatch_owner = cx.entity().clone();
        let dispatch_key: std::sync::Arc<
            dyn Fn(&gpui::KeyDownEvent, &mut gpui::App) -> bool + Send + Sync,
        > = std::sync::Arc::new(move |event, cx| {
            if event.is_held {
                return false;
            }
            let Some(command_id) = dispatch_owner.read(cx).shortcut_command_id(event) else {
                return false;
            };
            let _ = dispatch_owner.update(cx, |layout, cx| {
                let owner_bounds = layout.studio_window_bounds(cx);
                layout.dispatch_command_id_from_bounds(&command_id, owner_bounds, cx);
            });
            true
        });

        // The pop-out draws the menus its own controls open; the Studio still
        // runs them, because it owns the state every entry acts on.
        let menu_owner = cx.entity().clone();
        let on_menu_command: std::sync::Arc<
            dyn Fn(&str, &mut Window, &mut gpui::App) + Send + Sync,
        > = {
            let owner = menu_owner.clone();
            std::sync::Arc::new(move |command: &str, _window, cx| {
                let command = command.to_string();
                StudioLayout::defer_update(&owner, cx, move |layout, cx| {
                    let bounds = layout.studio_window_bounds(cx);
                    layout.dispatch_command_id_from_bounds(&command, bounds, cx);
                });
            })
        };
        let on_menu_close: std::sync::Arc<dyn Fn(&mut Window, &mut gpui::App) + Send + Sync> = {
            let owner = menu_owner;
            std::sync::Arc::new(move |_window, cx| {
                StudioLayout::defer_update(&owner, cx, move |layout, cx| {
                    layout.close_context_menu(cx);
                });
            })
        };

        match open_mixer_window(
            owner_bounds,
            snapshot,
            self.mixer_tree_sidebar.clone(),
            callbacks,
            on_close,
            on_mixer_scroll,
            on_mixer_split,
            dispatch_key,
            on_menu_command,
            on_menu_close,
            cx,
        ) {
            Ok(handle) => {
                self.external_windows.mixer = Some(handle);
                // Removing the docked mixer changes the arrangement's client
                // rectangle. Refresh the timeline's own reserved-chrome
                // metric too — `cx.refresh_windows()` only repaints the
                // already-computed layout, it doesn't recompute the bottom
                // panel height the timeline subtracts for its scroll/clip
                // geometry, which otherwise stays stale at the docked mixer's
                // height and clips the arrangement view.
                self.sync_timeline_chrome_metrics(cx);
                cx.refresh_windows();
                cx.notify();
            }
            Err(err) => {
                eprintln!("[mixer] failed to open external mixer window: {err}");
                self.panels.bottom_docked = true;
                self.sync_timeline_chrome_metrics(cx);
                self.set_active_panel(crate::layout::WorkspaceActivePanel::Mixer, cx);
                cx.notify();
            }
        }
    }

    pub(crate) fn close_mixer_window(&mut self, cx: &mut Context<Self>) {
        if let Some(handle) = self.external_windows.mixer.take() {
            let _ = handle.update(cx, |_mixer, window, _cx| window.remove_window());
        }
        cx.notify();
    }

    pub(super) fn note_mixer_window_closed(&mut self, cx: &mut Context<Self>) {
        self.external_windows.mixer = None;
        cx.notify();
    }

    pub(super) fn prune_mixer_window(&mut self, cx: &mut Context<Self>) {
        let Some(handle) = self.external_windows.mixer.clone() else {
            return;
        };
        if handle.update(cx, |_mixer, _window, _cx| ()).is_err() {
            self.external_windows.mixer = None;
        }
    }

    pub(crate) fn open_midi_editor_external_window(
        &mut self,
        owner_bounds: Option<Bounds<gpui::Pixels>>,
        cx: &mut Context<Self>,
    ) {
        if self.selected_midi_clip_id(cx).is_none() {
            return;
        }
        let owner_bounds =
            resolve_owner_bounds_with_preferred(owner_bounds, self.studio_window_bounds(cx), cx);
        self.midi_editor.pending_open = owner_bounds;
        self.schedule_pending_midi_editor_open(cx);
        cx.notify();
    }

    pub(super) fn schedule_pending_midi_editor_open(&mut self, cx: &mut Context<Self>) {
        if self.midi_editor.pending_open.is_none() {
            return;
        }
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(0))
                .await;
            let _ = this.update(cx, |layout, cx| layout.flush_pending_midi_editor_open(cx));
        })
        .detach();
    }

    pub(super) fn flush_pending_midi_editor_open(&mut self, cx: &mut Context<Self>) {
        let owner_bounds = resolve_owner_bounds_with_preferred(
            self.midi_editor.pending_open.take(),
            self.studio_window_bounds(cx),
            cx,
        );
        let Some(owner_bounds) = owner_bounds else {
            return;
        };

        if let Some(OpenPopover::Context { request }) = self.overlay.open_popover.as_ref() {
            if let ContextMenuTarget::Clip(clip_id) = &request.target {
                let clip_id = clip_id.clone();
                if self
                    .timeline
                    .read(cx)
                    .state
                    .find_clip(&clip_id)
                    .is_some_and(|(_, c)| matches!(c.clip_type, ClipType::Midi { .. }))
                {
                    self.select_midi_clip(&clip_id, cx);
                }
            }
        }

        self.prune_midi_editor_window(cx);
        if let Some(handle) = self.midi_editor.window.clone() {
            if handle
                .update(cx, |_w, window, _cx| window.activate_window())
                .is_ok()
            {
                midi_editor_debug("focus existing window");
                if let Some(clip_id) = self.selected_midi_clip_id(cx) {
                    if let Some((track, clip)) = self.timeline.read(cx).state.find_clip(&clip_id) {
                        midi_editor_debug(&format!(
                            "switch target clip clip={} track={}",
                            clip.name, track.name
                        ));
                    }
                }
                cx.notify();
                return;
            }
            self.midi_editor.window = None;
        }

        let clip_label = self
            .selected_midi_clip_id(cx)
            .and_then(|id| self.timeline.read(cx).state.find_clip(&id))
            .map(|(t, c)| (c.name.clone(), t.name.clone()));
        if let Some((clip_name, track_name)) = clip_label.as_ref() {
            midi_editor_debug(&format!("open window clip={clip_name} track={track_name}"));
        } else {
            midi_editor_debug("open window (no MIDI clip selected)");
        }

        self.menu_bar.open_menu_id = None;
        self.menu_bar.submenu_path.clear();

        let timeline = self.timeline.clone();
        let piano_roll = self.piano_roll_floating.clone();
        let virtual_keyboard = self.virtual_keyboard.clone();
        let owner = cx.entity().clone();
        let on_close: Arc<dyn Fn(&mut Window, &mut gpui::App) + Send + Sync> =
            Arc::new(move |window, cx| {
                // Capture the id while the window is still alive. The deferred
                // cleanup runs after `remove_window`, so it must not consult the
                // stored handle for metadata from an already-destroyed window.
                let window_id = window.window_handle().window_id();
                StudioLayout::defer_update(&owner, cx, move |layout, cx| {
                    layout.note_midi_editor_window_closed(window_id, cx);
                });
            });
        let dispatch_owner = cx.entity().clone();
        let dispatch_command: Arc<dyn Fn(&'static str, &mut gpui::App) + Send + Sync> =
            Arc::new(move |command_id, cx| {
                let _ = dispatch_owner.update(cx, |layout, cx| {
                    layout.dispatch_command_id(command_id, cx);
                    cx.notify();
                });
            });

        match open_midi_editor_window(
            Some(owner_bounds),
            timeline,
            piano_roll,
            virtual_keyboard,
            on_close,
            dispatch_command,
            cx,
        ) {
            Ok(handle) => {
                // Register the popout as a musical-typing source so its held
                // notes are released if it closes (register doesn't flush, so it
                // is safe to call directly here).
                let window_id = handle.window_id();
                let _ = self
                    .virtual_keyboard
                    .update(cx, |keyboard, _cx| keyboard.register_window(window_id));
                midi_editor_debug(&format!(
                    "register virtual-keyboard window id={}",
                    window_id.as_u64()
                ));
                self.midi_editor.window = Some(handle);
                cx.notify();
            }
            Err(err) => eprintln!("[midi-editor] failed to open window: {err}"),
        }
    }

    pub(crate) fn close_midi_editor_window(&mut self, cx: &mut Context<Self>) {
        let _ = self.piano_roll_floating.update(cx, |roll, cx| {
            roll.preview_all_notes_off("editor_close", cx);
        });
        if let Some(handle) = self.midi_editor.window.take() {
            // Drop any musical-typing notes the popout still held, and never
            // touch the window handle after it is removed.
            self.unregister_virtual_keyboard_window(handle.window_id(), cx);
            let _ = handle.update(cx, |_w, window, _cx| window.remove_window());
        }
        cx.notify();
    }

    pub(super) fn note_midi_editor_window_closed(
        &mut self,
        window_id: gpui::WindowId,
        cx: &mut Context<Self>,
    ) {
        let _ = self.piano_roll_floating.update(cx, |roll, cx| {
            roll.preview_all_notes_off("editor_close", cx);
        });
        midi_editor_debug(&format!(
            "unregister virtual-keyboard window id={}",
            window_id.as_u64()
        ));
        self.unregister_virtual_keyboard_window(window_id, cx);
        self.midi_editor.window = None;
        cx.notify();
    }

    pub(super) fn prune_midi_editor_window(&mut self, cx: &mut Context<Self>) {
        let Some(handle) = self.midi_editor.window.clone() else {
            return;
        };
        if handle.update(cx, |_w, _window, _cx| ()).is_err() {
            self.midi_editor.window = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_connections::{
        AudioConnection, AudioConnectionDirection, AudioConnectionId, AvailablePorts, ChannelLayout,
    };
    use crate::components::timeline::timeline_state::TimelineState;

    fn project_with_inputs() -> TimelineState {
        let ports = AvailablePorts::for_device("dev-1", "Interface", 4, 2);
        let mut state = TimelineState::default();
        state.audio_connections = crate::audio_connections::AudioConnectionRegistry::new();
        state.audio_connections.add(
            AudioConnection::new(
                "Microphone",
                AudioConnectionDirection::Input,
                ChannelLayout::Mono,
            )
            .bind_consecutive("dev-1", 0, |i| format!("Input {}", i + 1)),
        );
        state.audio_connections.add(
            AudioConnection::new(
                "Stereo Input",
                AudioConnectionDirection::Input,
                ChannelLayout::Stereo,
            )
            .bind_consecutive("dev-1", 2, |i| format!("Input {}", i + 1)),
        );
        state.audio_connections.add(
            AudioConnection::new(
                "Main Output",
                AudioConnectionDirection::Output,
                ChannelLayout::Stereo,
            )
            .bind_consecutive("dev-1", 0, |i| format!("Output {}", i + 1)),
        );
        state.audio_connections.revalidate(&ports);
        state
    }

    /// Add Track offers logical buses, never devices or channels.
    #[test]
    fn add_track_input_choices_come_from_the_registry_only() {
        let state = project_with_inputs();
        let labels: Vec<String> = dialog_audio_input_choices(&state)
            .into_iter()
            .map(|choice| choice.label)
            .collect();

        assert!(labels.contains(&"Stereo Input".to_string()));
        assert!(labels.contains(&"Microphone".to_string()));
        assert!(
            !labels.iter().any(|label| label.contains("Interface")
                || label.contains("Ch ")
                || label.contains("dev-1")),
            "no hardware identity may leak into Add Track: {labels:?}"
        );
        assert!(
            !labels.contains(&"Main Output".to_string()),
            "an output bus is never a track input"
        );
    }

    /// A mono track is only offered mono buses; a stereo bus would have no
    /// defined mapping into one channel.
    #[test]
    fn add_track_input_choices_respect_the_track_channel_count() {
        let state = project_with_inputs();
        let mut dialog =
            crate::components::add_track_dialog::AddTrackDialogState::open_for(0, false);
        dialog.audio_input_choices = dialog_audio_input_choices(&state);

        dialog.audio_format = AudioFormat::Mono;
        let mono: Vec<String> = dialog
            .audio_input_option_labels()
            .into_iter()
            .map(|option| option.id)
            .collect();
        assert_eq!(mono, vec!["No Input".to_string(), "Microphone".to_string()]);

        dialog.audio_format = AudioFormat::Stereo;
        let stereo: Vec<String> = dialog
            .audio_input_option_labels()
            .into_iter()
            .map(|option| option.id)
            .collect();
        assert!(stereo.contains(&"Stereo Input".to_string()));
        assert!(stereo.contains(&"Microphone".to_string()));
    }

    /// The dialog resolves a label to an id and nothing else; No Input and the
    /// escape hatch both yield no assignment.
    #[test]
    fn add_track_resolves_a_label_to_a_connection_id_without_creating_one() {
        let state = project_with_inputs();
        let choices = dialog_audio_input_choices(&state);
        let mut dialog =
            crate::components::add_track_dialog::AddTrackDialogState::open_for(0, false);
        dialog.audio_input_choices = choices.clone();
        dialog.audio_format = AudioFormat::Stereo;

        let stereo = choices
            .iter()
            .find(|choice| choice.label == "Stereo Input")
            .expect("stereo bus");
        assert_eq!(
            dialog
                .audio_input_connection_for_label(&stereo.label)
                .as_ref(),
            Some(&stereo.connection_id)
        );
        assert!(dialog
            .audio_input_connection_for_label(
                crate::components::add_track_dialog::NO_AUDIO_INPUT_LABEL
            )
            .is_none());

        // A stereo bus is not offered to — and cannot be assigned to — a mono
        // track.
        dialog.audio_format = AudioFormat::Mono;
        assert!(dialog
            .audio_input_connection_for_label(&stereo.label)
            .is_none());
        assert_eq!(
            state.audio_connections.len(),
            3,
            "listing and resolving must not create a bus"
        );
    }

    /// A default Audio track captures nothing until the user picks a bus.
    #[test]
    fn a_new_audio_track_defaults_to_no_input() {
        use crate::components::add_track_dialog::{AddTrackKind, NO_AUDIO_INPUT_LABEL};

        let dialog = crate::components::add_track_dialog::AddTrackDialogState::open_for(0, false);
        assert_eq!(dialog.input_label, NO_AUDIO_INPUT_LABEL);
        assert!(dialog
            .audio_input_connection_for_label(&dialog.input_label)
            .is_none());
    }

    /// A stale label from a removed bus becomes No Input, never another bus.
    #[test]
    fn a_selection_whose_bus_disappeared_falls_back_to_no_input() {
        use crate::components::add_track_dialog::{AddTrackKind, NO_AUDIO_INPUT_LABEL};

        let mut dialog =
            crate::components::add_track_dialog::AddTrackDialogState::open_for(0, false);
        dialog.audio_input_choices =
            vec![crate::components::add_track_dialog::AddTrackInputChoice {
                label: "Microphone".to_string(),
                connection_id: AudioConnectionId::from_stored("ac-mic"),
                channels: 1,
            }];
        dialog.input_label = "Microphone".to_string();

        dialog.audio_input_choices.clear();
        assert!(dialog
            .audio_input_connection_for_label(&dialog.input_label)
            .is_none());
        assert_eq!(
            dialog.audio_input_option_labels().len(),
            1,
            "only No Input remains"
        );
        assert_eq!(
            dialog.audio_input_option_labels()[0].id,
            NO_AUDIO_INPUT_LABEL
        );
    }
}

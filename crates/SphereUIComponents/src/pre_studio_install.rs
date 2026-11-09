//! Pre-studio session install — decode follow-up work that runs while only the
//! Loading Session window is visible. Restores plugins and warms the audio graph
//! before [`crate::layout::StudioLayout`] is mounted.

use std::time::{Duration, Instant};

use crate::components::plugin_picker::STUB_PLUGIN_ID;
use crate::components::progress_dialog::ProgressBarValue;
use crate::components::timeline::timeline_state::{
    InsertPluginFormat, PluginRuntimeBackend, PluginRuntimeState, TimelineState, TrackType,
    MASTER_TRACK_ID,
};
use crate::layout::engine_snapshot::build_engine_project_snapshot;
use crate::layout::plugin_bridge_runtime::{
    self, BridgePluginDescriptor, PluginBridgeRuntime, SharedPluginBridgeRuntime,
};
use crate::loading_session::{LoadedSessionPackage, SessionInstallHandoff};
use crate::project::apply_to_timeline;
use crate::settings::SettingsSchema;

const PLUGIN_RESTORE_TIMEOUT: Duration = Duration::from_secs(120);
const POLL_INTERVAL: Duration = Duration::from_millis(25);

#[derive(Debug, Clone, Default)]
pub struct PreStudioInstallReport {
    pub warnings: Vec<String>,
    pub restored: usize,
    pub failed: usize,
}

#[derive(Debug, Clone)]
struct RestoreTarget {
    track_id: String,
    slot_id: String,
    display_name: String,
    track_name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestoreOutcome {
    Pending,
    Ready,
    Missing,
    Failed,
    Timeout,
}

pub fn run_pre_studio_session_install(
    package: LoadedSessionPackage,
    progress: impl Fn(&str, ProgressBarValue),
) -> Result<(SessionInstallHandoff, PreStudioInstallReport), String> {
    eprintln!("[SessionLoad] progress sink attached");
    eprintln!("[LoadingSessionUI] presentation=dialog");

    progress("Preparing session", ProgressBarValue::value(0.15));

    let mut schema = SettingsSchema::load_from_disk();
    let output_device_name = schema.hardware.audio.device_out.clone();
    // The saved project owns its nominal rate. Application defaults seed new
    // projects only and must not replace a loaded session's audio timing.
    schema.general.project_defaults.sample_rate = match package.project.settings.sample_rate {
        44_100 | 48_000 | 88_200 | 96_000 | 192_000 => package.project.settings.sample_rate,
        _ => 48_000,
    };
    let (engine, stats) = crate::layout::build_and_warm_audio_engine(schema)?;

    let mut timeline_state = TimelineState::default();
    // See session_load: warning surfacing is deferred.
    let _load_warnings = apply_to_timeline(&package.project, &mut timeline_state);
    // Same split the session load grows: one send and one return when a
    // Rodhareist rig is in wet/dry/wet, and nothing when it is not.
    timeline_state.sync_all_wdw_splits();

    let mut bridge_slot: Option<SharedPluginBridgeRuntime> = None;
    if !plugin_bridge_runtime::bridge_enabled() {
        eprintln!("[PluginRestore] in-process path — engine sync will instantiate native inserts");
    }

    let targets = collect_restore_targets(&timeline_state);
    let total = targets.len().max(1);
    let mut report = PreStudioInstallReport::default();

    if plugin_bridge_runtime::bridge_enabled() {
        let runtime = PluginBridgeRuntime::ensure_shared(&mut bridge_slot)
            .map_err(|error| error.to_string())?;
        let sample_rate = engine.config().sample_rate;
        let max_block_size = engine.config().buffer_size;

        for (index, target) in targets.iter().enumerate() {
            let p = 0.2 + (0.55 * (index as f32) / total as f32);
            progress(
                &format!(
                    "Loading Plugin {}/{}: {} on {}",
                    index + 1,
                    total,
                    target.display_name,
                    target.track_name
                ),
                ProgressBarValue::value(p),
            );
            eprintln!(
                "[SessionLoad] plugin restore progress {}/{}: {} on {}",
                index + 1,
                total,
                target.display_name,
                target.track_name
            );

            match restore_bridge_target(
                &mut timeline_state,
                &runtime,
                target,
                sample_rate,
                max_block_size,
            ) {
                RestoreOutcome::Ready => report.restored += 1,
                RestoreOutcome::Missing => {
                    report.failed += 1;
                    report.warnings.push(format!(
                        "Missing plugin on {}: {}",
                        target.track_name, target.display_name
                    ));
                }
                RestoreOutcome::Failed | RestoreOutcome::Timeout => {
                    report.failed += 1;
                    report.warnings.push(format!(
                        "Failed to restore {} on {}",
                        target.display_name, target.track_name
                    ));
                }
                RestoreOutcome::Pending => {
                    match wait_for_bridge_target(&runtime, &mut timeline_state, &target.slot_id) {
                        RestoreOutcome::Ready => report.restored += 1,
                        RestoreOutcome::Missing => {
                            report.failed += 1;
                            report.warnings.push(format!(
                                "Missing plugin on {}: {}",
                                target.track_name, target.display_name
                            ));
                        }
                        _ => {
                            report.failed += 1;
                            report.warnings.push(format!(
                                "Failed to restore {} on {}",
                                target.display_name, target.track_name
                            ));
                        }
                    }
                }
            }
        }

        sync_bridge_sinks(&engine, &runtime, &timeline_state, "pre_studio_install");
    }

    progress("Rebuilding audio graph", ProgressBarValue::value(0.82));

    let sample_rate = engine.config().sample_rate;
    let project_root = package.path.parent().and_then(|path| path.to_str());
    let snapshot = build_engine_project_snapshot(&timeline_state, sample_rate, project_root, None);
    engine
        .load_project(snapshot)
        .map_err(|error| format!("engine load_project failed: {error}"))?;

    progress("Preparing mixer navigator", ProgressBarValue::value(0.92));

    let output_channels = default_output_channels(&engine, &output_device_name);
    crate::components::mixer_tree_model::ensure_timeline_mixer_tree_defaults(
        &mut timeline_state,
        output_channels,
    );

    progress("Finalizing session", ProgressBarValue::value(0.95));
    eprintln!("[SessionLoad] ready");

    let handoff = SessionInstallHandoff {
        engine,
        engine_stats: stats,
        bridge_runtime: bridge_slot,
        timeline_state,
    };
    Ok((handoff, report))
}

fn collect_restore_targets(state: &TimelineState) -> Vec<RestoreTarget> {
    let mut targets = Vec::new();
    for track in &state.tracks {
        let is_instrument_track =
            matches!(track.track_type, TrackType::Instrument | TrackType::Midi);
        for (index, slot) in track.inserts.iter().enumerate() {
            if slot.plugin_id.as_deref() == Some(STUB_PLUGIN_ID) {
                continue;
            }
            let is_instrument = is_instrument_track
                && (track.instrument_plugin_instance_id.as_deref() == Some(slot.id.as_str())
                    || index == 0);
            if let Some(target) = target_from_slot(&track.id, &track.name, slot, is_instrument) {
                targets.push(target);
            }
        }
    }
    for slot in &state.master.inserts {
        if slot.plugin_id.as_deref() == Some(STUB_PLUGIN_ID) {
            continue;
        }
        if let Some(target) = target_from_slot(MASTER_TRACK_ID, "Master", slot, false) {
            targets.push(target);
        }
    }
    targets
}

fn target_from_slot(
    track_id: &str,
    track_name: &str,
    slot: &crate::components::timeline::timeline_state::InsertSlotState,
    _is_instrument: bool,
) -> Option<RestoreTarget> {
    let is_builtin = slot
        .plugin_id
        .as_deref()
        .is_some_and(SpherePluginHost::builtin_audio_bridge_supported);
    if !is_builtin && !slot.is_bridge_hosted_external_module() {
        return None;
    }
    Some(RestoreTarget {
        track_id: track_id.to_string(),
        slot_id: slot.id.clone(),
        display_name: slot.display_name.clone(),
        track_name: track_name.to_string(),
    })
}

fn restore_bridge_target(
    timeline_state: &mut TimelineState,
    runtime: &SharedPluginBridgeRuntime,
    target: &RestoreTarget,
    sample_rate: u32,
    max_block_size: u32,
) -> RestoreOutcome {
    let terminal = terminal_state(timeline_state, &target.track_id, &target.slot_id);
    if terminal != RestoreOutcome::Pending {
        return terminal;
    }

    let Some(slot) = timeline_state
        .find_insert_slot(&target.track_id, &target.slot_id)
        .cloned()
    else {
        return RestoreOutcome::Failed;
    };

    let is_builtin = slot
        .plugin_id
        .as_deref()
        .is_some_and(SpherePluginHost::builtin_audio_bridge_supported);
    let is_audio_unit = !is_builtin && slot.plugin_format == Some(InsertPluginFormat::Au);
    // Not there, or not a module this machine's plug-in catalog knows: a
    // project must not make Studio load a module from wherever it names.
    if let Some(reason) = crate::plugin_module_gate::module_refusal(
        is_builtin,
        slot.plugin_format,
        slot.plugin_path.as_deref(),
    ) {
        timeline_state.set_insert_runtime(
            &target.track_id,
            &target.slot_id,
            PluginRuntimeBackend::ExternalBridge,
            PluginRuntimeState::Missing(reason),
            None,
        );
        return RestoreOutcome::Missing;
    }

    let path_string = slot
        .plugin_path
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default();
    let descriptor = BridgePluginDescriptor {
        track_id: target.track_id.clone(),
        insert_id: target.slot_id.clone(),
        plugin_path: path_string,
        class_id: slot.plugin_id.clone().unwrap_or_default(),
        display_name: slot.display_name.clone(),
        format: slot.plugin_format.map(|f| f.label().to_string()),
    };

    let Ok(mut bridge) = runtime.lock() else {
        return RestoreOutcome::Failed;
    };
    let host_pid = bridge.host_pid();
    timeline_state.set_insert_runtime(
        &target.track_id,
        &target.slot_id,
        PluginRuntimeBackend::ExternalBridge,
        PluginRuntimeState::Loading,
        host_pid,
    );
    let load_result = if is_builtin {
        let state_json = slot
            .vst3_state
            .as_deref()
            .and_then(|bytes| String::from_utf8(bytes.to_vec()).ok());
        bridge.send_load_builtin_plugin(descriptor, sample_rate, max_block_size, state_json)
    } else if is_audio_unit {
        bridge.send_load_au_plugin(
            descriptor,
            sample_rate,
            max_block_size,
            slot.vst3_state.as_deref().map(|bytes| bytes.as_slice()),
        )
    } else {
        bridge.send_load_plugin(descriptor, sample_rate, max_block_size)
    };
    if let Err(error) = load_result {
        timeline_state.set_insert_runtime(
            &target.track_id,
            &target.slot_id,
            PluginRuntimeBackend::ExternalBridge,
            PluginRuntimeState::Failed(error.to_string()),
            host_pid,
        );
        return RestoreOutcome::Failed;
    }
    // Built-ins and Audio Units receive their state as part of the load so it
    // is applied before the instance is published to the audio producer.
    if !is_builtin && !is_audio_unit {
        if let Some(state) = slot.vst3_state.as_ref() {
            let _ = bridge.send_plugin_state(&target.slot_id, state);
        }
    }
    drop(bridge);
    poll_bridge_events(runtime, timeline_state);
    terminal_state(timeline_state, &target.track_id, &target.slot_id)
}

fn wait_for_bridge_target(
    runtime: &SharedPluginBridgeRuntime,
    timeline_state: &mut TimelineState,
    slot_id: &str,
) -> RestoreOutcome {
    let deadline = Instant::now() + PLUGIN_RESTORE_TIMEOUT;
    while Instant::now() < deadline {
        std::thread::sleep(POLL_INTERVAL);
        poll_bridge_events(runtime, timeline_state);
        let owners = timeline_state.insert_owner_ids_containing(slot_id);
        for track_id in owners {
            let outcome = terminal_state(timeline_state, &track_id, slot_id);
            if outcome != RestoreOutcome::Pending {
                return outcome;
            }
        }
    }
    RestoreOutcome::Timeout
}

fn poll_bridge_events(runtime: &SharedPluginBridgeRuntime, timeline_state: &mut TimelineState) {
    use SpherePluginHost::ipc::HostEvent;
    use SpherePluginHost::plugin_host_client::ClientEvent;

    let Ok(mut bridge) = runtime.lock() else {
        return;
    };
    let events = bridge.drain_events();
    drop(bridge);

    for event in events {
        match event {
            ClientEvent::Host(HostEvent::PluginLoaded {
                plugin_instance_id, ..
            })
            | ClientEvent::Host(HostEvent::PluginAlreadyLoaded {
                plugin_instance_id, ..
            }) => {
                if let Ok(mut bridge) = runtime.lock() {
                    bridge.mark_plugin_loaded(&plugin_instance_id);
                }
                for track_id in timeline_state.insert_owner_ids_containing(&plugin_instance_id) {
                    let host_pid = runtime.lock().ok().and_then(|r| r.host_pid());
                    timeline_state.set_insert_runtime(
                        &track_id,
                        &plugin_instance_id,
                        PluginRuntimeBackend::ExternalBridge,
                        PluginRuntimeState::Active,
                        host_pid,
                    );
                }
            }
            ClientEvent::Host(HostEvent::PluginLoadFailed {
                plugin_instance_id,
                error,
            }) => {
                if let Ok(mut bridge) = runtime.lock() {
                    bridge.mark_plugin_load_failed(&plugin_instance_id);
                }
                for track_id in timeline_state.insert_owner_ids_containing(&plugin_instance_id) {
                    let host_pid = runtime.lock().ok().and_then(|r| r.host_pid());
                    timeline_state.set_insert_runtime(
                        &track_id,
                        &plugin_instance_id,
                        PluginRuntimeBackend::ExternalBridge,
                        PluginRuntimeState::Failed(error.clone()),
                        host_pid,
                    );
                }
            }
            ClientEvent::Host(HostEvent::ProcessingPrepared {
                plugin_instance_id,
                output_channels,
                output_bus_channels,
                ..
            }) => {
                for track_id in timeline_state.insert_owner_ids_containing(&plugin_instance_id) {
                    let host_pid = runtime.lock().ok().and_then(|r| r.host_pid());
                    timeline_state.set_insert_runtime(
                        &track_id,
                        &plugin_instance_id,
                        PluginRuntimeBackend::ExternalBridge,
                        PluginRuntimeState::Active,
                        host_pid,
                    );
                    // Capture the real per-bus output layout from the restored
                    // plugin. This event is drained HERE during project restore,
                    // so the studio event pump never sees it — without recording
                    // the layout now, multi-out plugins loaded from a saved
                    // project would show only "Main 1/2" and never build their
                    // per-bus child strips. Layout must be set before
                    // auto_enable so child-strip creation sees the bus counts.
                    timeline_state.set_insert_output_bus_layout(
                        &track_id,
                        &plugin_instance_id,
                        &output_bus_channels,
                    );
                    timeline_state.auto_enable_detected_insert_outputs(
                        &track_id,
                        &plugin_instance_id,
                        output_channels,
                    );
                }
            }
            _ => {}
        }
    }
}

fn terminal_state(timeline_state: &TimelineState, track_id: &str, slot_id: &str) -> RestoreOutcome {
    let Some(slot) = timeline_state.find_insert_slot(track_id, slot_id) else {
        return RestoreOutcome::Failed;
    };
    match &slot.runtime_state {
        PluginRuntimeState::Active
        | PluginRuntimeState::Loaded
        | PluginRuntimeState::Ready
        | PluginRuntimeState::EditorOpen
        | PluginRuntimeState::EditorClosed => RestoreOutcome::Ready,
        PluginRuntimeState::Missing(_) => RestoreOutcome::Missing,
        PluginRuntimeState::Failed(_) => RestoreOutcome::Failed,
        PluginRuntimeState::Loading | PluginRuntimeState::NotLoaded => RestoreOutcome::Pending,
        _ => RestoreOutcome::Pending,
    }
}

fn sync_bridge_sinks(
    engine: &DirectAudio::AudioEngine,
    runtime: &SharedPluginBridgeRuntime,
    timeline_state: &TimelineState,
    reason: &'static str,
) {
    let slots = bridge_hosted_insert_slots(timeline_state);
    let Ok(runtime) = runtime.lock() else {
        return;
    };
    for (track_id, insert_id) in slots {
        let Some(sink) = runtime.audio_sink_for(&insert_id) else {
            continue;
        };
        if engine
            .set_plugin_bridge_sink(insert_id.clone(), Some(sink))
            .is_ok()
        {
            eprintln!(
                "[PluginRestore] bridge registered instance={insert_id} track={track_id} source={reason}"
            );
        }
    }
}

fn bridge_hosted_insert_slots(state: &TimelineState) -> Vec<(String, String)> {
    let mut slots: Vec<(String, String)> = state
        .tracks
        .iter()
        .flat_map(|track| {
            track
                .inserts
                .iter()
                .filter(|slot| slot.is_bridge_hosted_external_module())
                .map(|slot| (track.id.clone(), slot.id.clone()))
        })
        .collect();
    slots.extend(
        state
            .master
            .inserts
            .iter()
            .filter(|slot| slot.is_bridge_hosted_external_module())
            .map(|slot| (MASTER_TRACK_ID.to_string(), slot.id.clone())),
    );
    slots
}

fn default_output_channels(engine: &DirectAudio::AudioEngine, wanted_device: &str) -> u32 {
    // CoreAudio's device capability query can block in the HAL on macOS when
    // a remote session or another DAW owns the audio session. A new workspace
    // can safely use the conventional stereo master layout and let an
    // explicit Settings refresh discover the actual endpoint later.
    #[cfg(target_os = "macos")]
    {
        let _ = (engine, wanted_device);
        return 2;
    }

    #[cfg(not(target_os = "macos"))]
    {
        let wanted = wanted_device.trim();
        let devices = engine.list_output_devices();
        if !wanted.is_empty() {
            if let Some(device) = devices.iter().find(|d| d.name == wanted || d.id == wanted) {
                return device.channels;
            }
        }
        devices
            .iter()
            .find(|d| d.is_default)
            .or_else(|| devices.first())
            .map(|d| d.channels)
            .unwrap_or(2)
    }
}

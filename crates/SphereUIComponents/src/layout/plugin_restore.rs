//! Awaitable plugin/VST restore during the session install transaction.

use std::time::{Duration, Instant};

use gpui::{App, Context};

use super::StudioLayout;
use crate::components::progress_dialog::ProgressBarValue;
use crate::components::timeline::timeline_state::{
    PluginRuntimeBackend, PluginRuntimeState, TrackType, MASTER_TRACK_ID,
};

const PLUGIN_RESTORE_TIMEOUT: Duration = Duration::from_secs(120);
const AUDIO_ENGINE_WAIT: Duration = Duration::from_secs(30);
const GRAPH_SYNC_WAIT: Duration = Duration::from_secs(20);
const POLL_INTERVAL: Duration = Duration::from_millis(25);

#[derive(Debug, Clone)]
pub(super) struct PluginRestoreTarget {
    pub track_id: String,
    pub slot_id: String,
    pub display_name: String,
    pub track_name: String,
    pub is_instrument: bool,
}

#[derive(Debug, Clone, Default)]
pub(super) struct PluginRestoreReport {
    pub warnings: Vec<String>,
    pub restored: usize,
    pub failed: usize,
    pub skipped: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PluginRestoreWaitOutcome {
    Pending,
    Ready,
    Failed,
    Missing,
    Timeout,
    Disconnected,
}

impl StudioLayout {
    pub(super) fn set_session_install_progress(
        &mut self,
        detail: impl Into<String>,
        progress: ProgressBarValue,
        cx: &mut Context<Self>,
    ) {
        self.session_install_detail = detail.into();
        self.session_install_progress = progress;
        cx.notify();
    }

    pub(super) fn collect_plugin_restore_targets(&self, cx: &App) -> Vec<PluginRestoreTarget> {
        use crate::components::plugin_picker::STUB_PLUGIN_ID;

        let state = &self.timeline.read(cx).state;
        let mut targets = Vec::new();

        for track in &state.tracks {
            let track_name = track.name.clone();
            let is_instrument_track =
                matches!(track.track_type, TrackType::Instrument | TrackType::Midi);
            for (index, slot) in track.inserts.iter().enumerate() {
                if slot.plugin_id.as_deref() == Some(STUB_PLUGIN_ID) {
                    continue;
                }
                let is_instrument = is_instrument_track
                    && (track.instrument_plugin_instance_id.as_deref() == Some(slot.id.as_str())
                        || index == 0);
                if let Some(target) = target_from_slot(&track.id, &track_name, slot, is_instrument)
                {
                    targets.push(target);
                }
            }
        }

        let master_name = "Master".to_string();
        for slot in &state.master.inserts {
            if slot.plugin_id.as_deref() == Some(STUB_PLUGIN_ID) {
                continue;
            }
            if let Some(target) = target_from_slot(MASTER_TRACK_ID, &master_name, slot, false) {
                targets.push(target);
            }
        }

        targets
    }

    pub(super) fn begin_async_plugin_restore_and_finalize(
        &mut self,
        package: crate::loading_session::LoadedSessionPackage,
        cx: &mut Context<Self>,
    ) {
        self.validate_session_references(cx);
        self.update_virtual_keyboard_target_status(cx);
        self.schedule_loaded_project_waveforms(&package, cx);
        self.mark_engine_media_dirty();
        self.set_session_install_progress("Preparing session", ProgressBarValue::value(0.15), cx);

        let package = package;
        let entity = cx.entity().clone();
        cx.spawn(async move |_this, mut cx| {
            if !wait_until(&mut cx, &entity, AUDIO_ENGINE_WAIT, |layout| {
                layout.audio_bridge.engine.is_some()
            })
            .await
            {
                let _ = entity.update(cx, |layout, cx| {
                    layout.finish_session_install_with_report(
                        package,
                        PluginRestoreReport {
                            warnings: vec![
                                "Audio engine was not ready in time; some plugins may still be loading."
                                    .to_string(),
                            ],
                            ..PluginRestoreReport::default()
                        },
                        cx,
                    );
                });
                return;
            }

            // ── Dispatch the whole batch ────────────────────────────────
            //
            // Every insert is an independent instance in the plugin host, and
            // asking it to load one is a one-way message — so there was never
            // anything to gain by waiting for one before requesting the next,
            // and a great deal to lose. The loop used to `await` each plugin to
            // a terminal state before it *dispatched* the one behind it, which
            // made opening a project cost the sum of its plugins' load times
            // instead of the longest of them, and left the host idle between
            // each one. Worse, a plugin that never answered held its whole
            // `PLUGIN_RESTORE_TIMEOUT` against everything behind it: two silent
            // plugins in a project meant four minutes before the window opened,
            // and the warning dialog at the end could not say which wait had
            // cost what.
            //
            // The batch now goes to the host in one pass, and the wait below is
            // one shared deadline over whatever has not answered yet.
            let (targets, dispatched) = entity.update(cx, |layout, cx| {
                layout.set_session_install_progress(
                    "Loading Plugins",
                    ProgressBarValue::value(0.2),
                    cx,
                );
                layout.prepare_bridge_plugin_restore_batch(cx);
                if !super::plugin_bridge_runtime::bridge_enabled() {
                    layout.schedule_audio_project_sync(cx, true, "session_install_in_process");
                }
                let targets = layout.collect_plugin_restore_targets(cx);

                // Each load would otherwise force its own engine graph rebuild.
                // The final sync after the wait covers the whole batch.
                layout.plugin_restore_batch_active = true;
                let mut dispatched = Vec::with_capacity(targets.len());
                for target in &targets {
                    let outcome = layout.restore_one_plugin_target(target, cx);
                    let host_gone = outcome == PluginRestoreWaitOutcome::Disconnected;
                    dispatched.push(outcome);
                    if host_gone {
                        // Nothing behind this can load either.
                        break;
                    }
                }
                layout.plugin_restore_batch_active = false;
                (targets, dispatched)
            });

            let total = targets.len().max(1);
            let mut report = PluginRestoreReport::default();

            // ── Retire what already has an answer ───────────────────────────
            let mut waiting: Vec<&PluginRestoreTarget> = Vec::new();
            for (target, outcome) in targets.iter().zip(dispatched.iter().copied()) {
                if outcome == PluginRestoreWaitOutcome::Pending {
                    waiting.push(target);
                } else {
                    record_outcome(&mut report, target, outcome);
                }
            }
            // A host that went away mid-dispatch left the rest unrequested.
            report.skipped += targets.len() - dispatched.len();

            // ── Wait for the rest, all on one deadline ──────────────────────
            //
            // They are loading concurrently, so the budget a single plugin used
            // to get is the right budget for the batch: the longest one sets the
            // wall clock, and a plugin that never answers no longer costs the
            // others anything.
            let deadline = Instant::now() + PLUGIN_RESTORE_TIMEOUT;
            // The bar is repainted when the count moves, not on every tick: the
            // poll runs 40 times a second and `set_session_install_progress`
            // notifies, which would put the loading dialog through a repaint per
            // poll for as long as the slowest plugin takes.
            let mut reported_done = usize::MAX;
            while !waiting.is_empty() && Instant::now() < deadline {
                let done = total - waiting.len();
                if done != reported_done {
                    reported_done = done;
                    let detail = match waiting.first() {
                        Some(next) if waiting.len() == 1 => format!(
                            "Loading Plugin {}/{}: {} on {}",
                            done + 1,
                            total,
                            next.display_name,
                            next.track_name
                        ),
                        _ => format!("Loading Plugins: {done} of {total} ready"),
                    };
                    let progress = 0.2 + (0.55 * (done as f32) / total as f32);
                    let _ = entity.update(cx, |layout, cx| {
                        layout.set_session_install_progress(
                            detail,
                            ProgressBarValue::value(progress),
                            cx,
                        );
                    });
                }

                cx.background_executor().timer(POLL_INTERVAL).await;

                // One drain of the host's event queue answers for every slot in
                // the batch; polling each slot separately would drain it once
                // per waiting plugin.
                let outcomes = entity.update(cx, |layout, cx| {
                    layout.poll_plugin_bridge_runtime(cx);
                    waiting
                        .iter()
                        .map(|target| layout.plugin_restore_terminal_state_any_owner(&target.slot_id, cx))
                        .collect::<Vec<_>>()
                });

                let mut still_waiting = Vec::with_capacity(waiting.len());
                for (target, outcome) in waiting.iter().copied().zip(outcomes) {
                    if outcome == PluginRestoreWaitOutcome::Pending {
                        still_waiting.push(target);
                    } else {
                        record_outcome(&mut report, target, outcome);
                    }
                }
                waiting = still_waiting;
            }

            // Whatever is still pending ran out the batch's clock. Say so:
            // "timed out" and "failed to load" are different problems, and the
            // dialog was calling both of them the same thing.
            for target in waiting {
                record_outcome(&mut report, target, PluginRestoreWaitOutcome::Timeout);
            }

            let _ = entity.update(cx, |layout, cx| {
                layout.set_session_install_progress(
                    "Rebuilding audio graph",
                    ProgressBarValue::value(0.82),
                    cx,
                );
                layout.sync_plugin_bridge_sinks_to_engine(cx, "session_install_restore");
                layout.schedule_audio_project_sync(cx, true, "session_install_restore");
            });

            let _ = wait_until(&mut cx, &entity, GRAPH_SYNC_WAIT, |layout| {
                !layout.audio_bridge.project_dirty
                    && !layout.audio_bridge.media_dirty
                    && !layout.audio_bridge.sync_in_flight
            })
            .await;

            let _ = entity.update(cx, |layout, cx| {
                layout.set_session_install_progress(
                    "Finalizing session",
                    ProgressBarValue::value(0.95),
                    cx,
                );
                layout.finish_session_install_with_report(package, report, cx);
            });
        })
        .detach();
    }

    fn restore_one_plugin_target(
        &mut self,
        target: &PluginRestoreTarget,
        cx: &mut Context<Self>,
    ) -> PluginRestoreWaitOutcome {
        if super::plugin_bridge_runtime::bridge_enabled() {
            return self.restore_bridge_target(target, cx);
        }
        self.restore_in_process_target(target, cx)
    }

    fn restore_bridge_target(
        &mut self,
        target: &PluginRestoreTarget,
        cx: &mut Context<Self>,
    ) -> PluginRestoreWaitOutcome {
        let terminal = self.plugin_restore_terminal_state(&target.track_id, &target.slot_id, cx);
        if terminal.is_ready() {
            return terminal;
        }

        let slot = self
            .timeline
            .read(cx)
            .state
            .find_insert_slot(&target.track_id, &target.slot_id)
            .cloned();
        let Some(slot) = slot else {
            return PluginRestoreWaitOutcome::Failed;
        };

        let is_builtin = slot
            .plugin_id
            .as_deref()
            .is_some_and(SpherePluginHost::builtin_audio_bridge_supported);
        // An Audio Unit has no module file to find; the host's instantiate
        // reports whether the component is installed.
        let has_module_file = slot
            .plugin_format
            .is_none_or(|format| format.has_module_file());
        if !is_builtin && has_module_file && !slot.plugin_path.as_ref().is_some_and(|p| p.exists())
        {
            let reason = slot
                .plugin_path
                .as_ref()
                .map(|p| format!("Plugin file not found: {}", p.display()))
                .unwrap_or_else(|| "Plugin file not found".to_string());
            let _ = self.timeline.update(cx, |timeline, _cx| {
                timeline.state.set_insert_runtime(
                    &target.track_id,
                    &target.slot_id,
                    PluginRuntimeBackend::ExternalBridge,
                    PluginRuntimeState::Missing(reason),
                    None,
                );
            });
            return PluginRestoreWaitOutcome::Missing;
        }

        if self.load_bridge_insert_for_slot(&target.track_id, &target.slot_id, cx) {
            self.poll_plugin_restore_terminal(&target.slot_id, cx)
        } else {
            PluginRestoreWaitOutcome::Failed
        }
    }

    fn restore_in_process_target(
        &mut self,
        target: &PluginRestoreTarget,
        _cx: &mut Context<Self>,
    ) -> PluginRestoreWaitOutcome {
        let Some(engine) = self.audio_bridge.engine.as_ref() else {
            return PluginRestoreWaitOutcome::Pending;
        };
        let statuses = engine.insert_statuses();
        if let Some(st) = statuses.iter().find(|st| st.insert_id == target.slot_id) {
            if st.ready {
                return PluginRestoreWaitOutcome::Ready;
            }
            return PluginRestoreWaitOutcome::Failed;
        }
        PluginRestoreWaitOutcome::Pending
    }

    pub(super) fn poll_plugin_restore_terminal(
        &mut self,
        slot_id: &str,
        cx: &mut Context<Self>,
    ) -> PluginRestoreWaitOutcome {
        self.poll_plugin_bridge_runtime(cx);
        self.plugin_restore_terminal_state_any_owner(slot_id, cx)
    }

    /// The slot's terminal state without draining the host's event queue.
    ///
    /// Separate from [`Self::poll_plugin_restore_terminal`] because a batch
    /// wait drains once and then asks about every slot it is waiting on; going
    /// through the polling form would drain the queue once per waiting plugin,
    /// every tick.
    pub(super) fn plugin_restore_terminal_state_any_owner(
        &self,
        slot_id: &str,
        cx: &App,
    ) -> PluginRestoreWaitOutcome {
        let owners = self
            .timeline
            .read(cx)
            .state
            .insert_owner_ids_containing(slot_id);
        for track_id in owners {
            let outcome = self.plugin_restore_terminal_state(&track_id, slot_id, cx);
            if outcome != PluginRestoreWaitOutcome::Pending {
                return outcome;
            }
        }
        PluginRestoreWaitOutcome::Pending
    }

    fn plugin_restore_terminal_state(
        &self,
        track_id: &str,
        slot_id: &str,
        cx: &App,
    ) -> PluginRestoreWaitOutcome {
        let Some(slot) = self
            .timeline
            .read(cx)
            .state
            .find_insert_slot(track_id, slot_id)
        else {
            return PluginRestoreWaitOutcome::Failed;
        };
        match &slot.runtime_state {
            PluginRuntimeState::Active
            | PluginRuntimeState::Loaded
            | PluginRuntimeState::Ready
            | PluginRuntimeState::EditorOpen
            | PluginRuntimeState::EditorClosed => PluginRestoreWaitOutcome::Ready,
            PluginRuntimeState::Missing(_) => PluginRestoreWaitOutcome::Missing,
            PluginRuntimeState::Failed(_) => PluginRestoreWaitOutcome::Failed,
            PluginRuntimeState::Loading | PluginRuntimeState::NotLoaded => {
                PluginRestoreWaitOutcome::Pending
            }
            _ => PluginRestoreWaitOutcome::Pending,
        }
    }

    pub(super) fn finish_session_install_with_report(
        &mut self,
        package: crate::loading_session::LoadedSessionPackage,
        report: PluginRestoreReport,
        cx: &mut Context<Self>,
    ) {
        self.session_install_warnings = report.warnings.clone();
        self.session_install_status = crate::app_state::SessionInstallStatus::Ready;
        self.project_state = if self.project_session.project_file_path.is_some() {
            crate::app_state::ProjectState::SavedProject { path: package.path }
        } else {
            crate::app_state::ProjectState::UnsavedWorkspace
        };
        self.session_install_detail.clear();
        self.session_install_progress = ProgressBarValue::value(1.0);

        session_log!(
            "install complete plugins restored={} failed={} warnings={}",
            report.restored,
            report.failed,
            report.warnings.len()
        );

        if !report.warnings.is_empty() {
            for warning in &report.warnings {
                eprintln!("[PluginRestore] warning: {warning}");
            }
            self.queue_session_load_warning_dialog(report.warnings, cx);
        }

        cx.notify();
    }

    pub(super) fn queue_session_load_warning_dialog(
        &mut self,
        warnings: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        use crate::components::message_box_dialog::{MessageBoxKind, MessageBoxOptions};
        use std::sync::Arc;

        let summary = if warnings.len() == 1 {
            warnings[0].clone()
        } else {
            format!(
                "{} plugin restore warning(s):\n\n{}",
                warnings.len(),
                warnings
                    .iter()
                    .take(8)
                    .map(|w| format!("• {w}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        };
        let owner_bounds = self.studio_window_bounds(cx);
        let options = MessageBoxOptions {
            kind: MessageBoxKind::Warning,
            title: "Project Loaded With Warnings".to_string(),
            message: summary,
            detail: None,
            buttons: vec!["OK".to_string()],
            default_id: 0,
            cancel_id: None,
        };
        let _ = crate::components::message_box_dialog::open_message_box_window(
            owner_bounds,
            options,
            Arc::new(|_result, _window, _cx| {}),
            cx,
        );
    }
}

fn target_from_slot(
    track_id: &str,
    track_name: &str,
    slot: &crate::components::timeline::timeline_state::InsertSlotState,
    is_instrument: bool,
) -> Option<PluginRestoreTarget> {
    let is_builtin = slot
        .plugin_id
        .as_deref()
        .is_some_and(SpherePluginHost::builtin_audio_bridge_supported);
    if !is_builtin && !slot.is_bridge_hosted_external_module() {
        return None;
    }
    Some(PluginRestoreTarget {
        track_id: track_id.to_string(),
        slot_id: slot.id.clone(),
        display_name: slot.display_name.clone(),
        track_name: track_name.to_string(),
        is_instrument,
    })
}

/// Fold one plugin's terminal outcome into the report.
///
/// Both phases of the restore end here, so a plugin that answered on dispatch
/// and one that answered while the batch was being waited on are reported the
/// same way — they are the same event, only observed at different moments.
///
/// `Pending` is the caller's business: it means "still waiting", which is not
/// an outcome and must never be recorded as one.
fn record_outcome(
    report: &mut PluginRestoreReport,
    target: &PluginRestoreTarget,
    outcome: PluginRestoreWaitOutcome,
) {
    match outcome {
        PluginRestoreWaitOutcome::Ready => {
            report.restored += 1;
        }
        PluginRestoreWaitOutcome::Missing => {
            report.failed += 1;
            report.warnings.push(format!(
                "Missing plugin on {}: {}",
                target.track_name, target.display_name
            ));
        }
        // A plugin the host answered about, and could not load.
        PluginRestoreWaitOutcome::Failed => {
            report.failed += 1;
            report.warnings.push(format!(
                "Failed to restore {} on {}",
                target.display_name, target.track_name
            ));
        }
        // A plugin the host never answered about at all. Worth its own wording:
        // the dialog used to call this "failed to restore" too, which sent the
        // reader looking for a broken plugin when the real story is a host that
        // went quiet — a different problem with a different fix.
        PluginRestoreWaitOutcome::Timeout => {
            report.failed += 1;
            report.warnings.push(format!(
                "Timed out restoring {} on {} after {}s",
                target.display_name,
                target.track_name,
                PLUGIN_RESTORE_TIMEOUT.as_secs()
            ));
        }
        PluginRestoreWaitOutcome::Disconnected => {
            report.failed += 1;
            report
                .warnings
                .push("Plugin bridge host disconnected during restore.".to_string());
        }
        PluginRestoreWaitOutcome::Pending => {
            debug_assert!(false, "Pending is not a terminal outcome");
        }
    }
}

async fn wait_until(
    cx: &mut gpui::AsyncApp,
    entity: &gpui::Entity<StudioLayout>,
    timeout: Duration,
    mut predicate: impl FnMut(&StudioLayout) -> bool,
) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let ready = entity.update(cx, |layout, _cx| predicate(layout));
        if ready {
            return true;
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
    false
}

impl PluginRestoreWaitOutcome {
    fn is_ready(self) -> bool {
        matches!(self, PluginRestoreWaitOutcome::Ready)
    }
}

macro_rules! session_log {
    ($($arg:tt)*) => {
        eprintln!("[SessionLoad] {}", format!($($arg)*))
    };
}
use session_log;

#[cfg(test)]
mod tests {
    use super::*;

    fn target(name: &str, track: &str) -> PluginRestoreTarget {
        PluginRestoreTarget {
            track_id: format!("track-{track}"),
            slot_id: format!("slot-{name}"),
            display_name: name.to_string(),
            track_name: track.to_string(),
            is_instrument: false,
        }
    }

    #[test]
    fn a_restored_plugin_is_counted_and_says_nothing() {
        let mut report = PluginRestoreReport::default();
        record_outcome(
            &mut report,
            &target("OTT", "Audio 1"),
            PluginRestoreWaitOutcome::Ready,
        );
        assert_eq!(report.restored, 1);
        assert_eq!(report.failed, 0);
        assert!(
            report.warnings.is_empty(),
            "a plugin that loaded is not a warning"
        );
    }

    #[test]
    fn a_timeout_reads_differently_from_a_failure() {
        // The dialog in the bug report said "Failed to restore ..." for both,
        // which is the one thing it could say that does not help: a plugin that
        // refused to load and a host that never answered need different fixes.
        let mut failed = PluginRestoreReport::default();
        record_outcome(
            &mut failed,
            &target("Auto-Tune Pro", "Audio 1"),
            PluginRestoreWaitOutcome::Failed,
        );
        let mut timed_out = PluginRestoreReport::default();
        record_outcome(
            &mut timed_out,
            &target("Auto-Tune Pro", "Audio 1"),
            PluginRestoreWaitOutcome::Timeout,
        );

        assert_eq!(
            failed.warnings,
            vec!["Failed to restore Auto-Tune Pro on Audio 1"]
        );
        assert_eq!(
            timed_out.warnings,
            vec![format!(
                "Timed out restoring Auto-Tune Pro on Audio 1 after {}s",
                PLUGIN_RESTORE_TIMEOUT.as_secs()
            )]
        );
        assert_eq!(failed.failed, 1);
        assert_eq!(timed_out.failed, 1);
    }

    #[test]
    fn a_missing_plugin_names_the_track_it_was_on() {
        let mut report = PluginRestoreReport::default();
        record_outcome(
            &mut report,
            &target("PSE Stereo", "Audio 1"),
            PluginRestoreWaitOutcome::Missing,
        );
        assert_eq!(
            report.warnings,
            vec!["Missing plugin on Audio 1: PSE Stereo"]
        );
        assert_eq!(report.failed, 1);
    }

    #[test]
    fn a_disconnected_host_reports_itself_not_the_plugin() {
        let mut report = PluginRestoreReport::default();
        record_outcome(
            &mut report,
            &target("Saturation Knob", "Audio 1"),
            PluginRestoreWaitOutcome::Disconnected,
        );
        assert_eq!(
            report.warnings,
            vec!["Plugin bridge host disconnected during restore."],
            "the plugin is not what went wrong"
        );
    }

    #[test]
    fn a_whole_batch_folds_into_one_report() {
        // Both phases of the restore record through here, so a mixed batch has
        // to add up whichever moment each plugin answered at.
        let targets = [
            target("OTT", "Audio 1"),
            target("PSE Stereo", "Audio 1"),
            target("Auto-Tune Pro", "Audio 1"),
            target("Saturation Knob", "Audio 2"),
        ];
        let outcomes = [
            PluginRestoreWaitOutcome::Ready,
            PluginRestoreWaitOutcome::Missing,
            PluginRestoreWaitOutcome::Timeout,
            PluginRestoreWaitOutcome::Ready,
        ];
        let mut report = PluginRestoreReport::default();
        for (target, outcome) in targets.iter().zip(outcomes) {
            record_outcome(&mut report, target, outcome);
        }

        assert_eq!(report.restored, 2);
        assert_eq!(report.failed, 2);
        assert_eq!(
            report.warnings.len(),
            report.failed,
            "every failure gets exactly one line in the dialog, and every \
             success gets none"
        );
    }
}

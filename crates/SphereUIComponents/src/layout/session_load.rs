//! Phased, crash-safe project-open transaction (the "Loading Session..." flow).
//!
//! Pre-studio decode/validate runs in [`crate::loading_session`] before
//! [`super::StudioLayout`] is mounted. Once a [`crate::loading_session::LoadedSessionPackage`]
//! is ready, this module installs it into a fresh studio workspace in one
//! synchronous pass (tracks, plugins, validation, waveforms) before the first
//! studio frame renders.
//!
//! In-studio project replacement keeps the root [`super::StudioLayout`] window
//! alive: quiesce the live session, show the in-window loading gate, decode on a
//! background thread, then install into the existing layout (rollback on failure).

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{BorrowAppContext, Context, Window, AppContext};

use crate::app_state::{AppMode, AppSessionGate, ProjectState, SessionInstallStatus};
use crate::loading_session::{
    LoadedSessionPackage, SessionInstallHandoff, SessionRollbackSnapshot,
};
use crate::project::io::{load_project_strict, validate_project_file};
use crate::project::{apply_to_timeline, now_secs};

/// Map a project-load warning onto the aggregated routing surface.
fn project_load_warning_to_routing_warning(
    warning: &crate::project::ProjectLoadWarning,
) -> crate::layout::routing_warnings::RoutingWarning {
    use crate::layout::routing_warnings::{RoutingWarning, RoutingWarningKind};
    use crate::project::ProjectLoadWarningKind;

    let kind = match warning.kind {
        ProjectLoadWarningKind::ConflictingLegacyMidiInput => {
            RoutingWarningKind::LegacyMidiConflict
        }
        ProjectLoadWarningKind::MissingAudioConnection => RoutingWarningKind::LegacyAudioMigration,
        ProjectLoadWarningKind::NoMasterOutput => RoutingWarningKind::MasterOutputUnavailable,
    };
    RoutingWarning::new(kind, warning.message.clone())
}
use crate::session_shutdown::{
    PluginUnloadTarget, SessionLifecycleStep, SessionShutdownReason, SessionShutdownSnapshot,
};
use crate::components::message_box_dialog::{
    open_message_box_window, MessageBoxKind, MessageBoxOptions, MessageBoxResponseCb, MessageBoxResult,
};

use super::project_ops::ProjectOpenOptions;
use super::{RecordingUiState, StudioLayout};

macro_rules! session_log {
    ($($arg:tt)*) => {
        eprintln!("[SessionLoad] {}", format!($($arg)*))
    };
}

/// Number of persisted project tracks in the live timeline, excluding runtime
/// VSTi multi-out child strips (`vsti-out:*`). Those are derived from a plugin's
/// reported output bus layout after load and are not part of the saved project,
/// so the post-restore track-count integrity check must ignore them.
fn persisted_track_count(
    tracks: &[crate::components::timeline::timeline_state::TrackState],
) -> usize {
    use crate::components::timeline::timeline_state::is_vsti_output_child_track_id;
    tracks
        .iter()
        .filter(|track| !is_vsti_output_child_track_id(&track.id))
        .count()
}

/// Same exclusion applied to the saved project rows. Child strips ARE saved
/// now (they carry substrip insert chains), but between load and this check
/// the plugin-prepare pass may legitimately retain, add, or remove them to
/// match the plugin's reported bus layout — so the integrity check compares
/// only the stable, non-derived tracks on both sides.
fn expected_persisted_track_count(tracks: &[crate::project::ProjectTrack]) -> usize {
    use crate::components::timeline::timeline_state::is_vsti_output_child_track_id;
    tracks
        .iter()
        .filter(|track| !is_vsti_output_child_track_id(&track.id))
        .count()
}

impl StudioLayout {
    /// Bump the session generation and return the new value. Call this at every
    /// point that tears down or replaces the live session (project reset,
    /// in-studio switch). A background project-load completion captures the
    /// value returned here at spawn time and rejects itself if
    /// [`Self::session_generation`] has since advanced — see
    /// [`Self::begin_in_studio_project_switch`]. Cheap (a `u64` increment); the
    /// generation is purely a staleness token, never persisted.
    pub(super) fn advance_session_generation(&mut self) -> u64 {
        self.session_generation = self.session_generation.wrapping_add(1);
        self.session_generation
    }

    /// Current session generation. Async work compares its captured value
    /// against this to detect that the session was replaced mid-flight.
    pub(super) fn session_generation(&self) -> u64 {
        self.session_generation
    }

    /// Capture the live session for rollback before an in-studio project swap.
    pub fn capture_session_rollback_snapshot(
        &self,
        cx: &mut Context<Self>,
    ) -> SessionRollbackSnapshot {
        SessionRollbackSnapshot {
            timeline_state: self.timeline.read(cx).state.clone(),
            session: self.project_session.clone(),
            project_state: self.project_state.clone(),
        }
    }

    /// Restore a rollback snapshot into a freshly mounted studio workspace.
    pub fn restore_session_rollback_snapshot(
        &mut self,
        snapshot: SessionRollbackSnapshot,
        cx: &mut Context<Self>,
    ) {
        session_log!("restoring rollback session: {}", snapshot.session.name);
        let _ = self.timeline.update(cx, |timeline, cx| {
            timeline.reset_input_state();
            timeline.state = snapshot.timeline_state;
            cx.notify();
        });
        self.project_session = snapshot.session;
        self.project_state = snapshot.project_state;
        self.sync_project_session_to_workspace(cx);
        self.session_install_status = crate::app_state::SessionInstallStatus::Ready;
        self.mark_engine_media_dirty();
        self.schedule_audio_project_sync(cx, true, "session_rollback_restore");
        cx.notify();
    }

    /// Install a decoded project into a new studio workspace before the first
    /// render. This is the only path that should load a saved project on a
    /// freshly mounted layout.
    pub fn install_loaded_session(
        &mut self,
        mut package: LoadedSessionPackage,
        cx: &mut Context<Self>,
    ) {
        session_log!(
            "install loaded session: {} ({})",
            package.project.name,
            package.path.display()
        );

        if let Some(handoff) = package.install_handoff.take() {
            self.install_loaded_session_with_handoff(package, handoff, cx);
            return;
        }

        self.session_install_status = crate::app_state::SessionInstallStatus::Loading;
        self.project_state = crate::app_state::ProjectState::Loading;

        if !self.apply_loaded_project_tracks(&package, cx) {
            self.session_install_status = crate::app_state::SessionInstallStatus::Failed;
            self.project_state = crate::app_state::ProjectState::Error(
                crate::i18n::I18n::from_app(cx).tr("project.error.restore-failed"),
            );
            session_log!("install failed: track integrity check");
            cx.notify();
            return;
        }

        self.begin_async_plugin_restore_and_finalize(package, cx);
    }

    fn install_loaded_session_with_handoff(
        &mut self,
        package: LoadedSessionPackage,
        handoff: SessionInstallHandoff,
        cx: &mut Context<Self>,
    ) {
        session_log!("install with pre-studio handoff");
        self.project_state = crate::app_state::ProjectState::Loading;

        self.adopt_session_install_handoff(handoff, cx);

        if !self.bind_loaded_project_session(&package, cx) {
            self.session_install_status = crate::app_state::SessionInstallStatus::Failed;
            self.project_state = crate::app_state::ProjectState::Error(
                crate::i18n::I18n::from_app(cx).tr("project.error.restore-failed"),
            );
            session_log!("install failed: track integrity check");
            cx.notify();
            return;
        }

        self.validate_session_references(cx);
        self.update_virtual_keyboard_target_status(cx);
        self.schedule_loaded_project_waveforms(&package, cx);

        self.session_install_warnings = package.restore_warnings.clone();
        self.session_install_status = crate::app_state::SessionInstallStatus::Ready;
        self.project_state = crate::app_state::ProjectState::SavedProject { path: package.path };
        self.session_install_detail.clear();
        self.session_install_progress =
            crate::components::progress_dialog::ProgressBarValue::value(1.0);

        session_log!(
            "install complete (pre-studio) warnings={}",
            self.session_install_warnings.len()
        );

        if !self.session_install_warnings.is_empty() {
            for warning in &self.session_install_warnings {
                eprintln!("[PluginRestore] warning: {warning}");
            }
            self.queue_session_load_warning_dialog(self.session_install_warnings.clone(), cx);
        }

        cx.notify();
    }

    fn bind_loaded_project_session(
        &mut self,
        package: &LoadedSessionPackage,
        cx: &mut Context<Self>,
    ) -> bool {
        let project = &package.project;
        let path = &package.path;
        let expected_tracks = expected_persisted_track_count(&project.tracks);

        // A project imported from another DAW's file has no Futureboard file to
        // save back into: bind it untitled and dirty so the first save is a
        // Save As and the imported file is never overwritten.
        if crate::project::is_import_path(path) {
            self.project_session
                .bind_untitled(project.name.clone(), true);
            session_log!("session bound from import: name={}", project.name);
        } else {
            let folder = path.parent().map(PathBuf::from);
            self.project_session.bind_saved(
                project.id.clone(),
                project.name.clone(),
                folder,
                path.clone(),
                project.created_at,
                project.modified_at,
            );
            session_log!(
                "session bound: name={} path={}",
                self.project_session.name,
                path.display()
            );
        }
        self.sync_project_session_to_workspace(cx);
        self.recent_projects
            .push(&project.name, path.clone(), now_secs());
        self.sync_recent_to_switcher();

        // Count only persisted project tracks. VSTi multi-out child strips
        // (`vsti-out:*`) are runtime-derived from the plugin's output bus layout
        // and are NOT part of the saved project, so they must never count toward
        // the integrity check — otherwise restoring a multi-out instrument fails
        // the check and the studio never mounts (blank screen).
        let restored_tracks = persisted_track_count(&self.timeline.read(cx).state.tracks);
        if restored_tracks != expected_tracks {
            session_log!(
                "integrity check failed: expected {expected_tracks} tracks, restored {restored_tracks}"
            );
            return false;
        }
        true
    }

    pub(super) fn adopt_session_install_handoff(
        &mut self,
        handoff: SessionInstallHandoff,
        cx: &mut Context<Self>,
    ) {
        let _ = self.timeline.update(cx, |timeline, cx| {
            timeline.reset_input_state();
            timeline.state = handoff.timeline_state;
            cx.notify();
        });

        if let Some(runtime) = handoff.bridge_runtime {
            self.plugin_editors.bridge_runtime = Some(runtime);
        }

        self.install_audio_callbacks(&handoff.engine, cx);
        self.audio_bridge.running = handoff.engine_stats.running;
        self.audio_bridge.stats = Some(handoff.engine_stats);
        self.audio_bridge.last_error = None;
        self.audio_bridge.engine = Some(handoff.engine);
        self.sync_plugin_bridge_sinks_to_engine(cx, "pre_studio_handoff");
        self.mark_engine_media_dirty();

        let output_channels = self.mixer_tree_output_channels(cx);
        self.timeline.update(cx, |timeline, _cx| {
            crate::components::mixer_tree_model::ensure_timeline_mixer_tree_defaults(
                &mut timeline.state,
                output_channels,
            );
        });
        self.mixer_view.tree_defaults_applied = true;
        self.invalidate_mixer_tree_model_cache();
        self.refresh_mixer_tree_sidebar_entity(cx);
    }

    pub fn load_project_from_path_with_options(
        &mut self,
        path: PathBuf,
        open_options: ProjectOpenOptions,
        cx: &mut Context<Self>,
    ) {
        if let Some(request_load) = self.window_hooks.on_request_project_load.clone() {
            request_load(path, open_options, cx);
            return;
        }
        session_log!(
            "on_request_project_load hook missing — cannot open {}",
            path.display()
        );
    }

    pub fn load_project_from_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.load_project_from_path_with_options(path, ProjectOpenOptions::default(), cx);
    }

    /// Quiesce the live session for an in-window project switch. Does not close
    /// or unhook the root studio window.
    pub fn prepare_for_in_studio_project_switch(&mut self, cx: &mut Context<Self>) -> usize {
        session_log!("prepare for in-studio project switch");
        let plugin_editors = self.plugin_editors.open.len() + self.plugin_editors.bridge.len();
        let midi_editor = usize::from(self.midi_editor.window.is_some());

        self.menu_bar.open_menu_id = None;
        self.menu_bar.submenu_path.clear();
        self.project_switcher.is_open = false;
        self.command_palette.close();
        self.overlay.text_context_menu = None;
        self.overlay.open_popover = None;

        if matches!(
            self.recording.ui_state,
            RecordingUiState::Recording
                | RecordingUiState::Preparing
                | RecordingUiState::CountingIn { .. }
                | RecordingUiState::Finalizing
        ) {
            self.stop_native_recording(cx);
        }
        self.stop_native_playback(cx);
        self.defer_panic_virtual_keyboard(cx);
        self.shutdown_plugin_editors(cx);
        if self.midi_editor.window.is_some() {
            self.close_midi_editor_window(cx);
        }

        plugin_editors + midi_editor
    }

    /// Replace the current project inside the existing studio window.
    pub fn begin_in_studio_project_switch(
        &mut self,
        path: PathBuf,
        open_options: ProjectOpenOptions,
        cx: &mut Context<Self>,
    ) {
        if let Some(request_load) = self.window_hooks.on_request_project_load.clone() {
            request_load(path, open_options, cx);
            return;
        }

        let root_alive = self.window_hooks.self_window.is_some();
        eprintln!("[ProjectSwitch] root window alive before switch={root_alive}");
        eprintln!("[ProjectSwitch] begin switch");
        let rollback = self.capture_session_rollback_snapshot(cx);
        let transient_count = self.prepare_for_in_studio_project_switch(cx);
        eprintln!("[ProjectSwitch] closing transient windows count={transient_count}");
        eprintln!("[ProjectSwitch] old session quiesced");

        // Stamp this switch. If another switch/reset replaces the session while
        // the decode is in flight, the generation advances and the completion
        // below rejects itself instead of installing a superseded project.
        let generation = self.advance_session_generation();

        self.session_install_status = SessionInstallStatus::Loading;
        self.project_state = ProjectState::Loading;
        cx.update_global::<AppSessionGate, _>(|gate, _| {
            eprintln!("[ProjectSwitch] entering LoadingSession mode");
            gate.mode = AppMode::LoadingSession;
        });
        cx.notify();

        let path_for_job = path.clone();
        let path_for_error = path.clone();
        let entity = cx.entity().clone();
        cx.spawn(async move |_this, cx| {
            eprintln!("[ProjectSwitch] loading target project");
            let decoded = cx
                .background_executor()
                .spawn(async move {
                    if !path_for_job.exists() {
                        return Err(LoadSwitchError::NotFound(path_for_job));
                    }
                    validate_project_file(&path_for_job).map_err(LoadSwitchError::Project)?;
                    load_project_strict(&path_for_job)
                        .map_err(LoadSwitchError::Project)
                        .map(|project| (project, path_for_job))
                })
                .await;

            let _ = entity.update(cx, |this, cx| {
                if this.session_generation() != generation {
                    eprintln!(
                        "[ProjectSwitch] stale switch completion ignored (generation {generation} != {})",
                        this.session_generation()
                    );
                    return;
                }
                match decoded {
                Ok((project, path)) => {
                    eprintln!("[ProjectSwitch] loaded target project");
                    eprintln!("[ProjectSwitch] installing session");
                    let failed_path = path.clone();
                    let package = LoadedSessionPackage {
                        project,
                        path,
                        open_options,
                        install_handoff: None,
                        restore_warnings: Vec::new(),
                    };
                    this.install_loaded_session(package, cx);
                    if this.session_install_status.is_failed() {
                        eprintln!("[ProjectSwitch] install failed — restoring rollback");
                        this.restore_session_rollback_snapshot(rollback, cx);
                        cx.update_global::<AppSessionGate, _>(|gate, _| {
                            gate.mode = AppMode::Studio
                        });
                        let i18n = crate::i18n::I18n::from_app(cx);
                        this.show_project_open_failed_dialog(
                            "Open Project Failed",
                            &i18n.tr("project.error.restore-session-failed"),
                            Some(i18n.tr("project.error.restore-failed")),
                            Some(failed_path),
                            open_options,
                            cx,
                        );
                    } else {
                        eprintln!(
                            "[ProjectSwitch] session install started — awaiting plugin restore"
                        );
                    }
                }
                Err(LoadSwitchError::NotFound(path)) => {
                    eprintln!(
                        "[ProjectSwitch] switch failed error=project not found: {}",
                        path.display()
                    );
                    let i18n = crate::i18n::I18n::from_app(cx);
                    this.finish_in_studio_switch_failure(
                        rollback,
                        "Open Project Failed",
                        &i18n.tr("project.error.file-not-found"),
                        Some(format!("Details: {}", path.display())),
                        Some(path),
                        open_options,
                        cx,
                    );
                }
                Err(LoadSwitchError::Project(e)) => {
                    eprintln!(
                        "[ProjectSwitch] switch failed error={}",
                        e.technical_detail()
                    );
                    // Check if this is an old version that can be loaded with a warning
                    if let crate::project::ProjectError::OldVersion(version) = &e {
                        // Show warning popup for old version
                        let version = *version;
                        let path = path_for_error.clone();
                        let open_options = open_options.clone();
                        let rollback = rollback.clone();
                        let entity = cx.entity().clone();
                        let _ = entity.update(cx, |this, cx| {
                            this.show_old_version_warning(version, path, open_options, rollback, cx);
                        });
                        return;
                    }
                    this.finish_in_studio_switch_failure(
                        rollback,
                        "Open Project Failed",
                        &e.user_message(),
                        Some(format!("Details: {}", e.technical_detail())),
                        Some(path_for_error),
                        open_options,
                        cx,
                    );
                }
                }
            });
        })
        .detach();
    }

    fn finish_in_studio_switch_failure(
        &mut self,
        rollback: SessionRollbackSnapshot,
        title: &str,
        message: &str,
        detail: Option<String>,
        path: Option<PathBuf>,
        open_options: ProjectOpenOptions,
        cx: &mut Context<Self>,
    ) {
        self.restore_session_rollback_snapshot(rollback, cx);
        cx.update_global::<AppSessionGate, _>(|gate, _| gate.mode = AppMode::Studio);
        let root_alive = self.window_hooks.self_window.is_some();
        eprintln!("[ProjectSwitch] root window alive after switch={root_alive}");
        eprintln!("[ProjectSwitch] notifying root window");
        self.show_project_open_failed_dialog(title, message, detail, path, open_options, cx);
        cx.notify();
    }

    /// Show a warning dialog for old project versions and re-attempt load if confirmed.
    fn show_old_version_warning(
        &mut self,
        version: u32,
        path: PathBuf,
        open_options: ProjectOpenOptions,
        rollback: SessionRollbackSnapshot,
        cx: &mut Context<Self>,
    ) {
        let version = version;
        let path = path.clone();
        let open_options = open_options.clone();
        let rollback = rollback.clone();
        let entity = cx.entity().clone();

        let owner_bounds = self.studio_window_bounds(cx);
        let options = MessageBoxOptions {
            kind: MessageBoxKind::Warning,
            title: "Old Project Version".to_string(),
            message: format!(
                "This project was created with Futureboard v{} (current is v{}).\n\
                 It can be opened, but some features may be missing or behave differently.\n\
                 The project will be upgraded to the current format when saved.",
                version, crate::project::format::PROJECT_VERSION
            ),
            detail: Some(format!(
                "Minimum supported version: v{}\n\
                 Your project version: v{}\n\
                 Current version: v{}",
                crate::project::format::MIN_SUPPORTED_VERSION, version, crate::project::format::PROJECT_VERSION
            )),
            buttons: vec!["Open Anyway".to_string(), "Cancel".to_string()],
            default_id: 0,
            cancel_id: Some(1),
        };

        let path = Arc::new(path.clone());
        let open_options = Arc::new(open_options.clone());
        let rollback = Arc::new(rollback.clone());
        let entity = entity.clone();
        let on_response: MessageBoxResponseCb = {
            let path = Arc::clone(&path);
            let open_options = Arc::clone(&open_options);
            let rollback = Arc::clone(&rollback);
            let entity = entity.clone();
            Arc::new(move |result, _window, app| {
                let path = Arc::clone(&path);
                let open_options = Arc::clone(&open_options);
                let rollback = Arc::clone(&rollback);
                let entity = entity.clone();
                if result.response == 0 {
                    eprintln!("[ProjectSwitch] User chose to open old version project anyway");
                    let _ = entity.update(app, move |this, cx| {
                        this.restore_session_rollback_snapshot((*rollback).clone(), cx);
                        cx.update_global::<AppSessionGate, _>(|gate, _| gate.mode = AppMode::Studio);
                        this.show_project_open_failed_dialog(
                            "Old Version Project",
                            "This project was created with an older version. Please use File > Open Project to reopen it.",
                            Some("The project will be upgraded to the current format when saved.".to_string()),
                            Some((*path).clone()),
                            (*open_options).clone(),
                            cx,
                        );
                    });
                } else {
                    let _ = entity.update(app, move |this, cx| {
                        this.restore_session_rollback_snapshot((*rollback).clone(), cx);
                        cx.update_global::<AppSessionGate, _>(|gate, _| gate.mode = AppMode::Studio);
                        this.show_project_open_failed_dialog(
                            "Open Project Cancelled",
                            "Opening the old version project was cancelled.",
                            None,
                            Some((*path).clone()),
                            (*open_options).clone(),
                            cx,
                        );
                    });
                }
            })
        };

        let owner_bounds = self.studio_window_bounds(cx);
        if let Err(err) = open_message_box_window(owner_bounds, options, on_response, cx) {
            eprintln!("[ProjectSwitch] failed to show old version warning: {}", err);
            self.restore_session_rollback_snapshot((*rollback).clone(), cx);
            cx.update_global::<AppSessionGate, _>(|gate, _| gate.mode = AppMode::Studio);
            let i18n = crate::i18n::I18n::from_app(cx);
            self.show_project_open_failed_dialog(
                "Open Project Failed",
                &i18n.tr("project.error.warning-dialog-failed"),
                Some(err.to_string()),
                Some((*path).clone()),
                (*open_options).clone(),
                cx,
            );
        }
    }

/// Prepare rollback for an in-studio project switch. Session shutdown runs
    /// asynchronously once the loading dialog is visible.
    pub fn prepare_for_in_studio_project_switch_transaction(
        &mut self,
        cx: &mut Context<Self>,
    ) -> (SessionRollbackSnapshot, Option<gpui::Bounds<gpui::Pixels>>) {
        session_log!("prepare for in-studio project switch transaction");
        eprintln!("[ProjectSwitch] close project switcher popover");
        self.menu_bar.open_menu_id = None;
        self.menu_bar.submenu_path.clear();
        self.project_switcher.is_open = false;
        self.command_palette.close();
        self.overlay.text_context_menu = None;
        self.overlay.open_popover = None;
        let rollback = self.capture_session_rollback_snapshot(cx);
        let owner_bounds = self.studio_window_bounds(cx);
        (rollback, owner_bounds)
    }

    /// Run one UI-thread session lifecycle step for the loading dialog.
    pub fn run_session_lifecycle_ui_step(
        &mut self,
        step: SessionLifecycleStep,
        clear_session_state: bool,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        eprintln!("[ProjectLifecycle] ui step begin: {}", step.label());
        let result = match step {
            SessionLifecycleStep::StopTransport => {
                self.prepare_immediate_session_shutdown(cx);
                Ok(())
            }
            SessionLifecycleStep::FlushAutosave => Ok(()),
            SessionLifecycleStep::StopAudioEngine => {
                if let Some(mut engine) = self.audio_bridge.engine.take() {
                    let _ = engine.stop();
                }
                self.audio_bridge.running = false;
                self.audio_bridge.stats = None;
                Ok(())
            }
            SessionLifecycleStep::StopWorkers => {
                self.cancel_session_background_workers(cx);
                Ok(())
            }
            SessionLifecycleStep::CloseFileWatchers => {
                eprintln!("[ProjectLifecycle] file watchers not configured");
                Ok(())
            }
            SessionLifecycleStep::ReleaseProjectResources => {
                self.teardown_all_plugin_instances(cx, "session_shutdown");
                Ok(())
            }
            SessionLifecycleStep::ClearSessionState => {
                if clear_session_state {
                    self.reset_project(cx);
                }
                Ok(())
            }
            SessionLifecycleStep::UnloadPlugins | SessionLifecycleStep::TerminatePluginHosts => {
                Ok(())
            }
        };
        match &result {
            Ok(()) => eprintln!("[ProjectLifecycle] ui step complete: {}", step.label()),
            Err(error) => {
                eprintln!(
                    "[ProjectLifecycle] ui step failed: {} error={error}",
                    step.label()
                );
            }
        }
        result
    }

    pub fn capture_session_shutdown_snapshot_for_loading(
        &mut self,
        reason: SessionShutdownReason,
        cx: &mut Context<Self>,
    ) -> SessionShutdownSnapshot {
        let (flush_autosave_path, flush_autosave_project) = self.session_autosave_flush_payload(cx);
        let mut snapshot = self.capture_session_shutdown_snapshot(reason, cx);
        snapshot.flush_autosave_path = flush_autosave_path;
        snapshot.flush_autosave_project = flush_autosave_project;
        snapshot
    }

    fn cancel_session_background_workers(&mut self, cx: &mut Context<Self>) {
        let ids: Vec<String> = self
            .background_tasks
            .tasks
            .iter()
            .filter(|(_, task)| {
                task.cancellable
                    && matches!(
                        task.status,
                        crate::components::background_tasks::BackgroundTaskStatus::Queued
                            | crate::components::background_tasks::BackgroundTaskStatus::Running
                            | crate::components::background_tasks::BackgroundTaskStatus::Paused
                    )
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in &ids {
            eprintln!("[ProjectLifecycle] cancel background worker id={id}");
            self.background_tasks.cancel(id);
        }
        if !ids.is_empty() {
            cx.notify();
        }
    }

    /// Tear down the live studio surface before a welcome-path project reload
    /// that closes and remounts the studio window.
    pub fn prepare_for_app_level_project_reload(
        &mut self,
        cx: &mut Context<Self>,
    ) -> (
        SessionRollbackSnapshot,
        Option<gpui::Bounds<gpui::Pixels>>,
        Option<gpui::WindowHandle<Self>>,
        SessionShutdownSnapshot,
    ) {
        session_log!("prepare for app-level project reload");
        self.prepare_immediate_session_shutdown(cx);
        let rollback = self.capture_session_rollback_snapshot(cx);
        let shutdown =
            self.capture_session_shutdown_snapshot(SessionShutdownReason::ProjectSwitch, cx);
        let owner_bounds = self.studio_window_bounds(cx);
        let self_window = self.window_hooks.self_window.take();
        (rollback, owner_bounds, self_window, shutdown)
    }

    /// Stop transport/recording and close transient UI before session shutdown.
    pub fn prepare_immediate_session_shutdown(&mut self, cx: &mut Context<Self>) {
        if matches!(
            self.recording.ui_state,
            super::RecordingUiState::Recording
                | super::RecordingUiState::Preparing
                | super::RecordingUiState::CountingIn { .. }
                | super::RecordingUiState::Finalizing
        ) {
            self.stop_native_recording(cx);
        }
        self.stop_native_playback(cx);
        self.defer_release_virtual_keyboard_notes(cx);
        self.shutdown_plugin_editors(cx);
        if self.midi_editor.window.is_some() {
            self.close_midi_editor_window(cx);
        }
    }

    /// Capture plugin/host state needed to shut down the session off the UI thread.
    pub fn capture_session_shutdown_snapshot(
        &mut self,
        reason: SessionShutdownReason,
        cx: &Context<Self>,
    ) -> SessionShutdownSnapshot {
        use crate::components::timeline::timeline_state::TrackType;

        let state = &self.timeline.read(cx).state;
        let mut plugin_targets = Vec::new();
        let mut instrument_track_ids = Vec::new();

        for track in &state.tracks {
            let is_instrument_track =
                matches!(track.track_type, TrackType::Instrument | TrackType::Midi);
            if is_instrument_track {
                instrument_track_ids.push(track.id.clone());
            }
            for (index, slot) in track.inserts.iter().enumerate() {
                if !slot.is_bridge_hosted_external_module() {
                    continue;
                }
                let is_instrument = is_instrument_track
                    && (track.instrument_plugin_instance_id.as_deref() == Some(slot.id.as_str())
                        || index == 0);
                plugin_targets.push(PluginUnloadTarget {
                    track_id: track.id.clone(),
                    insert_id: slot.id.clone(),
                    display_name: slot.display_name.clone(),
                    track_name: track.name.clone(),
                    is_instrument,
                });
            }
        }

        for slot in &state.master.inserts {
            if !slot.is_bridge_hosted_external_module() {
                continue;
            }
            plugin_targets.push(PluginUnloadTarget {
                track_id: crate::components::timeline::timeline_state::MASTER_TRACK_ID.to_string(),
                insert_id: slot.id.clone(),
                display_name: slot.display_name.clone(),
                track_name: "Master".to_string(),
                is_instrument: false,
            });
        }

        SessionShutdownSnapshot {
            reason,
            plugin_targets,
            bridge_runtime: self.plugin_editors.bridge_runtime.take(),
            instrument_track_ids,
            flush_autosave_path: None,
            flush_autosave_project: None,
        }
    }

    fn apply_loaded_project_tracks(
        &mut self,
        package: &LoadedSessionPackage,
        cx: &mut Context<Self>,
    ) -> bool {
        let project = &package.project;
        let path = &package.path;
        let expected_tracks = expected_persisted_track_count(&project.tracks);

        self.teardown_all_plugin_instances(cx, "project_load_replace");

        let (restored_tracks, load_warnings) = self.timeline.update(cx, |timeline, cx| {
            timeline.reset_input_state();
            let warnings = apply_to_timeline(project, &mut timeline.state);
            cx.notify();
            (persisted_track_count(&timeline.state.tracks), warnings)
        });
        // Reopen the ARA sessions this project's clips are bound to, and hand
        // each plug-in its saved document. Runs after the timeline is populated
        // so the graph the plug-in sees matches the restored arrangement.
        self.restore_ara_archives(project, cx);

        // Aggregated onto the one project surface. A project whose interface is
        // unplugged reports a single line, not one dialog per track.
        let classified = load_warnings
            .iter()
            .map(project_load_warning_to_routing_warning)
            .collect::<Vec<_>>();
        self.report_routing_warnings(classified, cx);

        if restored_tracks != expected_tracks {
            session_log!(
                "integrity check failed: expected {expected_tracks} tracks, restored {restored_tracks}"
            );
            return false;
        }

        // Project rate is persisted independently of application defaults. The
        // runtime must adopt it before restoring plugins for this session.
        let project_rate = self.timeline.read(cx).state.project_sample_rate;
        if self.current_audio_sample_rate() != project_rate {
            self.reopen_audio_with_sample_rate(project_rate, cx);
        }

        // A project imported from another DAW's file has no Futureboard file to
        // save back into: bind it untitled and dirty so the first save is a
        // Save As and the imported file is never overwritten.
        if crate::project::is_import_path(path) {
            self.project_session
                .bind_untitled(project.name.clone(), true);
            session_log!("session bound from import: name={}", project.name);
        } else {
            let folder = path.parent().map(PathBuf::from);
            self.project_session.bind_saved(
                project.id.clone(),
                project.name.clone(),
                folder,
                path.clone(),
                project.created_at,
                project.modified_at,
            );
            session_log!(
                "session bound: name={} path={}",
                self.project_session.name,
                path.display()
            );
        }
        // Compile the loaded project's routing — including Master/Monitor
        // hardware ownership — before anything can play.
        self.publish_audio_connection_routing(cx);
        self.sync_project_session_to_workspace(cx);
        self.recent_projects
            .push(&project.name, path.clone(), now_secs());
        self.sync_recent_to_switcher();
        true
    }

    pub(super) fn validate_session_references(&mut self, cx: &mut Context<Self>) {
        let mut dropped = 0usize;
        let _ = self.timeline.update(cx, |timeline, cx| {
            let state = &mut timeline.state;
            if let Some(track_id) = state.selection.selected_track_id.clone() {
                if !state.tracks.iter().any(|track| track.id == track_id) {
                    state.selection.selected_track_id = None;
                    dropped += 1;
                }
            }
            let existing: std::collections::HashSet<String> = state
                .tracks
                .iter()
                .flat_map(|track| track.clips.iter().map(|clip| clip.id.clone()))
                .collect();
            let before = state.selection.selected_clip_ids.len();
            state
                .selection
                .selected_clip_ids
                .retain(|id| existing.contains(id));
            dropped += before - state.selection.selected_clip_ids.len();
            cx.notify();
        });
        if let Some((track_id, insert_id)) = self.selected_insert.clone() {
            let valid = self
                .timeline
                .read(cx)
                .state
                .find_insert_slot(&track_id, &insert_id)
                .is_some();
            if !valid {
                self.selected_insert = None;
                dropped += 1;
            }
        }
        session_log!("validate: invalid references dropped={dropped}");
    }

    pub(super) fn schedule_loaded_project_waveforms(
        &mut self,
        package: &LoadedSessionPackage,
        cx: &mut Context<Self>,
    ) {
        let project = package.project.clone();
        let Some(root) = package.path.parent().map(PathBuf::from) else {
            return;
        };
        // Cubase/Nuendo XML (and other foreign imports) bind untitled: never
        // write peak caches or copy media into the archive's folder. Native
        // `.fbproj` sessions keep peak files under the project tree.
        let persist_peaks = !crate::project::is_import_path(&package.path);
        let timeline = self.timeline.clone();
        let layout = cx.entity().clone();
        crate::components::timeline::audio_import::schedule_project_waveform_restore(
            &project,
            root,
            persist_peaks,
            timeline,
            layout,
            cx,
        );
    }

    /// Finish mounting a workspace that was prepared on the loading-session
    /// screen (audio warm-up / plugin restore) before the studio shell opened.
    pub fn install_prepared_workspace(
        &mut self,
        mut package: LoadedSessionPackage,
        finish: PreparedWorkspaceFinish,
        cx: &mut Context<Self>,
    ) {
        session_log!("install prepared workspace");
        if let Some(handoff) = package.install_handoff.take() {
            self.adopt_session_install_handoff(handoff, cx);
        } else {
            session_log!("prepared workspace missing install handoff — falling back");
        }
        self.session_install_status = SessionInstallStatus::Ready;
        match finish {
            PreparedWorkspaceFinish::EmptyUntitled => {
                self.project_session
                    .bind_untitled("Untitled Project", false);
                self.project_state = ProjectState::UnsavedWorkspace;
                self.sync_project_session_to_workspace(cx);
                self.mark_engine_media_dirty();
                self.schedule_audio_project_sync(cx, true, "empty_workspace");
            }
            PreparedWorkspaceFinish::Template(template) => {
                self.new_project_from_template(template, cx);
            }
            PreparedWorkspaceFinish::OpenDialog => {
                self.dispatch_command_id("project:open", cx);
            }
            PreparedWorkspaceFinish::CreateProject(options) => {
                self.create_saved_project_from_options(options, cx);
            }
        }
        cx.notify();
    }
}

/// Post-install step after pre-studio prepare for non-file workspaces.
#[derive(Clone)]
pub enum PreparedWorkspaceFinish {
    EmptyUntitled,
    Template(crate::project::ProjectTemplate),
    OpenDialog,
    CreateProject(crate::project::ProjectCreateOptions),
}

enum LoadSwitchError {
    NotFound(PathBuf),
    Project(crate::project::ProjectError),
}


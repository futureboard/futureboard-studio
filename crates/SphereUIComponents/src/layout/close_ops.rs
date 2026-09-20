//! Window / project close confirmation and ordered studio teardown.

use gpui::{App, Bounds, Context, Pixels, Window, WindowId};

use std::sync::Arc;

use crate::components::message_box_dialog::{
    open_message_box_window, MessageBoxKind, MessageBoxOptions, MessageBoxResult,
};
use crate::shutdown::{self, ShutdownState};

use super::project_ops::LifecycleAction;
use super::StudioLayout;

/// Pending project-lifecycle dialog state — the close/quit/new/open actions
/// parked while the unsaved-changes guard dialog is shown, plus the last failed
/// open path used by recovery. Third `StudioLayout` decomposition slice; every
/// field is `Option`, so `Default` (all `None`) is derived.
#[derive(Default)]
pub(crate) struct LifecycleGuardState {
    /// Live unsaved-changes guard dialog (Save / Don't Save / Cancel), if shown.
    /// Tracked so New/Open/Close/Quit don't stack dialogs.
    pub unsaved_guard_window:
        Option<gpui::WindowHandle<crate::components::message_box_dialog::MessageBoxWindow>>,
    /// Close/quit action waiting on the unsaved-changes dialog.
    pub pending_close_action: Option<PendingCloseAction>,
    /// New/Open lifecycle action waiting on the unsaved-changes dialog.
    pub pending_lifecycle_action: Option<LifecycleAction>,
    /// Path of the last failed project open, used by recovery dialogs.
    pub pending_failed_open_path: Option<std::path::PathBuf>,
}

/// User-initiated close that may require an unsaved-changes prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingCloseAction {
    /// Unload the session and return to Welcome (File → Close Project).
    CloseProject,
    /// Exit the application (File → Quit, window X, Alt+F4).
    QuitApp,
    /// Platform window close for a specific handle (studio main window).
    CloseWindow(WindowId),
}

impl PendingCloseAction {
    fn label(self) -> &'static str {
        match self {
            Self::CloseProject => "close_project",
            Self::QuitApp => "quit_app",
            Self::CloseWindow(_) => "close_window",
        }
    }
}

/// Whether a guarded project-lifecycle action (New / Open / Close / Quit /
/// Switch) may begin, given the two global gates every entry point must
/// respect: app shutdown and an in-flight project-lifecycle transaction.
///
/// Kept pure so the re-entrancy contract can be unit-tested without a live
/// GPUI layout. All lifecycle entry points share this predicate so New/Open
/// cannot slip a second transaction on top of a half-torn-down session — the
/// gap that previously existed because `guard_dirty_then_lifecycle` checked
/// only `is_shutting_down()`, unlike its `request_close` /
/// `request_switch_project` siblings.
pub(crate) fn lifecycle_action_allowed(shutting_down: bool, lifecycle_busy: bool) -> bool {
    !shutting_down && !lifecycle_busy
}

impl StudioLayout {
    /// Entry point for OS window close (X), mapped to app quit on the studio window.
    pub fn request_close(
        &mut self,
        action: PendingCloseAction,
        owner_bounds: Option<Bounds<Pixels>>,
        cx: &mut Context<Self>,
    ) {
        if ShutdownState::global().is_shutting_down() {
            shutdown::log("request_close ignored — already shutting down");
            return;
        }
        if crate::loading_session::is_project_lifecycle_busy() {
            shutdown::log("request_close ignored — project lifecycle in progress");
            return;
        }
        let dirty = self.project_session.is_dirty;
        shutdown::log(&format!(
            "close requested action={} dirty={dirty}",
            action.label()
        ));
        self.lifecycle_guard.pending_close_action = Some(action);
        if !dirty {
            self.perform_pending_close(cx);
            return;
        }
        self.show_unsaved_changes_dialog(owner_bounds, cx);
    }

    /// Legacy alias used by the native shell WCO hook.
    pub fn request_quit(&mut self, owner_bounds: Option<Bounds<Pixels>>, cx: &mut Context<Self>) {
        self.request_close(PendingCloseAction::QuitApp, owner_bounds, cx);
    }

    pub(super) fn guard_dirty_then_lifecycle(
        &mut self,
        action: LifecycleAction,
        owner_bounds: Option<Bounds<Pixels>>,
        cx: &mut Context<Self>,
    ) {
        // Mirror request_close / request_switch_project: never start a New/Open
        // lifecycle action while shutting down or while another project
        // load/switch/close transaction is already in flight. Without the busy
        // guard, New/Open could stack a second transaction on top of a
        // half-torn-down session during the loading-session window.
        if !lifecycle_action_allowed(
            ShutdownState::global().is_shutting_down(),
            crate::loading_session::is_project_lifecycle_busy(),
        ) {
            shutdown::log(&format!(
                "lifecycle guard ignored (shutdown/busy) action={action:?}"
            ));
            return;
        }
        let dirty = self.project_session.is_dirty;
        shutdown::log(&format!("lifecycle guard action={action:?} dirty={dirty}"));
        if !dirty {
            self.run_lifecycle_action(action, cx);
            return;
        }
        self.lifecycle_guard.pending_lifecycle_action = Some(action);
        self.show_unsaved_changes_dialog(owner_bounds, cx);
    }

    fn show_unsaved_changes_dialog(
        &mut self,
        owner_bounds: Option<Bounds<Pixels>>,
        cx: &mut Context<Self>,
    ) {
        if let Some(handle) = self.lifecycle_guard.unsaved_guard_window.clone() {
            if handle
                .update(cx, |_mb, window, _cx| window.activate_window())
                .is_ok()
            {
                shutdown::log("unsaved dialog already open — focused");
                return;
            }
            self.lifecycle_guard.unsaved_guard_window = None;
        }

        let owner_bounds = crate::window_position::resolve_owner_bounds_with_preferred(
            owner_bounds,
            self.studio_window_bounds(cx),
            cx,
        );
        let options = MessageBoxOptions {
            kind: MessageBoxKind::Warning,
            title: "Save Changes?".to_string(),
            message: "This project has unsaved changes. Do you want to save before closing?"
                .to_string(),
            detail: None,
            buttons: vec![
                "Save".to_string(),
                "Don't Save".to_string(),
                "Cancel".to_string(),
            ],
            default_id: 0,
            cancel_id: Some(2),
        };

        let owner = cx.entity().clone();
        let on_response: Arc<dyn Fn(MessageBoxResult, &mut Window, &mut App) + Send + Sync> =
            Arc::new(move |result, _window, cx| {
                if ShutdownState::global().is_shutting_down() {
                    shutdown::log("unsaved dialog response ignored — shutting down");
                    return;
                }
                let _ = owner.update(cx, |this, cx| {
                    this.lifecycle_guard.unsaved_guard_window = None;
                    match result.response {
                        0 => {
                            shutdown::log("unsaved dialog: Save");
                            this.save_then_pending(cx);
                        }
                        1 => {
                            shutdown::log("unsaved dialog: Don't Save");
                            this.perform_pending_after_guard(cx);
                        }
                        _ => {
                            shutdown::log("unsaved dialog: Cancel");
                            this.lifecycle_guard.pending_close_action = None;
                            this.lifecycle_guard.pending_lifecycle_action = None;
                        }
                    }
                });
            });

        match open_message_box_window(owner_bounds, options, on_response, cx) {
            Ok(handle) => {
                self.lifecycle_guard.unsaved_guard_window = Some(handle);
                shutdown::log("unsaved dialog shown");
            }
            Err(err) => {
                eprintln!("[project] unsaved-changes dialog unavailable: {err}");
                self.lifecycle_guard.pending_close_action = None;
                self.lifecycle_guard.pending_lifecycle_action = None;
                shutdown::log("unsaved dialog failed — close aborted (stay open)");
            }
        }
    }

    fn save_then_pending(&mut self, cx: &mut Context<Self>) {
        if let Some(action) = self.lifecycle_guard.pending_lifecycle_action.take() {
            self.save_lifecycle_then(action, cx);
            return;
        }
        if self.lifecycle_guard.pending_close_action.is_some() {
            self.save_close_then(cx);
        }
    }

    fn perform_pending_after_guard(&mut self, cx: &mut Context<Self>) {
        if self.lifecycle_guard.pending_lifecycle_action.is_some() {
            let action = self
                .lifecycle_guard
                .pending_lifecycle_action
                .take()
                .expect("checked");
            self.run_lifecycle_action(action, cx);
            return;
        }
        self.perform_pending_close(cx);
    }

    pub(super) fn perform_pending_close(&mut self, cx: &mut Context<Self>) {
        let Some(action) = self.lifecycle_guard.pending_close_action.take() else {
            return;
        };
        shutdown::log(&format!("perform_pending_close action={}", action.label()));
        match action {
            PendingCloseAction::CloseProject => self.do_close_project(cx),
            PendingCloseAction::QuitApp | PendingCloseAction::CloseWindow(_) => {
                self.do_quit(cx);
            }
        }
    }

    /// Ordered teardown before GPUI / TLS destruction. Idempotent.
    pub(super) fn shutdown_studio(&mut self, cx: &mut Context<Self>) {
        if !ShutdownState::global().begin() {
            shutdown::log("shutdown_studio skipped — already began");
            return;
        }
        shutdown::log("shutdown_studio begin");

        // Save workspace layout before teardown so nothing is null / stale.
        shutdown::log("phase: save workspace layout");
        self.save_workspace_layout(cx);

        shutdown::log("phase: stop transport");
        self.stop_native_playback(cx);

        self.prepare_immediate_session_shutdown(cx);
        let snapshot = self.capture_session_shutdown_snapshot(
            crate::session_shutdown::SessionShutdownReason::AppExit,
            cx,
        );
        if let Err(error) = crate::session_shutdown::run_session_shutdown(snapshot, |_| {}) {
            eprintln!(
                "[SessionShutdown] shutdown failed step={:?} error={}",
                error.step, error.message
            );
        }

        shutdown::log("phase: audio engine shutdown");
        if let Some(engine) = self.audio_bridge.engine.as_mut() {
            engine.shutdown();
        }

        shutdown::log("shutdown_studio end");
    }

    pub(super) fn do_quit(&mut self, cx: &mut Context<Self>) {
        shutdown::log("do_quit");
        crate::window_lifecycle::log_cx_quit("do_quit");
        self.shutdown_studio(cx);
        shutdown::log("phase: cx.quit");
        cx.quit();
    }
}

#[cfg(test)]
mod tests {
    use super::lifecycle_action_allowed;

    #[test]
    fn lifecycle_allowed_only_when_idle() {
        // The only state that permits a New/Open/Close/Quit/Switch to begin is
        // "not shutting down AND not already busy with a lifecycle transaction".
        assert!(lifecycle_action_allowed(false, false));
    }

    #[test]
    fn lifecycle_blocked_while_busy() {
        // Regression: New/Open must be rejected while a project
        // load/switch/close is in flight, matching request_close /
        // request_switch_project. Previously guard_dirty_then_lifecycle ignored
        // the busy flag, so New/Open could stack a second transaction.
        assert!(!lifecycle_action_allowed(false, true));
    }

    #[test]
    fn lifecycle_blocked_while_shutting_down() {
        assert!(!lifecycle_action_allowed(true, false));
    }

    #[test]
    fn lifecycle_blocked_while_shutting_down_and_busy() {
        assert!(!lifecycle_action_allowed(true, true));
    }
}

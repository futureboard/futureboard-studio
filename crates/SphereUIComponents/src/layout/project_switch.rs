//! Central project switch controller — confirmation, save-then-switch, and session load.

use gpui::{App, Bounds, Context, Pixels, Window};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::components::message_box_dialog::{
    open_message_box_window, MessageBoxKind, MessageBoxOptions, MessageBoxResult,
};
#[cfg(feature = "native-dialogs")]
use crate::project::now_secs;

use super::project_ops::ProjectOpenOptions;
use super::StudioLayout;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectSwitchSource {
    ProjectSwitcher,
    RecentProject,
    OpenProjectDialog,
}

#[derive(Debug, Clone)]
pub struct ProjectSwitchRequest {
    pub target_path: PathBuf,
    pub target_name: Option<String>,
    pub source: ProjectSwitchSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectSwitchConfirmDecision {
    SaveAndSwitch,
    SwitchWithoutSaving,
    SwitchProject,
    Cancel,
}

#[derive(Default)]
pub(crate) struct ProjectSwitchGuardState {
    pub pending_request: Option<ProjectSwitchRequest>,
    pub confirm_dialog:
        Option<gpui::WindowHandle<crate::components::message_box_dialog::MessageBoxWindow>>,
    pub missing_dialog:
        Option<gpui::WindowHandle<crate::components::message_box_dialog::MessageBoxWindow>>,
}

impl StudioLayout {
    /// Entry point from Project Switcher UI — never loads a project directly.
    pub fn request_switch_project(
        &mut self,
        request: ProjectSwitchRequest,
        owner_bounds: Option<Bounds<Pixels>>,
        cx: &mut Context<Self>,
    ) {
        if crate::loading_session::is_project_lifecycle_busy() {
            eprintln!("[ProjectSwitch] ignored — project lifecycle already in progress");
            return;
        }
        self.close_project_switcher_and_overlays(cx);

        if self.is_current_project_path(&request.target_path) {
            eprintln!("[ProjectSwitcher] clicked current project");
            return;
        }

        eprintln!(
            "[ProjectSwitcher] switch requested target={}",
            request.target_path.display()
        );

        if self.is_missing_project_path(&request.target_path) {
            eprintln!(
                "[ProjectSwitcher] target missing path={}",
                request.target_path.display()
            );
            self.show_missing_project_switch_dialog(request, owner_bounds, cx);
            return;
        }

        let dirty = self.project_session.is_dirty;
        eprintln!("[ProjectSwitch] dirty current project={dirty}");
        self.project_switch.pending_request = Some(request.clone());

        if dirty {
            self.show_dirty_project_switch_dialog(request, owner_bounds, cx);
        } else {
            self.show_clean_project_switch_dialog(request, owner_bounds, cx);
        }
    }

    /// Applies a confirmation choice from the switch-project dialog.
    pub fn confirm_and_switch_project(
        &mut self,
        request: ProjectSwitchRequest,
        decision: ProjectSwitchConfirmDecision,
        owner_bounds: Option<Bounds<Pixels>>,
        cx: &mut Context<Self>,
    ) {
        eprintln!("[ProjectSwitch] confirm decision={decision:?}");
        self.project_switch.confirm_dialog = None;

        match decision {
            ProjectSwitchConfirmDecision::Cancel => {
                self.project_switch.pending_request = None;
            }
            ProjectSwitchConfirmDecision::SaveAndSwitch => {
                eprintln!(
                    "[ProjectSwitch] confirm accepted target={}",
                    request.target_path.display()
                );
                eprintln!("[ProjectSwitch] saving current project before switch");
                self.save_project_switch_then(request, cx);
            }
            ProjectSwitchConfirmDecision::SwitchWithoutSaving
            | ProjectSwitchConfirmDecision::SwitchProject => {
                eprintln!(
                    "[ProjectSwitch] confirm accepted target={}",
                    request.target_path.display()
                );
                if matches!(decision, ProjectSwitchConfirmDecision::SwitchWithoutSaving) {
                    self.discard_session_recovery(cx);
                }
                self.execute_confirmed_project_switch(request, cx);
            }
        }

        let _ = owner_bounds;
    }

    /// Open an untitled session's recovered autosave in this studio: the
    /// startup offer when the studio, not Welcome, is the first surface. The
    /// user already chose Recover, so a clean session switches at once; unsaved
    /// changes still go through the Save / Switch Without Saving / Cancel guard.
    pub fn recover_untitled_autosave(&mut self, autosave: PathBuf, cx: &mut Context<Self>) {
        if crate::loading_session::is_project_lifecycle_busy() {
            eprintln!("[ProjectSwitch] untitled recovery ignored — lifecycle in progress");
            return;
        }
        let request = ProjectSwitchRequest {
            target_path: autosave,
            target_name: None,
            source: ProjectSwitchSource::OpenProjectDialog,
        };
        if self.project_session.is_dirty {
            self.request_switch_project(request, None, cx);
        } else {
            self.close_project_switcher_and_overlays(cx);
            self.execute_confirmed_project_switch(request, cx);
        }
    }

    pub(super) fn handle_project_switch_current_row(&mut self, cx: &mut Context<Self>) {
        eprintln!("[ProjectSwitcher] clicked current project");
        self.close_project_switcher_and_overlays(cx);
        cx.notify();
    }

    pub(super) fn clear_project_switch_pending(&mut self) {
        self.project_switch.pending_request = None;
    }

    pub(super) fn execute_confirmed_project_switch(
        &mut self,
        request: ProjectSwitchRequest,
        cx: &mut Context<Self>,
    ) {
        // Neither early return replaces the live session. If it was switched
        // away from without saving, its changes are still being worked on:
        // they autosave again.
        if self.is_current_project_path(&request.target_path) {
            self.project_switch.pending_request = None;
            self.resume_session_recovery();
            return;
        }

        if self.is_missing_project_path(&request.target_path) {
            eprintln!(
                "[ProjectSwitch] switch failed error=target missing path={}",
                request.target_path.display()
            );
            self.project_switch.pending_request = None;
            self.resume_session_recovery();
            return;
        }

        eprintln!(
            "[ProjectSwitch] begin target={}",
            request.target_path.display()
        );
        eprintln!("[ProjectSwitch] source={:?}", request.source);
        let path = request.target_path.clone();
        let from_recent = matches!(
            request.source,
            ProjectSwitchSource::ProjectSwitcher | ProjectSwitchSource::RecentProject
        );
        self.project_switch.pending_request = None;
        self.begin_in_studio_project_switch(path, ProjectOpenOptions { from_recent }, cx);
    }

    pub(super) fn close_project_switcher_and_overlays(&mut self, _cx: &mut Context<Self>) {
        self.menu_bar.open_menu_id = None;
        self.menu_bar.submenu_path.clear();
        self.project_switcher.is_open = false;
        self.command_palette.close();
        self.overlay.text_context_menu = None;
        self.overlay.open_popover = None;
    }

    fn is_current_project_path(&self, target: &Path) -> bool {
        self.project_session
            .project_file_path
            .as_ref()
            .is_some_and(|current| current == target)
    }

    fn is_missing_project_path(&self, target: &Path) -> bool {
        !target.exists()
    }

    fn resolve_owner_bounds(
        &self,
        owner_bounds: Option<Bounds<Pixels>>,
        cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        crate::window_position::resolve_owner_bounds_with_preferred(
            owner_bounds,
            self.studio_window_bounds(cx),
            cx,
        )
    }

    fn show_dirty_project_switch_dialog(
        &mut self,
        request: ProjectSwitchRequest,
        owner_bounds: Option<Bounds<Pixels>>,
        cx: &mut Context<Self>,
    ) {
        if self.focus_existing_switch_dialog(cx) {
            return;
        }

        let owner_bounds = self.resolve_owner_bounds(owner_bounds, cx);
        let options = MessageBoxOptions {
            kind: MessageBoxKind::Warning,
            title: "Switch project?".to_string(),
            message: "Current project has unsaved changes.".to_string(),
            detail: None,
            buttons: vec![
                "Save and Switch".to_string(),
                "Switch Without Saving".to_string(),
                "Cancel".to_string(),
            ],
            default_id: 0,
            cancel_id: Some(2),
        };

        let owner = cx.entity().clone();
        let on_response: Arc<dyn Fn(MessageBoxResult, &mut Window, &mut App) + Send + Sync> =
            Arc::new(move |result, window, cx| {
                let decision = match result.response {
                    0 => ProjectSwitchConfirmDecision::SaveAndSwitch,
                    1 => ProjectSwitchConfirmDecision::SwitchWithoutSaving,
                    _ => ProjectSwitchConfirmDecision::Cancel,
                };
                let bounds = Some(window.bounds());
                let request = request.clone();
                StudioLayout::defer_update(&owner, cx, move |this, cx| {
                    this.confirm_and_switch_project(request, decision, bounds, cx);
                });
            });

        match open_message_box_window(owner_bounds, options, on_response, cx) {
            Ok(handle) => self.project_switch.confirm_dialog = Some(handle),
            Err(err) => {
                eprintln!("[ProjectSwitch] switch failed error=dialog unavailable: {err}");
                self.project_switch.pending_request = None;
            }
        }
    }

    fn show_clean_project_switch_dialog(
        &mut self,
        request: ProjectSwitchRequest,
        owner_bounds: Option<Bounds<Pixels>>,
        cx: &mut Context<Self>,
    ) {
        if self.focus_existing_switch_dialog(cx) {
            return;
        }

        let display_name = request
            .target_name
            .clone()
            .or_else(|| {
                request
                    .target_path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "project".to_string());

        let owner_bounds = self.resolve_owner_bounds(owner_bounds, cx);
        let options = MessageBoxOptions {
            kind: MessageBoxKind::Question,
            title: "Switch project?".to_string(),
            message: format!("Open {display_name}?"),
            detail: None,
            buttons: vec!["Switch Project".to_string(), "Cancel".to_string()],
            default_id: 0,
            cancel_id: Some(1),
        };

        let owner = cx.entity().clone();
        let on_response: Arc<dyn Fn(MessageBoxResult, &mut Window, &mut App) + Send + Sync> =
            Arc::new(move |result, window, cx| {
                let decision = if result.response == 0 {
                    ProjectSwitchConfirmDecision::SwitchProject
                } else {
                    ProjectSwitchConfirmDecision::Cancel
                };
                let bounds = Some(window.bounds());
                let request = request.clone();
                StudioLayout::defer_update(&owner, cx, move |this, cx| {
                    this.confirm_and_switch_project(request, decision, bounds, cx);
                });
            });

        match open_message_box_window(owner_bounds, options, on_response, cx) {
            Ok(handle) => self.project_switch.confirm_dialog = Some(handle),
            Err(err) => {
                eprintln!("[ProjectSwitch] switch failed error=dialog unavailable: {err}");
                self.project_switch.pending_request = None;
            }
        }
    }

    fn show_missing_project_switch_dialog(
        &mut self,
        request: ProjectSwitchRequest,
        owner_bounds: Option<Bounds<Pixels>>,
        cx: &mut Context<Self>,
    ) {
        if let Some(handle) = self.project_switch.missing_dialog.clone() {
            if handle
                .update(cx, |_mb, window, _cx| window.activate_window())
                .is_ok()
            {
                return;
            }
            self.project_switch.missing_dialog = None;
        }

        let owner_bounds = self.resolve_owner_bounds(owner_bounds, cx);
        let options = MessageBoxOptions {
            kind: MessageBoxKind::Error,
            title: "Project not found".to_string(),
            message: "This project may have been moved or deleted.".to_string(),
            detail: Some(request.target_path.to_string_lossy().into_owned()),
            buttons: vec![
                "Locate Project...".to_string(),
                "Remove from Recents".to_string(),
                "Cancel".to_string(),
            ],
            default_id: 0,
            cancel_id: Some(2),
        };

        let owner = cx.entity().clone();
        let missing_path = request.target_path.clone();
        let missing_name = request.target_name.clone();
        let on_response: Arc<dyn Fn(MessageBoxResult, &mut Window, &mut App) + Send + Sync> =
            Arc::new(move |result, _window, cx| {
                let _ = owner.update(cx, |this, cx| {
                    this.project_switch.missing_dialog = None;
                    match result.response {
                        0 => this.cmd_locate_missing_recent_project(
                            missing_path.clone(),
                            missing_name.clone(),
                            cx,
                        ),
                        1 => {
                            this.recent_projects.remove(&missing_path);
                            this.sync_recent_to_switcher();
                            cx.notify();
                        }
                        _ => {}
                    }
                });
            });

        match open_message_box_window(owner_bounds, options, on_response, cx) {
            Ok(handle) => self.project_switch.missing_dialog = Some(handle),
            Err(err) => eprintln!("[ProjectSwitch] missing dialog unavailable: {err}"),
        }
    }

    fn focus_existing_switch_dialog(&mut self, cx: &mut Context<Self>) -> bool {
        if let Some(handle) = self.project_switch.confirm_dialog.clone() {
            if handle
                .update(cx, |_mb, window, _cx| window.activate_window())
                .is_ok()
            {
                return true;
            }
            self.project_switch.confirm_dialog = None;
        }
        false
    }

    pub(super) fn cmd_locate_missing_recent_project(
        &mut self,
        missing_path: PathBuf,
        missing_name: Option<String>,
        cx: &mut Context<Self>,
    ) {
        #[cfg(feature = "native-dialogs")]
        {
            let default_dir = missing_path
                .parent()
                .map(PathBuf::from)
                .or_else(|| self.project_session.folder_path.clone())
                .unwrap_or_else(|| self.default_projects_dir(cx));
            let fallback_name = missing_name.unwrap_or_else(|| {
                missing_path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .unwrap_or("Project")
                    .to_string()
            });
            let entity = cx.entity().clone();
            cx.spawn(async move |_this, cx| {
                let result = rfd::AsyncFileDialog::new()
                    .set_title("Locate Project")
                    .set_directory(&default_dir)
                    .add_filter(
                        "Futureboard Project",
                        crate::project::io::SUPPORTED_PROJECT_FILE_EXTS,
                    )
                    // Projects exported by another DAW open through the
                    // importers in `crate::project::import`.
                    .add_filter(
                        "Cubase XML Track Archive",
                        crate::project::IMPORT_PROJECT_FILE_EXTS,
                    )
                    .pick_file()
                    .await;
                if let Some(handle) = result {
                    let path = handle.path().to_path_buf();
                    let _ = entity.update(cx, |this, cx| {
                        this.recent_projects.remove(&missing_path);
                        this.recent_projects
                            .push(&fallback_name, path.clone(), now_secs());
                        this.sync_recent_to_switcher();
                        this.request_switch_project(
                            ProjectSwitchRequest {
                                target_path: path,
                                target_name: Some(fallback_name),
                                source: ProjectSwitchSource::ProjectSwitcher,
                            },
                            None,
                            cx,
                        );
                    });
                }
            })
            .detach();
        }

        #[cfg(not(feature = "native-dialogs"))]
        {
            let _ = (missing_path, missing_name, cx);
            eprintln!("[Project] Locate Project unavailable: native file dialogs disabled");
        }
    }
}

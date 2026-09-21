//! App-level "Loading Session…" gate — runs before [`crate::layout::StudioLayout`]
//! is mounted so no session-bound UI can observe a half-loaded project.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    div, px, App, AppContext, BorrowAppContext, Bounds, Context, FocusHandle, Global,
    InteractiveElement, IntoElement, ParentElement, Pixels, Render, SharedString, Styled, Window,
    WindowHandle,
};

use crate::app_state::{AppMode, AppSessionGate};
use crate::components::controls::{fb_button, FbButtonKind};
use crate::components::progress_dialog::{progress_bar_animated, ProgressBarValue};
use crate::components::timeline::timeline_state::TimelineState;
use crate::components::title_bar::chromeless_window_titlebar;
use crate::layout::ProjectOpenOptions;
use crate::layout::StudioLayout;
use crate::project::io::{load_project, load_project_strict, validate_project_file};
use crate::project::{FutureboardProject, ProjectSession};
use crate::session_shutdown::{
    flush_autosave_blocking, run_session_shutdown, SessionShutdownError, POST_SHUTDOWN_UI_STEPS,
    UI_SHUTDOWN_STEPS,
};
pub use crate::session_shutdown::{
    SessionLifecycleStep, SessionShutdownReason, SessionShutdownSnapshot,
};
use crate::theme::{self, Colors};

const LOAD_WINDOW_WIDTH: f32 = 430.0;
const LOAD_WINDOW_HEIGHT: f32 = 168.0;
const BODY_PAD_X: f32 = 16.0;
const BODY_PAD_Y: f32 = 14.0;
const BODY_GAP: f32 = 10.0;
const STAGE_TICK: Duration = Duration::from_millis(20);
const UI_STEP_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Default)]
pub(crate) struct LoadingSessionGate {
    window: Option<WindowHandle<LoadingSessionWindow>>,
}

impl Global for LoadingSessionGate {}

#[derive(Debug, Default)]
pub(crate) struct ProjectLifecycleGate {
    busy: AtomicBool,
}

impl Global for ProjectLifecycleGate {}

static PROJECT_LIFECYCLE_BUSY: AtomicBool = AtomicBool::new(false);

pub fn is_project_lifecycle_busy() -> bool {
    PROJECT_LIFECYCLE_BUSY.load(Ordering::Relaxed)
}

fn set_project_lifecycle_busy(busy: bool) {
    PROJECT_LIFECYCLE_BUSY.store(busy, Ordering::Relaxed);
    eprintln!("[ProjectLifecycle] busy={busy}");
}

/// Which surface is carrying a pre-studio session transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionTransactionSurface {
    /// The session transaction window is open and shows progress and errors.
    TransactionWindow,
    /// The transaction window could not be opened. The session work still runs,
    /// but the window that started it is the only surface on screen.
    Headless,
}

impl SessionTransactionSurface {
    pub fn label(self) -> &'static str {
        match self {
            Self::TransactionWindow => "transaction_window",
            Self::Headless => "headless",
        }
    }
}

/// Windows the shell can hand the start screen off to.
///
/// Retiring Welcome before one of these exists leaves the app with no window and
/// nothing to report a failed handoff on, so both decisions read from here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReplacementSurfaces {
    pub transaction_window_open: bool,
    pub studio_mounted: bool,
}

impl ReplacementSurfaces {
    /// Welcome may only be retired once a replacement window is on screen.
    pub fn may_retire_welcome(self) -> bool {
        self.transaction_window_open || self.studio_mounted
    }

    /// Where a failed transaction has to report itself. Welcome is never
    /// re-opened in place of the real reason.
    pub fn failure_surface(self) -> FailureSurface {
        if self.transaction_window_open {
            FailureSurface::TransactionWindow
        } else {
            FailureSurface::WelcomeWindow
        }
    }
}

/// Surface that shows the reason a pre-studio transaction failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureSurface {
    /// Terminal error inside the still-open transaction window.
    TransactionWindow,
    /// Error dialog on the Welcome window that started the transaction.
    WelcomeWindow,
}

/// Returns the app to the start screen from a terminal transaction error.
/// Installed by the native shell, which owns the Welcome window.
pub type ReturnToWelcomeCb = Arc<dyn Fn(&mut App) + Send + Sync>;

static RETURN_TO_WELCOME: std::sync::OnceLock<ReturnToWelcomeCb> = std::sync::OnceLock::new();

/// Install the handler that dismisses a terminal transaction error back to the
/// start screen. Without it the error state would have no way out.
pub fn set_return_to_welcome_handler(handler: ReturnToWelcomeCb) {
    let _ = RETURN_TO_WELCOME.set(handler);
}

fn return_to_welcome_handler() -> Option<ReturnToWelcomeCb> {
    RETURN_TO_WELCOME.get().cloned()
}

/// Where a project lifecycle transaction is headed after the loading dialog finishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectLifecycleTarget {
    Studio,
    Welcome,
}

impl ProjectLifecycleTarget {
    fn label(self) -> &'static str {
        match self {
            Self::Studio => "studio",
            Self::Welcome => "welcome",
        }
    }
}

/// Dismiss the loading session dialog and mark the lifecycle transaction complete.
/// Safe when `target_project` is `None` (close-to-welcome).
pub fn complete_project_lifecycle<C: BorrowAppContext + AppContext>(
    cx: &mut C,
    target: ProjectLifecycleTarget,
) {
    eprintln!(
        "[ProjectLifecycle] ProjectLifecycleCompleted target={} target_project=None",
        target.label()
    );
    set_project_lifecycle_busy(false);
    close_loading_session_window_for(cx);
}

macro_rules! session_log {
    ($($arg:tt)*) => {
        eprintln!("[SessionLoad] {}", format!($($arg)*))
    };
}

/// Audio/plugin runtime prepared before [`crate::layout::StudioLayout`] mounts.
pub struct SessionInstallHandoff {
    pub engine: DirectAudio::AudioEngine,
    pub engine_stats: DirectAudio::EngineStats,
    pub(crate) bridge_runtime:
        Option<crate::layout::plugin_bridge_runtime::SharedPluginBridgeRuntime>,
    pub timeline_state: TimelineState,
}

/// Decoded project payload handed to a freshly mounted [`crate::layout::StudioLayout`].
pub struct LoadedSessionPackage {
    pub project: FutureboardProject,
    pub path: PathBuf,
    pub open_options: ProjectOpenOptions,
    /// Populated by pre-studio install; studio adopts this instead of re-restoring.
    pub install_handoff: Option<SessionInstallHandoff>,
    pub restore_warnings: Vec<String>,
}

/// Snapshot captured before replacing an in-flight studio session so a failed
/// open can restore the previous project without mounting partial state.
#[derive(Debug, Clone)]
pub struct SessionRollbackSnapshot {
    pub timeline_state: TimelineState,
    pub session: ProjectSession,
    pub project_state: crate::app_state::ProjectState,
}

pub struct LoadFailedContext {
    pub title: String,
    pub message: String,
    pub detail: Option<String>,
    pub path: Option<PathBuf>,
    pub open_options: ProjectOpenOptions,
    pub rollback: Option<SessionRollbackSnapshot>,
}

pub type LoadSuccessCb = Arc<dyn Fn(LoadedSessionPackage, &mut App) + Send + Sync>;
pub type LoadFailedCb = Arc<dyn Fn(LoadFailedContext, &mut App) + Send + Sync>;

pub type SessionShutdownCompleteCb = Arc<dyn Fn(&mut App) + Send + Sync>;

/// Ordered stages of a pre-studio session transaction.
///
/// A workspace prepare (New, Empty, Template, Open dialog) has no project file
/// to read and starts at [`LoadStage::SessionInstall`]. Replacing a live session
/// starts at [`LoadStage::SessionShutdown`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadStage {
    SessionShutdown,
    Validate,
    Decode,
    SessionInstall,
}

impl LoadStage {
    /// First stage for a transaction, from the work it actually has to do.
    pub fn initial(has_project_file: bool, replaces_live_session: bool) -> Self {
        if replaces_live_session {
            Self::SessionShutdown
        } else if has_project_file {
            Self::Validate
        } else {
            Self::SessionInstall
        }
    }

    /// Stage that follows this one once its work succeeded, or `None` when the
    /// transaction has nothing left to do.
    pub fn next(self, has_project_file: bool) -> Option<Self> {
        match self {
            Self::SessionShutdown if !has_project_file => None,
            Self::SessionShutdown => Some(Self::Validate),
            Self::Validate => Some(Self::Decode),
            Self::Decode => Some(Self::SessionInstall),
            Self::SessionInstall => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            LoadStage::SessionShutdown => "Closing current session",
            LoadStage::Validate => "Validating project file",
            LoadStage::Decode => "Reading project data",
            LoadStage::SessionInstall => "Preparing session",
        }
    }

    fn progress(self) -> ProgressBarValue {
        // Session/plugin restore work cannot report a reliable overall fraction:
        // one slow plug-in may dominate an otherwise short project load. Keep the
        // app-level gate honestly indeterminate and communicate progress through
        // the current activity label instead.
        ProgressBarValue::Indeterminate
    }
}

async fn run_studio_ui_step_with_timeout(
    studio: &WindowHandle<StudioLayout>,
    cx: &mut gpui::AsyncApp,
    ui: gpui::Entity<LoadingSessionWindow>,
    step: SessionLifecycleStep,
    clear_session_state: bool,
) -> Result<(), SessionShutdownError> {
    touch_loading_progress(
        cx,
        &ui,
        step.label(),
        ProgressBarValue::value(step.progress_base()),
    )
    .await;
    let deadline = std::time::Instant::now() + step.timeout().min(UI_STEP_TIMEOUT);
    let slot = Arc::new(std::sync::Mutex::new(None::<Result<(), String>>));
    let slot_wait = slot.clone();
    let update_result = studio.update(cx, |layout, _window, cx| {
        let result = layout.run_session_lifecycle_ui_step(step, clear_session_state, cx);
        if let Ok(mut guard) = slot_wait.lock() {
            *guard = Some(result);
        }
    });
    if update_result.is_err() {
        return Err(SessionShutdownError {
            step,
            message: "studio window update failed".to_string(),
        });
    }
    while slot.lock().ok().and_then(|guard| guard.clone()).is_none() {
        if std::time::Instant::now() >= deadline {
            return Err(SessionShutdownError {
                step,
                message: format!(
                    "{} timed out after {:?}",
                    step.label(),
                    step.timeout().min(UI_STEP_TIMEOUT)
                ),
            });
        }
        cx.background_executor()
            .timer(Duration::from_millis(25))
            .await;
    }
    slot.lock()
        .ok()
        .and_then(|mut guard| guard.take())
        .unwrap_or_else(|| Err(format!("{} did not report a result", step.label())))
        .map_err(|message| SessionShutdownError { step, message })
}

async fn capture_shutdown_snapshot_from_studio(
    studio: &WindowHandle<StudioLayout>,
    cx: &mut gpui::AsyncApp,
    reason: SessionShutdownReason,
) -> Result<SessionShutdownSnapshot, SessionShutdownError> {
    let step = SessionLifecycleStep::UnloadPlugins;
    let deadline = std::time::Instant::now() + UI_STEP_TIMEOUT;
    let slot = Arc::new(std::sync::Mutex::new(None::<SessionShutdownSnapshot>));
    let slot_wait = slot.clone();
    let update_result = studio.update(cx, |layout, _window, cx| {
        let snapshot = layout.capture_session_shutdown_snapshot_for_loading(reason, cx);
        if let Ok(mut guard) = slot_wait.lock() {
            *guard = Some(snapshot);
        }
    });
    if update_result.is_err() {
        return Err(SessionShutdownError {
            step,
            message: "failed to capture shutdown snapshot".to_string(),
        });
    }
    while slot.lock().ok().and_then(|guard| guard.clone()).is_none() {
        if std::time::Instant::now() >= deadline {
            return Err(SessionShutdownError {
                step,
                message: "capturing shutdown snapshot timed out".to_string(),
            });
        }
        cx.background_executor()
            .timer(Duration::from_millis(25))
            .await;
    }
    slot.lock()
        .ok()
        .and_then(|mut guard| guard.take())
        .ok_or_else(|| SessionShutdownError {
            step,
            message: "shutdown snapshot missing after capture".to_string(),
        })
}

async fn touch_loading_progress(
    cx: &mut gpui::AsyncApp,
    ui: &gpui::Entity<LoadingSessionWindow>,
    detail: &str,
    bar: ProgressBarValue,
) {
    let ui = ui.clone();
    let detail = detail.to_string();
    let _ = ui.update(cx, |window, cx| window.set_progress(detail, bar, cx));
}

async fn run_async_session_shutdown(
    cx: &mut gpui::AsyncApp,
    ui: gpui::Entity<LoadingSessionWindow>,
    studio: Option<WindowHandle<StudioLayout>>,
    reason: SessionShutdownReason,
    clear_session_state: bool,
    prepared_snapshot: Option<SessionShutdownSnapshot>,
) -> Result<(), SessionShutdownError> {
    if let Some(studio) = studio.as_ref() {
        for step in UI_SHUTDOWN_STEPS {
            if *step == SessionLifecycleStep::FlushAutosave {
                continue;
            }
            run_studio_ui_step_with_timeout(studio, cx, ui.clone(), *step, clear_session_state)
                .await?;
        }
    }

    let mut snapshot = if let Some(snapshot) = prepared_snapshot {
        snapshot
    } else if let Some(studio) = studio.as_ref() {
        capture_shutdown_snapshot_from_studio(studio, cx, reason).await?
    } else {
        return Err(SessionShutdownError {
            step: SessionLifecycleStep::StopTransport,
            message: "no studio surface available for session shutdown".to_string(),
        });
    };

    if let (Some(path), Some(project)) = (
        snapshot.flush_autosave_path.clone(),
        snapshot.flush_autosave_project.take(),
    ) {
        touch_loading_progress(
            cx,
            &ui,
            SessionLifecycleStep::FlushAutosave.label(),
            ProgressBarValue::value(SessionLifecycleStep::FlushAutosave.progress_base()),
        )
        .await;
        let flush_result = cx
            .background_executor()
            .spawn(async move {
                flush_autosave_blocking(
                    path,
                    project,
                    SessionLifecycleStep::FlushAutosave.timeout(),
                )
            })
            .await;
        if let Err(message) = flush_result {
            return Err(SessionShutdownError {
                step: SessionLifecycleStep::FlushAutosave,
                message,
            });
        }
    }

    touch_loading_progress(
        cx,
        &ui,
        SessionLifecycleStep::UnloadPlugins.label(),
        ProgressBarValue::Indeterminate,
    )
    .await;
    let progress_slot = Arc::new(std::sync::Mutex::new((
        SessionLifecycleStep::UnloadPlugins.label().to_string(),
        ProgressBarValue::Indeterminate,
    )));
    let progress_for_shutdown = progress_slot.clone();
    let shutdown_done = Arc::new(AtomicBool::new(false));
    let shutdown_done_flag = shutdown_done.clone();
    let shutdown_future = cx.background_executor().spawn(async move {
        let result = run_session_shutdown(snapshot, move |report| {
            if let Ok(mut slot) = progress_for_shutdown.lock() {
                *slot = (report.stage.clone(), report.bar);
            }
        });
        shutdown_done_flag.store(true, Ordering::Release);
        result
    });
    while !shutdown_done.load(Ordering::Acquire) {
        // Snapshot + drop the mutex guard before awaiting — never hold a lock
        // across an await point.
        let progress = progress_slot.lock().ok().as_deref().cloned();
        if let Some((detail, bar)) = progress {
            touch_loading_progress(cx, &ui, &detail, bar).await;
        }
        cx.background_executor()
            .timer(Duration::from_millis(50))
            .await;
    }
    let shutdown_result = shutdown_future.await;

    shutdown_result?;

    if let Some(studio) = studio.as_ref() {
        for step in POST_SHUTDOWN_UI_STEPS {
            run_studio_ui_step_with_timeout(studio, cx, ui.clone(), *step, clear_session_state)
                .await?;
        }
    }

    if clear_session_state {
        if let Some(studio) = studio.as_ref() {
            run_studio_ui_step_with_timeout(
                studio,
                cx,
                ui,
                SessionLifecycleStep::ClearSessionState,
                true,
            )
            .await?;
        }
    }

    Ok(())
}

struct SessionLoadTransaction {
    path: Option<PathBuf>,
    open_options: ProjectOpenOptions,
    rollback: Option<SessionRollbackSnapshot>,
    shutdown: Option<SessionShutdownSnapshot>,
    shutdown_reason: Option<SessionShutdownReason>,
    studio: Option<WindowHandle<StudioLayout>>,
    clear_session_state: bool,
    on_shutdown_complete: Option<SessionShutdownCompleteCb>,
    stage: LoadStage,
    project: Option<FutureboardProject>,
    on_success: LoadSuccessCb,
    on_failure: LoadFailedCb,
}

pub struct LoadingSessionWindow {
    heading: SharedString,
    detail: SharedString,
    progress: ProgressBarValue,
    footer: SharedString,
    focus_handle: FocusHandle,
    indeterminate_phase: f32,
    animation_active: bool,
    has_error: bool,
    /// Only set in the terminal error state — the window is otherwise owned by a
    /// running transaction and must not expose a way to close it.
    on_dismiss: Option<ReturnToWelcomeCb>,
    transaction: Option<SessionLoadTransaction>,
}

impl LoadingSessionWindow {
    fn new(
        heading: impl Into<SharedString>,
        initial_detail: impl Into<SharedString>,
        initial_progress: ProgressBarValue,
        transaction: Option<SessionLoadTransaction>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            heading: heading.into(),
            detail: initial_detail.into(),
            progress: initial_progress,
            footer: "This can take a moment for large sessions.".into(),
            focus_handle: cx.focus_handle(),
            indeterminate_phase: 0.0,
            animation_active: false,
            has_error: false,
            on_dismiss: None,
            transaction,
        }
    }

    /// Progress-only surface for work driven outside this window (workspace
    /// prepare). It owns no transaction, so losing it cannot stall the session.
    fn new_progress(
        heading: impl Into<SharedString>,
        detail: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new(heading, detail, ProgressBarValue::Indeterminate, None, cx)
    }

    fn start_progress_animation(&mut self, cx: &mut Context<Self>) {
        if self.animation_active {
            return;
        }
        self.animation_active = true;
        cx.spawn(async move |entity, cx| loop {
            cx.background_executor().timer(STAGE_TICK).await;
            let still_active = entity
                .update(cx, |window, cx| {
                    if !window.animation_active {
                        return false;
                    }
                    window.indeterminate_phase =
                        (window.indeterminate_phase + 0.035).rem_euclid(1.0);
                    cx.notify();
                    true
                })
                .unwrap_or(false);
            if !still_active {
                break;
            }
        })
        .detach();
    }

    fn stop_progress_animation(&mut self) {
        self.animation_active = false;
    }

    fn new_for_load(
        session_name: Option<String>,
        transaction: SessionLoadTransaction,
        cx: &mut Context<Self>,
    ) -> Self {
        let heading = if transaction.shutdown.is_some() {
            "Switching Project…".to_string()
        } else {
            session_name
                .filter(|name| !name.is_empty())
                .map(|name| format!("Loading {name}"))
                .unwrap_or_else(|| "Loading Session…".to_string())
        };
        let stage = transaction.stage;
        Self::new(
            heading,
            stage.label(),
            stage.progress(),
            Some(transaction),
            cx,
        )
    }

    fn set_stage(&mut self, stage: LoadStage, cx: &mut Context<Self>) {
        self.detail = stage.label().into();
        self.progress = stage.progress();
        cx.notify();
    }

    fn set_detail(&mut self, detail: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.detail = detail.into();
        cx.notify();
    }

    fn set_progress(
        &mut self,
        detail: impl Into<SharedString>,
        _progress: ProgressBarValue,
        cx: &mut Context<Self>,
    ) {
        self.detail = detail.into();
        self.progress = ProgressBarValue::Indeterminate;
        cx.notify();
    }

    fn schedule_tick(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(STAGE_TICK).await;
            let _ = this.update(cx, |this, cx| this.advance(cx));
        })
        .detach();
    }

    /// Move the transaction to the stage that follows `from` and keep driving it.
    fn continue_after(&mut self, from: LoadStage, cx: &mut Context<Self>) {
        let has_project_file = self
            .transaction
            .as_ref()
            .is_some_and(|load| load.path.is_some());
        let Some(next) = from.next(has_project_file) else {
            return;
        };
        if let Some(load) = self.transaction.as_mut() {
            load.stage = next;
        }
        self.schedule_tick(cx);
    }

    fn advance(&mut self, cx: &mut Context<Self>) {
        let Some(stage) = self.transaction.as_ref().map(|load| load.stage) else {
            return;
        };
        session_log!("stage: {}", stage.label());
        self.set_stage(stage, cx);

        match stage {
            LoadStage::SessionShutdown => {
                let Some(mut transaction) = self.transaction.take() else {
                    return;
                };
                let snapshot = transaction.shutdown.take();
                let on_shutdown_complete = transaction.on_shutdown_complete.clone();
                let has_load = transaction.path.is_some();
                let needs_shutdown = transaction.studio.is_some()
                    || snapshot.is_some()
                    || transaction.shutdown_reason.is_some();
                if !needs_shutdown {
                    self.transaction = Some(transaction);
                    self.continue_after(LoadStage::SessionShutdown, cx);
                    return;
                }
                self.transaction = Some(transaction);
                self.begin_session_shutdown(snapshot, has_load, on_shutdown_complete, cx);
            }
            LoadStage::Validate => {
                let path = match self.transaction.as_ref().and_then(|load| load.path.clone()) {
                    Some(path) => path,
                    None => {
                        self.finish_failure(
                            "Open Project Failed",
                            "No project path was provided.",
                            None,
                            cx,
                        );
                        return;
                    }
                };
                if !path.exists() {
                    let i18n = crate::i18n::I18n::from_app(cx);
                    self.finish_failure(
                        "Open Project Failed",
                        &i18n.tr("project.error.file-not-found"),
                        Some(format!("Details: {}", path.display())),
                        cx,
                    );
                    return;
                }
                match validate_project_file(&path) {
                    Ok(version) => {
                        session_log!("project schema version={version}");
                        self.continue_after(LoadStage::Validate, cx);
                    }
                    Err(e) => {
                        session_log!("header validation failed: {}", e.technical_detail());
                        self.finish_failure(
                            "Open Project Failed",
                            e.user_message(),
                            Some(format!("Details: {}", e.technical_detail())),
                            cx,
                        );
                    }
                }
            }
            LoadStage::Decode => {
                self.set_detail("Loading project file", cx);
                let path = match self.transaction.as_ref().and_then(|load| load.path.clone()) {
                    Some(path) => path,
                    None => {
                        self.finish_failure(
                            "Open Project Failed",
                            "No project path was provided.",
                            None,
                            cx,
                        );
                        return;
                    }
                };
                let this = cx.entity().clone();
                cx.spawn(async move |_entity, cx| {
                    let decoded = cx
                        .background_executor()
                        .spawn(async move { load_project(&path, false) })
                        .await;
                    let _ = this.update(cx, |this, cx| this.on_decode_complete(decoded, cx));
                })
                .detach();
            }
            LoadStage::SessionInstall => {
                let Some(mut transaction) = self.transaction.take() else {
                    return;
                };
                let Some(project) = transaction.project.take() else {
                    self.transaction = Some(transaction);
                    let i18n = crate::i18n::I18n::from_app(cx);
                    self.finish_failure(
                        "Open Project Failed",
                        &i18n.tr("project.error.restore-session-failed"),
                        Some("Decoded project data was missing.".to_string()),
                        cx,
                    );
                    return;
                };
                let package = LoadedSessionPackage {
                    project,
                    path: transaction.path.unwrap_or_else(|| PathBuf::from(".")),
                    open_options: transaction.open_options,
                    install_handoff: None,
                    restore_warnings: Vec::new(),
                };
                let on_success = transaction.on_success;
                let on_failure = transaction.on_failure;
                self.set_detail("Preparing session", cx);
                self.progress = ProgressBarValue::value(0.25);
                self.start_progress_animation(cx);
                // Install runs outside this entity update so it is driven the
                // same way whether or not this window exists.
                cx.defer(move |cx| {
                    spawn_session_install(package, on_success, on_failure, cx);
                });
            }
        }
    }

    fn on_decode_complete(
        &mut self,
        decoded: Result<FutureboardProject, crate::project::ProjectError>,
        cx: &mut Context<Self>,
    ) {
        let Some(load) = self.transaction.as_mut() else {
            return;
        };
        match decoded {
            Ok(project) => {
                let track_count = project.tracks.len();
                let clip_count: usize = project.tracks.iter().map(|t| t.clips.len()).sum();
                session_log!("decoded: tracks={track_count} clips={clip_count}");
                load.project = Some(project);
                self.continue_after(LoadStage::Decode, cx);
            }
            Err(e) => {
                session_log!("decode failed: {}", e.technical_detail());
                self.finish_failure(
                    "Open Project Failed",
                    e.user_message(),
                    Some(format!("Details: {}", e.technical_detail())),
                    cx,
                );
            }
        }
    }

    fn finish_failure(
        &mut self,
        title: &str,
        message: &str,
        detail: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.stop_progress_animation();
        set_project_lifecycle_busy(false);
        let Some(transaction) = self.transaction.take() else {
            return;
        };
        let ctx = LoadFailedContext {
            title: title.to_string(),
            message: message.to_string(),
            detail,
            path: transaction.path,
            open_options: transaction.open_options,
            rollback: transaction.rollback,
        };
        let on_failure = transaction.on_failure;
        // Defer so we never invoke failure handling from inside our own update.
        // The shell closes this window only after a replacement surface exists.
        cx.defer(move |cx| {
            on_failure(ctx, cx);
        });
    }

    fn show_terminal_error(
        &mut self,
        heading: impl Into<SharedString>,
        message: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.stop_progress_animation();
        self.has_error = true;
        self.heading = heading.into();
        self.detail = message.into();
        self.progress = ProgressBarValue::Value(0.0);
        self.footer = "No project was opened. The previous session is unchanged.".into();
        self.transaction = None;
        self.on_dismiss = return_to_welcome_handler();
        if self.on_dismiss.is_none() {
            session_log!("terminal error has no return-to-welcome handler installed");
        }
        cx.notify();
    }

    fn dismiss_terminal_error(&mut self, cx: &mut Context<Self>) {
        let Some(dismiss) = self.on_dismiss.take() else {
            return;
        };
        cx.defer(move |cx| dismiss(cx));
    }

    fn finish_shutdown_failure(&mut self, error: SessionShutdownError, cx: &mut Context<Self>) {
        session_log!(
            "shutdown failed step={} error={}",
            error.step.label(),
            error.message
        );
        self.show_terminal_error(
            "Session Operation Failed",
            format!("{} failed.\n\n{}", error.step.label(), error.message),
            cx,
        );
        set_project_lifecycle_busy(false);
    }

    fn begin_session_shutdown(
        &mut self,
        snapshot: Option<SessionShutdownSnapshot>,
        continue_to_load: bool,
        on_shutdown_complete: Option<SessionShutdownCompleteCb>,
        cx: &mut Context<Self>,
    ) {
        let Some(mut transaction) = self.transaction.take() else {
            return;
        };
        let studio = transaction.studio.clone();
        let shutdown_reason = transaction
            .shutdown_reason
            .or_else(|| snapshot.as_ref().map(|value| value.reason))
            .unwrap_or(SessionShutdownReason::ProjectClose);
        let clear_session_state = transaction.clear_session_state;
        let snapshot_for_async = snapshot.clone();
        transaction.shutdown = snapshot;
        transaction.on_shutdown_complete = on_shutdown_complete;
        self.transaction = Some(transaction);
        self.start_progress_animation(cx);
        set_project_lifecycle_busy(true);

        eprintln!("[SessionLoad] progress sink attached (shutdown)");
        eprintln!("[LoadingSessionUI] presentation=dialog");
        self.set_detail("Closing current session", cx);

        let this = cx.entity().clone();
        cx.spawn(async move |_view, cx| {
            let shutdown_result = run_async_session_shutdown(
                cx,
                this.clone(),
                studio,
                shutdown_reason,
                clear_session_state,
                snapshot_for_async,
            )
            .await;

            let _ = this.update(cx, |window, cx| {
                set_project_lifecycle_busy(false);
                match shutdown_result {
                    Ok(()) => {
                        if continue_to_load {
                            eprintln!(
                                "[ProjectLifecycle] shutdown complete continue_to_load=true"
                            );
                            if let Some(load) = window.transaction.as_mut() {
                                load.stage = LoadStage::Validate;
                            }
                            window.set_detail("Reading project", cx);
                            window.progress = ProgressBarValue::value(0.1);
                            cx.notify();
                            window.schedule_tick(cx);
                        } else if let Some(on_complete) = window
                            .transaction
                            .as_ref()
                            .and_then(|load| load.on_shutdown_complete.clone())
                        {
                            eprintln!(
                                "[ProjectLifecycle] shutdown complete continue_to_load=false — invoking completion callback"
                            );
                            window.stop_progress_animation();
                            window.transaction = None;
                            cx.defer(move |cx| {
                                on_complete(cx);
                            });
                        } else {
                            eprintln!(
                                "[ProjectLifecycle] shutdown complete continue_to_load=false target_project=None"
                            );
                            window.stop_progress_animation();
                            window.transaction = None;
                            cx.defer(move |cx| {
                                complete_project_lifecycle(cx, ProjectLifecycleTarget::Welcome);
                            });
                        }
                    }
                    Err(error) => window.finish_shutdown_failure(error, cx),
                }
            });
        })
        .detach();
    }
}

/// Run the pre-studio session install for `package` off the UI thread.
///
/// The session transaction window is only a progress surface here: the install
/// is driven by the app, reports its outcome through `on_success` / `on_failure`,
/// and completes identically when that window could not be opened.
fn spawn_session_install(
    package: LoadedSessionPackage,
    on_success: LoadSuccessCb,
    on_failure: LoadFailedCb,
    cx: &mut App,
) {
    eprintln!("[SessionLoad] progress sink attached");
    set_project_lifecycle_busy(true);
    touch_loading_session_progress(cx, "Preparing session", ProgressBarValue::Indeterminate);

    let path = package.path.clone();
    let open_options = package.open_options.clone();
    let project = package.project.clone();

    cx.spawn(async move |cx| {
        let install_result = cx
            .background_executor()
            .spawn(async move {
                crate::pre_studio_install::run_pre_studio_session_install(package, |detail, bar| {
                    eprintln!(
                        "[SessionLoad] install progress: {detail} {:?}",
                        bar.fraction()
                    );
                })
            })
            .await;

        let _ = cx.update(|cx| {
            set_project_lifecycle_busy(false);
            match install_result {
                Ok((handoff, report)) => {
                    let ready = LoadedSessionPackage {
                        project,
                        path,
                        open_options,
                        install_handoff: Some(handoff),
                        restore_warnings: report.warnings,
                    };
                    finish_loading_session_progress(cx, "Opening studio");
                    eprintln!("[SessionLoad] ready");
                    cx.defer(move |cx| {
                        on_success(ready, cx);
                    });
                }
                Err(error) => {
                    session_log!("session install failed: {error}");
                    let i18n = crate::i18n::I18n::from_app(cx);
                    let ctx = LoadFailedContext {
                        title: "Open Project Failed".to_string(),
                        message: i18n.tr("project.error.restore-session-failed"),
                        detail: Some(format!("Details: {error}")),
                        path: Some(path),
                        open_options,
                        rollback: None,
                    };
                    cx.defer(move |cx| {
                        on_failure(ctx, cx);
                    });
                }
            }
        });
    })
    .detach();
}

/// Update the pre-studio loading window with session-install progress.
pub(crate) fn touch_loading_session_progress<C: BorrowAppContext + AppContext>(
    cx: &mut C,
    detail: &str,
    progress: ProgressBarValue,
) {
    let detail = detail.to_string();
    cx.update_default_global::<LoadingSessionGate, _>(|gate, cx| {
        if let Some(handle) = gate.window.as_ref() {
            let detail = detail.clone();
            let _ = handle.update(cx, |window, _win, cx| {
                window.set_progress(detail, progress, cx);
            });
        }
    });
}

/// Update the pre-studio loading window with session-install progress.
pub fn update_loading_session_progress(cx: &mut App, detail: &str, progress: ProgressBarValue) {
    touch_loading_session_progress(cx, detail, progress);
}

/// Settle the transaction window on its final label and stop the indeterminate
/// sweep, so the window never keeps animating while the studio shell mounts.
fn finish_loading_session_progress(cx: &mut App, detail: &str) {
    let detail = detail.to_string();
    cx.update_default_global::<LoadingSessionGate, _>(|gate, cx| {
        if let Some(handle) = gate.window.as_ref() {
            let detail = detail.clone();
            let _ = handle.update(cx, |window, _win, cx| {
                window.stop_progress_animation();
                window.progress = ProgressBarValue::value(1.0);
                window.detail = detail.into();
                cx.notify();
            });
        }
    });
}

pub fn is_loading_session_window_open(cx: &App) -> bool {
    cx.try_global::<LoadingSessionGate>()
        .map(|gate| gate.window.is_some())
        .unwrap_or(false)
}

pub(crate) fn close_loading_session_window_for<C: BorrowAppContext + AppContext>(cx: &mut C) {
    eprintln!("[WindowLifecycle] close loading session window requested");
    session_log!("closing loading window");
    set_project_lifecycle_busy(false);
    crate::window_lifecycle::log_remove_window("LoadingSessionWindow", "session_load_complete");
    cx.update_default_global::<LoadingSessionGate, _>(|gate, cx| {
        if let Some(handle) = gate.window.take() {
            let _ = handle.update(cx, |_view, window, _cx| window.remove_window());
        }
    });
}

/// Close the loading-session window. Call only after a replacement window
/// (Studio or Welcome) is open and its handle is retained in app state.
pub fn close_loading_session_window(cx: &mut App) {
    close_loading_session_window_for(cx);
}

/// Show a terminal error on the transaction window instead of closing the app.
///
/// Returns `false` when no transaction window is open, so the caller can report
/// the failure on its own surface rather than dropping it.
pub fn show_loading_session_error(
    cx: &mut App,
    heading: impl Into<String>,
    message: impl Into<String>,
) -> bool {
    if !cx.has_global::<LoadingSessionGate>() {
        return false;
    }
    let heading = heading.into();
    let message = message.into();
    cx.update_global::<LoadingSessionGate, _>(|gate, cx| {
        let Some(handle) = gate.window.as_ref() else {
            return false;
        };
        handle
            .update(cx, |window, _win, cx| {
                window.show_terminal_error(heading.clone(), message.clone(), cx)
            })
            .is_ok()
    })
}

fn store_loading_session_window(cx: &mut App, handle: WindowHandle<LoadingSessionWindow>) {
    if cx.has_global::<LoadingSessionGate>() {
        cx.update_global::<LoadingSessionGate, _>(|gate, _| gate.window = Some(handle));
    } else {
        cx.set_global(LoadingSessionGate {
            window: Some(handle),
        });
    }
}

impl Render for LoadingSessionWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let heading = self.heading.clone();
        let detail = self.detail.clone();
        let footer = self.footer.clone();
        let has_error = self.has_error;
        let this = cx.entity().clone();

        // The close control exists only once the transaction is over and the
        // window holds a terminal error; while loading, lifecycle belongs to the
        // transaction and there is nothing valid to close into.
        let on_close: Option<crate::components::title_bar::WindowChromeCloseCb> =
            has_error.then(|| {
                let this = this.clone();
                Arc::new(move |_window: &mut Window, cx: &mut App| {
                    let _ = this.update(cx, |window, cx| window.dismiss_terminal_error(cx));
                }) as crate::components::title_bar::WindowChromeCloseCb
            });

        let mut body = div()
            .flex()
            .flex_col()
            .flex_1()
            .px(px(BODY_PAD_X))
            .py(px(BODY_PAD_Y))
            .gap(px(BODY_GAP))
            .child(
                div()
                    .text_size(px(12.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(if has_error {
                        Colors::accent_danger()
                    } else {
                        Colors::text_primary()
                    })
                    .child(heading),
            )
            .child(
                div()
                    .text_size(px(10.0))
                    .line_height(px(15.0))
                    .text_color(Colors::text_muted())
                    .child(detail),
            );

        if has_error {
            body = body.child(
                div()
                    .text_size(px(10.0))
                    .text_color(Colors::text_faint())
                    .child(footer),
            );
            if self.on_dismiss.is_some() {
                let this = this.clone();
                body = body.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_end()
                        .mt_auto()
                        .child(fb_button(
                            "loading-session-dismiss",
                            "Back to Welcome",
                            FbButtonKind::Primary,
                            true,
                            move |_, _window, cx| {
                                let _ =
                                    this.update(cx, |window, cx| window.dismiss_terminal_error(cx));
                            },
                        )),
                );
            }
        } else {
            body = body
                .child(progress_bar_animated(
                    ProgressBarValue::Indeterminate,
                    self.indeterminate_phase,
                ))
                .child(
                    div()
                        .text_size(px(10.0))
                        .text_color(Colors::text_faint())
                        .child(footer),
                );
        }

        div()
            .flex()
            .flex_col()
            .size_full()
            .font(theme::ui_font())
            .bg(Colors::surface_base())
            .overflow_hidden()
            .rounded(px(crate::theme::radius::CONTROL))
            .border(px(1.0))
            .border_color(if has_error {
                Colors::accent_danger()
            } else {
                Colors::border_subtle()
            })
            .shadow(vec![gpui::BoxShadow {
                color: Colors::surface_overlay().into(),
                offset: gpui::point(px(0.0), px(6.0)),
                blur_radius: px(20.0),
                spread_radius: px(0.0),
                inset: false,
            }])
            .child(div().w(px(0.0)).h(px(0.0)).track_focus(&self.focus_handle))
            .capture_key_down(move |event, _window, cx| {
                if event.keystroke.key.as_str() == "escape" {
                    let _ = this.update(cx, |window, cx| window.dismiss_terminal_error(cx));
                }
            })
            .child(chromeless_window_titlebar(
                "Loading Session".to_string(),
                "loading-session-close",
                on_close,
            ))
            .child(body)
    }
}

fn set_app_mode(cx: &mut App, mode: AppMode) {
    if cx.has_global::<AppSessionGate>() {
        let from = cx.update_global::<AppSessionGate, _>(|gate, _| {
            let from = gate.mode;
            if from != mode {
                crate::window_lifecycle::log_app_mode_change(from, mode, "loading_session");
            }
            gate.mode = mode;
            from
        });
        let _ = from;
    } else {
        cx.set_global(AppSessionGate { mode });
    }
}

fn run_headless_load(
    path: PathBuf,
    open_options: ProjectOpenOptions,
    rollback: Option<SessionRollbackSnapshot>,
    on_success: &LoadSuccessCb,
    on_failure: &LoadFailedCb,
    cx: &mut App,
) {
    if !path.exists() {
        on_failure(
            LoadFailedContext {
                title: "Open Project Failed".to_string(),
                message: "The project file could not be found at the saved location.".to_string(),
                detail: Some(format!("Details: {}", path.display())),
                path: Some(path),
                open_options,
                rollback,
            },
            cx,
        );
        return;
    }
    match validate_project_file(&path) {
        Ok(_) => match load_project_strict(&path) {
            Ok(project) => on_success(
                LoadedSessionPackage {
                    project,
                    path,
                    open_options,
                    install_handoff: None,
                    restore_warnings: Vec::new(),
                },
                cx,
            ),
            Err(e) => on_failure(
                LoadFailedContext {
                    title: "Open Project Failed".to_string(),
                    message: e.user_message().to_string(),
                    detail: Some(format!("Details: {}", e.technical_detail())),
                    path: Some(path),
                    open_options,
                    rollback,
                },
                cx,
            ),
        },
        Err(e) => on_failure(
            LoadFailedContext {
                title: "Open Project Failed".to_string(),
                message: e.user_message().to_string(),
                detail: Some(format!("Details: {}", e.technical_detail())),
                path: Some(path),
                open_options,
                rollback,
            },
            cx,
        ),
    }
}

/// Begin pre-studio prepare for a workspace that does not need file decode
/// (New Project, Empty Project, template seed, open-dialog shell). The session
/// install is driven by the app, not by the transaction window, so a workspace
/// still reaches the studio shell when that window cannot be opened.
pub fn begin_pre_studio_workspace_prepare(
    heading: impl Into<SharedString>,
    project: FutureboardProject,
    on_success: LoadSuccessCb,
    on_failure: LoadFailedCb,
    cx: &mut App,
) -> SessionTransactionSurface {
    set_app_mode(cx, AppMode::LoadingSession);
    eprintln!("[AppMode] -> LoadingSession (workspace prepare)");
    session_log!("begin pre-studio workspace prepare");

    let heading = heading.into();
    let surface = match open_workspace_prepare_window(heading, cx) {
        Ok(handle) => {
            store_loading_session_window(cx, handle);
            SessionTransactionSurface::TransactionWindow
        }
        Err(err) => {
            session_log!("transaction window unavailable: {err} — preparing workspace headless");
            SessionTransactionSurface::Headless
        }
    };
    eprintln!(
        "[SessionLoad] workspace prepare surface={}",
        surface.label()
    );

    let package = LoadedSessionPackage {
        project,
        // Workspace prepares are not bound to a file yet; the studio shell binds
        // the real path when the follow-up finish step saves or seeds.
        path: PathBuf::from("."),
        open_options: ProjectOpenOptions::default(),
        install_handoff: None,
        restore_warnings: Vec::new(),
    };
    spawn_session_install(package, on_success, on_failure, cx);
    surface
}

/// Begin a pre-studio project open. Shows the loading window immediately and
/// only invokes `on_success` after decode/validate succeed.
pub fn begin_project_session_load(
    path: PathBuf,
    open_options: ProjectOpenOptions,
    rollback: Option<SessionRollbackSnapshot>,
    shutdown: Option<SessionShutdownSnapshot>,
    owner_bounds: Option<Bounds<Pixels>>,
    on_success: LoadSuccessCb,
    on_failure: LoadFailedCb,
    cx: &mut App,
) -> SessionTransactionSurface {
    let shutdown_reason = shutdown.as_ref().map(|snapshot| snapshot.reason);
    begin_project_session_load_inner(
        path,
        open_options,
        rollback,
        shutdown,
        None,
        shutdown_reason,
        false,
        owner_bounds,
        None,
        on_success,
        on_failure,
        cx,
    )
}

/// Close the current session with visible progress, then invoke `on_complete`.
pub fn begin_studio_session_shutdown(
    reason: SessionShutdownReason,
    studio: WindowHandle<StudioLayout>,
    owner_bounds: Option<Bounds<Pixels>>,
    clear_session_state: bool,
    on_complete: SessionShutdownCompleteCb,
    cx: &mut App,
) {
    set_app_mode(cx, AppMode::LoadingSession);
    eprintln!("[AppMode] Studio -> LoadingSession (shutdown)");
    let transaction = SessionLoadTransaction {
        path: None,
        open_options: ProjectOpenOptions::default(),
        rollback: None,
        shutdown: None,
        shutdown_reason: Some(reason),
        studio: Some(studio),
        clear_session_state,
        on_shutdown_complete: Some(on_complete.clone()),
        stage: LoadStage::SessionShutdown,
        project: None,
        on_success: Arc::new(|_, _| {}),
        on_failure: Arc::new(|_, _| {}),
    };
    match open_loading_session_window(None, transaction, owner_bounds, cx) {
        Ok(handle) => {
            store_loading_session_window(cx, handle.clone());
            let _ = handle.update(cx, |window, _win, cx| {
                window.start_progress_animation(cx);
                window.schedule_tick(cx);
            });
        }
        Err(err) => {
            session_log!("loading window unavailable for shutdown: {err}");
            on_complete(cx);
        }
    }
}

/// In-studio project switch — show the loading dialog immediately, then shut
/// down the live session asynchronously before decoding the target project.
pub fn begin_studio_project_session_load(
    path: PathBuf,
    open_options: ProjectOpenOptions,
    rollback: SessionRollbackSnapshot,
    studio: WindowHandle<StudioLayout>,
    owner_bounds: Option<Bounds<Pixels>>,
    on_success: LoadSuccessCb,
    on_failure: LoadFailedCb,
    cx: &mut App,
) -> SessionTransactionSurface {
    begin_project_session_load_inner(
        path,
        open_options,
        Some(rollback),
        None,
        Some(studio),
        Some(SessionShutdownReason::ProjectSwitch),
        false,
        owner_bounds,
        None,
        on_success,
        on_failure,
        cx,
    )
}

fn begin_project_session_load_inner(
    path: PathBuf,
    open_options: ProjectOpenOptions,
    rollback: Option<SessionRollbackSnapshot>,
    shutdown: Option<SessionShutdownSnapshot>,
    studio: Option<WindowHandle<StudioLayout>>,
    shutdown_reason: Option<SessionShutdownReason>,
    clear_session_state: bool,
    owner_bounds: Option<Bounds<Pixels>>,
    on_shutdown_complete: Option<SessionShutdownCompleteCb>,
    on_success: LoadSuccessCb,
    on_failure: LoadFailedCb,
    cx: &mut App,
) -> SessionTransactionSurface {
    set_app_mode(cx, AppMode::LoadingSession);
    session_log!("begin pre-studio load: {}", path.display());
    if studio.is_some() || shutdown.is_some() {
        eprintln!("[AppMode] Studio -> LoadingSession (project switch)");
    } else {
        eprintln!("[AppMode] Welcome -> LoadingSession");
    }

    let session_name = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .map(str::to_string);

    let rollback_for_headless = rollback.clone();
    let shutdown_for_headless = shutdown.clone();
    let initial_stage = LoadStage::initial(true, studio.is_some() || shutdown.is_some());
    let transaction = SessionLoadTransaction {
        path: Some(path.clone()),
        open_options,
        rollback,
        shutdown,
        shutdown_reason,
        studio,
        clear_session_state,
        on_shutdown_complete,
        stage: initial_stage,
        project: None,
        on_success: on_success.clone(),
        on_failure: on_failure.clone(),
    };

    match open_loading_session_window(session_name, transaction, owner_bounds, cx) {
        Ok(handle) => {
            store_loading_session_window(cx, handle.clone());
            let _ = handle.update(cx, |window, _win, cx| {
                window.start_progress_animation(cx);
                window.schedule_tick(cx);
            });
            SessionTransactionSurface::TransactionWindow
        }
        Err(err) => {
            session_log!("transaction window unavailable: {err} — loading project headless");
            if let Some(snapshot) = shutdown_for_headless {
                let _ = crate::session_shutdown::run_session_shutdown(snapshot, |_| {});
            }
            run_headless_load(
                path,
                open_options,
                rollback_for_headless,
                &on_success,
                &on_failure,
                cx,
            );
            SessionTransactionSurface::Headless
        }
    }
}

fn session_transaction_window_options(
    owner_bounds: Option<Bounds<Pixels>>,
    cx: &mut App,
) -> gpui::WindowOptions {
    use crate::components::title_bar::TITLEBAR_HEIGHT;
    use crate::window_position::{apply_owner_display, centered_window_bounds};
    use gpui::{size, WindowBackgroundAppearance, WindowBounds};

    let height = LOAD_WINDOW_HEIGHT + TITLEBAR_HEIGHT;
    let window_bounds =
        centered_window_bounds(owner_bounds, size(px(LOAD_WINDOW_WIDTH), px(height)), cx);

    let mut window_options = crate::platform_chrome::session_transaction_window_options();
    window_options.window_bounds = Some(WindowBounds::Windowed(window_bounds));
    window_options.window_background = WindowBackgroundAppearance::Transparent;
    apply_owner_display(&mut window_options, owner_bounds, cx);
    window_options
}

fn open_workspace_prepare_window(
    heading: SharedString,
    cx: &mut App,
) -> Result<WindowHandle<LoadingSessionWindow>, String> {
    let window_options = session_transaction_window_options(None, cx);
    let handle = cx
        .open_window(window_options, move |_window, cx| {
            cx.new(|cx| LoadingSessionWindow::new_progress(heading, "Preparing session", cx))
        })
        .map_err(|e| e.to_string())?;
    let _ = handle.update(cx, |window, _win, cx| window.start_progress_animation(cx));
    Ok(handle)
}

fn open_loading_session_window(
    session_name: Option<String>,
    transaction: SessionLoadTransaction,
    owner_bounds: Option<Bounds<Pixels>>,
    cx: &mut App,
) -> Result<WindowHandle<LoadingSessionWindow>, String> {
    let window_options = session_transaction_window_options(owner_bounds, cx);
    cx.open_window(window_options, move |_window, cx| {
        cx.new(|cx| LoadingSessionWindow::new_for_load(session_name, transaction, cx))
    })
    .map_err(|e| e.to_string())
}

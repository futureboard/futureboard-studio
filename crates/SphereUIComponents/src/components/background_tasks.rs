//! Background Tasks: every long job Studio runs, and the status-bar popup that
//! shows them.
//!
//! The [`BackgroundTaskStore`] belongs to the studio layout, which draws it.
//! Jobs report to it in one of two ways:
//!
//! * the layout's own helpers (`start_background_task`, `complete_…`), for work
//!   the layout drives on the UI thread, and
//! * a [`BackgroundJob`] guard, for work anywhere else — a worker thread, a
//!   component with no layout to call, a tool window. The guard posts to a
//!   process-wide inbox the layout drains on its status tick, so a job never
//!   has to reach the UI thread to say how far it has got, and the inbox is
//!   never touched from an audio callback.
//!
//! A job says how it ended: [`BackgroundJob::complete`] or
//! [`BackgroundJob::fail`]. One dropped without either is recorded as stopped,
//! never as a success it may not have been.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, svg, App, InteractiveElement, IntoElement, ParentElement, StatefulInteractiveElement,
    Styled, Window,
};

use crate::assets;
use crate::theme::{radius, space, typography, Colors};

pub type BackgroundTaskToggleCb = Arc<dyn Fn(&(), &mut Window, &mut App) + 'static>;
pub type BackgroundTaskCancelCb = Arc<dyn Fn(&String, &mut Window, &mut App) + 'static>;

/// What the popup's rows and header can ask of the store's owner.
#[derive(Clone)]
pub struct BackgroundTaskActions {
    pub on_close: BackgroundTaskToggleCb,
    pub on_cancel: BackgroundTaskCancelCb,
    /// Take a failed or finished job off the list.
    pub on_dismiss: BackgroundTaskCancelCb,
    pub on_clear_finished: BackgroundTaskToggleCb,
}

const RECENT_COMPLETE_MS: u64 = 30_000;
const KEEP_COMPLETED: usize = 20;
const FINISHED_SHOWN: usize = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BackgroundTaskKind {
    Import,
    MediaCopy,
    MetadataScan,
    Waveform,
    PeakGeneration,
    PeakLoading,
    NativeSync,
    ProjectSave,
    Recording,
    /// Opening a project: reading it, restoring its plug-ins.
    ProjectLoad,
    /// Loading and restoring plug-ins.
    PluginLoad,
    /// Rendering audio: export, bounce, freeze.
    Render,
    /// Offline audio processing: stretch, tempo detection, repair, stems.
    AudioProcess,
    SampleRateConversion,
    /// A plug-in or tool reading the audio it works on (ARA analysis).
    Analysis,
    /// Looking for media files that moved.
    MediaSearch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundTaskStatus {
    Queued,
    Running,
    Paused,
    Complete,
    Failed,
    Cancelled,
}

impl BackgroundTaskStatus {
    pub fn is_live(self) -> bool {
        matches!(self, Self::Queued | Self::Running | Self::Paused)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackgroundTaskProgress {
    pub current: u32,
    pub total: u32,
}

impl BackgroundTaskProgress {
    pub fn fraction(self) -> Option<f32> {
        (self.total > 0).then(|| (self.current as f32 / self.total as f32).clamp(0.0, 1.0))
    }
}

#[derive(Debug, Clone)]
pub struct BackgroundTask {
    pub id: String,
    pub kind: BackgroundTaskKind,
    pub title: String,
    pub detail: Option<String>,
    pub status: BackgroundTaskStatus,
    pub progress: Option<BackgroundTaskProgress>,
    pub started_at: Option<u64>,
    pub updated_at: u64,
    /// When it stopped running, however it ended.
    pub finished_at: Option<u64>,
    pub error: Option<String>,
    pub cancellable: bool,
    pub parent_id: Option<String>,
}

impl BackgroundTask {
    /// How long it ran, or has been running.
    pub fn elapsed_ms(&self, now: u64) -> Option<u64> {
        let started = self.started_at?;
        Some(self.finished_at.unwrap_or(now).saturating_sub(started))
    }
}

#[derive(Debug, Clone)]
pub struct BackgroundTaskUpdate {
    pub kind: BackgroundTaskKind,
    pub title: String,
    pub detail: Option<String>,
    pub status: BackgroundTaskStatus,
    pub progress: Option<BackgroundTaskProgress>,
    pub error: Option<String>,
    pub cancellable: bool,
    pub parent_id: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct BackgroundTaskStore {
    pub tasks: HashMap<String, BackgroundTask>,
    pub panel_open: bool,
    /// Bumped on every change, so the status bar repaints only when the list
    /// moved.
    pub revision: u64,
}

#[derive(Debug, Clone)]
pub struct BackgroundTaskSummary {
    pub label: String,
    pub tone: BackgroundTaskTone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundTaskTone {
    Idle,
    Active,
    Info,
    Warning,
    Error,
}

impl BackgroundTaskStore {
    pub fn add_or_update(&mut self, id: impl Into<String>, update: BackgroundTaskUpdate) {
        let id = id.into();
        let now = now_ms();
        let previous = self.tasks.get(&id);
        let started_at = if update.status == BackgroundTaskStatus::Running {
            previous.and_then(|task| task.started_at).or(Some(now))
        } else {
            previous.and_then(|task| task.started_at)
        };
        let finished_at = (!update.status.is_live()).then_some(now);
        self.tasks.insert(
            id.clone(),
            BackgroundTask {
                id,
                kind: update.kind,
                title: update.title,
                detail: update.detail,
                status: update.status,
                progress: update.progress,
                started_at,
                updated_at: now,
                finished_at,
                error: update.error,
                cancellable: update.cancellable,
                parent_id: update.parent_id,
            },
        );
        self.revision += 1;
        self.prune_completed();
    }

    pub fn complete(&mut self, id: &str, detail: Option<String>) {
        self.patch_status(id, BackgroundTaskStatus::Complete, detail, None);
    }

    pub fn fail(&mut self, id: &str, error: impl Into<String>) {
        self.patch_status(id, BackgroundTaskStatus::Failed, None, Some(error.into()));
    }

    /// Stop a job: the user's Cancel. A job that reports through a
    /// [`BackgroundJob`] is told, and stops at its next check.
    pub fn cancel(&mut self, id: &str) {
        request_job_cancel(id);
        self.patch_status(id, BackgroundTaskStatus::Cancelled, None, None);
    }

    /// Take a job that has ended off the list. A live one stays.
    pub fn dismiss(&mut self, id: &str) {
        if self
            .tasks
            .get(id)
            .is_some_and(|task| !task.status.is_live())
        {
            self.tasks.remove(id);
            self.revision += 1;
        }
    }

    /// Remove every job that has finished or been stopped. Failures stay
    /// until they are dismissed one by one: they are what needs reading.
    pub fn clear_finished(&mut self) {
        let before = self.tasks.len();
        self.tasks.retain(|_, task| {
            !matches!(
                task.status,
                BackgroundTaskStatus::Complete | BackgroundTaskStatus::Cancelled
            )
        });
        if self.tasks.len() != before {
            self.revision += 1;
        }
    }

    pub fn set_panel_open(&mut self, open: bool) {
        if self.panel_open != open {
            self.panel_open = open;
            self.revision += 1;
        }
    }

    pub fn toggle_panel(&mut self) {
        self.panel_open = !self.panel_open;
        self.revision += 1;
    }

    pub fn running_count(&self) -> usize {
        self.tasks
            .values()
            .filter(|task| task.status.is_live())
            .count()
    }

    pub fn sorted_tasks(&self) -> Vec<BackgroundTask> {
        let mut tasks = self.tasks.values().cloned().collect::<Vec<_>>();
        tasks.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        tasks
    }

    /// Apply what [`BackgroundJob`]s posted since the last drain. Returns
    /// whether anything changed.
    pub fn drain_job_inbox(&mut self) -> bool {
        let events = take_job_events();
        if events.is_empty() {
            return false;
        }
        for event in events {
            match event {
                JobEvent::Update(id, update) => {
                    // A job the user already cancelled stays cancelled, even
                    // if it posts once more before it notices.
                    if self
                        .tasks
                        .get(&id)
                        .is_some_and(|task| task.status == BackgroundTaskStatus::Cancelled)
                    {
                        continue;
                    }
                    self.add_or_update(id, update);
                }
                JobEvent::Complete(id, detail) => self.complete(&id, detail),
                JobEvent::Fail(id, error) => self.fail(&id, error),
                JobEvent::Stopped(id) => {
                    self.patch_status(&id, BackgroundTaskStatus::Cancelled, None, None)
                }
            }
        }
        true
    }

    pub fn summary(&self) -> BackgroundTaskSummary {
        let tasks = self.tasks.values().collect::<Vec<_>>();
        let failed = tasks
            .iter()
            .filter(|task| task.status == BackgroundTaskStatus::Failed)
            .count();
        if failed > 0 {
            return BackgroundTaskSummary {
                label: format!(
                    "{failed} background job{} failed",
                    if failed == 1 { "" } else { "s" }
                ),
                tone: BackgroundTaskTone::Error,
            };
        }
        let mut live: Vec<&&BackgroundTask> = tasks
            .iter()
            .filter(|task| task.status == BackgroundTaskStatus::Running)
            .collect();
        // The longest-running job names the pill; the rest are counted.
        live.sort_by_key(|task| task.started_at.unwrap_or(u64::MAX));
        if let Some(first) = live.first() {
            let others = live.len() - 1;
            let mut label = progress_label(&first.title, first);
            if others > 0 {
                label.push_str(&format!(" +{others}"));
            }
            let tone = match first.kind {
                BackgroundTaskKind::NativeSync | BackgroundTaskKind::ProjectSave => {
                    BackgroundTaskTone::Warning
                }
                BackgroundTaskKind::Waveform
                | BackgroundTaskKind::PeakGeneration
                | BackgroundTaskKind::PeakLoading => BackgroundTaskTone::Info,
                _ => BackgroundTaskTone::Active,
            };
            return BackgroundTaskSummary { label, tone };
        }
        let queued = tasks
            .iter()
            .filter(|task| task.status == BackgroundTaskStatus::Queued)
            .count();
        if queued > 0 {
            return BackgroundTaskSummary {
                label: format!("{queued} job{} waiting", if queued == 1 { "" } else { "s" }),
                tone: BackgroundTaskTone::Info,
            };
        }
        BackgroundTaskSummary {
            label: "Background idle".to_string(),
            tone: BackgroundTaskTone::Idle,
        }
    }

    fn patch_status(
        &mut self,
        id: &str,
        status: BackgroundTaskStatus,
        detail: Option<String>,
        error: Option<String>,
    ) {
        let Some(task) = self.tasks.get_mut(id) else {
            return;
        };
        let now = now_ms();
        task.status = status;
        if detail.is_some() {
            task.detail = detail;
        }
        task.error = error;
        task.updated_at = now;
        if !status.is_live() {
            task.finished_at.get_or_insert(now);
            task.cancellable = false;
        }
        self.revision += 1;
        self.prune_completed();
    }

    fn prune_completed(&mut self) {
        let now = now_ms();
        let mut completed = self
            .tasks
            .values()
            .filter(|task| {
                matches!(
                    task.status,
                    BackgroundTaskStatus::Complete | BackgroundTaskStatus::Cancelled
                )
            })
            .cloned()
            .collect::<Vec<_>>();
        completed.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        let keep = completed
            .into_iter()
            .enumerate()
            .filter(|(index, task)| {
                *index < KEEP_COMPLETED || now.saturating_sub(task.updated_at) < RECENT_COMPLETE_MS
            })
            .map(|(_, task)| task.id)
            .collect::<std::collections::HashSet<_>>();
        self.tasks.retain(|id, task| {
            !matches!(
                task.status,
                BackgroundTaskStatus::Complete | BackgroundTaskStatus::Cancelled
            ) || keep.contains(id)
        });
    }
}

// ── The job inbox ───────────────────────────────────────────────────────────

enum JobEvent {
    Update(String, BackgroundTaskUpdate),
    Complete(String, Option<String>),
    Fail(String, String),
    Stopped(String),
}

#[derive(Default)]
struct JobInbox {
    events: Vec<JobEvent>,
    /// Cancel flags of the jobs that can be stopped, by id.
    cancel_flags: HashMap<String, Arc<AtomicBool>>,
}

fn inbox() -> &'static Mutex<JobInbox> {
    static INBOX: OnceLock<Mutex<JobInbox>> = OnceLock::new();
    INBOX.get_or_init(|| Mutex::new(JobInbox::default()))
}

/// Undrained posts kept at most (no Studio window to drain them — a test, a
/// headless run). Past it the oldest go first.
const INBOX_CAPACITY: usize = 4096;

fn post(event: JobEvent) {
    if let Ok(mut inbox) = inbox().lock() {
        if inbox.events.len() >= INBOX_CAPACITY {
            let overflow = inbox.events.len() + 1 - INBOX_CAPACITY;
            inbox.events.drain(..overflow);
        }
        inbox.events.push(event);
    }
}

fn take_job_events() -> Vec<JobEvent> {
    inbox()
        .lock()
        .map(|mut inbox| std::mem::take(&mut inbox.events))
        .unwrap_or_default()
}

fn request_job_cancel(id: &str) {
    if let Ok(inbox) = inbox().lock() {
        if let Some(flag) = inbox.cancel_flags.get(id) {
            flag.store(true, Ordering::Release);
        }
    }
}

/// Progress posts at most this often: a job reporting per block would
/// otherwise fill the inbox faster than the status tick drains it.
const PROGRESS_POST_INTERVAL_MS: u128 = 100;

struct JobProgressState {
    detail: Option<String>,
    progress: Option<BackgroundTaskProgress>,
    last_post: Instant,
}

/// One running job, reported to the Background Tasks popup for as long as
/// the guard lives. `Send`, so it can go to the thread doing the work.
pub struct BackgroundJob {
    id: String,
    kind: BackgroundTaskKind,
    title: String,
    cancellable: bool,
    cancel: Arc<AtomicBool>,
    state: Mutex<JobProgressState>,
    ended: bool,
}

impl BackgroundJob {
    /// Report a job that runs to its end.
    pub fn start(kind: BackgroundTaskKind, title: impl Into<String>) -> Self {
        Self::begin(kind, title.into(), false)
    }

    /// Report a job the user can stop from the popup; the work polls
    /// [`Self::is_cancelled`].
    pub fn start_cancellable(kind: BackgroundTaskKind, title: impl Into<String>) -> Self {
        Self::begin(kind, title.into(), true)
    }

    fn begin(kind: BackgroundTaskKind, title: String, cancellable: bool) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let id = format!("job-{}", NEXT.fetch_add(1, Ordering::Relaxed));
        let cancel = Arc::new(AtomicBool::new(false));
        if cancellable {
            if let Ok(mut inbox) = inbox().lock() {
                inbox.cancel_flags.insert(id.clone(), cancel.clone());
            }
        }
        let job = Self {
            id,
            kind,
            title,
            cancellable,
            cancel,
            state: Mutex::new(JobProgressState {
                detail: None,
                progress: None,
                last_post: Instant::now(),
            }),
            ended: false,
        };
        job.post_update(None, None);
        job
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// Whether the user pressed Cancel on it.
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Acquire)
    }

    /// What it is doing now ("Track 3 of 8", a file name).
    pub fn set_detail(&self, detail: impl Into<String>) {
        let detail = Some(detail.into());
        let progress = self.state.lock().ok().and_then(|state| state.progress);
        self.post_update(detail, progress);
    }

    /// How far it has got. Posts are throttled; the last one always lands
    /// with the outcome.
    pub fn set_progress(&self, current: u32, total: u32) {
        let progress = Some(BackgroundTaskProgress { current, total });
        let due = self.state.lock().is_ok_and(|mut state| {
            state.progress = progress;
            let due = current >= total
                || state.last_post.elapsed().as_millis() >= PROGRESS_POST_INTERVAL_MS;
            if due {
                state.last_post = Instant::now();
            }
            due
        });
        if due {
            let detail = self
                .state
                .lock()
                .ok()
                .and_then(|state| state.detail.clone());
            self.post_update(detail, progress);
        }
    }

    fn post_update(&self, detail: Option<String>, progress: Option<BackgroundTaskProgress>) {
        if let Ok(mut state) = self.state.lock() {
            state.detail = detail.clone();
            state.progress = progress;
        }
        post(JobEvent::Update(
            self.id.clone(),
            BackgroundTaskUpdate {
                kind: self.kind,
                title: self.title.clone(),
                detail,
                status: BackgroundTaskStatus::Running,
                progress,
                error: None,
                cancellable: self.cancellable,
                parent_id: None,
            },
        ));
    }

    /// It finished; `detail` says with what ("4 files", "-14.1 LUFS").
    pub fn complete(mut self, detail: Option<String>) {
        self.ended = true;
        post(JobEvent::Complete(self.id.clone(), detail));
    }

    /// It failed, and why.
    pub fn fail(mut self, error: impl Into<String>) {
        self.ended = true;
        post(JobEvent::Fail(self.id.clone(), error.into()));
    }
}

impl Drop for BackgroundJob {
    fn drop(&mut self) {
        if self.cancellable {
            if let Ok(mut inbox) = inbox().lock() {
                inbox.cancel_flags.remove(&self.id);
            }
        }
        if !self.ended {
            // No outcome was given: it was stopped, cancelled or unwound.
            // Recorded as such, never as a success.
            post(JobEvent::Stopped(self.id.clone()));
        }
    }
}

// ── The status-bar pill and the popup ───────────────────────────────────────

pub fn background_task_button(
    store: &BackgroundTaskStore,
    on_toggle: BackgroundTaskToggleCb,
) -> impl IntoElement {
    let summary = store.summary();
    let color = tone_color(summary.tone);
    let busy = store.running_count() > 0;
    div()
        .id("background-task-status")
        .h(px(18.0))
        .max_w(px(280.0))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .px(px(7.0))
        .rounded(px(radius::CONTROL))
        .border(px(1.0))
        .border_color(if store.panel_open {
            Colors::border_accent()
        } else {
            Colors::border_subtle()
        })
        .bg(if store.panel_open {
            Colors::accent_muted()
        } else {
            Colors::surface_input()
        })
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(|s| s.bg(Colors::surface_hover()))
        .tooltip(crate::components::controls::fb_tooltip("Background tasks"))
        .on_click(move |_, w, cx| on_toggle(&(), w, cx))
        .child(
            div()
                .w(px(if busy { 6.0 } else { 5.0 }))
                .h(px(if busy { 6.0 } else { 5.0 }))
                .rounded(px(radius::PILL))
                .bg(color),
        )
        .child(
            div()
                .truncate()
                .text_size(px(10.0))
                .text_color(Colors::text_secondary())
                .child(summary.label),
        )
}

pub fn background_task_panel(
    store: &BackgroundTaskStore,
    actions: BackgroundTaskActions,
) -> impl IntoElement {
    let now = now_ms();
    let tasks = store.sorted_tasks();
    let mut running: Vec<BackgroundTask> = tasks
        .iter()
        .filter(|task| task.status.is_live())
        .cloned()
        .collect();
    // Oldest first: the job that has been going longest leads.
    running.sort_by_key(|task| {
        (
            task.status != BackgroundTaskStatus::Running,
            task.started_at,
        )
    });
    let failed: Vec<BackgroundTask> = tasks
        .iter()
        .filter(|task| task.status == BackgroundTaskStatus::Failed)
        .cloned()
        .collect();
    let finished: Vec<BackgroundTask> = tasks
        .iter()
        .filter(|task| {
            matches!(
                task.status,
                BackgroundTaskStatus::Complete | BackgroundTaskStatus::Cancelled
            )
        })
        .take(FINISHED_SHOWN)
        .cloned()
        .collect();
    let has_finished = !finished.is_empty();

    let counts = header_counts(running.len(), failed.len(), finished.len());
    let on_close = actions.on_close.clone();
    let on_clear = actions.on_clear_finished.clone();

    let mut list = div()
        .id("background-task-panel-scroll")
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        .p(px(space::SNUG))
        .flex()
        .flex_col()
        .gap(px(space::BASE));
    if running.is_empty() && failed.is_empty() && finished.is_empty() {
        list = list.child(
            div()
                .py(px(18.0))
                .px(px(space::BASE))
                .flex()
                .flex_col()
                .items_center()
                .gap(px(space::TIGHT))
                .child(
                    div()
                        .text_size(px(typography::DENSE_LABEL))
                        .text_color(Colors::text_secondary())
                        .child("Nothing running"),
                )
                .child(
                    div()
                        .text_size(px(typography::DENSE_CAPTION))
                        .text_color(Colors::text_faint())
                        .child(
                            "Imports, saves, renders, analysis and plug-in loading show here while they work.",
                        ),
                ),
        );
    }
    if !running.is_empty() {
        list = list.child(section(
            "Running",
            running
                .into_iter()
                .map(|task| task_row(task, now, &actions).into_any_element())
                .collect(),
        ));
    }
    if !failed.is_empty() {
        list = list.child(section(
            "Failed",
            failed
                .into_iter()
                .map(|task| task_row(task, now, &actions).into_any_element())
                .collect(),
        ));
    }
    if has_finished {
        list = list.child(section(
            "Finished",
            finished
                .into_iter()
                .map(|task| task_row(task, now, &actions).into_any_element())
                .collect(),
        ));
    }

    div()
        .absolute()
        .right(px(12.0))
        .bottom(px(26.0))
        .w(px(400.0))
        .max_h(px(460.0))
        .flex()
        .flex_col()
        .rounded(px(radius::SURFACE))
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_panel())
        .shadow(vec![gpui::BoxShadow {
            color: Colors::surface_overlay().into(),
            offset: gpui::point(px(0.0), px(16.0)),
            blur_radius: px(34.0),
            spread_radius: px(0.0),
            inset: false,
        }])
        .occlude()
        .child(
            div()
                .flex()
                .flex_row()
                .flex_none()
                .items_center()
                .gap(px(space::SNUG))
                .h(px(34.0))
                .px(px(space::BASE))
                .border_b(px(1.0))
                .border_color(Colors::border_subtle())
                .child(
                    div()
                        .text_size(px(typography::DENSE_LABEL + 1.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(Colors::text_primary())
                        .child("Background Tasks"),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(typography::DENSE_CAPTION))
                        .text_color(Colors::text_faint())
                        .child(counts),
                )
                .when(has_finished, |row| {
                    row.child(text_button(
                        "background-task-clear",
                        "Clear finished",
                        move |w, cx| on_clear(&(), w, cx),
                    ))
                })
                .child(icon_button(
                    "background-task-panel-close",
                    assets::ICON_X_PATH,
                    "Close",
                    move |w, cx| on_close(&(), w, cx),
                )),
        )
        .child(list)
}

fn header_counts(running: usize, failed: usize, finished: usize) -> String {
    let mut parts = Vec::new();
    if running > 0 {
        parts.push(format!("{running} running"));
    }
    if failed > 0 {
        parts.push(format!("{failed} failed"));
    }
    if finished > 0 {
        parts.push(format!("{finished} finished"));
    }
    parts.join(" · ")
}

fn section(title: &'static str, rows: Vec<gpui::AnyElement>) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(space::TIGHT))
        .child(
            div()
                .px(px(2.0))
                .text_size(px(typography::DENSE_CAPTION))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(Colors::text_faint())
                .child(title),
        )
        .children(rows)
}

fn task_row(task: BackgroundTask, now: u64, actions: &BackgroundTaskActions) -> impl IntoElement {
    let pct = task.progress.and_then(BackgroundTaskProgress::fraction);
    let element_id = stable_task_id(&task.id);
    let live = task.status.is_live();
    let failed = task.status == BackgroundTaskStatus::Failed;
    let accent = match task.status {
        BackgroundTaskStatus::Failed => Colors::status_error(),
        BackgroundTaskStatus::Cancelled => Colors::text_faint(),
        BackgroundTaskStatus::Complete => Colors::status_success(),
        _ => Colors::accent_primary(),
    };
    let timing = match (task.status, task.elapsed_ms(now)) {
        (BackgroundTaskStatus::Queued, _) => "waiting".to_string(),
        (_, Some(ms)) if live => format_duration(ms),
        (_, Some(ms)) => format!(
            "{} · {}",
            format_duration(ms),
            ago(now.saturating_sub(task.finished_at.unwrap_or(task.updated_at)))
        ),
        (_, None) => ago(now.saturating_sub(task.updated_at)),
    };
    let progress_text = task.progress.filter(|p| p.total > 0).map(|p| {
        format!(
            "{}%",
            ((p.current as f32 / p.total as f32) * 100.0).round() as u32
        )
    });

    let cancel = (live && task.cancellable).then(|| {
        let on_cancel = actions.on_cancel.clone();
        let id = task.id.clone();
        text_button(
            ("background-task-cancel", element_id),
            "Cancel",
            move |w, cx| on_cancel(&id, w, cx),
        )
    });
    let dismiss = (!live).then(|| {
        let on_dismiss = actions.on_dismiss.clone();
        let id = task.id.clone();
        icon_button(
            ("background-task-dismiss", element_id),
            assets::ICON_X_PATH,
            "Dismiss",
            move |w, cx| on_dismiss(&id, w, cx),
        )
    });

    div()
        .flex()
        .flex_col()
        .gap(px(3.0))
        .px(px(space::SNUG))
        .py(px(6.0))
        .rounded(px(radius::CONTROL))
        .border(px(1.0))
        .border_color(if failed {
            Colors::with_alpha(Colors::status_error(), 0.45)
        } else {
            Colors::border_subtle()
        })
        .bg(Colors::surface_input())
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::SNUG))
                .child(
                    svg()
                        .path(kind_icon(task.kind))
                        .flex_none()
                        .w(px(13.0))
                        .h(px(13.0))
                        .text_color(accent),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(typography::DENSE_LABEL))
                        .text_color(if task.status == BackgroundTaskStatus::Cancelled {
                            Colors::text_muted()
                        } else {
                            Colors::text_primary()
                        })
                        .child(task.title.clone()),
                )
                .children(progress_text.map(|text| {
                    div()
                        .flex_none()
                        .text_size(px(typography::DENSE_CAPTION))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(Colors::text_secondary())
                        .child(text)
                }))
                .child(
                    div()
                        .flex_none()
                        .text_size(px(typography::DENSE_CAPTION))
                        .text_color(Colors::text_faint())
                        .child(timing),
                )
                .children(cancel)
                .children(dismiss),
        )
        .children(status_line(&task).map(|(text, color)| {
            div()
                .pl(px(21.0))
                .truncate()
                .text_size(px(typography::DENSE_CAPTION))
                .text_color(color)
                .child(text)
        }))
        .when(live, |row| {
            row.child(
                div()
                    .ml(px(21.0))
                    .h(px(3.0))
                    .rounded(px(radius::PILL))
                    .overflow_hidden()
                    .bg(Colors::border_subtle())
                    .child(
                        div()
                            .h_full()
                            .rounded(px(radius::PILL))
                            .bg(Colors::accent_primary())
                            // Without a measure, a short bar that says
                            // "working", not a made-up percentage.
                            .w(gpui::relative(pct.unwrap_or(0.12))),
                    ),
            )
        })
}

/// The line under the title: the error, what it is doing, or how it ended.
fn status_line(task: &BackgroundTask) -> Option<(String, gpui::Rgba)> {
    if let Some(error) = task.error.clone() {
        return Some((error, Colors::status_error()));
    }
    match task.status {
        BackgroundTaskStatus::Cancelled => Some((
            task.detail
                .clone()
                .map(|detail| format!("Stopped · {detail}"))
                .unwrap_or_else(|| "Stopped".to_string()),
            Colors::text_faint(),
        )),
        _ => task
            .detail
            .clone()
            .map(|detail| (detail, Colors::text_faint())),
    }
}

fn text_button(
    id: impl Into<gpui::ElementId>,
    label: &'static str,
    on_click: impl Fn(&mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .flex_none()
        .px(px(6.0))
        .py(px(1.0))
        .rounded(px(radius::CONTROL))
        .text_size(px(typography::DENSE_CAPTION))
        .text_color(Colors::text_secondary())
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(|s| {
            s.bg(Colors::surface_hover())
                .text_color(Colors::text_primary())
        })
        .on_click(move |_, w, cx| on_click(w, cx))
        .child(label)
}

fn icon_button(
    id: impl Into<gpui::ElementId>,
    icon: &'static str,
    tip: &'static str,
    on_click: impl Fn(&mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .w(px(18.0))
        .h(px(18.0))
        .rounded(px(radius::CONTROL))
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(|s| s.bg(Colors::surface_hover()))
        .tooltip(crate::components::controls::fb_tooltip(tip))
        .on_click(move |_, w, cx| on_click(w, cx))
        .child(
            svg()
                .path(icon)
                .w(px(10.0))
                .h(px(10.0))
                .text_color(Colors::text_muted()),
        )
}

fn progress_label(prefix: &str, task: &BackgroundTask) -> String {
    if let Some(progress) = task.progress {
        if progress.total > 0 {
            return format!("{prefix} {}/{}", progress.current, progress.total);
        }
    }
    prefix.to_string()
}

fn kind_icon(kind: BackgroundTaskKind) -> &'static str {
    match kind {
        BackgroundTaskKind::Import => assets::ICON_FOLDER_OPEN_PATH,
        BackgroundTaskKind::MediaCopy => assets::ICON_HARD_DRIVE_PATH,
        BackgroundTaskKind::MetadataScan | BackgroundTaskKind::MediaSearch => {
            assets::ICON_SEARCH_PATH
        }
        BackgroundTaskKind::Waveform
        | BackgroundTaskKind::PeakGeneration
        | BackgroundTaskKind::PeakLoading => assets::ICON_AUDIO_LINES_PATH,
        BackgroundTaskKind::NativeSync => assets::ICON_REFRESH_CW_PATH,
        BackgroundTaskKind::ProjectSave => assets::ICON_SAVE_PATH,
        BackgroundTaskKind::Recording => assets::ICON_MIC_PATH,
        BackgroundTaskKind::ProjectLoad => assets::ICON_FOLDER_PATH,
        BackgroundTaskKind::PluginLoad => assets::ICON_PLUG_PATH,
        BackgroundTaskKind::Render => assets::ICON_SHARE_PATH,
        BackgroundTaskKind::AudioProcess => assets::ICON_SPARKLES_PATH,
        BackgroundTaskKind::SampleRateConversion => assets::ICON_REPEAT2_PATH,
        BackgroundTaskKind::Analysis => assets::ICON_GAUGE_PATH,
    }
}

fn tone_color(tone: BackgroundTaskTone) -> gpui::Rgba {
    match tone {
        BackgroundTaskTone::Idle => Colors::status_success(),
        BackgroundTaskTone::Active => Colors::accent_primary(),
        BackgroundTaskTone::Info => Colors::accent_primary(),
        BackgroundTaskTone::Warning => Colors::status_warning(),
        BackgroundTaskTone::Error => Colors::status_error(),
    }
}

/// "0:07", "2:14", "1:02:30".
fn format_duration(ms: u64) -> String {
    let secs = ms / 1000;
    if secs >= 3600 {
        format!("{}:{:02}:{:02}", secs / 3600, (secs / 60) % 60, secs % 60)
    } else {
        format!("{}:{:02}", secs / 60, secs % 60)
    }
}

fn ago(ms: u64) -> String {
    let secs = ms / 1000;
    match secs {
        0..=4 => "just now".to_string(),
        5..=59 => format!("{secs}s ago"),
        60..=3599 => format!("{}m ago", secs / 60),
        _ => format!("{}h ago", secs / 3600),
    }
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn stable_task_id(id: &str) -> u64 {
    let mut hash = 14_695_981_039_346_656_037u64;
    for byte in id.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(1_099_511_628_211);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The inbox is process-wide; these tests drain it, so they take turns.
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn drain_into(store: &mut BackgroundTaskStore, id: &str) -> BackgroundTask {
        store.drain_job_inbox();
        store.tasks.get(id).cloned().expect("the job is listed")
    }

    #[test]
    fn a_job_reports_its_progress_and_its_outcome() {
        let _serial = serial();
        let mut store = BackgroundTaskStore::default();
        let job = BackgroundJob::start(BackgroundTaskKind::Render, "Export Mixdown");
        let id = job.id().to_string();
        assert_eq!(
            drain_into(&mut store, &id).status,
            BackgroundTaskStatus::Running
        );
        job.set_detail("Track 2 of 4");
        job.set_progress(4, 4);
        let task = drain_into(&mut store, &id);
        assert_eq!(task.detail.as_deref(), Some("Track 2 of 4"));
        assert_eq!(
            task.progress.and_then(BackgroundTaskProgress::fraction),
            Some(1.0)
        );
        job.complete(Some("Written".into()));
        let task = drain_into(&mut store, &id);
        assert_eq!(task.status, BackgroundTaskStatus::Complete);
        assert!(task.finished_at.is_some());
    }

    /// A job dropped without an outcome is not passed off as a success.
    #[test]
    fn a_job_dropped_without_an_outcome_reads_as_stopped() {
        let _serial = serial();
        let mut store = BackgroundTaskStore::default();
        let id = {
            let job = BackgroundJob::start(BackgroundTaskKind::AudioProcess, "Stretch");
            job.id().to_string()
        };
        assert_eq!(
            drain_into(&mut store, &id).status,
            BackgroundTaskStatus::Cancelled
        );
    }

    #[test]
    fn cancel_reaches_the_job_and_sticks() {
        let _serial = serial();
        let mut store = BackgroundTaskStore::default();
        let job = BackgroundJob::start_cancellable(BackgroundTaskKind::Analysis, "Analyse");
        let id = job.id().to_string();
        store.drain_job_inbox();
        store.cancel(&id);
        assert!(job.is_cancelled());
        // It posts once more before it notices; the list keeps it stopped.
        job.set_detail("still going");
        assert_eq!(
            drain_into(&mut store, &id).status,
            BackgroundTaskStatus::Cancelled
        );
    }

    #[test]
    fn failures_survive_clear_finished_until_dismissed() {
        let _serial = serial();
        let mut store = BackgroundTaskStore::default();
        let ok = BackgroundJob::start(BackgroundTaskKind::Import, "Import");
        let bad = BackgroundJob::start(BackgroundTaskKind::Import, "Import 2");
        let (ok_id, bad_id) = (ok.id().to_string(), bad.id().to_string());
        ok.complete(None);
        bad.fail("Unsupported file");
        store.drain_job_inbox();
        store.clear_finished();
        assert!(!store.tasks.contains_key(&ok_id));
        assert!(store.tasks.contains_key(&bad_id));
        store.dismiss(&bad_id);
        assert!(!store.tasks.contains_key(&bad_id));
    }

    #[test]
    fn durations_read_like_a_clock() {
        assert_eq!(format_duration(7_400), "0:07");
        assert_eq!(format_duration(134_000), "2:14");
        assert_eq!(format_duration(3_750_000), "1:02:30");
    }
}

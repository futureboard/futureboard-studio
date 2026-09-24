//! Native "Export Arrangement" dialog.
//!
//! The window owns a plain [`EngineProjectSnapshot`] + [`ExportProjectDefaults`]
//! captured when it opens, edits an [`ExportSettings`], and — on Export — spawns
//! a background thread that runs the engine's `export_arrangement`. Progress
//! flows back through a shared `Mutex` polled by a GPUI timer loop; the worker
//! thread never touches GPUI and the UI never blocks. No entity is leased during
//! the render/encode work.
//!
//! Three rules shape the layout.
//!
//! * **One scroll owner, one clip owner.** The root clips (that is what makes
//!   `radius::DIALOG` cut the square titlebar), the body `export-body` is the
//!   only scroller, and the status strip plus the action footer stay pinned. A
//!   short or narrow window scrolls its form instead of hiding it.
//! * **Readouts come from the request, not from a parallel calculation.**
//!   Everything numeric is read out of [`ExportSettings::estimate`], which is
//!   built from the same `ArrangementExportRequest` the engine receives, so a
//!   number shown here cannot disagree with the file that gets written.
//! * **Nothing on screen is decorative.** Every control writes a value the
//!   encoder or the offline renderer consumes; options the project cannot
//!   satisfy are disabled and say why, or are absent entirely.

use std::ops::Range;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use gpui::{
    div, px, App, AppContext, Bounds, Context, Entity, EntityInputHandler, FocusHandle, FontWeight,
    InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Pixels, Point, Render,
    StatefulInteractiveElement, Styled, UTF16Selection, Window, WindowHandle,
};

use sphere_encoder::AudioFileFormat;
use DirectAudio::plugin_bridge::PluginBridgeSinkMap;
use DirectAudio::types::EngineProjectSnapshot;
use DirectAudio::{
    export_arrangement_with_bridges, export_tracks_single_pass_with_bridges,
    ArrangementExportRequest, ArrangementExportSummary, ExportCancelToken, ExportProgress,
    ExportStage, TrackExportTarget,
};

use crate::assets;
use crate::components::controls::{
    fb_button, fb_section_header, fb_segment, fb_segmented_track, FbButtonKind, FbSegment,
};
use crate::components::form::select::{select, select_dismiss_backdrop, SelectOption};
use crate::components::message_box_dialog::{
    open_message_box_window, MessageBoxKind, MessageBoxOptions, MessageBoxResponseCb,
    MessageBoxResult,
};
use crate::components::progress_dialog::{progress_bar, ProgressBarValue};
use crate::components::text_input::{bind_mouse_selection, text_field_with_callbacks_and_ime};
use crate::components::title_bar::{external_window_titlebar_with_icon, TITLEBAR_HEIGHT};
use crate::components::{TextInputAction, TextInputState};
use crate::i18n::I18n;
use crate::theme::{self, elevation, radius, size, space, typography, Colors};

use super::export_settings::{
    ExportChannelMode, ExportEstimate, ExportMode, ExportNormalizeChoice, ExportProjectDefaults,
    ExportRangeChoice, ExportSampleRateChoice, ExportSettings, ExportSettingsError,
    ExportTailChoice, FLAC_COMPRESSION_RANGE, PEAK_TARGETS_DB, TAIL_FIXED_SECONDS,
    TAIL_SILENCE_MAX_SECONDS, TAIL_SILENCE_THRESHOLD_DB,
};

// ── Window geometry ──────────────────────────────────────────────────────────

pub const EXPORT_WINDOW_WIDTH: f32 = 600.0;
const EXPORT_WINDOW_HEIGHT: f32 = 660.0;
/// Below this the label column and the control would fight for the same pixels.
const EXPORT_WINDOW_MIN_WIDTH: f32 = 520.0;
/// The body scrolls, so the floor only has to keep the titlebar, one section,
/// the status strip and the footer on screen at once.
const EXPORT_WINDOW_MIN_HEIGHT: f32 = 520.0;
/// Horizontal inset shared by the body, the status strip and the footer, so
/// every band in the dialog starts on the same line.
const BODY_PAD_X: f32 = space::LOOSE;
/// Footer band: one primary button plus its breathing room, top and bottom.
const FOOTER_HEIGHT: f32 = size::PROMINENT + 2.0 * space::BASE;
/// Ceiling for a right-aligned readout value before it truncates. A layout
/// constant (a measured column), not spacing.
const READOUT_VALUE_MAX: f32 = 200.0;
/// Ceiling for a form section's width. Maximized, an uncapped form would stretch
/// an 86 px label away from a 1800 px control and stop reading as one row.
const FORM_MAX_WIDTH: f32 = 720.0;
/// Beat fields hold "1234.000" plus a little slack; a full-width numeric input
/// next to a three-character value reads as a mistake.
const BEAT_FIELD_WIDTH: f32 = 104.0;
/// Left rail that gives the status strip a second, non-colour channel.
const STATUS_RAIL_WIDTH: f32 = 2.0;

/// Lifecycle of the export job, surfaced in the window body.
pub enum ExportJobState {
    Editing,
    Running(ExportProgress),
    Complete(Vec<ArrangementExportSummary>),
    Failed(String),
    Cancelled,
}

#[derive(Default)]
struct ExportShared {
    progress: Option<ExportProgress>,
    done: Option<Result<Vec<ArrangementExportSummary>, String>>,
}

struct ExportJob {
    shared: Arc<Mutex<ExportShared>>,
    cancel: ExportCancelToken,
}

/// Detaches the live realtime plugin-bridge sinks for the duration of an export
/// and guarantees they are re-installed when the guard leaves scope — success,
/// error, or worker panic (Drop runs during unwind). Losing the restore would
/// leave every bridged insert silent in realtime playback after the export.
struct BridgeSinkHandoff<'a> {
    engine: Option<&'a DirectAudio::AudioEngine>,
    sinks: &'a PluginBridgeSinkMap,
}

impl<'a> BridgeSinkHandoff<'a> {
    fn detach(
        engine: Option<&'a DirectAudio::AudioEngine>,
        sinks: &'a PluginBridgeSinkMap,
    ) -> Self {
        if let Some(engine) = engine {
            if !sinks.is_empty() {
                for id in sinks.keys() {
                    let _ = engine.set_plugin_bridge_sink(id.clone(), None);
                }
                // Deterministic handoff: wait for the audio callback to ack that
                // the removals were applied before the offline worker starts
                // driving the shared bridge. Ack timeout (no open stream, paused
                // device, stalled callback) falls back to the old fixed grace
                // sleep rather than racing the callback.
                if !engine.wait_for_command_barrier(std::time::Duration::from_millis(500)) {
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
            }
        }
        Self { engine, sinks }
    }
}

impl Drop for BridgeSinkHandoff<'_> {
    fn drop(&mut self) {
        if let Some(engine) = self.engine {
            for (id, sink) in self.sinks {
                let _ = engine.set_plugin_bridge_sink(id.clone(), Some(sink.clone()));
            }
        }
    }
}

/// Which dropdown is currently open (only one at a time). Mode and channel count
/// are exclusive three- and two-way choices, so they are segmented controls
/// rather than dropdowns and do not appear here.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SelectField {
    Format,
    FormatOption,
    FlacCompression,
    Range,
    SampleRate,
    Normalize,
    Tail,
}

/// Which text field currently owns the keyboard. Drives both the IME bridge and
/// the key-priority chain, so "typing" always beats "application command".
#[derive(Clone, Copy, PartialEq, Eq)]
enum ExportField {
    Name,
    RangeStart,
    RangeEnd,
}

pub struct ExportArrangementWindow {
    project_name: String,
    /// Captured at open so `render` never reads a global mid-frame.
    language: String,
    snapshot: EngineProjectSnapshot,
    bridge_sinks: PluginBridgeSinkMap,
    audio_engine: Option<DirectAudio::AudioEngine>,
    defaults: ExportProjectDefaults,
    settings: ExportSettings,
    /// Editable export stem (mixdown file name / stems folder prefix).
    name_input: TextInputState,
    /// Custom-range bounds, in beats. Beats (not bars) because the export
    /// snapshot carries a single global time signature and no signature map, so
    /// bars.beats could not be converted exactly.
    range_start_input: TextInputState,
    range_end_input: TextInputState,
    /// True while a custom-range field holds text that is not a number. The
    /// model keeps its last valid range; Export stays disabled until the draft
    /// parses, so a half-typed value can never silently redefine the range.
    range_draft_invalid: bool,
    /// Cached validation + geometry for the current settings.
    ///
    /// Recomputed on mutation, never during `render`: building it stats the
    /// output folder and walks the snapshot's tempo map and clip list, and
    /// DESIGN.md keeps render functions free of filesystem and scanning work.
    estimate: Result<ExportEstimate, ExportSettingsError>,
    state: ExportJobState,
    open_select: Option<SelectField>,
    job: Option<ExportJob>,
    /// One-shot: the first frame moves keyboard focus into the name field, so
    /// the first Tab has an anchor instead of starting from nowhere.
    focus_primed: bool,
    focus_handle: FocusHandle,
}

impl ExportArrangementWindow {
    pub fn new(
        project_name: String,
        snapshot: EngineProjectSnapshot,
        bridge_sinks: PluginBridgeSinkMap,
        audio_engine: Option<DirectAudio::AudioEngine>,
        defaults: ExportProjectDefaults,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut settings = ExportSettings::default();
        // Default output: <project>.wav in the temp dir as a safe fallback; the
        // opener can override with a project Exports folder.
        let file = ExportSettings::default_file_name(&project_name, settings.format);
        settings.output_path = Some(std::env::temp_dir().join(file));
        let stem = export_stem_from_input(&project_name);
        let mut name_input = TextInputState::new("export-file-name", cx.focus_handle())
            .with_placeholder("Export name");
        name_input.set_value(&stem);

        let mut range_start_input = TextInputState::new("export-range-start", cx.focus_handle())
            .with_placeholder("0")
            .with_ascii_charset("0123456789.");
        let mut range_end_input = TextInputState::new("export-range-end", cx.focus_handle())
            .with_placeholder("0")
            .with_ascii_charset("0123456789.");
        range_start_input.set_value(format_beats(0.0));
        range_end_input.set_value(format_beats(defaults.content_end_beat.max(1.0)));

        let estimate = settings.estimate(&snapshot, &defaults);
        let language = I18n::from_app(cx).locale().code().to_string();

        Self {
            project_name,
            language,
            snapshot,
            bridge_sinks,
            audio_engine,
            defaults,
            settings,
            name_input,
            range_start_input,
            range_end_input,
            range_draft_invalid: false,
            estimate,
            state: ExportJobState::Editing,
            open_select: None,
            job: None,
            focus_primed: false,
            focus_handle: cx.focus_handle(),
        }
    }

    /// Override the default output path (e.g. project Exports folder).
    pub fn set_default_output(&mut self, path: PathBuf) {
        self.settings.output_path = Some(path);
        self.sync_name_from_path();
        self.refresh_estimate();
    }

    fn export_name(&self) -> String {
        export_stem_from_input(&self.name_input.value)
    }

    /// Keep `output_path` in the same folder, with the name field as the stem.
    fn sync_output_from_name(&mut self) {
        let stem = self.export_name();
        let parent = self
            .settings
            .output_path
            .as_ref()
            .and_then(|p| p.parent().map(std::path::Path::to_path_buf))
            .unwrap_or_else(std::env::temp_dir);
        let file = format!("{stem}.{}", self.settings.format.extension());
        self.settings.output_path = Some(parent.join(file));
    }

    fn sync_name_from_path(&mut self) {
        if let Some(stem) = self
            .settings
            .output_path
            .as_ref()
            .and_then(|p| p.file_stem())
            .and_then(|s| s.to_str())
            .filter(|s| !s.trim().is_empty())
        {
            self.name_input.set_value(stem);
        }
    }

    /// Recompute the cached validation + geometry. Call after every mutation of
    /// `settings`; never from `render`.
    fn refresh_estimate(&mut self) {
        self.estimate = self.settings.estimate(&self.snapshot, &self.defaults);
    }

    fn can_export(&self) -> bool {
        self.estimate.is_ok() && !self.range_draft_invalid
    }

    /// Parse both custom-range fields. The model only moves when *both* parse,
    /// so a partially typed pair never resolves to a range nobody asked for.
    fn sync_range_from_inputs(&mut self) {
        if !matches!(self.settings.range, ExportRangeChoice::Custom { .. }) {
            return;
        }
        let start = self.range_start_input.value.trim().parse::<f64>();
        let end = self.range_end_input.value.trim().parse::<f64>();
        match (start, end) {
            (Ok(start), Ok(end))
                if start.is_finite() && end.is_finite() && start >= 0.0 && end >= 0.0 =>
            {
                self.range_draft_invalid = false;
                self.settings.range = ExportRangeChoice::Custom {
                    start_beat: start,
                    end_beat: end,
                };
            }
            _ => self.range_draft_invalid = true,
        }
    }

    fn seed_range_inputs(&mut self, start_beat: f64, end_beat: f64) {
        self.range_start_input.set_value(format_beats(start_beat));
        self.range_end_input.set_value(format_beats(end_beat));
        self.range_draft_invalid = false;
    }

    fn focused_field(&self, window: &Window) -> Option<ExportField> {
        if self.name_input.is_focused(window) {
            Some(ExportField::Name)
        } else if self.range_start_input.is_focused(window) {
            Some(ExportField::RangeStart)
        } else if self.range_end_input.is_focused(window) {
            Some(ExportField::RangeEnd)
        } else {
            None
        }
    }

    fn close(&mut self, window: &mut Window, _cx: &mut Context<Self>) {
        // Closing always cancels a running job so no orphaned worker keeps
        // writing after the window is gone.
        if let Some(job) = &self.job {
            job.cancel.cancel();
        }
        window.remove_window();
    }

    /// Key priority, per DESIGN.md: dialog → text/numeric input → local surface
    /// → application command.
    fn handle_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        let editing = matches!(
            self.state,
            ExportJobState::Editing | ExportJobState::Failed(_)
        );

        // 1. An open dropdown owns Escape, so dismissing a menu never also
        //    dismisses the dialog behind it — including while a field is
        //    focused.
        if key == "escape" && self.open_select.is_some() {
            self.open_select = None;
            cx.notify();
            return;
        }

        // 2. Typing beats every application command.
        let focused = self.focused_field(window);
        if editing {
            if let Some(field) = focused {
                let action = match field {
                    ExportField::Name => self.name_input.handle_key_ime(event, Some(cx)),
                    ExportField::RangeStart => {
                        self.range_start_input.handle_key_ime(event, Some(cx))
                    }
                    ExportField::RangeEnd => self.range_end_input.handle_key_ime(event, Some(cx)),
                };
                match field {
                    ExportField::Name => self.sync_output_from_name(),
                    ExportField::RangeStart | ExportField::RangeEnd => {
                        self.sync_range_from_inputs()
                    }
                }
                self.refresh_estimate();
                match action {
                    // Enter in the name field is the dialog's accept gesture.
                    // In a numeric field it only commits the value.
                    TextInputAction::Submit => {
                        if field == ExportField::Name && self.can_export() {
                            self.start_export(window, cx);
                            return;
                        }
                    }
                    TextInputAction::Cancel => {
                        self.close(window, cx);
                        return;
                    }
                    TextInputAction::Consumed | TextInputAction::Pass => {}
                }
                cx.notify();
                return;
            }
        }

        match key {
            "escape" => {
                if matches!(self.state, ExportJobState::Running(_)) {
                    self.request_cancel(cx);
                } else {
                    self.close(window, cx);
                }
            }
            "enter" | "numpad_enter" if editing && self.can_export() => {
                self.start_export(window, cx);
            }
            _ => {}
        }
    }

    fn request_cancel(&mut self, cx: &mut Context<Self>) {
        if let Some(job) = &self.job {
            job.cancel.cancel();
        }
        cx.notify();
    }

    fn cancel_requested(&self) -> bool {
        self.job
            .as_ref()
            .is_some_and(|job| job.cancel.is_cancelled())
    }

    fn browse_output(&mut self, cx: &mut Context<Self>) {
        #[cfg(feature = "native-dialogs")]
        {
            let entity = cx.entity().clone();
            let format = self.settings.format;
            let mode = self.settings.mode;
            let export_name = self.export_name();
            let start = self
                .settings
                .output_path
                .clone()
                .and_then(|p| p.parent().map(|d| d.to_path_buf()))
                .unwrap_or_else(std::env::temp_dir);
            let file = format!("{export_name}.{}", format.extension());
            cx.spawn(async move |_this, cx| {
                let dialog = rfd::AsyncFileDialog::new()
                    .set_title(if mode == ExportMode::Mixdown {
                        "Export Mixdown"
                    } else {
                        "Choose Export Folder"
                    })
                    .set_directory(&start);
                let result = if mode == ExportMode::Mixdown {
                    dialog
                        .set_file_name(&file)
                        .add_filter(format.as_str().to_uppercase(), &[format.extension()])
                        .save_file()
                        .await
                } else {
                    dialog.pick_folder().await
                };
                if let Some(handle) = result {
                    let path = if mode == ExportMode::Mixdown {
                        handle.path().to_path_buf()
                    } else {
                        handle
                            .path()
                            .join(format!("{export_name}.{}", format.extension()))
                    };
                    let _ = entity.update(cx, |this, cx| {
                        this.settings.output_path = Some(path);
                        this.sync_name_from_path();
                        this.refresh_estimate();
                        cx.notify();
                    });
                }
            })
            .detach();
        }

        #[cfg(not(feature = "native-dialogs"))]
        {
            self.state =
                ExportJobState::Failed("Native file dialogs are unavailable in this build.".into());
            cx.notify();
        }
    }

    // ── Export job ───────────────────────────────────────────────────────────

    /// Resolve the request and either start the job or ask before replacing
    /// existing files. The exporter overwrites its destination on success, so
    /// this is a destructive action and DESIGN.md requires an explicit Cancel.
    fn start_export(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_select = None;
        self.sync_output_from_name();
        self.refresh_estimate();
        let job = self.resolve_job(cx);
        let Some((request, batch_targets)) = job else {
            return;
        };

        let existing = existing_destinations(&request, &batch_targets);
        if existing.is_empty() {
            self.spawn_export(request, batch_targets, cx);
            return;
        }

        let i18n = I18n::new(&self.language);
        let entity = cx.entity().clone();
        let detail = existing
            .iter()
            .take(4)
            .map(|path| file_label(path))
            .collect::<Vec<_>>()
            .join("\n");
        let detail = if existing.len() > 4 {
            format!(
                "{detail}\n{}",
                tr_vars_or(
                    i18n,
                    "export.overwrite.more",
                    "…and { $count } more.",
                    &[("count", (existing.len() - 4).to_string())],
                )
            )
        } else {
            detail
        };
        let options = MessageBoxOptions::new(tr_vars_or(
            i18n,
            "export.overwrite.message",
            "Replace { $count } existing file(s)?",
            &[("count", existing.len().to_string())],
        ))
        .title(i18n.tr_or("export.overwrite.title", "Replace Existing Files"))
        .detail(detail)
        .kind(MessageBoxKind::Warning)
        .buttons([
            i18n.tr_or("export.action.cancel", "Cancel"),
            i18n.tr_or("export.overwrite.replace", "Replace"),
        ])
        .default_id(0)
        .cancel_id(0);

        let on_response: MessageBoxResponseCb =
            Arc::new(move |result: MessageBoxResult, _w, cx| {
                if result.response != 1 {
                    return;
                }
                let _ = entity.update(cx, |this, cx| {
                    let job = this.resolve_job(cx);
                    if let Some((request, targets)) = job {
                        this.spawn_export(request, targets, cx);
                    }
                });
            });
        let _ = open_message_box_window(Some(window.bounds()), options, on_response, cx);
    }

    /// Build the engine request + batch targets, publishing a failure state
    /// instead of returning it. `None` means "do not start".
    fn resolve_job(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<(ArrangementExportRequest, Vec<TrackExportTarget>)> {
        let request = match self.settings.to_request(&self.snapshot, &self.defaults) {
            Ok(request) => request,
            Err(err) => {
                self.state = ExportJobState::Failed(localized_error(&self.language, &err));
                cx.notify();
                return None;
            }
        };
        let batch_targets = build_batch_targets(
            &request,
            self.settings.mode,
            &self.defaults,
            &self.export_name(),
        );
        Some((request, batch_targets))
    }

    fn spawn_export(
        &mut self,
        request: ArrangementExportRequest,
        batch_targets: Vec<TrackExportTarget>,
        cx: &mut Context<Self>,
    ) {
        let shared = Arc::new(Mutex::new(ExportShared::default()));
        let cancel = ExportCancelToken::new();
        self.job = Some(ExportJob {
            shared: shared.clone(),
            cancel: cancel.clone(),
        });
        // Seed the denominator with content + tail, which is what every later
        // worker report uses. Seeding with content alone made "N of M" change
        // its M mid-run.
        self.state = ExportJobState::Running(ExportProgress::stage_only(
            ExportStage::Preparing,
            request
                .render
                .content_frames()
                .saturating_add(request.render.max_tail_frames()),
        ));

        // Worker thread: plain data only, never touches GPUI.
        let worker_shared = shared.clone();
        let worker_cancel = cancel.clone();
        let snapshot = self.snapshot.clone();
        let bridge_sinks = self.bridge_sinks.clone();
        let audio_engine = self.audio_engine.clone();
        std::thread::Builder::new()
            .name("fb-arrangement-export".to_string())
            .spawn(move || {
                let progress_shared = worker_shared.clone();
                let mut on_progress = move |progress| {
                    if let Ok(mut guard) = progress_shared.lock() {
                        guard.progress = Some(progress);
                    }
                };
                // Scope the sink handoff so the live sinks are re-installed
                // (guard Drop) before the terminal result is published below —
                // and on any panic path, since Drop runs during unwind.
                let result = {
                    let _bridge_handoff =
                        BridgeSinkHandoff::detach(audio_engine.as_ref(), &bridge_sinks);
                    if batch_targets.is_empty() {
                        export_arrangement_with_bridges(
                            &snapshot,
                            &request,
                            &worker_cancel,
                            Some(&bridge_sinks),
                            &mut on_progress,
                        )
                        .map(|summary| vec![summary])
                    } else {
                        export_tracks_single_pass_with_bridges(
                            &snapshot,
                            &batch_targets,
                            &worker_cancel,
                            Some(&bridge_sinks),
                            &mut on_progress,
                        )
                    }
                    .map_err(|error| error.to_string())
                };
                if let Ok(mut guard) = worker_shared.lock() {
                    guard.done = Some(result);
                }
            })
            .ok();

        // Poll loop: copy shared progress into UI state until terminal.
        cx.spawn(async move |this, cx| {
            let executor = cx.background_executor().clone();
            loop {
                if crate::shutdown::ShutdownState::global().is_shutting_down() {
                    break;
                }
                executor.timer(std::time::Duration::from_millis(50)).await;
                let keep_going = this
                    .update(cx, |this, cx| this.poll_job(cx))
                    .unwrap_or(false);
                if !keep_going {
                    break;
                }
            }
        })
        .detach();

        cx.notify();
    }

    /// Apply the latest shared progress/result. Returns `false` once terminal.
    fn poll_job(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(job) = &self.job else {
            return false;
        };
        let (progress, done) = {
            let Ok(mut guard) = job.shared.lock() else {
                return true;
            };
            (guard.progress.take(), guard.done.take())
        };
        if let Some(done) = done {
            self.state = match done {
                Ok(summary) => ExportJobState::Complete(summary),
                Err(message) if job.cancel.is_cancelled() && message.contains("cancel") => {
                    ExportJobState::Cancelled
                }
                Err(message) => ExportJobState::Failed(message),
            };
            self.job = None;
            cx.notify();
            return false;
        }
        if let Some(progress) = progress {
            self.state = ExportJobState::Running(progress);
            cx.notify();
        }
        true
    }
}

// ── Rendering ────────────────────────────────────────────────────────────────

impl Render for ExportArrangementWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // One-shot focus anchor. Without it the first Tab has nothing to move
        // from, so no focus ring is ever visible.
        if !self.focus_primed {
            self.focus_primed = true;
            self.name_input.focus_handle.focus(window, cx);
        }

        let i18n = I18n::new(&self.language);
        let target = cx.entity().clone();
        let dismiss_backdrop = self.open_select.is_some().then(|| {
            let target = target.clone();
            select_dismiss_backdrop(Arc::new(move |_, _window, cx| {
                let _ = target.update(cx, |this, cx| {
                    if this.open_select.take().is_some() {
                        cx.notify();
                    }
                });
            }))
        });

        let (body, footer) = match &self.state {
            ExportJobState::Editing | ExportJobState::Failed(_) => (
                self.render_editing(window, i18n, &target),
                self.footer_editing(i18n, &target),
            ),
            ExportJobState::Running(progress) => (
                self.render_progress(progress, i18n),
                self.footer_running(i18n, &target),
            ),
            ExportJobState::Complete(summaries) => (
                self.render_complete(summaries, i18n),
                self.footer_complete(summaries, i18n, &target),
            ),
            ExportJobState::Cancelled => (
                self.render_terminal_message(
                    i18n.tr_or("export.state.cancelled", "Export cancelled."),
                    i18n.tr_or(
                        "export.state.cancelled-hint",
                        "No file was written. Partial output was discarded.",
                    ),
                ),
                self.footer_terminal(i18n, &target),
            ),
        };

        div()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .font(theme::ui_font())
            .bg(Colors::surface_base())
            // The clip owner: this is what makes the dialog radius actually cut
            // the square-cornered titlebar child.
            .overflow_hidden()
            .rounded(px(radius::DIALOG))
            .border(px(1.0))
            .border_color(Colors::border_normal())
            .shadow(elevation::shadow(elevation::OVERLAY))
            .capture_key_down({
                let target = target.clone();
                move |event, window, cx| {
                    let _ = target.update(cx, |this, cx| this.handle_key(event, window, cx));
                }
            })
            .child(div().w(px(0.0)).h(px(0.0)).track_focus(&self.focus_handle))
            .child(external_window_titlebar_with_icon(
                Some(assets::ICON_SHARE_PATH),
                i18n.tr_or("export.title", "Export Arrangement"),
                "export-window-close",
                {
                    let target = target.clone();
                    move |window, cx| {
                        let _ = target.update(cx, |this, cx| this.close(window, cx));
                    }
                },
            ))
            .child(body)
            .children(self.status_strip(i18n))
            .child(footer)
            .children(dismiss_backdrop)
    }
}

impl ExportArrangementWindow {
    // ── Editing body ─────────────────────────────────────────────────────────

    fn render_editing(
        &self,
        window: &Window,
        i18n: I18n,
        target: &Entity<Self>,
    ) -> gpui::AnyElement {
        body_scroll()
            .child(self.section_destination(window, i18n, target))
            .child(self.section_source(i18n, target))
            .child(self.section_range(window, i18n, target))
            .child(self.section_format(i18n, target))
            .child(self.section_summary(i18n))
            .into_any_element()
    }

    /// Where the files land: the stem the user types, the folder, and a literal
    /// statement of what will be written there.
    fn section_destination(
        &self,
        window: &Window,
        i18n: I18n,
        target: &Entity<Self>,
    ) -> impl IntoElement {
        let batch = self.settings.mode != ExportMode::Mixdown;
        let focused = self.name_input.is_focused(window);
        let name_field = text_field_with_callbacks_and_ime(
            &self.name_input,
            focused,
            bind_mouse_selection(target.clone(), |this| &mut this.name_input),
            target.clone(),
        );

        let folder = self
            .settings
            .normalized_output_path()
            .and_then(|path| path.parent().map(|dir| dir.display().to_string()))
            .unwrap_or_else(|| i18n.tr_or("export.readout.no-folder", "No folder selected"));

        let browse = fb_button(
            "export-browse",
            i18n.tr_or("export.action.browse", "Browse…"),
            FbButtonKind::Default,
            true,
            {
                let target = target.clone();
                move |_, _window, cx| {
                    let _ = target.update(cx, |this, cx| this.browse_output(cx));
                }
            },
        );

        form_section(
            i18n.tr_or("export.section.destination", "Destination"),
            vec![
                form_row(
                    if batch {
                        i18n.tr_or("export.field.base-name", "Base name")
                    } else {
                        i18n.tr_or("export.field.name", "File name")
                    },
                    name_field,
                ),
                form_row(
                    i18n.tr_or("export.field.folder", "Folder"),
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(space::SNUG))
                        .child(readout_surface(folder))
                        .child(browse),
                ),
                readout_row(
                    i18n.tr_or("export.readout.writes-to", "Writes"),
                    self.destination_summary(i18n),
                ),
            ],
        )
    }

    /// What gets rendered: the master bus, or one file per channel.
    fn section_source(&self, i18n: I18n, target: &Entity<Self>) -> impl IntoElement {
        let mode = self.settings.mode;
        let modes = [
            (
                ExportMode::Mixdown,
                "export-mode-mixdown",
                i18n.tr_or("export.mode.mixdown", "Mixdown"),
                FbSegment::First,
            ),
            (
                ExportMode::Stems,
                "export-mode-stems",
                i18n.tr_or("export.mode.stems", "All channels"),
                FbSegment::Middle,
            ),
            (
                ExportMode::Multitrack,
                "export-mode-multitrack",
                i18n.tr_or("export.mode.multitrack", "Source tracks"),
                FbSegment::Last,
            ),
        ];
        let mut track = fb_segmented_track().w_full();
        for (value, id, label, position) in modes {
            let target = target.clone();
            track = track.child(fb_segment(
                id,
                label,
                mode == value,
                position,
                move |_, _window, cx| {
                    let _ = target.update(cx, |this, cx| {
                        this.set_mode(value);
                        cx.notify();
                    });
                },
            ));
        }

        let mut rows = vec![form_row(i18n.tr_or("export.field.mode", "Export"), track)];
        if mode != ExportMode::Mixdown {
            rows.push(readout_row(
                i18n.tr_or("export.readout.included", "Channels included"),
                tr_vars_or(
                    i18n,
                    "export.readout.n-of-m",
                    "{ $selected } of { $total }",
                    &[
                        (
                            "selected",
                            self.settings.batch_target_count(&self.defaults).to_string(),
                        ),
                        ("total", self.defaults.track_targets.len().to_string()),
                    ],
                ),
            ));
            // The engine rejects normalization for a batch export outright, so
            // say so rather than leaving the disabled dropdown unexplained.
            rows.push(note_row(i18n.tr_or(
                "export.note.batch-normalize",
                "Stem and multitrack exports are written without normalization.",
            )));
        }
        form_section(i18n.tr_or("export.section.source", "Source"), rows)
    }

    /// Which span of the arrangement, and what happens after it ends.
    fn section_range(
        &self,
        window: &Window,
        i18n: I18n,
        target: &Entity<Self>,
    ) -> impl IntoElement {
        let selected = match self.settings.range {
            ExportRangeChoice::EntireArrangement => "entire",
            ExportRangeChoice::TimeSelection { .. } => "selection",
            ExportRangeChoice::LoopRange { .. } => "loop",
            ExportRangeChoice::Custom { .. } => "custom",
        };

        let mut options = vec![SelectOption::new(
            "entire",
            i18n.tr_or("export.range.entire", "Entire arrangement"),
        )];
        // The opener does not carry a time selection today, so the option is
        // simply absent rather than permanently greyed out: a control that can
        // never become available is not a control.
        if self.defaults.time_selection.is_some() {
            options.push(SelectOption::new(
                "selection",
                i18n.tr_or("export.range.selection", "Time selection"),
            ));
        }
        options.push(match self.defaults.loop_range {
            Some((start, end)) => {
                SelectOption::new("loop", i18n.tr_or("export.range.loop", "Loop range"))
                    .description(tr_vars_or(
                        i18n,
                        "export.range.loop-span",
                        "beats { $start } – { $end }",
                        &[("start", format_beats(start)), ("end", format_beats(end))],
                    ))
            }
            None => SelectOption::new(
                "loop",
                i18n.tr_or("export.range.loop-unset", "Loop range (none set)"),
            )
            .disabled(true),
        });
        options.push(SelectOption::new(
            "custom",
            i18n.tr_or("export.range.custom", "Custom range"),
        ));

        let mut rows = vec![form_row(
            i18n.tr_or("export.field.range", "Range"),
            self.dropdown(
                SelectField::Range,
                "export-range",
                selected.to_string(),
                options,
                target,
            ),
        )];

        if matches!(self.settings.range, ExportRangeChoice::Custom { .. }) {
            rows.push(form_row(
                i18n.tr_or("export.field.start-beat", "Start (beats)"),
                beat_field(&self.range_start_input, window, target, |this| {
                    &mut this.range_start_input
                }),
            ));
            rows.push(form_row(
                i18n.tr_or("export.field.end-beat", "End (beats)"),
                beat_field(&self.range_end_input, window, target, |this| {
                    &mut this.range_end_input
                }),
            ));
        }

        // The derived seconds readout: beats are the input, but the file is
        // measured in time, and the tempo map is what connects them.
        rows.push(readout_row(
            i18n.tr_or("export.readout.span", "Span"),
            match &self.estimate {
                Ok(estimate) => format!(
                    "{} → {}",
                    format_duration(estimate.start_seconds),
                    format_duration(estimate.end_seconds)
                ),
                Err(_) => "—".to_string(),
            },
        ));

        let tail_selected = match self.settings.tail {
            ExportTailChoice::None => "none",
            ExportTailChoice::FixedSeconds(_) => "fixed",
            ExportTailChoice::UntilSilence { .. } => "silence",
        };
        let tail_options = vec![
            SelectOption::new("none", i18n.tr_or("export.tail.none", "None")),
            SelectOption::new(
                "fixed",
                tr_vars_or(
                    i18n,
                    "export.tail.fixed",
                    "Fixed { $seconds } s",
                    &[("seconds", format!("{TAIL_FIXED_SECONDS:.1}"))],
                ),
            ),
            SelectOption::new(
                "silence",
                tr_vars_or(
                    i18n,
                    "export.tail.silence",
                    "Until silence (max { $max } s)",
                    &[("max", format!("{TAIL_SILENCE_MAX_SECONDS:.1}"))],
                ),
            )
            .description(tr_vars_or(
                i18n,
                "export.tail.silence-detail",
                "Stops once the block peak falls below { $db } dBFS.",
                &[("db", format!("{TAIL_SILENCE_THRESHOLD_DB:.0}"))],
            )),
        ];
        rows.push(form_row(
            i18n.tr_or("export.field.tail", "Tail"),
            self.dropdown(
                SelectField::Tail,
                "export-tail",
                tail_selected.to_string(),
                tail_options,
                target,
            ),
        ));

        form_section(i18n.tr_or("export.section.range", "Range"), rows)
    }

    /// Container, resolution and gain staging — everything the encoder reads.
    fn section_format(&self, i18n: I18n, target: &Entity<Self>) -> impl IntoElement {
        let mut rows = Vec::new();

        let format_options = vec![
            SelectOption::new("wav", "WAV"),
            SelectOption::new("flac", "FLAC"),
            {
                let option = SelectOption::new("mp3", "MP3");
                if self.defaults.mp3_available {
                    option
                } else {
                    option.disabled(true).description(i18n.tr_or(
                        "export.hint.mp3-unavailable",
                        "Not compiled into this build.",
                    ))
                }
            },
        ];
        rows.push(form_row(
            i18n.tr_or("export.field.format", "Format"),
            self.dropdown(
                SelectField::Format,
                "export-format",
                self.settings.format.as_str().to_string(),
                format_options,
                target,
            ),
        ));

        // Rauf is never selectable, so it contributes no row rather than an
        // empty dropdown.
        if let Some((label, selected, options)) = self.format_option_field(i18n) {
            rows.push(form_row(
                label,
                self.dropdown(
                    SelectField::FormatOption,
                    "export-format-option",
                    selected,
                    options,
                    target,
                ),
            ));
        }

        if self.settings.format == AudioFileFormat::Flac {
            let level = self.settings.flac_compression_level.unwrap_or(5);
            let options = (FLAC_COMPRESSION_RANGE.0..=FLAC_COMPRESSION_RANGE.1)
                .map(|value| {
                    let label = match value {
                        v if v == FLAC_COMPRESSION_RANGE.0 => tr_vars_or(
                            i18n,
                            "export.flac.fastest",
                            "{ $level } — fastest",
                            &[("level", value.to_string())],
                        ),
                        v if v == FLAC_COMPRESSION_RANGE.1 => tr_vars_or(
                            i18n,
                            "export.flac.smallest",
                            "{ $level } — smallest",
                            &[("level", value.to_string())],
                        ),
                        _ => value.to_string(),
                    };
                    SelectOption::new(value.to_string(), label)
                })
                .collect::<Vec<_>>();
            rows.push(form_row(
                i18n.tr_or("export.field.flac-compression", "Compression"),
                self.dropdown(
                    SelectField::FlacCompression,
                    "export-flac-compression",
                    level.to_string(),
                    options,
                    target,
                ),
            ));
        }

        let rate_selected = match self.settings.sample_rate {
            ExportSampleRateChoice::Project => "project",
            ExportSampleRateChoice::Hz44100 => "44100",
            ExportSampleRateChoice::Hz48000 => "48000",
            ExportSampleRateChoice::Hz88200 => "88200",
            ExportSampleRateChoice::Hz96000 => "96000",
        };
        let rate_options = vec![
            SelectOption::new(
                "project",
                tr_vars_or(
                    i18n,
                    "export.rate.project",
                    "Project ({ $hz } Hz)",
                    &[("hz", self.defaults.project_sample_rate.to_string())],
                ),
            ),
            SelectOption::new("44100", "44100 Hz"),
            SelectOption::new("48000", "48000 Hz"),
            SelectOption::new("88200", "88200 Hz"),
            SelectOption::new("96000", "96000 Hz"),
        ];
        rows.push(form_row(
            i18n.tr_or("export.field.sample-rate", "Sample rate"),
            self.dropdown(
                SelectField::SampleRate,
                "export-rate",
                rate_selected.to_string(),
                rate_options,
                target,
            ),
        ));

        let channels = self.settings.channels;
        let mut channel_track = fb_segmented_track().w_full();
        for (value, id, label, position) in [
            (
                ExportChannelMode::Stereo,
                "export-channels-stereo",
                i18n.tr_or("export.channels.stereo", "Stereo"),
                FbSegment::First,
            ),
            (
                ExportChannelMode::Mono,
                "export-channels-mono",
                i18n.tr_or("export.channels.mono", "Mono"),
                FbSegment::Last,
            ),
        ] {
            let target = target.clone();
            channel_track = channel_track.child(fb_segment(
                id,
                label,
                channels == value,
                position,
                move |_, _window, cx| {
                    let _ = target.update(cx, |this, cx| {
                        this.settings.channels = value;
                        this.refresh_estimate();
                        cx.notify();
                    });
                },
            ));
        }
        rows.push(form_row(
            i18n.tr_or("export.field.channels", "Channels"),
            channel_track,
        ));

        // Normalization is a real two-pass gain stage, and the engine rejects it
        // for batch exports — so the option carries the reason, not just a grey.
        let mixdown_only = self.settings.mode != ExportMode::Mixdown;
        let normalize_selected = match self.settings.normalize {
            ExportNormalizeChoice::Off => "off".to_string(),
            ExportNormalizeChoice::PeakDb(db) => peak_option_id(db),
        };
        let mut normalize_options = vec![SelectOption::new(
            "off",
            i18n.tr_or("export.normalize.off", "Off"),
        )];
        for db in PEAK_TARGETS_DB {
            let option = SelectOption::new(
                peak_option_id(db),
                tr_vars_or(
                    i18n,
                    "export.normalize.peak",
                    "Peak { $db } dBFS",
                    &[("db", format_db(db))],
                ),
            );
            normalize_options.push(if mixdown_only {
                option.disabled(true).description(i18n.tr_or(
                    "export.hint.normalize-mixdown-only",
                    "Available for mixdown only.",
                ))
            } else {
                option
            });
        }
        rows.push(form_row(
            i18n.tr_or("export.field.normalize", "Normalize"),
            self.dropdown(
                SelectField::Normalize,
                "export-normalize",
                normalize_selected,
                normalize_options,
                target,
            ),
        ));

        form_section(i18n.tr_or("export.section.format", "Audio format"), rows)
    }

    /// The bit-depth / bitrate row, which changes identity with the container.
    fn format_option_field(&self, i18n: I18n) -> Option<(String, String, Vec<SelectOption>)> {
        match self.settings.format {
            AudioFileFormat::Wav => Some((
                i18n.tr_or("export.field.bit-depth", "Bit depth"),
                match self.settings.wav_sample_format {
                    sphere_encoder::AudioSampleFormat::F32 => "f32",
                    sphere_encoder::AudioSampleFormat::I24 => "i24",
                    _ => "i16",
                }
                .to_string(),
                vec![
                    SelectOption::new("f32", i18n.tr_or("export.depth.f32", "Float 32")),
                    SelectOption::new("i24", i18n.tr_or("export.depth.i24", "PCM 24")),
                    SelectOption::new("i16", i18n.tr_or("export.depth.i16", "PCM 16")),
                ],
            )),
            AudioFileFormat::Flac => Some((
                i18n.tr_or("export.field.bit-depth", "Bit depth"),
                self.settings.flac_bit_depth.to_string(),
                vec![
                    SelectOption::new("16", "16-bit"),
                    SelectOption::new("24", "24-bit"),
                ],
            )),
            AudioFileFormat::Mp3 => Some((
                i18n.tr_or("export.field.bitrate", "Bitrate"),
                self.settings.mp3_bitrate_kbps.to_string(),
                vec![
                    SelectOption::new("128", "128 kbps"),
                    SelectOption::new("192", "192 kbps"),
                    SelectOption::new("256", "256 kbps"),
                    SelectOption::new("320", "320 kbps"),
                ],
            )),
            AudioFileFormat::Rauf => None,
        }
    }

    /// What the export will actually produce, read out of the engine request.
    fn section_summary(&self, i18n: I18n) -> impl IntoElement {
        let rows = match &self.estimate {
            Ok(estimate) => {
                let mut rows = vec![
                    readout_row(
                        i18n.tr_or("export.readout.duration", "Duration"),
                        format_duration(estimate.content_seconds),
                    ),
                    readout_row(
                        i18n.tr_or("export.readout.tail", "Tail"),
                        self.tail_summary(i18n, estimate),
                    ),
                    readout_row(
                        i18n.tr_or("export.readout.output", "Output"),
                        output_spec(estimate, i18n),
                    ),
                    readout_row(
                        i18n.tr_or("export.readout.frames", "Frames"),
                        grouped(estimate.content_frames),
                    ),
                    readout_row(
                        i18n.tr_or("export.readout.files", "Files"),
                        estimate.file_count.to_string(),
                    ),
                ];
                if let Some(bytes) = estimate.uncompressed_bytes {
                    rows.push(readout_row(
                        i18n.tr_or("export.readout.size", "File size"),
                        format!("≈ {}", format_bytes(bytes)),
                    ));
                }
                rows
            }
            // Never a fabricated number: when the request cannot be built there
            // is nothing truthful to show.
            Err(err) => vec![note_row(localized_error(&self.language, err))],
        };
        form_section(i18n.tr_or("export.section.summary", "Summary"), rows)
    }

    fn tail_summary(&self, i18n: I18n, estimate: &ExportEstimate) -> String {
        match self.settings.tail {
            ExportTailChoice::None => i18n.tr_or("export.tail.none", "None"),
            ExportTailChoice::FixedSeconds(seconds) => format!("{seconds:.1} s"),
            // `max_tail_frames` reports the cap; the renderer stops early once
            // the block peak drops, so this is a ceiling, not a duration.
            ExportTailChoice::UntilSilence { .. } => tr_vars_or(
                i18n,
                "export.readout.tail-up-to",
                "up to { $seconds } s",
                &[(
                    "seconds",
                    format!(
                        "{:.1}",
                        estimate.max_tail_frames as f64 / estimate.sample_rate.max(1) as f64
                    ),
                )],
            ),
        }
    }

    /// A literal statement of what lands on disk, in the user's own naming.
    fn destination_summary(&self, i18n: I18n) -> String {
        let stem = self.export_name();
        if self.settings.mode == ExportMode::Mixdown {
            return format!("{stem}.{}", self.settings.format.extension());
        }
        let folder = format!(
            "{} {}",
            sanitize_file_stem(&stem),
            batch_folder_suffix(self.settings.mode)
        );
        let count = self.settings.batch_target_count(&self.defaults);
        tr_vars_or(
            i18n,
            "export.readout.batch-writes",
            "{ $folder }/ · { $count } file(s)",
            &[("folder", folder), ("count", count.to_string())],
        )
    }

    // ── Job bodies ───────────────────────────────────────────────────────────

    fn render_progress(&self, progress: &ExportProgress, i18n: I18n) -> gpui::AnyElement {
        // Only the render/encode pass reports frames. The peak-analysis pass
        // hands the exporter a no-op progress callback, so a determinate bar
        // there would sit frozen at 0% for half the export; preparing and
        // finalizing report nothing at all.
        let determinate = matches!(
            progress.stage,
            ExportStage::Encoding | ExportStage::Rendering
        );
        let value = if determinate {
            ProgressBarValue::value(progress.percent / 100.0)
        } else {
            ProgressBarValue::Indeterminate
        };
        let path = self
            .settings
            .normalized_output_path()
            .map(|path| path.display().to_string())
            .unwrap_or_default();

        let mut rows = vec![
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(px(space::BASE))
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(px(typography::UI_MD))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(Colors::text_primary())
                        .child(i18n.tr_or(stage_key(progress.stage), progress.stage.as_str())),
                )
                .children(determinate.then(|| {
                    div()
                        .flex_shrink_0()
                        .font_features(tabular_figures())
                        .text_size(px(typography::UI_SM))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(Colors::text_primary())
                        .child(format!("{:.0}%", progress.percent))
                }))
                .into_any_element(),
            div().child(progress_bar(value)).into_any_element(),
        ];
        if determinate {
            rows.push(readout_row(
                i18n.tr_or("export.readout.frames", "Frames"),
                format!(
                    "{} / {}",
                    grouped(progress.rendered_frames),
                    grouped(progress.total_frames)
                ),
            ));
        }
        rows.push(
            path_row(i18n.tr_or("export.readout.writing", "Writing"), path).into_any_element(),
        );

        body_scroll()
            .child(form_section(
                i18n.tr_or("export.section.progress", "Exporting"),
                rows,
            ))
            .into_any_element()
    }

    fn render_complete(
        &self,
        summaries: &[ArrangementExportSummary],
        i18n: I18n,
    ) -> gpui::AnyElement {
        let Some(first) = summaries.first() else {
            return self.render_terminal_message(
                i18n.tr_or(
                    "export.state.empty",
                    "Export finished with no output files.",
                ),
                i18n.tr_or(
                    "export.state.empty-hint",
                    "Check the source selection and try again.",
                ),
            );
        };

        let total_seconds: f64 = summaries.iter().map(|s| s.duration_seconds).sum();
        let peak = summaries
            .iter()
            .filter_map(|s| s.peak_db)
            .fold(f32::NEG_INFINITY, f32::max);

        let mut summary_rows = vec![
            readout_row(
                i18n.tr_or("export.readout.files", "Files"),
                summaries.len().to_string(),
            ),
            readout_row(
                i18n.tr_or("export.readout.output", "Output"),
                format!(
                    "{} Hz · {}",
                    first.sample_rate,
                    channel_label(first.channels, i18n)
                ),
            ),
            readout_row(
                i18n.tr_or("export.readout.total-duration", "Total duration"),
                format_duration(total_seconds),
            ),
        ];
        if peak.is_finite() {
            summary_rows.push(readout_row(
                i18n.tr_or("export.readout.peak", "Peak"),
                format!("{peak:.1} dBFS"),
            ));
        }

        let file_rows: Vec<gpui::AnyElement> = summaries
            .iter()
            .map(|summary| {
                file_row(
                    file_label(&summary.output_path),
                    format_duration(summary.duration_seconds),
                )
                .into_any_element()
            })
            .collect();

        body_scroll()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_shrink_0()
                    .items_center()
                    .gap(px(space::SNUG))
                    .child(
                        div()
                            .text_size(px(typography::UI_MD))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(Colors::status_success())
                            .child(i18n.tr_or("export.state.complete", "Export complete")),
                    ),
            )
            .child(form_section(
                i18n.tr_or("export.section.summary", "Summary"),
                summary_rows,
            ))
            .child(form_section(
                i18n.tr_or("export.section.files", "Files written"),
                file_rows,
            ))
            .into_any_element()
    }

    fn render_terminal_message(&self, message: String, hint: String) -> gpui::AnyElement {
        body_scroll()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_shrink_0()
                    .max_w(px(FORM_MAX_WIDTH))
                    .gap(px(space::SNUG))
                    .child(
                        div()
                            .text_size(px(typography::UI_MD))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(Colors::text_primary())
                            .child(message),
                    )
                    .child(
                        div()
                            .text_size(px(typography::UI_XS))
                            .text_color(Colors::text_muted())
                            .child(hint),
                    ),
            )
            .into_any_element()
    }

    // ── Status strip and footers ─────────────────────────────────────────────

    /// Pinned band between the scrolling form and the action footer.
    ///
    /// It is outside the scroller on purpose: an error the user has scrolled
    /// past is an error they cannot act on. It wraps rather than truncates,
    /// because `OutputDirMissing` carries the path that has to be fixed.
    fn status_strip(&self, i18n: I18n) -> Option<gpui::AnyElement> {
        let (message, tone) = match &self.state {
            ExportJobState::Failed(message) => (
                tr_vars_or(
                    i18n,
                    "export.status.failed",
                    "Export failed — { $reason }",
                    &[("reason", message.clone())],
                ),
                Colors::status_error(),
            ),
            ExportJobState::Editing => {
                if self.range_draft_invalid {
                    (
                        i18n.tr_or(
                            "export.status.range-draft",
                            "Cannot export — the custom range needs two numbers in beats.",
                        ),
                        Colors::status_warning(),
                    )
                } else {
                    match &self.estimate {
                        Ok(_) => return None,
                        Err(err) => (
                            tr_vars_or(
                                i18n,
                                "export.status.invalid",
                                "Cannot export — { $reason }",
                                &[("reason", localized_error(&self.language, err))],
                            ),
                            Colors::status_warning(),
                        ),
                    }
                }
            }
            _ => return None,
        };

        Some(
            div()
                .flex_shrink_0()
                .flex()
                .flex_row()
                .items_start()
                .gap(px(space::BASE))
                .px(px(BODY_PAD_X))
                .py(px(space::BASE))
                .border_t(px(1.0))
                .border_color(Colors::border_subtle())
                .bg(Colors::composite(
                    Colors::surface_base(),
                    Colors::with_alpha(tone, 0.10),
                ))
                // Second channel: the state is carried by a leading rail as well
                // as by colour, so it survives a colour-blind read.
                .child(
                    div()
                        .flex_shrink_0()
                        .w(px(STATUS_RAIL_WIDTH))
                        .h(px(size::MICRO))
                        .bg(tone),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(typography::UI_XS))
                        .text_color(Colors::text_secondary())
                        .child(message),
                )
                .into_any_element(),
        )
    }

    fn footer_editing(&self, i18n: I18n, target: &Entity<Self>) -> gpui::AnyElement {
        let can_export = self.can_export();
        footer_band()
            .child(fb_button(
                "export-cancel",
                i18n.tr_or("export.action.cancel", "Cancel"),
                FbButtonKind::Default,
                true,
                {
                    let target = target.clone();
                    move |_, window, cx| {
                        let _ = target.update(cx, |this, cx| this.close(window, cx));
                    }
                },
            ))
            .child(fb_button(
                "export-start",
                i18n.tr_or("export.action.export", "Export"),
                FbButtonKind::Primary,
                can_export,
                {
                    let target = target.clone();
                    move |_, window, cx| {
                        let _ = target.update(cx, |this, cx| this.start_export(window, cx));
                    }
                },
            ))
            .into_any_element()
    }

    fn footer_running(&self, i18n: I18n, target: &Entity<Self>) -> gpui::AnyElement {
        // Cancel is real — the token is polled by the render loop — but it is
        // one-shot, so a second press must not look like a dead button.
        let cancelling = self.cancel_requested();
        footer_band()
            .child(fb_button(
                "export-cancel-run",
                if cancelling {
                    i18n.tr_or("export.action.cancelling", "Cancelling…")
                } else {
                    i18n.tr_or("export.action.cancel", "Cancel")
                },
                FbButtonKind::Default,
                !cancelling,
                {
                    let target = target.clone();
                    move |_, _window, cx| {
                        let _ = target.update(cx, |this, cx| this.request_cancel(cx));
                    }
                },
            ))
            .into_any_element()
    }

    fn footer_complete(
        &self,
        summaries: &[ArrangementExportSummary],
        i18n: I18n,
        target: &Entity<Self>,
    ) -> gpui::AnyElement {
        let folder = summaries.first().and_then(|summary| {
            summary
                .output_path
                .parent()
                .map(std::path::Path::to_path_buf)
        });
        footer_band()
            .children(folder.map(|folder| {
                fb_button(
                    "export-reveal",
                    i18n.tr_or("export.action.open-folder", "Open Folder"),
                    FbButtonKind::Default,
                    true,
                    move |_, _window, _cx| {
                        let _ = open_in_file_manager(&folder);
                    },
                )
            }))
            .child(fb_button(
                "export-close",
                i18n.tr_or("export.action.close", "Close"),
                FbButtonKind::Primary,
                true,
                {
                    let target = target.clone();
                    move |_, window, cx| {
                        let _ = target.update(cx, |this, cx| this.close(window, cx));
                    }
                },
            ))
            .into_any_element()
    }

    fn footer_terminal(&self, i18n: I18n, target: &Entity<Self>) -> gpui::AnyElement {
        footer_band()
            .child(fb_button(
                "export-close-term",
                i18n.tr_or("export.action.close", "Close"),
                FbButtonKind::Primary,
                true,
                {
                    let target = target.clone();
                    move |_, window, cx| {
                        let _ = target.update(cx, |this, cx| this.close(window, cx));
                    }
                },
            ))
            .into_any_element()
    }

    // ── Choice plumbing ──────────────────────────────────────────────────────

    fn dropdown(
        &self,
        field: SelectField,
        id: &'static str,
        selected: String,
        options: Vec<SelectOption>,
        target: &Entity<Self>,
    ) -> impl IntoElement {
        let open = self.open_select == Some(field);
        let toggle_target = target.clone();
        let change_target = target.clone();
        select(
            id,
            Some(selected.as_str()),
            selected.clone(),
            options,
            open,
            false,
            Arc::new(move |_, _window, cx| {
                let _ = toggle_target.update(cx, |this, cx| {
                    this.open_select = if this.open_select == Some(field) {
                        None
                    } else {
                        Some(field)
                    };
                    cx.notify();
                });
            }),
            Arc::new(move |value, _window, cx| {
                let value = value.clone();
                let _ = change_target.update(cx, |this, cx| {
                    this.apply_select(field, &value);
                    this.open_select = None;
                    this.refresh_estimate();
                    cx.notify();
                });
            }),
        )
    }

    fn set_mode(&mut self, mode: ExportMode) {
        self.settings.mode = mode;
        // The engine rejects normalization for batch exports outright, so the
        // setting follows the mode rather than failing later.
        if mode != ExportMode::Mixdown {
            self.settings.normalize = ExportNormalizeChoice::Off;
        }
        self.refresh_estimate();
    }

    fn apply_select(&mut self, field: SelectField, value: &str) {
        match field {
            SelectField::Format => {
                self.settings.format = match value {
                    "flac" => AudioFileFormat::Flac,
                    "mp3" => AudioFileFormat::Mp3,
                    _ => AudioFileFormat::Wav,
                };
                self.sync_output_from_name();
            }
            SelectField::FormatOption => match self.settings.format {
                AudioFileFormat::Wav => {
                    self.settings.wav_sample_format = match value {
                        "f32" => sphere_encoder::AudioSampleFormat::F32,
                        "i16" => sphere_encoder::AudioSampleFormat::I16,
                        _ => sphere_encoder::AudioSampleFormat::I24,
                    };
                }
                AudioFileFormat::Flac => {
                    self.settings.flac_bit_depth = if value == "16" { 16 } else { 24 };
                }
                AudioFileFormat::Mp3 => {
                    self.settings.mp3_bitrate_kbps = value.parse().unwrap_or(256);
                }
                AudioFileFormat::Rauf => {}
            },
            SelectField::FlacCompression => {
                if let Ok(level) = value.parse::<u8>() {
                    self.settings.flac_compression_level =
                        Some(level.clamp(FLAC_COMPRESSION_RANGE.0, FLAC_COMPRESSION_RANGE.1));
                }
            }
            SelectField::Range => {
                let range = match value {
                    "selection" => self
                        .defaults
                        .time_selection
                        .map(|(start_beat, end_beat)| ExportRangeChoice::TimeSelection {
                            start_beat,
                            end_beat,
                        })
                        .unwrap_or(ExportRangeChoice::EntireArrangement),
                    "loop" => self
                        .defaults
                        .loop_range
                        .map(|(start_beat, end_beat)| ExportRangeChoice::LoopRange {
                            start_beat,
                            end_beat,
                        })
                        .unwrap_or(ExportRangeChoice::EntireArrangement),
                    "custom" => {
                        // Seed from whatever the user last had in the fields, so
                        // switching back and forth does not discard an edit.
                        let fallback_end = self.defaults.content_end_beat.max(1.0);
                        let start = self
                            .range_start_input
                            .value
                            .trim()
                            .parse::<f64>()
                            .ok()
                            .filter(|v| v.is_finite() && *v >= 0.0)
                            .unwrap_or(0.0);
                        let end = self
                            .range_end_input
                            .value
                            .trim()
                            .parse::<f64>()
                            .ok()
                            .filter(|v| v.is_finite() && *v >= 0.0)
                            .unwrap_or(fallback_end);
                        self.seed_range_inputs(start, end);
                        ExportRangeChoice::Custom {
                            start_beat: start,
                            end_beat: end,
                        }
                    }
                    _ => ExportRangeChoice::EntireArrangement,
                };
                self.settings.range = range;
                if !matches!(range, ExportRangeChoice::Custom { .. }) {
                    self.range_draft_invalid = false;
                }
            }
            SelectField::SampleRate => {
                self.settings.sample_rate = match value {
                    "44100" => ExportSampleRateChoice::Hz44100,
                    "48000" => ExportSampleRateChoice::Hz48000,
                    "88200" => ExportSampleRateChoice::Hz88200,
                    "96000" => ExportSampleRateChoice::Hz96000,
                    _ => ExportSampleRateChoice::Project,
                };
            }
            SelectField::Normalize => {
                self.settings.normalize = parse_peak_option(value)
                    .map(ExportNormalizeChoice::PeakDb)
                    .unwrap_or(ExportNormalizeChoice::Off);
            }
            SelectField::Tail => {
                self.settings.tail = match value {
                    "fixed" => ExportTailChoice::FixedSeconds(TAIL_FIXED_SECONDS),
                    "silence" => ExportTailChoice::UntilSilence {
                        max_seconds: TAIL_SILENCE_MAX_SECONDS,
                        threshold_db: TAIL_SILENCE_THRESHOLD_DB,
                    },
                    _ => ExportTailChoice::None,
                };
            }
        }
    }
}

// ── IME bridge ───────────────────────────────────────────────────────────────

/// Multi-field IME bridge.
///
/// Every platform text commit — including CJK/Thai composition, which never
/// passes through `handle_key` — has to reach the focused field *and* re-derive
/// the values that depend on it. Missing that is how the output path silently
/// desyncs from the name in the locales this app actually ships.
impl EntityInputHandler for ExportArrangementWindow {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let field = self.focused_field(window);
        match field {
            Some(ExportField::RangeStart) => self
                .range_start_input
                .text_for_utf16_range(range, actual_range),
            Some(ExportField::RangeEnd) => self
                .range_end_input
                .text_for_utf16_range(range, actual_range),
            _ => self.name_input.text_for_utf16_range(range, actual_range),
        }
    }

    fn selected_text_range(
        &mut self,
        ignore_disabled_input: bool,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let field = self.focused_field(window);
        match field {
            Some(ExportField::RangeStart) => self
                .range_start_input
                .selected_text_range_utf16(ignore_disabled_input),
            Some(ExportField::RangeEnd) => self
                .range_end_input
                .selected_text_range_utf16(ignore_disabled_input),
            _ => self
                .name_input
                .selected_text_range_utf16(ignore_disabled_input),
        }
    }

    fn marked_text_range(
        &self,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        let field = self.focused_field(window);
        match field {
            Some(ExportField::RangeStart) => self.range_start_input.marked_text_range_utf16(),
            Some(ExportField::RangeEnd) => self.range_end_input.marked_text_range_utf16(),
            _ => self.name_input.marked_text_range_utf16(),
        }
    }

    fn unmark_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let field = self.focused_field(window);
        match field {
            Some(ExportField::RangeStart) => self.range_start_input.unmark_text(),
            Some(ExportField::RangeEnd) => self.range_end_input.unmark_text(),
            _ => self.name_input.unmark_text(),
        }
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let field = self.focused_field(window);
        match field {
            Some(ExportField::RangeStart) => {
                self.range_start_input
                    .replace_text_in_utf16_range(range, text);
                self.sync_range_from_inputs();
            }
            Some(ExportField::RangeEnd) => {
                self.range_end_input
                    .replace_text_in_utf16_range(range, text);
                self.sync_range_from_inputs();
            }
            _ => {
                self.name_input.replace_text_in_utf16_range(range, text);
                self.sync_output_from_name();
            }
        }
        self.refresh_estimate();
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let field = self.focused_field(window);
        match field {
            Some(ExportField::RangeStart) => {
                self.range_start_input.replace_and_mark_text_in_utf16_range(
                    range,
                    new_text,
                    new_selected_range,
                );
                self.sync_range_from_inputs();
            }
            Some(ExportField::RangeEnd) => {
                self.range_end_input.replace_and_mark_text_in_utf16_range(
                    range,
                    new_text,
                    new_selected_range,
                );
                self.sync_range_from_inputs();
            }
            _ => {
                self.name_input.replace_and_mark_text_in_utf16_range(
                    range,
                    new_text,
                    new_selected_range,
                );
                self.sync_output_from_name();
            }
        }
        self.refresh_estimate();
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let field = self.focused_field(window);
        match field {
            Some(ExportField::RangeStart) => self
                .range_start_input
                .bounds_for_utf16_range(range_utf16, element_bounds),
            Some(ExportField::RangeEnd) => self
                .range_end_input
                .bounds_for_utf16_range(range_utf16, element_bounds),
            _ => self
                .name_input
                .bounds_for_utf16_range(range_utf16, element_bounds),
        }
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

// ── Layout primitives ────────────────────────────────────────────────────────

/// The dialog's single scroll owner and horizontal clip owner.
fn body_scroll() -> gpui::Stateful<gpui::Div> {
    div()
        .id("export-body")
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        // Long paths and track names truncate; a dialog that scrolls sideways
        // is a layout bug, not a feature.
        .overflow_x_hidden()
        .gap(px(space::LOOSE))
        .px(px(BODY_PAD_X))
        .py(px(space::LOOSE))
}

/// Caption plus a bordered card of rows. This is the dialog card idiom (the
/// Add Track dialog's `form_panel`), not the right dock's inspector card.
fn form_section(title: String, rows: Vec<gpui::AnyElement>) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .w_full()
        .max_w(px(FORM_MAX_WIDTH))
        .flex_shrink_0()
        .gap(px(space::SNUG))
        .child(fb_section_header(title))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(space::SNUG))
                .rounded(px(radius::SURFACE))
                .border(px(1.0))
                .border_color(Colors::border_subtle())
                .bg(Colors::surface_panel_alt())
                .p(px(space::BASE))
                .children(rows),
        )
}

fn form_row(label: String, control: impl IntoElement) -> gpui::AnyElement {
    crate::components::controls::fb_form_row(label, control).into_any_element()
}

/// Right-aligned readout. Values take tabular figures so a column of numbers
/// lines up on one grid, and truncate rather than wrap.
fn readout_row(label: String, value: String) -> gpui::AnyElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::BASE))
        .min_h(px(size::DEFAULT))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(typography::DENSE_LABEL))
                .font_weight(FontWeight::MEDIUM)
                .text_color(Colors::text_muted())
                .child(label),
        )
        .child(
            div()
                .flex_shrink_0()
                .max_w(px(READOUT_VALUE_MAX))
                .truncate()
                .font_features(tabular_figures())
                .text_size(px(typography::UI_XS))
                .font_weight(FontWeight::MEDIUM)
                .text_color(Colors::text_primary())
                .child(value),
        )
        .into_any_element()
}

/// One written file: the name leads, the measurement stays quiet behind it.
fn file_row(name: String, meta: String) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::BASE))
        .min_h(px(size::DEFAULT))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(typography::UI_XS))
                .text_color(Colors::text_primary())
                .child(name),
        )
        .child(
            div()
                .flex_shrink_0()
                .font_features(tabular_figures())
                .text_size(px(typography::DENSE_LABEL))
                .text_color(Colors::text_muted())
                .child(meta),
        )
}

/// Full-width path line: the value needs the whole row, so it does not share
/// one with a label column.
fn path_row(label: String, path: String) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(space::HAIR))
        .child(
            div()
                .text_size(px(typography::DENSE_LABEL))
                .font_weight(FontWeight::MEDIUM)
                .text_color(Colors::text_muted())
                .child(label),
        )
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_size(px(typography::UI_XS))
                .text_color(Colors::text_secondary())
                .child(path),
        )
}

/// A quiet full-width explanation inside a section. Wraps: it is prose, not a
/// chrome label.
fn note_row(text: String) -> gpui::AnyElement {
    div()
        .text_size(px(typography::DENSE_LABEL))
        .text_color(Colors::text_muted())
        .child(text)
        .into_any_element()
}

/// Read-only, recessed surface for a value the user cannot type into.
fn readout_surface(text: String) -> impl IntoElement {
    div()
        .flex_1()
        .min_w_0()
        .h(px(size::COMFORTABLE))
        .px(px(space::BASE))
        .flex()
        .items_center()
        .rounded(px(radius::CONTROL))
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_input())
        .text_size(px(typography::UI_XS))
        .text_color(Colors::text_secondary())
        .truncate()
        .child(text)
}

fn beat_field(
    state: &TextInputState,
    window: &Window,
    target: &Entity<ExportArrangementWindow>,
    get: fn(&mut ExportArrangementWindow) -> &mut TextInputState,
) -> impl IntoElement {
    let focused = state.is_focused(window);
    let callbacks = bind_mouse_selection(target.clone(), get);
    div()
        .w(px(BEAT_FIELD_WIDTH))
        .child(text_field_with_callbacks_and_ime(
            state,
            focused,
            callbacks,
            target.clone(),
        ))
}

/// The action band. One accent primary action, an explicit Cancel, and a fixed
/// height so the footer never moves between dialog states.
fn footer_band() -> gpui::Div {
    div()
        .flex_shrink_0()
        .flex()
        .flex_row()
        .items_center()
        .justify_end()
        .gap(px(space::BASE))
        .h(px(FOOTER_HEIGHT))
        .px(px(BODY_PAD_X))
        .border_t(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_titlebar())
}

/// Tabular (fixed-advance) figures for readouts, per DESIGN.md's numeric rules.
/// A no-op on faces without the feature, so it can never make text worse.
fn tabular_figures() -> gpui::FontFeatures {
    gpui::FontFeatures(Arc::new(vec![("tnum".to_string(), 1)]))
}

// ── Formatting ───────────────────────────────────────────────────────────────

/// `m:ss.mmm` — the arrangement's own time language.
fn format_duration(seconds: f64) -> String {
    let clamped = if seconds.is_finite() && seconds > 0.0 {
        seconds
    } else {
        0.0
    };
    let total_ms = (clamped * 1000.0).round() as u64;
    let minutes = total_ms / 60_000;
    let secs = (total_ms % 60_000) / 1000;
    let ms = total_ms % 1000;
    format!("{minutes}:{secs:02}.{ms:03}")
}

fn format_beats(beats: f64) -> String {
    let value = if beats.is_finite() { beats } else { 0.0 };
    format!("{value:.3}")
}

fn format_db(db: f32) -> String {
    // U+2212 MINUS SIGN: a hyphen at 11 px reads as a dash, not a sign.
    if db < 0.0 {
        format!("\u{2212}{:.1}", -db)
    } else {
        format!("{db:.1}")
    }
}

fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let value = bytes as f64;
    if value < KIB {
        format!("{bytes} B")
    } else if value < KIB * KIB {
        format!("{:.1} KB", value / KIB)
    } else if value < KIB * KIB * KIB {
        format!("{:.1} MB", value / (KIB * KIB))
    } else {
        format!("{:.2} GB", value / (KIB * KIB * KIB))
    }
}

/// Thousands-grouped integer. The value div is `whitespace_nowrap`, so a plain
/// space cannot break the number across lines.
fn grouped(value: u64) -> String {
    let digits = value.to_string();
    let len = digits.len();
    let mut out = String::with_capacity(len + len / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (len - index) % 3 == 0 {
            out.push(' ');
        }
        out.push(ch);
    }
    out
}

fn channel_label(channels: u16, i18n: I18n) -> String {
    if channels == 1 {
        i18n.tr_or("export.channels.mono", "Mono")
    } else {
        i18n.tr_or("export.channels.stereo", "Stereo")
    }
}

/// Rate · channels · resolution. MP3 has no meaningful bit depth — the settings
/// model only carries one because the container API wants a field — so it shows
/// the bitrate it is actually encoded at.
fn output_spec(estimate: &ExportEstimate, i18n: I18n) -> String {
    let channels = channel_label(estimate.channels, i18n);
    match estimate.mp3_bitrate_kbps {
        Some(kbps) => format!("{} Hz · {channels} · {kbps} kbps", estimate.sample_rate),
        None => format!(
            "{} Hz · {channels} · {}-bit",
            estimate.sample_rate,
            estimate.sample_format.bits()
        ),
    }
}

fn stage_key(stage: ExportStage) -> &'static str {
    match stage {
        ExportStage::Preparing => "export.stage.preparing",
        ExportStage::Rendering => "export.stage.rendering",
        ExportStage::AnalyzingPeak => "export.stage.analyzing-peak",
        ExportStage::Encoding => "export.stage.encoding",
        ExportStage::Finalizing => "export.stage.finalizing",
        ExportStage::Complete => "export.stage.complete",
        ExportStage::Failed => "export.stage.failed",
        ExportStage::Cancelled => "export.stage.cancelled",
    }
}

fn peak_option_id(db: f32) -> String {
    format!("peak:{db:.1}")
}

fn parse_peak_option(value: &str) -> Option<f32> {
    value.strip_prefix("peak:")?.parse::<f32>().ok()
}

fn localized_error(language: &str, error: &ExportSettingsError) -> String {
    I18n::new(language).tr_or(error.message_key(), &error.user_message())
}

/// `I18n::tr_vars` falls back to the *key* when a message is missing, which
/// would print `export.readout.n-of-m` in the UI. This keeps the English
/// fallback and applies the same `{ $name }` substitution to whichever string
/// wins.
fn tr_vars_or(i18n: I18n, key: &str, fallback: &str, vars: &[(&str, String)]) -> String {
    let mut text = i18n.tr_or(key, fallback);
    for (name, value) in vars {
        text = text.replace(&format!("{{ ${name} }}"), value);
    }
    text
}

fn file_label(path: &std::path::Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| path.display().to_string())
}

// ── Batch targets ────────────────────────────────────────────────────────────

fn batch_folder_suffix(mode: ExportMode) -> &'static str {
    if mode == ExportMode::Stems {
        "Stems"
    } else {
        "Multitrack"
    }
}

pub(super) fn build_batch_targets(
    request: &DirectAudio::ArrangementExportRequest,
    mode: ExportMode,
    defaults: &ExportProjectDefaults,
    project_name: &str,
) -> Vec<TrackExportTarget> {
    if mode == ExportMode::Mixdown {
        return Vec::new();
    }
    let base_dir = request
        .output_path
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(std::env::temp_dir);
    let output_dir = base_dir.join(format!(
        "{} {}",
        sanitize_file_stem(project_name),
        batch_folder_suffix(mode)
    ));

    defaults
        .track_targets
        .iter()
        .filter(|target| mode.selects(target))
        .enumerate()
        .map(|(index, target)| {
            let mut item_request = request.clone();
            item_request.output_path = output_dir.join(format!(
                "{:02} {}.{}",
                index + 1,
                sanitize_file_stem(&target.name),
                request.format.extension()
            ));
            TrackExportTarget {
                track_id: target.id.clone(),
                request: item_request,
            }
        })
        .collect()
}

/// Destinations this job would replace. The exporter overwrites silently on
/// success, so this is what turns the Export press into a confirmable action.
fn existing_destinations(
    request: &ArrangementExportRequest,
    batch_targets: &[TrackExportTarget],
) -> Vec<PathBuf> {
    if batch_targets.is_empty() {
        if request.output_path.exists() {
            vec![request.output_path.clone()]
        } else {
            Vec::new()
        }
    } else {
        batch_targets
            .iter()
            .map(|target| target.request.output_path.clone())
            .filter(|path| path.exists())
            .collect()
    }
}

fn sanitize_file_stem(name: &str) -> String {
    let sanitized: String = name
        .trim()
        .chars()
        .map(|character| {
            if matches!(
                character,
                '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
            ) {
                '_'
            } else {
                character
            }
        })
        .collect();
    if sanitized.is_empty() {
        "Track".to_string()
    } else {
        sanitized
    }
}

/// Stem for the mixdown file / stems folder prefix. Empty input falls back to
/// "Export" (not "Track" — that default is for unnamed mixer channels).
fn export_stem_from_input(name: &str) -> String {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return "Export".to_string();
    }
    sanitize_file_stem(trimmed)
}

fn open_in_file_manager(dir: &std::path::Path) -> std::io::Result<()> {
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("explorer").arg(dir).spawn()?;
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(dir).spawn()?;
    }
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open").arg(dir).spawn()?;
    }
    Ok(())
}

// ── Opener ───────────────────────────────────────────────────────────────────

/// Open the external Export Arrangement window centered over `owner_bounds`.
pub fn open_export_arrangement_window(
    owner_bounds: Option<Bounds<gpui::Pixels>>,
    project_name: String,
    snapshot: EngineProjectSnapshot,
    bridge_sinks: PluginBridgeSinkMap,
    audio_engine: Option<DirectAudio::AudioEngine>,
    defaults: ExportProjectDefaults,
    default_output: Option<PathBuf>,
    cx: &mut App,
) -> Result<WindowHandle<ExportArrangementWindow>, String> {
    use crate::window_position::{apply_owner_display, centered_window_bounds};
    use gpui::{size, WindowBackgroundAppearance, WindowBounds, WindowKind};

    let height = TITLEBAR_HEIGHT + EXPORT_WINDOW_HEIGHT;
    let window_bounds =
        centered_window_bounds(owner_bounds, size(px(EXPORT_WINDOW_WIDTH), px(height)), cx);

    let mut window_options = crate::platform_chrome::external_dialog_window_options_partial();
    window_options.window_bounds = Some(WindowBounds::Windowed(window_bounds));
    window_options.kind = WindowKind::Dialog;
    // Resizable so the form can be validated (and used) at narrow, short and
    // maximized sizes; the body owns the scroll that makes that safe.
    window_options.is_resizable = true;
    window_options.is_minimizable = false;
    window_options.window_background = WindowBackgroundAppearance::Transparent;
    window_options.window_min_size = Some(size(
        px(EXPORT_WINDOW_MIN_WIDTH),
        px(TITLEBAR_HEIGHT + EXPORT_WINDOW_MIN_HEIGHT),
    ));
    apply_owner_display(&mut window_options, owner_bounds, cx);

    cx.open_window(window_options, move |_window, cx| {
        cx.new(|cx| {
            let mut win = ExportArrangementWindow::new(
                project_name,
                snapshot,
                bridge_sinks,
                audio_engine,
                defaults,
                cx,
            );
            if let Some(path) = default_output {
                win.set_default_output(path);
            }
            win
        })
    })
    .map_err(|e| e.to_string())
}

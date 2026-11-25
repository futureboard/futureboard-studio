//! Native Stem Extractor dialog.
//!
//! Compact Futureboard dialog for offline MDX-NET stem separation.
//! Owns serializable [`StemExtractParams`] from SphereAudioProcessor, edits them
//! in the UI, and runs a cancellable background job that never leases GPUI
//! entities during decode/separate/encode work.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, size, svg, App, AppContext, Bounds, Context, FocusHandle, InteractiveElement,
    IntoElement, KeyDownEvent, ParentElement, Render, Role, SharedString,
    StatefulInteractiveElement, Styled, Toggled, Window, WindowBackgroundAppearance, WindowBounds,
    WindowHandle, WindowKind,
};
use sphere_encoder::{
    create_encoder, AudioEncodeOptions, AudioEncodeSpec, AudioFileFormat, AudioSampleFormat,
};

use crate::assets;
use crate::components::controls::{
    fb_button, fb_segment, fb_segmented_track, FbButtonKind, FbSegment,
};
use crate::components::form::select::{select, SelectOption};
use crate::components::progress_dialog::{progress_bar, ProgressBarValue};
use crate::components::title_bar::{external_window_titlebar_compact, TITLEBAR_HEIGHT};
use crate::theme::{self, elevation, radius, space, state, typography, Colors};
use crate::window_position::{apply_owner_display, centered_window_bounds};

pub const STEM_EXTRACTOR_WINDOW_WIDTH: f32 = 520.0;
const STEM_EXTRACTOR_WINDOW_HEIGHT: f32 = 610.0;
const BODY_PAD: f32 = space::SECTION + space::TIGHT;
/// Width of the label column in step 3 (Model, Quality).
const FIELD_LABEL_W: f32 = 52.0;
/// The number disc in front of each step title.
const STEP_DISC: f32 = 20.0;
const STEM_TILE_H: f32 = 66.0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum SelectField {
    Clip,
    Model,
    Quality,
}

/// One resolvable audio clip on a track, offered as a Stem Extractor source.
#[derive(Clone, Debug, PartialEq)]
pub struct StemSourceClip {
    pub clip_id: String,
    pub track_id: String,
    pub track_name: String,
    pub clip_name: String,
    pub source_path: PathBuf,
    pub start_beat: f32,
    pub duration_beats: f32,
    pub source_duration_seconds: Option<f64>,
}

impl StemSourceClip {
    pub fn label(&self) -> String {
        format!("{} · {}", self.track_name, self.clip_name)
    }
}

pub enum StemExtractJobState {
    Editing,
    Running(SphereAudioProcessor::StemExtractProgress),
    Complete(StemExtractJobSummary),
    Failed(String),
    Cancelled,
}

/// One rendered stem ready to place on a new arrangement track.
#[derive(Clone, Debug)]
pub struct StemExtractResultStem {
    pub kind: SphereAudioProcessor::StemKind,
    pub path: PathBuf,
}

/// Result handed to StudioLayout after a successful extract job.
#[derive(Clone, Debug)]
pub struct StemExtractApplyRequest {
    pub source_track_id: String,
    pub source_clip_id: String,
    pub source_clip_name: String,
    pub start_beat: f32,
    pub duration_beats: f32,
    pub source_duration_seconds: Option<f64>,
    pub stems: Vec<StemExtractResultStem>,
}

#[derive(Clone, Debug)]
pub struct StemExtractJobSummary {
    pub model: SphereAudioProcessor::StemModel,
    pub device: SphereAudioProcessor::InferDevice,
    pub runtime: SphereAudioProcessor::StemPlatformRuntime,
    pub backend: SphereAudioProcessor::InferBackendKind,
    pub apply: StemExtractApplyRequest,
}

#[derive(Default)]
struct SharedJob {
    progress: Option<SphereAudioProcessor::StemExtractProgress>,
    done: Option<Result<StemExtractJobSummary, String>>,
}

struct ActiveJob {
    shared: Arc<Mutex<SharedJob>>,
    cancel: SphereAudioProcessor::StemExtractCancelToken,
}

#[derive(Default)]
struct SharedDownloadJob {
    progress: Option<SphereAudioProcessor::StemModelDownloadProgress>,
    done: Option<Result<(), String>>,
}

struct ActiveDownload {
    shared: Arc<Mutex<SharedDownloadJob>>,
    cancel: SphereAudioProcessor::StemExtractCancelToken,
}

enum ModelDownloadUi {
    Idle,
    Running(SphereAudioProcessor::StemModelDownloadProgress),
    Failed(String),
}

/// Plain data captured when the dialog opens — never a live entity borrow.
#[derive(Clone, Debug, Default)]
pub struct StemExtractorDialogDefaults {
    pub project_name: String,
    /// Audio clips currently on arrangement tracks with a resolvable media path.
    pub audio_clips: Vec<StemSourceClip>,
    /// Preselected clip id (usually the timeline selection).
    pub selected_clip_id: Option<String>,
    /// Project root used for rendered stem WAV files (`Rendered/Stems`).
    pub project_root: Option<PathBuf>,
    /// ONNX install folder (`Documents/Futureboard Studio/Utilities/Models`).
    pub models_dir: Option<PathBuf>,
}

pub struct StemExtractorWindow {
    defaults: StemExtractorDialogDefaults,
    params: SphereAudioProcessor::StemExtractParams,
    selected_clip_id: Option<String>,
    source_path: Option<PathBuf>,
    state: StemExtractJobState,
    open_select: Option<SelectField>,
    job: Option<ActiveJob>,
    download: Option<ActiveDownload>,
    download_ui: ModelDownloadUi,
    models_dir: PathBuf,
    model_installed: bool,
    focus_handle: FocusHandle,

    on_apply: Arc<dyn Fn(StemExtractApplyRequest, &mut App) + 'static>,
    applied: bool,
}

impl StemExtractorWindow {
    pub fn new(
        defaults: StemExtractorDialogDefaults,
        on_apply: Arc<dyn Fn(StemExtractApplyRequest, &mut App) + 'static>,
        cx: &mut Context<Self>,
    ) -> Self {
        let selected_clip_id = defaults
            .selected_clip_id
            .clone()
            .or_else(|| defaults.audio_clips.first().map(|c| c.clip_id.clone()));
        let source_path = selected_clip_id.as_ref().and_then(|id| {
            defaults
                .audio_clips
                .iter()
                .find(|c| &c.clip_id == id)
                .map(|c| c.source_path.clone())
        });
        let models_dir = defaults
            .models_dir
            .clone()
            .unwrap_or_else(SphereAudioProcessor::default_models_dir);
        let _ = SphereAudioProcessor::ensure_models_dir(&models_dir);
        // The worker resolves the platform runtime when extraction starts.
        let params = SphereAudioProcessor::auto_stem_extract_params();
        let model_installed = SphereAudioProcessor::model_installed(params.model, &models_dir);
        Self {
            defaults,
            params,
            selected_clip_id,
            source_path,
            state: StemExtractJobState::Editing,
            open_select: None,
            job: None,
            download: None,
            download_ui: ModelDownloadUi::Idle,
            models_dir,
            model_installed,
            focus_handle: cx.focus_handle(),

            on_apply,
            applied: false,
        }
    }

    fn refresh_model_installed(&mut self) {
        self.model_installed =
            SphereAudioProcessor::model_installed(self.params.model, &self.models_dir);
    }

    fn is_downloading(&self) -> bool {
        self.download.is_some() || matches!(self.download_ui, ModelDownloadUi::Running(_))
    }

    fn selected_clip(&self) -> Option<&StemSourceClip> {
        let id = self.selected_clip_id.as_ref()?;
        self.defaults
            .audio_clips
            .iter()
            .find(|clip| &clip.clip_id == id)
    }

    fn select_clip(&mut self, clip_id: &str) {
        if clip_id.is_empty() || clip_id == "__none__" {
            return;
        }
        if let Some(clip) = self
            .defaults
            .audio_clips
            .iter()
            .find(|clip| clip.clip_id == clip_id)
        {
            self.selected_clip_id = Some(clip.clip_id.clone());
            self.source_path = Some(clip.source_path.clone());
        }
    }

    fn close(&mut self, window: &mut Window, _cx: &mut Context<Self>) {
        if let Some(job) = &self.job {
            job.cancel.cancel();
        }
        if let Some(download) = &self.download {
            download.cancel.cancel();
        }
        window.remove_window();
    }

    fn handle_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let key = event.keystroke.key.as_str();
        match key {
            "escape" => {
                if self.open_select.is_some() {
                    self.open_select = None;
                    cx.notify();
                } else if self.is_downloading() {
                    self.request_cancel_download(cx);
                } else if matches!(self.state, StemExtractJobState::Running(_)) {
                    self.request_cancel(cx);
                } else {
                    self.close(window, cx);
                }
                true
            }
            "enter"
                if matches!(self.state, StemExtractJobState::Editing)
                    && self.extract_blocker().is_none() =>
            {
                self.start_extract(cx);
                true
            }
            _ => false,
        }
    }

    fn request_cancel(&mut self, cx: &mut Context<Self>) {
        if let Some(job) = &self.job {
            job.cancel.cancel();
        }
        cx.notify();
    }

    fn request_cancel_download(&mut self, cx: &mut Context<Self>) {
        if let Some(download) = &self.download {
            download.cancel.cancel();
        }
        cx.notify();
    }

    fn start_download(&mut self, cx: &mut Context<Self>) {
        if self.is_downloading() || matches!(self.state, StemExtractJobState::Running(_)) {
            return;
        }
        if self.model_installed {
            return;
        }
        let models_dir = self.models_dir.clone();
        let model = self.params.model;
        let shared = Arc::new(Mutex::new(SharedDownloadJob::default()));
        let cancel = SphereAudioProcessor::StemExtractCancelToken::new();
        self.download = Some(ActiveDownload {
            shared: shared.clone(),
            cancel: cancel.clone(),
        });
        self.download_ui =
            ModelDownloadUi::Running(SphereAudioProcessor::StemModelDownloadProgress::new(
                model,
                model
                    .package()
                    .files
                    .first()
                    .map(|f| f.file_name)
                    .unwrap_or("model"),
                0,
                model.package().file_count().max(1),
                0,
                None,
                0.0,
                format!("Starting {} download…", model.label()),
            ));

        let worker_shared = shared.clone();
        let bg_job = crate::components::background_tasks::BackgroundJob::start_cancellable(
            crate::components::BackgroundTaskKind::MediaCopy,
            format!("Download {} model", model.label()),
        );
        std::thread::Builder::new()
            .name("fb-stem-model-dl".to_string())
            .spawn(move || {
                let progress_shared = worker_shared.clone();
                let job = &bg_job;
                let job_cancel = cancel.clone();
                let mut on_progress =
                    move |progress: SphereAudioProcessor::StemModelDownloadProgress| {
                        if job.is_cancelled() {
                            job_cancel.cancel();
                        }
                        job.set_detail(progress.detail.clone());
                        job.set_progress((progress.percent * 10.0) as u32, 1000);
                        if let Ok(mut guard) = progress_shared.lock() {
                            guard.progress = Some(progress);
                        }
                    };
                let result = SphereAudioProcessor::download_model(
                    model,
                    &models_dir,
                    &cancel,
                    &mut on_progress,
                )
                .map_err(|e| e.user_message());
                match &result {
                    Ok(()) => bg_job.complete(None),
                    Err(_) if cancel.is_cancelled() => drop(bg_job),
                    Err(message) => bg_job.fail(message.clone()),
                }
                if let Ok(mut guard) = worker_shared.lock() {
                    guard.done = Some(result);
                }
            })
            .ok();

        cx.spawn(async move |this, cx| {
            let executor = cx.background_executor().clone();
            loop {
                if crate::shutdown::ShutdownState::global().is_shutting_down() {
                    break;
                }
                executor.timer(std::time::Duration::from_millis(50)).await;
                let keep_going = this
                    .update(cx, |this, cx| this.poll_download(cx))
                    .unwrap_or(false);
                if !keep_going {
                    break;
                }
            }
        })
        .detach();
        cx.notify();
    }

    fn poll_download(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(job) = &self.download else {
            return false;
        };
        let (progress, done) = {
            let Ok(mut guard) = job.shared.lock() else {
                return true;
            };
            (guard.progress.take(), guard.done.take())
        };
        if let Some(done) = done {
            match done {
                Ok(()) => {
                    self.refresh_model_installed();
                    self.download_ui = ModelDownloadUi::Idle;
                }
                Err(message)
                    if job.cancel.is_cancelled()
                        || message.to_ascii_lowercase().contains("cancel") =>
                {
                    self.download_ui = ModelDownloadUi::Idle;
                }
                Err(message) => {
                    self.download_ui = ModelDownloadUi::Failed(message);
                }
            }
            self.download = None;
            cx.notify();
            return false;
        }
        if let Some(progress) = progress {
            self.download_ui = ModelDownloadUi::Running(progress);
            cx.notify();
        }
        true
    }

    fn start_extract(&mut self, cx: &mut Context<Self>) {
        self.open_select = None;
        self.applied = false;
        if let Err(err) = self.params.validate() {
            self.state = StemExtractJobState::Failed(err.user_message());
            cx.notify();
            return;
        }
        let Some(clip) = self.selected_clip().cloned() else {
            self.state = StemExtractJobState::Failed(
                "Select an audio clip on a track as the source.".into(),
            );
            cx.notify();
            return;
        };
        let source_path = clip.source_path.clone();
        if !source_path.exists() {
            self.state = StemExtractJobState::Failed(format!(
                "Audio clip media is missing: {}",
                source_path.display()
            ));
            cx.notify();
            return;
        }
        let output_dir = resolve_render_dir(self.defaults.project_root.as_deref());
        if let Err(err) = std::fs::create_dir_all(&output_dir) {
            self.state =
                StemExtractJobState::Failed(format!("Could not create stem render folder: {err}"));
            cx.notify();
            return;
        }

        let shared = Arc::new(Mutex::new(SharedJob::default()));
        let cancel = SphereAudioProcessor::StemExtractCancelToken::new();
        self.job = Some(ActiveJob {
            shared: shared.clone(),
            cancel: cancel.clone(),
        });
        self.state = StemExtractJobState::Running(SphereAudioProcessor::StemExtractProgress::new(
            SphereAudioProcessor::StemExtractStage::Preparing,
            0.0,
            "Preparing…",
        ));

        let params = self.params.clone();
        let source_stem = sanitize_file_stem(&clip.clip_name);
        let apply_meta = StemExtractApplyRequest {
            source_track_id: clip.track_id.clone(),
            source_clip_id: clip.clip_id.clone(),
            source_clip_name: clip.clip_name.clone(),
            start_beat: clip.start_beat,
            duration_beats: clip.duration_beats,
            source_duration_seconds: clip.source_duration_seconds,
            stems: Vec::new(),
        };
        let worker_shared = shared.clone();
        let bg_job = crate::components::background_tasks::BackgroundJob::start_cancellable(
            crate::components::BackgroundTaskKind::AudioProcess,
            format!("Extract stems: {}", clip.clip_name),
        );
        std::thread::Builder::new()
            .name("fb-stem-extract".to_string())
            .spawn(move || {
                let progress_shared = worker_shared.clone();
                let job = &bg_job;
                let job_cancel = cancel.clone();
                let mut on_progress = move |progress: SphereAudioProcessor::StemExtractProgress| {
                    if job.is_cancelled() {
                        job_cancel.cancel();
                    }
                    job.set_detail(progress.detail.clone());
                    job.set_progress((progress.percent * 10.0) as u32, 1000);
                    if let Ok(mut guard) = progress_shared.lock() {
                        guard.progress = Some(progress);
                    }
                };

                let result = (|| -> Result<StemExtractJobSummary, String> {
                    on_progress(SphereAudioProcessor::StemExtractProgress::new(
                        SphereAudioProcessor::StemExtractStage::Preparing,
                        2.0,
                        "Decoding source audio…",
                    ));
                    if cancel.is_cancelled() {
                        return Err("cancelled".into());
                    }
                    let buffer =
                        DirectAudio::load_audio_file_for_edit(&source_path.to_string_lossy())
                            .map_err(|e| e.to_string())?;
                    let channels = buffer.channels.max(1);
                    let input = SphereAudioProcessor::StemExtractInput::new(
                        buffer.sample_rate,
                        channels,
                        buffer.samples,
                    );
                    let extracted = SphereAudioProcessor::extract_stems(
                        &input,
                        &params,
                        &cancel,
                        &mut on_progress,
                    )
                    .map_err(|e| e.user_message())?;

                    if cancel.is_cancelled() {
                        return Err("cancelled".into());
                    }

                    let total = extracted.stems.len().max(1);
                    let mut stems = Vec::with_capacity(extracted.stems.len());
                    for (index, stem) in extracted.stems.iter().enumerate() {
                        if cancel.is_cancelled() {
                            return Err("cancelled".into());
                        }
                        let percent = 90.0 + (index as f32 / total as f32) * 9.0;
                        on_progress(
                            SphereAudioProcessor::StemExtractProgress::new(
                                SphereAudioProcessor::StemExtractStage::Writing,
                                percent,
                                format!("Rendering {}.wav", stem.kind.as_str()),
                            )
                            .with_stem(stem.kind),
                        );
                        let path = unique_stem_path(
                            &output_dir,
                            &source_stem,
                            stem.kind.file_stem_suffix(),
                        );
                        write_stem_wav(&path, stem)?;
                        stems.push(StemExtractResultStem {
                            kind: stem.kind,
                            path,
                        });
                    }

                    Ok(StemExtractJobSummary {
                        model: extracted.model,
                        device: extracted.device,
                        runtime: extracted.runtime,
                        backend: extracted.backend,
                        apply: StemExtractApplyRequest {
                            stems,
                            ..apply_meta
                        },
                    })
                })();
                match &result {
                    Ok(summary) => bg_job.complete(Some(format!(
                        "{} stem{}",
                        summary.apply.stems.len(),
                        if summary.apply.stems.len() == 1 {
                            ""
                        } else {
                            "s"
                        }
                    ))),
                    Err(_) if cancel.is_cancelled() => drop(bg_job),
                    Err(message) => bg_job.fail(message.clone()),
                }

                if let Ok(mut guard) = worker_shared.lock() {
                    guard.done = Some(result);
                }
            })
            .ok();

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
                Ok(summary) => {
                    if !self.applied {
                        (self.on_apply)(summary.apply.clone(), cx);
                        self.applied = true;
                    }
                    StemExtractJobState::Complete(summary)
                }
                Err(message)
                    if job.cancel.is_cancelled()
                        || message.to_ascii_lowercase().contains("cancel") =>
                {
                    StemExtractJobState::Cancelled
                }
                Err(message) => StemExtractJobState::Failed(message),
            };
            self.job = None;
            cx.notify();
            return false;
        }
        if let Some(progress) = progress {
            self.state = StemExtractJobState::Running(progress);
            cx.notify();
        }
        true
    }

    fn apply_select(&mut self, field: SelectField, value: &str) {
        match field {
            SelectField::Clip => self.select_clip(value),
            SelectField::Model => {
                if let Some(model) = SphereAudioProcessor::StemModel::parse(value) {
                    self.params.set_model(model);
                    self.refresh_model_installed();
                    if !matches!(self.download_ui, ModelDownloadUi::Running(_)) {
                        self.download_ui = ModelDownloadUi::Idle;
                    }
                }
            }

            SelectField::Quality => {
                if let Some(quality) = SphereAudioProcessor::StemExtractQuality::parse(value) {
                    self.params.quality = quality;
                }
            }
        }
    }
}

impl Render for StemExtractorWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Keep dialog key focus so Escape/Enter route here (not the studio).
        if !self.focus_handle.is_focused(window) {
            self.focus_handle.focus(window, cx);
        }
        let target = cx.entity().clone();
        let body = match &self.state {
            StemExtractJobState::Editing | StemExtractJobState::Failed(_) => {
                self.render_editing(target.clone())
            }
            StemExtractJobState::Running(progress) => {
                self.render_progress(progress.clone(), target.clone())
            }
            StemExtractJobState::Complete(summary) => {
                self.render_complete(summary.clone(), target.clone())
            }
            StemExtractJobState::Cancelled => self.render_cancelled(target.clone()),
        };

        div()
            .flex()
            .flex_col()
            .size_full()
            .font(theme::ui_font())
            .bg(Colors::surface_base())
            .overflow_hidden()
            .capture_key_down({
                let target = target.clone();
                move |event, window, cx| {
                    let _ = target.update(cx, |this, cx| this.handle_key(event, window, cx));
                }
            })
            .child(div().w(px(0.0)).h(px(0.0)).track_focus(&self.focus_handle))
            .child(external_window_titlebar_compact(
                "Stem Extractor".to_string(),
                "stem-extractor-close",
                {
                    let target = target.clone();
                    move |window, cx| {
                        let _ = target.update(cx, |this, cx| this.close(window, cx));
                    }
                },
            ))
            .child(body)
    }
}

impl StemExtractorWindow {
    /// Why Extract is unavailable right now, in words the footer can show.
    /// `None` when the job can start.
    fn extract_blocker(&self) -> Option<String> {
        if self.is_downloading() {
            return Some("Wait for the download to finish.".into());
        }
        if self.defaults.audio_clips.is_empty() {
            return Some("Add an audio clip to a track first.".into());
        }
        if let Err(error) = SphereAudioProcessor::resolve_current_platform_runtime() {
            return Some(error.user_message());
        }
        if self.selected_clip().is_none() || self.source_path.is_none() {
            return Some("Pick a clip to split.".into());
        }
        if self.params.stems.is_empty() {
            return Some("Pick at least one stem.".into());
        }
        if let Err(error) = self.params.validate() {
            return Some(error.user_message());
        }
        None
    }

    /// Puts the dialog straight onto a later screen, for a preview.
    #[cfg(feature = "ui-preview")]
    pub fn show_state_for_preview(&mut self, state: StemExtractJobState, cx: &mut Context<Self>) {
        self.state = state;
        cx.notify();
    }

    fn back_to_editing(&mut self, cx: &mut Context<Self>) {
        self.state = StemExtractJobState::Editing;
        cx.notify();
    }

    fn render_editing(&self, target: gpui::Entity<Self>) -> gpui::AnyElement {
        let stem_count = self.params.stems.len();
        let stems_total = self.params.model.default_stems().len();
        let mut body = div()
            .id("stem-extractor-body")
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .px(px(BODY_PAD))
            .pt(px(space::LOOSE))
            .pb(px(BODY_PAD))
            .gap(px(space::SECTION))
            .child(
                div()
                    .text_size(px(typography::UI_SM))
                    .text_color(Colors::text_secondary())
                    .child("Split a clip into separate parts. Each part lands on its own track."),
            )
            .child(step(
                1,
                "Choose a clip",
                None,
                self.render_source(target.clone()),
            ))
            .child(step(
                2,
                "Pick the stems",
                Some(
                    div()
                        .text_size(px(typography::UI_XS))
                        .text_color(Colors::text_muted())
                        .child(format!("{stem_count} of {stems_total}"))
                        .into_any_element(),
                ),
                div()
                    .flex()
                    .flex_col()
                    .gap(px(space::BASE))
                    .child(self.render_stem_tiles(target.clone()))
                    .child(caption(
                        "Each stem becomes a new track. The original track is muted.",
                    ))
                    .into_any_element(),
            ))
            .child(step(
                3,
                "Model and quality",
                None,
                div()
                    .flex()
                    .flex_col()
                    .gap(px(space::BASE))
                    .child(self.model_select(target.clone()))
                    .child(self.render_model_status(target.clone()))
                    .child(self.render_quality(target.clone()))
                    .into_any_element(),
            ));

        if let StemExtractJobState::Failed(message) = &self.state {
            body = body.child(notice(
                assets::ICON_TRIANGLE_ALERT_PATH,
                Colors::status_error(),
                "Extraction failed",
                message.clone(),
            ));
        }

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .child(body)
            .child(self.footer(target))
            .into_any_element()
    }

    fn render_source(&self, target: gpui::Entity<Self>) -> gpui::AnyElement {
        if self.defaults.audio_clips.is_empty() {
            return div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::LOOSE))
                .px(px(space::LOOSE))
                .py(px(space::LOOSE))
                .rounded(px(radius::CONTROL))
                .border(px(1.0))
                .border_dashed()
                .border_color(Colors::border_normal())
                .child(
                    svg()
                        .path(assets::ICON_AUDIO_LINES_PATH)
                        .size(px(20.0))
                        .flex_none()
                        .text_color(Colors::text_faint()),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(space::HAIR))
                        .min_w(px(0.0))
                        .child(
                            div()
                                .text_size(px(typography::UI_SM))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(Colors::text_secondary())
                                .child("No audio clips in this project yet"),
                        )
                        .child(caption(
                            "Import or record audio on a track, then open Stem Extractor again.",
                        )),
                )
                .into_any_element();
        }

        let options = self
            .defaults
            .audio_clips
            .iter()
            .map(|clip| {
                SelectOption::new(clip.clip_id.clone(), clip.label())
                    .description(clip_file_name(clip))
            })
            .collect();
        let selected = self.selected_clip_id.clone().unwrap_or_default();
        let control = self.dropdown(SelectField::Clip, "stem-clip", selected, options, target);
        let meta = self.selected_clip().map(|clip| {
            let mut parts = vec![clip_file_name(clip)];
            if let Some(seconds) = clip.source_duration_seconds {
                parts.push(format_duration(seconds));
            }
            parts.join(" · ")
        });
        div()
            .flex()
            .flex_col()
            .gap(px(space::SNUG))
            .child(control)
            .children(meta.map(caption))
            .into_any_element()
    }

    fn render_stem_tiles(&self, target: gpui::Entity<Self>) -> impl IntoElement {
        let stems = self.params.model.default_stems();
        div()
            .flex()
            .flex_row()
            .gap(px(space::BASE))
            .children(stems.iter().copied().map(|stem| {
                let checked = self.params.stems.contains(stem);
                let target = target.clone();
                stem_tile(stem, checked, move |_, _window, cx| {
                    let _ = target.update(cx, |this, cx| {
                        this.params
                            .set_stem(stem, !this.params.stems.contains(stem));
                        if matches!(this.state, StemExtractJobState::Failed(_)) {
                            this.state = StemExtractJobState::Editing;
                        }
                        cx.notify();
                    });
                })
            }))
    }

    fn model_select(&self, target: gpui::Entity<Self>) -> impl IntoElement {
        let models_dir = self.models_dir.clone();
        let options = SphereAudioProcessor::STEM_MODELS
            .iter()
            .map(|info| {
                let package = info.model.package();
                let installed = SphereAudioProcessor::model_installed(info.model, &models_dir);
                let status = if installed {
                    "Downloaded"
                } else {
                    "Not downloaded"
                };
                SelectOption::new(info.id, info.label).description(format!(
                    "{} · {} · {status}",
                    info.description,
                    package.approx_size_label()
                ))
            })
            .collect::<Vec<_>>();
        let control = self.dropdown(
            SelectField::Model,
            "stem-model",
            self.params.model.as_str().to_string(),
            options,
            target,
        );
        field_row("Model", control)
    }

    /// One card that says whether the chosen model is ready and, when it is
    /// not, offers the download right there — progress, cancel and retry
    /// included.
    fn render_model_status(&self, target: gpui::Entity<Self>) -> impl IntoElement {
        let package = self.params.model.package();
        let size = package.approx_size_label();
        let running_job = matches!(self.state, StemExtractJobState::Running(_));

        let card = div()
            .flex()
            .flex_col()
            .gap(px(space::BASE))
            .px(px(space::LOOSE))
            .py(px(space::BASE + space::HAIR))
            .rounded(px(radius::CONTROL))
            .bg(Colors::surface_panel())
            .border(px(1.0))
            .border_color(Colors::border_subtle());

        if let ModelDownloadUi::Running(progress) = &self.download_ui {
            let detail = if progress.file_count > 1 {
                format!(
                    "{} ({}/{})",
                    progress.file_name,
                    progress.file_index + 1,
                    progress.file_count
                )
            } else {
                progress.file_name.clone()
            };
            return card
                .child(status_line(
                    assets::ICON_DOWNLOAD_PATH,
                    Colors::accent_primary(),
                    format!(
                        "Downloading {} · {:.0}%",
                        progress.model.label(),
                        progress.percent
                    ),
                    detail,
                    Some(
                        fb_button(
                            "stem-download-cancel",
                            "Cancel",
                            FbButtonKind::Default,
                            true,
                            {
                                let target = target.clone();
                                move |_, _window, cx| {
                                    let _ = target
                                        .update(cx, |this, cx| this.request_cancel_download(cx));
                                }
                            },
                        )
                        .into_any_element(),
                    ),
                ))
                .child(progress_bar(ProgressBarValue::value(
                    progress.percent / 100.0,
                )));
        }

        if self.model_installed {
            return card.child(status_line(
                assets::ICON_CIRCLE_CHECK_PATH,
                Colors::status_success(),
                "Ready to use".to_string(),
                format!("Downloaded · {size} · runs on {}", platform_runtime_label()),
                None,
            ));
        }

        let download_target = target.clone();
        let download = move |label: &'static str| {
            let target = download_target.clone();
            fb_button(
                "stem-model-download",
                label,
                FbButtonKind::Default,
                !running_job,
                move |_, _window, cx| {
                    let _ = target.update(cx, |this, cx| this.start_download(cx));
                },
            )
            .into_any_element()
        };

        if let ModelDownloadUi::Failed(message) = &self.download_ui {
            return card.child(status_line(
                assets::ICON_TRIANGLE_ALERT_PATH,
                Colors::status_error(),
                "Download failed".to_string(),
                message.clone(),
                Some(download("Try Again")),
            ));
        }

        card.child(status_line(
            assets::ICON_DOWNLOAD_PATH,
            Colors::text_secondary(),
            format!("Download the model · {size}"),
            "One-time download. Until then a basic spectral split is used.".to_string(),
            Some(download("Download")),
        ))
    }

    fn render_quality(&self, target: gpui::Entity<Self>) -> impl IntoElement {
        use SphereAudioProcessor::StemExtractQuality as Quality;
        let current = self.params.quality;
        let choices = [
            (Quality::Draft, "Fast", FbSegment::First),
            (Quality::Balanced, "Balanced", FbSegment::Middle),
            (Quality::High, "Best", FbSegment::Last),
        ];
        let track = fb_segmented_track()
            .flex_1()
            .children(choices.into_iter().map(|(quality, label, position)| {
                let target = target.clone();
                fb_segment(
                    SharedString::from(format!("stem-quality-{}", quality.as_str())),
                    label,
                    current == quality,
                    position,
                    move |_, _window, cx| {
                        let _ = target.update(cx, |this, cx| {
                            this.apply_select(SelectField::Quality, quality.as_str());
                            if matches!(this.state, StemExtractJobState::Failed(_)) {
                                this.state = StemExtractJobState::Editing;
                            }
                            cx.notify();
                        });
                    },
                )
            }));
        let explanation = match current {
            Quality::Draft => "Quickest. Good for a first listen.",
            Quality::Balanced => "Clean stems at a moderate speed. Recommended.",
            Quality::High => "Cleanest stems. Takes the longest.",
        };
        div()
            .flex()
            .flex_col()
            .gap(px(space::SNUG))
            .child(field_row("Quality", track))
            .child(
                div()
                    .pl(px(FIELD_LABEL_W + space::BASE))
                    .child(caption(explanation)),
            )
    }

    fn dropdown(
        &self,
        field: SelectField,
        id: &'static str,
        selected: String,
        options: Vec<SelectOption>,
        target: gpui::Entity<Self>,
    ) -> impl IntoElement {
        let open = self.open_select == Some(field);
        let toggle_target = target.clone();
        let change_target = target;
        let selected_value = selected.clone();
        select(
            id,
            Some(selected.as_str()),
            selected_value,
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
                    if matches!(this.state, StemExtractJobState::Failed(_)) {
                        this.state = StemExtractJobState::Editing;
                    }
                    cx.notify();
                });
            }),
        )
    }

    /// The action bar: what Extract will do (or why it can't yet) on the
    /// left, Cancel and Extract on the right.
    fn footer(&self, target: gpui::Entity<Self>) -> impl IntoElement {
        let blocker = self.extract_blocker();
        let count = self.params.stems.len();
        let (hint, hint_color) = match &blocker {
            Some(reason) => (reason.clone(), Colors::text_muted()),
            None => (
                format!(
                    "Adds {count} new track{}",
                    if count == 1 { "" } else { "s" }
                ),
                Colors::text_secondary(),
            ),
        };
        let extract_label = match count {
            0 => "Extract".to_string(),
            1 => "Extract 1 Stem".to_string(),
            n => format!("Extract {n} Stems"),
        };
        footer_bar()
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .text_size(px(typography::UI_XS))
                    .text_color(hint_color)
                    .truncate()
                    .child(hint),
            )
            .child(fb_button(
                "stem-cancel",
                "Cancel",
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
                "stem-extract",
                extract_label,
                FbButtonKind::Primary,
                blocker.is_none(),
                {
                    let target = target.clone();
                    move |_, _window, cx| {
                        let _ = target.update(cx, |this, cx| this.start_extract(cx));
                    }
                },
            ))
    }

    fn render_progress(
        &self,
        progress: SphereAudioProcessor::StemExtractProgress,
        target: gpui::Entity<Self>,
    ) -> gpui::AnyElement {
        use SphereAudioProcessor::StemExtractStage as Stage;
        let stages = [
            (Stage::Preparing, "Read audio"),
            (Stage::LoadingModel, "Load model"),
            (Stage::Separating, "Separate"),
            (Stage::Writing, "Write tracks"),
        ];
        let current = stages
            .iter()
            .position(|(stage, _)| *stage == progress.stage)
            .unwrap_or(stages.len());
        let detail = if let Some(stem) = progress.current_stem {
            format!("{} · {}", progress.detail, stem.label())
        } else {
            progress.detail.clone()
        };
        let clip = self
            .selected_clip()
            .map(StemSourceClip::label)
            .unwrap_or_default();

        let body = div()
            .flex()
            .flex_col()
            .flex_1()
            .px(px(BODY_PAD))
            .py(px(BODY_PAD + space::BASE))
            .gap(px(space::SECTION))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(space::HAIR))
                    .child(
                        div()
                            .text_size(px(typography::UI_LG))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(Colors::text_primary())
                            .child("Extracting stems…"),
                    )
                    .child(caption(clip)),
            )
            .child(
                div().flex().flex_row().gap(px(space::BASE)).children(
                    stages
                        .iter()
                        .enumerate()
                        .map(|(index, (_, label))| stage_chip(label, index.cmp(&current))),
                ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(space::SNUG))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(space::BASE))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .text_size(px(typography::UI_XS))
                                    .text_color(Colors::text_secondary())
                                    .truncate()
                                    .child(detail),
                            )
                            .child(
                                div()
                                    .text_size(px(typography::UI_XS))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(Colors::accent_primary())
                                    .child(format!("{:.0}%", progress.percent)),
                            ),
                    )
                    .child(progress_bar(ProgressBarValue::value(
                        progress.percent / 100.0,
                    ))),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap(px(space::BASE))
                    .children(self.params.stems.iter().map(stem_badge)),
            )
            .child(caption(format!(
                "{} · {} · {} quality",
                self.params.model.label(),
                self.params.device.label(),
                self.params.quality.label()
            )));

        div()
            .flex()
            .flex_col()
            .flex_1()
            .child(body)
            .child(footer_bar().child(div().flex_1()).child(fb_button(
                "stem-cancel-run",
                "Cancel",
                FbButtonKind::Default,
                true,
                {
                    let target = target.clone();
                    move |_, _window, cx| {
                        let _ = target.update(cx, |this, cx| this.request_cancel(cx));
                    }
                },
            )))
            .into_any_element()
    }

    fn render_complete(
        &self,
        summary: StemExtractJobSummary,
        target: gpui::Entity<Self>,
    ) -> gpui::AnyElement {
        let count = summary.apply.stems.len();
        let stub = summary.backend == SphereAudioProcessor::InferBackendKind::SpectralStub;
        let mut body = result_header(
            assets::ICON_CIRCLE_CHECK_PATH,
            Colors::status_success(),
            "Your stems are ready",
            format!(
                "{count} new track{} added from {}. The original track is muted.",
                if count == 1 { " was" } else { "s were" },
                summary.apply.source_clip_name
            ),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap(px(space::BASE))
                .children(summary.apply.stems.iter().map(|stem| stem_badge(stem.kind))),
        );
        if stub {
            body = body.child(notice(
                assets::ICON_TRIANGLE_ALERT_PATH,
                Colors::status_warning(),
                "Made with the basic spectral split",
                format!(
                    "The {} model wasn't available. Download it for cleaner stems.",
                    summary.model.label()
                ),
            ));
        } else {
            body = body.child(caption(format!(
                "{} · {}",
                summary.backend.label(),
                summary.device.label()
            )));
        }

        div()
            .flex()
            .flex_col()
            .flex_1()
            .child(body)
            .child(footer_bar().child(div().flex_1()).child(fb_button(
                "stem-close",
                "Done",
                FbButtonKind::Primary,
                true,
                {
                    let target = target.clone();
                    move |_, window, cx| {
                        let _ = target.update(cx, |this, cx| this.close(window, cx));
                    }
                },
            )))
            .into_any_element()
    }

    fn render_cancelled(&self, target: gpui::Entity<Self>) -> gpui::AnyElement {
        div()
            .flex()
            .flex_col()
            .flex_1()
            .child(result_header(
                assets::ICON_CLOSE_SMALL_PATH,
                Colors::text_muted(),
                "Extraction cancelled",
                "Nothing was added to the project.".to_string(),
            ))
            .child(
                footer_bar()
                    .child(div().flex_1())
                    .child(fb_button(
                        "stem-start-over",
                        "Start Over",
                        FbButtonKind::Default,
                        true,
                        {
                            let target = target.clone();
                            move |_, _window, cx| {
                                let _ = target.update(cx, |this, cx| this.back_to_editing(cx));
                            }
                        },
                    ))
                    .child(fb_button(
                        "stem-close-term",
                        "Close",
                        FbButtonKind::Primary,
                        true,
                        {
                            let target = target.clone();
                            move |_, window, cx| {
                                let _ = target.update(cx, |this, cx| this.close(window, cx));
                            }
                        },
                    )),
            )
            .into_any_element()
    }
}

fn platform_runtime_label() -> String {
    SphereAudioProcessor::resolve_current_platform_runtime()
        .map(|runtime| runtime.label().to_string())
        .unwrap_or_else(|error| error.user_message())
}

fn clip_file_name(clip: &StemSourceClip) -> String {
    clip.source_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| clip.source_path.display().to_string())
}

/// `m:ss`, or `h:mm:ss` past an hour.
fn format_duration(seconds: f64) -> String {
    let total = seconds.max(0.0).round() as u64;
    let (h, m, s) = (total / 3600, (total / 60) % 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

fn stem_icon(kind: SphereAudioProcessor::StemKind) -> &'static str {
    use SphereAudioProcessor::StemKind as Kind;
    match kind {
        Kind::Vocals => assets::ICON_MIC_PATH,
        Kind::Drums => assets::ICON_DRUM_PATH,
        Kind::Bass => assets::ICON_GUITAR_PATH,
        Kind::Other => assets::ICON_KEYBOARD_PATH,
        Kind::Instrumental => assets::ICON_AUDIO_LINES_PATH,
    }
}

fn caption(text: impl Into<SharedString>) -> impl IntoElement {
    div()
        .text_size(px(typography::UI_XS))
        .text_color(Colors::text_muted())
        .child(text.into())
}

/// A numbered step: a small number disc and title, an optional note on the
/// right, and the step's controls indented under the title.
fn step(
    number: u8,
    title: &'static str,
    aside: Option<gpui::AnyElement>,
    content: gpui::AnyElement,
) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(space::BASE))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::BASE))
                .child(
                    div()
                        .size(px(STEP_DISC))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(radius::PILL))
                        .bg(Colors::surface_raised())
                        .text_size(px(typography::DENSE_LABEL))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(Colors::text_secondary())
                        .child(number.to_string()),
                )
                .child(
                    div()
                        .flex_1()
                        .text_size(px(typography::UI_MD))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(Colors::text_primary())
                        .child(title),
                )
                .children(aside),
        )
        .child(div().pl(px(STEP_DISC + space::BASE)).child(content))
}

fn field_row(label: &'static str, control: impl IntoElement) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::BASE))
        .child(
            div()
                .w(px(FIELD_LABEL_W))
                .flex_none()
                .text_size(px(typography::UI_XS))
                .text_color(Colors::text_secondary())
                .child(label),
        )
        .child(
            div()
                .flex()
                .flex_1()
                .min_w(px(0.0))
                .child(control.into_any_element()),
        )
}

/// A stem as a toggle tile: its icon and name, filled and outlined in the
/// accent with a check when it will be extracted.
fn stem_tile(
    stem: SphereAudioProcessor::StemKind,
    checked: bool,
    on_toggle: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let rest = if checked {
        Colors::composite(Colors::surface_panel(), Colors::accent_muted())
    } else {
        Colors::surface_panel()
    };
    let hover = Colors::composite(rest, Colors::state_hover());
    let pressed = Colors::composite(rest, Colors::state_recessed());
    let focus = Colors::state_focus_ring();
    let label = stem.label();
    div()
        .id(SharedString::from(format!("stem-toggle-{}", stem.as_str())))
        .role(Role::CheckBox)
        .aria_label(label)
        .aria_toggled(if checked {
            Toggled::True
        } else {
            Toggled::False
        })
        .focusable()
        .tab_stop(true)
        .focus_visible(move |style| style.shadow(elevation::focus_ring(focus)))
        .relative()
        .flex_1()
        .min_w(px(0.0))
        .h(px(STEM_TILE_H))
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(space::SNUG))
        .rounded(px(radius::CONTROL))
        .border(px(1.0))
        .border_color(if checked {
            Colors::with_alpha(Colors::accent_primary(), state::ARMED_BORDER)
        } else {
            Colors::border_subtle()
        })
        .bg(rest)
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(move |s| s.bg(hover))
        .active(move |s| s.bg(pressed))
        .on_click(on_toggle)
        .child(
            svg()
                .path(stem_icon(stem))
                .size(px(20.0))
                .text_color(if checked {
                    Colors::accent_primary()
                } else {
                    Colors::text_muted()
                }),
        )
        .child(
            div()
                .max_w_full()
                .px(px(space::TIGHT))
                .truncate()
                .text_size(px(typography::UI_SM))
                .font_weight(if checked {
                    gpui::FontWeight::SEMIBOLD
                } else {
                    gpui::FontWeight::NORMAL
                })
                .text_color(if checked {
                    Colors::text_primary()
                } else {
                    Colors::text_secondary()
                })
                .child(label),
        )
        .child(
            div()
                .absolute()
                .top(px(space::SNUG))
                .right(px(space::SNUG))
                .size(px(14.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(radius::PILL))
                .border(px(1.0))
                .border_color(if checked {
                    Colors::accent_primary()
                } else {
                    Colors::border_strong()
                })
                .bg(if checked {
                    Colors::accent_primary()
                } else {
                    Colors::surface_input()
                })
                .when(checked, |this| {
                    this.child(
                        svg()
                            .path(assets::ICON_CHECK_PATH)
                            .size(px(10.0))
                            .text_color(Colors::on_accent()),
                    )
                }),
        )
}

/// A finished stem, as listed on the result screen.
fn stem_badge(kind: SphereAudioProcessor::StemKind) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::SNUG))
        .h(px(theme::size::DEFAULT + space::TIGHT))
        .px(px(space::LOOSE))
        .rounded(px(radius::PILL))
        .bg(Colors::surface_panel())
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .child(
            svg()
                .path(stem_icon(kind))
                .size(px(14.0))
                .text_color(Colors::accent_primary()),
        )
        .child(
            div()
                .text_size(px(typography::UI_SM))
                .text_color(Colors::text_primary())
                .child(kind.label()),
        )
}

/// Icon, title and detail, with an optional action on the right.
fn status_line(
    icon: &'static str,
    tone: gpui::Rgba,
    title: String,
    detail: String,
    action: Option<gpui::AnyElement>,
) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::BASE + space::HAIR))
        .child(svg().path(icon).size(px(16.0)).flex_none().text_color(tone))
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(0.0))
                .gap(px(space::HAIR))
                .child(
                    div()
                        .text_size(px(typography::UI_SM))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(Colors::text_primary())
                        .truncate()
                        .child(title),
                )
                .child(
                    div()
                        .text_size(px(typography::UI_XS))
                        .text_color(Colors::text_muted())
                        .child(detail),
                ),
        )
        .children(action)
}

/// A tinted message with an icon — an error or a warning the user should read.
fn notice(icon: &'static str, tone: gpui::Rgba, title: &'static str, detail: String) -> gpui::Div {
    div()
        .px(px(space::LOOSE))
        .py(px(space::BASE + space::HAIR))
        .rounded(px(radius::CONTROL))
        .bg(Colors::composite(
            Colors::surface_panel(),
            Colors::with_alpha(tone, 0.10),
        ))
        .border(px(1.0))
        .border_color(Colors::with_alpha(tone, 0.35))
        .child(status_line(icon, tone, title.to_string(), detail, None))
}

/// The large icon and message that open the done and cancelled screens.
fn result_header(
    icon: &'static str,
    tone: gpui::Rgba,
    title: &'static str,
    detail: String,
) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .flex_1()
        .px(px(BODY_PAD))
        .py(px(BODY_PAD + space::BASE))
        .gap(px(space::SECTION))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::LOOSE))
                .child(
                    div()
                        .size(px(40.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(radius::PILL))
                        .bg(Colors::with_alpha(tone, 0.14))
                        .child(svg().path(icon).size(px(20.0)).text_color(tone)),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w(px(0.0))
                        .gap(px(space::HAIR))
                        .child(
                            div()
                                .text_size(px(typography::UI_LG))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(Colors::text_primary())
                                .child(title),
                        )
                        .child(
                            div()
                                .text_size(px(typography::UI_SM))
                                .text_color(Colors::text_secondary())
                                .child(detail),
                        ),
                ),
        )
}

/// A progress step: done ones carry a check, the current one is lit.
fn stage_chip(label: &'static str, order: std::cmp::Ordering) -> impl IntoElement {
    use std::cmp::Ordering;
    let (bg, border, text) = match order {
        Ordering::Less => (
            Colors::surface_panel(),
            Colors::border_subtle(),
            Colors::text_secondary(),
        ),
        Ordering::Equal => (
            Colors::composite(Colors::surface_panel(), Colors::accent_muted()),
            Colors::with_alpha(Colors::accent_primary(), state::ARMED_BORDER),
            Colors::text_primary(),
        ),
        Ordering::Greater => (
            Colors::surface_base(),
            Colors::border_subtle(),
            Colors::text_faint(),
        ),
    };
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_center()
        .gap(px(space::TIGHT))
        .flex_1()
        .min_w(px(0.0))
        .h(px(theme::size::DEFAULT + space::TIGHT))
        .px(px(space::SNUG))
        .rounded(px(radius::PILL))
        .bg(bg)
        .border(px(1.0))
        .border_color(border)
        .when(order == Ordering::Less, |this| {
            this.child(
                svg()
                    .path(assets::ICON_CHECK_PATH)
                    .size(px(12.0))
                    .flex_none()
                    .text_color(Colors::status_success()),
            )
        })
        .child(
            div()
                .min_w(px(0.0))
                .truncate()
                .text_size(px(typography::UI_XS))
                .font_weight(if order == Ordering::Equal {
                    gpui::FontWeight::SEMIBOLD
                } else {
                    gpui::FontWeight::NORMAL
                })
                .text_color(text)
                .child(label),
        )
}

fn footer_bar() -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .flex_none()
        .items_center()
        .gap(px(space::BASE))
        .px(px(BODY_PAD))
        .py(px(space::LOOSE))
        .border_t(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_panel_alt())
}

fn write_stem_wav(
    path: &std::path::Path,
    stem: &SphereAudioProcessor::StemExtractOutput,
) -> Result<(), String> {
    let channels = stem.channels.max(1) as u16;
    let spec = AudioEncodeSpec {
        sample_rate: stem.sample_rate,
        channels,
        sample_format: AudioSampleFormat::F32,
    };
    let options = AudioEncodeOptions {
        format: AudioFileFormat::Wav,
        ..AudioEncodeOptions::default()
    };
    let mut encoder = create_encoder(path, spec, options).map_err(|e| e.to_string())?;
    encoder
        .write_interleaved_f32(&stem.samples)
        .map_err(|e| e.to_string())?;
    encoder.finalize().map_err(|e| e.to_string())?;
    Ok(())
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
        "stem".to_string()
    } else {
        sanitized
    }
}

fn resolve_render_dir(project_root: Option<&std::path::Path>) -> PathBuf {
    if let Some(root) = project_root {
        root.join("Rendered").join("Stems")
    } else {
        std::env::temp_dir().join("futureboard-stems")
    }
}

fn unique_stem_path(dir: &std::path::Path, source_stem: &str, suffix: &str) -> PathBuf {
    let base = format!("{}_{}", sanitize_file_stem(source_stem), suffix);
    let candidate = dir.join(format!("{base}.wav"));
    if !candidate.exists() {
        return candidate;
    }
    for index in 2..10_000 {
        let path = dir.join(format!("{base}_{index}.wav"));
        if !path.exists() {
            return path;
        }
    }
    dir.join(format!(
        "{base}_{}.wav",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ))
}

/// Open the Stem Extractor dialog centered over `owner_bounds`.
pub fn open_stem_extractor_window(
    owner_bounds: Option<Bounds<gpui::Pixels>>,
    defaults: StemExtractorDialogDefaults,
    on_apply: Arc<dyn Fn(StemExtractApplyRequest, &mut App) + 'static>,
    cx: &mut App,
) -> Result<WindowHandle<StemExtractorWindow>, String> {
    let height = TITLEBAR_HEIGHT + STEM_EXTRACTOR_WINDOW_HEIGHT;
    let window_bounds = centered_window_bounds(
        owner_bounds,
        size(px(STEM_EXTRACTOR_WINDOW_WIDTH), px(height)),
        cx,
    );

    let mut options = crate::platform_chrome::external_dialog_window_options_partial();
    options.window_bounds = Some(WindowBounds::Windowed(window_bounds));
    options.kind = WindowKind::Dialog;
    options.is_resizable = false;
    options.is_minimizable = false;
    options.window_background = WindowBackgroundAppearance::Transparent;
    apply_owner_display(&mut options, owner_bounds, cx);

    crate::dialog_frame::open_dialog_window(cx, options, move |_window, cx| {
        cx.new(|cx| StemExtractorWindow::new(defaults, on_apply, cx))
    })
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_illegal_path_chars() {
        assert_eq!(sanitize_file_stem("Lead/Vox:A"), "Lead_Vox_A");
        assert_eq!(sanitize_file_stem("   "), "stem");
    }
}

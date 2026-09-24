//! Native Stem Extractor dialog.
//!
//! Compact Futureboard dialog for offline MDX-NET stem separation.
//! Owns serializable [`StemExtractParams`] from SphereAudioProcessor, edits them
//! in the UI, and runs a cancellable background job that never leases GPUI
//! entities during decode/separate/encode work.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use gpui::{
    div, px, size, App, AppContext, Bounds, Context, FocusHandle, InteractiveElement, IntoElement,
    KeyDownEvent, ParentElement, Render, SharedString, Styled, Window, WindowBackgroundAppearance,
    WindowBounds, WindowHandle, WindowKind,
};
use sphere_encoder::{
    create_encoder, AudioEncodeOptions, AudioEncodeSpec, AudioFileFormat, AudioSampleFormat,
};

use crate::components::controls::{fb_button, fb_checkbox, FbButtonKind};
use crate::components::form::select::{select, SelectOption};
use crate::components::progress_dialog::{progress_bar, ProgressBarValue};
use crate::components::title_bar::{external_window_titlebar_compact, TITLEBAR_HEIGHT};
use crate::theme::{self, Colors};
use crate::window_position::{apply_owner_display, centered_window_bounds};

pub const STEM_EXTRACTOR_WINDOW_WIDTH: f32 = 520.0;
const STEM_EXTRACTOR_WINDOW_HEIGHT: f32 = 560.0;
const BODY_PAD: f32 = 14.0;
const ROW_GAP: f32 = 9.0;
const LABEL_W: f32 = 96.0;

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
                if matches!(self.state, StemExtractJobState::Editing) && !self.is_downloading() =>
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

    fn can_extract(&self) -> bool {
        !self.is_downloading()
            && self.selected_clip().is_some()
            && self.source_path.is_some()
            && self.params.validate().is_ok()
            && !self.params.stems.is_empty()
            && SphereAudioProcessor::resolve_current_platform_runtime().is_ok()
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
        std::thread::Builder::new()
            .name("fb-stem-model-dl".to_string())
            .spawn(move || {
                let progress_shared = worker_shared.clone();
                let mut on_progress =
                    move |progress: SphereAudioProcessor::StemModelDownloadProgress| {
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
        std::thread::Builder::new()
            .name("fb-stem-extract".to_string())
            .spawn(move || {
                let progress_shared = worker_shared.clone();
                let mut on_progress = move |progress: SphereAudioProcessor::StemExtractProgress| {
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
            StemExtractJobState::Cancelled => self.render_terminal(
                "Extraction cancelled.",
                Colors::text_secondary(),
                target.clone(),
            ),
        };

        div()
            .flex()
            .flex_col()
            .size_full()
            .font(theme::ui_font())
            .bg(Colors::surface_base())
            .overflow_hidden()
            .rounded(px(crate::theme::radius::CONTROL))
            .border(px(1.0))
            .border_color(Colors::border_subtle())
            .shadow(vec![gpui::BoxShadow {
                color: Colors::surface_overlay().into(),
                offset: gpui::point(px(0.0), px(6.0)),
                blur_radius: px(20.0),
                spread_radius: px(0.0),
                inset: false,
            }])
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
            .child(
                div()
                    .px(px(BODY_PAD))
                    .pt(px(6.0))
                    .text_size(px(11.0))
                    .text_color(Colors::text_muted())
                    .truncate()
                    .child(self.subtitle()),
            )
            .child(body)
    }
}

impl StemExtractorWindow {
    fn subtitle(&self) -> String {
        if let Some(clip) = self.selected_clip() {
            format!("Source clip · {}", clip.label())
        } else if self.defaults.audio_clips.is_empty() {
            "MDX-NET · select an audio clip on a track".to_string()
        } else {
            format!("MDX-NET stem separation · {}", platform_runtime_label())
        }
    }

    fn render_editing(&self, target: gpui::Entity<Self>) -> gpui::AnyElement {
        let mut col = div()
            .flex()
            .flex_col()
            .flex_1()
            .px(px(BODY_PAD))
            .py(px(BODY_PAD))
            .gap(px(ROW_GAP))
            .child(section_label("SOURCE"))
            .child(self.clip_row(target.clone()))
            .child(
                div()
                    .text_size(px(10.0))
                    .text_color(Colors::text_faint())
                    .child(
                        "Stems render to new tracks. The original source track is muted."
                            .to_string(),
                    ),
            )
            .child(section_label("MODEL"))
            .child(self.model_row(target.clone()))
            .child(self.model_status_row(target.clone()))
            .child(self.runtime_row())
            .child(self.quality_row(target.clone()))
            .child(section_label("STEMS"))
            .child(self.stems_row(target.clone()))
            .child(
                div()
                    .text_size(px(10.0))
                    .text_color(Colors::text_faint())
                    .child(model_hint(
                        &self.params,
                        self.model_installed,
                        &self.models_dir,
                    )),
            )
            .child(div().flex_1());

        if let ModelDownloadUi::Failed(message) = &self.download_ui {
            col = col.child(error_banner(message.clone()));
        } else if let ModelDownloadUi::Running(progress) = &self.download_ui {
            col = col.child(self.render_download_progress(progress.clone(), target.clone()));
        }

        if let StemExtractJobState::Failed(message) = &self.state {
            col = col.child(error_banner(message.clone()));
        } else if self.defaults.audio_clips.is_empty() {
            col = col.child(hint_banner(
                "No audio clips on tracks. Import or record audio first.".into(),
            ));
        } else if let Err(error) = SphereAudioProcessor::resolve_current_platform_runtime() {
            col = col.child(hint_banner(error.user_message()));
        } else if !self.can_extract() && !self.is_downloading() {
            col = col.child(hint_banner(
                "Select an audio clip and at least one stem.".into(),
            ));
        }

        col = col.child(self.footer(self.can_extract(), target));
        col.into_any_element()
    }

    fn clip_row(&self, target: gpui::Entity<Self>) -> impl IntoElement {
        let empty = self.defaults.audio_clips.is_empty();
        let options = if empty {
            vec![SelectOption::new("__none__", "No audio clips on tracks").disabled(true)]
        } else {
            self.defaults
                .audio_clips
                .iter()
                .map(|clip| {
                    let file_name = clip
                        .source_path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| clip.source_path.display().to_string());
                    SelectOption::new(clip.clip_id.clone(), clip.label()).description(file_name)
                })
                .collect()
        };
        let selected = self.selected_clip_id.clone().unwrap_or_else(|| {
            if empty {
                "__none__".to_string()
            } else {
                String::new()
            }
        });
        let control = self.dropdown(SelectField::Clip, "stem-clip", selected, options, target);
        self.labeled("Clip", control)
    }

    fn model_row(&self, target: gpui::Entity<Self>) -> impl IntoElement {
        let models_dir = self.models_dir.clone();
        let options = SphereAudioProcessor::STEM_MODELS
            .iter()
            .map(|info| {
                let package = info.model.package();
                let installed = SphereAudioProcessor::model_installed(info.model, &models_dir);
                let status = if installed {
                    "Installed"
                } else {
                    "Not installed"
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
        self.labeled("Model", control)
    }

    fn model_status_row(&self, target: gpui::Entity<Self>) -> impl IntoElement {
        let package = self.params.model.package();
        let downloading = self.is_downloading();
        let status = if let ModelDownloadUi::Running(progress) = &self.download_ui {
            format!(
                "Downloading… {:.0}% · {}",
                progress.percent, progress.file_name
            )
        } else if self.model_installed {
            format!(
                "Installed · {} file(s) · {}",
                package.file_count(),
                package.approx_size_label()
            )
        } else {
            format!(
                "Not installed · {} · {}",
                package.source_label,
                package.approx_size_label()
            )
        };
        let can_download = !self.model_installed
            && !downloading
            && !matches!(self.state, StemExtractJobState::Running(_));
        let control = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .text_size(px(11.0))
                    .text_color(if downloading {
                        Colors::accent_primary()
                    } else if self.model_installed {
                        Colors::text_secondary()
                    } else {
                        Colors::text_muted()
                    })
                    .truncate()
                    .child(status),
            )
            .child(fb_button(
                "stem-model-download",
                if downloading {
                    "Downloading…"
                } else {
                    "Download"
                },
                FbButtonKind::Default,
                can_download,
                {
                    let target = target.clone();
                    move |_, _window, cx| {
                        let _ = target.update(cx, |this, cx| this.start_download(cx));
                    }
                },
            ));
        self.labeled("Weights", control)
    }

    fn render_download_progress(
        &self,
        progress: SphereAudioProcessor::StemModelDownloadProgress,
        target: gpui::Entity<Self>,
    ) -> impl IntoElement {
        let percent = format!("{:.0}%", progress.percent);
        let detail = if progress.file_count > 1 {
            format!(
                "{} ({}/{}) · {}",
                progress.detail,
                progress.file_index + 1,
                progress.file_count,
                progress.file_name
            )
        } else {
            format!("{} · {}", progress.detail, progress.file_name)
        };
        div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .px(px(10.0))
            .py(px(8.0))
            .rounded(px(crate::theme::radius::CONTROL))
            .bg(Colors::surface_panel_alt())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(11.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(Colors::text_primary())
                            .child(format!("Downloading {}", progress.model.label())),
                    )
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(Colors::accent_primary())
                            .child(percent),
                    ),
            )
            .child(progress_bar(ProgressBarValue::value(
                progress.percent / 100.0,
            )))
            .child(
                div()
                    .text_size(px(10.0))
                    .text_color(Colors::text_muted())
                    .child(detail),
            )
            .child(div().flex().flex_row().justify_end().child(fb_button(
                "stem-download-cancel",
                "Cancel Download",
                FbButtonKind::Default,
                true,
                {
                    let target = target.clone();
                    move |_, _window, cx| {
                        let _ = target.update(cx, |this, cx| this.request_cancel_download(cx));
                    }
                },
            )))
    }

    fn runtime_row(&self) -> impl IntoElement {
        self.labeled(
            "Runtime",
            div()
                .text_size(px(11.0))
                .text_color(Colors::text_secondary())
                .child(platform_runtime_label()),
        )
    }

    fn quality_row(&self, target: gpui::Entity<Self>) -> impl IntoElement {
        let options = vec![
            SelectOption::new("draft", "Draft"),
            SelectOption::new("balanced", "Balanced"),
            SelectOption::new("high", "High"),
        ];
        let control = self.dropdown(
            SelectField::Quality,
            "stem-quality",
            self.params.quality.as_str().to_string(),
            options,
            target,
        );
        self.labeled("Quality", control)
    }

    fn stems_row(&self, target: gpui::Entity<Self>) -> impl IntoElement {
        let stems = self.params.model.default_stems();
        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap(px(10.0))
            .children(stems.iter().copied().map(|stem| {
                let checked = self.params.stems.contains(stem);
                let target = target.clone();
                fb_checkbox(
                    format!("stem-toggle-{}", stem.as_str()),
                    stem.label(),
                    checked,
                    true,
                    move |_, _window, cx| {
                        let _ = target.update(cx, |this, cx| {
                            this.params
                                .set_stem(stem, !this.params.stems.contains(stem));
                            if matches!(this.state, StemExtractJobState::Failed(_)) {
                                this.state = StemExtractJobState::Editing;
                            }
                            cx.notify();
                        });
                    },
                )
            }))
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

    fn labeled<E: IntoElement>(&self, label: &str, control: E) -> gpui::Stateful<gpui::Div> {
        div()
            .id(SharedString::from(format!("stem-row-{label}")))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .child(
                div()
                    .w(px(LABEL_W))
                    .flex_none()
                    .text_size(px(11.0))
                    .text_color(Colors::text_secondary())
                    .child(label.to_string()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .child(control.into_any_element()),
            )
    }

    fn footer(&self, can_extract: bool, target: gpui::Entity<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_end()
            .gap(px(8.0))
            .pt(px(8.0))
            .border_t(px(1.0))
            .border_color(Colors::border_subtle())
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
                "Extract",
                FbButtonKind::Primary,
                can_extract,
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
        let percent = format!("{:.0}%", progress.percent);
        let detail = if let Some(stem) = progress.current_stem {
            format!("{} · {}", progress.detail, stem.label())
        } else {
            progress.detail.clone()
        };
        div()
            .flex()
            .flex_col()
            .flex_1()
            .px(px(BODY_PAD))
            .py(px(BODY_PAD))
            .gap(px(10.0))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(12.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(Colors::text_primary())
                            .child(progress.stage.as_str().to_string()),
                    )
                    .child(
                        div()
                            .text_size(px(11.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(Colors::accent_primary())
                            .child(percent),
                    ),
            )
            .child(progress_bar(ProgressBarValue::value(
                progress.percent / 100.0,
            )))
            .child(
                div()
                    .text_size(px(10.0))
                    .text_color(Colors::text_muted())
                    .child(detail),
            )
            .child(
                div()
                    .text_size(px(10.0))
                    .text_color(Colors::text_faint())
                    .child(format!(
                        "{} · {}",
                        self.params.model.label(),
                        self.params.device.label()
                    )),
            )
            .child(div().flex_1())
            .child(div().flex().flex_row().justify_end().child(fb_button(
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
        let stem_names = summary
            .apply
            .stems
            .iter()
            .map(|stem| stem.kind.label())
            .collect::<Vec<_>>()
            .join(", ");
        div()
            .flex()
            .flex_col()
            .flex_1()
            .px(px(BODY_PAD))
            .py(px(BODY_PAD))
            .gap(px(10.0))
            .child(
                div()
                    .text_size(px(12.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(Colors::accent_primary())
                    .child("Extraction complete"),
            )
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(Colors::text_primary())
                    .child(format!(
                        "{count} stem track(s) created · {} · {}",
                        summary.model.label(),
                        summary.device.label()
                    )),
            )
            .child(
                div()
                    .text_size(px(10.0))
                    .text_color(Colors::text_muted())
                    .child(if stem_names.is_empty() {
                        "Original source track muted.".to_string()
                    } else {
                        format!("{stem_names} · original source track muted")
                    }),
            )
            .child(
                div()
                    .text_size(px(10.0))
                    .text_color(Colors::text_faint())
                    .child(format!("Backend · {}", summary.backend.label())),
            )
            .child(div().flex_1())
            .child(div().flex().flex_row().justify_end().child(fb_button(
                "stem-close",
                "Close",
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

    fn render_terminal(
        &self,
        message: &str,
        color: gpui::Rgba,
        target: gpui::Entity<Self>,
    ) -> gpui::AnyElement {
        div()
            .flex()
            .flex_col()
            .flex_1()
            .px(px(BODY_PAD))
            .py(px(BODY_PAD))
            .gap(px(10.0))
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(color)
                    .child(message.to_string()),
            )
            .child(div().flex_1())
            .child(div().flex().flex_row().justify_end().child(fb_button(
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
            )))
            .into_any_element()
    }
}

fn model_hint(
    params: &SphereAudioProcessor::StemExtractParams,
    installed: bool,
    models_dir: &std::path::Path,
) -> String {
    let runtime = platform_runtime_label();
    let weights = if installed {
        format!("weights in {}", models_dir.display())
    } else {
        "weights not installed — Download saves to Utilities/Models".to_string()
    };
    format!(
        "{} · {runtime} · {} · {weights}",
        params.model.description(),
        params.quality.label()
    )
}

fn platform_runtime_label() -> String {
    SphereAudioProcessor::resolve_current_platform_runtime()
        .map(|runtime| runtime.label().to_string())
        .unwrap_or_else(|error| error.user_message())
}

fn section_label(label: &'static str) -> impl IntoElement {
    div()
        .mt(px(2.0))
        .pb(px(2.0))
        .border_b(px(1.0))
        .border_color(Colors::border_subtle())
        .text_size(px(9.0))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(Colors::text_faint())
        .child(label)
}

fn error_banner(message: String) -> impl IntoElement {
    banner(message, Colors::status_error())
}

fn hint_banner(message: String) -> impl IntoElement {
    banner(message, Colors::text_muted())
}

fn banner(message: String, color: gpui::Rgba) -> impl IntoElement {
    div()
        .px(px(10.0))
        .py(px(6.0))
        .rounded(px(crate::theme::radius::CONTROL))
        .bg(Colors::surface_panel_alt())
        .text_size(px(10.0))
        .text_color(color)
        .child(message)
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

    cx.open_window(options, move |_window, cx| {
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

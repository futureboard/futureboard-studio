//! The Render dialog: runs a job the Export dialog put together, shows its
//! progress, and reports every file it wrote — its levels, and plainly
//! whether it went over 0 dBFS.
//!
//! Offline, the job renders on a worker thread against a plain snapshot, the
//! live plug-in bridges handed to it for the duration. Realtime, the studio
//! lends its transport ([`RealtimeTransportHooks`]): the project plays from
//! the start of the range through the live engine, the engine's render
//! capture records it, and a writer thread encodes what arrives. Either way
//! the worker never touches GPUI; this window polls what it shares.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use gpui::{
    div, px, App, AppContext, Bounds, Context, Entity, FocusHandle, FontWeight, InteractiveElement,
    IntoElement, KeyDownEvent, ParentElement, Render, Styled, Window, WindowHandle,
};

use DirectAudio::plugin_bridge::PluginBridgeSinkMap;
use DirectAudio::types::EngineProjectSnapshot;
use DirectAudio::{
    export_render_job_with_bridges, record_render_job, ArrangementExportSummary, ExportCancelToken,
    ExportProgress, ExportStage, RenderJob,
};

use crate::assets;
use crate::components::controls::{fb_badge, fb_button, FbButtonKind};
use crate::components::progress_dialog::{progress_bar, ProgressBarValue};
use crate::components::title_bar::{external_window_titlebar_with_icon, TITLEBAR_HEIGHT};
use crate::i18n::I18n;
use crate::theme::{self, elevation, radius, size, space, typography, Colors};

use super::ui_kit::*;

const RENDER_WINDOW_WIDTH: f32 = 560.0;
const RENDER_WINDOW_HEIGHT: f32 = 540.0;
const RENDER_WINDOW_MIN_WIDTH: f32 = 440.0;
const RENDER_WINDOW_MIN_HEIGHT: f32 = 360.0;
/// Planned files listed while a render runs before the list folds into a
/// count.
const RUNNING_FILE_LIST_MAX: usize = 8;
/// Seconds of unread audio the realtime capture can hold before it drops.
const REALTIME_CAPTURE_SECONDS: usize = 2;

/// How a realtime render borrows the studio's transport.
#[derive(Clone)]
pub struct RealtimeTransportHooks {
    /// Stop whatever plays, turn the loop and click off, seek to
    /// `start_seconds`, run `arm` (which starts the capture recording), then
    /// play. `Err` says why it could not; nothing is left changed then.
    #[allow(clippy::type_complexity)]
    pub start: Arc<dyn Fn(f64, Box<dyn FnOnce()>, &mut App) -> Result<(), String>>,
    /// Stop playback and give the transport back as it was.
    pub finish: Arc<dyn Fn(&mut App)>,
}

/// Everything a render needs, handed over by the Export dialog.
pub struct RenderLaunch {
    pub job: RenderJob,
    pub snapshot: EngineProjectSnapshot,
    pub bridge_sinks: PluginBridgeSinkMap,
    pub audio_engine: Option<DirectAudio::AudioEngine>,
    /// `Some` for a realtime render.
    pub realtime: Option<RealtimeTransportHooks>,
}

enum RenderState {
    Starting,
    Running(ExportProgress),
    Complete(Vec<ArrangementExportSummary>),
    Failed(String),
    Cancelled,
}

#[derive(Default)]
struct RenderShared {
    progress: Option<ExportProgress>,
    done: Option<Result<Vec<ArrangementExportSummary>, String>>,
}

/// Detaches the live realtime plugin-bridge sinks for the duration of an
/// offline render and guarantees they are re-installed when the guard leaves
/// scope — success, error, or worker panic (Drop runs during unwind). Losing
/// the restore would leave every bridged insert silent in realtime playback
/// after the render.
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

pub struct RenderDialog {
    language: String,
    launch: Option<RenderLaunch>,
    realtime: bool,
    /// What the job writes, in report order, for the running view.
    planned: Vec<PathBuf>,
    total_frames: u64,
    sample_rate: u32,
    state: RenderState,
    shared: Arc<Mutex<RenderShared>>,
    cancel: ExportCancelToken,
    /// Set while the studio's transport is lent to a realtime render, so it
    /// is given back exactly once.
    transport_lent: Option<(RealtimeTransportHooks, DirectAudio::AudioEngine)>,
    focus_handle: FocusHandle,
}

impl RenderDialog {
    fn new(language: String, launch: RenderLaunch, cx: &mut Context<Self>) -> Self {
        let render = launch
            .job
            .mixdown
            .as_ref()
            .map(|mixdown| &mixdown.render)
            .or_else(|| launch.job.tracks.first().map(|track| &track.request.render));
        let total_frames = render
            .map(|render| {
                render
                    .content_frames()
                    .saturating_add(render.max_tail_frames())
            })
            .unwrap_or(0);
        let sample_rate = render.map(|render| render.sample_rate).unwrap_or(1);
        let planned = launch
            .job
            .mixdown
            .iter()
            .chain(launch.job.tracks.iter().map(|track| &track.request))
            .map(|request| request.output_path.clone())
            .collect();
        let realtime = launch.realtime.is_some();
        // Started on the next turn: starting a realtime render reaches into the
        // studio, which must not happen from inside this window's creation.
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(1))
                .await;
            let _ = this.update(cx, |this, cx| this.start(cx));
        })
        .detach();
        Self {
            language,
            launch: Some(launch),
            realtime,
            planned,
            total_frames,
            sample_rate,
            state: RenderState::Starting,
            shared: Arc::new(Mutex::new(RenderShared::default())),
            cancel: ExportCancelToken::new(),
            transport_lent: None,
            focus_handle: cx.focus_handle(),
        }
    }

    fn start(&mut self, cx: &mut Context<Self>) {
        let Some(launch) = self.launch.take() else {
            return;
        };
        let started = match launch.realtime.clone() {
            Some(hooks) => self.start_realtime(launch, hooks, cx),
            None => {
                self.start_offline(launch);
                Ok(())
            }
        };
        match started {
            Ok(()) => {
                self.state = RenderState::Running(ExportProgress::stage_only(
                    ExportStage::Preparing,
                    self.total_frames,
                ));
                self.poll_until_done(cx);
            }
            Err(message) => self.state = RenderState::Failed(message),
        }
        cx.notify();
    }

    fn start_offline(&mut self, launch: RenderLaunch) {
        let shared = self.shared.clone();
        let cancel = self.cancel.clone();
        std::thread::Builder::new()
            .name("fb-render-offline".to_string())
            .spawn(move || {
                let progress_shared = shared.clone();
                let on_progress = move |progress| {
                    if let Ok(mut guard) = progress_shared.lock() {
                        guard.progress = Some(progress);
                    }
                };
                // Scope the sink handoff so the live sinks are re-installed
                // (guard Drop) before the result is published — and on any
                // panic path, since Drop runs during unwind.
                let result = {
                    let _handoff = BridgeSinkHandoff::detach(
                        launch.audio_engine.as_ref(),
                        &launch.bridge_sinks,
                    );
                    export_render_job_with_bridges(
                        &launch.snapshot,
                        &launch.job,
                        &cancel,
                        Some(&launch.bridge_sinks),
                        on_progress,
                    )
                    .map_err(|error| error.to_string())
                };
                if let Ok(mut guard) = shared.lock() {
                    guard.done = Some(result);
                }
            })
            .ok();
    }

    /// Arm the engine's capture, take the transport, and start the writer.
    fn start_realtime(
        &mut self,
        launch: RenderLaunch,
        hooks: RealtimeTransportHooks,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let engine = launch
            .audio_engine
            .clone()
            .ok_or_else(|| "no audio engine is running".to_string())?;
        let render = launch
            .job
            .mixdown
            .as_ref()
            .map(|mixdown| mixdown.render.clone())
            .or_else(|| launch.job.tracks.first().map(|t| t.request.render.clone()))
            .ok_or_else(|| "nothing to render".to_string())?;
        let track_ids: Vec<String> = launch
            .job
            .tracks
            .iter()
            .map(|track| track.track_id.clone())
            .collect();
        let capacity = render.sample_rate as usize * REALTIME_CAPTURE_SECONDS;
        let capture = engine
            .begin_render_capture(&track_ids, capacity)
            .map_err(|error| error.to_string())?;
        let arm = {
            let capture = capture.clone();
            Box::new(move || capture.set_recording(true)) as Box<dyn FnOnce()>
        };
        let start_seconds = render.start_sample as f64 / f64::from(render.sample_rate.max(1));
        if let Err(message) = (hooks.start)(start_seconds, arm, cx) {
            engine.end_render_capture();
            return Err(message);
        }
        self.transport_lent = Some((hooks, engine.clone()));
        let latency = engine.render_latency_frames();

        let shared = self.shared.clone();
        let cancel = self.cancel.clone();
        let job = launch.job;
        std::thread::Builder::new()
            .name("fb-render-realtime".to_string())
            .spawn(move || {
                let progress_shared = shared.clone();
                let on_progress = move |progress| {
                    if let Ok(mut guard) = progress_shared.lock() {
                        guard.progress = Some(progress);
                    }
                };
                let result = record_render_job(&capture, &job, latency, &cancel, on_progress)
                    .map_err(|error| error.to_string());
                if let Ok(mut guard) = shared.lock() {
                    guard.done = Some(result);
                }
            })
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    /// Give the studio its transport back, once.
    fn return_transport(&mut self, cx: &mut Context<Self>) {
        if let Some((hooks, engine)) = self.transport_lent.take() {
            (hooks.finish)(cx);
            engine.end_render_capture();
        }
    }

    fn poll_until_done(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let executor = cx.background_executor().clone();
            loop {
                if crate::shutdown::ShutdownState::global().is_shutting_down() {
                    break;
                }
                executor.timer(std::time::Duration::from_millis(50)).await;
                let keep_going = this.update(cx, |this, cx| this.poll(cx)).unwrap_or(false);
                if !keep_going {
                    break;
                }
            }
        })
        .detach();
    }

    /// Apply the latest progress/result. `false` once the render is over.
    fn poll(&mut self, cx: &mut Context<Self>) -> bool {
        let (progress, done) = {
            let Ok(mut guard) = self.shared.lock() else {
                return true;
            };
            (guard.progress.take(), guard.done.take())
        };
        if let Some(done) = done {
            self.return_transport(cx);
            self.state = match done {
                Ok(summaries) => RenderState::Complete(summaries),
                Err(_) if self.cancel.is_cancelled() => RenderState::Cancelled,
                Err(message) => RenderState::Failed(message),
            };
            cx.notify();
            return false;
        }
        if let Some(progress) = progress {
            self.state = RenderState::Running(progress);
            cx.notify();
        }
        true
    }

    fn running(&self) -> bool {
        matches!(self.state, RenderState::Starting | RenderState::Running(_))
    }

    fn request_cancel(&mut self, cx: &mut Context<Self>) {
        self.cancel.cancel();
        cx.notify();
    }

    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Closing cancels a running render and, at once, gives back the
        // transport it was playing — not whenever the writer notices.
        self.cancel.cancel();
        self.return_transport(cx);
        window.remove_window();
    }

    fn handle_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if event.keystroke.key.as_str() == "escape" {
            if self.running() {
                self.request_cancel(cx);
            } else {
                self.close(window, cx);
            }
        }
    }

    fn output_folder(&self) -> Option<PathBuf> {
        let first = match &self.state {
            RenderState::Complete(summaries) => summaries.first().map(|s| s.output_path.clone()),
            _ => self.planned.first().cloned(),
        }?;
        // The mixdown's folder, or — for channels only — the Stems folder's
        // parent, where the mixdown would have gone.
        let parent = first.parent()?.to_path_buf();
        Some(parent)
    }
}

// ── Rendering ────────────────────────────────────────────────────────────────

impl Render for RenderDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let i18n = I18n::new(&self.language);
        let target = cx.entity().clone();
        let (body, footer) = match &self.state {
            RenderState::Starting | RenderState::Running(_) => (
                self.render_running(i18n),
                self.footer_running(i18n, &target),
            ),
            RenderState::Complete(summaries) => (
                self.render_report(summaries, i18n),
                self.footer_done(i18n, &target, true),
            ),
            RenderState::Failed(message) => (
                terminal_message(
                    i18n.tr_or("render.state.failed", "Render failed"),
                    message.clone(),
                    Colors::status_error(),
                ),
                self.footer_done(i18n, &target, false),
            ),
            RenderState::Cancelled => (
                terminal_message(
                    i18n.tr_or("render.state.cancelled", "Render cancelled"),
                    i18n.tr_or(
                        "render.state.cancelled-hint",
                        "No file was written. Partial output was discarded.",
                    ),
                    Colors::text_muted(),
                ),
                self.footer_done(i18n, &target, false),
            ),
        };
        div()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .font(theme::ui_font())
            .bg(Colors::surface_base())
            .overflow_hidden()
            .rounded(px(radius::DIALOG))
            .border(px(1.0))
            .border_color(Colors::border_normal())
            .shadow(elevation::shadow(elevation::OVERLAY))
            .track_focus(&self.focus_handle)
            .capture_key_down({
                let target = target.clone();
                move |event, window, cx| {
                    let _ = target.update(cx, |this, cx| this.handle_key(event, window, cx));
                }
            })
            .child(external_window_titlebar_with_icon(
                Some(assets::ICON_SHARE_PATH),
                if self.realtime {
                    i18n.tr_or("render.title.realtime", "Realtime Render")
                } else {
                    i18n.tr_or("render.title", "Render")
                },
                "render-window-close",
                {
                    let target = target.clone();
                    move |window, cx| {
                        let _ = target.update(cx, |this, cx| this.close(window, cx));
                    }
                },
            ))
            .child(body)
            .child(footer)
    }
}

impl RenderDialog {
    fn render_running(&self, i18n: I18n) -> gpui::AnyElement {
        let progress = match &self.state {
            RenderState::Running(progress) => progress.clone(),
            _ => ExportProgress::stage_only(ExportStage::Preparing, self.total_frames),
        };
        let determinate = matches!(
            progress.stage,
            ExportStage::Encoding | ExportStage::Rendering
        );
        let value = if determinate {
            ProgressBarValue::value(progress.percent / 100.0)
        } else {
            ProgressBarValue::Indeterminate
        };
        let heading = if self.realtime {
            i18n.tr_or("render.heading.realtime", "Recording in real time")
        } else {
            i18n.tr_or(stage_key(progress.stage), progress.stage.as_str())
        };

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
                        .child(heading),
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
            let rate = f64::from(self.sample_rate.max(1));
            rows.push(readout_row(
                i18n.tr_or("render.readout.position", "Position"),
                format!(
                    "{} / {}",
                    format_duration(progress.rendered_frames as f64 / rate),
                    format_duration(progress.total_frames as f64 / rate)
                ),
            ));
        }
        if self.realtime {
            rows.push(note_row(i18n.tr_or(
                "render.note.realtime",
                "The project is playing through the live engine. Leave the transport \
                 alone until the render finishes; moving it stops the render.",
            )));
        }

        let mut files: Vec<gpui::AnyElement> = self
            .planned
            .iter()
            .take(RUNNING_FILE_LIST_MAX)
            .map(|path| path_line(file_label(path)))
            .collect();
        if self.planned.len() > RUNNING_FILE_LIST_MAX {
            files.push(note_row(tr_vars_or(
                i18n,
                "render.readout.more-files",
                "…and { $count } more.",
                &[(
                    "count",
                    (self.planned.len() - RUNNING_FILE_LIST_MAX).to_string(),
                )],
            )));
        }

        body_scroll()
            .child(form_section(
                i18n.tr_or("render.section.progress", "Progress"),
                rows,
            ))
            .child(form_section(
                tr_vars_or(
                    i18n,
                    "render.section.writing",
                    "Writing { $count } file(s)",
                    &[("count", self.planned.len().to_string())],
                ),
                files,
            ))
            .into_any_element()
    }

    fn render_report(
        &self,
        summaries: &[ArrangementExportSummary],
        i18n: I18n,
    ) -> gpui::AnyElement {
        let clipped = summaries.iter().filter(|s| s.levels.clips()).count();
        let true_peak_over = summaries
            .iter()
            .filter(|s| !s.levels.clips() && s.levels.true_peak_over())
            .count();
        let (tone, title, hint) = if clipped > 0 {
            (
                Colors::status_error(),
                tr_vars_or(
                    i18n,
                    "render.report.clipped",
                    "{ $count } file(s) went over 0 dBFS",
                    &[("count", clipped.to_string())],
                ),
                i18n.tr_or(
                    "render.report.clipped-hint",
                    "Lower the master or the loudest channels and render again. A 32-bit \
                     float WAV keeps the overs instead of clipping them.",
                ),
            )
        } else if true_peak_over > 0 {
            (
                Colors::status_warning(),
                tr_vars_or(
                    i18n,
                    "render.report.true-peak",
                    "{ $count } file(s) peak above 0 dBTP between samples",
                    &[("count", true_peak_over.to_string())],
                ),
                i18n.tr_or(
                    "render.report.true-peak-hint",
                    "No sample went over 0 dBFS, but a converter or an MP3/AAC encoder can \
                     still clip these peaks. Leave about 1 dB of headroom.",
                ),
            )
        } else {
            (
                Colors::status_success(),
                i18n.tr_or("render.report.clean", "Render complete"),
                i18n.tr_or("render.report.clean-hint", "No file went over 0 dBFS."),
            )
        };

        let total_seconds: f64 = summaries.iter().map(|s| s.duration_seconds).sum();
        let mut summary_rows = vec![readout_row(
            i18n.tr_or("export.readout.files", "Files"),
            summaries.len().to_string(),
        )];
        if let Some(first) = summaries.first() {
            summary_rows.push(readout_row(
                i18n.tr_or("export.readout.output", "Output"),
                format!(
                    "{} Hz · {}",
                    first.sample_rate,
                    channel_label(first.channels, i18n)
                ),
            ));
        }
        summary_rows.push(readout_row(
            i18n.tr_or("export.readout.total-duration", "Total duration"),
            format_duration(total_seconds),
        ));

        let files: Vec<gpui::AnyElement> = summaries
            .iter()
            .map(|summary| file_report(summary, i18n))
            .collect();

        body_scroll()
            .child(status_banner(tone, title, hint))
            .child(form_section(
                i18n.tr_or("export.section.summary", "Summary"),
                summary_rows,
            ))
            .child(form_section(
                i18n.tr_or("render.section.files", "Files"),
                files,
            ))
            .into_any_element()
    }

    fn footer_running(&self, i18n: I18n, target: &Entity<Self>) -> gpui::AnyElement {
        let cancelling = self.cancel.is_cancelled();
        footer_band()
            .child(fb_button(
                "render-cancel",
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

    fn footer_done(&self, i18n: I18n, target: &Entity<Self>, wrote: bool) -> gpui::AnyElement {
        let folder = wrote.then(|| self.output_folder()).flatten();
        footer_band()
            .children(folder.map(|folder| {
                fb_button(
                    "render-open-folder",
                    i18n.tr_or("export.action.open-folder", "Open Folder"),
                    FbButtonKind::Default,
                    true,
                    move |_, _window, _cx| {
                        let _ = open_in_file_manager(&folder);
                    },
                )
            }))
            .child(fb_button(
                "render-close",
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
}

/// One written file: its name and length, its levels, and — in words, a
/// badge and colour, not colour alone — whether it went over.
fn file_report(summary: &ArrangementExportSummary, i18n: I18n) -> gpui::AnyElement {
    let levels = &summary.levels;
    let db = |value: Option<f32>, unit: &str| match value {
        Some(value) => format!("{} {unit}", format_db(value)),
        None => format!("— {unit}"),
    };
    let mut measures = vec![
        tr_vars_or(
            i18n,
            "render.level.peak",
            "Peak { $value }",
            &[("value", db(levels.sample_peak_db, "dBFS"))],
        ),
        tr_vars_or(
            i18n,
            "render.level.true-peak",
            "True peak { $value }",
            &[("value", db(levels.true_peak_db, "dBTP"))],
        ),
    ];
    if let Some(lufs) = levels.integrated_lufs {
        measures.push(format!("{} LUFS", format_db(lufs)));
    }

    let (badge, tone, warning) = if levels.clips() {
        let at = levels
            .first_clip_seconds
            .map(format_duration)
            .unwrap_or_else(|| "—".to_string());
        let consequence = if summary.overs_clamped {
            i18n.tr_or("render.clip.clamped", "clipped flat in this file.")
        } else {
            i18n.tr_or(
                "render.clip.kept",
                "kept above 0 dBFS in the float file; it clips when converted.",
            )
        };
        (
            Some(i18n.tr_or("render.badge.clipped", "CLIPPED")),
            Colors::status_error(),
            Some(tr_vars_or(
                i18n,
                "render.clip.detail",
                "{ $count } samples over 0 dBFS, the first at { $at } — { $consequence }",
                &[
                    ("count", grouped(levels.clipped_samples)),
                    ("at", at),
                    ("consequence", consequence),
                ],
            )),
        )
    } else if levels.true_peak_over() {
        (
            Some(i18n.tr_or("render.badge.true-peak", "TRUE PEAK")),
            Colors::status_warning(),
            Some(i18n.tr_or(
                "render.clip.true-peak",
                "Peaks above 0 dBTP between samples; can clip on playback or in lossy formats.",
            )),
        )
    } else {
        (None, Colors::border_subtle(), None)
    };

    div()
        .flex()
        .flex_col()
        .gap(px(space::HAIR))
        .p(px(space::SNUG))
        .rounded(px(radius::CONTROL))
        .border(px(1.0))
        .border_color(if badge.is_some() {
            Colors::with_alpha(tone, 0.6)
        } else {
            Colors::border_subtle()
        })
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::SNUG))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(typography::UI_XS))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(Colors::text_primary())
                        .child(file_label(&summary.output_path)),
                )
                .children(badge.map(|label| fb_badge(label, tone)))
                .child(
                    div()
                        .flex_shrink_0()
                        .font_features(tabular_figures())
                        .text_size(px(typography::DENSE_LABEL))
                        .text_color(Colors::text_muted())
                        .child(format_duration(summary.duration_seconds)),
                ),
        )
        .child(
            div()
                .font_features(tabular_figures())
                .text_size(px(typography::DENSE_LABEL))
                .text_color(Colors::text_secondary())
                .child(measures.join(" · ")),
        )
        .children(warning.map(|warning| {
            div()
                .text_size(px(typography::DENSE_LABEL))
                .text_color(tone)
                .child(warning)
        }))
        .into_any_element()
}

/// Outcome band at the top of the report: a rail and a tone for the eye, a
/// sentence for everyone.
fn status_banner(tone: gpui::Rgba, title: String, hint: String) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .flex_shrink_0()
        .items_start()
        .gap(px(space::BASE))
        .max_w(px(FORM_MAX_WIDTH))
        .p(px(space::BASE))
        .rounded(px(radius::SURFACE))
        .bg(Colors::composite(
            Colors::surface_base(),
            Colors::with_alpha(tone, 0.12),
        ))
        .child(
            div()
                .flex_shrink_0()
                .w(px(STATUS_RAIL_WIDTH))
                .h(px(size::DEFAULT))
                .bg(tone),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(space::HAIR))
                .min_w_0()
                .child(
                    div()
                        .text_size(px(typography::UI_SM))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(Colors::text_primary())
                        .child(title),
                )
                .child(
                    div()
                        .text_size(px(typography::UI_XS))
                        .text_color(Colors::text_secondary())
                        .child(hint),
                ),
        )
}

fn terminal_message(title: String, detail: String, tone: gpui::Rgba) -> gpui::AnyElement {
    body_scroll()
        .child(status_banner(tone, title, detail))
        .into_any_element()
}

fn path_line(name: String) -> gpui::AnyElement {
    div()
        .min_w_0()
        .truncate()
        .text_size(px(typography::UI_XS))
        .text_color(Colors::text_secondary())
        .child(name)
        .into_any_element()
}

/// Open the Render dialog on `launch`, centered over `owner_bounds`. The
/// render starts as soon as the window is up.
pub fn open_render_dialog(
    owner_bounds: Option<Bounds<gpui::Pixels>>,
    language: &str,
    launch: RenderLaunch,
    cx: &mut App,
) -> Result<WindowHandle<RenderDialog>, String> {
    use crate::window_position::{apply_owner_display, centered_window_bounds};
    use gpui::{size, WindowBackgroundAppearance, WindowBounds, WindowKind};

    let height = TITLEBAR_HEIGHT + RENDER_WINDOW_HEIGHT;
    let window_bounds =
        centered_window_bounds(owner_bounds, size(px(RENDER_WINDOW_WIDTH), px(height)), cx);
    let mut window_options = crate::platform_chrome::external_dialog_window_options_partial();
    window_options.window_bounds = Some(WindowBounds::Windowed(window_bounds));
    window_options.kind = WindowKind::Dialog;
    window_options.is_resizable = true;
    window_options.is_minimizable = false;
    window_options.window_background = WindowBackgroundAppearance::Transparent;
    window_options.window_min_size = Some(size(
        px(RENDER_WINDOW_MIN_WIDTH),
        px(TITLEBAR_HEIGHT + RENDER_WINDOW_MIN_HEIGHT),
    ));
    apply_owner_display(&mut window_options, owner_bounds, cx);

    let language = language.to_string();
    cx.open_window(window_options, move |window, cx| {
        let dialog = cx.new(|cx| RenderDialog::new(language, launch, cx));
        let focus = dialog.read(cx).focus_handle.clone();
        focus.focus(window, cx);
        dialog
    })
    .map_err(|e| e.to_string())
}

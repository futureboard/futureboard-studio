//! Software Update dialog.
//!
//! Opened from Help / application menu → Check for Updates (`app:check-for-updates`)
//! and by the automatic startup check when it finds a release. One window at a
//! time; re-opening activates the existing one.
//!
//! All release lookup, download, and installer hand-off runs through
//! [`crate::update_service`] on the background executor. This entity only owns
//! presentation state and polls the shared download counters.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, size, App, AppContext, Bounds, Context, FocusHandle, Global, InteractiveElement,
    IntoElement, KeyDownEvent, ParentElement, Render, Styled, Window, WindowBackgroundAppearance,
    WindowBounds, WindowHandle, WindowKind,
};

use crate::components::controls::{fb_button, FbButtonKind};
use crate::components::progress_dialog::{progress_bar, ProgressBarValue};
use crate::components::title_bar::external_window_titlebar;
use crate::settings::{SettingsSchema, UpdateChannel};
use crate::theme::{self, Colors};
use crate::update_service::{
    format_bytes, update_provider, InstallOutcome, UpdateCandidate, UpdateProvider,
};
use crate::window_position::{apply_owner_display, centered_window_bounds};

pub const UPDATE_WINDOW_WIDTH: f32 = 520.0;
pub const UPDATE_WINDOW_HEIGHT: f32 = 300.0;

/// Poll interval for the download counters. The download itself runs on a
/// background thread; the UI samples it, so no cross-thread entity update is
/// needed.
const DOWNLOAD_POLL: Duration = Duration::from_millis(100);

#[derive(Default)]
struct DownloadShared {
    received: AtomicU64,
    total: AtomicU64,
    finished: Mutex<Option<Result<PathBuf, String>>>,
}

enum Phase {
    Checking,
    UpToDate,
    Available(UpdateCandidate),
    Downloading {
        candidate: UpdateCandidate,
        shared: Arc<DownloadShared>,
    },
    Ready {
        version: String,
        staged: PathBuf,
    },
    Installing,
    Failed(String),
}

pub struct UpdateDialogWindow {
    focus_handle: FocusHandle,
    phase: Phase,
    channel: UpdateChannel,
    current_version: String,
}

/// Live Software Update window, so the menu command and the automatic check
/// share one surface instead of stacking dialogs.
struct OpenUpdateDialog(WindowHandle<UpdateDialogWindow>);

impl Global for OpenUpdateDialog {}

impl UpdateDialogWindow {
    fn new(candidate: Option<UpdateCandidate>, cx: &mut Context<Self>) -> Self {
        let general = SettingsSchema::load_from_disk().general;
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            phase: Phase::Checking,
            channel: general.update_channel,
            current_version: update_provider()
                .map(|provider| provider.current_version())
                .unwrap_or_else(crate::edition::app_version),
        };
        match candidate {
            Some(candidate) => {
                this.channel = candidate.channel;
                this.phase = Phase::Available(candidate);
            }
            None => this.start_check(cx),
        }
        this
    }

    fn provider_or_fail(&mut self, cx: &mut Context<Self>) -> Option<Arc<dyn UpdateProvider>> {
        match update_provider() {
            Some(provider) => Some(provider),
            None => {
                self.phase =
                    Phase::Failed("This build has no update service registered.".to_string());
                cx.notify();
                None
            }
        }
    }

    fn start_check(&mut self, cx: &mut Context<Self>) {
        let Some(provider) = self.provider_or_fail(cx) else {
            return;
        };
        self.channel = SettingsSchema::load_from_disk().general.update_channel;
        self.current_version = provider.current_version();
        self.phase = Phase::Checking;
        cx.notify();

        let channel = self.channel;
        let executor = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            let result = executor.spawn(async move { provider.check(channel) }).await;
            let _ = this.update(cx, |this, cx| {
                this.phase = match result {
                    Ok(Some(candidate)) => Phase::Available(candidate),
                    Ok(None) => Phase::UpToDate,
                    Err(error) => Phase::Failed(error),
                };
                cx.notify();
            });
        })
        .detach();
    }

    fn start_download(&mut self, candidate: UpdateCandidate, cx: &mut Context<Self>) {
        let Some(provider) = self.provider_or_fail(cx) else {
            return;
        };
        let shared = Arc::new(DownloadShared::default());
        shared.total.store(candidate.asset_size, Ordering::Relaxed);
        self.phase = Phase::Downloading {
            candidate: candidate.clone(),
            shared: shared.clone(),
        };
        cx.notify();

        let executor = cx.background_executor().clone();
        let worker_shared = shared.clone();
        executor
            .spawn(async move {
                let progress_shared = worker_shared.clone();
                let result = provider.download(&candidate, &move |received, total| {
                    progress_shared.received.store(received, Ordering::Relaxed);
                    if total > 0 {
                        progress_shared.total.store(total, Ordering::Relaxed);
                    }
                });
                if let Ok(mut slot) = worker_shared.finished.lock() {
                    *slot = Some(result);
                }
            })
            .detach();

        let poll_executor = cx.background_executor().clone();
        cx.spawn(async move |this, cx| loop {
            if crate::shutdown::ShutdownState::global().is_shutting_down() {
                break;
            }
            poll_executor.timer(DOWNLOAD_POLL).await;
            let finished = shared.finished.lock().ok().and_then(|mut slot| slot.take());
            let keep_going = this
                .update(cx, |this, cx| {
                    let Phase::Downloading { candidate, .. } = &this.phase else {
                        return false;
                    };
                    match finished {
                        None => {
                            cx.notify();
                            true
                        }
                        Some(Ok(staged)) => {
                            this.phase = Phase::Ready {
                                version: candidate.version.clone(),
                                staged,
                            };
                            cx.notify();
                            false
                        }
                        Some(Err(error)) => {
                            this.phase = Phase::Failed(error);
                            cx.notify();
                            false
                        }
                    }
                })
                .unwrap_or(false);
            if !keep_going {
                break;
            }
        })
        .detach();
    }

    fn start_install(&mut self, staged: PathBuf, cx: &mut Context<Self>) {
        let Some(provider) = self.provider_or_fail(cx) else {
            return;
        };
        self.phase = Phase::Installing;
        cx.notify();

        let executor = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .spawn(async move { provider.install(&staged) })
                .await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(InstallOutcome::QuitRequired) => {
                    cx.quit();
                }
                Ok(InstallOutcome::Handoff) => {
                    this.phase = Phase::UpToDate;
                    cx.notify();
                }
                Err(error) => {
                    this.phase = Phase::Failed(error);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn headline(&self) -> String {
        match &self.phase {
            Phase::Checking => "Checking for updates…".to_string(),
            Phase::UpToDate => format!("Futureboard Studio {} is up to date", self.current_version),
            Phase::Available(candidate) => {
                format!("Futureboard Studio {} is available", candidate.version)
            }
            Phase::Downloading { candidate, .. } => {
                format!("Downloading Futureboard Studio {}", candidate.version)
            }
            Phase::Ready { version, .. } => format!("Futureboard Studio {version} is ready"),
            Phase::Installing => "Starting the installer…".to_string(),
            Phase::Failed(_) => "Update failed".to_string(),
        }
    }

    fn detail(&self) -> String {
        match &self.phase {
            Phase::Checking => format!(
                "Contacting the {} channel for release {}.",
                self.channel.label(),
                self.current_version
            ),
            Phase::UpToDate => format!("No newer release on the {} channel.", self.channel.label()),
            Phase::Available(candidate) => format!(
                "{} · {} · {} channel",
                candidate.asset_name,
                format_bytes(candidate.asset_size),
                candidate.channel.label()
            ),
            Phase::Downloading { shared, .. } => {
                let received = shared.received.load(Ordering::Relaxed);
                let total = shared.total.load(Ordering::Relaxed);
                if total > 0 {
                    format!("{} of {}", format_bytes(received), format_bytes(total))
                } else {
                    format_bytes(received)
                }
            }
            Phase::Ready { .. } => install_detail().to_string(),
            Phase::Installing => {
                "Save your project — Futureboard closes to finish the update.".to_string()
            }
            Phase::Failed(error) => error.clone(),
        }
    }
}

/// Platform wording for what "Install" actually does.
fn install_detail() -> &'static str {
    #[cfg(target_os = "windows")]
    {
        "Save your project first. Futureboard closes and the installer updates \
         this installation in place."
    }
    #[cfg(target_os = "macos")]
    {
        "Save your project first. Futureboard closes, the application bundle is \
         replaced, and Studio reopens."
    }
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    {
        "Save your project first. Futureboard closes, the AppImage is replaced, \
         and Studio reopens."
    }
}

impl Render for UpdateDialogWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let failed = matches!(self.phase, Phase::Failed(_));

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(Colors::surface_base())
            .text_color(Colors::text_primary())
            .font(theme::ui_font())
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|_this, event: &KeyDownEvent, window, _cx| {
                if event.keystroke.key.as_str() == "escape" {
                    window.remove_window();
                }
            }))
            .child(external_window_titlebar(
                "Software Update",
                "update-window-close",
                move |window, cx| {
                    let _ = cx;
                    window.remove_window();
                },
            ))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .gap(px(10.0))
                    .px(px(20.0))
                    .pt(px(20.0))
                    .child(
                        div()
                            .text_size(px(15.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .child(self.headline()),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(if failed {
                                Colors::status_error()
                            } else {
                                Colors::text_muted()
                            })
                            .child(self.detail()),
                    )
                    .child(self.body(cx)),
            )
            .child(self.footer(cx))
    }
}

impl UpdateDialogWindow {
    /// Progress rail for the phases that have one; an installed-version row
    /// otherwise. Keeping one owner here avoids the footer shifting between
    /// phases.
    fn body(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        let bar = match &self.phase {
            Phase::Checking | Phase::Installing => Some(ProgressBarValue::Indeterminate),
            Phase::Downloading { shared, .. } => {
                let total = shared.total.load(Ordering::Relaxed);
                let received = shared.received.load(Ordering::Relaxed);
                Some(if total > 0 {
                    ProgressBarValue::value(received as f32 / total as f32)
                } else {
                    ProgressBarValue::Indeterminate
                })
            }
            _ => None,
        };

        div()
            .flex()
            .flex_col()
            .justify_center()
            .flex_1()
            .gap(px(8.0))
            .when_some(bar, |element, value| element.child(progress_bar(value)))
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(Colors::text_faint())
                    .child(format!(
                        "Installed {} · {} channel",
                        self.current_version,
                        self.channel.label()
                    )),
            )
    }

    fn footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut row = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_end()
            .gap(px(8.0))
            .flex_none()
            .h(px(52.0))
            .px(px(16.0))
            .border_t(px(1.0))
            .border_color(Colors::border_subtle());

        match &self.phase {
            Phase::Checking | Phase::Installing => {
                row = row.child(fb_button(
                    "update-close",
                    "Close",
                    FbButtonKind::Default,
                    true,
                    move |_, window, _cx| window.remove_window(),
                ));
            }
            Phase::UpToDate | Phase::Failed(_) => {
                row = row
                    .child(fb_button(
                        "update-recheck",
                        "Check Again",
                        FbButtonKind::Default,
                        true,
                        cx.listener(|this, _, _window, cx| this.start_check(cx)),
                    ))
                    .child(fb_button(
                        "update-close",
                        "Close",
                        FbButtonKind::Primary,
                        true,
                        move |_, window, _cx| window.remove_window(),
                    ));
            }
            Phase::Available(candidate) => {
                let candidate = candidate.clone();
                row = row
                    .child(fb_button(
                        "update-later",
                        "Later",
                        FbButtonKind::Default,
                        true,
                        move |_, window, _cx| window.remove_window(),
                    ))
                    .child(fb_button(
                        "update-download",
                        "Download",
                        FbButtonKind::Primary,
                        true,
                        cx.listener(move |this, _, _window, cx| {
                            this.start_download(candidate.clone(), cx)
                        }),
                    ));
            }
            Phase::Downloading { .. } => {
                row = row.child(fb_button(
                    "update-downloading",
                    "Downloading…",
                    FbButtonKind::Default,
                    false,
                    move |_, _window, _cx| {},
                ));
            }
            Phase::Ready { staged, .. } => {
                let staged = staged.clone();
                row = row
                    .child(fb_button(
                        "update-later",
                        "Later",
                        FbButtonKind::Default,
                        true,
                        move |_, window, _cx| window.remove_window(),
                    ))
                    .child(fb_button(
                        "update-install",
                        "Install and Restart",
                        FbButtonKind::Primary,
                        true,
                        cx.listener(move |this, _, _window, cx| {
                            this.start_install(staged.clone(), cx)
                        }),
                    ));
            }
        }

        row
    }
}

/// Open (or focus) the Software Update window and start a fresh check.
pub fn open_update_dialog(
    owner_bounds: Option<Bounds<gpui::Pixels>>,
    cx: &mut App,
) -> Result<WindowHandle<UpdateDialogWindow>, String> {
    open_update_dialog_impl(owner_bounds, None, cx)
}

/// Open (or focus) the Software Update window already showing a release found
/// by the automatic startup check.
pub fn open_update_dialog_for(
    candidate: UpdateCandidate,
    cx: &mut App,
) -> Result<WindowHandle<UpdateDialogWindow>, String> {
    open_update_dialog_impl(None, Some(candidate), cx)
}

fn open_update_dialog_impl(
    owner_bounds: Option<Bounds<gpui::Pixels>>,
    candidate: Option<UpdateCandidate>,
    cx: &mut App,
) -> Result<WindowHandle<UpdateDialogWindow>, String> {
    if let Some(existing) = cx.try_global::<OpenUpdateDialog>().map(|global| global.0) {
        if existing
            .update(cx, |_view, window, _cx| window.activate_window())
            .is_ok()
        {
            return Ok(existing);
        }
        cx.remove_global::<OpenUpdateDialog>();
    }

    let window_bounds = centered_window_bounds(
        owner_bounds,
        size(px(UPDATE_WINDOW_WIDTH), px(UPDATE_WINDOW_HEIGHT)),
        cx,
    );
    let mut options = crate::platform_chrome::external_dialog_window_options_partial();
    options.window_bounds = Some(WindowBounds::Windowed(window_bounds));
    options.kind = WindowKind::Dialog;
    options.is_resizable = false;
    options.is_minimizable = false;
    options.window_background = WindowBackgroundAppearance::Transparent;
    apply_owner_display(&mut options, owner_bounds, cx);

    let handle = crate::dialog_frame::open_dialog_window(cx, options, move |_window, cx| {
        cx.new(|cx| UpdateDialogWindow::new(candidate, cx))
    })
    .map_err(|error| error.to_string())?;
    cx.set_global(OpenUpdateDialog(handle));
    Ok(handle)
}

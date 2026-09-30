//! Compact progress dialogs for session loading and file copy work.
//!
//! Uses the same GPUI-rendered native-dialog chrome as the native message box,
//! but exposes a reusable progress bar with determinate and indeterminate modes.

use std::sync::Arc;

use crate::components::title_bar::{external_window_titlebar_compact, TITLEBAR_HEIGHT};
use crate::theme::{self, Colors};
use std::time::Duration;

use gpui::{
    div, ease_in_out, px, Animation, AnimationExt, App, AppContext, Bounds, Context, FocusHandle,
    InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Render,
    StatefulInteractiveElement, Styled, Window, WindowHandle,
};

pub const PROGRESS_DIALOG_WIDTH: f32 = 430.0;
const PROGRESS_DIALOG_HEIGHT: f32 = 168.0;
const BODY_PAD_X: f32 = 16.0;
const BODY_PAD_Y: f32 = 14.0;
const BODY_GAP: f32 = 10.0;
const BAR_H: f32 = 6.0;
const BAR_RADIUS: f32 = 3.0;
const BUTTON_H: f32 = 27.0;
const BUTTON_PAD_X: f32 = 12.0;
const BUTTON_MIN_W: f32 = 82.0;
const FOOTER_H: f32 = 44.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ProgressBarValue {
    Value(f32),
    Indeterminate,
}

impl Default for ProgressBarValue {
    fn default() -> Self {
        Self::Indeterminate
    }
}

impl ProgressBarValue {
    pub fn value(value: f32) -> Self {
        Self::Value(value.clamp(0.0, 1.0))
    }

    pub fn fraction(self) -> Option<f32> {
        match self {
            Self::Value(value) => Some(value.clamp(0.0, 1.0)),
            Self::Indeterminate => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProgressDialogOptions {
    pub title: String,
    pub heading: String,
    pub detail: Option<String>,
    pub progress: ProgressBarValue,
    pub footer: Option<String>,
    pub show_percent: bool,
    pub cancel_label: Option<String>,
}

impl Default for ProgressDialogOptions {
    fn default() -> Self {
        Self {
            title: "Working".to_string(),
            heading: "Working".to_string(),
            detail: None,
            progress: ProgressBarValue::Indeterminate,
            footer: None,
            show_percent: true,
            cancel_label: None,
        }
    }
}

impl ProgressDialogOptions {
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    pub fn heading(mut self, heading: impl Into<String>) -> Self {
        self.heading = heading.into();
        self
    }

    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub fn progress(mut self, progress: ProgressBarValue) -> Self {
        self.progress = progress;
        self
    }

    pub fn footer(mut self, footer: impl Into<String>) -> Self {
        self.footer = Some(footer.into());
        self
    }

    pub fn cancel_label(mut self, label: impl Into<String>) -> Self {
        self.cancel_label = Some(label.into());
        self
    }

    pub fn hide_percent(mut self) -> Self {
        self.show_percent = false;
        self
    }
}

#[derive(Debug, Clone)]
pub struct LoadingSessionDialogOptions {
    pub session_name: Option<String>,
    pub detail: Option<String>,
    pub progress: ProgressBarValue,
}

impl Default for LoadingSessionDialogOptions {
    fn default() -> Self {
        Self {
            session_name: None,
            detail: Some("Preparing tracks, plug-ins, and media...".to_string()),
            progress: ProgressBarValue::Indeterminate,
        }
    }
}

impl From<LoadingSessionDialogOptions> for ProgressDialogOptions {
    fn from(options: LoadingSessionDialogOptions) -> Self {
        let heading = options
            .session_name
            .filter(|name| !name.is_empty())
            .map(|name| format!("Loading {name}"))
            .unwrap_or_else(|| "Loading session".to_string());
        ProgressDialogOptions {
            title: "Loading Session".to_string(),
            heading,
            detail: options.detail,
            progress: options.progress,
            footer: Some("This can take a moment for large sessions.".to_string()),
            show_percent: options.progress.fraction().is_some(),
            cancel_label: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CopyingFileDialogOptions {
    pub file_name: String,
    pub source: Option<String>,
    pub destination: Option<String>,
    pub progress: ProgressBarValue,
    pub bytes_label: Option<String>,
    pub cancel_label: Option<String>,
}

impl CopyingFileDialogOptions {
    pub fn new(file_name: impl Into<String>) -> Self {
        Self {
            file_name: file_name.into(),
            source: None,
            destination: None,
            progress: ProgressBarValue::Indeterminate,
            bytes_label: None,
            cancel_label: Some("Cancel".to_string()),
        }
    }
}

impl From<CopyingFileDialogOptions> for ProgressDialogOptions {
    fn from(options: CopyingFileDialogOptions) -> Self {
        let mut details = Vec::new();
        if !options.file_name.is_empty() {
            details.push(options.file_name);
        }
        if let Some(source) = options.source.filter(|s| !s.is_empty()) {
            details.push(format!("From: {source}"));
        }
        if let Some(destination) = options.destination.filter(|s| !s.is_empty()) {
            details.push(format!("To: {destination}"));
        }

        ProgressDialogOptions {
            title: "Copying File".to_string(),
            heading: "Copying file".to_string(),
            detail: (!details.is_empty()).then(|| details.join("\n")),
            progress: options.progress,
            footer: options.bytes_label,
            show_percent: true,
            cancel_label: options.cancel_label,
        }
    }
}

pub type ProgressDialogCancelCb = Arc<dyn Fn(&mut Window, &mut App) + Send + Sync>;

pub struct ProgressDialogWindow {
    options: ProgressDialogOptions,
    on_cancel: Option<ProgressDialogCancelCb>,
    focus_handle: FocusHandle,
}

impl ProgressDialogWindow {
    pub fn new(
        options: ProgressDialogOptions,
        on_cancel: Option<ProgressDialogCancelCb>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            options,
            on_cancel,
            focus_handle: cx.focus_handle(),
        }
    }

    pub fn set_options(&mut self, options: ProgressDialogOptions, cx: &mut Context<Self>) {
        self.options = options;
        cx.notify();
    }

    pub fn set_progress(&mut self, progress: ProgressBarValue, cx: &mut Context<Self>) {
        self.options.progress = progress;
        self.options.show_percent = progress.fraction().is_some();
        cx.notify();
    }

    pub fn set_detail(&mut self, detail: impl Into<String>, cx: &mut Context<Self>) {
        self.options.detail = Some(detail.into());
        cx.notify();
    }

    pub fn set_footer(&mut self, footer: impl Into<String>, cx: &mut Context<Self>) {
        self.options.footer = Some(footer.into());
        cx.notify();
    }

    fn close_or_cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(on_cancel) = self.on_cancel.clone() {
            on_cancel(window, cx);
        }
        window.remove_window();
    }

    fn handle_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if event.keystroke.key.as_str() == "escape" && self.options.cancel_label.is_some() {
            self.close_or_cancel(window, cx);
        }
    }
}

impl Render for ProgressDialogWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let target = cx.entity().clone();
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
                self.options.title.clone(),
                "progress-dialog-close",
                {
                    let target = target.clone();
                    move |window, cx| {
                        let _ = target.update(cx, |this, cx| {
                            this.close_or_cancel(window, cx);
                        });
                    }
                },
            ))
            .child(progress_dialog_body(&self.options, target))
    }
}

fn progress_dialog_body(
    options: &ProgressDialogOptions,
    target: gpui::Entity<ProgressDialogWindow>,
) -> impl IntoElement {
    let percent = options
        .progress
        .fraction()
        .map(|v| format!("{:.0}%", v * 100.0));
    let mut body = div()
        .flex()
        .flex_col()
        .flex_1()
        .px(px(BODY_PAD_X))
        .py(px(BODY_PAD_Y))
        .gap(px(BODY_GAP))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(px(10.0))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .text_size(px(12.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(Colors::text_primary())
                        .truncate()
                        .child(options.heading.clone()),
                )
                .children(percent.filter(|_| options.show_percent).map(|percent| {
                    div()
                        .flex_none()
                        .text_size(px(11.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(Colors::accent_primary())
                        .child(percent)
                })),
        )
        .children(
            options
                .detail
                .as_ref()
                .filter(|d| !d.is_empty())
                .map(|detail| {
                    div()
                        .text_size(px(10.0))
                        .line_height(px(15.0))
                        .text_color(Colors::text_muted())
                        .child(detail.clone())
                }),
        )
        .child(progress_bar(options.progress))
        .children(
            options
                .footer
                .as_ref()
                .filter(|f| !f.is_empty())
                .map(|footer| {
                    div()
                        .text_size(px(10.0))
                        .text_color(Colors::text_faint())
                        .truncate()
                        .child(footer.clone())
                }),
        );

    if let Some(cancel_label) = options.cancel_label.clone() {
        body = body.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_end()
                .h(px(FOOTER_H))
                .mt_auto()
                .child(cancel_button(cancel_label, target)),
        );
    }

    body
}

pub fn progress_bar(value: ProgressBarValue) -> impl IntoElement {
    progress_bar_animated(value, 0.0)
}

/// Primary indeterminate segment width, as a fraction of the rail.
const INDETERMINATE_PRIMARY_W: f32 = 0.34;
/// Trailing indeterminate segment width, as a fraction of the rail.
const INDETERMINATE_SECONDARY_W: f32 = 0.20;

/// Determinate bars fill to `value`. Indeterminate bars self-animate a sweeping
/// segment via GPUI's frame-driven `with_animation` — the `phase` argument is
/// retained for API compatibility but no longer needs to be advanced by the
/// caller (previously callers passed a static/zero phase, so the bar never
/// moved). Easing is `ease_in_out` so each sweep accelerates then decelerates.
pub fn progress_bar_animated(value: ProgressBarValue, _phase: f32) -> impl IntoElement {
    let rail = div()
        .h(px(BAR_H))
        .w_full()
        .rounded(px(BAR_RADIUS))
        .bg(Colors::surface_panel_alt())
        .overflow_hidden()
        .relative();

    match value {
        ProgressBarValue::Value(value) => rail.child(
            div()
                .h_full()
                .w(gpui::relative(value.clamp(0.0, 1.0).max(0.015)))
                .bg(Colors::accent_primary()),
        ),
        ProgressBarValue::Indeterminate => rail
            .child(indeterminate_segment(
                "progress-indeterminate-primary",
                INDETERMINATE_PRIMARY_W,
                1300,
                Colors::accent_primary(),
            ))
            .child(indeterminate_segment(
                "progress-indeterminate-secondary",
                INDETERMINATE_SECONDARY_W,
                1000,
                Colors::with_alpha(Colors::accent_primary(), 0.45),
            )),
    }
}

/// One self-animating indeterminate segment that sweeps from just off the left
/// edge to just off the right edge and repeats, eased in/out. Two segments with
/// different periods desync over time for a lively, non-mechanical sweep. The
/// rail clips (`overflow_hidden`) so the off-edge travel is hidden.
fn indeterminate_segment(
    id: &'static str,
    seg_w: f32,
    period_ms: u64,
    color: gpui::Rgba,
) -> impl IntoElement {
    div()
        .absolute()
        .top_0()
        .bottom_0()
        .w(gpui::relative(seg_w))
        .rounded(px(BAR_RADIUS))
        .bg(color)
        .with_animation(
            id,
            Animation::new(Duration::from_millis(period_ms))
                .repeat()
                .with_easing(ease_in_out),
            move |el, delta| {
                let left = -seg_w + delta * (1.0 + seg_w);
                el.left(gpui::relative(left))
            },
        )
}

fn cancel_button(label: String, target: gpui::Entity<ProgressDialogWindow>) -> impl IntoElement {
    div()
        .id("progress-dialog-cancel")
        .flex()
        .items_center()
        .justify_center()
        .h(px(BUTTON_H))
        .min_w(px(BUTTON_MIN_W))
        .px(px(BUTTON_PAD_X))
        .rounded(px(crate::theme::radius::CONTROL))
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .text_size(px(12.0))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(Colors::text_secondary())
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(|s| s.bg(Colors::surface_control_hover()))
        .on_click(move |_, window, cx| {
            let _ = target.update(cx, |this, cx| {
                this.close_or_cancel(window, cx);
            });
        })
        .child(label)
}

fn progress_window_height(options: &ProgressDialogOptions) -> f32 {
    let mut height = TITLEBAR_HEIGHT + PROGRESS_DIALOG_HEIGHT;
    if options.cancel_label.is_some() {
        height += 10.0;
    }
    height
}

pub fn open_progress_dialog_window(
    owner_bounds: Option<Bounds<gpui::Pixels>>,
    options: ProgressDialogOptions,
    on_cancel: Option<ProgressDialogCancelCb>,
    cx: &mut App,
) -> Result<WindowHandle<ProgressDialogWindow>, String> {
    open_progress_dialog_window_with_kind(
        owner_bounds,
        options,
        on_cancel,
        gpui::WindowKind::Dialog,
        cx,
    )
}

/// Open progress as an independent startup surface rather than an owner-bound
/// dialog. This keeps it alive after the Splash popup is removed on Windows.
pub fn open_standalone_progress_dialog_window(
    options: ProgressDialogOptions,
    on_cancel: Option<ProgressDialogCancelCb>,
    cx: &mut App,
) -> Result<WindowHandle<ProgressDialogWindow>, String> {
    open_progress_dialog_window_with_kind(None, options, on_cancel, gpui::WindowKind::Normal, cx)
}

fn open_progress_dialog_window_with_kind(
    owner_bounds: Option<Bounds<gpui::Pixels>>,
    options: ProgressDialogOptions,
    on_cancel: Option<ProgressDialogCancelCb>,
    kind: gpui::WindowKind,
    cx: &mut App,
) -> Result<WindowHandle<ProgressDialogWindow>, String> {
    use crate::window_position::{apply_owner_display, centered_window_bounds};
    use gpui::{size, WindowBackgroundAppearance, WindowBounds};

    let height = progress_window_height(&options);
    let window_bounds = centered_window_bounds(
        owner_bounds,
        size(px(PROGRESS_DIALOG_WIDTH), px(height)),
        cx,
    );

    let mut window_options = crate::platform_chrome::external_dialog_window_options_partial();
    window_options.window_bounds = Some(WindowBounds::Windowed(window_bounds));
    window_options.kind = kind;
    window_options.is_resizable = false;
    window_options.is_minimizable = false;
    window_options.window_background = WindowBackgroundAppearance::Transparent;
    apply_owner_display(&mut window_options, owner_bounds, cx);

    cx.open_window(window_options, move |_window, cx| {
        cx.new(|cx| ProgressDialogWindow::new(options, on_cancel, cx))
    })
    .map_err(|e| e.to_string())
}

pub fn open_loading_session_dialog_window(
    owner_bounds: Option<Bounds<gpui::Pixels>>,
    options: LoadingSessionDialogOptions,
    cx: &mut App,
) -> Result<WindowHandle<ProgressDialogWindow>, String> {
    open_progress_dialog_window(owner_bounds, options.into(), None, cx)
}

pub fn open_copying_file_dialog_window(
    owner_bounds: Option<Bounds<gpui::Pixels>>,
    options: CopyingFileDialogOptions,
    on_cancel: Option<ProgressDialogCancelCb>,
    cx: &mut App,
) -> Result<WindowHandle<ProgressDialogWindow>, String> {
    open_progress_dialog_window(owner_bounds, options.into(), on_cancel, cx)
}

//! Borderless native message box (cross-platform GPUI dialog).
//!
//! Title, message, optional detail, custom button labels, default/cancel
//! indices, and kind (info / warning / error / question). Built on the Studio
//! dialog shell every other dialog uses: the external-dialog title bar,
//! `fb_button` actions in the shared footer band, and only theme tokens for
//! size and type. The window's corners, frame and shadow are the platform
//! window shell's; the content paints none of its own.
//!
//! **Size contract.** The width is fixed; the height is the content's. The
//! window opens at an estimate (so it is centred close to right), then, once
//! the first layout has measured the title bar, body and footer, it is resized
//! to exactly their sum. A long message is never clipped: the body grows up to
//! [`MESSAGE_BODY_MAX_HEIGHT`] and scrolls past it, so the footer always stays
//! on screen.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use crate::components::controls::{fb_button, FbButtonKind};
use crate::components::title_bar::{external_window_titlebar_compact, TITLEBAR_HEIGHT};
use crate::theme::{self, radius, size, space, typography, Colors};
use gpui::{
    div, px, App, Bounds, Context, FocusHandle, InteractiveElement, IntoElement, KeyDownEvent,
    ParentElement, Render, StatefulInteractiveElement, Styled, Window, WindowHandle,
};

pub const MESSAGE_BOX_WIDTH: f32 = 460.0;
/// Ceiling for the message + detail block before it scrolls, so a long
/// report still leaves the window a reasonable height with its footer
/// visible.
const MESSAGE_BODY_MAX_HEIGHT: f32 = 420.0;
/// Inset around the body: a message box is read, not scanned, so it takes
/// the section step rather than a form's.
const BODY_PAD: f32 = space::SECTION;
/// Action band: one `PROMINENT` button plus its breathing room — the same
/// band as the Export and Render dialogs.
const FOOTER_HEIGHT: f32 = size::PROMINENT + 2.0 * space::BASE;
/// The kind token (i / ! / ?) beside the message.
const KIND_TOKEN_SIZE: f32 = size::COMFORTABLE;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MessageBoxKind {
    #[default]
    None,
    Info,
    Error,
    Question,
    Warning,
}

#[derive(Debug, Clone)]
pub struct MessageBoxOptions {
    pub kind: MessageBoxKind,
    pub title: String,
    pub message: String,
    pub detail: Option<String>,
    pub buttons: Vec<String>,
    pub default_id: usize,
    pub cancel_id: Option<usize>,
}

impl Default for MessageBoxOptions {
    fn default() -> Self {
        Self {
            kind: MessageBoxKind::None,
            title: String::new(),
            message: String::new(),
            detail: None,
            buttons: vec!["OK".to_string()],
            default_id: 0,
            cancel_id: None,
        }
    }
}

impl MessageBoxOptions {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            ..Default::default()
        }
    }

    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub fn kind(mut self, kind: MessageBoxKind) -> Self {
        self.kind = kind;
        self
    }

    pub fn buttons(mut self, buttons: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.buttons = buttons.into_iter().map(Into::into).collect();
        self
    }

    pub fn default_id(mut self, id: usize) -> Self {
        self.default_id = id;
        self
    }

    pub fn cancel_id(mut self, id: usize) -> Self {
        self.cancel_id = Some(id);
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageBoxResult {
    pub response: usize,
    /// The box was dismissed (Escape, or its close button) rather than
    /// answered with a button; `response` is then the cancel button's index.
    pub dismissed: bool,
}

/// What a message box hands back when a button is chosen. Public so callers
/// can name the callback type instead of respelling the whole `dyn Fn`.
pub type MessageBoxResponseCb = Arc<dyn Fn(MessageBoxResult, &mut Window, &mut App) + Send + Sync>;

/// Opening height, before the real layout is measured: wraps each paragraph
/// at an average glyph width. Only placement depends on it — the window is
/// resized to the measured content on its first frame.
fn estimated_height(options: &MessageBoxOptions) -> f32 {
    let text_width = MESSAGE_BOX_WIDTH - 2.0 * BODY_PAD - KIND_TOKEN_SIZE - space::LOOSE;
    let lines = |text: &str, font: f32| -> f32 {
        let per_line = (text_width / (font * 0.55)).max(1.0);
        text.split('\n')
            .map(|line| (line.chars().count() as f32 / per_line).ceil().max(1.0))
            .sum()
    };
    let line_height = |font: f32| font * typography::LINE_HEIGHT;
    let mut body = lines(&options.message, typography::UI_MD) * line_height(typography::UI_MD);
    if let Some(detail) = options.detail.as_ref().filter(|d| !d.is_empty()) {
        body += space::SNUG + lines(detail, typography::UI_SM) * line_height(typography::UI_SM);
    }
    let body = body.max(KIND_TOKEN_SIZE).min(MESSAGE_BODY_MAX_HEIGHT);
    TITLEBAR_HEIGHT + 2.0 * BODY_PAD + body + FOOTER_HEIGHT
}

fn normalized_buttons(options: &MessageBoxOptions) -> Vec<String> {
    if options.buttons.is_empty() {
        return vec!["OK".to_string()];
    }
    options.buttons.clone()
}

fn clamp_index(index: Option<usize>, len: usize) -> Option<usize> {
    index.filter(|&i| i < len)
}

fn button_kind(index: usize, label: &str, options: &MessageBoxOptions, len: usize) -> FbButtonKind {
    if clamp_index(Some(options.default_id), len) == Some(index) {
        return FbButtonKind::Primary;
    }
    let lower = label.to_ascii_lowercase();
    if lower.contains("don't save") || lower == "discard" || lower == "delete" {
        return FbButtonKind::Danger;
    }
    FbButtonKind::Default
}

fn kind_accent(kind: MessageBoxKind) -> gpui::Rgba {
    match kind {
        MessageBoxKind::Error => Colors::status_error(),
        MessageBoxKind::Warning => Colors::status_warning(),
        MessageBoxKind::Info | MessageBoxKind::Question => Colors::accent_primary(),
        MessageBoxKind::None => Colors::text_muted(),
    }
}

fn kind_glyph(kind: MessageBoxKind) -> &'static str {
    match kind {
        MessageBoxKind::Error => "!",
        MessageBoxKind::Warning => "!",
        MessageBoxKind::Info => "i",
        MessageBoxKind::Question => "?",
        MessageBoxKind::None => "·",
    }
}

fn message_box_body(options: &MessageBoxOptions) -> impl IntoElement {
    let accent = kind_accent(options.kind);
    let glyph = kind_glyph(options.kind);

    // The body's own scroller: it takes the text's height up to the ceiling
    // and scrolls past it, so the window never has to clip.
    div()
        .id("message-box-body")
        .flex_shrink_0()
        .max_h(px(MESSAGE_BODY_MAX_HEIGHT + 2.0 * BODY_PAD))
        .overflow_y_scroll()
        .p(px(BODY_PAD))
        .child(
            div()
                .flex()
                .flex_row()
                .items_start()
                .w_full()
                .min_w_0()
                .gap(px(space::LOOSE))
                .child(
                    // Kind is carried by the glyph as well as the colour.
                    div()
                        .flex_shrink_0()
                        .size(px(KIND_TOKEN_SIZE))
                        .rounded(px(radius::PILL))
                        .border(px(1.0))
                        .border_color(Colors::with_alpha(accent, 0.35))
                        .bg(Colors::with_alpha(accent, 0.10))
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(typography::UI_MD))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(accent)
                        .child(glyph),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(space::SNUG))
                        .child(
                            div()
                                .text_size(px(typography::UI_MD))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(Colors::text_primary())
                                .child(options.message.clone()),
                        )
                        .children(options.detail.as_ref().filter(|d| !d.is_empty()).map(
                            |detail| {
                                div()
                                    .text_size(px(typography::UI_SM))
                                    .text_color(Colors::text_muted())
                                    .child(detail.clone())
                            },
                        )),
                ),
        )
}

fn message_box_footer(
    options: &MessageBoxOptions,
    on_response: impl Fn(usize, &mut Window, &mut App) + Clone + 'static,
) -> impl IntoElement {
    let buttons = normalized_buttons(options);
    let len = buttons.len();
    div()
        .flex_shrink_0()
        .flex()
        .flex_row()
        .items_center()
        .justify_end()
        .gap(px(space::BASE))
        .h(px(FOOTER_HEIGHT))
        .px(px(space::LOOSE))
        .border_t(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_titlebar())
        .children(buttons.into_iter().enumerate().map(|(index, label)| {
            let kind = button_kind(index, &label, options, len);
            let on_response = on_response.clone();
            fb_button(
                ("message-box-btn", index),
                label,
                kind,
                true,
                move |_, window, cx| on_response(index, window, cx),
            )
        }))
}

pub struct MessageBoxWindow {
    options: MessageBoxOptions,
    on_response: MessageBoxResponseCb,
    focus_handle: FocusHandle,
    responded: bool,
    /// The height last asked of the window, so the fit runs once per real
    /// change in content height instead of chasing DPI rounding.
    fitted_height: Rc<Cell<f32>>,
}

impl MessageBoxWindow {
    pub fn new(
        options: MessageBoxOptions,
        on_response: MessageBoxResponseCb,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            options,
            on_response,
            focus_handle: cx.focus_handle(),
            responded: false,
            fitted_height: Rc::new(Cell::new(0.0)),
        }
    }

    fn finish(
        &mut self,
        response: usize,
        dismissed: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.responded {
            return;
        }
        self.responded = true;
        let cb = self.on_response.clone();
        window.remove_window();
        cb(
            MessageBoxResult {
                response,
                dismissed,
            },
            window,
            cx,
        );
    }

    fn cancel_response_index(&self) -> usize {
        let len = normalized_buttons(&self.options).len();
        clamp_index(self.options.cancel_id, len)
            .or_else(|| clamp_index(Some(self.options.default_id), len))
            .unwrap_or(0)
    }

    fn handle_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let len = normalized_buttons(&self.options).len();
        match event.keystroke.key.as_str() {
            "escape" => {
                let response = self.cancel_response_index();
                self.finish(response, true, window, cx);
            }
            "enter" | "numpad_enter" => {
                let response = clamp_index(Some(self.options.default_id), len).unwrap_or(0);
                self.finish(response, false, window, cx);
            }
            _ => {}
        }
    }
}

impl Render for MessageBoxWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.focus_handle.is_focused(window) {
            self.focus_handle.focus(window, cx);
        }
        let title = if self.options.title.is_empty() {
            "Futureboard Studio".to_string()
        } else {
            self.options.title.clone()
        };
        let target = cx.entity().clone();
        let fitted_height = self.fitted_height.clone();

        div()
            .flex()
            .flex_col()
            .size_full()
            .font(theme::ui_font())
            .bg(Colors::surface_base())
            // The clip owner. No radius, frame or shadow: the window shell
            // draws those.
            .overflow_hidden()
            .capture_key_down({
                let target = target.clone();
                move |event, window, cx| {
                    let _ = target.update(cx, |this, cx| this.handle_key(event, window, cx));
                }
            })
            // Fit the window to what was laid out: the children keep their
            // natural heights (none of them grows or shrinks), so their sum is
            // the height the content needs whatever size the window has now.
            .on_children_prepainted(move |children, window, cx| {
                let content: f32 = children
                    .iter()
                    .map(|bounds| f32::from(bounds.size.height))
                    .sum();
                let needed = content.ceil();
                let current = f32::from(window.viewport_size().height);
                if (needed - current).abs() > 1.0 && (needed - fitted_height.get()).abs() > 1.0 {
                    fitted_height.set(needed);
                    let width = window.viewport_size().width;
                    window.defer(cx, move |window, _| {
                        window.resize(gpui::size(width, px(needed)));
                    });
                }
            })
            .child(div().w(px(0.0)).h(px(0.0)).track_focus(&self.focus_handle))
            .child(external_window_titlebar_compact(
                title,
                "message-box-close",
                {
                    let target = target.clone();
                    move |window, cx| {
                        let _ = target.update(cx, |this, cx| {
                            this.finish(this.cancel_response_index(), true, window, cx);
                        });
                    }
                },
            ))
            .child(message_box_body(&self.options))
            .child(message_box_footer(&self.options, {
                let target = target.clone();
                move |response, window, cx| {
                    let _ = target.update(cx, |this, cx| {
                        this.finish(response, false, window, cx);
                    });
                }
            }))
    }
}

/// Open a borderless message box centered over `owner_bounds`.
pub fn open_message_box_window(
    owner_bounds: Option<Bounds<gpui::Pixels>>,
    options: MessageBoxOptions,
    on_response: MessageBoxResponseCb,
    cx: &mut App,
) -> Result<WindowHandle<MessageBoxWindow>, String> {
    open_message_box_window_with_kind(
        owner_bounds,
        options,
        on_response,
        gpui::WindowKind::Dialog,
        cx,
    )
}

/// Open a message box as an independent application surface. Startup uses
/// this variant because a modal dialog without a normal owner can become owned
/// by the Splash popup on Windows and disappear when Splash is retired.
pub fn open_standalone_message_box_window(
    options: MessageBoxOptions,
    on_response: MessageBoxResponseCb,
    cx: &mut App,
) -> Result<WindowHandle<MessageBoxWindow>, String> {
    open_message_box_window_with_kind(None, options, on_response, gpui::WindowKind::Normal, cx)
}

fn open_message_box_window_with_kind(
    owner_bounds: Option<Bounds<gpui::Pixels>>,
    options: MessageBoxOptions,
    on_response: MessageBoxResponseCb,
    kind: gpui::WindowKind,
    cx: &mut App,
) -> Result<WindowHandle<MessageBoxWindow>, String> {
    use crate::window_position::{apply_owner_display, centered_window_bounds};
    use gpui::{size, AppContext, WindowBackgroundAppearance, WindowBounds};

    let height = estimated_height(&options);
    let window_bounds =
        centered_window_bounds(owner_bounds, size(px(MESSAGE_BOX_WIDTH), px(height)), cx);

    let mut window_options = crate::platform_chrome::external_dialog_window_options_partial();
    window_options.window_bounds = Some(WindowBounds::Windowed(window_bounds));
    window_options.kind = kind;
    window_options.is_resizable = false;
    window_options.is_minimizable = false;
    window_options.window_background = WindowBackgroundAppearance::Transparent;
    apply_owner_display(&mut window_options, owner_bounds, cx);

    cx.open_window(window_options, move |_window, cx| {
        cx.new(|cx| MessageBoxWindow::new(options, on_response, cx))
    })
    .map_err(|e| e.to_string())
}

/// Preset matching web unsaved-changes guard (`projectLifecycle.ts`).
pub fn unsaved_changes_options(project_name: &str, detail: &str) -> MessageBoxOptions {
    MessageBoxOptions {
        kind: MessageBoxKind::Warning,
        title: "Unsaved Changes".to_string(),
        message: format!("Save changes to \"{project_name}\"?"),
        detail: Some(detail.to_string()),
        buttons: vec![
            "Save".to_string(),
            "Don't Save".to_string(),
            "Cancel".to_string(),
        ],
        default_id: 0,
        cancel_id: Some(2),
    }
}

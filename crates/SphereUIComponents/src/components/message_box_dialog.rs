//! Borderless native message box (cross-platform GPUI dialog).
//!
//! Mirrors the Electron / web [`MessageBoxOptions`] surface: title, message,
//! optional detail, custom button labels, default/cancel indices, and kind
//! (info / warning / error / question). Opens as a compact GPUI-rendered native
//! dialog using the same chrome as Add Track / Settings dialogs.

use std::sync::Arc;

use crate::components::title_bar::{external_window_titlebar_compact, TITLEBAR_HEIGHT};
use crate::theme::{self, Colors};
use gpui::{
    div, px, App, Bounds, Context, FocusHandle, InteractiveElement, IntoElement, KeyDownEvent,
    ParentElement, Render, StatefulInteractiveElement, Styled, Window, WindowHandle,
};

pub const MESSAGE_BOX_WIDTH: f32 = 450.0;
const BODY_PAD_X: f32 = 14.0;
const BODY_PAD_Y: f32 = 12.0;
const FOOTER_PAD_X: f32 = 12.0;
const FOOTER_PAD_Y: f32 = 8.0;
const FOOTER_GAP: f32 = 6.0;
const BUTTON_H: f32 = 27.0;
const BUTTON_PAD_X: f32 = 12.0;
const BUTTON_RADIUS: f32 = 5.0;
/// Compact native-control min width. Wider labels (e.g. "Don't Save") grow past
/// this from their own content + padding rather than forcing every button up.
const BUTTON_MIN_W: f32 = 80.0;
const FOOTER_H: f32 = BUTTON_H + FOOTER_PAD_Y * 2.0;
const MESSAGE_TEXT_SIZE: f32 = 12.0;
const MESSAGE_LINE_H: f32 = 18.0;
/// Vertical room for up to two wrapped message lines at 12px / 18px line-height.
const MESSAGE_BLOCK_H: f32 = MESSAGE_LINE_H * 2.0;
const DETAIL_TEXT_SIZE: f32 = 11.0;
const DETAIL_LINE_H: f32 = 16.0;
const DETAIL_BLOCK_H: f32 = DETAIL_LINE_H * 2.0;
const ICON_GAP: f32 = 10.0;
const BODY_TEXT_GAP: f32 = 4.0;
const WARNING_TOKEN_SIZE: f32 = 28.0;
const ICON_GLYPH_SIZE: f32 = 14.0;

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

#[derive(Clone, Copy, PartialEq, Eq)]
enum MessageBoxButtonStyle {
    Default,
    Primary,
    Destructive,
}

fn message_box_height(options: &MessageBoxOptions) -> f32 {
    let mut h = TITLEBAR_HEIGHT + BODY_PAD_Y + MESSAGE_BLOCK_H + BODY_PAD_Y + FOOTER_H;
    if options.detail.as_ref().is_some_and(|d| !d.is_empty()) {
        h += BODY_TEXT_GAP + DETAIL_BLOCK_H;
    }
    h
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

fn button_style(
    index: usize,
    label: &str,
    options: &MessageBoxOptions,
    len: usize,
) -> MessageBoxButtonStyle {
    if clamp_index(Some(options.default_id), len) == Some(index) {
        return MessageBoxButtonStyle::Primary;
    }
    let lower = label.to_ascii_lowercase();
    if lower.contains("don't save") || lower == "discard" || lower == "delete" {
        return MessageBoxButtonStyle::Destructive;
    }
    MessageBoxButtonStyle::Default
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

/// A calm, flat footer button. Only the primary action is filled; the
/// destructive and neutral actions are ghost/outline so the hierarchy reads
/// softly rather than as three competing solid blocks.
fn message_box_button(
    index: usize,
    label: String,
    style: MessageBoxButtonStyle,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    // (background, border, text, hover background)
    let (bg, border, text, hover_bg) = match style {
        MessageBoxButtonStyle::Primary => (
            Colors::accent_primary(),
            Colors::border_accent(),
            Colors::on_accent(),
            Colors::accent_primary(),
        ),
        MessageBoxButtonStyle::Destructive => (
            Colors::with_alpha(Colors::status_error(), 0.0), // transparent
            Colors::with_alpha(Colors::status_error(), 0.45),
            Colors::status_error(),
            Colors::with_alpha(Colors::status_error(), 0.10),
        ),
        MessageBoxButtonStyle::Default => (
            Colors::with_alpha(Colors::surface_base(), 0.0), // transparent (ghost)
            Colors::border_subtle(),
            Colors::text_secondary(),
            Colors::surface_control_hover(),
        ),
    };

    div()
        .id(("message-box-btn", index))
        .flex()
        .items_center()
        .justify_center()
        .h(px(BUTTON_H))
        .min_w(px(BUTTON_MIN_W))
        .px(px(BUTTON_PAD_X))
        .rounded(px(BUTTON_RADIUS))
        .border(px(1.0))
        .border_color(border)
        .bg(bg)
        .text_size(px(MESSAGE_TEXT_SIZE))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(text)
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(move |s| s.bg(hover_bg))
        .on_click(on_click)
        .child(label)
}

fn message_box_body(
    options: &MessageBoxOptions,
    on_response: MessageBoxResponseCb,
) -> impl IntoElement {
    let buttons = normalized_buttons(options);
    let len = buttons.len();
    let accent = kind_accent(options.kind);
    let glyph = kind_glyph(options.kind);

    // Body: icon + message, top-aligned (no flex grow — native message-box density).
    let content = div()
        .flex_shrink_0()
        .px(px(BODY_PAD_X))
        .py(px(BODY_PAD_Y))
        .child(
            div()
                .flex()
                .flex_row()
                .items_start()
                .w_full()
                .min_w_0()
                .min_h(px(MESSAGE_BLOCK_H))
                .gap(px(ICON_GAP))
                .child(
                    div()
                        .flex_shrink_0()
                        .w(px(WARNING_TOKEN_SIZE))
                        .h(px(WARNING_TOKEN_SIZE))
                        .rounded(px(crate::theme::radius::PILL))
                        .border(px(1.0))
                        .border_color(Colors::with_alpha(accent, 0.35))
                        .bg(Colors::with_alpha(accent, 0.10))
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(ICON_GLYPH_SIZE))
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
                        .gap(px(BODY_TEXT_GAP))
                        .child(
                            div()
                                .text_size(px(MESSAGE_TEXT_SIZE))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .line_height(px(MESSAGE_LINE_H))
                                .text_color(Colors::text_primary())
                                .child(options.message.clone()),
                        )
                        .children(options.detail.as_ref().filter(|d| !d.is_empty()).map(
                            |detail| {
                                div()
                                    .text_size(px(DETAIL_TEXT_SIZE))
                                    .line_height(px(DETAIL_LINE_H))
                                    .text_color(Colors::text_muted())
                                    .child(detail.clone())
                            },
                        )),
                ),
        );

    let mut footer = div()
        .flex()
        .flex_row()
        .justify_end()
        .items_center()
        .gap(px(FOOTER_GAP))
        .h(px(FOOTER_H))
        .px(px(FOOTER_PAD_X))
        .py(px(FOOTER_PAD_Y))
        .border_t(px(1.0))
        .border_color(Colors::border_subtle());

    for (index, label) in buttons.iter().enumerate() {
        let style = button_style(index, label, options, len);
        let on_response = on_response.clone();
        let label = label.clone();
        let on_click = move |_: &gpui::ClickEvent, window: &mut Window, cx: &mut App| {
            on_response(
                MessageBoxResult {
                    response: index,
                    dismissed: false,
                },
                window,
                cx,
            );
        };
        footer = footer.child(message_box_button(index, label, style, on_click));
    }

    div()
        .flex()
        .flex_col()
        .flex_1()
        .child(content)
        .child(footer.flex_shrink_0())
}

pub struct MessageBoxWindow {
    options: MessageBoxOptions,
    on_response: MessageBoxResponseCb,
    focus_handle: FocusHandle,
    responded: bool,
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
            .child(message_box_body(
                &self.options,
                Arc::new({
                    let target = target.clone();
                    move |result, window, cx| {
                        let _ = target.update(cx, |this, cx| {
                            this.finish(result.response, false, window, cx);
                        });
                    }
                }),
            ))
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

    let height = message_box_height(&options);
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

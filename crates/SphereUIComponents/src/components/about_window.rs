//! About Futureboard Studio window.
//!
//! Shows the application icon, name, version, edition, a short runtime summary,
//! and the full project credits embedded from `packages/shared/CREDIT.txt`.
//! Opened from Help → About Futureboard Studio (`app:about`).

use gpui::{
    div, img, px, size, App, AppContext, Bounds, Context, FocusHandle, InteractiveElement,
    IntoElement, KeyDownEvent, ObjectFit, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, StyledImage, Window, WindowBackgroundAppearance,
    WindowBounds, WindowHandle, WindowKind,
};

use crate::components::controls::{fb_button, FbButtonKind};
use crate::components::title_bar::external_window_titlebar;
use crate::embedded_assets::APP_LOGO_PATH;
use crate::theme::{self, Colors};
use crate::window_position::{apply_owner_display, centered_window_bounds};

pub const ABOUT_WINDOW_WIDTH: f32 = 540.0;
pub const ABOUT_WINDOW_HEIGHT: f32 = 660.0;

/// Full project credits, embedded at compile time so the window has no runtime
/// dependency on the source tree / install layout.
const CREDITS_TEXT: &str = include_str!("../../../../packages/shared/CREDIT.txt");

pub struct AboutWindow {
    focus_handle: FocusHandle,
}

impl AboutWindow {
    fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
        }
    }
}

impl Render for AboutWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let version = crate::edition::app_version();
        let info = crate::edition::current_edition_info();
        let edition = info
            .as_ref()
            .map(|info| info.edition)
            .unwrap_or("Community");
        let audio_engine = info.as_ref().and_then(|info| info.audio_engine.clone());

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
                "About Futureboard Studio",
                "about-window-close",
                move |window, cx| {
                    let _ = cx;
                    window.remove_window();
                },
            ))
            .child(about_header(&version, edition))
            .child(info_rows(audio_engine.as_ref().map(|badge| badge.name)))
            .children(audio_engine.as_ref().map(audio_engine_badge))
            .child(credits_panel())
            .child(footer())
    }
}

/// Partner engine credit: logo beside the engine name and its trademark line.
/// Only rendered when the edition provider reports a licensed engine, so a
/// Community build shows nothing here rather than an empty frame.
fn audio_engine_badge(badge: &crate::edition::AudioEngineBadge) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(12.0))
        .mx(px(16.0))
        .mb(px(12.0))
        .px(px(12.0))
        .py(px(8.0))
        .rounded(px(crate::theme::radius::SURFACE))
        .bg(Colors::surface_panel())
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .children(badge.logo.clone().map(|logo| {
            img(gpui::ImageSource::Image(logo))
                .flex_none()
                .h(px(28.0))
                .max_w(px(96.0))
                .object_fit(ObjectFit::Contain)
        }))
        .child(
            div()
                .flex()
                .flex_col()
                .min_w(px(0.0))
                .gap(px(2.0))
                .child(
                    div()
                        .text_size(px(11.5))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(Colors::text_primary())
                        .child(badge.name),
                )
                .child(
                    div()
                        .text_size(px(9.5))
                        .text_color(Colors::text_muted())
                        .child(badge.notice),
                ),
        )
}

/// Icon + name + version/edition badges, centred at the top of the window.
fn about_header(version: &str, edition: &str) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .items_center()
        .gap(px(8.0))
        .px(px(20.0))
        .pt(px(22.0))
        .pb(px(16.0))
        .child(
            img(SharedString::from(APP_LOGO_PATH))
                .w(px(84.0))
                .h(px(84.0))
                .rounded(px(crate::theme::radius::DIALOG))
                .object_fit(ObjectFit::Contain),
        )
        .child(
            div()
                .text_size(px(20.0))
                .font_weight(gpui::FontWeight::BOLD)
                .text_color(Colors::text_primary())
                .child("Futureboard Studio"),
        )
        .child(
            div()
                .text_size(px(11.5))
                .text_color(Colors::text_muted())
                .child("Mochi DAW"),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .mt(px(4.0))
                .child(badge(format!("Version {version}"), false))
                .child(badge(format!("{edition} Edition"), true)),
        )
}

/// A small rounded pill used for the version and edition markers.
fn badge(label: String, accent: bool) -> impl IntoElement {
    let (bg, fg, border) = if accent {
        (
            Colors::accent_muted(),
            Colors::accent_primary(),
            Colors::accent_primary(),
        )
    } else {
        (
            Colors::surface_panel(),
            Colors::text_secondary(),
            Colors::border_subtle(),
        )
    };
    div()
        .px(px(10.0))
        .py(px(3.0))
        .rounded(px(crate::theme::radius::PILL))
        .bg(bg)
        .border(px(1.0))
        .border_color(border)
        .text_size(px(10.5))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(fg)
        .child(label)
}

/// Compact key/value rows summarising the runtime, plugin host, and — on a
/// build running a licensed engine — the audio engine.
fn info_rows(audio_engine: Option<&'static str>) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .px(px(24.0))
        .pb(px(14.0))
        .gap(px(4.0))
        .child(info_row("Runtime", "GPUI + Rust"))
        .children(audio_engine.map(|engine| info_row("Audio Engine", engine)))
        .child(info_row("Plugin Host", "VST3 / CLAP"))
        .child(info_row("Graphics", "WGPU"))
}

fn info_row(key: &'static str, value: &'static str) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .h(px(20.0))
        .text_size(px(11.5))
        .child(div().text_color(Colors::text_muted()).child(key))
        .child(
            div()
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(Colors::text_secondary())
                .child(value),
        )
}

/// Scrollable panel rendering the embedded `CREDIT.txt`, taking the remaining
/// vertical space. Section headers, `name - role` entries, and plain name lines
/// are styled distinctly for readability.
fn credits_panel() -> impl IntoElement {
    let mut lines: Vec<gpui::AnyElement> = Vec::new();
    // A non-`name - role` line that follows a blank line (or opens the file) is
    // a section header; a plain line that follows content is a name/value.
    let mut prev_blank = true;
    for raw in CREDITS_TEXT.lines() {
        let line = raw.trim_end();
        if line.trim().is_empty() {
            lines.push(div().h(px(8.0)).into_any_element());
            prev_blank = true;
            continue;
        }
        if let Some((name, desc)) = line.split_once(" - ") {
            lines.push(credit_entry(name.trim(), desc.trim()));
        } else if prev_blank {
            lines.push(credit_header(line));
        } else {
            lines.push(credit_plain(line));
        }
        prev_blank = false;
    }

    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .mx(px(16.0))
        .mb(px(12.0))
        .rounded(px(crate::theme::radius::SURFACE))
        .bg(Colors::surface_panel())
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .child(
            div()
                .flex_none()
                .px(px(14.0))
                .py(px(8.0))
                .border_b(px(1.0))
                .border_color(Colors::border_subtle())
                .text_size(px(10.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(Colors::text_muted())
                .child("CREDITS"),
        )
        .child(
            div()
                .id("about-credits")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .px(px(14.0))
                .py(px(10.0))
                .flex()
                .flex_col()
                .children(lines),
        )
}

fn credit_header(text: &str) -> gpui::AnyElement {
    div()
        .mt(px(4.0))
        .mb(px(2.0))
        .text_size(px(12.5))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(Colors::accent_primary())
        .child(text.to_string())
        .into_any_element()
}

fn credit_entry(name: &str, desc: &str) -> gpui::AnyElement {
    div()
        .flex()
        .flex_row()
        .items_baseline()
        .gap(px(6.0))
        .py(px(1.0))
        .text_size(px(11.5))
        .child(
            div()
                .flex_none()
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(Colors::text_primary())
                .child(name.to_string()),
        )
        .child(
            div()
                .min_w(px(0.0))
                .text_color(Colors::text_muted())
                .child(format!("— {desc}")),
        )
        .into_any_element()
}

fn credit_plain(text: &str) -> gpui::AnyElement {
    div()
        .py(px(1.0))
        .text_size(px(11.5))
        .text_color(Colors::text_secondary())
        .child(text.to_string())
        .into_any_element()
}

fn footer() -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .flex_none()
        .h(px(46.0))
        .px(px(16.0))
        .border_t(px(1.0))
        .border_color(Colors::border_subtle())
        .child(
            div()
                .text_size(px(10.5))
                .text_color(Colors::text_muted())
                .child("© Nawaphol Bunchuea (Arizkami)"),
        )
        .child(fb_button(
            "about-close",
            "Close",
            FbButtonKind::Primary,
            true,
            move |_, window, _cx| {
                window.remove_window();
            },
        ))
}

pub fn open_about_window(
    owner_bounds: Option<Bounds<gpui::Pixels>>,
    cx: &mut App,
) -> Result<WindowHandle<AboutWindow>, String> {
    let window_bounds = centered_window_bounds(
        owner_bounds,
        size(px(ABOUT_WINDOW_WIDTH), px(ABOUT_WINDOW_HEIGHT)),
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
        cx.new(AboutWindow::new)
    })
    .map_err(|error| error.to_string())
}

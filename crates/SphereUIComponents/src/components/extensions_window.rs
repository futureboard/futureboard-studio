//! Extensions — the community registry browser.
//!
//! Tag row (Plugins / Themes / Audio Extensions / Icons) over a list of registry
//! entries, each with a Download button. Only the sections the registry actually
//! serves list entries; the rest state why they are empty rather than showing
//! placeholder rows.
//!
//! Network and filesystem work runs on GPUI's background executor and reports
//! back through the entity, so `Render` only ever reads already-resolved state.
//! Installing a theme writes `<AppData>/Extensions/Themes/<slug>/theme.json`,
//! which is the same location [`crate::theme`] scans, so an installed theme
//! becomes selectable in Preferences without any extra wiring.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, size, AnyElement, App, AppContext, Bounds, Context, FocusHandle, InteractiveElement,
    IntoElement, MouseButton, ParentElement, Render, SharedString, StatefulInteractiveElement,
    Styled, Window, WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind,
};

use crate::components::title_bar::external_window_titlebar;
use crate::extensions_registry::{
    download_theme, fetch_extensions, is_theme_installed, ExtensionKind, RegistryItem,
};
use crate::theme::{self, Colors};
use crate::window_position::{apply_owner_display, centered_window_bounds};

pub const EXTENSIONS_WINDOW_WIDTH: f32 = 840.0;
pub const EXTENSIONS_WINDOW_HEIGHT: f32 = 560.0;
pub const EXTENSIONS_WINDOW_MIN_WIDTH: f32 = 520.0;
pub const EXTENSIONS_WINDOW_MIN_HEIGHT: f32 = 360.0;

const ROW_HEIGHT: f32 = 62.0;
const TAG_HEIGHT: f32 = 26.0;

/// Load state for one registry section.
#[derive(Clone)]
enum SectionState {
    Idle,
    Loading,
    Loaded(Vec<RegistryItem>),
    Failed(SharedString),
}

pub struct ExtensionsWindow {
    kind: ExtensionKind,
    sections: HashMap<&'static str, SectionState>,
    /// Slugs with a download in flight.
    downloading: HashSet<String>,
    /// Slugs already present in the themes directory.
    installed: HashSet<String>,
    status: Option<SharedString>,
    error: Option<SharedString>,
    on_close: Arc<dyn Fn(&mut Window, &mut App) + Send + Sync>,
    focus_handle: FocusHandle,
}

impl ExtensionsWindow {
    pub fn new(
        on_close: Arc<dyn Fn(&mut Window, &mut App) + Send + Sync>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            kind: ExtensionKind::Themes,
            sections: HashMap::new(),
            downloading: HashSet::new(),
            installed: HashSet::new(),
            status: None,
            error: None,
            on_close,
            focus_handle: cx.focus_handle(),
        };
        this.load_section(ExtensionKind::Themes, cx);
        this
    }

    fn state(&self, kind: ExtensionKind) -> SectionState {
        self.sections
            .get(kind.label())
            .cloned()
            .unwrap_or(SectionState::Idle)
    }

    fn select_kind(&mut self, kind: ExtensionKind, cx: &mut Context<Self>) {
        if self.kind == kind {
            return;
        }
        self.kind = kind;
        self.error = None;
        self.status = None;
        if matches!(self.state(kind), SectionState::Idle) {
            self.load_section(kind, cx);
        }
        cx.notify();
    }

    /// Fetches one section on the background executor. Sections the registry
    /// does not serve resolve to an empty list without issuing a request.
    fn load_section(&mut self, kind: ExtensionKind, cx: &mut Context<Self>) {
        self.sections.insert(kind.label(), SectionState::Loading);
        let entity = cx.entity().clone();
        cx.spawn(async move |_this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let items = fetch_extensions(kind)?;
                    // Resolve installed state on the same background hop — this
                    // stats the themes directory and must not run during render.
                    let installed: HashSet<String> = items
                        .iter()
                        .filter(|item| is_theme_installed(&item.slug))
                        .map(|item| item.slug.clone())
                        .collect();
                    Ok::<_, String>((items, installed))
                })
                .await;
            let _ = entity.update(cx, |this, cx| {
                match result {
                    Ok((items, installed)) => {
                        this.installed.extend(installed);
                        this.sections
                            .insert(kind.label(), SectionState::Loaded(items));
                    }
                    Err(error) => {
                        this.sections.insert(
                            kind.label(),
                            SectionState::Failed(SharedString::from(error)),
                        );
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        self.status = None;
        let kind = self.kind;
        self.load_section(kind, cx);
        cx.notify();
    }

    /// Downloads and installs one entry. The registry generates the document
    /// from its database per request, so the installed file always matches the
    /// current theme format.
    fn download(&mut self, item: &RegistryItem, cx: &mut Context<Self>) {
        if self.downloading.contains(&item.slug) {
            return;
        }
        self.downloading.insert(item.slug.clone());
        self.error = None;
        self.status = Some(SharedString::from(format!("Downloading {}…", item.name)));
        cx.notify();

        let slug = item.slug.clone();
        let name = item.name.clone();
        let entity = cx.entity().clone();
        cx.spawn(async move |_this, cx| {
            let slug_for_task = slug.clone();
            let result = cx
                .background_executor()
                .spawn(async move { download_theme(&slug_for_task) })
                .await;
            let _ = entity.update(cx, |this, cx| {
                this.downloading.remove(&slug);
                match result {
                    Ok((path, _theme_id)) => {
                        this.installed.insert(slug.clone());
                        this.status = Some(SharedString::from(format!(
                            "Installed {name} — press Apply to wear it."
                        )));
                        eprintln!("[extensions] installed theme {slug} -> {}", path.display());
                    }
                    Err(error) => {
                        this.status = None;
                        this.error = Some(SharedString::from(error));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn tag_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .px(px(12.0))
            .py(px(8.0))
            .border_b(px(1.0))
            .border_color(Colors::border_subtle())
            .bg(Colors::surface_panel());

        for kind in ExtensionKind::ALL {
            let selected = kind == self.kind;
            row = row.child(
                div()
                    .id(SharedString::from(format!(
                        "extensions-tag-{}",
                        kind.label()
                    )))
                    .flex()
                    .items_center()
                    .h(px(TAG_HEIGHT))
                    .px(px(10.0))
                    .rounded(px(crate::theme::radius::CONTROL_SM))
                    .cursor(gpui::CursorStyle::PointingHand)
                    .bg(if selected {
                        Colors::accent_soft()
                    } else {
                        Colors::surface_raised()
                    })
                    .border(px(1.0))
                    .border_color(if selected {
                        Colors::accent_primary()
                    } else {
                        Colors::border_subtle()
                    })
                    .text_size(px(11.5))
                    .text_color(if selected {
                        Colors::accent_primary()
                    } else {
                        Colors::text_secondary()
                    })
                    .hover(|style| style.bg(Colors::surface_hover()))
                    .on_click(cx.listener(move |this, _event, _window, cx| {
                        this.select_kind(kind, cx);
                    }))
                    .child(kind.label()),
            );
        }

        row.child(div().flex_1()).child(
            div()
                .id("extensions-refresh")
                .flex()
                .items_center()
                .h(px(TAG_HEIGHT))
                .px(px(10.0))
                .rounded(px(crate::theme::radius::CONTROL_SM))
                .cursor(gpui::CursorStyle::PointingHand)
                .bg(Colors::surface_raised())
                .border(px(1.0))
                .border_color(Colors::border_subtle())
                .text_size(px(11.5))
                .text_color(Colors::text_secondary())
                .hover(|style| style.bg(Colors::surface_hover()))
                .on_click(cx.listener(|this, _event, _window, cx| this.refresh(cx)))
                .child("Refresh"),
        )
    }

    fn item_row(&self, item: &RegistryItem, cx: &mut Context<Self>) -> impl IntoElement {
        let installed = self.installed.contains(&item.slug);
        let downloading = self.downloading.contains(&item.slug);

        let mut swatches = div().flex().flex_row().gap(px(2.0)).flex_shrink_0();
        for color in item.preview.swatches() {
            swatches = swatches.child(
                div()
                    .w(px(9.0))
                    .h(px(26.0))
                    .rounded(px(crate::theme::radius::MICRO))
                    .bg(color),
            );
        }

        let subtitle = {
            let author = if item.author.trim().is_empty() {
                "Unknown author".to_string()
            } else {
                item.author.clone()
            };
            let downloads = if item.downloads == 1 {
                "1 download".to_string()
            } else {
                format!("{} downloads", item.downloads)
            };
            if item.version.trim().is_empty() {
                format!("{author} · {downloads}")
            } else {
                format!("{author} · v{} · {downloads}", item.version)
            }
        };

        let action = {
            let label = if downloading {
                "Downloading…"
            } else if installed {
                "Reinstall"
            } else {
                "Download"
            };
            let item_for_click = item.clone();
            div()
                .id(SharedString::from(format!(
                    "extensions-download-{}",
                    item.slug
                )))
                .flex()
                .items_center()
                .justify_center()
                .h(px(26.0))
                .px(px(12.0))
                .flex_shrink_0()
                .rounded(px(crate::theme::radius::CONTROL_SM))
                .bg(if downloading {
                    Colors::surface_raised()
                } else {
                    Colors::accent_soft()
                })
                .border(px(1.0))
                .border_color(if downloading {
                    Colors::border_subtle()
                } else {
                    Colors::accent_primary()
                })
                .text_size(px(11.5))
                .text_color(if downloading {
                    Colors::text_muted()
                } else {
                    Colors::accent_primary()
                })
                .when(!downloading, |element| {
                    element
                        .cursor(gpui::CursorStyle::PointingHand)
                        .hover(|style| style.bg(Colors::accent_muted()))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _event, _window, cx| {
                                this.download(&item_for_click, cx);
                            }),
                        )
                })
                .child(label)
        };

        // Apply, next to Reinstall, for a theme already on disk.
        //
        // Installing wrote a file and told the user to go and find it in
        // Preferences — a second trip through a second window to finish a job
        // they had just asked for. The theme is downloaded *because* they want
        // to look at it, so the button that wears it is on the row that
        // installed it.
        let apply = (installed && self.kind == ExtensionKind::Themes).then(|| {
            let active = crate::theme::active_theme_summary().0;
            let theme_id = crate::extensions_registry::installed_theme_id(&item.slug);
            let is_active = theme_id.as_deref() == Some(active.as_str());
            let name = item.name.clone();
            div()
                .id(SharedString::from(format!(
                    "extensions-apply-{}",
                    item.slug
                )))
                .flex()
                .items_center()
                .justify_center()
                .h(px(26.0))
                .px(px(12.0))
                .flex_shrink_0()
                .rounded(px(crate::theme::radius::CONTROL_SM))
                .bg(if is_active {
                    Colors::surface_raised()
                } else {
                    Colors::accent_primary()
                })
                .text_size(px(11.5))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(if is_active {
                    Colors::text_muted()
                } else {
                    Colors::on_accent()
                })
                .when(!is_active, |element| {
                    element
                        .cursor(gpui::CursorStyle::PointingHand)
                        .hover(|style| style.bg(Colors::accent_primary_hover()))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _event, _window, cx| {
                                let Some(id) = theme_id.clone() else {
                                    this.error = Some(SharedString::from(
                                        "That theme file has no id to select.",
                                    ));
                                    cx.notify();
                                    return;
                                };
                                if crate::theme::activate_theme_by_id(&id) {
                                    this.error = None;
                                    this.status =
                                        Some(SharedString::from(format!("Applied {name}.")));
                                    // Every window is painted from these
                                    // colours, so all of them repaint — not just
                                    // the one the button is in.
                                    cx.refresh_windows();
                                } else {
                                    this.error = Some(SharedString::from(format!(
                                        "Could not apply {name}."
                                    )));
                                }
                                cx.notify();
                            }),
                        )
                })
                .child(if is_active { "Applied" } else { "Apply" })
        });

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(12.0))
            .h(px(ROW_HEIGHT))
            .px(px(12.0))
            .border_b(px(1.0))
            .border_color(Colors::border_subtle())
            .child(swatches)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.0))
                            .child(
                                div()
                                    .truncate()
                                    .min_w_0()
                                    .text_size(px(12.5))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(Colors::text_primary())
                                    .child(item.name.clone()),
                            )
                            .when(installed, |row| {
                                row.child(
                                    div()
                                        .flex_none()
                                        .rounded(px(crate::theme::radius::CONTROL))
                                        .bg(Colors::surface_badge())
                                        .px(px(6.0))
                                        .py(px(1.0))
                                        .text_size(px(9.0))
                                        .text_color(Colors::status_success())
                                        .child("Installed"),
                                )
                            }),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(px(10.5))
                            .text_color(Colors::text_muted())
                            .child(subtitle),
                    )
                    .when(!item.description.trim().is_empty(), |column| {
                        column.child(
                            div()
                                .truncate()
                                .text_size(px(10.5))
                                .text_color(Colors::text_secondary())
                                .child(item.description.clone()),
                        )
                    }),
            )
            .children(apply)
            .child(action)
    }

    fn body(&self, cx: &mut Context<Self>) -> AnyElement {
        let kind = self.kind;

        if !kind.is_available() {
            return div()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .flex_1()
                .min_h_0()
                .gap(px(6.0))
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(Colors::text_secondary())
                        .child(format!("No {} yet", kind.label().to_lowercase())),
                )
                .child(
                    div()
                        .text_size(px(10.5))
                        .text_color(Colors::text_muted())
                        .child(kind.unavailable_note()),
                )
                .into_any_element();
        }

        match self.state(kind) {
            SectionState::Idle | SectionState::Loading => {
                centered_note("Loading extensions…").into_any_element()
            }
            SectionState::Failed(error) => centered_note(error).into_any_element(),
            SectionState::Loaded(items) if items.is_empty() => {
                centered_note("The registry has no published extensions in this section yet.")
                    .into_any_element()
            }
            SectionState::Loaded(items) => {
                let mut list = div().flex().flex_col();
                for item in &items {
                    list = list.child(self.item_row(item, cx));
                }
                div()
                    .id("extensions-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(list)
                    .into_any_element()
            }
        }
    }
}

fn centered_note(text: impl Into<SharedString>) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .justify_center()
        .flex_1()
        .min_h_0()
        .text_size(px(11.5))
        .text_color(Colors::text_muted())
        .child(text.into())
}

impl Render for ExtensionsWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.focus_handle.is_focused(window) {
            self.focus_handle.focus(window, cx);
        }

        let on_close = self.on_close.clone();
        let footer_text: Option<(SharedString, gpui::Rgba)> = self
            .error
            .clone()
            .map(|error| (error, Colors::status_error()))
            .or_else(|| {
                self.status
                    .clone()
                    .map(|status| (status, Colors::text_secondary()))
            });

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(Colors::surface_window())
            .text_color(Colors::text_primary())
            .font(theme::ui_font())
            .overflow_hidden()
            .track_focus(&self.focus_handle)
            .on_key_down(
                cx.listener(|_this, event: &gpui::KeyDownEvent, window, _cx| {
                    if event.keystroke.key.as_str() == "escape" {
                        window.remove_window();
                    }
                }),
            )
            .child(external_window_titlebar(
                "Extensions",
                "extensions-window-close",
                move |window, cx| {
                    on_close(window, cx);
                    window.remove_window();
                },
            ))
            .child(self.tag_row(cx))
            .child(self.body(cx))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .h(px(26.0))
                    .px(px(12.0))
                    .border_t(px(1.0))
                    .border_color(Colors::border_subtle())
                    .bg(Colors::surface_panel())
                    .text_size(px(10.0))
                    .child(match footer_text {
                        Some((text, color)) => div().text_color(color).truncate().child(text),
                        None => div().text_color(Colors::text_muted()).child(
                            "Downloads install to Extensions/Themes and appear in Preferences.",
                        ),
                    }),
            )
    }
}

pub fn open_extensions_window(
    owner_bounds: Option<Bounds<gpui::Pixels>>,
    on_close: Arc<dyn Fn(&mut Window, &mut App) + Send + Sync>,
    cx: &mut App,
) -> Result<WindowHandle<ExtensionsWindow>, String> {
    let window_bounds = centered_window_bounds(
        owner_bounds,
        size(px(EXTENSIONS_WINDOW_WIDTH), px(EXTENSIONS_WINDOW_HEIGHT)),
        cx,
    );
    let mut options = crate::platform_chrome::external_dialog_window_options_partial();
    options.window_bounds = Some(WindowBounds::Windowed(window_bounds));
    options.kind = WindowKind::Dialog;
    options.is_resizable = true;
    options.is_minimizable = true;
    options.window_background = WindowBackgroundAppearance::Opaque;
    options.window_min_size = Some(size(
        px(EXTENSIONS_WINDOW_MIN_WIDTH),
        px(EXTENSIONS_WINDOW_MIN_HEIGHT),
    ));
    apply_owner_display(&mut options, owner_bounds, cx);

    crate::dialog_frame::open_dialog_window(cx, options, move |_window, cx| {
        cx.new(|cx| ExtensionsWindow::new(on_close, cx))
    })
    .map_err(|error| error.to_string())
}

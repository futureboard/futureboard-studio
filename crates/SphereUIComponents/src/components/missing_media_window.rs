//! Missing Plug-ins and Files — what a project refers to that is not where it
//! says, one row each, and a way to point it at the right place.
//!
//! ```txt
//! summary                                   ← how many are still missing
//! Type │ Name │ Used on │ Expected location │ Status │ Action
//! notes                                     ← load problems that are not a file
//! Search Folder…                     Close
//! ```
//!
//! The rows are the studio's: [`MissingMediaCallbacks::collect`] builds them
//! from the project, and the window asks again after every relink and on a
//! slow timer while open (a relinked plug-in loads in the host, so its status
//! moves on its own). The window holds no project state of its own, and its
//! render does no file-system work.

use std::sync::Arc;
use std::time::Duration;

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, size, uniform_list, App, AppContext, Bounds, Context, FocusHandle, InteractiveElement,
    IntoElement, KeyDownEvent, ParentElement, Render, Rgba, Styled, UniformListScrollHandle,
    Window, WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind,
};

use crate::components::controls::{fb_button, FbButtonKind};
use crate::components::scroll_thumb::vertical_scrollbar_thumb;
use crate::components::title_bar::external_window_titlebar;
use crate::i18n::I18n;
use crate::theme::{self, size as sizes, space, typography, Colors};

pub const MISSING_MEDIA_WINDOW_WIDTH: f32 = 1040.0;
pub const MISSING_MEDIA_WINDOW_HEIGHT: f32 = 560.0;
const MISSING_MEDIA_WINDOW_MIN_WIDTH: f32 = 780.0;
const MISSING_MEDIA_WINDOW_MIN_HEIGHT: f32 = 380.0;

/// How often an open window asks for the rows again.
const REFRESH_INTERVAL: Duration = Duration::from_millis(750);

const COL_TYPE_W: f32 = 76.0;
/// Name and Used on are fixed so the path, where two places differ, gets
/// whatever room the window has.
const COL_NAME_W: f32 = 170.0;
const COL_USED_W: f32 = 150.0;
const COL_PATH_MIN: f32 = 180.0;
const COL_STATUS_W: f32 = 112.0;
const COL_ACTION_W: f32 = 200.0;
const COL_GAP: f32 = space::BASE;
const ROW_PAD_X: f32 = space::LOOSE;
/// Two lines of text (a value and its detail) and the row's buttons.
const ROW_HEIGHT: f32 = sizes::PROMINENT + space::SNUG * 2.0;

/// What a row refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissingKind {
    Plugin,
    Audio,
}

/// Where a row stands now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MissingStatus {
    /// Not where the project says.
    Missing,
    /// Relinked; the plug-in is loading.
    Loading,
    /// The file is there and (for a plug-in) loaded.
    Found,
    /// The file is there but the plug-in did not load, and why.
    Failed(String),
}

/// One missing plug-in (every slot using it) or audio file (every clip on it).
#[derive(Debug, Clone, PartialEq)]
pub struct MissingRow {
    /// Stable for the window's life: what relink callbacks are given.
    pub id: String,
    pub kind: MissingKind,
    pub name: String,
    /// Plug-in format ("VST3", "CLAP"…), or the file type for audio.
    pub detail: String,
    /// The tracks it is used on, and how many places.
    pub used_on: String,
    /// Where the project expects it, or where it was relinked to.
    pub path: String,
    pub status: MissingStatus,
    /// Where the plug-in is installed by its own id, when that differs from
    /// `path`: the scan found it somewhere else.
    pub installed_at: Option<String>,
}

type RowCb = Arc<dyn Fn(&str, &mut Window, &mut App) + 'static>;

/// How the window reaches the studio.
#[derive(Clone)]
pub struct MissingMediaCallbacks {
    /// The rows as they stand now.
    pub collect: Arc<dyn Fn(&mut App) -> Vec<MissingRow> + 'static>,
    /// Pick the file for a row.
    pub relocate: RowCb,
    /// Use the installed plug-in in [`MissingRow::installed_at`].
    pub use_installed: RowCb,
    /// Pick a folder and relink every missing row whose file is in it.
    pub search_folder: Arc<dyn Fn(&mut Window, &mut App) + 'static>,
}

pub struct MissingMediaWindow {
    rows: Vec<MissingRow>,
    /// Load problems that are not a missing file (a plug-in that timed out,
    /// a host that went away), said once under the table.
    notes: Vec<String>,
    /// What the last folder search found, said in the summary.
    status: Option<String>,
    callbacks: MissingMediaCallbacks,
    list_scroll: UniformListScrollHandle,
    focus_handle: FocusHandle,
}

impl MissingMediaWindow {
    pub(crate) fn new(
        rows: Vec<MissingRow>,
        callbacks: MissingMediaCallbacks,
        notes: Vec<String>,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(REFRESH_INTERVAL).await;
            if crate::shutdown::ShutdownState::global().is_shutting_down() {
                break;
            }
            if this.update(cx, |this, cx| this.refresh(cx)).is_err() {
                break;
            }
        })
        .detach();
        Self {
            rows,
            notes,
            status: None,
            callbacks,
            list_scroll: UniformListScrollHandle::new(),
            focus_handle: cx.focus_handle(),
        }
    }

    /// Ask the studio for the rows again. Only from the window's own timer:
    /// the studio pushes rows with [`Self::set_rows`] from inside its own
    /// updates, where reading it back would re-enter it.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let rows = (self.callbacks.collect)(cx);
        self.set_rows(rows, cx);
    }

    /// The rows as the studio has them now; repaints only when they changed.
    pub fn set_rows(&mut self, rows: Vec<MissingRow>, cx: &mut Context<Self>) {
        if rows != self.rows {
            self.rows = rows;
            cx.notify();
        }
    }

    /// Say what a folder search did.
    pub fn set_status(&mut self, status: impl Into<String>, cx: &mut Context<Self>) {
        self.status = Some(status.into());
        cx.notify();
    }

    fn handle_key(&mut self, event: &KeyDownEvent, window: &mut Window, _cx: &mut Context<Self>) {
        if event.keystroke.key.as_str() == "escape" {
            window.remove_window();
        }
    }

    fn summary(&self, i18n: I18n) -> (Rgba, String) {
        let missing = |kind| {
            self.rows
                .iter()
                .filter(|row| row.kind == kind && row.status == MissingStatus::Missing)
                .count()
        };
        let (plugins, files) = (missing(MissingKind::Plugin), missing(MissingKind::Audio));
        let failed = self
            .rows
            .iter()
            .filter(|row| matches!(row.status, MissingStatus::Failed(_)))
            .count();
        if plugins + files == 0 && failed == 0 {
            return (
                Colors::status_success(),
                i18n.tr_or(
                    "missing-media.summary.all-found",
                    "Everything this project uses is found.",
                ),
            );
        }
        let mut parts = Vec::new();
        if plugins > 0 {
            parts.push(tr_vars_or(
                i18n,
                "missing-media.summary.plugins",
                plural(
                    plugins,
                    "{ $count } plug-in missing",
                    "{ $count } plug-ins missing",
                ),
                &[("count", plugins.to_string())],
            ));
        }
        if files > 0 {
            parts.push(tr_vars_or(
                i18n,
                "missing-media.summary.files",
                plural(
                    files,
                    "{ $count } audio file missing",
                    "{ $count } audio files missing",
                ),
                &[("count", files.to_string())],
            ));
        }
        if failed > 0 {
            parts.push(tr_vars_or(
                i18n,
                "missing-media.summary.failed",
                plural(
                    failed,
                    "{ $count } plug-in failed to load",
                    "{ $count } plug-ins failed to load",
                ),
                &[("count", failed.to_string())],
            ));
        }
        (Colors::status_warning(), parts.join(" · "))
    }
}

impl Render for MissingMediaWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let i18n = I18n::from_app(cx);
        let target = cx.entity().clone();
        let (tone, summary) = self.summary(i18n);
        let any_missing = self
            .rows
            .iter()
            .any(|row| row.status == MissingStatus::Missing);

        let header = div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .gap(px(space::HAIR))
            .px(px(space::LOOSE))
            .pt(px(space::LOOSE))
            .pb(px(space::BASE))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(space::SNUG))
                    .child(
                        div()
                            .w(px(3.0))
                            .h(px(sizes::DEFAULT * 0.6))
                            .flex_shrink_0()
                            .bg(tone),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(px(typography::UI_MD))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(Colors::text_primary())
                            .child(summary),
                    ),
            )
            .child(
                div()
                    .truncate()
                    .text_size(px(typography::UI_XS))
                    .text_color(Colors::text_muted())
                    .child(self.status.clone().unwrap_or_else(|| {
                        i18n.tr_or(
                            "missing-media.hint",
                            "Relocate points a row at its file. A relinked plug-in keeps its \
                             settings and automation.",
                        )
                    })),
            );

        let notes = (!self.notes.is_empty()).then(|| {
            div()
                .flex()
                .flex_col()
                .flex_shrink_0()
                .gap(px(space::HAIR))
                .px(px(space::LOOSE))
                .py(px(space::SNUG))
                .border_t(px(1.0))
                .border_color(Colors::divider())
                .children(self.notes.iter().map(|note| {
                    div()
                        .truncate()
                        .text_size(px(typography::UI_XS))
                        .text_color(Colors::text_secondary())
                        .child(format!("• {note}"))
                }))
        });

        let footer = div()
            .flex()
            .flex_row()
            .flex_shrink_0()
            .items_center()
            .justify_between()
            .gap(px(space::BASE))
            .px(px(space::LOOSE))
            .py(px(space::BASE))
            .border_t(px(1.0))
            .border_color(Colors::divider())
            .child(fb_button(
                "missing-media-search-folder",
                i18n.tr_or("missing-media.action.search-folder", "Search Folder…"),
                FbButtonKind::Default,
                any_missing,
                {
                    let search = self.callbacks.search_folder.clone();
                    move |_, window, cx| search(window, cx)
                },
            ))
            .child(fb_button(
                "missing-media-close",
                i18n.tr_or("missing-media.action.close", "Close"),
                FbButtonKind::Primary,
                true,
                |_, window, _cx| window.remove_window(),
            ));

        div()
            .flex()
            .flex_col()
            .size_full()
            .font(theme::ui_font())
            .bg(Colors::surface_window())
            .overflow_hidden()
            .track_focus(&self.focus_handle)
            .capture_key_down({
                let target = target.clone();
                move |event, window, cx| {
                    let _ = target.update(cx, |this, cx| this.handle_key(event, window, cx));
                }
            })
            .child(external_window_titlebar(
                i18n.tr_or("missing-media.title", "Missing Plug-ins and Files"),
                "missing-media-window-close",
                |window, _cx| window.remove_window(),
            ))
            .child(header)
            .child(table(
                Arc::new(self.rows.clone()),
                &self.callbacks,
                &self.list_scroll,
                i18n,
            ))
            .children(notes)
            .child(footer)
    }
}

/// The English fallback for `count`: one or many.
fn plural(count: usize, one: &'static str, many: &'static str) -> &'static str {
    if count == 1 {
        one
    } else {
        many
    }
}

fn tr_vars_or(i18n: I18n, key: &str, fallback: &str, vars: &[(&str, String)]) -> String {
    let mut text = i18n.tr_or(key, fallback);
    for (name, value) in vars {
        text = text.replace(&format!("{{ ${name} }}"), value);
    }
    text
}

fn header_cell(label: String) -> impl IntoElement {
    div()
        .truncate()
        .text_size(px(typography::DENSE_CAPTION))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(Colors::text_faint())
        .child(label)
}

fn table(
    rows: Arc<Vec<MissingRow>>,
    callbacks: &MissingMediaCallbacks,
    list_scroll: &UniformListScrollHandle,
    i18n: I18n,
) -> impl IntoElement {
    let header = div()
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .flex_shrink_0()
        .h(px(sizes::ROW_DENSE))
        .px(px(ROW_PAD_X))
        .gap(px(COL_GAP))
        .border_y(px(1.0))
        .border_color(Colors::divider())
        .bg(Colors::surface_base())
        .child(
            div()
                .w(px(COL_TYPE_W))
                .flex_shrink_0()
                .child(header_cell(i18n.tr_or("missing-media.column.type", "Type"))),
        )
        .child(
            div()
                .w(px(COL_NAME_W))
                .flex_shrink_0()
                .child(header_cell(i18n.tr_or("missing-media.column.name", "Name"))),
        )
        .child(div().w(px(COL_USED_W)).flex_shrink_0().child(header_cell(
            i18n.tr_or("missing-media.column.used-on", "Used on"),
        )))
        .child(div().flex_1().min_w(px(COL_PATH_MIN)).child(header_cell(
            i18n.tr_or("missing-media.column.location", "Expected location"),
        )))
        .child(div().w(px(COL_STATUS_W)).flex_shrink_0().child(header_cell(
            i18n.tr_or("missing-media.column.status", "Status"),
        )))
        .child(div().w(px(COL_ACTION_W)).flex_shrink_0());

    let body = if rows.is_empty() {
        div()
            .flex()
            .items_center()
            .justify_center()
            .size_full()
            .text_size(px(typography::UI_XS))
            .text_color(Colors::text_faint())
            .child(i18n.tr_or(
                "missing-media.empty",
                "Nothing is missing: every plug-in and audio file is where the project says.",
            ))
            .into_any_element()
    } else {
        let relocate = callbacks.relocate.clone();
        let use_installed = callbacks.use_installed.clone();
        let thumb = list_scroll.0.borrow().base_handle.clone();
        let count = rows.len();
        let list = uniform_list("missing-media-list", count, move |range, _window, _cx| {
            range
                .filter_map(|index| {
                    let row = rows.get(index)?;
                    Some(
                        table_row(index, row, relocate.clone(), use_installed.clone(), i18n)
                            .into_any_element(),
                    )
                })
                .collect::<Vec<_>>()
        })
        .track_scroll(list_scroll)
        .size_full();
        div()
            .relative()
            .size_full()
            .child(list)
            .child(vertical_scrollbar_thumb(thumb))
            .into_any_element()
    };

    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h(px(0.0))
        .bg(Colors::surface_base())
        .child(header)
        .child(div().flex_1().min_h(px(0.0)).child(body))
}

fn table_row(
    index: usize,
    row: &MissingRow,
    relocate: RowCb,
    use_installed: RowCb,
    i18n: I18n,
) -> impl IntoElement {
    let rest = Colors::surface_base();
    let hover = Colors::composite(rest, Colors::state_hover());
    let open = matches!(
        row.status,
        MissingStatus::Missing | MissingStatus::Failed(_)
    );
    let kind_label = match row.kind {
        MissingKind::Plugin => i18n.tr_or("missing-media.kind.plugin", "Plug-in"),
        MissingKind::Audio => i18n.tr_or("missing-media.kind.audio", "Audio"),
    };
    let path_color = if row.status == MissingStatus::Missing {
        Colors::text_muted()
    } else {
        Colors::text_secondary()
    };
    let installed = row.installed_at.clone().filter(|_| open);

    let actions = div()
        .w(px(COL_ACTION_W))
        .flex_shrink_0()
        .flex()
        .flex_row()
        .items_center()
        .justify_end()
        .gap(px(space::SNUG))
        .when_some(installed, |actions, _| {
            let id = row.id.clone();
            let use_installed = use_installed.clone();
            actions.child(fb_button(
                ("missing-media-use", index),
                i18n.tr_or("missing-media.action.use-installed", "Use Installed"),
                FbButtonKind::Default,
                true,
                move |_, window, cx| use_installed(&id, window, cx),
            ))
        })
        .when(open, |actions| {
            let id = row.id.clone();
            let relocate = relocate.clone();
            actions.child(fb_button(
                ("missing-media-relocate", index),
                i18n.tr_or("missing-media.action.relocate", "Relocate…"),
                FbButtonKind::Default,
                true,
                move |_, window, cx| relocate(&id, window, cx),
            ))
        });

    div()
        .id(("missing-media-row", index))
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(COL_GAP))
        .h(px(ROW_HEIGHT))
        .px(px(ROW_PAD_X))
        .bg(rest)
        .hover(move |style| style.bg(hover))
        .border_b(px(1.0))
        .border_color(Colors::divider())
        .child(
            div()
                .w(px(COL_TYPE_W))
                .flex_shrink_0()
                .flex()
                .flex_col()
                .child(
                    div()
                        .truncate()
                        .text_size(px(typography::UI_XS))
                        .text_color(Colors::text_secondary())
                        .child(kind_label),
                )
                .child(
                    div()
                        .truncate()
                        .text_size(px(typography::DENSE_CAPTION))
                        .text_color(Colors::text_faint())
                        .child(row.detail.clone()),
                ),
        )
        .child(
            div()
                .w(px(COL_NAME_W))
                .flex_shrink_0()
                .truncate()
                .text_size(px(typography::UI_SM))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(Colors::text_primary())
                .child(row.name.clone()),
        )
        .child(
            div()
                .w(px(COL_USED_W))
                .flex_shrink_0()
                .truncate()
                .text_size(px(typography::UI_XS))
                .text_color(Colors::text_muted())
                .child(row.used_on.clone()),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(COL_PATH_MIN))
                .flex()
                .flex_col()
                .child(
                    div()
                        .truncate()
                        .text_size(px(typography::UI_XS))
                        .text_color(path_color)
                        .child(row.path.clone()),
                )
                .when_some(row.installed_at.clone().filter(|_| open), |cell, at| {
                    cell.child(
                        div()
                            .truncate()
                            .text_size(px(typography::DENSE_CAPTION))
                            .text_color(Colors::text_faint())
                            .child(tr_vars_or(
                                i18n,
                                "missing-media.installed-at",
                                "Installed at { $path }",
                                &[("path", at)],
                            )),
                    )
                }),
        )
        .child(
            div()
                .w(px(COL_STATUS_W))
                .flex_shrink_0()
                .child(status_chip(&row.status, i18n)),
        )
        .child(actions)
}

/// A dot and a word, so the status is not carried by hue alone; a failure
/// says why on hover.
fn status_chip(status: &MissingStatus, i18n: I18n) -> impl IntoElement {
    let (tone, label) = match status {
        MissingStatus::Missing => (
            Colors::status_error(),
            i18n.tr_or("missing-media.status.missing", "Missing"),
        ),
        MissingStatus::Loading => (
            Colors::status_warning(),
            i18n.tr_or("missing-media.status.loading", "Loading…"),
        ),
        MissingStatus::Found => (
            Colors::status_success(),
            i18n.tr_or("missing-media.status.found", "Found"),
        ),
        MissingStatus::Failed(_) => (
            Colors::status_error(),
            i18n.tr_or("missing-media.status.failed", "Failed to load"),
        ),
    };
    let reason = match status {
        MissingStatus::Failed(reason) => Some(reason.clone()),
        _ => None,
    };
    div()
        .flex()
        .flex_col()
        .min_w(px(0.0))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::SNUG))
                .child(
                    div()
                        .w(px(6.0))
                        .h(px(6.0))
                        .flex_shrink_0()
                        .rounded(px(theme::radius::PILL))
                        .bg(tone),
                )
                .child(
                    div()
                        .truncate()
                        .text_size(px(typography::UI_XS))
                        .text_color(Colors::text_secondary())
                        .child(label),
                ),
        )
        .when_some(reason, |chip, reason| {
            chip.child(
                div()
                    .truncate()
                    .text_size(px(typography::DENSE_CAPTION))
                    .text_color(Colors::text_faint())
                    .child(reason),
            )
        })
}

/// Open the window over `owner_bounds` on `rows`. `notes` are load problems
/// that are not a missing file, listed under the table.
pub fn open_missing_media_window(
    owner_bounds: Option<Bounds<gpui::Pixels>>,
    rows: Vec<MissingRow>,
    callbacks: MissingMediaCallbacks,
    notes: Vec<String>,
    cx: &mut App,
) -> Result<WindowHandle<MissingMediaWindow>, String> {
    let window_bounds = crate::window_position::centered_window_bounds(
        owner_bounds,
        size(
            px(MISSING_MEDIA_WINDOW_WIDTH),
            px(MISSING_MEDIA_WINDOW_HEIGHT),
        ),
        cx,
    );
    let mut options = crate::platform_chrome::external_dialog_window_options_partial();
    options.window_bounds = Some(WindowBounds::Windowed(window_bounds));
    options.kind = WindowKind::Dialog;
    options.is_resizable = true;
    options.is_minimizable = false;
    options.window_background = WindowBackgroundAppearance::Transparent;
    options.window_min_size = Some(size(
        px(MISSING_MEDIA_WINDOW_MIN_WIDTH),
        px(MISSING_MEDIA_WINDOW_MIN_HEIGHT),
    ));
    crate::window_position::apply_owner_display(&mut options, owner_bounds, cx);

    crate::dialog_frame::open_dialog_window(cx, options, move |window, cx| {
        let view = cx.new(|cx| MissingMediaWindow::new(rows, callbacks, notes, cx));
        let focus = view.read(cx).focus_handle.clone();
        focus.focus(window, cx);
        view
    })
    .map_err(|error| error.to_string())
}

//! Browser sidebar — left dock of the studio shell.
//!
//! A dense, grouped DAW asset browser. The navigation model lives in
//! `file_browser.rs`; this module is pure presentation.
//!
//! Region contract:
//!
//! * **Layout owner** — the shell's main row (`studio_render`), which gives
//!   this element a fixed [`SIDEBAR_WIDTH`]. The width is load-bearing for the
//!   arrangement's coordinate transform (`panel_origin_x`), so it is not
//!   resizable.
//! * **Scroll owner and clip owner** — the single `uniform_list`. Nothing else
//!   in the sidebar scrolls or clips, and the scrollbar thumb is a sibling
//!   overlay inside the list's `relative()` wrapper.
//! * **Focus owner** — the tree focus handle supplied by the layout. Rows take
//!   it on click, so arrows, Enter and type-ahead work after a mouse
//!   selection instead of only while the search field is focused.
//! * **Row geometry** — every row kind is exactly [`ROW_H`] tall.
//!   `gpui::uniform_list` measures one row and multiplies, so a row of a
//!   different height desynchronizes the scroll math.
//!
//! Visual contract: rows are full-bleed and square; rest, hover, pressed and
//! selected all paint on the *same* inset backplate, so a row never changes
//! shape between states. Every dimension comes from `crate::theme`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::prelude::FluentBuilder;
use gpui::{
    canvas, div, fill, point, px, svg, uniform_list, App, AppContext, Bounds, Div, Empty,
    FocusHandle, InteractiveElement, IntoElement, ParentElement, Pixels, Render, Rgba, Role,
    Stateful, StatefulInteractiveElement, Styled, Toggled, UniformListScrollHandle, Window,
};

use crate::assets;
use crate::components::controls::{fb_shortcut_hint, fb_tooltip};
use crate::components::file_browser::{
    format_size, BrowserCrumb, BrowserIcon, BrowserNodeKind, BrowserVisibleNode, FileBrowserState,
    MAX_VISUAL_DEPTH,
};
use crate::components::scroll_thumb::vertical_scrollbar_thumb;
use crate::components::text_input::{
    text_field_with_callbacks, TextInputCallbacks, TextInputState,
};
use crate::components::timeline::waveform_cache;
use crate::i18n::I18n;
use crate::theme::{elevation, radius, size, space, typography, Colors};
use DirectAudio::AUDITION_PREVIEW_SECONDS;

pub const SIDEBAR_WIDTH: f32 = 272.0;

/// Row height for every row kind. `size::ROW_DENSE` is documented in the theme
/// as "Browser tree rows. Must stay exact" precisely because `uniform_list`
/// derives its scroll window from one measured row.
const ROW_H: f32 = size::ROW_DENSE;
/// Per-depth indent. Smaller than a generic file explorer so deep trees still
/// fit the fixed sidebar width.
const INDENT: f32 = space::LOOSE;
/// Disclosure column width — keeps icons aligned across rows whether or not the
/// row can open.
const DISCLOSURE_W: f32 = space::LOOSE;
/// Horizontal inset of the row backplate inside the full-bleed row. Only the
/// backplate rounds; the row itself stays square (DESIGN: full-width list rows).
const PLATE_INSET_X: f32 = space::TIGHT;
/// Vertical inset, which leaves an 18 px plate inside a 22 px row — the 16–20 px
/// band, hence `radius::CONTROL_SM`.
const PLATE_INSET_Y: f32 = space::HAIR;
/// Content padding inside the backplate.
const PLATE_PAD_X: f32 = space::TIGHT;
/// Row glyphs share the label's optical size so icon and text keep one baseline.
const GLYPH: f32 = typography::UI_XS;
/// Disclosure chevron — one step quieter than the row glyph.
const CHEVRON: f32 = typography::DENSE_CAPTION;
/// Preview waveform height. The arrangement's minimum audio lane height is the
/// smallest height this system treats a waveform as readable at.
const WAVEFORM_H: f32 = size::TRACK_ROW_MIN;

pub type ActivateFileCb = Arc<dyn Fn(&PathBuf, &mut Window, &mut App) + 'static>;
pub type SelectEntryCb = Arc<dyn Fn(&PathBuf, &mut Window, &mut App) + 'static>;
pub type ToggleNodeCb = Arc<dyn Fn(&(String, Option<PathBuf>), &mut Window, &mut App) + 'static>;
pub type BrowserContextCb =
    Arc<dyn Fn(&(Option<PathBuf>, f32, f32), &mut Window, &mut App) + 'static>;
/// Toolbar action with no payload (Collapse All / Rescan / Stop preview).
pub type BrowserActionCb = Arc<dyn Fn(&mut Window, &mut App) + 'static>;

/// Everything the sidebar can ask the layout to do.
///
/// Bundled because the tree hands the same set to every visible row: passing
/// them positionally meant cloning four `Arc`s per row per frame and a
/// thirteen-argument entry point.
#[derive(Clone)]
pub struct BrowserCallbacks {
    pub on_toggle: ToggleNodeCb,
    pub on_select: SelectEntryCb,
    /// Breadcrumb jump: expand the ancestors of a path and select it.
    pub on_reveal: SelectEntryCb,
    pub on_activate_file: ActivateFileCb,
    pub on_context_menu: BrowserContextCb,
    pub on_collapse_all: BrowserActionCb,
    pub on_rescan: BrowserActionCb,
    pub on_toggle_preview: BrowserActionCb,
    pub on_stop_preview: BrowserActionCb,
}

/// Payload for a browser drag.
///
/// **Contract**: the arrangement (`timeline/render.rs`) and the mixer's insert
/// chips (`mixer_panel.rs`) match on this concrete type. Do not rename, move,
/// or reorder its fields.
#[derive(Clone, Debug)]
pub struct BrowserDragItem {
    pub path: PathBuf,
    pub label: String,
}

pub struct BrowserDragPreview {
    label: String,
    icon: &'static str,
}

impl Render for BrowserDragPreview {
    fn render(&mut self, _w: &mut Window, _cx: &mut gpui::Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(space::SNUG))
            .px(px(space::BASE))
            .py(px(space::TIGHT))
            .rounded(px(radius::CONTROL))
            .border(px(1.0))
            .border_color(Colors::border_subtle())
            .bg(Colors::surface_raised())
            // `elevation::DRAG` is documented as covering browser drags.
            .shadow(elevation::shadow(elevation::DRAG))
            .child(
                svg()
                    .path(self.icon)
                    .w(px(GLYPH))
                    .h(px(GLYPH))
                    .text_color(Colors::text_secondary()),
            )
            .child(
                div()
                    .text_size(px(typography::UI_XS))
                    .text_color(Colors::text_primary())
                    .child(self.label.clone()),
            )
    }
}

/// Tabular figures for the details pane's numeric readouts, so digits keep a
/// fixed advance while arrowing through a folder.
fn tabular_figures() -> gpui::FontFeatures {
    gpui::FontFeatures(Arc::new(vec![
        ("tnum".to_string(), 1),
        ("lnum".to_string(), 1),
    ]))
}

/// Resolve a node's display string. Chrome labels carry a message key; real
/// file and folder names do not and are shown verbatim.
fn node_label(label: &str, label_key: Option<&'static str>, i18n: I18n) -> String {
    match label_key {
        Some(key) => i18n.tr_or(key, label),
        None => label.to_string(),
    }
}

#[allow(clippy::too_many_arguments)]
pub fn sidebar(
    state: &FileBrowserState,
    scroll: UniformListScrollHandle,
    tree_focus: &FocusHandle,
    search_input: &TextInputState,
    search_focused: bool,
    active: bool,
    search_callbacks: TextInputCallbacks,
    callbacks: BrowserCallbacks,
    i18n: I18n,
) -> impl IntoElement {
    let panel_border = if active {
        Colors::panel_border_focused()
    } else {
        Colors::border_subtle()
    };

    // ── Header ──────────────────────────────────────────────────────
    // Mirrors the Inspector dock header (32 px, DENSE_LABEL, bold) so the two
    // docks read as one system.
    let chrome_base = Colors::surface_panel_alt();
    let collapse_cb = callbacks.on_collapse_all.clone();
    let rescan_cb = callbacks.on_rescan.clone();
    let preview_cb = callbacks.on_toggle_preview.clone();
    let preview_on = state.preview_enabled;

    let header = div()
        .flex_shrink_0()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::SNUG))
        .h(px(size::PROMINENT))
        .pl(px(space::BASE))
        .pr(px(space::TIGHT))
        .border_b(px(1.0))
        .border_color(panel_border)
        .bg(chrome_base)
        .child(
            svg()
                .path(assets::ICON_FOLDER_PATH)
                .w(px(typography::UI_MD))
                .h(px(typography::UI_MD))
                .text_color(if active {
                    Colors::panel_header_active()
                } else {
                    Colors::text_muted()
                }),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .overflow_hidden()
                .truncate()
                .text_size(px(typography::DENSE_LABEL))
                .font_weight(gpui::FontWeight::BOLD)
                .text_color(if active {
                    Colors::panel_header_active()
                } else {
                    Colors::tab_text()
                })
                .child(i18n.tr("browser.panel.title")),
        )
        .child(fb_shortcut_hint(crate::keymap::accel_display("Ctrl+1")))
        .child(
            div()
                .flex_shrink_0()
                .font_features(tabular_figures())
                .text_size(px(typography::DENSE_CAPTION))
                .text_color(Colors::text_faint())
                .child(format!("{}", state.visible_node_count())),
        )
        .child(
            crate::components::title_bar::chrome_cluster()
                .flex_shrink_0()
                .child(browser_icon_button(
                    "browser-preview-toggle",
                    if preview_on {
                        assets::ICON_VOLUME_2_PATH
                    } else {
                        assets::ICON_VOLUME_X_PATH
                    },
                    i18n.tr_or("browser.preview.auto", "Auto-preview selected audio"),
                    chrome_base,
                    Some(preview_on),
                    true,
                    move |_e, w, cx| preview_cb(w, cx),
                ))
                .child(browser_icon_button(
                    "browser-collapse-all",
                    assets::ICON_MINUS_PATH,
                    i18n.tr_or("browser.action.collapse-all", "Collapse all folders"),
                    chrome_base,
                    None,
                    true,
                    move |_e, w, cx| collapse_cb(w, cx),
                ))
                .child(browser_icon_button(
                    "browser-rescan",
                    assets::ICON_REPEAT_PATH,
                    i18n.tr_or("browser.action.rescan", "Rescan folders"),
                    chrome_base,
                    None,
                    true,
                    move |_e, w, cx| rescan_cb(w, cx),
                )),
        );

    // ── Search ──────────────────────────────────────────────────────
    let search_container = div()
        .flex_shrink_0()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::SNUG))
        .px(px(space::BASE))
        .py(px(space::TIGHT))
        .border_b(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(chrome_base)
        .child(
            svg()
                .path(assets::ICON_SEARCH_PATH)
                .w(px(GLYPH))
                .h(px(GLYPH))
                .text_color(Colors::text_muted()),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .child(text_field_with_callbacks(
                    search_input,
                    search_focused,
                    search_callbacks,
                )),
        );

    // ── Breadcrumb ──────────────────────────────────────────────────
    // Rendered only when there is a trail to show, so the panel never reserves
    // a permanently empty row.
    let crumbs = state.breadcrumb();
    let breadcrumb =
        (!crumbs.is_empty()).then(|| breadcrumb_bar(&crumbs, callbacks.on_reveal.clone(), i18n));

    // ── Row virtualization ──────────────────────────────────────────
    let nodes = state.visible_nodes();
    let count = nodes.len();
    crate::perf::count("browser_rows", count as u64);
    let scroll_for_thumb = scroll.0.borrow().base_handle.clone();
    let nodes_for_list = nodes.clone();
    let callbacks_for_list = callbacks.clone();
    let focus_for_list = tree_focus.clone();

    let list = uniform_list("browser-tree", count, move |range, _window, _cx| {
        let nodes = nodes_for_list.clone();
        let cbs = callbacks_for_list.clone();
        let focus = focus_for_list.clone();
        range
            .map(|i| {
                let node = &nodes[i];
                match node.kind {
                    BrowserNodeKind::GroupHeader => {
                        group_header_row(i, node, &cbs, i18n).into_any_element()
                    }
                    BrowserNodeKind::Info => info_row(node, i18n).into_any_element(),
                    _ => tree_row(i, node, count, &cbs, &focus, i18n).into_any_element(),
                }
            })
            .collect::<Vec<_>>()
    })
    .track_scroll(&scroll)
    .size_full();

    let focus_ring = Colors::state_focus_ring();
    let empty_hint = (count == 0).then(|| {
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .px(px(space::LOOSE))
            .text_size(px(typography::DENSE_CAPTION))
            .text_color(Colors::text_faint())
            .child(if state.filter.is_empty() {
                i18n.tr_or("browser.empty.tree", "Nothing to show")
            } else {
                i18n.tr_or(
                    "browser.empty.no-results",
                    "No matches in the expanded folders",
                )
            })
    });

    let listing = div()
        .id("browser-tree-body")
        .role(Role::Tree)
        .aria_label(i18n.tr_or("browser.tree.label", "Browser tree"))
        .track_focus(tree_focus)
        .tab_stop(true)
        .focus_visible(move |style| style.shadow(elevation::focus_ring(focus_ring)))
        .flex_1()
        .min_h_0()
        .relative()
        .child(list)
        .children(empty_hint)
        .child(vertical_scrollbar_thumb(scroll_for_thumb));

    // ── Details for the current selection ───────────────────────────
    let details = state
        .selected_node()
        .map(|node| details_pane(state, node, &callbacks, i18n));

    div()
        .flex()
        .flex_col()
        .w(px(SIDEBAR_WIDTH))
        .h_full()
        .bg(Colors::surface_panel())
        .border_r(px(1.0))
        .border_color(panel_border)
        .child(header)
        .child(search_container)
        .children(breadcrumb)
        .child(listing)
        .children(details)
}

// ---------------------------------------------------------------------------
// Chrome primitives
// ---------------------------------------------------------------------------

/// Square icon button for the browser header.
///
/// `controls::fb_icon_button` composites its states over `surface.panel`, but
/// the browser's chrome ground is `surface.panelAlt`; compositing hover over
/// the wrong plane is exactly the rule this is meant to follow. It also returns
/// an opaque `impl IntoElement`, which cannot carry the tooltip DESIGN requires
/// for an icon-only control — hence a concrete `Stateful<Div>` here.
fn browser_icon_button(
    id: &'static str,
    icon_path: &'static str,
    label: String,
    base: Rgba,
    toggled: Option<bool>,
    enabled: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let visual = size::DENSE;
    let pad = size::hit_target(visual);
    let on = toggled.unwrap_or(false);
    let rest = if on {
        Colors::composite(base, Colors::accent_active())
    } else {
        Colors::with_alpha(base, 0.0)
    };
    let hover = Colors::composite(rest, Colors::state_hover());
    let pressed = Colors::composite(rest, Colors::state_recessed());
    let tint = if !enabled {
        Colors::text_disabled()
    } else if on {
        Colors::accent_primary()
    } else {
        Colors::text_muted()
    };
    let focus = Colors::state_focus_ring();

    div()
        .id(id)
        .role(Role::Button)
        .aria_label(label.clone())
        .aria_disabled(!enabled)
        .when_some(toggled, |b, t| {
            b.aria_toggled(if t { Toggled::True } else { Toggled::False })
        })
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .w(px(visual + pad * 2.0))
        .h(px(visual + pad * 2.0))
        // 20 px control → the 16–20 px radius tier.
        .rounded(px(radius::CONTROL_SM))
        .bg(rest)
        .tooltip(fb_tooltip(label))
        .when(enabled, |b| {
            b.focusable()
                .tab_stop(true)
                .focus_visible(move |style| style.shadow(elevation::focus_ring(focus)))
                .cursor(gpui::CursorStyle::PointingHand)
                .hover(move |s| s.bg(hover))
                .active(move |s| s.bg(pressed))
                .on_click(on_click)
        })
        .child(
            svg()
                .path(icon_path)
                .w(px(GLYPH))
                .h(px(GLYPH))
                .text_color(tint),
        )
}

/// Path trail from the mounted root down to the selection.
///
/// Truncation is predictable and never head-elides the leaf: when the trail is
/// too long it keeps the root, one non-interactive ellipsis, and the last two
/// crumbs.
fn breadcrumb_bar(
    crumbs: &[BrowserCrumb],
    on_reveal: SelectEntryCb,
    i18n: I18n,
) -> impl IntoElement {
    let mut shown: Vec<Option<&BrowserCrumb>> = Vec::new();
    if crumbs.len() <= 3 {
        shown.extend(crumbs.iter().map(Some));
    } else {
        shown.push(Some(&crumbs[0]));
        shown.push(None);
        shown.push(Some(&crumbs[crumbs.len() - 2]));
        shown.push(Some(&crumbs[crumbs.len() - 1]));
    }
    let last = shown.len().saturating_sub(1);

    let mut bar = div()
        .flex_shrink_0()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::HAIR))
        .h(px(ROW_H))
        .px(px(space::SNUG))
        .overflow_hidden()
        .border_b(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_panel_alt());

    for (i, crumb) in shown.iter().enumerate() {
        if i > 0 {
            bar = bar.child(
                svg()
                    .path(assets::ICON_CHEVRON_RIGHT_PATH)
                    .flex_shrink_0()
                    .w(px(CHEVRON))
                    .h(px(CHEVRON))
                    .text_color(Colors::text_faint()),
            );
        }
        match crumb {
            None => {
                bar = bar.child(
                    div()
                        .flex_shrink_0()
                        .px(px(space::HAIR))
                        .text_size(px(typography::DENSE_CAPTION))
                        .text_color(Colors::text_faint())
                        .child("…"),
                );
            }
            Some(crumb) => {
                let label = node_label(&crumb.label, crumb.label_key, i18n);
                let is_leaf = i == last;
                if is_leaf {
                    // The leaf *is* the selection; it is a readout, not a jump.
                    bar = bar.child(
                        div()
                            .flex_shrink_0()
                            .min_w(px(0.0))
                            .overflow_hidden()
                            .truncate()
                            .px(px(space::HAIR))
                            .text_size(px(typography::DENSE_CAPTION))
                            .text_color(Colors::text_primary())
                            .child(label),
                    );
                } else {
                    let base = Colors::surface_panel_alt();
                    let hover = Colors::composite(base, Colors::state_hover());
                    let pressed = Colors::composite(base, Colors::state_recessed());
                    let focus = Colors::state_focus_ring();
                    let target = crumb.path.clone();
                    let reveal = on_reveal.clone();
                    bar = bar.child(
                        div()
                            .id(("browser-crumb", i))
                            .role(Role::Button)
                            .aria_label(label.clone())
                            .focusable()
                            .tab_stop(true)
                            .focus_visible(move |style| style.shadow(elevation::focus_ring(focus)))
                            .flex_shrink_0()
                            .min_w(px(0.0))
                            .overflow_hidden()
                            .truncate()
                            .px(px(space::HAIR))
                            .rounded(px(radius::MICRO))
                            .text_size(px(typography::DENSE_CAPTION))
                            .text_color(Colors::text_muted())
                            .cursor(gpui::CursorStyle::PointingHand)
                            .hover(move |s| s.bg(hover))
                            .active(move |s| s.bg(pressed))
                            .on_click(move |_e, w, cx| reveal(&target, w, cx))
                            .child(label),
                    );
                }
            }
        }
    }
    bar
}

// ---------------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------------

/// The inset backplate every row state paints on.
///
/// Rest, hover, pressed and selected share this one geometry: the row itself is
/// full-bleed and square (a rounded full-width row exposes a wedge at the
/// panel edge), and only the plate rounds.
fn row_shell(id: (&'static str, usize)) -> Stateful<Div> {
    div()
        .id(id)
        .relative()
        .flex()
        .flex_row()
        .items_center()
        .flex_1()
        .min_w(px(0.0))
        .rounded(px(radius::CONTROL_SM))
}

/// The full-bleed, square row box. It deliberately does **not** centre its
/// children: the backplate stretches to the frame's content height, which is
/// what makes the plate exactly `ROW_H - 2 * PLATE_INSET_Y` tall without
/// resolving a percentage against the wrong box.
fn row_frame() -> Div {
    div()
        .flex()
        .flex_row()
        .h(px(ROW_H))
        .w_full()
        .flex_shrink_0()
        .px(px(PLATE_INSET_X))
        .py(px(PLATE_INSET_Y))
}

/// Subtle, collapsible section header (COLLECTIONS / LIBRARY / PLACES).
///
/// While a search filter is active every group is forced open, so the header
/// drops its chevron and click target rather than offering a control that
/// would do nothing visible.
fn group_header_row(
    index: usize,
    node: &BrowserVisibleNode,
    callbacks: &BrowserCallbacks,
    i18n: I18n,
) -> impl IntoElement {
    let id = node.id.clone();
    let expandable = node.expandable;
    let expanded = node.expanded;
    let label = node_label(&node.label, node.label_key, i18n);
    let on_toggle = callbacks.on_toggle.clone();

    let base = Colors::surface_panel();
    let hover = Colors::composite(base, Colors::state_hover());
    let pressed = Colors::composite(base, Colors::state_recessed());

    let plate = row_shell(("browser-group", index))
        .px(px(PLATE_PAD_X))
        .gap(px(space::TIGHT))
        .when(expandable, |b| {
            b.cursor(gpui::CursorStyle::PointingHand)
                .hover(move |s| s.bg(hover))
                .active(move |s| s.bg(pressed))
                .on_click(move |_e, w, cx| on_toggle(&(id.clone(), None), w, cx))
        })
        .child(if expandable {
            svg()
                .path(if expanded {
                    assets::ICON_CHEVRON_DOWN_PATH
                } else {
                    assets::ICON_CHEVRON_RIGHT_PATH
                })
                .flex_shrink_0()
                .w(px(CHEVRON))
                .h(px(CHEVRON))
                .text_color(Colors::text_faint())
                .into_any_element()
        } else {
            div()
                .flex_shrink_0()
                .w(px(CHEVRON))
                .h(px(CHEVRON))
                .into_any_element()
        })
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .overflow_hidden()
                .truncate()
                .text_size(px(typography::DENSE_CAPTION))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(Colors::text_faint())
                .child(label),
        );

    row_frame()
        .relative()
        // Hairline above every group but the first. Absolutely positioned so it
        // can never change the height `uniform_list` measured.
        .child(if index == 0 {
            Empty.into_any_element()
        } else {
            div()
                .absolute()
                .top(px(0.0))
                .left(px(space::SNUG))
                .right(px(space::SNUG))
                .h(px(1.0))
                .bg(Colors::divider())
                .into_any_element()
        })
        .child(plate)
}

/// Non-interactive hint row — an honest empty state, a pending listing, or a
/// directory that could not be read.
fn info_row(node: &BrowserVisibleNode, i18n: I18n) -> impl IntoElement {
    let is_error = node.error.is_some();
    let depth = node.depth.min(MAX_VISUAL_DEPTH) as f32;
    let label = node_label(&node.label, node.label_key, i18n);
    row_frame().child(
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(space::TIGHT))
            .flex_1()
            .min_w(px(0.0))
            .pl(px(PLATE_PAD_X + depth * INDENT + DISCLOSURE_W))
            .pr(px(PLATE_PAD_X))
            .child(if is_error {
                svg()
                    .path(assets::ICON_X_PATH)
                    .flex_shrink_0()
                    .w(px(CHEVRON))
                    .h(px(CHEVRON))
                    .text_color(Colors::status_warning())
                    .into_any_element()
            } else {
                Empty.into_any_element()
            })
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .truncate()
                    .text_size(px(typography::DENSE_CAPTION))
                    .text_color(if is_error {
                        Colors::status_warning()
                    } else {
                        Colors::text_faint()
                    })
                    .child(label),
            ),
    )
}

fn tree_row(
    index: usize,
    node: &BrowserVisibleNode,
    total: usize,
    callbacks: &BrowserCallbacks,
    tree_focus: &FocusHandle,
    i18n: I18n,
) -> impl IntoElement {
    let node_id = node.id.clone();
    let path = node.path.clone();
    let label = node_label(&node.label, node.label_key, i18n);
    let expandable = node.expandable;
    let expanded = node.expanded;
    let selected = node.selected;
    let is_folder = node.kind == BrowserNodeKind::Folder;
    let is_file = node.kind == BrowserNodeKind::File;
    let is_root_item = node.depth == 1;
    let depth = node.depth.min(MAX_VISUAL_DEPTH) as f32;

    // One background per div, so every state fill is resolved here and handed
    // to the closure — `.hover(|s| s.bg(token))` would replace the plate's fill
    // rather than lift it.
    let base = Colors::surface_panel();
    let rest = if selected {
        Colors::composite(base, Colors::state_selected())
    } else {
        Colors::with_alpha(base, 0.0)
    };
    let hover = if selected {
        Colors::composite(base, Colors::state_selected_hover())
    } else {
        Colors::composite(base, Colors::state_hover())
    };
    let pressed = Colors::composite(if selected { rest } else { base }, Colors::state_recessed());

    let text_color = if selected {
        Colors::text_primary()
    } else if is_folder {
        Colors::text_secondary()
    } else if node.is_audio() || node.is_midi() || node.is_plugin_preset() || node.is_video() {
        Colors::text_muted()
    } else {
        Colors::text_faint()
    };
    let label_weight = if is_root_item {
        gpui::FontWeight::SEMIBOLD
    } else if is_folder {
        gpui::FontWeight::MEDIUM
    } else {
        gpui::FontWeight::NORMAL
    };

    let icon_path = browser_icon_path(node.icon, expanded);
    let icon_color = if selected {
        Colors::text_primary()
    } else {
        Colors::text_muted()
    };

    let on_select = callbacks.on_select.clone();
    let on_toggle = callbacks.on_toggle.clone();
    let on_activate = callbacks.on_activate_file.clone();
    let on_context = callbacks.on_context_menu.clone();
    let focus_for_click = tree_focus.clone();
    let select_path = path.clone();
    let toggle_path = path.clone();
    let activate_path = path.clone();
    let context_path = path.clone();

    let mut plate = row_shell(("browser-tree-row", index))
        .role(Role::TreeItem)
        .aria_label(label.clone())
        .aria_selected(selected)
        // Only a row that can actually open reports an expanded state; a file
        // announced as "collapsed" is a lie to a screen reader.
        .when(expandable, |b| b.aria_expanded(expanded))
        .aria_level(node.depth + 1)
        .aria_position_in_set(index + 1)
        .aria_size_of_set(total)
        .pl(px(PLATE_PAD_X + depth * INDENT))
        .pr(px(PLATE_PAD_X))
        .gap(px(space::TIGHT))
        .bg(rest)
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(move |s| s.bg(hover))
        .active(move |s| s.bg(pressed))
        // Selection is carried on two channels: the plate fill above and this
        // leading-edge accent marker, drawn as an overlay so the row never
        // reflows.
        .child(if selected {
            div()
                .absolute()
                .left(px(0.0))
                .top(px(space::TIGHT))
                .bottom(px(space::TIGHT))
                .w(px(space::HAIR))
                .bg(Colors::accent_primary())
                .into_any_element()
        } else {
            Empty.into_any_element()
        })
        .child(
            div()
                .flex()
                .items_center()
                .justify_center()
                .flex_shrink_0()
                .w(px(DISCLOSURE_W))
                .child(disclosure_icon(expandable, expanded)),
        )
        .child(
            svg()
                .path(icon_path)
                .flex_shrink_0()
                .w(px(GLYPH))
                .h(px(GLYPH))
                .text_color(icon_color),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .overflow_hidden()
                .truncate()
                .text_size(px(typography::UI_XS))
                .font_weight(label_weight)
                .text_color(text_color)
                .child(label.clone()),
        );

    plate = plate.on_click(move |event, window, cx| {
        // Take tree focus first: without it the arrows, Enter and type-ahead
        // died the moment the user clicked a row.
        focus_for_click.focus(window, cx);
        if let Some(p) = select_path.as_ref() {
            on_select(p, window, cx);
        }
        if expandable {
            on_toggle(&(node_id.clone(), toggle_path.clone()), window, cx);
        } else if is_file && event.click_count() >= 2 {
            if let Some(p) = activate_path.as_ref() {
                on_activate(p, window, cx);
            }
        }
    });

    plate = plate.on_mouse_down(
        gpui::MouseButton::Right,
        move |event: &gpui::MouseDownEvent, window, cx| {
            let x: f32 = event.position.x.into();
            let y: f32 = event.position.y.into();
            on_context(&(context_path.clone(), x, y), window, cx);
        },
    );

    // Drag sources the arrangement and the mixer already accept. The mixer's
    // `can_drop` rejects anything that is not a `.pst`, and `drag_over` is
    // gated by it, so widening this cannot make an insert chip highlight for an
    // audio drag.
    if let Some(drag_path) = path.filter(|_| is_file) {
        if let Some(kind) = drag_kind(node, &drag_path) {
            let drag_label = label.clone();
            plate = plate.on_drag(
                BrowserDragItem {
                    path: drag_path,
                    label: drag_label,
                },
                move |drag, _offset, _window, cx| {
                    cx.new(|_| BrowserDragPreview {
                        label: drag.label.clone(),
                        icon: kind,
                    })
                },
            );
        }
    }

    row_frame().child(plate)
}

/// Glyph for a draggable row, or `None` when nothing downstream accepts it.
fn drag_kind(node: &BrowserVisibleNode, path: &Path) -> Option<&'static str> {
    if node.is_audio() {
        Some(assets::ICON_AUDIO_LINES_PATH)
    } else if node.is_midi() {
        Some(assets::ICON_LIST_MUSIC_PATH)
    } else if sphere_video_player::is_supported_video_path(path) {
        Some(assets::ICON_FILM_PATH)
    } else if path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("pst"))
    {
        Some(assets::ICON_PLUG_PATH)
    } else {
        None
    }
}

fn disclosure_icon(expandable: bool, expanded: bool) -> impl IntoElement {
    if expandable {
        svg()
            .path(if expanded {
                assets::ICON_CHEVRON_DOWN_PATH
            } else {
                assets::ICON_CHEVRON_RIGHT_PATH
            })
            .w(px(CHEVRON))
            .h(px(CHEVRON))
            .text_color(Colors::text_faint())
            .into_any_element()
    } else {
        div().w(px(CHEVRON)).h(px(CHEVRON)).into_any_element()
    }
}

/// Resolve a semantic [`BrowserIcon`] to a registered SVG glyph.
///
/// Monochrome by design: browser glyphs carry their meaning in the shape, so
/// they share one neutral ramp instead of a per-content-type hue.
fn browser_icon_path(icon: BrowserIcon, expanded: bool) -> &'static str {
    match icon {
        BrowserIcon::Favorites => assets::ICON_STAR_PATH,
        BrowserIcon::Recent => assets::ICON_CLOCK_PATH,
        BrowserIcon::Samples => assets::ICON_AUDIO_LINES_PATH,
        BrowserIcon::Instruments => assets::ICON_CPU_PATH,
        BrowserIcon::Plugins | BrowserIcon::PresetFile => assets::ICON_PLUG_PATH,
        BrowserIcon::AudioFiles | BrowserIcon::Music | BrowserIcon::AudioFile => {
            assets::ICON_MUSIC_PATH
        }
        BrowserIcon::Projects | BrowserIcon::ProjectFile => assets::ICON_SAVE_PATH,
        BrowserIcon::Templates => assets::ICON_NEWSPAPER_PATH,
        BrowserIcon::MidiFile => assets::ICON_LIST_MUSIC_PATH,
        BrowserIcon::VideoFile => assets::ICON_FILM_PATH,
        BrowserIcon::Drive => assets::ICON_HARD_DRIVE_PATH,
        BrowserIcon::UserLibrary => assets::ICON_LAYERS_PATH,
        BrowserIcon::Home => assets::ICON_USER_PATH,
        BrowserIcon::Downloads => assets::ICON_SHARE_PATH,
        BrowserIcon::Desktop => assets::ICON_MAXIMIZE_PATH,
        BrowserIcon::Documents => assets::ICON_TYPE_PATH,
        BrowserIcon::Folder => {
            if expanded {
                assets::ICON_FOLDER_OPEN_PATH
            } else {
                assets::ICON_FOLDER_PATH
            }
        }
        BrowserIcon::FolderOpen => assets::ICON_FOLDER_OPEN_PATH,
        BrowserIcon::GenericFile | BrowserIcon::None => assets::ICON_FILE_PATH,
    }
}

// ---------------------------------------------------------------------------
// Details pane
// ---------------------------------------------------------------------------

/// Everything known about the current selection: name, size, and — for audio —
/// the decoded peaks, the audition window, and a real Stop control.
///
/// Size and duration live here rather than in a row column: at 272 px with a
/// 12 px indent per level, a reserved right-hand column starves the name from
/// depth 4 down, and the name is what the user is actually scanning.
fn details_pane(
    state: &FileBrowserState,
    node: &BrowserVisibleNode,
    callbacks: &BrowserCallbacks,
    i18n: I18n,
) -> impl IntoElement {
    let label = node_label(&node.label, node.label_key, i18n);
    let audio_path = state.selected_audio_path().map(|p| p.to_path_buf());

    let mut meta: Vec<String> = Vec::new();
    if !node.extension.is_empty() {
        meta.push(node.extension.to_uppercase());
    }
    if let Some(bytes) = node.size_bytes {
        meta.push(format_size(bytes));
    }

    let header = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::SNUG))
        .h(px(size::DENSE))
        .child(
            svg()
                .path(browser_icon_path(node.icon, node.expanded))
                .flex_shrink_0()
                .w(px(GLYPH))
                .h(px(GLYPH))
                .text_color(Colors::text_muted()),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .overflow_hidden()
                .truncate()
                .text_size(px(typography::DENSE_LABEL))
                .text_color(Colors::text_secondary())
                .child(label),
        )
        .children((!meta.is_empty()).then(|| {
            div()
                .flex_shrink_0()
                .font_features(tabular_figures())
                .text_size(px(typography::DENSE_CAPTION))
                .text_color(Colors::text_faint())
                .child(meta.join(" · "))
        }));

    let audio_section = audio_path.map(|path| {
        let key = path.to_string_lossy().to_string();
        let preview = waveform_cache::get_preview_arc(&key);
        let playhead = state.preview_position_for(&path);
        let failed = state.waveform_failed(&path);
        let playing = state.preview_playing.is_some();
        let stop_cb = callbacks.on_stop_preview.clone();

        // Format / duration readout, tabular so it does not reflow.
        let transport_text = match preview.as_ref() {
            Some(p) => {
                let secs = p.duration_seconds.max(0.0);
                let mins = (secs as u64) / 60;
                let rem = secs - (mins * 60) as f64;
                let sr = p.sample_rate as f32 / 1000.0;
                format!("{mins}:{rem:04.1} · {sr:.1} kHz · {} ch", p.channels)
            }
            None if failed => i18n.tr_or("browser.preview.failed", "Could not decode audio"),
            None => i18n.tr_or("browser.preview.decoding", "Decoding waveform…"),
        };

        let canvas_body: gpui::AnyElement = match preview {
            Some(preview) => mini_waveform_canvas(preview, playhead).into_any_element(),
            None => div()
                .flex()
                .items_center()
                .justify_center()
                .size_full()
                .text_size(px(typography::DENSE_CAPTION))
                .text_color(if failed {
                    Colors::status_error()
                } else {
                    Colors::text_faint()
                })
                .child(if failed {
                    i18n.tr_or("browser.preview.failed", "Could not decode audio")
                } else {
                    i18n.tr_or("browser.preview.decoding", "Decoding waveform…")
                })
                .into_any_element(),
        };

        div()
            .flex()
            .flex_col()
            .gap(px(space::TIGHT))
            .child(
                // Square: a waveform's content mask is an axis-aligned rect.
                div()
                    .relative()
                    .h(px(WAVEFORM_H))
                    .w_full()
                    .overflow_hidden()
                    .bg(Colors::surface_input())
                    .child(canvas_body),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(space::SNUG))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .overflow_hidden()
                            .truncate()
                            .font_features(tabular_figures())
                            .text_size(px(typography::DENSE_CAPTION))
                            .text_color(if failed {
                                Colors::status_error()
                            } else {
                                Colors::text_faint()
                            })
                            .child(transport_text),
                    )
                    .child(browser_icon_button(
                        "browser-preview-stop",
                        assets::ICON_SQUARE_PATH,
                        i18n.tr_or("browser.preview.stop", "Stop preview"),
                        Colors::surface_panel_alt(),
                        None,
                        playing,
                        move |_e, w, cx| stop_cb(w, cx),
                    )),
            )
    });

    div()
        .flex_shrink_0()
        .flex()
        .flex_col()
        .gap(px(space::TIGHT))
        .px(px(space::BASE))
        .py(px(space::SNUG))
        .border_t(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_panel_alt())
        .child(header)
        .children(audio_section)
}

/// Draw a peak-rendered waveform filling the canvas. Columns are computed from
/// the real paint bounds (DPI-correct) using one min/max bar per pixel column —
/// canvas + `paint_quad`, never DOM spam (DESIGN.md waveform rules).
///
/// Two overlays share this canvas so they cannot disagree about the
/// seconds → x transform: the preview-limit shade (the part of a long file the
/// audition will not reach) and the playhead.
fn mini_waveform_canvas(
    preview: Arc<waveform_cache::WaveformPreview>,
    playhead_seconds: Option<f32>,
) -> impl IntoElement {
    // The accent already marks row selection in this panel, so the waveform
    // takes the arrangement's audio-clip treatment instead of a second cyan.
    let wave_color = Colors::timeline_audio_clip_waveform(Colors::track_audio());
    let past_limit_shade = Colors::with_alpha(
        Colors::surface_input(),
        crate::theme::state::DISABLED_CONTENT,
    );
    let limit_marker = Colors::border_strong();
    let played_shade = Colors::timeline_selection();
    let playhead_color = Colors::timeline_playhead();

    let element = canvas(
        |_bounds, _window, _cx| {},
        move |bounds: Bounds<Pixels>, (), window, _cx| {
            let w: f32 = f32::from(bounds.size.width).max(1.0);
            let h: f32 = f32::from(bounds.size.height).max(1.0);
            let center = h / 2.0;
            let cols = (w.floor() as usize).max(1);
            // Whole-file seconds → x. Both overlays below use only this.
            let duration = preview.duration_seconds.max(f64::MIN_POSITIVE);
            let x_at = |seconds: f64| ((seconds / duration) as f32).clamp(0.0, 1.0) * w;
            let samples_per_pixel = (preview.total_frames.max(1) as f32 / w).max(1.0);
            let Some(lod) = waveform_cache::pick_lod(&preview, samples_per_pixel) else {
                return;
            };
            let total = lod.peaks.len().max(1);
            for col in 0..cols {
                let frac0 = col as f32 / cols as f32;
                let frac1 = (col + 1) as f32 / cols as f32;
                let p0 = (frac0 * total as f32).floor() as usize;
                let p1 = (frac1 * total as f32).ceil() as usize;
                let end = p1.min(total).max(p0 + 1);
                let mut mn = 0.0f32;
                let mut mx = 0.0f32;
                for pk in &lod.peaks[p0..end] {
                    if pk.min < mn {
                        mn = pk.min;
                    }
                    if pk.max > mx {
                        mx = pk.max;
                    }
                }
                let top = center - mx.min(1.0) * center;
                let bottom = center - mn.max(-1.0) * center;
                let bar_h = (bottom - top).max(1.0);
                let r = Bounds::new(
                    bounds.origin + point(px(col as f32), px(top)),
                    gpui::size(px(1.0), px(bar_h)),
                );
                window.paint_quad(fill(r, wave_color));
            }

            // Preview window: a selection only auditions the file's first
            // `AUDITION_PREVIEW_SECONDS`. Shade the rest so the waveform stays
            // honest about what will actually be heard.
            let limit_x = x_at(AUDITION_PREVIEW_SECONDS);
            if limit_x < w - 1.0 {
                window.paint_quad(fill(
                    Bounds::new(
                        bounds.origin + point(px(limit_x), px(0.0)),
                        gpui::size(px(w - limit_x), px(h)),
                    ),
                    past_limit_shade,
                ));
                window.paint_quad(fill(
                    Bounds::new(
                        bounds.origin + point(px(limit_x), px(0.0)),
                        gpui::size(px(1.0), px(h)),
                    ),
                    limit_marker,
                ));
            }

            // Playhead — engine position, drawn only while a preview is audible.
            if let Some(seconds) = playhead_seconds {
                let x = x_at(seconds as f64);
                if x > 0.0 {
                    window.paint_quad(fill(
                        Bounds::new(bounds.origin, gpui::size(px(x), px(h))),
                        played_shade,
                    ));
                }
                window.paint_quad(fill(
                    Bounds::new(
                        bounds.origin + point(px(x.min(w - 1.0)), px(0.0)),
                        gpui::size(px(1.0), px(h)),
                    ),
                    playhead_color,
                ));
            }
        },
    )
    .absolute()
    .inset_0();

    div()
        .relative()
        .size_full()
        .overflow_hidden()
        .child(element)
}

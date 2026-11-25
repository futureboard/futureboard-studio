//! Browser sidebar — left dock of the studio shell.
//!
//! A dense, grouped DAW asset browser. The navigation model lives in
//! `file_browser.rs`; this module is pure presentation. Top to bottom:
//!
//! ```txt
//! title + preview / collapse / rescan
//! six named tabs, three to a row
//! search + type chips              (tabs of mixed files)
//! folder path with a parent button
//! tree: roots as captions, size or tag on the right
//! preview card: play, name, facts, waveform, Add
//! ```
//!
//! Region contract:
//!
//! * **Layout owner** — the shell's main row (`studio_render`), which gives
//!   this element its width: [`SIDEBAR_WIDTH`] by default, resizable by
//!   dragging the dock's edge. The width is load-bearing for the arrangement's
//!   coordinate transform (`panel_origin_x`), so every change goes through
//!   `sync_timeline_chrome_metrics`; this element only fills what it is given.
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
use crate::components::controls::{fb_button, fb_tooltip, FbButtonKind};
pub use crate::components::file_browser::BrowserKindFilter;
use crate::components::file_browser::{
    format_size, BrowserCrumb, BrowserIcon, BrowserNodeKind, BrowserTab, BrowserVisibleNode,
    FileBrowserState, MAX_VISUAL_DEPTH,
};
use crate::components::scrollbar::scroll_handle_scrollbar;
use crate::components::text_input::{
    text_field_with_callbacks, TextInputCallbacks, TextInputState,
};
use crate::components::timeline::waveform_cache;
use crate::i18n::I18n;
use crate::theme::{elevation, radius, size, space, typography, Colors};
use DirectAudio::AUDITION_PREVIEW_SECONDS;

pub const SIDEBAR_WIDTH: f32 = 272.0;

/// Row height for every row kind. Exact and shared, because `uniform_list`
/// derives its scroll window from one measured row.
const ROW_H: f32 = size::ROW;
/// Per-depth indent. Smaller than a generic file explorer so deep trees still
/// fit the fixed sidebar width.
const INDENT: f32 = space::LOOSE;
/// Disclosure column width — keeps icons aligned across rows whether or not the
/// row can open.
const DISCLOSURE_W: f32 = space::LOOSE;
/// Horizontal inset of the row backplate inside the full-bleed row. Only the
/// backplate rounds; the row itself stays square (DESIGN: full-width list rows).
const PLATE_INSET_X: f32 = space::TIGHT;
/// Vertical inset, which leaves a 20 px plate inside a 24 px row — the 16–20 px
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
const WAVEFORM_H: f32 = size::TRACK_ROW_MIN + space::BASE;

pub type ActivateFileCb = Arc<dyn Fn(&PathBuf, &mut Window, &mut App) + 'static>;
pub type SelectEntryCb = Arc<dyn Fn(&PathBuf, &mut Window, &mut App) + 'static>;
pub type ToggleNodeCb = Arc<dyn Fn(&(String, Option<PathBuf>), &mut Window, &mut App) + 'static>;
pub type BrowserContextCb =
    Arc<dyn Fn(&(Option<PathBuf>, f32, f32), &mut Window, &mut App) + 'static>;
/// Toolbar action with no payload (Collapse All / Rescan / Stop preview).
pub type BrowserActionCb = Arc<dyn Fn(&mut Window, &mut App) + 'static>;
/// Tab strip: show a tab.
pub type BrowserTabCb = Arc<dyn Fn(&BrowserTab, &mut Window, &mut App) + 'static>;
/// Type chips: show one type of file.
pub type BrowserKindCb = Arc<dyn Fn(&BrowserKindFilter, &mut Window, &mut App) + 'static>;

/// Everything the sidebar can ask the layout to do.
///
/// Bundled because the tree hands the same set to every visible row: passing
/// them positionally meant cloning four `Arc`s per row per frame and a
/// thirteen-argument entry point.
#[derive(Clone)]
pub struct BrowserCallbacks {
    pub on_select_tab: BrowserTabCb,
    pub on_select_kind: BrowserKindCb,
    pub on_toggle: ToggleNodeCb,
    pub on_select: SelectEntryCb,
    /// Path jump: expand the ancestors of a path and select it.
    pub on_reveal: SelectEntryCb,
    pub on_activate_file: ActivateFileCb,
    /// Play an audio file now, whether or not auto-preview is on.
    pub on_audition: SelectEntryCb,
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

/// Tabular figures for numeric readouts, so digits keep a fixed advance while
/// arrowing through a folder.
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

/// The depth at which rows are drawn as captions. A tab with group headers
/// captions the headers (depth 0); one without captions its roots (depth 1),
/// so on either the first level of real content sits at the left edge.
fn caption_depth(nodes: &[BrowserVisibleNode]) -> usize {
    if nodes.iter().any(|n| n.kind == BrowserNodeKind::GroupHeader) {
        0
    } else {
        1
    }
}

/// Indent steps for a row below the caption level.
fn indent_steps(depth: usize, caption_depth: usize) -> f32 {
    depth
        .saturating_sub(caption_depth + 1)
        .min(MAX_VISUAL_DEPTH) as f32
}

#[allow(clippy::too_many_arguments)]
pub fn sidebar(
    state: &FileBrowserState,
    scroll: UniformListScrollHandle,
    tree_focus: &FocusHandle,
    search_input: &TextInputState,
    search_focused: bool,
    _active: bool,
    search_callbacks: TextInputCallbacks,
    callbacks: BrowserCallbacks,
    i18n: I18n,
    rigs_panel: Option<gpui::AnyView>,
) -> impl IntoElement {
    let panel_border = Colors::border_subtle();

    // ── Header ──────────────────────────────────────────────────────
    // Mirrors the Inspector dock header so the two docks read as one system.
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
        .pl(px(space::LOOSE))
        .pr(px(space::TIGHT))
        .bg(chrome_base)
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .truncate()
                .text_size(px(typography::UI_MD))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(Colors::text_primary())
                .child(i18n.tr("browser.panel.title")),
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
                    assets::ICON_REFRESH_CW_PATH,
                    i18n.tr_or("browser.action.rescan", "Rescan folders"),
                    chrome_base,
                    None,
                    true,
                    move |_e, w, cx| rescan_cb(w, cx),
                )),
        );

    // ── Tabs, search, type chips ────────────────────────────────────
    let controls = div()
        .flex_shrink_0()
        .flex()
        .flex_col()
        .gap(px(space::BASE))
        .px(px(space::BASE))
        .pb(px(space::BASE))
        .border_b(px(1.0))
        .border_color(panel_border)
        .bg(chrome_base)
        .child(tab_grid(state.tab, callbacks.on_select_tab.clone(), i18n))
        // The tree's filter means nothing to the Rigs page, which has its own
        // search; a field that filters nothing would be a control that lies.
        .when(state.tab != BrowserTab::Rigs, |col| {
            col.child(text_field_with_callbacks(
                search_input,
                search_focused,
                search_callbacks,
            ))
        })
        .when(state.tab.has_kind_filter(), |col| {
            col.child(kind_chips(
                state.kind_filter,
                callbacks.on_select_kind.clone(),
            ))
        });

    // ── Folder path ─────────────────────────────────────────────────
    let trail = (state.tab != BrowserTab::Instruments)
        .then(|| folder_trail(state, callbacks.on_reveal.clone(), i18n))
        .flatten();

    // ── Row virtualization ──────────────────────────────────────────
    let nodes = state.visible_nodes();
    let count = nodes.len();
    crate::perf::count("browser_rows", count as u64);
    let captions = caption_depth(&nodes);
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
                    BrowserNodeKind::Info => info_row(node, captions, i18n).into_any_element(),
                    _ if node.depth == captions && node.kind == BrowserNodeKind::Folder => {
                        root_caption_row(i, node, &cbs, &focus, i18n).into_any_element()
                    }
                    _ => tree_row(i, node, count, captions, &cbs, &focus, i18n).into_any_element(),
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
        .children(scroll_handle_scrollbar(
            "browser-tree-scrollbar",
            &scroll_for_thumb,
        ));

    // ── The current selection ───────────────────────────────────────
    let card = state
        .selected_node()
        .map(|node| preview_card(state, node, &callbacks, i18n));

    let column = div()
        .flex()
        .flex_col()
        .w_full()
        .h_full()
        .bg(Colors::surface_panel())
        .border_r(px(1.0))
        .border_color(panel_border)
        .child(header)
        .child(controls);
    // The Rigs tab is a web page: it takes the whole body below the tabs in
    // place of the tree, its path and its card.
    match rigs_panel {
        Some(panel) if state.tab == BrowserTab::Rigs => {
            column.child(div().flex_1().min_h_0().child(panel))
        }
        _ => column.children(trail).child(listing).children(card),
    }
}

// ---------------------------------------------------------------------------
// Chrome primitives
// ---------------------------------------------------------------------------

/// Every tab named, three to a row: six names do not fit one row of the dock,
/// and a glyph alone made five of them guesswork.
fn tab_grid(active: BrowserTab, on_select: BrowserTabCb, i18n: I18n) -> impl IntoElement {
    let track = Colors::surface_input();
    let cell = |index: usize, tab: BrowserTab| {
        let is_active = tab == active;
        let label = i18n.tr_or(tab.label_key(), tab.label());
        let rest = if is_active {
            Colors::composite(track, Colors::state_selected())
        } else {
            Colors::with_alpha(track, 0.0)
        };
        let hover = Colors::composite(if is_active { rest } else { track }, Colors::state_hover());
        let pressed = Colors::composite(
            if is_active { rest } else { track },
            Colors::state_recessed(),
        );
        let select = on_select.clone();
        div()
            .id(("browser-tab", index))
            .role(Role::Tab)
            .aria_label(label.clone())
            .aria_selected(is_active)
            .flex()
            .flex_row()
            .items_center()
            .justify_center()
            .gap(px(space::TIGHT))
            .flex_1()
            .min_w(px(0.0))
            .h(px(size::DEFAULT))
            .px(px(space::HAIR))
            .rounded(px(radius::inner(radius::CONTROL, space::HAIR)))
            .bg(rest)
            .cursor(gpui::CursorStyle::PointingHand)
            .hover(move |s| s.bg(hover))
            .active(move |s| s.bg(pressed))
            .on_click(move |_e, w, cx| select(&tab, w, cx))
            .child(
                svg()
                    .path(browser_icon_path(tab.icon(), false))
                    .flex_shrink_0()
                    .w(px(GLYPH))
                    .h(px(GLYPH))
                    .text_color(if is_active {
                        Colors::accent_primary()
                    } else {
                        Colors::text_muted()
                    }),
            )
            .child(
                div()
                    .min_w(px(0.0))
                    .truncate()
                    .text_size(px(typography::UI_XS))
                    .font_weight(if is_active {
                        gpui::FontWeight::SEMIBOLD
                    } else {
                        gpui::FontWeight::NORMAL
                    })
                    .text_color(if is_active {
                        Colors::text_primary()
                    } else {
                        Colors::text_secondary()
                    })
                    .child(label),
            )
    };
    let (first, second) = BrowserTab::ALL.split_at(3);
    div()
        .flex()
        .flex_col()
        .gap(px(space::HAIR))
        .p(px(space::HAIR))
        .rounded(px(radius::CONTROL))
        .bg(track)
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .child(
            div()
                .flex()
                .flex_row()
                .gap(px(space::HAIR))
                .children(first.iter().enumerate().map(|(i, t)| cell(i, *t))),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .gap(px(space::HAIR))
                .children(second.iter().enumerate().map(|(i, t)| cell(i + 3, *t))),
        )
}

/// One chip per file type. The active chip carries a fill and a bold label.
fn kind_chips(active: BrowserKindFilter, on_select: BrowserKindCb) -> impl IntoElement {
    let base = Colors::surface_panel_alt();
    div().flex().flex_row().gap(px(space::TIGHT)).children(
        BrowserKindFilter::ALL.iter().enumerate().map(|(i, kind)| {
            let kind = *kind;
            let on = kind == active;
            let rest = if on {
                Colors::composite(base, Colors::state_selected())
            } else {
                base
            };
            let hover = Colors::composite(rest, Colors::state_hover());
            let select = on_select.clone();
            div()
                .id(("browser-kind", i))
                .role(Role::Button)
                .aria_label(kind.label())
                .aria_toggled(if on { Toggled::True } else { Toggled::False })
                .flex()
                .items_center()
                .h(px(size::MICRO + space::TIGHT))
                .px(px(space::BASE))
                .rounded(px(radius::PILL))
                .bg(rest)
                .border(px(1.0))
                .border_color(if on {
                    Colors::with_alpha(Colors::accent_primary(), 0.55)
                } else {
                    Colors::border_subtle()
                })
                .cursor(gpui::CursorStyle::PointingHand)
                .hover(move |s| s.bg(hover))
                .on_click(move |_e, w, cx| select(&kind, w, cx))
                .text_size(px(typography::DENSE_LABEL))
                .font_weight(if on {
                    gpui::FontWeight::SEMIBOLD
                } else {
                    gpui::FontWeight::NORMAL
                })
                .text_color(if on {
                    Colors::text_primary()
                } else {
                    Colors::text_muted()
                })
                .child(kind.label())
        }),
    )
}

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

/// The folders from the mounted root down to the one being browsed, with a
/// button up to its parent. A selected file is in the card below, so it is
/// not repeated here. `None` when nothing is selected.
///
/// Truncation keeps the last three folders: the deepest is where you are and
/// the ones above it are where the parent button goes.
fn folder_trail(
    state: &FileBrowserState,
    on_reveal: SelectEntryCb,
    i18n: I18n,
) -> Option<impl IntoElement> {
    let mut crumbs: Vec<BrowserCrumb> = state.breadcrumb();
    let selected_is_file = state
        .selected_node()
        .is_some_and(|n| n.kind == BrowserNodeKind::File);
    if selected_is_file {
        crumbs.pop();
    }
    if crumbs.is_empty() {
        return None;
    }
    let parent = (crumbs.len() >= 2).then(|| crumbs[crumbs.len() - 2].path.clone());
    let shown = &crumbs[crumbs.len().saturating_sub(3)..];
    let last = shown.len() - 1;

    let base = Colors::surface_panel();
    let hover = Colors::composite(base, Colors::state_hover());
    let pressed = Colors::composite(base, Colors::state_recessed());
    let focus = Colors::state_focus_ring();
    let up_label = i18n.tr_or("browser.path.parent", "Parent folder");
    let up_reveal = on_reveal.clone();
    let up_enabled = parent.is_some();
    let up = div()
        .id("browser-path-parent")
        .role(Role::Button)
        .aria_label(up_label.clone())
        .aria_disabled(!up_enabled)
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .w(px(size::DENSE))
        .h(px(size::DENSE))
        .rounded(px(radius::CONTROL_SM))
        .bg(Colors::surface_raised())
        .tooltip(fb_tooltip(up_label))
        .when_some(parent, |b, parent| {
            b.focusable()
                .tab_stop(true)
                .focus_visible(move |style| style.shadow(elevation::focus_ring(focus)))
                .cursor(gpui::CursorStyle::PointingHand)
                .hover(move |s| {
                    s.bg(Colors::composite(
                        Colors::surface_raised(),
                        Colors::state_hover(),
                    ))
                })
                .on_click(move |_e, w, cx| up_reveal(&parent, w, cx))
        })
        .child(
            svg()
                .path(assets::ICON_CHEVRON_LEFT_PATH)
                .w(px(GLYPH))
                .h(px(GLYPH))
                .text_color(if up_enabled {
                    Colors::text_secondary()
                } else {
                    Colors::text_disabled()
                }),
        );

    let mut bar = div()
        .flex_shrink_0()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::TIGHT))
        .h(px(size::COMFORTABLE))
        .px(px(space::SNUG))
        .overflow_hidden()
        .border_b(px(1.0))
        .border_color(Colors::border_subtle())
        .child(up);
    for (i, crumb) in shown.iter().enumerate() {
        if i > 0 {
            bar = bar.child(
                div()
                    .flex_shrink_0()
                    .text_size(px(typography::DENSE_LABEL))
                    .text_color(Colors::text_faint())
                    .child("/"),
            );
        }
        let label = node_label(&crumb.label, crumb.label_key, i18n);
        let crumb_el = div()
            .id(("browser-crumb", i))
            .min_w(px(0.0))
            .truncate()
            .px(px(space::HAIR))
            .rounded(px(radius::MICRO))
            .text_size(px(typography::UI_XS));
        bar = bar.child(if i == last {
            // Where you are: a readout, not a jump.
            crumb_el
                .flex_shrink_0()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(Colors::text_primary())
                .child(label)
        } else {
            let target = crumb.path.clone();
            let reveal = on_reveal.clone();
            crumb_el
                .role(Role::Button)
                .aria_label(label.clone())
                .text_color(Colors::text_muted())
                .cursor(gpui::CursorStyle::PointingHand)
                .hover(move |s| s.bg(hover))
                .active(move |s| s.bg(pressed))
                .on_click(move |_e, w, cx| reveal(&target, w, cx))
                .child(label)
        });
    }
    Some(bar)
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
        .pl(px(PLATE_INSET_X))
        // The scrollbar overlays the list's right edge; keep the row's
        // trailing size clear of it.
        .pr(px(crate::components::scrollbar::THICKNESS))
        .py(px(PLATE_INSET_Y))
}

/// A quiet uppercase caption with a chevron: group headers (Places, Drives, a
/// vendor) and, on tabs without groups, the tab's root folders.
#[allow(clippy::too_many_arguments)]
fn caption_plate(
    id: (&'static str, usize),
    label: String,
    expandable: bool,
    expanded: bool,
    selected: bool,
    on_click: Option<Box<dyn Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static>>,
) -> Stateful<Div> {
    let base = Colors::surface_panel();
    let hover = Colors::composite(base, Colors::state_hover());
    let pressed = Colors::composite(base, Colors::state_recessed());
    row_shell(id)
        .px(px(PLATE_PAD_X))
        .gap(px(space::TIGHT))
        .when_some(on_click, |b, on_click| {
            b.cursor(gpui::CursorStyle::PointingHand)
                .hover(move |s| s.bg(hover))
                .active(move |s| s.bg(pressed))
                .on_click(on_click)
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
            div()
                .flex_1()
                .min_w(px(0.0))
                .overflow_hidden()
                .truncate()
                .text_size(px(typography::DENSE_CAPTION))
                .font_weight(gpui::FontWeight::BOLD)
                .text_color(if selected {
                    Colors::text_primary()
                } else {
                    Colors::text_faint()
                })
                .child(label.to_uppercase()),
        )
}

/// Collapsible section header (Places / Drives / a vendor).
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
    let on_toggle = callbacks.on_toggle.clone();
    let plate = caption_plate(
        ("browser-group", index),
        node_label(&node.label, node.label_key, i18n),
        node.expandable,
        node.expanded,
        false,
        node.expandable.then(|| {
            Box::new(move |_e: &gpui::ClickEvent, w: &mut Window, cx: &mut App| {
                on_toggle(&(id.clone(), None), w, cx)
            }) as Box<dyn Fn(&gpui::ClickEvent, &mut Window, &mut App)>
        }),
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

/// A tab's root folder drawn as a caption, on tabs without group headers. It
/// still opens, selects and takes the context menu like any folder row.
fn root_caption_row(
    index: usize,
    node: &BrowserVisibleNode,
    callbacks: &BrowserCallbacks,
    tree_focus: &FocusHandle,
    i18n: I18n,
) -> impl IntoElement {
    let label = node_label(&node.label, node.label_key, i18n);
    let node_id = node.id.clone();
    let path = node.path.clone();
    let context_path = node.path.clone();
    let on_select = callbacks.on_select.clone();
    let on_toggle = callbacks.on_toggle.clone();
    let on_context = callbacks.on_context_menu.clone();
    let focus = tree_focus.clone();
    let plate = caption_plate(
        ("browser-root", index),
        label.clone(),
        node.expandable,
        node.expanded,
        node.selected,
        Some(Box::new(move |_e, window, cx| {
            focus.focus(window, cx);
            if let Some(p) = path.as_ref() {
                on_select(p, window, cx);
            }
            on_toggle(&(node_id.clone(), path.clone()), window, cx);
        })),
    )
    .role(Role::TreeItem)
    .aria_label(label)
    .aria_selected(node.selected)
    .aria_expanded(node.expanded)
    .on_mouse_down(
        gpui::MouseButton::Right,
        move |event: &gpui::MouseDownEvent, window, cx| {
            let x: f32 = event.position.x.into();
            let y: f32 = event.position.y.into();
            on_context(&(context_path.clone(), x, y), window, cx);
        },
    );
    row_frame().child(plate)
}

/// Non-interactive hint row — an honest empty state, a pending listing, or a
/// directory that could not be read.
fn info_row(node: &BrowserVisibleNode, caption_depth: usize, i18n: I18n) -> impl IntoElement {
    let is_error = node.error.is_some();
    let depth = indent_steps(node.depth, caption_depth);
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

/// What a row shows at its end: an instrument's format, "MIDI" for a MIDI
/// file, otherwise the file's size.
fn row_meta(node: &BrowserVisibleNode) -> Option<String> {
    if let Some(detail) = node.detail.clone() {
        return Some(detail);
    }
    if node.kind != BrowserNodeKind::File {
        return None;
    }
    if node.is_midi() {
        return Some("MIDI".to_string());
    }
    node.size_bytes.map(format_size)
}

#[allow(clippy::too_many_arguments)]
fn tree_row(
    index: usize,
    node: &BrowserVisibleNode,
    total: usize,
    caption_depth: usize,
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
    let depth = indent_steps(node.depth, caption_depth);

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

    let text_color = if selected || is_folder {
        Colors::text_primary()
    } else if node.icon == BrowserIcon::Instruments
        || node.is_audio()
        || node.is_midi()
        || node.is_plugin_preset()
        || node.is_video()
    {
        Colors::text_secondary()
    } else {
        Colors::text_faint()
    };
    let label_weight = if is_folder || selected {
        gpui::FontWeight::MEDIUM
    } else {
        gpui::FontWeight::NORMAL
    };

    let icon_path = if node.is_audio() {
        assets::ICON_AUDIO_LINES_PATH
    } else {
        browser_icon_path(node.icon, expanded)
    };
    let icon_color = if selected {
        Colors::accent_primary()
    } else if is_folder {
        Colors::text_muted()
    } else {
        Colors::text_faint()
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
                .w(px(2.0))
                .rounded(px(radius::PILL))
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
        )
        .children(row_meta(node).map(|meta| {
            div()
                .flex_shrink_0()
                .font_features(tabular_figures())
                .text_size(px(typography::DENSE_CAPTION))
                .text_color(if selected {
                    Colors::text_secondary()
                } else {
                    Colors::text_faint()
                })
                .child(meta)
        }));

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
        BrowserIcon::Instruments => assets::ICON_KEYBOARD_PATH,
        BrowserIcon::Plugins | BrowserIcon::PresetFile => assets::ICON_PLUG_PATH,
        BrowserIcon::AudioFiles | BrowserIcon::Music | BrowserIcon::AudioFile => {
            assets::ICON_MUSIC_PATH
        }
        BrowserIcon::Projects | BrowserIcon::ProjectFile => assets::ICON_SAVE_PATH,
        BrowserIcon::Templates => assets::ICON_NEWSPAPER_PATH,
        BrowserIcon::Rigs => assets::ICON_SHARE_PATH,
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
// Preview card
// ---------------------------------------------------------------------------

/// "0:04.0 · 48 kHz · Stereo" from decoded peaks.
fn audio_facts(preview: &waveform_cache::WaveformPreview) -> Vec<String> {
    let secs = preview.duration_seconds.max(0.0);
    let khz = preview.sample_rate as f32 / 1000.0;
    vec![
        format!("{}:{:04.1}", (secs as u64) / 60, secs % 60.0),
        if khz.fract() == 0.0 {
            format!("{khz:.0} kHz")
        } else {
            format!("{khz:.1} kHz")
        },
        match preview.channels {
            1 => "Mono".to_string(),
            2 => "Stereo".to_string(),
            n => format!("{n} ch"),
        },
    ]
}

/// Everything known about the current selection: name and facts, for audio
/// the decoded waveform with play / stop, and the one action a double-click
/// would take.
fn preview_card(
    state: &FileBrowserState,
    node: &BrowserVisibleNode,
    callbacks: &BrowserCallbacks,
    i18n: I18n,
) -> impl IntoElement {
    let label = node_label(&node.label, node.label_key, i18n);
    let audio_path = state.selected_audio_path().map(|p| p.to_path_buf());
    let instrument = node
        .path
        .as_deref()
        .and_then(|path| state.instrument_for(path));
    let preview = audio_path
        .as_ref()
        .and_then(|p| waveform_cache::get_preview_arc(&p.to_string_lossy()));
    let failed = audio_path
        .as_deref()
        .is_some_and(|p| state.waveform_failed(p));
    let playing = audio_path.is_some() && state.preview_playing.as_deref() == audio_path.as_deref();
    let playhead = audio_path
        .as_deref()
        .and_then(|p| state.preview_position_for(p));

    let mut facts: Vec<String> = Vec::new();
    if let Some(instrument) = instrument {
        facts.extend(
            [
                instrument.vendor.as_str(),
                instrument.format.as_str(),
                instrument.category.as_str(),
            ]
            .into_iter()
            .filter(|part| !part.trim().is_empty())
            .map(str::to_string),
        );
    } else {
        match preview.as_ref() {
            Some(p) => facts.extend(audio_facts(p)),
            // The extension is in the name; it is only worth repeating when
            // nothing was decoded.
            None if !node.extension.is_empty() => facts.push(node.extension.to_uppercase()),
            None => {}
        }
        if let Some(bytes) = node.size_bytes {
            facts.push(format_size(bytes));
        }
    }

    // Play / stop for audio. Stop ends this file's audition; play starts it
    // whether or not auto-preview is on.
    let play = audio_path.clone().map(|path| {
        let audition = callbacks.on_audition.clone();
        let stop = callbacks.on_stop_preview.clone();
        let tip = if playing {
            i18n.tr_or("browser.preview.stop", "Stop preview")
        } else {
            i18n.tr_or("browser.preview.play", "Play preview")
        };
        let rest = if playing {
            Colors::accent_primary()
        } else {
            Colors::surface_raised()
        };
        let hover = Colors::composite(rest, Colors::state_hover());
        div()
            .id("browser-preview-play")
            .role(Role::Button)
            .aria_label(tip.clone())
            .flex()
            .items_center()
            .justify_center()
            .flex_shrink_0()
            .w(px(size::PROMINENT))
            .h(px(size::PROMINENT))
            .rounded(px(radius::PILL))
            .bg(rest)
            .border(px(1.0))
            .border_color(Colors::border_subtle())
            .cursor(gpui::CursorStyle::PointingHand)
            .hover(move |s| s.bg(hover))
            .tooltip(fb_tooltip(tip))
            .on_click(move |_e, w, cx| {
                if playing {
                    stop(w, cx);
                } else {
                    audition(&path, w, cx);
                }
            })
            .child(
                svg()
                    .path(if playing {
                        assets::ICON_SQUARE_PATH
                    } else {
                        assets::ICON_PLAY_PATH
                    })
                    .w(px(12.0))
                    .h(px(12.0))
                    .text_color(if playing {
                        Colors::on_accent()
                    } else {
                        Colors::text_primary()
                    }),
            )
    });

    let waveform = audio_path.as_ref().map(|_| {
        let body: gpui::AnyElement = match preview.clone() {
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
        // Square: a waveform's content mask is an axis-aligned rect.
        div()
            .relative()
            .h(px(WAVEFORM_H))
            .w_full()
            .overflow_hidden()
            .bg(Colors::surface_input())
            .child(body)
    });

    // What a double-click does, offered as a button — and only where it does
    // something (audio onto a track, a plug-in preset loaded); a drag-only
    // file says so instead.
    let is_file = node.kind == BrowserNodeKind::File;
    let path = node.path.clone();
    let action: Option<(String, String)> = if !is_file {
        None
    } else if audio_path.is_some() {
        Some((
            i18n.tr_or("browser.preview.add-hint", "Double-click or drag to add"),
            i18n.tr_or("browser.preview.add-to-track", "Add to track"),
        ))
    } else if node.is_plugin_preset() {
        Some((
            if instrument.is_some() {
                i18n.tr_or("browser.instruments.hint.short", "Adds on a new track")
            } else {
                i18n.tr_or("browser.preview.add-hint", "Double-click or drag to add")
            },
            i18n.tr_or("browser.preview.add-plugin", "Add plug-in"),
        ))
    } else {
        None
    };
    let drag_only = is_file
        && action.is_none()
        && path
            .as_deref()
            .is_some_and(|p| drag_kind(node, p).is_some());
    let footer = if let (Some((hint, button)), Some(path)) = (action, path) {
        let activate = callbacks.on_activate_file.clone();
        Some(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(px(space::SNUG))
                .child(
                    div()
                        .min_w(px(0.0))
                        .truncate()
                        .text_size(px(typography::DENSE_CAPTION))
                        .text_color(Colors::text_faint())
                        .child(hint),
                )
                .child(div().flex_shrink_0().child(fb_button(
                    "browser-preview-add",
                    button,
                    FbButtonKind::Default,
                    true,
                    move |_e, w, cx| activate(&path, w, cx),
                )))
                .into_any_element(),
        )
    } else if drag_only {
        Some(
            div()
                .text_size(px(typography::DENSE_CAPTION))
                .text_color(Colors::text_faint())
                .child(i18n.tr_or(
                    "browser.preview.drag-hint",
                    "Drag onto the arrangement to add",
                ))
                .into_any_element(),
        )
    } else {
        None
    };

    div()
        .flex_shrink_0()
        .m(px(space::BASE))
        .flex()
        .flex_col()
        .gap(px(space::BASE))
        .p(px(space::BASE))
        .rounded(px(radius::SURFACE))
        .bg(Colors::surface_panel_alt())
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::BASE))
                .children(play)
                .when(audio_path.is_none(), |row| {
                    row.child(
                        svg()
                            .path(browser_icon_path(node.icon, node.expanded))
                            .flex_shrink_0()
                            .w(px(GLYPH))
                            .h(px(GLYPH))
                            .text_color(Colors::text_muted()),
                    )
                })
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .min_w(px(0.0))
                        .child(
                            div()
                                .truncate()
                                .text_size(px(typography::UI_SM))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(Colors::text_primary())
                                .child(label),
                        )
                        .when(!facts.is_empty(), |col| {
                            col.child(
                                div()
                                    .truncate()
                                    .font_features(tabular_figures())
                                    .text_size(px(typography::DENSE_LABEL))
                                    .text_color(Colors::text_muted())
                                    .child(facts.join(" · ")),
                            )
                        }),
                ),
        )
        .children(waveform)
        .children(footer)
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
                let left: f32 = bounds.origin.x.into();
                let top: f32 = bounds.origin.y.into();
                crate::components::timeline::playhead::paint_playhead_line(
                    window,
                    left + x.min(w - 1.0),
                    top,
                    top + h,
                    1.0,
                );
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

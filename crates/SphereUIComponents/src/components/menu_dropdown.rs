//! Native GPUI port of the WebUI top-menu dropdown panel.
//!
//! Source of truth for the visuals is `apps/web/src/components/TransportBar.tsx`'s
//! `MenuPanel`. Matching tokens:
//!
//! * panel background  = `daw-surface`
//! * panel border      = `daw-border`
//! * panel shadow      = `0 12px 36px rgba(0,0,0,0.52)`
//! * panel padding     = 6 px (Tailwind `p-1.5`)
//! * panel width       = content sized, at least 220 px
//! * item height       = 28 px (Tailwind `h-7`)
//! * item radius       = 7 px (Tailwind `rounded-[7px]`)
//! * item text         = 12 px, `daw-text` (or `daw-red` when `danger`)
//! * item hover        = `daw-surface-high`
//! * item disabled     = ~35 % opacity
//! * shortcut          = 11 px, `daw-faint`, right aligned
//! * separator         = 1 px horizontal rule, 4 px vertical margin
//!
//! Submenus open on hover and also respond to click. The open submenu path
//! lives in `MenuBarUiState` and threads through the same dropdown render.

use std::sync::Arc;

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, svg, App, InteractiveElement, IntoElement, ParentElement, StatefulInteractiveElement,
    Styled, Window,
};

use crate::assets;
use crate::components::title_bar::TITLEBAR_HEIGHT;
use crate::i18n::I18n;
use crate::menu::{Menu, MenuItem, MenuItemKind};
use crate::overlay::{
    compute_overlay_position, OverlayAnchor, OverlayPlacement, OverlaySize, OVERLAY_WINDOW_MARGIN,
};
use crate::theme::{menu as menu_style, Colors};

pub const TOP_CHROME_HEIGHT: f32 = TITLEBAR_HEIGHT;

const MENU_PANEL_EDGE_GAP: f32 = 0.0;
const MENU_SUBMENU_GAP: f32 = 0.0;
const MENU_MIN_VISIBLE_HEIGHT: f32 = 20.0;
const SEPARATOR_HEIGHT: f32 = menu_style::SEPARATOR_MARGIN_Y * 2.0 + 1.0;

pub type CommandCb = Arc<dyn Fn(&String, &mut Window, &mut App) + 'static>;
pub type CloseCb = Arc<dyn Fn(&(), &mut Window, &mut App) + 'static>;
/// `(depth, submenu_id)` — depth is the index in the open path that the
/// caller wants to toggle. Setting a fresh id at `depth` truncates the
/// path beyond `depth`; toggling the same id closes the submenu.
pub type ToggleSubmenuCb = Arc<dyn Fn(&(usize, String), &mut Window, &mut App) + 'static>;

/// Render the dropdown overlay rooted at `menu`, anchored under the clicked
/// top-level label. `submenu_path` is the current open path *below* the
/// top level — i.e. `submenu_path[0]` is the id of the submenu open in the
/// root panel, `submenu_path[1]` the id of the submenu open in *that*
/// panel, etc.
pub fn menu_dropdown(
    menu: &Menu,
    anchor: OverlayAnchor,
    viewport_width: f32,
    viewport_height: f32,
    submenu_path: &[String],
    on_toggle_submenu: ToggleSubmenuCb,
    on_command: CommandCb,
    on_close: CloseCb,
    i18n: I18n,
) -> impl IntoElement {
    let window_bounds = gpui::bounds(
        gpui::point(px(0.0), px(0.0)),
        gpui::size(px(viewport_width), px(viewport_height)),
    );
    let root_width = panel_width_for_items(&menu.items, i18n);
    let root_height = panel_height_for_items(&menu.items);
    let root_pos = compute_overlay_position(
        anchor.bounds,
        OverlaySize {
            width: root_width,
            height: root_height.max(MENU_MIN_VISIBLE_HEIGHT),
        },
        window_bounds,
        OverlayPlacement::BottomStart,
        MENU_PANEL_EDGE_GAP,
    );
    let root_left: f32 = root_pos.x.into();
    let root_top: f32 = root_pos.y.into();

    // Build the chain of panels: root, then for each open submenu in the
    // path, the nested panel positioned to the right of its parent.
    let mut panels: Vec<gpui::AnyElement> = Vec::new();
    let mut items_ref: &[MenuItem] = &menu.items;
    let mut panel_left = root_left;
    let mut panel_top = root_top;

    for depth in 0..=submenu_path.len() {
        let current_width = panel_width_for_items(items_ref, i18n);
        panels.push(
            panel_view(
                depth,
                items_ref,
                panel_left,
                panel_top,
                current_width,
                submenu_path.get(depth).cloned(),
                on_toggle_submenu.clone(),
                on_command.clone(),
                on_close.clone(),
                i18n,
            )
            .into_any_element(),
        );

        // Walk into the next submenu level if the path requests it.
        let Some(next_id) = submenu_path.get(depth) else {
            break;
        };
        let Some(trigger_index) = items_ref
            .iter()
            .position(|it| it.kind == MenuItemKind::Submenu && &it.id == next_id)
        else {
            break;
        };
        let submenu = &items_ref[trigger_index];

        // Align the child panel's top edge with the trigger row's top in
        // the parent panel, matching the WebUI DOMRect-based submenu anchor.
        let trigger_y = trigger_top_offset(items_ref, trigger_index);
        items_ref = &submenu.children;
        let child_width = panel_width_for_items(items_ref, i18n);
        panel_top = clamp_top(panel_top + trigger_y, viewport_height);
        panel_left = submenu_left(panel_left, current_width, child_width, viewport_width);
    }

    // Click-blocking backdrop behind every panel. Starts below the titlebar so
    // a second click on the open menu label (or another menu title) reaches the
    // menubar toggle instead of closing-then-reopening via event fallthrough.
    let backdrop_close = on_close.clone();
    let backdrop = div()
        .absolute()
        .top(px(TITLEBAR_HEIGHT))
        .left_0()
        .right_0()
        .bottom_0()
        .id("menu-dropdown-backdrop")
        .on_mouse_down(gpui::MouseButton::Left, move |_, w, cx| {
            backdrop_close(&(), w, cx);
            cx.stop_propagation();
        });

    div().absolute().inset_0().child(backdrop).children(panels)
}

fn clamp_left(left: f32, width: f32, viewport_width: f32) -> f32 {
    let max_left = (viewport_width - width - OVERLAY_WINDOW_MARGIN).max(OVERLAY_WINDOW_MARGIN);
    left.clamp(OVERLAY_WINDOW_MARGIN, max_left)
}

fn clamp_top(top: f32, viewport_height: f32) -> f32 {
    let max_top =
        (viewport_height - MENU_MIN_VISIBLE_HEIGHT - MENU_PANEL_EDGE_GAP).max(MENU_PANEL_EDGE_GAP);
    top.clamp(MENU_PANEL_EDGE_GAP, max_top)
}

fn submenu_left(parent_left: f32, parent_width: f32, child_width: f32, viewport_width: f32) -> f32 {
    let preferred = parent_left + parent_width + MENU_SUBMENU_GAP;
    if preferred + child_width + MENU_PANEL_EDGE_GAP <= viewport_width {
        preferred
    } else {
        clamp_left(
            parent_left - child_width - MENU_SUBMENU_GAP,
            child_width,
            viewport_width,
        )
    }
}

/// Y offset (in px) of the row at `trigger_index` relative to the panel's
/// top edge, accounting for panel padding, item gaps, and the smaller
/// height of separator rows.
fn trigger_top_offset(items: &[MenuItem], trigger_index: usize) -> f32 {
    let mut y = menu_style::PANEL_PAD;
    for (i, item) in items.iter().enumerate() {
        if i == trigger_index {
            break;
        }
        if !item.visible {
            continue;
        }
        let h = match item.kind {
            MenuItemKind::Separator => SEPARATOR_HEIGHT,
            _ => menu_style::ROW_HEIGHT,
        };
        y += h + menu_style::ITEM_GAP;
    }
    y
}

fn panel_width_for_items(items: &[MenuItem], i18n: I18n) -> f32 {
    let has_check = items
        .iter()
        .any(|it| it.visible && it.kind == MenuItemKind::Checkbox);
    let mut width: f32 = menu_style::PANEL_MIN_WIDTH;

    for item in items.iter().filter(|it| it.visible) {
        if item.kind == MenuItemKind::Separator {
            continue;
        }

        let label = i18n.tr_menu(&item.id, item.label.as_deref().unwrap_or(""));
        let shortcut_chars = item.shortcut.as_deref().unwrap_or_default().chars().count() as f32;
        let left_slot = if has_check {
            menu_style::CHECK_SLOT_W + 6.0
        } else {
            0.0
        };
        let submenu_slot = if item.kind == MenuItemKind::Submenu {
            menu_style::CHEVRON_SIZE + 8.0
        } else {
            0.0
        };
        let shortcut_slot = if shortcut_chars > 0.0 {
            14.0 + shortcut_chars * 6.0
        } else {
            0.0
        };

        // Approximate text width per script. GPUI rows do not currently
        // auto-size popovers from text contents, so this preserves the WebUI
        // content-driven feel without making the panel a hard narrow width.
        let label_slot = menu_style::estimate_label_width(&label);
        let needed = menu_style::PANEL_PAD * 2.0
            + menu_style::ROW_PAD_X * 2.0
            + left_slot
            + label_slot
            + shortcut_slot
            + submenu_slot;
        width = width.max(needed);
    }

    width.clamp(menu_style::PANEL_MIN_WIDTH, menu_style::PANEL_MAX_WIDTH)
}

fn panel_height_for_items(items: &[MenuItem]) -> f32 {
    let content = items
        .iter()
        .filter(|it| it.visible)
        .map(|item| match item.kind {
            MenuItemKind::Separator => SEPARATOR_HEIGHT,
            _ => menu_style::ROW_HEIGHT + menu_style::ITEM_GAP,
        })
        .sum::<f32>();
    menu_style::PANEL_PAD * 2.0 + content
}

fn panel_shadow() -> Vec<gpui::BoxShadow> {
    vec![gpui::BoxShadow {
        color: Colors::surface_overlay().into(),
        offset: gpui::point(px(0.0), px(12.0)),
        blur_radius: px(36.0),
        spread_radius: px(0.0),
        inset: false,
    }]
}

fn panel_view(
    depth: usize,
    items: &[MenuItem],
    left: f32,
    top: f32,
    width: f32,
    open_child_id: Option<String>,
    on_toggle_submenu: ToggleSubmenuCb,
    on_command: CommandCb,
    on_close: CloseCb,
    i18n: I18n,
) -> impl IntoElement {
    // Per-panel decision: only reserve a left check-slot if the panel
    // actually contains a checkbox item. That removes the empty gutter
    // from menus like File / Project that are pure command lists.
    let has_check = items
        .iter()
        .any(|it| it.visible && it.kind == MenuItemKind::Checkbox);

    let mut item_els: Vec<gpui::AnyElement> = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        if !item.visible {
            continue;
        }
        match item.kind {
            MenuItemKind::Separator => item_els.push(separator().into_any_element()),
            _ => item_els.push(
                menu_item_row(
                    depth,
                    i,
                    item,
                    has_check,
                    open_child_id.as_deref(),
                    on_toggle_submenu.clone(),
                    on_command.clone(),
                    on_close.clone(),
                    i18n,
                )
                .into_any_element(),
            ),
        }
    }

    div()
        .absolute()
        .top(px(top))
        .left(px(left))
        .w(px(width))
        .id(("menu-dropdown-panel", (depth + 1) * 1000 + left as usize))
        .flex()
        .flex_col()
        .gap(px(1.0))
        .p(px(menu_style::PANEL_PAD))
        .rounded(px(crate::theme::radius::CONTROL))
        .bg(Colors::surface_panel())
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .shadow(panel_shadow())
        // Soaks up clicks so they don't bubble to the backdrop.
        .occlude()
        .children(item_els)
}

fn separator() -> impl IntoElement {
    div()
        .my(px(menu_style::SEPARATOR_MARGIN_Y))
        .h(px(1.0))
        .bg(Colors::border_subtle())
}

fn menu_item_row(
    depth: usize,
    index: usize,
    item: &MenuItem,
    panel_has_check: bool,
    open_child_id: Option<&str>,
    on_toggle_submenu: ToggleSubmenuCb,
    on_command: CommandCb,
    on_close: CloseCb,
    i18n: I18n,
) -> impl IntoElement {
    let enabled = item.enabled;
    let label = i18n.tr_menu(&item.id, item.label.as_deref().unwrap_or(""));
    let shortcut = item.shortcut.clone();
    let is_submenu = item.kind == MenuItemKind::Submenu;
    let is_checkbox = item.kind == MenuItemKind::Checkbox;
    let is_checked = is_checkbox && item.checked;
    let is_open_submenu = is_submenu && open_child_id == Some(item.id.as_str());
    let command = item.command.clone();
    let item_id = item.id.clone();

    let text_color = if !enabled {
        let mut c = Colors::text_secondary();
        c.a = 0.35;
        c
    } else if item.danger {
        Colors::status_error()
    } else {
        Colors::text_secondary()
    };
    let shortcut_color = {
        let mut c = Colors::text_faint();
        if !enabled {
            c.a = 0.35;
        }
        c
    };

    let on_close_click = on_close.clone();
    let on_toggle_click = on_toggle_submenu.clone();

    // ── Left cluster: optional check slot + label ───────────────────────
    let check_slot: Option<gpui::Div> = if panel_has_check {
        Some(
            div()
                .flex()
                .items_center()
                .justify_center()
                .w(px(menu_style::CHECK_SLOT_W))
                .h(px(menu_style::ROW_HEIGHT))
                .flex_none()
                .child(if is_checked {
                    svg()
                        .path(assets::ICON_CHECK_PATH)
                        .w(px(menu_style::ICON_SIZE))
                        .h(px(menu_style::ICON_SIZE))
                        .text_color(Colors::accent_primary())
                        .into_any_element()
                } else {
                    div().into_any_element()
                }),
        )
    } else {
        None
    };

    let mut left = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .flex_1()
        .min_w(px(0.0));
    if let Some(slot) = check_slot {
        left = left.child(slot);
    }
    left = left.child(
        div()
            .flex_1()
            .min_w(px(0.0))
            .h(px(menu_style::ROW_HEIGHT))
            .flex()
            .items_center()
            .gap(px(6.0))
            .text_size(px(menu_style::LABEL_TEXT_SIZE))
            .text_color(text_color)
            .child(if item.detail.is_some() {
                div().flex_none().child(label)
            } else {
                div().min_w(px(0.0)).truncate().child(label)
            })
            // What the item acts on right now ("Undo  Paste FX Chain"): quieter
            // than the label, and the part that gives way when space runs out.
            .when_some(item.detail.clone(), |row, detail| {
                row.child(
                    div()
                        .min_w(px(0.0))
                        .truncate()
                        .text_color(shortcut_color)
                        .child(detail),
                )
            }),
    );

    // ── Right cluster: shortcut / chevron ────────────────────────────────
    let mut right = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .flex_none()
        .pl(px(12.0));
    if let Some(sc) = shortcut {
        right = right.child(
            div()
                .text_size(px(menu_style::META_TEXT_SIZE))
                .h(px(menu_style::ROW_HEIGHT))
                .flex()
                .items_center()
                .text_color(shortcut_color)
                .child(sc),
        );
    }
    if is_submenu {
        right = right.child(
            svg()
                .path(assets::ICON_CHEVRON_RIGHT_PATH)
                .w(px(menu_style::CHEVRON_SIZE))
                .h(px(menu_style::CHEVRON_SIZE))
                .text_color(Colors::text_faint()),
        );
    }

    // ── Row container ────────────────────────────────────────────────────
    let mut row = div()
        .id(("menu-item", depth * 10_000 + index))
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .h(px(menu_style::ROW_HEIGHT))
        .w_full()
        .px(px(menu_style::ROW_PAD_X))
        .rounded(px(crate::theme::radius::CONTROL))
        .bg(if is_open_submenu {
            Colors::surface_control_hover()
        } else {
            gpui::transparent_black().into()
        })
        .opacity(if enabled { 1.0 } else { 0.45 })
        .text_size(px(menu_style::LABEL_TEXT_SIZE))
        .child(left)
        .child(right);

    if enabled {
        row = row
            .cursor(gpui::CursorStyle::PointingHand)
            .hover(|s| s.bg(Colors::surface_control_hover()));
        if is_submenu {
            let hover_toggle = on_toggle_submenu.clone();
            let hover_item_id = item_id.clone();
            row = row
                .on_hover(move |hovered, w, cx| {
                    if *hovered && !is_open_submenu {
                        hover_toggle(&(depth, hover_item_id.clone()), w, cx);
                    }
                })
                .on_click(move |_, w, cx| {
                    on_toggle_click(&(depth, item_id.clone()), w, cx);
                });
        } else {
            row = row.on_click(move |_, w, cx| {
                if let Some(cmd) = command.as_ref() {
                    on_command(cmd, w, cx);
                }
                on_close_click(&(), w, cx);
            });
        }
    }

    row
}

use std::sync::Arc;

use gpui::{
    div, px, svg, App, InteractiveElement, IntoElement, ParentElement, StatefulInteractiveElement,
    Styled, Window,
};

use crate::assets;
use crate::overlay::{
    compute_overlay_position, pointer_anchor, OverlayPlacement, OverlaySize, OVERLAY_WINDOW_MARGIN,
};
use crate::theme::{menu as menu_style, Colors};

pub type ContextCommandCb = Arc<dyn Fn(&String, &mut Window, &mut App) + 'static>;
pub type ContextCloseCb = Arc<dyn Fn(&(), &mut Window, &mut App) + 'static>;

const EDGE_GAP: f32 = OVERLAY_WINDOW_MARGIN;

#[derive(Clone, Debug)]
pub enum ContextMenuEntry {
    Header(String),
    Separator,
    Item {
        label: String,
        command: String,
        shortcut: Option<String>,
        checked: bool,
        disabled: bool,
        danger: bool,
    },
}

impl ContextMenuEntry {
    pub fn item(label: impl Into<String>, command: impl Into<String>) -> Self {
        Self::Item {
            label: label.into(),
            command: command.into(),
            shortcut: None,
            checked: false,
            disabled: false,
            danger: false,
        }
    }

    pub fn disabled_item(label: impl Into<String>, command: impl Into<String>) -> Self {
        Self::Item {
            label: label.into(),
            command: command.into(),
            shortcut: None,
            checked: false,
            disabled: true,
            danger: false,
        }
    }

    pub fn checked_item(
        label: impl Into<String>,
        command: impl Into<String>,
        checked: bool,
    ) -> Self {
        Self::Item {
            label: label.into(),
            command: command.into(),
            shortcut: None,
            checked,
            disabled: false,
            danger: false,
        }
    }

    pub fn danger_item(label: impl Into<String>, command: impl Into<String>) -> Self {
        Self::Item {
            label: label.into(),
            command: command.into(),
            shortcut: None,
            checked: false,
            disabled: false,
            danger: true,
        }
    }

    pub fn with_shortcut(mut self, shortcut: impl Into<String>) -> Self {
        if let Self::Item {
            shortcut: target, ..
        } = &mut self
        {
            *target = Some(shortcut.into());
        }
        self
    }

    /// Fill in this item's shortcut hint from `lookup` unless it already carries
    /// one (an explicit `with_shortcut` always wins) or the item is disabled or
    /// a non-dispatching `noop`. `lookup` maps a command id to its display
    /// accelerator, e.g. `KeymapManager::shortcut_for_command`.
    pub fn backfill_shortcut(mut self, lookup: impl Fn(&str) -> Option<String>) -> Self {
        if let Self::Item {
            command,
            shortcut,
            disabled,
            ..
        } = &mut self
        {
            if shortcut.is_none() && !*disabled && command != "noop" {
                *shortcut = lookup(command);
            }
        }
        self
    }
}

/// Backfill shortcut hints on every entry in place, leaving explicit shortcuts,
/// disabled rows, headers, separators, and `noop` items untouched. One call
/// annotates a whole context menu from the active keymap.
pub fn backfill_shortcuts(
    entries: Vec<ContextMenuEntry>,
    lookup: impl Fn(&str) -> Option<String>,
) -> Vec<ContextMenuEntry> {
    entries
        .into_iter()
        .map(|entry| entry.backfill_shortcut(&lookup))
        .collect()
}

/// The context menu at `x, y` (window coordinates).
///
/// Where the platform has native popup menus
/// ([`crate::native_popup_menu::available`]) the menu is the system's: this
/// element draws nothing and, when first painted, brings the native menu up
/// at the same point. The caller's open state stays as it is until the menu
/// closes; then `on_command` runs for a choice, and `on_close` always runs
/// after it, so the state comes down however the menu was left.
pub fn context_menu_overlay(
    entries: Vec<ContextMenuEntry>,
    x: f32,
    y: f32,
    viewport_width: f32,
    viewport_height: f32,
    on_command: ContextCommandCb,
    on_close: ContextCloseCb,
) -> gpui::AnyElement {
    if crate::native_popup_menu::available() {
        return native_context_menu(entries, x, y, on_command, on_close);
    }
    drawn_context_menu(
        entries,
        x,
        y,
        viewport_width,
        viewport_height,
        on_command,
        on_close,
    )
    .into_any_element()
}

/// `entries` as native popup rows.
///
/// A drawn menu groups its rows under `Header` captions; a native one nests
/// them instead. Each captioned group of two or more rows becomes a submenu
/// named by its caption ("ARA ▸", "Track Height ▸"), which keeps a long menu
/// short. A group of one row stays inline (a submenu holding one row is a
/// detour), and so does a menu that is nothing but one group — there the
/// caption stays as a caption.
pub fn popup_items(entries: Vec<ContextMenuEntry>) -> Vec<crate::native_popup_menu::PopupItem> {
    use crate::native_popup_menu::PopupItem;

    fn row(entry: ContextMenuEntry) -> Option<PopupItem> {
        match entry {
            ContextMenuEntry::Header(label) => Some(PopupItem::Header(label)),
            ContextMenuEntry::Separator => Some(PopupItem::Separator),
            ContextMenuEntry::Item {
                label,
                command,
                shortcut,
                checked,
                disabled,
                danger: _,
            } => Some(PopupItem::Command {
                label,
                command,
                shortcut,
                checked,
                enabled: !disabled,
            }),
        }
    }

    // Split into runs: a caption and the rows under it (up to the next
    // separator or caption), or loose rows and separators.
    enum Run {
        Group(String, Vec<ContextMenuEntry>),
        Loose(ContextMenuEntry),
    }
    let mut runs: Vec<Run> = Vec::new();
    let mut open_group = false;
    for entry in entries {
        match entry {
            ContextMenuEntry::Header(caption) => {
                runs.push(Run::Group(caption, Vec::new()));
                open_group = true;
            }
            ContextMenuEntry::Separator => {
                open_group = false;
                runs.push(Run::Loose(ContextMenuEntry::Separator));
            }
            item => match runs.last_mut() {
                Some(Run::Group(_, rows)) if open_group => rows.push(item),
                _ => runs.push(Run::Loose(item)),
            },
        }
    }

    let only_one_group = runs
        .iter()
        .filter(|run| !matches!(run, Run::Loose(ContextMenuEntry::Separator)))
        .count()
        == 1
        && matches!(
            runs.iter()
                .find(|run| !matches!(run, Run::Loose(ContextMenuEntry::Separator))),
            Some(Run::Group(..))
        );

    let mut items = Vec::new();
    for run in runs {
        match run {
            Run::Loose(entry) => items.extend(row(entry)),
            Run::Group(caption, rows) if only_one_group => {
                items.push(PopupItem::Header(caption));
                items.extend(rows.into_iter().filter_map(row));
            }
            Run::Group(_, rows) if rows.len() < 2 => {
                items.extend(rows.into_iter().filter_map(row));
            }
            Run::Group(caption, rows) => {
                let enabled = rows.iter().any(|row| {
                    matches!(
                        row,
                        ContextMenuEntry::Item {
                            disabled: false,
                            ..
                        }
                    )
                });
                items.push(PopupItem::Submenu {
                    label: caption,
                    enabled,
                    items: rows.into_iter().filter_map(row).collect(),
                });
            }
        }
    }
    items
}

fn native_context_menu(
    entries: Vec<ContextMenuEntry>,
    x: f32,
    y: f32,
    on_command: ContextCommandCb,
    on_close: ContextCloseCb,
) -> gpui::AnyElement {
    crate::components::menu_dropdown::native_menu_element(
        gpui::point(px(x), px(y)),
        None,
        popup_items(entries),
        on_command,
        on_close,
    )
}

fn drawn_context_menu(
    entries: Vec<ContextMenuEntry>,
    x: f32,
    y: f32,
    viewport_width: f32,
    viewport_height: f32,
    on_command: ContextCommandCb,
    on_close: ContextCloseCb,
) -> impl IntoElement {
    let width = panel_width_for_entries(&entries);
    let height = panel_height_for_entries(&entries);
    let window_bounds = gpui::bounds(
        gpui::point(px(0.0), px(0.0)),
        gpui::size(px(viewport_width), px(viewport_height)),
    );
    let pos = compute_overlay_position(
        pointer_anchor(x, y).bounds,
        OverlaySize { width, height },
        window_bounds,
        OverlayPlacement::Pointer,
        EDGE_GAP,
    );
    let left: f32 = pos.x.into();
    let top: f32 = pos.y.into();

    let close_backdrop = on_close.clone();
    div()
        .absolute()
        .inset_0()
        .id("context-menu-overlay")
        .child(
            div()
                .absolute()
                .inset_0()
                .on_mouse_down(gpui::MouseButton::Left, move |_, w, cx| {
                    close_backdrop(&(), w, cx)
                })
                .on_mouse_down(gpui::MouseButton::Right, move |_, w, cx| {
                    on_close(&(), w, cx)
                }),
        )
        .child(panel(entries, left, top, width, on_command))
}

fn panel_width_for_entries(entries: &[ContextMenuEntry]) -> f32 {
    let has_check = entries
        .iter()
        .any(|entry| matches!(entry, ContextMenuEntry::Item { checked: true, .. }));
    let mut width = menu_style::PANEL_MIN_WIDTH;
    for entry in entries {
        let ContextMenuEntry::Item {
            label, shortcut, ..
        } = entry
        else {
            continue;
        };
        let left_slot = if has_check {
            menu_style::CHECK_SLOT_W + 6.0
        } else {
            0.0
        };
        let label_slot = menu_style::estimate_label_width(label);
        let shortcut_slot = shortcut
            .as_ref()
            .map(|s| 14.0 + s.chars().count() as f32 * 6.0)
            .unwrap_or(0.0);
        width = width.max(
            menu_style::PANEL_PAD * 2.0
                + menu_style::ROW_PAD_X * 2.0
                + left_slot
                + label_slot
                + shortcut_slot,
        );
    }
    width.clamp(menu_style::PANEL_MIN_WIDTH, menu_style::PANEL_MAX_WIDTH)
}

fn panel_height_for_entries(entries: &[ContextMenuEntry]) -> f32 {
    let content = entries
        .iter()
        .map(|entry| match entry {
            ContextMenuEntry::Separator => menu_style::SEPARATOR_MARGIN_Y * 2.0 + 1.0,
            ContextMenuEntry::Header(_) => menu_style::HEADER_HEIGHT,
            ContextMenuEntry::Item { .. } => menu_style::ROW_HEIGHT + menu_style::ITEM_GAP,
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

fn panel(
    entries: Vec<ContextMenuEntry>,
    left: f32,
    top: f32,
    width: f32,
    on_command: ContextCommandCb,
) -> impl IntoElement {
    let has_check = entries
        .iter()
        .any(|entry| matches!(entry, ContextMenuEntry::Item { checked: true, .. }));
    div()
        .absolute()
        .left(px(left))
        .top(px(top))
        .w(px(width))
        .max_h(px(560.0))
        .id("context-menu-panel")
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .gap(px(1.0))
        .p(px(menu_style::PANEL_PAD))
        .rounded(px(crate::theme::radius::SURFACE))
        .bg(Colors::surface_panel())
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .shadow(panel_shadow())
        .occlude()
        .children(entries.into_iter().enumerate().map(|(index, entry)| {
            match entry {
                ContextMenuEntry::Header(label) => header_row(index, label).into_any_element(),
                ContextMenuEntry::Separator => separator().into_any_element(),
                ContextMenuEntry::Item {
                    label,
                    command,
                    shortcut,
                    checked,
                    disabled,
                    danger,
                } => item_row(
                    index,
                    label,
                    command,
                    shortcut,
                    checked,
                    disabled,
                    danger,
                    has_check,
                    on_command.clone(),
                )
                .into_any_element(),
            }
        }))
}

fn header_row(index: usize, label: String) -> impl IntoElement {
    div()
        .id(("context-menu-header", index))
        .h(px(menu_style::HEADER_HEIGHT))
        .flex()
        .items_center()
        .px(px(8.0))
        .text_size(px(menu_style::HEADER_TEXT_SIZE))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(Colors::text_faint())
        .child(label)
}

fn separator() -> impl IntoElement {
    div()
        .my(px(menu_style::SEPARATOR_MARGIN_Y))
        .h(px(1.0))
        .bg(Colors::border_subtle())
}

#[allow(clippy::too_many_arguments)]
fn item_row(
    index: usize,
    label: String,
    command: String,
    shortcut: Option<String>,
    checked: bool,
    disabled: bool,
    danger: bool,
    panel_has_check: bool,
    on_command: ContextCommandCb,
) -> impl IntoElement {
    let mut text_color = if danger {
        Colors::status_error()
    } else {
        Colors::text_secondary()
    };
    let mut meta_color = Colors::text_faint();
    if disabled {
        text_color.a = 0.35;
        meta_color.a = 0.35;
    }

    let mut row = div()
        .id(("context-menu-item", index))
        .h(px(menu_style::ROW_HEIGHT))
        .w_full()
        .px(px(menu_style::ROW_PAD_X))
        .rounded(px(crate::theme::radius::CONTROL))
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .min_w(px(0.0))
                .flex_1()
                .children(panel_has_check.then(|| {
                    div()
                        .w(px(menu_style::CHECK_SLOT_W))
                        .h(px(menu_style::ROW_HEIGHT))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(if checked {
                            svg()
                                .path(assets::ICON_CHECK_PATH)
                                .w(px(menu_style::ICON_SIZE))
                                .h(px(menu_style::ICON_SIZE))
                                .text_color(Colors::accent_primary())
                                .into_any_element()
                        } else {
                            div().into_any_element()
                        })
                }))
                .child(
                    div()
                        .min_w(px(0.0))
                        .flex_1()
                        .truncate()
                        .text_size(px(menu_style::LABEL_TEXT_SIZE))
                        .text_color(text_color)
                        .child(label),
                ),
        )
        .child(
            div()
                .flex_none()
                .pl(px(12.0))
                .text_size(px(menu_style::META_TEXT_SIZE))
                .text_color(meta_color)
                .children(shortcut),
        );

    if !disabled {
        row = row
            .cursor(gpui::CursorStyle::PointingHand)
            .hover(|s| s.bg(Colors::surface_hover()))
            .on_click(move |_, w, cx| on_command(&command, w, cx));
    }

    row
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_popup_menu::PopupItem;

    fn labels(items: &[PopupItem]) -> Vec<String> {
        items
            .iter()
            .map(|item| match item {
                PopupItem::Command { label, .. } => label.clone(),
                PopupItem::Header(label) => format!("# {label}"),
                PopupItem::Separator => "-".to_string(),
                PopupItem::Submenu { label, items, .. } => {
                    format!("{label} > [{}]", labels(items).join(", "))
                }
            })
            .collect()
    }

    #[test]
    fn captioned_groups_nest_as_submenus() {
        let items = popup_items(vec![
            ContextMenuEntry::item("Rename", "rename"),
            ContextMenuEntry::Separator,
            ContextMenuEntry::Header("Folder".into()),
            ContextMenuEntry::item("Group Selected", "group"),
            ContextMenuEntry::Separator,
            ContextMenuEntry::Header("ARA".into()),
            ContextMenuEntry::item("Melodyne", "ara:1"),
            ContextMenuEntry::item("RePitch", "ara:2"),
            ContextMenuEntry::Separator,
            ContextMenuEntry::item("Add Audio Track", "add"),
        ]);
        assert_eq!(
            labels(&items),
            vec![
                "Rename",
                "-",
                "Group Selected",
                "-",
                "ARA > [Melodyne, RePitch]",
                "-",
                "Add Audio Track"
            ]
        );
    }

    #[test]
    fn a_menu_that_is_one_group_stays_flat() {
        let items = popup_items(vec![
            ContextMenuEntry::Header("Pad".into()),
            ContextMenuEntry::item("Load", "load"),
            ContextMenuEntry::item("Clear", "clear"),
        ]);
        assert_eq!(labels(&items), vec!["# Pad", "Load", "Clear"]);
    }

    #[test]
    fn a_group_of_disabled_rows_is_a_disabled_submenu() {
        let items = popup_items(vec![
            ContextMenuEntry::item("Rename", "rename"),
            ContextMenuEntry::Header("ARA".into()),
            ContextMenuEntry::disabled_item("Melodyne", "ara:1"),
            ContextMenuEntry::disabled_item("RePitch", "ara:2"),
        ]);
        assert!(matches!(
            items.last(),
            Some(PopupItem::Submenu { enabled: false, .. })
        ));
    }
}

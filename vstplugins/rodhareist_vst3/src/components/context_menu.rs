//! The context menu, as far as Studio's text field uses it: its Cut / Copy /
//! Paste / Select All menu on a right click.
//!
//! Where the platform has native popup menus
//! ([`crate::native_popup_menu::available`]) the menu is the system's, as in
//! Studio: it is not clipped to the editor window the host gives the plug-in.
//! Elsewhere this draws the same entries as a plain list in Studio's theme, at
//! the click, kept inside the window.

use std::sync::Arc;

use gpui::{
    div, px, App, InteractiveElement, IntoElement, ParentElement, StatefulInteractiveElement,
    Styled, Window,
};

use crate::theme::Colors;

pub type ContextCommandCb = Arc<dyn Fn(&String, &mut Window, &mut App) + 'static>;
pub type ContextCloseCb = Arc<dyn Fn(&(), &mut Window, &mut App) + 'static>;

const ROW_H: f32 = 26.0;
const MENU_W: f32 = 200.0;
const EDGE: f32 = 8.0;

#[derive(Clone, Debug)]
pub enum ContextMenuEntry {
    Header(String),
    Separator,
    Item {
        label: String,
        command: String,
        shortcut: Option<String>,
        disabled: bool,
    },
}

impl ContextMenuEntry {
    pub fn item(label: impl Into<String>, command: impl Into<String>) -> Self {
        Self::Item {
            label: label.into(),
            command: command.into(),
            shortcut: None,
            disabled: false,
        }
    }

    pub fn disabled_item(label: impl Into<String>, command: impl Into<String>) -> Self {
        Self::Item {
            label: label.into(),
            command: command.into(),
            shortcut: None,
            disabled: true,
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
}

/// The menu at `x, y` (window coordinates). A choice runs `on_command`; any
/// press outside, or a choice, then runs `on_close`.
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
    let height: f32 = entries
        .iter()
        .map(|entry| match entry {
            ContextMenuEntry::Separator => 9.0,
            _ => ROW_H,
        })
        .sum::<f32>()
        + 8.0;
    let left = x.min(viewport_width - MENU_W - EDGE).max(EDGE);
    let top = y.min(viewport_height - height - EDGE).max(EDGE);

    let rows = entries
        .into_iter()
        .enumerate()
        .map(|(index, entry)| match entry {
            ContextMenuEntry::Separator => div()
                .h(px(1.0))
                .my(px(4.0))
                .bg(Colors::border_subtle())
                .into_any_element(),
            ContextMenuEntry::Header(label) => div()
                .h(px(ROW_H))
                .px(px(10.0))
                .flex()
                .items_center()
                .text_size(px(11.0))
                .text_color(Colors::text_faint())
                .child(label)
                .into_any_element(),
            ContextMenuEntry::Item {
                label,
                command,
                shortcut,
                disabled,
            } => {
                let mut row = div()
                    .id(("plugin-context-menu-row", index))
                    .h(px(ROW_H))
                    .px(px(10.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .rounded(px(3.0))
                    .text_size(px(12.0))
                    .text_color(if disabled {
                        Colors::text_faint()
                    } else {
                        Colors::text_primary()
                    })
                    .child(label)
                    .children(shortcut.map(|shortcut| {
                        div()
                            .text_size(px(11.0))
                            .text_color(Colors::text_faint())
                            .child(shortcut)
                    }));
                if !disabled {
                    let on_command = on_command.clone();
                    let on_close = on_close.clone();
                    row = row
                        .cursor_pointer()
                        .hover(|style| style.bg(Colors::surface_hover()))
                        .on_click(move |_event, window, cx| {
                            on_command(&command, window, cx);
                            on_close(&(), window, cx);
                        });
                }
                row.into_any_element()
            }
        });

    let close = on_close.clone();
    gpui::deferred(
        div()
            .absolute()
            .left(px(left))
            .top(px(top))
            .w(px(MENU_W))
            .p(px(4.0))
            .flex()
            .flex_col()
            .rounded(px(6.0))
            .border(px(1.0))
            .border_color(Colors::border_subtle())
            .bg(Colors::surface_card())
            .occlude()
            .on_mouse_down_out(move |_event, window, cx| close(&(), window, cx))
            .children(rows),
    )
    .with_priority(3)
    .into_any_element()
}

/// `entries` as native popup rows.
fn popup_items(entries: Vec<ContextMenuEntry>) -> Vec<crate::native_popup_menu::PopupItem> {
    use crate::native_popup_menu::PopupItem;
    entries
        .into_iter()
        .map(|entry| match entry {
            ContextMenuEntry::Header(label) => PopupItem::Header(label),
            ContextMenuEntry::Separator => PopupItem::Separator,
            ContextMenuEntry::Item {
                label,
                command,
                shortcut,
                disabled,
            } => PopupItem::Command {
                label,
                command,
                shortcut,
                checked: false,
                enabled: !disabled,
            },
        })
        .collect()
}

/// Draws nothing; when first painted, brings the system menu up at the click.
/// The caller's open state stays until the menu closes, then `on_command`
/// runs for a choice and `on_close` always runs after it. Studio's
/// `menu_dropdown::native_menu_element`, which the plug-in does not mount.
fn native_context_menu(
    entries: Vec<ContextMenuEntry>,
    x: f32,
    y: f32,
    on_command: ContextCommandCb,
    on_close: ContextCloseCb,
) -> gpui::AnyElement {
    let items = popup_items(entries);
    gpui::canvas(
        |_, _, _| (),
        move |_, _, window, cx| {
            // Repaints while the menu is up land here too.
            if crate::native_popup_menu::is_open(window) {
                return;
            }
            let close = on_close.clone();
            let on_command = on_command.clone();
            let shown = crate::native_popup_menu::show(
                window,
                cx,
                gpui::point(px(x), px(y)),
                None,
                items.clone(),
                Box::new(move |chosen, window, cx| {
                    if let Some(command) = chosen {
                        on_command(&command, window, cx);
                    }
                    close(&(), window, cx);
                }),
            );
            if !shown {
                // Nothing native came up: drop the open state rather than
                // leave an invisible menu holding it.
                let close = on_close.clone();
                window.defer(cx, move |window, cx| close(&(), window, cx));
            }
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .w(px(1.0))
    .h(px(1.0))
    .into_any_element()
}

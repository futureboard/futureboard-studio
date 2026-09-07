use std::sync::Arc;

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, App, InteractiveElement, IntoElement, ParentElement, StatefulInteractiveElement,
    Styled, Window,
};

use crate::components::text_input::{
    text_field_with_callbacks, TextInputCallbacks, TextInputState,
};
use crate::menu::{MenuItem, MenuItemKind, MenuManifest};
use crate::theme::{menu as menu_style, text as text_style, Colors};

pub type CommandPaletteCommandCb = Arc<dyn Fn(&String, &mut Window, &mut App) + 'static>;
pub type CommandPaletteCloseCb = Arc<dyn Fn(&(), &mut Window, &mut App) + 'static>;

const PALETTE_W: f32 = 520.0;
const PALETTE_MAX_H: f32 = 420.0;
const ROW_MIN_H: f32 = 40.0;
const ROW_PAD_Y: f32 = 5.0;
const FOOTER_H: f32 = 26.0;
const SEARCH_H: f32 = 38.0;
const MAX_RESULTS: usize = 12;

#[derive(Debug, Clone, Default)]
pub struct CommandPaletteState {
    pub is_open: bool,
    pub query: String,
    pub selected_index: usize,
}

#[derive(Debug, Clone)]
pub struct CommandPaletteEntry {
    pub label: String,
    pub command: String,
    pub shortcut: Option<String>,
    pub path: String,
}

impl CommandPaletteState {
    pub fn open(&mut self) {
        self.is_open = true;
        self.query.clear();
        self.selected_index = 0;
    }

    pub fn close(&mut self) {
        self.is_open = false;
        self.query.clear();
        self.selected_index = 0;
    }
}

pub fn command_palette_entries(query: &str) -> Vec<CommandPaletteEntry> {
    let query = query.trim().to_lowercase();
    if query.starts_with("overlay:theme") {
        let mut themes = crate::theme::available_theme_summaries()
            .into_iter()
            .map(|(id, name)| CommandPaletteEntry {
                label: name,
                command: format!("theme:select:{id}"),
                shortcut: None,
                path: "Theme".to_string(),
            })
            .collect::<Vec<_>>();
        themes.truncate(MAX_RESULTS);
        return themes;
    }
    let mut entries = Vec::new();
    for menu in &MenuManifest::load().menus {
        collect_menu_items(&menu.items, &menu.label, &query, &mut entries);
    }
    entries.truncate(MAX_RESULTS);
    if query.is_empty() || "overlay:theme".contains(&query) {
        entries.insert(
            0,
            CommandPaletteEntry {
                label: "Select Theme".to_string(),
                command: "overlay:theme".to_string(),
                shortcut: None,
                path: "Theme".to_string(),
            },
        );
        entries.truncate(MAX_RESULTS);
    }
    entries
}

fn collect_menu_items(
    items: &[MenuItem],
    path: &str,
    query: &str,
    entries: &mut Vec<CommandPaletteEntry>,
) {
    for item in items {
        if !item.visible {
            continue;
        }
        match item.kind {
            MenuItemKind::Submenu => {
                if let Some(label) = item.label.as_deref() {
                    let next_path = format!("{path} / {label}");
                    collect_menu_items(&item.children, &next_path, query, entries);
                }
            }
            MenuItemKind::Normal | MenuItemKind::Checkbox | MenuItemKind::Radio => {
                if !item.enabled {
                    continue;
                }
                let Some(command) = item.command.as_ref() else {
                    continue;
                };
                let label = item.label.clone().unwrap_or_else(|| command.clone());
                let haystack = format!(
                    "{} {} {} {}",
                    label,
                    command,
                    path,
                    item.description.clone().unwrap_or_default()
                )
                .to_lowercase();
                if query.is_empty() || haystack.contains(query) {
                    entries.push(CommandPaletteEntry {
                        label,
                        command: command.clone(),
                        shortcut: item.shortcut.clone(),
                        path: path.to_string(),
                    });
                }
            }
            MenuItemKind::Separator => {}
        }
    }
}

fn panel_shadow() -> Vec<gpui::BoxShadow> {
    vec![gpui::BoxShadow {
        color: Colors::surface_overlay().into(),
        offset: gpui::point(px(0.0), px(12.0)),
        blur_radius: px(40.0),
        spread_radius: px(0.0),
        inset: false,
    }]
}

pub fn command_palette_overlay(
    state: &CommandPaletteState,
    search_input: &TextInputState,
    search_focused: bool,
    search_callbacks: TextInputCallbacks,
    viewport_width: f32,
    viewport_height: f32,
    on_command: CommandPaletteCommandCb,
    on_close: CommandPaletteCloseCb,
) -> impl IntoElement {
    let query = search_input.value.as_str();
    let entries = command_palette_entries(query);
    let left = ((viewport_width - PALETTE_W) * 0.5).max(12.0);
    let width = PALETTE_W.min((viewport_width - 24.0).max(320.0));
    let top = (viewport_height * 0.14).clamp(32.0, 108.0);
    let close_click = on_close.clone();

    div()
        .absolute()
        .inset_0()
        .id("command-palette-overlay")
        .child(
            div()
                .absolute()
                .inset_0()
                .bg(Colors::with_alpha(Colors::surface_base(), 0.22))
                // Modal scrim, so the wheel does not reach whatever is behind it.
                .occlude()
                .on_mouse_down(gpui::MouseButton::Left, move |_, w, cx| {
                    close_click(&(), w, cx)
                }),
        )
        .child(
            div()
                .absolute()
                .left(px(left))
                .top(px(top))
                .w(px(width))
                .max_h(px(PALETTE_MAX_H))
                .flex()
                .flex_col()
                .overflow_hidden()
                .rounded(px(crate::theme::radius::SURFACE))
                .border(px(1.0))
                .border_color(Colors::border_subtle())
                .bg(Colors::surface_card())
                .shadow(panel_shadow())
                .occlude()
                .child(search_row(search_input, search_focused, search_callbacks))
                .child(result_list(entries, state.selected_index, on_command))
                .child(footer_hint()),
        )
}

fn search_row(
    search_input: &TextInputState,
    search_focused: bool,
    callbacks: TextInputCallbacks,
) -> impl IntoElement {
    div()
        .h(px(SEARCH_H))
        .flex()
        .flex_col()
        .justify_center()
        .border_b(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_panel())
        .px(px(8.0))
        .py(px(5.0))
        .child(text_field_with_callbacks(
            search_input,
            search_focused,
            callbacks,
        ))
}

fn footer_hint() -> impl IntoElement {
    div()
        .h(px(FOOTER_H))
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .border_t(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_panel())
        .px(px(10.0))
        .text_size(px(text_style::META))
        .text_color(Colors::text_faint())
        .child(hint_chip("↑↓", "navigate"))
        .child(hint_chip("Enter", "run"))
        .child(hint_chip("Esc", "close"))
}

fn hint_chip(key: &'static str, action: &'static str) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(4.0))
        .child(div().text_color(Colors::text_muted()).child(key))
        .child(action)
}

fn result_list(
    entries: Vec<CommandPaletteEntry>,
    selected_index: usize,
    on_command: CommandPaletteCommandCb,
) -> impl IntoElement {
    let list_max_h = PALETTE_MAX_H - SEARCH_H - FOOTER_H;

    if entries.is_empty() {
        return div()
            .h(px(72.0))
            .max_h(px(list_max_h))
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(4.0))
            .child(
                div()
                    .text_size(px(text_style::UI))
                    .text_color(Colors::text_muted())
                    .child("No matching commands"),
            )
            .child(
                div()
                    .text_size(px(text_style::META))
                    .text_color(Colors::text_faint())
                    .child("Try a different search term"),
            )
            .into_any_element();
    }

    div()
        .flex_1()
        .min_h(px(0.0))
        .max_h(px(list_max_h))
        .id("command-palette-results")
        .overflow_y_scroll()
        .p(px(menu_style::PANEL_PAD))
        .flex()
        .flex_col()
        .gap(px(menu_style::ITEM_GAP))
        .children(entries.into_iter().enumerate().map(|(index, entry)| {
            command_row(index, entry, index == selected_index, on_command.clone())
                .into_any_element()
        }))
        .into_any_element()
}

fn format_path_breadcrumb(path: &str) -> String {
    path.replace(" / ", " › ")
}

fn shortcut_badge(shortcut: String) -> impl IntoElement {
    div()
        .flex_none()
        .rounded(px(crate::theme::radius::CONTROL))
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_raised())
        .px(px(6.0))
        .py(px(2.0))
        .text_size(px(text_style::META))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(Colors::text_muted())
        .child(shortcut)
}

fn command_row(
    index: usize,
    entry: CommandPaletteEntry,
    selected: bool,
    on_command: CommandPaletteCommandCb,
) -> impl IntoElement {
    let command = entry.command.clone();
    let path_label = format_path_breadcrumb(&entry.path);
    let show_path = !entry.path.is_empty();

    let mut row = div()
        .id(("command-palette-row", index))
        .min_h(px(ROW_MIN_H))
        .py(px(ROW_PAD_Y))
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(8.0))
        .pl(px(menu_style::ROW_PAD_X))
        .pr(px(10.0))
        .rounded(px(crate::theme::radius::CONTROL))
        .cursor(gpui::CursorStyle::PointingHand)
        .on_click(move |_, w, cx| on_command(&command, w, cx))
        .child(
            div()
                .min_w_0()
                .flex_1()
                .flex()
                .flex_col()
                .justify_center()
                .gap(px(1.0))
                .when(show_path, |col| {
                    col.child(
                        div()
                            .truncate()
                            .text_size(px(text_style::META))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(Colors::text_faint())
                            .child(path_label),
                    )
                })
                .child(
                    div()
                        .truncate()
                        .text_size(px(text_style::UI))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(if selected {
                            Colors::text_primary()
                        } else {
                            Colors::text_secondary()
                        })
                        .child(entry.label),
                ),
        );

    if let Some(shortcut) = entry.shortcut {
        row = row.child(shortcut_badge(shortcut));
    }

    if selected {
        row = row.bg(Colors::with_alpha(Colors::accent_primary(), 0.16));
    } else {
        row = row.hover(|s| s.bg(Colors::surface_hover()));
    }

    row
}

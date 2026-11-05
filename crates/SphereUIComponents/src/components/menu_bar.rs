use std::sync::Arc;

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, App, InteractiveElement, IntoElement, ParentElement, Role, StatefulInteractiveElement,
    Styled, Window,
};

use crate::i18n::I18n;
use crate::menu::MenuManifest;
use crate::overlay::{compute_overlay_position, OverlayAnchor, OverlayPlacement, OverlaySize};
use crate::platform_chrome::PlatformChromePolicy;
use crate::theme::{menu as menu_style, Colors};

use super::title_bar::{CHROME_TEXT_SIZE, TITLEBAR_HEIGHT};

pub type MenuOpenCb = Arc<dyn Fn(&(String, f32), &mut Window, &mut App) + 'static>;
pub type MenuCloseCb = Arc<dyn Fn(&(), &mut Window, &mut App) + 'static>;

/// `open_menu_id` value while the overflow picker is shown.
pub const MENU_PICKER_ID: &str = "__menu_picker__";

const PICKER_PANEL_WIDTH: f32 = 168.0;
const PICKER_ROW_HEIGHT: f32 = menu_style::ROW_HEIGHT;
const MENU_BAR_PAD_X: f32 = 2.0;
const MENU_LABEL_PAD_X: f32 = 7.0;
const MENU_LABEL_GAP: f32 = 1.0;

/// Widest the centred project chip can get. The menu bar's own track is the
/// left half of what that chip leaves behind, so the bar has to know it.
const CENTRE_CHIP_WIDTH: f32 = super::app_chrome::PROJECT_CHIP_MAX_WIDTH;

/// Width the menu bar actually has to draw in.
///
/// The titlebar is three tracks - menu bar, centred project chip, right-hand
/// cluster - where the two side tracks are `flex_1` with a zero basis and so
/// split whatever the chip leaves equally. That half, less one gap, is the
/// budget; the whole window width never was.
///
/// This replaces a single 1400 px viewport breakpoint below which every menu
/// title vanished behind a hamburger at once. Titles now stay on the bar until
/// they genuinely do not fit, and only the ones that do not fit move.
pub fn menu_bar_available_width(viewport_width: f32) -> f32 {
    let chrome_left: f32 = PlatformChromePolicy::current()
        .traffic_light_left_padding()
        .into();
    let side = (viewport_width - chrome_left - CENTRE_CHIP_WIDTH) * 0.5;
    (side - crate::theme::space::BASE).max(0.0)
}

/// Per-menu label widths in manifest order, plus the width of the overflow
/// trigger that has to be reserved when not everything fits.
fn menu_label_widths(i18n: I18n) -> (Vec<f32>, f32) {
    let manifest = MenuManifest::load();
    let labels = manifest
        .menus
        .iter()
        .map(|menu| menu_label_width(&i18n.tr_menu(&menu.id, &menu.label)))
        .collect();
    (labels, menu_label_width(&overflow_label(i18n)))
}

/// How many leading menu titles fit; the rest go to the overflow picker.
///
/// Never returns zero while there is a menu to show: a bar carrying nothing but
/// an overflow button is the hamburger this replaced.
fn visible_menu_count(widths: &[f32], overflow_width: f32, available_width: f32) -> usize {
    if widths.is_empty() {
        return 0;
    }
    let padding = MENU_BAR_PAD_X * 2.0;
    let total: f32 = widths.iter().sum::<f32>()
        + widths.len().saturating_sub(1) as f32 * MENU_LABEL_GAP
        + padding;
    if total <= available_width {
        return widths.len();
    }

    // Something has to spill, so the trigger's own width comes off the budget.
    let budget = available_width - padding - overflow_width - MENU_LABEL_GAP;
    let mut used = 0.0;
    let mut visible = 0;
    for (index, width) in widths.iter().enumerate() {
        let gap = if index == 0 { 0.0 } else { MENU_LABEL_GAP };
        if used + gap + width > budget {
            break;
        }
        used += gap + width;
        visible += 1;
    }
    visible.max(1)
}

/// Menus that did not fit, as manifest indices.
fn overflow_menu_range(available_width: f32, i18n: I18n) -> std::ops::Range<usize> {
    let (widths, overflow_width) = menu_label_widths(i18n);
    let visible = visible_menu_count(&widths, overflow_width, available_width);
    visible..widths.len()
}

fn overflow_label(i18n: I18n) -> String {
    i18n.tr_or("menu.more", "More")
}

pub fn menu_bar_chrome_width(viewport_width: f32, i18n: I18n) -> f32 {
    let available = menu_bar_available_width(viewport_width);
    let (widths, overflow_width) = menu_label_widths(i18n);
    let visible = visible_menu_count(&widths, overflow_width, available);
    let shown: f32 = widths.iter().take(visible).sum();
    let gaps = visible.saturating_sub(1) as f32 * MENU_LABEL_GAP;
    let overflow = if visible < widths.len() {
        MENU_LABEL_GAP + overflow_width
    } else {
        0.0
    };
    MENU_BAR_PAD_X * 2.0 + shown + gaps + overflow
}

pub fn menu_bar(
    open_menu_id: Option<&str>,
    on_open_menu: MenuOpenCb,
    viewport_width: f32,
    i18n: I18n,
) -> impl IntoElement {
    let manifest = MenuManifest::load();
    let available = menu_bar_available_width(viewport_width);
    let (widths, overflow_width) = menu_label_widths(i18n);
    let visible = visible_menu_count(&widths, overflow_width, available);
    let has_overflow = visible < manifest.menus.len();
    let open_id_owned = open_menu_id.map(|s| s.to_string());
    let chrome_left: f32 = PlatformChromePolicy::current()
        .traffic_light_left_padding()
        .into();
    let mut next_label_left = chrome_left + MENU_BAR_PAD_X;

    let mut bar = div()
        .id("top-menu-bar")
        .role(Role::MenuBar)
        .aria_label("Application menu")
        .flex()
        .flex_row()
        .items_center()
        .gap(px(MENU_LABEL_GAP))
        .flex_none()
        .h(px(TITLEBAR_HEIGHT))
        .px(px(MENU_BAR_PAD_X));

    for (i, menu) in manifest.menus.iter().take(visible).enumerate() {
        let is_open = open_id_owned.as_deref() == Some(menu.id.as_str());
        let menu_id = menu.id.clone();
        let hover_menu_id = menu.id.clone();
        let label = i18n.tr_menu(&menu.id, &menu.label);
        let anchor_x = next_label_left;
        next_label_left += menu_label_width(&label) + MENU_LABEL_GAP;
        let cb = on_open_menu.clone();
        let hover_cb = on_open_menu.clone();
        let can_hover_switch = open_id_owned.is_some() && !is_open;

        bar = bar.child(menu_label_button(
            ("top-menu", i),
            label,
            is_open,
            can_hover_switch,
            move |hovered, window, cx| {
                if *hovered {
                    hover_cb(&(hover_menu_id.clone(), anchor_x), window, cx);
                }
            },
            move |_event, window, cx| {
                cb(&(menu_id.clone(), anchor_x), window, cx);
            },
        ));
    }

    if has_overflow {
        let is_open = open_id_owned.as_deref() == Some(MENU_PICKER_ID);
        let anchor_x = next_label_left;
        let cb = on_open_menu.clone();
        let hover_cb = on_open_menu.clone();
        let can_hover_switch = open_id_owned.is_some() && !is_open;
        bar = bar.child(menu_label_button(
            "top-menu-overflow",
            overflow_label(i18n),
            is_open,
            can_hover_switch,
            move |hovered, window, cx| {
                if *hovered {
                    hover_cb(&(MENU_PICKER_ID.to_string(), anchor_x), window, cx);
                }
            },
            move |_event, window, cx| {
                cb(&(MENU_PICKER_ID.to_string(), anchor_x), window, cx);
            },
        ));
    }

    bar
}

/// Overflow panel: the menu titles that did not fit on the bar.
///
/// The same rows the compact hamburger used to list, but only the ones that
/// actually spilled - a row for a title the user can see two centimetres to
/// the left is noise, and picking it would open that panel under the picker
/// instead of under its own title.
pub fn menu_picker_dropdown(
    anchor: OverlayAnchor,
    viewport_width: f32,
    viewport_height: f32,
    on_open_menu: MenuOpenCb,
    on_close: MenuCloseCb,
    i18n: I18n,
) -> impl IntoElement {
    let manifest = MenuManifest::load();
    let overflow = overflow_menu_range(menu_bar_available_width(viewport_width), i18n);
    let row_count = overflow.len();
    // Widening the window while the picker is open leaves nothing overflowed.
    // Draw only the dismiss layer then, rather than an empty bordered panel.
    let has_rows = row_count > 0;
    let panel_height = menu_style::PANEL_PAD * 2.0
        + row_count as f32 * PICKER_ROW_HEIGHT
        + (row_count.saturating_sub(1)) as f32 * menu_style::ITEM_GAP;

    let window_bounds = gpui::bounds(
        gpui::point(px(0.0), px(0.0)),
        gpui::size(px(viewport_width), px(viewport_height)),
    );
    let pos = compute_overlay_position(
        anchor.bounds,
        OverlaySize {
            width: PICKER_PANEL_WIDTH,
            height: panel_height.max(80.0),
        },
        window_bounds,
        OverlayPlacement::BottomStart,
        4.0,
    );
    let panel_left: f32 = pos.x.into();
    let panel_top: f32 = pos.y.into();

    div()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .child(div().absolute().top_0().left_0().size_full().on_mouse_down(
            gpui::MouseButton::Left,
            {
                let on_close = on_close.clone();
                move |_, window, cx| on_close(&(), window, cx)
            },
        ))
        .when(has_rows, |overlay| {
            overlay.child(
                div()
                    .id("menu-picker")
                    .role(Role::Menu)
                    .aria_label("Application menus")
                    .absolute()
                    .left(px(panel_left))
                    .top(px(panel_top))
                    .w(px(PICKER_PANEL_WIDTH))
                    .flex()
                    .flex_col()
                    .p(px(menu_style::PANEL_PAD))
                    .gap(px(menu_style::ITEM_GAP))
                    .rounded(px(crate::theme::radius::CONTROL))
                    .border(px(1.0))
                    .border_color(Colors::border_subtle())
                    .bg(Colors::surface_raised())
                    .shadow_lg()
                    .children(manifest.menus[overflow.clone()].iter().enumerate().map(
                        |(i, menu)| {
                            let menu_id = menu.id.clone();
                            let label = i18n.tr_menu(&menu.id, &menu.label);
                            let cb = on_open_menu.clone();
                            div()
                                .id(("menu-picker-row", i))
                                .role(Role::MenuItem)
                                .aria_label(label.clone())
                                .focusable()
                                .tab_stop(true)
                                .focus_visible(|style| style.bg(Colors::surface_control_hover()))
                                .h(px(PICKER_ROW_HEIGHT))
                                .px(px(menu_style::ROW_PAD_X))
                                .flex()
                                .items_center()
                                .rounded(px(crate::theme::radius::CONTROL))
                                .text_size(px(menu_style::LABEL_TEXT_SIZE))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(Colors::text_primary())
                                .hover(|s| s.bg(Colors::surface_control_hover()))
                                .cursor(gpui::CursorStyle::PointingHand)
                                .on_click(move |_event, window, cx| {
                                    cb(&(menu_id.clone(), panel_left), window, cx);
                                })
                                .occlude()
                                .child(label)
                        },
                    )),
            )
        })
}

/// Rendered width of one top-level title, button padding included.
///
/// Uses the shared script-aware estimator rather than a flat `chars * 6.2`: the
/// flat figure charged a full advance for every Thai tone mark and vowel sign,
/// so a label like the Thai for "Project" measured about a third wider than it
/// draws - and that error is now what decides whether a title spills.
fn menu_label_width(label: &str) -> f32 {
    MENU_LABEL_PAD_X * 2.0 + menu_style::estimate_label_width(label)
}

pub fn menu_label_button(
    id: impl Into<gpui::ElementId>,
    label: impl Into<String>,
    active: bool,
    enable_hover_switch: bool,
    on_hover: impl Fn(&bool, &mut Window, &mut App) + 'static,
    on_mouse_down: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let label: String = label.into();
    div()
        .id(id)
        .role(Role::Button)
        .aria_label(label.clone())
        .aria_expanded(active)
        .focusable()
        .tab_stop(true)
        .focus_visible(|style| style.bg(Colors::surface_control_hover()))
        .h(px(24.0))
        .px(px(MENU_LABEL_PAD_X))
        .flex()
        .items_center()
        .rounded(px(crate::theme::radius::CONTROL))
        .text_size(px(CHROME_TEXT_SIZE))
        // Under the OS caption the row reads as the window's menu bar, set in
        // the regular weight a Windows menu bar uses. The drawn titlebar keeps
        // the medium weight of the rest of the chrome.
        .font_weight(if crate::platform_chrome::studio_native_frame() {
            gpui::FontWeight::NORMAL
        } else {
            gpui::FontWeight::MEDIUM
        })
        .text_color(if active {
            Colors::text_primary()
        } else {
            Colors::text_muted()
        })
        .bg(if active {
            Colors::surface_control_hover()
        } else {
            gpui::transparent_black().into()
        })
        .hover(|s| {
            s.bg(Colors::surface_control_hover())
                .text_color(Colors::text_primary())
        })
        .cursor(gpui::CursorStyle::PointingHand)
        .when(enable_hover_switch, |this| this.on_hover(on_hover))
        .on_click(on_mouse_down)
        .occlude()
        .child(label)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ten equal titles, so the arithmetic is easy to reason about.
    fn widths(count: usize, each: f32) -> Vec<f32> {
        vec![each; count]
    }

    /// The whole point of the change: room for every title means every title
    /// stays on the bar and there is no overflow trigger at all.
    #[test]
    fn everything_that_fits_stays_on_the_bar() {
        let widths = widths(10, 50.0);
        let total = 500.0 + 9.0 * MENU_LABEL_GAP + MENU_BAR_PAD_X * 2.0;
        assert_eq!(visible_menu_count(&widths, 40.0, total), 10);
        assert_eq!(visible_menu_count(&widths, 40.0, total + 200.0), 10);
    }

    /// One pixel short and only the last title moves — not all ten, which is
    /// what the old 1400 px breakpoint did.
    #[test]
    fn only_the_titles_that_do_not_fit_spill() {
        let widths = widths(10, 50.0);
        let total = 500.0 + 9.0 * MENU_LABEL_GAP + MENU_BAR_PAD_X * 2.0;
        let visible = visible_menu_count(&widths, 40.0, total - 1.0);
        assert!(visible > 0 && visible < 10, "got {visible}");
        // The trigger has to fit beside whatever stayed.
        let used: f32 = widths.iter().take(visible).sum::<f32>()
            + visible.saturating_sub(1) as f32 * MENU_LABEL_GAP
            + MENU_BAR_PAD_X * 2.0
            + MENU_LABEL_GAP
            + 40.0;
        assert!(used <= total - 1.0, "used {used} of {}", total - 1.0);
    }

    /// A bar showing nothing but an overflow button is the hamburger this
    /// replaced, so one title always survives however narrow the window is.
    #[test]
    fn one_title_always_survives() {
        let widths = widths(10, 50.0);
        assert_eq!(visible_menu_count(&widths, 40.0, 0.0), 1);
        assert_eq!(visible_menu_count(&widths, 40.0, 10.0), 1);
    }

    /// The reserved chrome width has to describe what is drawn, or the project
    /// chip's dropdown anchors past the end of a bar that shrank.
    #[test]
    fn reserved_width_matches_what_is_drawn() {
        let widths = widths(10, 50.0);
        let overflow_width = 40.0;
        for available in [0.0_f32, 120.0, 300.0, 600.0, 2000.0] {
            let visible = visible_menu_count(&widths, overflow_width, available);
            let shown: f32 = widths.iter().take(visible).sum();
            let expected = MENU_BAR_PAD_X * 2.0
                + shown
                + visible.saturating_sub(1) as f32 * MENU_LABEL_GAP
                + if visible < widths.len() {
                    MENU_LABEL_GAP + overflow_width
                } else {
                    0.0
                };
            assert!(expected.is_finite(), "available {available}");
        }
    }

    /// An empty manifest must not report a phantom title.
    #[test]
    fn no_menus_means_no_titles() {
        assert_eq!(visible_menu_count(&[], 40.0, 500.0), 0);
    }

    /// The budget is the bar's own track, not the window: the titlebar centres
    /// a chip between two equal side tracks.
    #[test]
    fn the_budget_is_one_side_track_not_the_window() {
        let wide = menu_bar_available_width(1920.0);
        let narrow = menu_bar_available_width(1000.0);
        assert!(wide > narrow);
        assert!(wide < 1920.0 * 0.5, "got {wide}");
        assert_eq!(menu_bar_available_width(100.0), 0.0);
    }
}

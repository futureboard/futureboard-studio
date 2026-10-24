use std::sync::Arc;

use gpui::prelude::FluentBuilder;
use gpui::{
    deferred, div, point, px, relative, size, svg, AnyElement, App, AvailableSpace, Bounds, Div,
    Element, ElementId, GlobalElementId, InteractiveElement, IntoElement, LayoutId, ParentElement,
    Pixels, Position, Stateful, StatefulInteractiveElement, Style, Styled, Window,
};

use crate::assets;
use crate::theme::Colors;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectOption {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    pub disabled: bool,
}

impl SelectOption {
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            description: None,
            disabled: false,
        }
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectMenuPlacement {
    Below,
    Above,
}

/// Paint priority for the deferred select menu. Kept above ordinary deferred
/// content (priority 0) so the dropdown always sits over sibling form rows,
/// the dialog footer, and any scroll container that would otherwise clip it.
const SELECT_MENU_PRIORITY: usize = 100;

/// Upper bound on the open menu's height.
///
/// The menu sizes itself to its options — a five-entry list is five rows tall,
/// not a scroll box — so ordinary dropdowns never scroll. This is only a guard
/// for lists long enough to run off the window (device and channel pickers can
/// be dozens of entries): the menu is painted `deferred`, so without a cap the
/// overflow would not be clipped, it would simply be unreachable. Roughly 13
/// plain rows, which clears every fixed list in the app.
const SELECT_MENU_MAX_HEIGHT: f32 = 420.0;

/// Gap between the trigger and the menu edge.
const SELECT_MENU_GAP: f32 = 3.0;

/// Minimum distance kept between the menu and the window edge.
const SELECT_MENU_WINDOW_MARGIN: f32 = 8.0;

pub fn select(
    id: &'static str,
    selected_id: Option<&str>,
    placeholder: impl Into<String>,
    options: Vec<SelectOption>,
    open: bool,
    disabled: bool,
    on_toggle: Arc<dyn Fn(&(), &mut Window, &mut App) + 'static>,
    on_change: Arc<dyn Fn(&String, &mut Window, &mut App) + 'static>,
) -> impl IntoElement {
    select_with_placement(
        id,
        selected_id,
        placeholder,
        options,
        open,
        disabled,
        SelectMenuPlacement::Below,
        on_toggle,
        on_change,
    )
}

pub fn select_with_placement(
    id: &'static str,
    selected_id: Option<&str>,
    placeholder: impl Into<String>,
    options: Vec<SelectOption>,
    open: bool,
    disabled: bool,
    placement: SelectMenuPlacement,
    on_toggle: Arc<dyn Fn(&(), &mut Window, &mut App) + 'static>,
    on_change: Arc<dyn Fn(&String, &mut Window, &mut App) + 'static>,
) -> impl IntoElement {
    select_with_placement_and_header(
        id,
        selected_id,
        placeholder,
        options,
        open,
        disabled,
        placement,
        None,
        on_toggle,
        on_change,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn select_with_placement_and_header(
    id: &'static str,
    selected_id: Option<&str>,
    placeholder: impl Into<String>,
    options: Vec<SelectOption>,
    open: bool,
    disabled: bool,
    placement: SelectMenuPlacement,
    menu_header: Option<AnyElement>,
    on_toggle: Arc<dyn Fn(&(), &mut Window, &mut App) + 'static>,
    on_change: Arc<dyn Fn(&String, &mut Window, &mut App) + 'static>,
) -> impl IntoElement {
    let placeholder = placeholder.into();
    let selected_label = selected_id
        .and_then(|selected| options.iter().find(|option| option.id == selected))
        .map(|option| option.label.clone());
    let label = selected_label.unwrap_or(placeholder);
    let toggle = on_toggle.clone();

    div()
        .relative()
        .w_full()
        .child(
            div()
                .id(id)
                .h(px(28.0))
                .w_full()
                .min_w(px(0.0))
                .rounded(px(crate::theme::radius::CONTROL))
                .border(px(1.0))
                .border_color(if open {
                    Colors::border_focus()
                } else {
                    Colors::border_subtle()
                })
                .bg(if open {
                    Colors::surface_card()
                } else {
                    Colors::surface_input()
                })
                .px(px(8.0))
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(px(8.0))
                .opacity(if disabled { 0.48 } else { 1.0 })
                .cursor(if disabled {
                    gpui::CursorStyle::Arrow
                } else {
                    gpui::CursorStyle::PointingHand
                })
                .hover(|s| {
                    if disabled {
                        s
                    } else {
                        s.bg(Colors::surface_control_hover())
                            .border_color(Colors::border_strong())
                    }
                })
                .on_mouse_down(gpui::MouseButton::Left, move |_event, window, cx| {
                    cx.stop_propagation();
                    if !disabled {
                        toggle(&(), window, cx);
                    }
                    window.prevent_default();
                })
                .child(
                    div()
                        .min_w(px(0.0))
                        .flex_1()
                        .truncate()
                        .text_size(px(11.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(Colors::text_primary())
                        .child(label),
                )
                .child(
                    svg()
                        .path(assets::ICON_CHEVRON_DOWN_PATH)
                        .w(px(10.0))
                        .h(px(10.0))
                        .flex_shrink_0()
                        .text_color(Colors::text_faint()),
                ),
        )
        .when(open && !disabled, move |root| {
            if crate::ui_debug_enabled() {
                eprintln!(
                    "[ui-popup] render kind=select id={id} placement={placement:?} z=overlay"
                );
            }
            let menu = div()
                .rounded(px(crate::theme::radius::CONTROL))
                .border(px(1.0))
                .border_color(Colors::border_default())
                .bg(Colors::surface_panel_raised())
                .shadow(vec![gpui::BoxShadow {
                    color: Colors::surface_overlay().into(),
                    offset: gpui::point(px(0.0), px(2.0)),
                    blur_radius: px(24.0),
                    spread_radius: px(0.0),
                    inset: false,
                }])
                .p(px(4.0))
                .id((gpui::ElementId::from(id), "menu"))
                .overflow_y_scroll()
                .occlude()
                .on_mouse_down(gpui::MouseButton::Left, |_, _window, cx| {
                    cx.stop_propagation();
                })
                .children(menu_header)
                .children(options.into_iter().enumerate().map(move |(index, option)| {
                    let active = selected_id == Some(option.id.as_str());
                    let disabled = option.disabled;
                    let value = option.id.clone();
                    let on_change = on_change.clone();
                    div()
                        .id(("select-option", index))
                        .min_h(px(if option.description.is_some() {
                            34.0
                        } else {
                            24.0
                        }))
                        .w_full()
                        .rounded(px(crate::theme::radius::CONTROL))
                        .px(px(8.0))
                        .py(px(4.0))
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .gap(px(8.0))
                        .bg(if active {
                            Colors::accent_muted()
                        } else {
                            gpui::transparent_black().into()
                        })
                        .opacity(if disabled { 0.45 } else { 1.0 })
                        .cursor(if disabled {
                            gpui::CursorStyle::Arrow
                        } else {
                            gpui::CursorStyle::PointingHand
                        })
                        .hover(|s| {
                            if disabled {
                                s
                            } else {
                                s.bg(Colors::surface_control_hover())
                            }
                        })
                        .on_click(move |_, window, cx| {
                            if !disabled {
                                cx.stop_propagation();
                                if crate::ui_debug_enabled() {
                                    eprintln!("[ui-select] selected id={id} value={value}");
                                    eprintln!("[ui-select] close id={id} reason=option_selected");
                                }
                                on_change(&value, window, cx);
                            }
                        })
                        .child(
                            div()
                                .min_w(px(0.0))
                                .flex_1()
                                .flex()
                                .flex_col()
                                .gap(px(1.0))
                                .child(
                                    div()
                                        .truncate()
                                        .text_size(px(10.5))
                                        .font_weight(if active {
                                            gpui::FontWeight::SEMIBOLD
                                        } else {
                                            gpui::FontWeight::NORMAL
                                        })
                                        .text_color(if active {
                                            Colors::text_primary()
                                        } else {
                                            Colors::text_secondary()
                                        })
                                        .child(option.label),
                                )
                                .children(option.description.map(|description| {
                                    div()
                                        .truncate()
                                        .text_size(px(9.0))
                                        .text_color(Colors::text_faint())
                                        .child(description)
                                })),
                        )
                        .children(active.then(|| {
                            svg()
                                .path(assets::ICON_CHECK_PATH)
                                .w(px(11.0))
                                .h(px(11.0))
                                .flex_shrink_0()
                                .text_color(Colors::accent_primary())
                        }))
                }));
            // Defer the menu so it paints after every sibling row and the
            // dialog footer, and escapes any parent scroll container's clip.
            // The anchor covers the trigger, so the menu keeps its width and
            // is fitted against the window once the trigger is placed.
            root.child(
                deferred(SelectMenuAnchor {
                    menu: Some(menu),
                    preferred: placement,
                    menu_element: None,
                })
                .with_priority(SELECT_MENU_PRIORITY),
            )
        })
}

/// Places the open menu next to the trigger and keeps it inside the window:
/// it opens on the preferred side when the menu fits there, otherwise on the
/// side with more room, and its height is capped to that room so the last
/// rows stay reachable by scrolling instead of being painted off-window.
struct SelectMenuAnchor {
    menu: Option<Stateful<Div>>,
    preferred: SelectMenuPlacement,
    menu_element: Option<AnyElement>,
}

impl IntoElement for SelectMenuAnchor {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for SelectMenuAnchor {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.position = Position::Absolute;
        style.inset.top = px(0.0).into();
        style.inset.left = px(0.0).into();
        style.size.width = relative(1.0).into();
        style.size.height = relative(1.0).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        trigger: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let Some(menu) = self.menu.take() else {
            return;
        };
        let viewport = window.viewport_size();
        let space_below = f32::from(viewport.height)
            - f32::from(trigger.bottom())
            - SELECT_MENU_GAP
            - SELECT_MENU_WINDOW_MARGIN;
        let space_above = f32::from(trigger.top()) - SELECT_MENU_GAP - SELECT_MENU_WINDOW_MARGIN;
        let max_height = space_below
            .max(space_above)
            .min(SELECT_MENU_MAX_HEIGHT)
            .max(0.0);
        let width = trigger.size.width;
        let mut menu = menu.w(width).max_h(px(max_height)).into_any_element();
        let height = f32::from(
            menu.layout_as_root(
                size(AvailableSpace::Definite(width), AvailableSpace::MinContent),
                window,
                cx,
            )
            .height,
        );
        let placement = fit_menu_placement(self.preferred, height, space_below, space_above);
        let top = match placement {
            SelectMenuPlacement::Below => f32::from(trigger.bottom()) + SELECT_MENU_GAP,
            SelectMenuPlacement::Above => f32::from(trigger.top()) - SELECT_MENU_GAP - height,
        };
        if crate::ui_debug_enabled() {
            eprintln!(
                "[ui-popup] fit kind=select preferred={:?} placement={placement:?} height={height:.0} below={space_below:.0} above={space_above:.0}",
                self.preferred
            );
        }
        menu.prepaint_at(point(trigger.left(), px(top.round())), window, cx);
        self.menu_element = Some(menu);
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        if let Some(menu) = self.menu_element.as_mut() {
            menu.paint(window, cx);
        }
    }
}

/// Side the menu opens on: the preferred one when `height` fits there,
/// otherwise whichever side has more room.
fn fit_menu_placement(
    preferred: SelectMenuPlacement,
    height: f32,
    space_below: f32,
    space_above: f32,
) -> SelectMenuPlacement {
    let preferred_space = match preferred {
        SelectMenuPlacement::Below => space_below,
        SelectMenuPlacement::Above => space_above,
    };
    if height <= preferred_space {
        preferred
    } else if space_below >= space_above {
        SelectMenuPlacement::Below
    } else {
        SelectMenuPlacement::Above
    }
}

/// Full-window click-catcher that dismisses an open [`select`] when the user
/// clicks anywhere outside the menu. Render this once at the dialog root (the
/// element that fills the window) whenever any select is open, e.g.
/// `.children(open_select.is_some().then(|| select_dismiss_backdrop(cb)))`.
///
/// The deferred select menu paints above this layer and `.occlude()`s its own
/// clicks, so only genuine outside clicks reach the backdrop. This is the
/// reusable counterpart to the in-component menu fix — no per-dialog overlay
/// plumbing required beyond a single state flag and this one line.
pub fn select_dismiss_backdrop(
    on_dismiss: Arc<dyn Fn(&(), &mut Window, &mut App) + 'static>,
) -> impl IntoElement {
    div()
        .absolute()
        .inset_0()
        .id("select-dismiss-backdrop")
        .on_mouse_down(gpui::MouseButton::Left, move |_, window, cx| {
            on_dismiss(&(), window, cx);
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Height of one plain option row: `min_h(24)` content plus `py(4)`.
    const PLAIN_ROW_HEIGHT: f32 = 24.0 + 4.0 + 4.0;
    /// The menu's own `p(4)` top and bottom.
    const MENU_PADDING: f32 = 8.0;

    fn menu_height_for(rows: usize) -> f32 {
        MENU_PADDING + PLAIN_ROW_HEIGHT * rows as f32
    }

    /// Regression guard: the cap used to be 144px, which is 4 rows — so a
    /// five-entry list (the keymap profile picker) scrolled and clipped its last
    /// item instead of simply being five rows tall.
    #[test]
    fn ordinary_dropdowns_fit_without_scrolling() {
        for rows in 1..=12 {
            assert!(
                menu_height_for(rows) <= SELECT_MENU_MAX_HEIGHT,
                "{rows} options should size to content, not scroll (needs {}px)",
                menu_height_for(rows)
            );
        }
    }

    /// The cap still has to engage somewhere: a `deferred` menu is not clipped
    /// by the window, so an unbounded one would paint items off-screen where
    /// they cannot be reached at all.
    #[test]
    fn very_long_lists_stay_bounded() {
        assert!(menu_height_for(40) > SELECT_MENU_MAX_HEIGHT);
    }

    #[test]
    fn menu_keeps_preferred_side_when_it_fits() {
        let below = SelectMenuPlacement::Below;
        assert_eq!(fit_menu_placement(below, 120.0, 200.0, 400.0), below);
        let above = SelectMenuPlacement::Above;
        assert_eq!(fit_menu_placement(above, 120.0, 400.0, 200.0), above);
    }

    /// A trigger near the bottom of a short dialog window: the list does not
    /// fit below, so it opens upward where there is more room.
    #[test]
    fn menu_flips_to_the_roomier_side_when_it_overflows() {
        assert_eq!(
            fit_menu_placement(SelectMenuPlacement::Below, 380.0, 150.0, 400.0),
            SelectMenuPlacement::Above
        );
        assert_eq!(
            fit_menu_placement(SelectMenuPlacement::Above, 380.0, 400.0, 150.0),
            SelectMenuPlacement::Below
        );
    }
}

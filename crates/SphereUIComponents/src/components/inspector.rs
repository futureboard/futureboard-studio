use gpui::prelude::FluentBuilder;
use gpui::{
    anchored, deferred, div, px, svg, App, AppContext, DragMoveEvent, InteractiveElement,
    IntoElement, ParentElement, RenderOnce, Role, StatefulInteractiveElement, Styled, Window,
};

use crate::components::controls::fb_checkbox;
use crate::components::spin_drag::SpinDrag;
use crate::theme::Colors;

pub type InspectorNumericChangeCb = std::sync::Arc<dyn Fn(f64, &mut Window, &mut App) + 'static>;
pub type InspectorNumericGestureCb = std::sync::Arc<dyn Fn(&mut Window, &mut App) + 'static>;

#[derive(Clone, Copy)]
pub struct InspectorSelectOption<T: Copy + PartialEq + 'static> {
    pub label: &'static str,
    pub value: T,
}

pub fn inspector_section(
    title: impl Into<String>,
    subtitle: Option<impl Into<String>>,
    children: impl IntoElement,
) -> impl IntoElement {
    let title = title.into();
    let subtitle = subtitle.map(Into::into);
    div()
        .flex()
        .flex_col()
        .gap(px(5.0))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(1.0))
                .child(
                    div()
                        .h(px(18.0))
                        .flex()
                        .items_center()
                        .text_size(px(9.5))
                        .font_weight(gpui::FontWeight::BOLD)
                        .text_color(Colors::text_faint())
                        .child(title),
                )
                .children(subtitle.map(|text| {
                    div()
                        .min_w(px(0.0))
                        .text_size(px(10.0))
                        .text_color(Colors::text_faint())
                        .child(text)
                })),
        )
        .child(div().flex().flex_col().gap(px(3.0)).child(children))
}

pub fn inspector_row(
    label: impl Into<String>,
    disabled: bool,
    control: impl IntoElement,
) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .min_h(px(24.0))
        .opacity(if disabled { 0.48 } else { 1.0 })
        .child(
            div()
                .w(px(106.0))
                .flex_shrink_0()
                .truncate()
                .text_size(px(10.5))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(Colors::text_muted())
                .child(label.into()),
        )
        .child(div().flex_1().min_w_0().child(control))
}

pub fn inspector_value(text: impl Into<String>) -> impl IntoElement {
    div()
        .min_w(px(0.0))
        .h(px(24.0))
        .flex()
        .items_center()
        .justify_end()
        .truncate()
        .text_size(px(11.0))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(Colors::text_secondary())
        .child(text.into())
}

/// A real dropdown for Inspector enum fields.
///
/// Clicking the trigger opens a menu of every option; choosing one commits it
/// and closes the menu. The open flag, highlighted row and the trigger's
/// measured bounds are element state (`window.use_keyed_state`), so a caller
/// hands over nothing but the value and the commit callback — no overlay slot
/// in the layout, no per-dropdown plumbing.
///
/// The menu is `deferred` + `anchored` to the trigger's measured window
/// bounds, so it paints above the rest of the dock, escapes the scroll body's
/// clip and snaps inside the window. A full-window backdrop under it closes
/// the menu on any outside press (and swallows that press, the way a native
/// popup does). Escape closes; Up/Down move the highlight; Enter commits.
pub fn inspector_select<T: Copy + PartialEq + 'static>(
    id: impl Into<gpui::ElementId>,
    selected: T,
    options: &'static [InspectorSelectOption<T>],
    disabled: bool,
    on_change: impl Fn(T, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    InspectorSelect {
        id: id.into(),
        selected,
        options,
        disabled,
        on_change: std::rc::Rc::new(on_change),
    }
}

/// Paint priority of the open menu and its backdrop. Above ordinary deferred
/// content so the menu sits over sibling rows and any other popover in the dock.
const SELECT_BACKDROP_PRIORITY: usize = 110;
const SELECT_MENU_PRIORITY: usize = 111;
/// Visual height of one menu row.
const SELECT_ROW_HEIGHT: f32 = 24.0;
/// Gap between the trigger and the menu.
const SELECT_MENU_GAP: f32 = 4.0;

struct InspectorSelectState {
    open: bool,
    highlighted: usize,
    /// Trigger bounds in window coordinates, captured every prepaint so the
    /// menu anchors to where the trigger really is, not to a guessed column.
    trigger: Option<gpui::Bounds<gpui::Pixels>>,
    focus: gpui::FocusHandle,
}

#[derive(IntoElement)]
struct InspectorSelect<T: Copy + PartialEq + 'static> {
    id: gpui::ElementId,
    selected: T,
    options: &'static [InspectorSelectOption<T>],
    disabled: bool,
    on_change: std::rc::Rc<dyn Fn(T, &mut Window, &mut App)>,
}

impl<T: Copy + PartialEq + 'static> RenderOnce for InspectorSelect<T> {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Self {
            id,
            selected,
            options,
            disabled,
            on_change,
        } = self;
        let state = window.use_keyed_state(id.clone(), cx, |_, cx| InspectorSelectState {
            open: false,
            highlighted: 0,
            trigger: None,
            focus: cx.focus_handle(),
        });
        let selected_index = options
            .iter()
            .position(|option| option.value == selected)
            .unwrap_or(0);
        let label = options
            .get(selected_index)
            .map(|option| option.label)
            .unwrap_or("-");
        let (open, highlighted, trigger_bounds, focus) = {
            let s = state.read(cx);
            (
                s.open && !disabled,
                s.highlighted.min(options.len().saturating_sub(1)),
                s.trigger,
                s.focus.clone(),
            )
        };

        let rest = Colors::surface_input();
        let hover = Colors::composite(rest, Colors::state_hover());
        let pressed = Colors::composite(rest, Colors::state_recessed());
        let ring = Colors::state_focus_ring();

        let toggle_state = state.clone();
        let toggle_focus = focus.clone();
        let trigger = div()
            .id(id.clone())
            .role(Role::Button)
            .aria_label(label)
            .aria_disabled(disabled)
            .h(px(crate::theme::size::DEFAULT))
            .w_full()
            .min_w(px(0.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(crate::theme::space::SNUG))
            .px(px(crate::theme::space::BASE))
            .rounded(px(crate::theme::radius::CONTROL))
            .border(px(1.0))
            .border_color(if open {
                Colors::border_focus()
            } else {
                Colors::border_subtle()
            })
            .bg(if open { hover } else { rest })
            .when(disabled, |this| {
                this.opacity(crate::theme::state::DISABLED_CONTENT + 0.2)
            })
            .when(!disabled, |this| {
                this.focusable()
                    .tab_stop(true)
                    .focus_visible(move |style| {
                        style.shadow(crate::theme::elevation::focus_ring(ring))
                    })
                    .cursor(gpui::CursorStyle::PointingHand)
                    .hover(move |s| s.bg(hover))
                    .active(move |s| s.bg(pressed))
                    .on_mouse_down(gpui::MouseButton::Left, move |_, window, cx| {
                        cx.stop_propagation();
                        let focus = toggle_focus.clone();
                        toggle_state.update(cx, |s, cx| {
                            s.open = !s.open;
                            s.highlighted = selected_index;
                            cx.notify();
                        });
                        if toggle_state.read(cx).open {
                            window.focus(&focus, cx);
                        }
                    })
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(crate::theme::typography::UI_SM))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(if disabled {
                        Colors::text_disabled()
                    } else {
                        Colors::text_primary()
                    })
                    .child(label),
            )
            .child(
                svg()
                    .path(crate::assets::ICON_CHEVRON_DOWN_PATH)
                    .w(px(11.0))
                    .h(px(11.0))
                    .flex_shrink_0()
                    .text_color(Colors::text_muted()),
            );

        let measure_state = state.clone();
        let measure = gpui::canvas(
            move |bounds, _window, cx| {
                measure_state.update(cx, |s, _| s.trigger = Some(bounds));
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full();

        div()
            .relative()
            .w_full()
            .min_w(px(0.0))
            .child(trigger)
            .child(measure)
            .when(open, |root| {
                let Some(bounds) = trigger_bounds else {
                    return root;
                };
                let close_state = state.clone();
                let close = move |cx: &mut App| {
                    close_state.update(cx, |s, cx| {
                        if s.open {
                            s.open = false;
                            cx.notify();
                        }
                    });
                };
                let viewport = window.viewport_size();
                let backdrop_close = close.clone();
                let backdrop = anchored().position(gpui::point(px(0.0), px(0.0))).child(
                    div()
                        .id((id.clone(), "backdrop"))
                        .w(viewport.width)
                        .h(viewport.height)
                        .occlude()
                        .on_mouse_down(gpui::MouseButton::Left, move |_, _, cx| {
                            cx.stop_propagation();
                            backdrop_close(cx);
                        })
                        .on_mouse_down(gpui::MouseButton::Right, {
                            let close = close.clone();
                            move |_, _, cx| {
                                cx.stop_propagation();
                                close(cx);
                            }
                        }),
                );

                let key_state = state.clone();
                let key_change = on_change.clone();
                let row_radius = crate::theme::radius::inner(
                    crate::theme::radius::SURFACE,
                    crate::theme::space::TIGHT,
                );
                let menu_surface = Colors::surface_panel_raised();
                let row_hover = Colors::composite(menu_surface, Colors::state_hover());
                let row_selected = Colors::composite(menu_surface, Colors::state_selected());
                let menu = div()
                    .id((id.clone(), "menu"))
                    .track_focus(&focus)
                    .w(bounds.size.width)
                    .flex()
                    .flex_col()
                    .gap(px(1.0))
                    .p(px(crate::theme::space::TIGHT))
                    .rounded(px(crate::theme::radius::SURFACE))
                    .border(px(1.0))
                    .border_color(Colors::border_normal())
                    .bg(menu_surface)
                    .shadow(crate::theme::elevation::shadow(
                        crate::theme::elevation::OVERLAY,
                    ))
                    .occlude()
                    .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_key_down(move |event, window, cx| {
                        let len = options.len();
                        if len == 0 {
                            return;
                        }
                        match event.keystroke.key.as_str() {
                            "escape" => {
                                cx.stop_propagation();
                                key_state.update(cx, |s, cx| {
                                    s.open = false;
                                    cx.notify();
                                });
                            }
                            "down" => {
                                cx.stop_propagation();
                                key_state.update(cx, |s, cx| {
                                    s.highlighted = (s.highlighted + 1).min(len - 1);
                                    cx.notify();
                                });
                            }
                            "up" => {
                                cx.stop_propagation();
                                key_state.update(cx, |s, cx| {
                                    s.highlighted = s.highlighted.saturating_sub(1);
                                    cx.notify();
                                });
                            }
                            "enter" | "space" => {
                                cx.stop_propagation();
                                let index = key_state.read(cx).highlighted.min(len - 1);
                                key_state.update(cx, |s, cx| {
                                    s.open = false;
                                    cx.notify();
                                });
                                let value = options[index].value;
                                if value != selected {
                                    key_change(value, window, cx);
                                }
                            }
                            _ => {}
                        }
                    })
                    .children(options.iter().enumerate().map(|(index, option)| {
                        let active = option.value == selected;
                        let lit = index == highlighted;
                        let value = option.value;
                        let pick_state = state.clone();
                        let pick = on_change.clone();
                        let hover_state = state.clone();
                        div()
                            .id(("inspector-select-option", index))
                            .role(Role::MenuItem)
                            .aria_label(option.label)
                            .h(px(SELECT_ROW_HEIGHT))
                            .w_full()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(crate::theme::space::SNUG))
                            .px(px(crate::theme::space::BASE))
                            .rounded(px(row_radius))
                            .bg(if lit {
                                row_hover
                            } else if active {
                                row_selected
                            } else {
                                menu_surface
                            })
                            .cursor(gpui::CursorStyle::PointingHand)
                            .on_mouse_move(move |_, _, cx| {
                                hover_state.update(cx, |s, cx| {
                                    if s.highlighted != index {
                                        s.highlighted = index;
                                        cx.notify();
                                    }
                                });
                            })
                            .on_click(move |_, window, cx| {
                                cx.stop_propagation();
                                pick_state.update(cx, |s, cx| {
                                    s.open = false;
                                    cx.notify();
                                });
                                if value != selected {
                                    pick(value, window, cx);
                                }
                            })
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(crate::theme::typography::UI_SM))
                                    .font_weight(if active {
                                        gpui::FontWeight::SEMIBOLD
                                    } else {
                                        gpui::FontWeight::NORMAL
                                    })
                                    .text_color(if active || lit {
                                        Colors::text_primary()
                                    } else {
                                        Colors::text_secondary()
                                    })
                                    .child(option.label),
                            )
                            .children(active.then(|| {
                                svg()
                                    .path(crate::assets::ICON_CHECK_PATH)
                                    .w(px(11.0))
                                    .h(px(11.0))
                                    .flex_shrink_0()
                                    .text_color(Colors::accent_primary())
                            }))
                    }));
                let menu = anchored()
                    .position(gpui::point(
                        bounds.origin.x,
                        bounds.origin.y + bounds.size.height + px(SELECT_MENU_GAP),
                    ))
                    .snap_to_window_with_margin(px(crate::theme::space::BASE))
                    .child(menu);
                root.child(deferred(backdrop).with_priority(SELECT_BACKDROP_PRIORITY))
                    .child(deferred(menu).with_priority(SELECT_MENU_PRIORITY))
            })
    }
}

pub fn inspector_checkbox(
    id: impl Into<gpui::ElementId>,
    checked: bool,
    disabled: bool,
    label: impl Into<String>,
    on_change: impl Fn(bool, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    fb_checkbox(id, label, checked, !disabled, move |_, window, cx| {
        if !disabled {
            on_change(!checked, window, cx);
        }
    })
}

pub fn inspector_numeric_stepper(
    id: &'static str,
    value: f64,
    display: impl Into<String>,
    min: f64,
    max: f64,
    step: f64,
    disabled: bool,
    on_change: impl Fn(f64, &mut Window, &mut App) + Clone + 'static,
) -> impl IntoElement {
    inspector_numeric_stepper_with_drag_callbacks(
        id,
        value,
        display,
        min,
        max,
        step,
        disabled,
        None,
        std::sync::Arc::new(on_change),
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn inspector_numeric_stepper_with_drag_callbacks(
    id: &'static str,
    value: f64,
    display: impl Into<String>,
    min: f64,
    max: f64,
    step: f64,
    disabled: bool,
    on_drag_start: Option<InspectorNumericGestureCb>,
    on_drag_preview: InspectorNumericChangeCb,
    on_drag_commit: Option<InspectorNumericGestureCb>,
) -> impl IntoElement {
    numeric_stepper(
        id,
        value,
        display,
        min,
        max,
        step,
        None,
        disabled,
        on_drag_start,
        on_drag_preview,
        on_drag_commit,
    )
}

/// [`inspector_numeric_stepper_with_drag_callbacks`] for a value with a long
/// range (a fade in ms): holding Shift while scrubbing moves by `coarse_step`
/// per step instead of `step`, carrying on from the value reached when Shift
/// went down or came up.
#[allow(clippy::too_many_arguments)]
pub fn inspector_numeric_stepper_coarse(
    id: &'static str,
    value: f64,
    display: impl Into<String>,
    min: f64,
    max: f64,
    step: f64,
    coarse_step: f64,
    disabled: bool,
    on_drag_start: Option<InspectorNumericGestureCb>,
    on_drag_preview: InspectorNumericChangeCb,
    on_drag_commit: Option<InspectorNumericGestureCb>,
) -> impl IntoElement {
    numeric_stepper(
        id,
        value,
        display,
        min,
        max,
        step,
        Some(coarse_step),
        disabled,
        on_drag_start,
        on_drag_preview,
        on_drag_commit,
    )
}

#[allow(clippy::too_many_arguments)]
fn numeric_stepper(
    id: &'static str,
    value: f64,
    display: impl Into<String>,
    min: f64,
    max: f64,
    step: f64,
    coarse_step: Option<f64>,
    disabled: bool,
    on_drag_start: Option<InspectorNumericGestureCb>,
    on_drag_preview: InspectorNumericChangeCb,
    on_drag_commit: Option<InspectorNumericGestureCb>,
) -> impl IntoElement {
    const SCRUB_PIXELS_PER_STEP: f32 = 5.0;
    let drag_id = id.to_string();
    let drag_id_move = drag_id.clone();
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_end()
        .opacity(if disabled { 0.48 } else { 1.0 })
        .child({
            let display = display.into();
            let field = div()
                .id((id, 0usize))
                .w(px(108.0))
                .h(px(24.0))
                .flex()
                .items_center()
                .justify_end()
                .rounded(px(crate::theme::radius::CONTROL))
                .border(px(1.0))
                .border_color(Colors::border_subtle())
                .bg(Colors::surface_input())
                .px(px(7.0))
                .text_size(px(11.0))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(Colors::text_primary())
                .child(display);
            if disabled {
                field.into_any_element()
            } else {
                field
                    .cursor(gpui::CursorStyle::ResizeUpDown)
                    .hover(|style| {
                        style
                            .bg(Colors::surface_control_hover())
                            .border_color(Colors::border_strong())
                    })
                    .on_drag(
                        SpinDrag::new(drag_id, value),
                        move |drag, _offset, window, cx| {
                            drag.begin();
                            if let Some(start) = on_drag_start.as_ref() {
                                start(window, cx);
                            }
                            cx.new(|_| drag.clone())
                        },
                    )
                    .on_drag_move::<SpinDrag>(move |event: &DragMoveEvent<SpinDrag>, window, cx| {
                        let drag = event.drag(cx);
                        if !drag.matches(&drag_id_move) {
                            return;
                        }
                        let current_y: f32 = event.event.position.y.into();
                        let next = match coarse_step {
                            None => drag.value_at(
                                current_y,
                                step / f64::from(SCRUB_PIXELS_PER_STEP),
                                min,
                                max,
                                Some(step),
                            ),
                            Some(coarse_step) => {
                                let step = if event.event.modifiers.shift {
                                    coarse_step
                                } else {
                                    step
                                };
                                drag.value_at_rescaled(
                                    current_y,
                                    step / f64::from(SCRUB_PIXELS_PER_STEP),
                                    min,
                                    max,
                                    Some(step),
                                )
                            }
                        };
                        on_drag_preview(next, window, cx);
                    })
                    .when_some(on_drag_commit, |field, commit| {
                        let commit_out = commit.clone();
                        field
                            .on_mouse_up(gpui::MouseButton::Left, move |_, window, cx| {
                                commit(window, cx)
                            })
                            .on_mouse_up_out(gpui::MouseButton::Left, move |_, window, cx| {
                                commit_out(window, cx)
                            })
                    })
                    .into_any_element()
            }
        })
}

pub fn inspector_mini_button(
    id: impl Into<gpui::ElementId>,
    label: impl Into<String>,
    enabled: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let label: String = label.into();
    let mut button = div()
        .id(id)
        .role(Role::Button)
        .aria_label(label.clone())
        .aria_disabled(!enabled)
        .when(enabled, |button| {
            button
                .focusable()
                .tab_stop(true)
                .focus_visible(|style| style.border_color(Colors::border_focus()))
        })
        .h(px(24.0))
        .min_w(px(26.0))
        .px(px(7.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(crate::theme::radius::CONTROL))
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_input())
        .opacity(if enabled { 1.0 } else { 0.45 })
        .text_size(px(10.5))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(Colors::text_secondary())
        .child(label);
    if enabled {
        button = button
            .cursor(gpui::CursorStyle::PointingHand)
            .hover(|s| {
                s.bg(Colors::surface_control_hover())
                    .border_color(Colors::border_strong())
            })
            .on_click(on_click);
    }
    button
}

pub fn inspector_hint_text(text: impl Into<String>) -> impl IntoElement {
    div()
        .min_w(px(0.0))
        .pt(px(1.0))
        .text_size(px(10.0))
        .text_color(Colors::text_faint())
        .child(text.into())
}

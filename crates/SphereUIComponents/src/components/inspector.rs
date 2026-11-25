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

/// A section: the dock's one card, with an optional quiet line under its
/// title.
pub fn inspector_section(
    title: impl Into<String>,
    subtitle: Option<impl Into<String>>,
    children: impl IntoElement,
) -> impl IntoElement {
    let subtitle = subtitle.map(Into::into);
    crate::components::inspector_kit::ins_card(
        title,
        div()
            .flex()
            .flex_col()
            .gap(px(crate::theme::space::SNUG))
            .children(subtitle.map(|text: String| inspector_hint_text(text)))
            .child(children),
    )
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
                .text_size(px(crate::theme::typography::UI_XS))
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
            .h(px(crate::components::inspector_kit::PROP_CONTROL_H))
            .w_full()
            .min_w(px(0.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(crate::theme::space::TIGHT))
            .px(px(crate::theme::space::SNUG))
            .rounded(px(crate::theme::radius::CONTROL_SM))
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
                    .text_size(px(crate::theme::typography::UI_XS))
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
                    .w(px(10.0))
                    .h(px(10.0))
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
    NumericStepper {
        id,
        value,
        display: display.into(),
        min,
        max,
        step,
        coarse_step,
        disabled,
        on_begin: on_drag_start,
        on_set: on_drag_preview,
        on_commit: on_drag_commit,
    }
}

/// An Inspector number: drag it up or down to scrub, or double-click it to
/// type a value.
#[derive(IntoElement)]
struct NumericStepper {
    id: &'static str,
    value: f64,
    display: String,
    min: f64,
    max: f64,
    step: f64,
    coarse_step: Option<f64>,
    disabled: bool,
    on_begin: Option<InspectorNumericGestureCb>,
    on_set: InspectorNumericChangeCb,
    on_commit: Option<InspectorNumericGestureCb>,
}

/// A typed edit of an Inspector number, while it lasts.
///
/// Kept per field as keyed view state. The draft is the field's text; the
/// session holds the value it started from and the field's range, and the
/// callbacks are the ones the field had when the edit began, so the value
/// lands on the object that was being edited even if the selection moves.
pub struct NumericEntry {
    input: crate::components::text_input::TextInputState,
    session: Option<crate::components::numeric_edit::NumericEditSession>,
    on_begin: Option<InspectorNumericGestureCb>,
    on_set: Option<InspectorNumericChangeCb>,
    on_commit: Option<InspectorNumericGestureCb>,
}

impl gpui::Render for NumericEntry {
    fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

thread_local! {
    /// The number being typed into, if any — at most one field has the
    /// keyboard. The studio window's key handler hands it every key first
    /// (see [`route_numeric_entry_key`]), before its own shortcuts.
    static ACTIVE_NUMERIC_ENTRY: std::cell::RefCell<Option<gpui::WeakEntity<NumericEntry>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(feature = "ui-preview")]
thread_local! {
    static PREVIEW_EDIT_FIELD: std::cell::Cell<Option<&'static str>> =
        const { std::cell::Cell::new(None) };
}

/// Open the number field `id` for typing the next time it renders, as a
/// double-click would (`ui-preview` only: the preview cannot click).
#[cfg(feature = "ui-preview")]
pub(crate) fn preview_edit_number(id: &'static str) {
    PREVIEW_EDIT_FIELD.with(|field| field.set(Some(id)));
}

/// How many decimals a number is shown with: the digits after the point in
/// its readout ("16.00 bt" → 2, "-3.0 dB" → 1, "0 ms" → 0). The typed draft
/// starts at the precision the field shows, not at whatever the float holds.
fn display_decimals(display: &str) -> u8 {
    let Some(number) = display
        .split_whitespace()
        .find(|word| word.chars().any(|c| c.is_ascii_digit()))
    else {
        return 0;
    };
    match number.split_once('.') {
        Some((_, fraction)) => fraction.chars().take_while(|c| c.is_ascii_digit()).count() as u8,
        None => 0,
    }
}

/// What a key does to a number being typed.
#[derive(Debug, Clone, Copy, PartialEq)]
enum EntryKey {
    /// Enter or Tab: write the typed value.
    Commit,
    /// Escape: leave the value as it was.
    Cancel,
    /// Up / Down: step the draft by this many steps.
    Nudge(f64),
    /// Anything else edits the text.
    Edit,
}

fn entry_key(event: &gpui::KeyDownEvent) -> EntryKey {
    let modifiers = event.keystroke.modifiers;
    let plain = !(modifiers.control || modifiers.alt || modifiers.platform || modifiers.function);
    let steps = if modifiers.shift { 10.0 } else { 1.0 };
    match event.keystroke.key.as_str() {
        "enter" | "numpad_enter" | "tab" if plain => EntryKey::Commit,
        "escape" => EntryKey::Cancel,
        "up" | "arrow_up" if plain => EntryKey::Nudge(steps),
        "down" | "arrow_down" if plain => EntryKey::Nudge(-steps),
        _ => EntryKey::Edit,
    }
}

impl NumericEntry {
    fn editing(&self) -> bool {
        self.session.is_some()
    }
}

/// Start typing into a number: its value, at the precision it is shown with,
/// selected so the first key replaces it.
#[allow(clippy::too_many_arguments)]
fn begin_numeric_entry(
    entry: &gpui::Entity<NumericEntry>,
    value: f64,
    format: crate::components::numeric_edit::NumberFormat,
    on_begin: Option<InspectorNumericGestureCb>,
    on_set: InspectorNumericChangeCb,
    on_commit: Option<InspectorNumericGestureCb>,
    window: &mut Window,
    cx: &mut App,
) {
    use crate::components::numeric_edit::{CommitPolicy, NumericEditSession};
    let focus = entry.update(cx, |entry, cx| {
        let session = NumericEditSession::begin(value, format, CommitPolicy::OnBlur);
        entry.input.set_value(session.original_text());
        entry.input.select_all();
        entry.session = Some(session);
        entry.on_begin = on_begin;
        entry.on_set = Some(on_set);
        entry.on_commit = on_commit;
        cx.notify();
        entry.input.focus_handle.clone()
    });
    ACTIVE_NUMERIC_ENTRY.with(|active| *active.borrow_mut() = Some(entry.downgrade()));
    window.focus(&focus, cx);
}

/// End a typed edit: write the draft (`commit`) or drop it. A draft that does
/// not parse writes nothing. The write is one gesture — begin, set, commit —
/// so it is one undo step, the same as a drag.
fn end_numeric_entry(
    entry: &gpui::Entity<NumericEntry>,
    commit: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let (value, on_begin, on_set, on_commit, focused) = entry.update(cx, |entry, cx| {
        let session = entry.session.take();
        let value = session
            .filter(|_| commit)
            .and_then(|session| session.commit(&entry.input.value));
        cx.notify();
        (
            value,
            entry.on_begin.take(),
            entry.on_set.take(),
            entry.on_commit.take(),
            entry.input.is_focused(window),
        )
    });
    ACTIVE_NUMERIC_ENTRY.with(|active| {
        let mut active = active.borrow_mut();
        if active
            .as_ref()
            .and_then(|weak| weak.upgrade())
            .is_none_or(|current| current == *entry)
        {
            *active = None;
        }
    });
    if focused {
        window.blur();
    }
    if let (Some(value), Some(set)) = (value, on_set) {
        if let Some(begin) = on_begin {
            begin(window, cx);
        }
        set(value, window, cx);
        if let Some(commit) = on_commit {
            commit(window, cx);
        }
    }
}

/// Apply one key to a number being typed. Returns whether the key was the
/// entry's (always, while it is editing).
fn numeric_entry_key(
    entry: &gpui::Entity<NumericEntry>,
    event: &gpui::KeyDownEvent,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    if !entry.read(cx).editing() {
        return false;
    }
    match entry_key(event) {
        EntryKey::Commit => end_numeric_entry(entry, true, window, cx),
        EntryKey::Cancel => end_numeric_entry(entry, false, window, cx),
        EntryKey::Nudge(steps) => entry.update(cx, |entry, cx| {
            if let Some(session) = entry.session {
                let next = session.nudge_draft(&entry.input.value, steps);
                entry.input.set_value(next);
                entry.input.select_all();
                cx.notify();
            }
        }),
        EntryKey::Edit => entry.update(cx, |entry, cx| {
            let _ = entry.input.handle_key_with_clipboard(event, Some(cx));
            cx.notify();
        }),
    }
    true
}

/// Give a key to the Inspector number being typed into, if one has the
/// keyboard. The studio window calls this before its own shortcuts, so Space,
/// Enter, digits and Escape type into the field instead of driving the
/// transport or clearing the selection. Returns whether the key was taken.
pub fn route_numeric_entry_key(
    event: &gpui::KeyDownEvent,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    let Some(entry) = ACTIVE_NUMERIC_ENTRY
        .with(|active| active.borrow().as_ref().and_then(|weak| weak.upgrade()))
    else {
        return false;
    };
    if !entry.read(cx).input.is_focused(window) {
        // Focus went elsewhere without a click outside the field (another
        // window, a dialog): the edit is over, and its draft is dropped.
        if entry.read(cx).editing() {
            end_numeric_entry(&entry, false, window, cx);
        }
        return false;
    }
    numeric_entry_key(&entry, event, window, cx)
}

impl RenderOnce for NumericStepper {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        const SCRUB_PIXELS_PER_STEP: f32 = 5.0;
        let Self {
            id,
            value,
            display,
            min,
            max,
            step,
            coarse_step,
            disabled,
            on_begin,
            on_set,
            on_commit,
        } = self;
        let entry = window.use_keyed_state((id, 1usize), cx, |_, cx| NumericEntry {
            input: crate::components::text_input::TextInputState::new(
                "inspector-number-entry",
                cx.focus_handle(),
            )
            .with_field_height(crate::components::inspector_kit::PROP_CONTROL_H)
            .with_inline_text(crate::theme::space::SNUG, gpui::FontWeight::MEDIUM)
            .with_ascii_charset("0123456789.,-+"),
            session: None,
            on_begin: None,
            on_set: None,
            on_commit: None,
        });
        #[cfg(feature = "ui-preview")]
        if !disabled
            && !entry.read(cx).editing()
            && PREVIEW_EDIT_FIELD.with(|field| field.get() == Some(id))
        {
            PREVIEW_EDIT_FIELD.with(|field| field.set(None));
            let format = crate::components::numeric_edit::NumberFormat::decimal(
                min,
                max,
                display_decimals(&display),
            );
            begin_numeric_entry(
                &entry,
                value,
                format,
                on_begin.clone(),
                on_set.clone(),
                on_commit.clone(),
                window,
                cx,
            );
        }
        let editing = !disabled && entry.read(cx).editing();

        let root = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_end()
            .w_full()
            .min_w(px(0.0));

        if editing {
            let focused = entry.read(cx).input.is_focused(window);
            let mut callbacks = crate::components::text_input::bind_mouse_selection(
                entry.clone(),
                |entry: &mut NumericEntry| &mut entry.input,
            );
            let outside = entry.clone();
            callbacks.on_mouse_down_out = Some(std::sync::Arc::new(move |window, cx| {
                end_numeric_entry(&outside, true, window, cx);
            }));
            let keys = entry.clone();
            let field = crate::components::text_input::text_field_with_callbacks(
                &entry.read(cx).input,
                focused,
                callbacks,
            );
            return root
                .child(
                    div()
                        .w_full()
                        .min_w(px(0.0))
                        // Windows without the studio's key routing (a
                        // detached panel) still reach the field this way.
                        .on_key_down(move |event, window, cx| {
                            if numeric_entry_key(&keys, event, window, cx) {
                                cx.stop_propagation();
                            }
                        })
                        .child(field),
                )
                .into_any_element();
        }

        let drag_id = id.to_string();
        let drag_id_move = drag_id.clone();
        let field = div()
            .id((id, 0usize))
            .w_full()
            .min_w(px(0.0))
            .h(px(crate::components::inspector_kit::PROP_CONTROL_H))
            .flex()
            .items_center()
            .justify_end()
            .rounded(px(crate::theme::radius::CONTROL_SM))
            .border(px(1.0))
            .border_color(Colors::border_subtle())
            .bg(Colors::surface_input())
            .px(px(crate::theme::space::SNUG))
            .text_size(px(crate::theme::typography::UI_XS))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(Colors::text_primary())
            .child(display.clone());
        let field = if disabled {
            field.into_any_element()
        } else {
            let format = crate::components::numeric_edit::NumberFormat::decimal(
                min,
                max,
                display_decimals(&display),
            )
            .with_step(step);
            let start_entry = entry.clone();
            let entry_begin = on_begin.clone();
            let entry_set = on_set.clone();
            let entry_commit = on_commit.clone();
            field
                .cursor(gpui::CursorStyle::ResizeUpDown)
                .tooltip(crate::components::controls::fb_tooltip(
                    "Drag to change. Double-click to type a value.",
                ))
                .hover(|style| {
                    style
                        .bg(Colors::surface_control_hover())
                        .border_color(Colors::border_strong())
                })
                .on_click(move |event, window, cx| {
                    if event.click_count() == 2 {
                        begin_numeric_entry(
                            &start_entry,
                            value,
                            format,
                            entry_begin.clone(),
                            entry_set.clone(),
                            entry_commit.clone(),
                            window,
                            cx,
                        );
                    }
                })
                .on_drag(
                    SpinDrag::new(drag_id, value),
                    move |drag, _offset, window, cx| {
                        drag.begin();
                        if let Some(start) = on_begin.as_ref() {
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
                    on_set(next, window, cx);
                })
                .when_some(on_commit, |field, commit| {
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
        };
        root.opacity(if disabled { 0.48 } else { 1.0 })
            .child(field)
            .into_any_element()
    }
}

#[cfg(test)]
mod numeric_entry_tests {
    use super::*;

    #[test]
    fn a_typed_value_starts_at_the_precision_it_is_shown_with() {
        assert_eq!(display_decimals("16.00 bt"), 2);
        assert_eq!(display_decimals("-3.0 dB"), 1);
        assert_eq!(display_decimals("0 ms"), 0);
        assert_eq!(display_decimals("+0 st"), 0);
        assert_eq!(display_decimals("100.5 %"), 1);
        assert_eq!(display_decimals(""), 0);
    }

    fn key(name: &str, shift: bool) -> gpui::KeyDownEvent {
        gpui::KeyDownEvent {
            keystroke: gpui::Keystroke {
                modifiers: gpui::Modifiers {
                    shift,
                    ..Default::default()
                },
                key: name.to_string(),
                key_char: None,
            },
            is_held: false,
            prefer_character_input: false,
        }
    }

    #[test]
    fn enter_writes_escape_drops_and_arrows_step() {
        assert_eq!(entry_key(&key("enter", false)), EntryKey::Commit);
        assert_eq!(entry_key(&key("tab", false)), EntryKey::Commit);
        assert_eq!(entry_key(&key("escape", false)), EntryKey::Cancel);
        assert_eq!(entry_key(&key("up", false)), EntryKey::Nudge(1.0));
        assert_eq!(entry_key(&key("down", true)), EntryKey::Nudge(-10.0));
        assert_eq!(entry_key(&key("5", false)), EntryKey::Edit);
        assert_eq!(entry_key(&key("space", false)), EntryKey::Edit);
    }
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
        .h(px(crate::components::inspector_kit::PROP_CONTROL_H))
        .min_w(px(26.0))
        .px(px(crate::theme::space::SNUG))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(crate::theme::radius::CONTROL_SM))
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_input())
        .opacity(if enabled { 1.0 } else { 0.45 })
        .text_size(px(crate::theme::typography::DENSE_LABEL))
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
        .text_size(px(crate::theme::typography::DENSE_LABEL))
        .line_height(px(15.0))
        .text_color(Colors::text_muted())
        .child(text.into())
}

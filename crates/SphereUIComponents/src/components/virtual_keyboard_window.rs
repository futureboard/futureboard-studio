//! The Virtual Keyboard: a modeless native window.
//!
//! It used to be a card drawn over the arrangement's corner, which covered
//! clips and could not be moved to another screen. It is now a tool window of
//! its own that the user can place anywhere and leave open while working:
//! nothing in Studio waits on it.
//!
//! The window owns no musical state. Octave, velocity, channel, sustain and
//! every sounding note live in the app-lifetime [`VirtualKeyboardPanel`]
//! service, which every Futureboard window forwards its keys into, so typing
//! plays the same whether this window, the arrangement or the floating MIDI
//! editor has focus, and closing any of them releases only the notes it held.

use std::collections::HashSet;
use std::sync::Arc;

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, relative, size, App, AppContext, Bounds, Context, Entity, FocusHandle,
    InteractiveElement, IntoElement, KeyDownEvent, MouseButton, ParentElement, Render,
    StatefulInteractiveElement, Styled, Subscription, Window, WindowBackgroundAppearance,
    WindowBounds, WindowHandle, WindowKind,
};

use crate::components::title_bar::external_window_titlebar;
use crate::components::virtual_keyboard::{VirtualKeyboardPanel, BLACK_KEYS, WHITE_KEYS};
use crate::theme::{radius, space, typography, Colors};

const WINDOW_WIDTH: f32 = 760.0;
const WINDOW_HEIGHT: f32 = 236.0;
const WINDOW_MIN_WIDTH: f32 = 520.0;
const WINDOW_MIN_HEIGHT: f32 = 200.0;

/// Octaves drawn: one below the typing range, the two it spans. Every key can
/// be clicked; the ones the computer keyboard reaches carry their letter.
const OCTAVES_SHOWN: i16 = 3;

/// Unconsumed keys go back to Studio's keymap (Space plays, Ctrl+S saves)
/// rather than dying in a window that has no commands of its own.
pub type VirtualKeyboardKeyCb = Arc<dyn Fn(&KeyDownEvent, &mut App) -> bool + 'static>;

pub struct VirtualKeyboardWindow {
    keyboard: Entity<VirtualKeyboardPanel>,
    focus_handle: FocusHandle,
    focus_claimed: bool,
    dispatch_key: VirtualKeyboardKeyCb,
    on_close: Arc<dyn Fn(&mut Window, &mut App) + 'static>,
    _keyboard_observer: Subscription,
}

impl VirtualKeyboardWindow {
    pub(crate) fn new(
        keyboard: Entity<VirtualKeyboardPanel>,
        dispatch_key: VirtualKeyboardKeyCb,
        on_close: Arc<dyn Fn(&mut Window, &mut App) + 'static>,
        cx: &mut Context<Self>,
    ) -> Self {
        // The service changes from every window (a note typed in the
        // arrangement lights its key here), so this view follows it.
        let _keyboard_observer = cx.observe(&keyboard, |_, _, cx| cx.notify());
        Self {
            keyboard,
            focus_handle: cx.focus_handle(),
            focus_claimed: false,
            dispatch_key,
            on_close,
            _keyboard_observer,
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let window_id = window.window_handle().window_id();
        let mods = event.keystroke.modifiers;
        let command_modifier = mods.control || mods.alt || mods.platform || mods.function;
        let consumed = self.keyboard.update(cx, |keyboard, cx| {
            keyboard.handle_key_down(
                window_id,
                event.keystroke.key.as_str(),
                command_modifier,
                event.is_held,
                false,
                cx,
            )
        });
        if consumed {
            window.prevent_default();
            cx.stop_propagation();
            return;
        }
        if event.is_held {
            return;
        }
        if (self.dispatch_key)(event, cx) {
            window.prevent_default();
            cx.stop_propagation();
        }
    }
}

impl Render for VirtualKeyboardWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.focus_claimed {
            self.focus_handle.focus(window, cx);
            self.focus_claimed = true;
        }
        let native_frame = crate::platform_chrome::native_tool_window_frame();
        let keyboard = self.keyboard.read(cx);
        let state = &keyboard.state;
        let octave = state.controller.octave;
        let velocity = state.controller.velocity;
        let channel = state.controller.channel;
        let sustain = state.controller.sustain;
        let active = state.controller.active_note_values();
        let target_label = state.target_label.clone();
        let hint = state.hint.clone();

        let target = cx.entity().clone();
        let keys_entity = self.keyboard.clone();
        let on_close = self.on_close.clone();

        div()
            .flex()
            .flex_col()
            .size_full()
            .font(crate::theme::ui_font())
            .bg(Colors::surface_window())
            .text_color(Colors::text_secondary())
            .overflow_hidden()
            .capture_key_down(move |event, window, cx| {
                let _ = target.update(cx, |this, cx| this.on_key_down(event, window, cx));
            })
            .capture_key_up({
                let keyboard = self.keyboard.clone();
                move |event, window: &mut Window, cx| {
                    let window_id = window.window_handle().window_id();
                    let handled = keyboard.update(cx, |keyboard, cx| {
                        keyboard.handle_key_up(window_id, event.keystroke.key.as_str(), cx)
                    });
                    if handled {
                        cx.stop_propagation();
                    }
                }
            })
            // A release anywhere in the window ends every mouse-held note, so
            // a drag off a key never leaves it sounding.
            .capture_any_mouse_up({
                let keyboard = self.keyboard.clone();
                move |_event, _window, cx| {
                    let _ = keyboard.update(cx, |keyboard, cx| keyboard.release_mouse_notes(cx));
                }
            })
            // Focus lives on a 0×0 child, not the root: a root focus hitbox
            // wins over the caption's drag area on Windows.
            .child(div().w(px(0.0)).h(px(0.0)).track_focus(&self.focus_handle))
            .when(!native_frame, |root| {
                root.child(external_window_titlebar(
                    "Virtual Keyboard",
                    "virtual-keyboard-window-close",
                    move |window, cx| {
                        on_close(window, cx);
                        window.remove_window();
                    },
                ))
            })
            .child(control_bar(
                target_label,
                hint.is_none(),
                octave,
                velocity,
                channel,
                sustain,
                &self.keyboard,
            ))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .p(px(space::BASE))
                    .child(keyboard_view(octave, &active, keys_entity)),
            )
            .child(footer(hint))
    }
}

/// Target, octave, velocity, channel, sustain and panic, in one row.
fn control_bar(
    target_label: Option<String>,
    target_ok: bool,
    octave: i8,
    velocity: u8,
    channel: u8,
    sustain: bool,
    keyboard: &Entity<VirtualKeyboardPanel>,
) -> impl IntoElement {
    let step =
        |id: &'static str,
         caption: &'static str,
         value: String,
         tip: &'static str,
         apply: fn(&mut VirtualKeyboardPanel, i8, &mut Context<VirtualKeyboardPanel>)| {
            let keyboard = keyboard.clone();
            stepper(id, caption, value, tip, move |delta, cx| {
                let _ = keyboard.update(cx, |keyboard, cx| apply(keyboard, delta, cx));
            })
        };
    let sustain_keyboard = keyboard.clone();
    let panic_keyboard = keyboard.clone();
    div()
        .flex()
        .flex_row()
        .flex_none()
        .items_center()
        .gap(px(space::BASE))
        .h(px(36.0))
        .px(px(space::BASE))
        .bg(Colors::surface_panel())
        .border_b(px(1.0))
        .border_color(Colors::border_subtle())
        .child(target_pill(target_label, target_ok))
        .child(div().flex_1())
        .child(step(
            "vkbd-octave",
            "Octave",
            octave_label(octave),
            "Octave of the typing keys (Z / X)",
            |keyboard, delta, cx| keyboard.step_octave(delta, cx),
        ))
        .child(step(
            "vkbd-velocity",
            "Velocity",
            velocity.to_string(),
            "Velocity of typed and clicked notes (C / V)",
            |keyboard, delta, cx| keyboard.step_velocity(delta, cx),
        ))
        .child(step(
            "vkbd-channel",
            "Channel",
            (channel + 1).to_string(),
            "MIDI channel the keyboard sends on",
            |keyboard, delta, cx| keyboard.step_channel(delta, cx),
        ))
        .child(latch_button(
            "vkbd-sustain",
            "Sustain",
            sustain,
            "Hold every note until Sustain is released (Q)",
            move |cx| {
                let _ = sustain_keyboard.update(cx, |keyboard, cx| keyboard.toggle_sustain(cx));
            },
        ))
        .child(plain_button(
            "vkbd-panic",
            "Panic",
            "Release every note the keyboard is holding",
            move |cx| {
                let _ = panic_keyboard.update(cx, |keyboard, cx| keyboard.panic(cx));
            },
        ))
}

/// C4 is middle C: the octave of the leftmost typing key.
fn octave_label(octave: i8) -> String {
    format!("C{octave}")
}

/// Where the notes go: the armed or selected instrument, or why nothing will
/// sound.
fn target_pill(label: Option<String>, ok: bool) -> impl IntoElement {
    let (fg, edge) = if ok {
        (Colors::text_primary(), Colors::border_default())
    } else {
        (Colors::status_warning(), Colors::status_warning())
    };
    div()
        .flex()
        .flex_row()
        .flex_shrink_1()
        .min_w_0()
        .items_center()
        .gap(px(space::TIGHT))
        .h(px(24.0))
        .px(px(space::SNUG))
        .rounded(px(radius::CONTROL_SM))
        .bg(Colors::surface_input())
        .border(px(1.0))
        .border_color(edge)
        .child(
            div()
                .flex_none()
                .text_size(px(typography::DENSE_CAPTION))
                .text_color(Colors::text_faint())
                .child("To"),
        )
        .child(
            div()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_size(px(typography::DENSE_LABEL))
                .text_color(fg)
                .child(label.unwrap_or_else(|| "No target".to_string())),
        )
}

fn stepper(
    id: &'static str,
    caption: &'static str,
    value: String,
    tip: &'static str,
    on_step: impl Fn(i8, &mut App) + Clone + 'static,
) -> impl IntoElement {
    let down = on_step.clone();
    let up = on_step;
    div()
        .flex()
        .flex_row()
        .flex_none()
        .items_center()
        .gap(px(space::TIGHT))
        .child(
            div()
                .text_size(px(typography::DENSE_CAPTION))
                .text_color(Colors::text_faint())
                .child(caption),
        )
        .child(
            div()
                .id(id)
                .flex()
                .flex_row()
                .items_center()
                .h(px(24.0))
                .rounded(px(radius::CONTROL_SM))
                .bg(Colors::surface_input())
                .border(px(1.0))
                .border_color(Colors::border_default())
                .tooltip(crate::components::controls::fb_tooltip(tip))
                .child(step_arrow((id, 0usize), "−", move |cx| down(-1, cx)))
                .child(
                    div()
                        .min_w(px(34.0))
                        .flex()
                        .justify_center()
                        .text_size(px(typography::DENSE_LABEL))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(Colors::text_primary())
                        .child(value),
                )
                .child(step_arrow((id, 1usize), "+", move |cx| up(1, cx))),
        )
}

fn step_arrow(
    id: (&'static str, usize),
    label: &'static str,
    on_click: impl Fn(&mut App) + 'static,
) -> impl IntoElement {
    let hover = Colors::composite(Colors::surface_input(), Colors::state_hover());
    div()
        .id(id)
        .w(px(20.0))
        .h_full()
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(typography::DENSE_LABEL))
        .text_color(Colors::text_secondary())
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(move |s| s.bg(hover))
        .on_click(move |_, _, cx| on_click(cx))
        .child(label)
}

/// A latched toggle: lit in the accent while on, like every other latched
/// mode button in Studio.
fn latch_button(
    id: &'static str,
    label: &'static str,
    on: bool,
    tip: &'static str,
    on_click: impl Fn(&mut App) + 'static,
) -> impl IntoElement {
    let rest = if on {
        Colors::accent_muted()
    } else {
        Colors::surface_raised()
    };
    let hover = Colors::composite(rest, Colors::state_hover());
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .h(px(24.0))
        .px(px(space::BASE))
        .rounded(px(radius::CONTROL_SM))
        .bg(rest)
        .border(px(1.0))
        .border_color(if on {
            Colors::accent_primary()
        } else {
            Colors::border_default()
        })
        .text_size(px(typography::DENSE_LABEL))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(if on {
            Colors::text_primary()
        } else {
            Colors::text_secondary()
        })
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(move |s| s.bg(hover))
        .tooltip(crate::components::controls::fb_tooltip(tip))
        .on_click(move |_, _, cx| on_click(cx))
        .child(label)
}

fn plain_button(
    id: &'static str,
    label: &'static str,
    tip: &'static str,
    on_click: impl Fn(&mut App) + 'static,
) -> impl IntoElement {
    let rest = Colors::surface_raised();
    let hover = Colors::composite(rest, Colors::state_hover());
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .h(px(24.0))
        .px(px(space::BASE))
        .rounded(px(radius::CONTROL_SM))
        .bg(rest)
        .border(px(1.0))
        .border_color(Colors::border_default())
        .text_size(px(typography::DENSE_LABEL))
        .text_color(Colors::text_primary())
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(move |s| s.bg(hover))
        .tooltip(crate::components::controls::fb_tooltip(tip))
        .on_click(move |_, _, cx| on_click(cx))
        .child(label)
}

fn footer(hint: Option<String>) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .flex_none()
        .items_center()
        .gap(px(space::BASE))
        .h(px(24.0))
        .px(px(space::BASE))
        .border_t(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_panel())
        .text_size(px(typography::DENSE_CAPTION))
        .text_color(Colors::text_faint())
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .when_some(hint, |row, hint| {
                    row.text_color(Colors::status_warning()).child(hint)
                }),
        )
        .child(div().flex_none().child(
            "A–; play  ·  W E T Y U O P sharps  ·  Z/X octave  ·  C/V velocity  ·  Q sustain",
        ))
}

/// One key of the drawn keyboard.
#[derive(Debug, Clone, Copy, PartialEq)]
struct DrawnKey {
    note: u8,
    black: bool,
    /// Left edge and width, as fractions of the keyboard's width.
    left: f32,
    width: f32,
    /// The computer key that plays it at the current octave, if any.
    letter: Option<&'static str>,
}

/// The keys of [`OCTAVES_SHOWN`] octaves starting an octave below the typing
/// range, laid out as fractions of the keyboard width so the window can be
/// any size.
fn drawn_keys(octave: i8) -> Vec<DrawnKey> {
    const WHITE_STEPS: [u8; 7] = [0, 2, 4, 5, 7, 9, 11];
    const BLACK_AFTER: [(u8, u8); 5] = [(0, 1), (1, 3), (3, 6), (4, 8), (5, 10)];
    let typing_root = 60 + (octave as i16 - 4) * 12;
    let first = typing_root - 12;
    let white_count = 7 * OCTAVES_SHOWN as usize;
    let white_w = 1.0 / white_count as f32;
    let black_w = white_w * 0.62;
    let letter_for = |note: i16| -> Option<&'static str> {
        let semitone = note - typing_root;
        WHITE_KEYS
            .iter()
            .chain(BLACK_KEYS.iter())
            .find(|key| key.semitone as i16 == semitone)
            .map(|key| key.key)
    };
    let mut keys = Vec::new();
    for oct in 0..OCTAVES_SHOWN {
        let base = first + oct * 12;
        for (i, step) in WHITE_STEPS.iter().enumerate() {
            let note = base + *step as i16;
            if !(0..=127).contains(&note) {
                continue;
            }
            let index = oct as usize * 7 + i;
            keys.push(DrawnKey {
                note: note as u8,
                black: false,
                left: index as f32 * white_w,
                width: white_w,
                letter: letter_for(note),
            });
        }
        for (white_index, step) in BLACK_AFTER {
            let note = base + step as i16;
            if !(0..=127).contains(&note) {
                continue;
            }
            let boundary = (oct as usize * 7 + white_index as usize + 1) as f32 * white_w;
            keys.push(DrawnKey {
                note: note as u8,
                black: true,
                left: boundary - black_w * 0.5,
                width: black_w,
                letter: letter_for(note),
            });
        }
    }
    keys
}

fn keyboard_view(
    octave: i8,
    active: &HashSet<u8>,
    keyboard: Entity<VirtualKeyboardPanel>,
) -> impl IntoElement {
    let keys = drawn_keys(octave);
    let (whites, blacks): (Vec<DrawnKey>, Vec<DrawnKey>) =
        keys.into_iter().partition(|key| !key.black);
    let mut root = div()
        .relative()
        .size_full()
        .rounded(px(radius::CONTROL_SM))
        .overflow_hidden()
        .border(px(1.0))
        .border_color(Colors::border_default())
        .bg(Colors::piano_key_seam());
    for key in whites.iter().chain(blacks.iter()) {
        root = root.child(piano_key(
            *key,
            active.contains(&key.note),
            keyboard.clone(),
        ));
    }
    root
}

fn piano_key(
    key: DrawnKey,
    sounding: bool,
    keyboard: Entity<VirtualKeyboardPanel>,
) -> impl IntoElement {
    let rest = if key.black {
        Colors::piano_black_key()
    } else {
        Colors::piano_white_key()
    };
    let fill = if sounding {
        Colors::composite(rest, Colors::with_alpha(Colors::accent_primary(), 0.72))
    } else {
        rest
    };
    let hover = Colors::composite(rest, Colors::with_alpha(Colors::accent_primary(), 0.18));
    let label_color = if key.black {
        Colors::with_alpha(Colors::piano_white_key(), 0.72)
    } else {
        Colors::piano_key_label()
    };
    let down = keyboard.clone();
    let up = keyboard;
    let note = key.note;
    div()
        .id(("vkbd-key", note as usize))
        .absolute()
        .top_0()
        .left(relative(key.left))
        .w(relative(key.width))
        .h(relative(if key.black { 0.6 } else { 1.0 }))
        .when(!key.black, |key| {
            key.border_r(px(1.0)).border_color(Colors::piano_key_seam())
        })
        .when(key.black, |key| key.rounded_b(px(radius::MICRO)))
        .bg(fill)
        .when(!sounding, move |key| key.hover(move |s| s.bg(hover)))
        .cursor(gpui::CursorStyle::PointingHand)
        .flex()
        .flex_col()
        .items_center()
        .justify_end()
        .pb(px(space::TIGHT))
        .on_mouse_down(MouseButton::Left, move |_, _, cx| {
            cx.stop_propagation();
            let _ = down.update(cx, |keyboard, cx| keyboard.mouse_note_on(note, cx));
        })
        .on_mouse_up(MouseButton::Left, move |_, _, cx| {
            let _ = up.update(cx, |keyboard, cx| keyboard.mouse_note_off(note, cx));
        })
        .when_some(key.letter, |el, letter| {
            el.child(
                div()
                    .text_size(px(typography::DENSE_CAPTION))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(label_color)
                    .child(letter.to_ascii_uppercase()),
            )
        })
        .when(!key.black && note % 12 == 0, |el| {
            el.child(
                div()
                    .text_size(px(typography::DENSE_CAPTION))
                    .text_color(Colors::with_alpha(Colors::piano_key_label(), 0.55))
                    .child(format!("C{}", note as i16 / 12 - 1)),
            )
        })
}

/// Open the Virtual Keyboard window near `owner_bounds`.
pub fn open_virtual_keyboard_window(
    owner_bounds: Option<Bounds<gpui::Pixels>>,
    keyboard: Entity<VirtualKeyboardPanel>,
    dispatch_key: VirtualKeyboardKeyCb,
    on_close: Arc<dyn Fn(&mut Window, &mut App) + 'static>,
    cx: &mut App,
) -> Result<WindowHandle<VirtualKeyboardWindow>, String> {
    let window_bounds = crate::window_position::centered_window_bounds(
        owner_bounds,
        size(px(WINDOW_WIDTH), px(WINDOW_HEIGHT)),
        cx,
    );
    // A top-level tool window, not an owned dialog: modeless, and free to sit
    // on another screen while the arrangement keeps the keyboard focus.
    let mut options =
        crate::platform_chrome::native_tool_window_options_partial("Virtual Keyboard");
    options.window_bounds = Some(WindowBounds::Windowed(window_bounds));
    options.kind = WindowKind::Floating;
    options.is_resizable = true;
    options.is_minimizable = true;
    options.window_background = WindowBackgroundAppearance::Opaque;
    options.window_min_size = Some(size(px(WINDOW_MIN_WIDTH), px(WINDOW_MIN_HEIGHT)));
    crate::window_position::apply_owner_display(&mut options, owner_bounds, cx);

    cx.open_window(options, move |window, cx| {
        // The platform's close button and Alt+F4 skip the drawn titlebar, so
        // Studio hears the close here: that is what releases held notes.
        let closing = on_close.clone();
        window.on_window_should_close(cx, move |window, cx| {
            closing(window, cx);
            true
        });
        cx.new(|cx| VirtualKeyboardWindow::new(keyboard, dispatch_key, on_close, cx))
    })
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_typing_keys_are_lettered_and_start_at_the_octave() {
        let keys = drawn_keys(4);
        let a = keys
            .iter()
            .find(|key| key.letter == Some("a"))
            .expect("A is drawn");
        assert_eq!(a.note, 60, "A plays the octave's C");
        assert!(!a.black);
        let w = keys
            .iter()
            .find(|key| key.letter == Some("w"))
            .expect("W is drawn");
        assert_eq!(w.note, 61);
        assert!(w.black);
        assert_eq!(keys.iter().filter(|key| !key.black).count(), 21);
        assert_eq!(
            keys.first().map(|key| key.note),
            Some(48),
            "an octave below"
        );
    }

    #[test]
    fn keys_tile_the_width_and_stay_in_midi_range() {
        let whites: Vec<DrawnKey> = drawn_keys(4).into_iter().filter(|key| !key.black).collect();
        let right = whites.last().map(|key| key.left + key.width).unwrap_or(0.0);
        assert!((right - 1.0).abs() < 1e-4);
        // At the top octave the notes past 127 are left out, not wrapped.
        assert!(drawn_keys(8).iter().all(|key| key.note <= 127));
        assert!(drawn_keys(0)
            .iter()
            .all(|key| key.letter.is_none() || key.note <= 127));
    }
}
